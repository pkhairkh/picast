//! boGDan Custom Site Resolvers
//!
//! Handles video hosting sites that yt-dlp does not support (or supports
//! poorly). Currently implements resolvers for:
//!
//! - **Voe / charlessheimprove.com** (and other Voe CDN front-ends):
//!   The page embeds an obfuscated JSON blob in a `<script
//!   type="application/json">` tag. The blob is decoded through a
//!   multi-step pipeline (ROT13 → strip markers → Base64 → char-shift →
//!   reverse → Base64) to recover the direct media URL.
//!
//! - **DoodStream / playmogo.com** (and other DoodStream front-ends):
//!   These pages embed a video player behind a Cloudflare Turnstile
//!   CAPTCHA. The resolver follows the `/e/` embed iframe, extracts
//!   the download token from the page, and constructs the direct
//!   media URL from DoodStream's pass/dl API endpoint.

use crate::resolver_socks::ResolverSocksForwarder;
use crate::{ResolveError, ResolveResult, UrlCategory};
use base64::Engine;
use scraper::{Html, Selector};
use std::time::Duration;
use tokio::time::timeout;

/// Browser-like User-Agent string sent by all custom resolver requests.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Build a reqwest client with cookie jar and browser-like defaults.
///
/// When `socks5_proxy` is provided, we start a LOCAL HTTP CONNECT→SOCKS5
/// forwarder that ONLY offers username/password auth (0x02) in its SOCKS5
/// greeting. This is CRITICAL: reqwest's built-in SOCKS5 support offers
/// BOTH no-auth (0x00) and username/password (0x02), which allows Tor to
/// choose no-auth. When Tor chooses no-auth, the isolation username is
/// never sent, and the stream gets assigned to a DIFFERENT circuit than
/// the playback path (which uses our SocksForwarder with only 0x02).
/// Different circuits → different exit IPs → CDN 403.
///
/// By using our own forwarder that only offers 0x02, we guarantee the
/// same Tor circuit as the playback path.
///
/// The `socks5_proxy` parameter should be a full SOCKS5h proxy URL with
/// isolation username, e.g. `socks5h://bogdan-hash@127.0.0.1:9050`.
async fn build_client(
    socks5_proxy: Option<&str>,
) -> Result<(reqwest::Client, Option<ResolverSocksForwarder>), ResolveError> {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(CUSTOM_RESOLVER_TIMEOUT_SECS))
        .user_agent(UA)
        .cookie_store(true)
        // Some sites redirect HTTP→HTTPS; follow all redirects.
        .redirect(reqwest::redirect::Policy::limited(10))
        // Accept gzip/br to look like a real browser.
        .gzip(true)
        .brotli(true);

    let mut forwarder = None;

    if let Some(proxy_url) = socks5_proxy {
        // Parse the SOCKS5h URL to extract the isolation username and
        // Tor SOCKS address, then start a local HTTP CONNECT→SOCKS5
        // forwarder that only offers username/password auth (0x02).
        //
        // URL format: socks5h://bogdan-HASH@127.0.0.1:9050/
        if let Some((username, socks_addr)) = parse_socks5_url(proxy_url) {
            match ResolverSocksForwarder::start(socks_addr, username).await {
                Ok(fwd) => {
                    let http_proxy_url = fwd.proxy_url();
                    let proxy = reqwest::Proxy::all(&http_proxy_url).map_err(|e| {
                        ResolveError::Network(format!("failed to configure HTTP proxy: {}", e))
                    })?;
                    builder = builder.proxy(proxy);
                    tracing::info!(
                        http_proxy = %http_proxy_url,
                        socks5_proxy = %proxy_url,
                        "custom resolver: routing through local SOCKS5 forwarder (auth=0x02 only, same circuit as playback)"
                    );
                    forwarder = Some(fwd);
                },
                Err(e) => {
                    // Fallback: if the forwarder fails to start, fall back
                    // to reqwest's built-in SOCKS5 (suboptimal but better
                    // than no proxy at all).
                    tracing::warn!(
                        error = %e,
                        "failed to start resolver SOCKS5 forwarder — falling back to reqwest built-in SOCKS5 (may cause circuit mismatch)"
                    );
                    let proxy = reqwest::Proxy::all(proxy_url).map_err(|e| {
                        ResolveError::Network(format!("failed to configure SOCKS5 proxy: {}", e))
                    })?;
                    builder = builder.proxy(proxy);
                },
            }
        } else {
            // Could not parse the SOCKS5h URL — use it as-is.
            tracing::warn!(proxy = %proxy_url, "could not parse SOCKS5h URL — using as-is (may cause circuit mismatch)");
            let proxy = reqwest::Proxy::all(proxy_url).map_err(|e| {
                ResolveError::Network(format!("failed to configure SOCKS5 proxy: {}", e))
            })?;
            builder = builder.proxy(proxy);
        }
    }

    let client = builder
        .build()
        .map_err(|e| ResolveError::Network(format!("failed to build HTTP client: {}", e)))?;

    Ok((client, forwarder))
}

/// Parse a SOCKS5h proxy URL to extract the isolation username and address.
///
/// Input: `socks5h://bogdan-HASH@127.0.0.1:9050/`
/// Returns: `Some(("bogdan-HASH", "127.0.0.1:9050"))`
fn parse_socks5_url(url: &str) -> Option<(String, String)> {
    // Strip the scheme prefix
    let rest = url.strip_prefix("socks5h://").or_else(|| url.strip_prefix("socks5://"))?;

    // Split on '@' to separate username from host
    let (username, host_part) = if let Some(at_pos) = rest.find('@') {
        (rest[..at_pos].to_string(), &rest[at_pos + 1..])
    } else {
        // No username — can't do circuit isolation
        return None;
    };

    // Strip trailing slash
    let host_part = host_part.strip_suffix('/').unwrap_or(host_part);

    Some((username, host_part.to_string()))
}

/// HTTP request timeout for custom resolvers (15 seconds).
const CUSTOM_RESOLVER_TIMEOUT_SECS: u64 = 15;

/// Canonical Voe domain and unblock proxies.
///
/// This list is intentionally MINIMAL. Voe rotates front-end domains
/// constantly to evade adblockers — maintaining a static list of every
/// front-end is futile. Instead, the Voe custom resolver uses **content-
/// based detection** (obfuscated JSON patterns) as its primary mechanism.
///
/// The resolver is tried FIRST for ALL WebPage URLs (before yt-dlp),
/// regardless of whether the domain is in this list. If the page contains
/// Voe's signature obfuscated JSON, the resolver succeeds; if not, it
/// falls back to yt-dlp. No domain list can ever be complete — the
/// content heuristic is what makes this work.
///
/// Domains listed here are used ONLY for:
/// - Logging clarity ("known Voe domain" vs "unknown — trying Voe resolver")
/// - The `is_voe_domain()` heuristic fast-path check
/// - The classifier's `WEB_PAGE_DOMAINS` list (for URL categorisation)
const VOE_DOMAINS: &[&str] = &[
    // The canonical Voe domain
    "voe.sx",
    // Unblock proxies (these are stable)
    "voe-unblock.com",
    "voeunblock.com",
    "voeunbl0ck.com",
    "voe-unblk.com",
    "voeunblk.com",
    "voeunblock2.com",
];

/// Known DoodStream front-end domains.
const DOODSTREAM_DOMAINS: &[&str] = &[
    "playmogo.com",
    "doodstream.com",
    "dood.to",
    "dood.watch",
    "dood.la",
    "dood.ws",
    "doodstream.co",
    "dood.yt",
    "dood.re",
    "dood.pm",
    "dood.wf",
    "dood.cx",
];

/// Check if a URL points to an HLS playlist (.m3u8).
///
/// HLS playlists are text files that reference multiple segment URLs.
/// The `StreamSource` now has an HLS client that fetches the master
/// playlist, selects the best quality variant, and downloads each
/// .ts segment sequentially. The segments are pushed into appsrc as
/// MPEG-TS data, which parsebin handles natively via tsdemux.
///
/// HLS URLs are still deprioritized in favor of direct MP4 URLs
/// (fewer HTTP requests, lower latency), but are returned as a
/// fallback when no MP4 URL is available.
fn is_hls_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.contains(".m3u8")
}

/// Extract the CDN speed-limit parameter (`sp=`) from a URL's query string.
///
/// Many video CDNs (e.g. Voe) embed a rate-limit token as `&sp=NNN` where
/// NNN is the maximum download speed in kbps. When `sp=380`, the CDN caps
/// throughput at ~380 kbps — far below what any 720p video needs (~2500 kbps).
/// Even 480p needs ~1200 kbps, so a low `sp=` value guarantees stuttering.
///
/// Returns `None` if the URL has no `sp=` parameter (no rate limit — best case)
/// or if the value cannot be parsed as a number.
fn extract_cdn_speed_param(url: &str) -> Option<u64> {
    for prefix in &["&sp=", "?sp="] {
        if let Some(pos) = url.find(prefix) {
            let after = &url[pos + prefix.len()..];
            let value = after.split('&').next().unwrap_or("");
            if let Ok(speed) = value.parse::<u64>() {
                return Some(speed);
            }
        }
    }
    None
}

/// Typical video bitrates by quality level (in kbps).
///
/// These are conservative estimates for H.264-encoded MP4 content from video
/// CDNs. Used to determine whether a CDN rate limit (`sp=` parameter) can
/// sustain a given quality level.
///
/// If the CDN rate limit is below the typical bitrate, playback will
/// stutter regardless of buffer size — the download can never keep up
/// with the decode rate.
fn typical_bitrate_kbps(quality: &str) -> Option<u64> {
    match quality {
        "240" => Some(400),
        "360" => Some(800),
        "480" => Some(1500),
        "720" => Some(3000),
        "1080" => Some(6000),
        _ => None,
    }
}

/// Extract a media URL from a JSON value, handling both simple strings
/// and objects with multiple quality levels.
///
/// Voe's deobfuscated JSON can have `mp4` as either:
/// - A simple string: `"mp4": "https://cdn.example.com/video.mp4"`
/// - An object with quality levels: `"mp4": {"720": "url1", "1080": "url2"}`
///
/// When it's an object, we select the best quality based on CDN rate limits:
///
/// 1. Parse the `sp=` (speed limit) parameter from each quality URL.
///    - `sp=380` means the CDN rate-limits to ~380 kbps
///    - No `sp=` means no rate limit (best case)
///
/// 2. Select the **highest quality whose CDN rate limit can sustain playback**:
///    - If `sp=` ≥ typical bitrate for that quality → sustainable
///    - If `sp=` < typical bitrate → will stutter (download can't keep up)
///    - No `sp=` → treat as unlimited (always sustainable)
///
/// 3. Among sustainable qualities, prefer: 720 → 480 → 360 → 240 → 1080
///    (720p is optimal for Pi 4 HW decode; 1080p is last resort)
///
/// 4. If NO quality is sustainable (all rate-limited below their bitrate),
///    select the quality with the **highest `sp=` value** — it will still
///    stutter, but a higher rate limit means more data per second and
///    longer play periods between rebuffer pauses.
///
/// Returns `None` if the value is neither a string nor a quality-level object,
/// or if all extracted URLs are HLS playlists.
fn extract_media_from_json_value(value: &serde_json::Value) -> Option<String> {
    // Case 1: Simple string URL
    if let Some(url) = value.as_str() {
        if !url.is_empty() {
            if is_hls_url(url) {
                tracing::info!(url = %url, "Voe: HLS URL found in JSON value — returning as fallback (StreamSource has HLS client)");
            } else {
                let sp = extract_cdn_speed_param(url);
                if let Some(speed) = sp {
                    tracing::info!(
                        sp_kbps = speed,
                        url = %url,
                        "Voe: single MP4 URL with CDN rate limit (sp= parameter)"
                    );
                }
            }
            return Some(url.to_owned());
        }
        return None;
    }

    // Case 2: Object with quality levels (e.g., {"720": "url", "1080": "url"})
    if let Some(obj) = value.as_object() {
        // Collect all quality URLs with their CDN rate limits.
        // HLS URLs are included as fallback — StreamSource now has an
        // HLS client that can download segments and feed them as MPEG-TS
        // to appsrc/parsebin.
        let mut mp4_candidates: Vec<(&str, &str, Option<u64>)> = Vec::new(); // (quality, url, sp_value)
        let mut hls_candidates: Vec<(&str, &str)> = Vec::new(); // (quality, url)

        for (key, url_val) in obj.iter() {
            if let Some(url) = url_val.as_str() {
                if !url.is_empty() {
                    if is_hls_url(url) {
                        tracing::info!(
                            quality = %key,
                            "Voe: HLS URL found at quality level — available as fallback (StreamSource has HLS client)"
                        );
                        hls_candidates.push((key, url));
                    } else {
                        let sp = extract_cdn_speed_param(url);
                        mp4_candidates.push((key, url, sp));
                    }
                }
            }
        }

        if mp4_candidates.is_empty() && hls_candidates.is_empty() {
            tracing::warn!(
                keys = ?obj.keys().collect::<Vec<_>>(),
                "Voe: mp4 object contained no usable URLs"
            );
            return None;
        }

        // Prefer MP4 URLs over HLS. Only use HLS if no MP4 candidates.
        let mut candidates = if !mp4_candidates.is_empty() {
            mp4_candidates
        } else {
            tracing::info!("Voe: no MP4 URLs available, using HLS URL as fallback");
            // Return the highest quality HLS URL (prefer 720 > 480 > etc.)
            let quality_rank = |q: &str| -> i32 {
                match q {
                    "720" => 0,
                    "480" => 1,
                    "360" => 2,
                    "240" => 3,
                    "1080" => 4,
                    _ => 5,
                }
            };
            let mut sorted_hls = hls_candidates;
            sorted_hls.sort_by_key(|(q, _)| quality_rank(q));
            return Some(sorted_hls[0].1.to_owned());
        };

        // If only one candidate, return it (no choice to make)
        if candidates.len() == 1 {
            let (quality, url, sp) = &candidates[0];
            tracing::info!(
                quality = %quality,
                sp_kbps = ?sp,
                url = %url,
                "Voe: only one quality available"
            );
            return Some((*url).to_owned());
        }

        // Log all available quality levels with their CDN rate limits
        for (q, _u, sp) in &candidates {
            let sustainable = match (sp, typical_bitrate_kbps(q)) {
                (Some(speed), Some(bitrate)) => *speed >= bitrate,
                (None, _) => true, // No rate limit = always sustainable
                (Some(_), None) => true, // Unknown quality = assume sustainable
            };
            tracing::info!(
                quality = %q,
                sp_kbps = ?sp,
                typical_bitrate_kbps = ?typical_bitrate_kbps(q),
                sustainable = sustainable,
                "Voe: available quality level"
            );
        }

        // Quality preference order for Pi 4 V4L2 HW decode:
        // 720p is the sweet spot — full hardware decode support, reasonable bitrate
        // 1080p may work but pushes the V4L2 decoder's limits
        // Lower qualities are fine but lower visual quality
        let quality_rank = |q: &str| -> i32 {
            match q {
                "720" => 0,
                "480" => 1,
                "360" => 2,
                "240" => 3,
                "1080" => 4,
                _ => 5,
            }
        };

        // Strategy 1: Select the highest sustainable quality
        // A quality is "sustainable" if its CDN rate limit (sp=) is >= the
        // typical bitrate for that quality, OR if there's no sp= (unlimited).
        let mut sustainable: Vec<_> = candidates
            .iter()
            .filter(|(q, _, sp)| {
                match (sp, typical_bitrate_kbps(q)) {
                    (Some(speed), Some(bitrate)) => *speed >= bitrate,
                    (None, _) => true,
                    (Some(_), None) => true,
                }
            })
            .collect();

        if !sustainable.is_empty() {
            // Sort by quality preference (lowest rank = preferred)
            sustainable.sort_by_key(|(q, _, _)| quality_rank(q));
            let (quality, url, sp) = sustainable[0];
            tracing::info!(
                quality = %quality,
                sp_kbps = ?sp,
                url = %url,
                "Voe: selected SUSTAINABLE quality (CDN rate limit >= typical bitrate)"
            );
            return Some((*url).to_owned());
        }

        // Strategy 2: No quality is sustainable — pick the one with the
        // highest sp= value (most data per second, longest play between
        // rebuffer pauses). Among equal sp=, prefer lower quality
        // (lower bitrate = less data needed per second = slower drain).
        tracing::warn!(
            "Voe: NO quality level is sustainable through CDN rate limit — \
             selecting highest sp= value (will still stutter)"
        );
        candidates.sort_by(|a, b| {
            let sp_a = a.2.unwrap_or(0);
            let sp_b = b.2.unwrap_or(0);
            // Higher sp= first; on tie, lower quality (lower bitrate) first
            match sp_b.cmp(&sp_a) {
                std::cmp::Ordering::Equal => quality_rank(a.0).cmp(&quality_rank(b.0)),
                other => other,
            }
        });
        let (quality, url, sp) = &candidates[0];
        tracing::info!(
            quality = %quality,
            sp_kbps = ?sp,
            url = %url,
            "Voe: selected best-available quality (highest CDN rate limit)"
        );
        return Some((*url).to_owned());
    }

    None
}

/// Known bait/test video domains and filenames that Voe and DoodStream
/// use as decoy sources to foil scrapers.
const BAIT_DOMAINS: &[&str] =
    &["test-videos.co.uk", "sample-videos.com", "commondatastorage.googleapis.com"];

const BAIT_FILENAMES: &[&str] = &["BigBuckBunny", "Big_Buck_Bunny_1080_10s_5MB", "bbb.mp4"];

// ── Public API ─────────────────────────────────────────────────────

/// Check if a hostname is likely a Voe domain.
///
/// Uses a two-tier approach:
/// 1. **Exact match** against the canonical VOE_DOMAINS list (voe.sx, unblock proxies)
/// 2. **Heuristic detection** for Voe's rotating front-end domains
///
/// Voe front-end domains follow predictable patterns:
/// - They're `.com` domains (Voe doesn't use other TLDs for front-ends)
/// - They consist of 3-4 lowercase English words concatenated together
///   (e.g. "maryspecialwatch.com", "cactusheadroomscaling.com")
/// - The URL path is always a short alphanumeric ID (e.g. "/i5xi8glffb1d")
/// - The domain name is typically 15-35 characters of pure lowercase
/// - They contain no hyphens (unlike the unblock proxies)
/// - They don't match any well-known domain (youtube, vimeo, etc.)
///
/// This heuristic catches newly-rotated Voe domains without requiring
/// manual updates to a static list.
pub fn is_voe_domain(host: &str) -> bool {
    let host_lower = host.to_lowercase();

    // Tier 1: Exact match against known domains
    if VOE_DOMAINS.iter().any(|d| host_lower == *d || host_lower.ends_with(&format!(".{}", d))) {
        return true;
    }

    // Tier 2: Heuristic detection for Voe's rotating front-end domains
    //
    // Voe front-end domains look like: "maryspecialwatch.com",
    // "cactusheadroomscaling.com", "emberexactly.com"
    // Pattern: 3-4 concatenated lowercase English words + .com
    // Length: typically 15-35 chars before .com
    // No hyphens, no numbers, only a-z letters
    if host_lower.ends_with(".com") {
        let name = &host_lower[..host_lower.len() - 4]; // strip .com
        let len = name.len();
        // Voe front-end domains are typically 15-35 chars of pure lowercase
        if (12..=40).contains(&len)
            && name.chars().all(|c| c.is_ascii_lowercase())
            && !is_well_known_domain(name)
        {
            // Additional heuristic: Voe domain names are composed of
            // common English words concatenated. Real words have vowels.
            // English text typically has 30-45% vowels. Voe domain names
            // (being concatenated English words) follow this pattern.
            // Random letter strings have ~19% vowels (5/26).
            let vowel_count = name.chars().filter(|c| "aeiou".contains(*c)).count();
            let vowel_ratio = vowel_count as f64 / len as f64;
            if vowel_ratio >= 0.20 {
                return true;
            }
        }
    }

    false
}

/// Check if a domain name (without TLD) is a well-known domain that
/// should NOT be flagged as a potential Voe front-end.
fn is_well_known_domain(name: &str) -> bool {
    const WELL_KNOWN: &[&str] = &[
        "google",
        "youtube",
        "facebook",
        "twitter",
        "instagram",
        "amazon",
        "microsoft",
        "apple",
        "netflix",
        "reddit",
        "twitch",
        "vimeo",
        "dailymotion",
        "tiktok",
        "pinterest",
        "tumblr",
        "linkedin",
        "whatsapp",
        "telegram",
        "discord",
    ];
    WELL_KNOWN.contains(&name)
}

/// Check if a hostname should be handled by the DoodStream custom resolver.
pub fn is_doodstream_domain(host: &str) -> bool {
    let host_lower = host.to_lowercase();
    DOODSTREAM_DOMAINS.iter().any(|d| host_lower == *d || host_lower.ends_with(&format!(".{}", d)))
}

/// Resolve a Voe (or Voe CDN front-end) URL to a direct media URL.
///
/// 1. Follow JavaScript redirects (e.g. `voe.sx` → `charlessheimprove.com`).
/// 2. Try Method 8: decode the obfuscated JSON in `<script type="application/json">`.
/// 3. Try Method 7: decode the MKGMa-encoded source.
/// 4. Try Method 6: decode the `a168c` Base64-encoded source.
/// 5. Fallback: search for `var source = '...'` and direct `.mp4` URLs (no HLS).
///
/// `socks5_proxy` should be a full SOCKS5h proxy URL with isolation
/// username, e.g. `socks5h://bogdan-abc123@127.0.0.1:9050`. This ensures
/// the page fetch goes through the same Tor circuit as the media fetch,
/// so the CDN's IP-bound token matches.
pub async fn resolve_voe(
    url: &str,
    socks5_proxy: Option<&str>,
) -> Result<ResolveResult, ResolveError> {
    let (client, _forwarder) = build_client(socks5_proxy).await?;
    // _forwarder keeps the local HTTP→SOCKS5 proxy alive for the
    // duration of the resolve. It's dropped (and shut down) when
    // this function returns.

    let mut all_cookies: Vec<String> = Vec::new();

    // Follow the initial URL, then check for JS redirects.
    let (html_text, cookies) = fetch_page(&client, url, None).await?;
    all_cookies.extend(cookies);
    let resolved_url = follow_js_redirect(&html_text, url);

    // If we got redirected, fetch the new page.
    let (final_url, page_html) = if resolved_url != url {
        let (new_html, cookies) = fetch_page(&client, &resolved_url, Some(url)).await?;
        all_cookies.extend(cookies);
        (resolved_url, new_html)
    } else {
        (url.to_owned(), html_text)
    };

    // Extract metadata from the parsed HTML document.
    // NOTE: Html is NOT Send (contains Cell<usize> inside tendril), so we
    // must drop it before any .await point. We scope it in a block and
    // extract only the String values we need.
    let (title, thumbnail) = {
        let document = Html::parse_document(&page_html);
        let title = extract_meta_content(&document, "og:title")
            .or_else(|| extract_meta_content(&document, "twitter:title"))
            .or_else(|| extract_document_title(&document));
        let thumbnail = extract_meta_content(&document, "og:image")
            .or_else(|| extract_meta_content(&document, "twitter:image"));
        (title, thumbnail)
    };

    // Extract the file_code from the URL path for the /engine/update POST.
    // The file_code is the last path segment (e.g., "8aqd75zlo0et" from
    // "https://maryspecialwatch.com/8aqd75zlo0et").
    let file_code = url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|segments| segments.last().map(|s| s.to_string()))
        })
        .filter(|s| !s.is_empty());

    // Try Method 8: obfuscated JSON in <script type="application/json">
    if let Some(media_url) = try_method8(&page_html) {
        tracing::info!(url = %media_url, method = "method8", "Voe: resolved media URL");
        // Send /engine/update POST to activate the CDN session.
        // Without this POST, the CDN may reject the download URL (403/404)
        // because the server hasn't registered the user's session.
        voe_engine_update(&client, &final_url, file_code.as_deref(), &page_html).await;
        let mut result = build_result(&final_url, &media_url, &title, &thumbnail);
        result.cookies = all_cookies;
        return Ok(result);
    }

    // Try Method 7: MKGMa-encoded source
    if let Some(media_url) = try_method7(&page_html) {
        tracing::info!(url = %media_url, method = "method7", "Voe: resolved media URL");
        voe_engine_update(&client, &final_url, file_code.as_deref(), &page_html).await;
        let mut result = build_result(&final_url, &media_url, &title, &thumbnail);
        result.cookies = all_cookies;
        return Ok(result);
    }

    // Try Method 6: a168c Base64-encoded source
    if let Some(media_url) = try_method6(&page_html) {
        tracing::info!(url = %media_url, method = "method6", "Voe: resolved media URL");
        voe_engine_update(&client, &final_url, file_code.as_deref(), &page_html).await;
        let mut result = build_result(&final_url, &media_url, &title, &thumbnail);
        result.cookies = all_cookies;
        return Ok(result);
    }

    // Fallback: look for var source = '...' and direct URLs
    if let Some(media_url) = try_fallback_urls(&page_html) {
        tracing::info!(url = %media_url, method = "fallback", "Voe: resolved media URL");
        voe_engine_update(&client, &final_url, file_code.as_deref(), &page_html).await;
        let mut result = build_result(&final_url, &media_url, &title, &thumbnail);
        result.cookies = all_cookies;
        return Ok(result);
    }

    Err(ResolveError::NoMediaFound(format!(
        "Voe resolver: could not extract media URL from {}",
        final_url
    )))
}

/// Resolve a DoodStream (or DoodStream front-end) URL to a direct media URL.
///
/// 1. Fetch the page and look for an embed iframe (`/e/...`).
/// 2. Fetch the embed page and look for the `download` link.
/// 3. Extract the direct media URL from the download page.
///
/// `socks5_proxy` should be a full SOCKS5h proxy URL with isolation
/// username, e.g. `socks5h://bogdan-abc123@127.0.0.1:9050`. This ensures
/// the page fetch goes through the same Tor circuit as the media fetch,
/// so the CDN's IP-bound token matches.
pub async fn resolve_doodstream(
    url: &str,
    socks5_proxy: Option<&str>,
) -> Result<ResolveResult, ResolveError> {
    let (client, _forwarder) = build_client(socks5_proxy).await?;
    // _forwarder keeps the local HTTP→SOCKS5 proxy alive for the
    // duration of the resolve. It's dropped (and shut down) when
    // this function returns.

    let mut all_cookies: Vec<String> = Vec::new();

    // DoodStream pages often sit behind Cloudflare.  The main /d/ page may
    // return 403, but the /e/ (embed) page is usually less protected because
    // it is designed to be loaded in cross-origin iframes.  We try the main
    // page first; if it fails we fall through to the embed-URL heuristic
    // below.

    let mut title: Option<String> = None;
    let mut thumbnail: Option<String> = None;
    let mut embed_href: Option<String> = None;

    match fetch_page(&client, url, None).await {
        Ok((html_text, cookies)) => {
            all_cookies.extend(cookies);
            // Extract all data from the Html document up-front, then drop it.
            // scraper::Html is !Send (contains Cell<usize>), so it must not
            // survive across an .await point.
            let (t, th, e) = {
                let document = Html::parse_document(&html_text);
                let t = extract_meta_content(&document, "og:title")
                    .or_else(|| extract_meta_content(&document, "twitter:title"))
                    .or_else(|| extract_document_title(&document));
                let th = extract_meta_content(&document, "og:image")
                    .or_else(|| extract_meta_content(&document, "twitter:image"));
                let e = find_embed_iframe(&document, url);
                (t, th, e)
            };
            title = t;
            thumbnail = th;
            embed_href = e;

            // If we already have a direct URL on the main page, return it.
            if let Some(media_url) = try_fallback_urls(&html_text) {
                tracing::info!(url = %media_url, method = "main-page-fallback", "DoodStream: resolved media URL");
                let mut result = build_result(url, &media_url, &title, &thumbnail);
                result.cookies = all_cookies;
                return Ok(result);
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "DoodStream: main page fetch failed, trying embed URL heuristic");
        },
    }

    // ── Embed-URL heuristic ──────────────────────────────────────────
    // DoodStream /d/<id> → /e/<id>.  If the main page didn't yield an
    // iframe src, or if it returned 403, derive the embed URL from the
    // original URL path.
    if embed_href.is_none() {
        if let Some(derived) = derive_embed_url(url) {
            tracing::info!(derived = %derived, "DoodStream: derived embed URL from /d/ → /e/");
            embed_href = Some(derived);
        }
    }

    if let Some(href) = embed_href {
        let full_embed = if href.starts_with("http") {
            href
        } else {
            format!(
                "{}://{}{}",
                url::Url::parse(url)
                    .ok()
                    .map(|u| u.scheme().to_string())
                    .unwrap_or_else(|| "https".into()),
                url::Url::parse(url)
                    .ok()
                    .and_then(|u| u.host_str().map(|h| h.to_string()))
                    .unwrap_or_else(|| "playmogo.com".into()),
                href
            )
        };

        tracing::info!(embed_url = %full_embed, "DoodStream: fetching embed page");

        // Fetch the embed page (send Referer to appear browser-like)
        match fetch_page(&client, &full_embed, Some(url)).await {
            Ok((embed_html, cookies)) => {
                all_cookies.extend(cookies);
                // Try to find the direct media URL in the embed page
                if let Some(media_url) = extract_doodstream_media(&embed_html, &full_embed) {
                    tracing::info!(url = %media_url, "DoodStream: resolved media URL via embed");
                    let mut result = build_result(url, &media_url, &title, &thumbnail);
                    result.cookies = all_cookies;
                    return Ok(result);
                }
                // Fallback: search the embed HTML for direct URLs
                if let Some(media_url) = try_fallback_urls(&embed_html) {
                    tracing::info!(url = %media_url, method = "embed-fallback", "DoodStream: resolved media URL");
                    let mut result = build_result(url, &media_url, &title, &thumbnail);
                    result.cookies = all_cookies;
                    return Ok(result);
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "DoodStream: embed page fetch also failed");
            },
        }
    }

    Err(ResolveError::NoMediaFound(format!(
        "DoodStream resolver: could not extract media URL from {}",
        url
    )))
}

/// Derive the DoodStream embed URL from a /d/ URL.
///
/// `https://playmogo.com/d/abc123` → `https://playmogo.com/e/abc123`
fn derive_embed_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let path = parsed.path();
    // Match /d/<id> pattern
    let re = regex_lite::Regex::new(r#"^/d/([^/]+)$"#).ok()?;
    let caps = re.captures(path)?;
    let id = caps.get(1)?.as_str();
    let base = format!("{}://{}", parsed.scheme(), parsed.host_str()?);
    Some(format!("{}/e/{}", base, id))
}

// ── Method 8: Obfuscated JSON decode ───────────────────────────────

/// Try Method 8: decode the obfuscated JSON blob found in
/// `<script type="application/json">` tags.
///
/// Decode pipeline: ROT13 → strip marker patterns → Base64 decode →
/// shift chars by -3 → reverse string → Base64 decode → JSON parse.
fn try_method8(html: &str) -> Option<String> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("script[type='application/json']").ok()?;
    for element in document.select(&selector) {
        let raw = element.text().collect::<String>();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(url) = deobfuscate_embedded_json(trimmed) {
            if !is_bait_source(&url) {
                return Some(url);
            }
        }
    }
    None
}

/// Decode the obfuscated JSON array found in Voe's
/// `<script type="application/json">` tags.
fn deobfuscate_embedded_json(raw_json: &str) -> Option<String> {
    let arr: Vec<String> = serde_json::from_str(raw_json).ok()?;
    if arr.is_empty() {
        return None;
    }
    let obf = &arr[0];

    // Step 1: ROT13
    let step1 = rot13(obf);
    // Step 2: Strip marker patterns
    let step2 = replace_patterns(&step1);
    // Step 3: Base64 decode
    let step3 = safe_b64_decode(&step2)?;
    // Step 4: Shift chars by -3
    let step4 = shift_chars(&step3, 3);
    // Step 5: Reverse
    let step5: String = step4.chars().rev().collect();
    // Step 6: Base64 decode again
    let step6 = safe_b64_decode(&step5)?;

    // Try to parse as JSON and extract media URL
    //
    // URL extraction priority:
    //   1. "mp4" key — direct MP4 URL (preferred, always works with appsrc)
    //   2. "direct_access_url" — CDN direct URL (may be HLS, skip if so)
    //   3. "source" — source URL (may be HLS, skip if so)
    //   4. Regex fallback — search for .mp4 URLs in the text
    //
    // HLS (.m3u8) URLs are NEVER returned because the appsrc pipeline
    // cannot handle them. HLS requires fetching multiple segment URLs,
    // which appsrc's sequential byte-stream model can't provide.
    //
    // The "mp4" key is tried FIRST because:
    //   - It always provides a direct MP4 URL (or multi-quality object)
    //   - The other keys ("direct_access_url", "source") may return HLS URLs
    //   - Previously, "direct_access_url" was tried first, causing the
    //     resolver to return HLS URLs when "mp4" was a multi-quality object
    //     (the .as_str() call failed on the object, falling through to "hls")
    //
    // CDN Authorization Token (&rq=):
    //
    // Voe's CDN requires an &rq= (request) query parameter for
    // authorization. Without it, the CDN returns 403 Forbidden on
    // the direct MP4 download URL ("direct_access_url"). The HLS
    // source URL already includes &rq=, but the direct_access_url
    // does NOT — it must be appended from the JSON's "request" field.
    //
    // This was discovered because Voe pages have
    // "direct_access_allowed": false and "check": true, meaning
    // the CDN validates the request token before allowing the
    // download. The &rq= token is the same value as the JSON's
    // "request" field and is also present as &rq= in the HLS
    // source URL.
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&step6) {
        if let Some(obj) = parsed.as_object() {
            // Extract the "request" token for CDN session activation.
            //
            // CRITICAL: Do NOT append &rq= to MP4 download URLs!
            //
            // The &rq= parameter is HLS-only — it's included in the
            // HLS source URL by the CDN server as part of the signed
            // URL. The CDN's &t= (signed token) parameter is computed
            // over the URL's query parameters. For MP4 download URLs
            // (direct_access_url, mp4), the &t= token was computed
            // WITHOUT &rq=. Appending &rq= changes the URL, which
            // invalidates the &t= signature → CDN returns 403/404.
            //
            // The "request" token is still used for the /engine/update
            // POST (session activation), but NOT appended to the URL.
            let request_token = obj
                .get("request")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_owned());

            // Also extract file_code for /engine/update POST.
            let file_code = obj
                .get("file_code")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_owned());

            if let Some(ref token) = request_token {
                tracing::info!(
                    rq_token = %token,
                    file_code = ?file_code,
                    "Voe method8: extracted 'request' token and file_code for CDN session activation"
                );
            }

            // Priority 1: "mp4" key — direct MP4 URL or multi-quality object
            // Do NOT append &rq= — it breaks the CDN's &t= signature.
            if let Some(mp4_val) = obj.get("mp4") {
                if let Some(url) = extract_media_from_json_value(mp4_val) {
                    tracing::info!(url = %url, "Voe method8: extracted MP4 URL from 'mp4' key (no &rq= appended — breaks CDN signature)");
                    return Some(url);
                }
            }

            // Priority 2: "direct_access_url" — allow HLS as fallback
            // Do NOT append &rq= — it breaks the CDN's &t= signature.
            if let Some(url) = obj.get("direct_access_url").and_then(|v| v.as_str()) {
                if !url.is_empty() {
                    if is_hls_url(url) {
                        tracing::info!(url = %url, "Voe method8: HLS URL in 'direct_access_url' — returning as fallback (StreamSource has HLS client)");
                    } else {
                        tracing::info!(url = %url, "Voe method8: extracted URL from 'direct_access_url' (no &rq= appended — breaks CDN signature)");
                    }
                    return Some(url.to_owned());
                }
            }

            // Priority 3: "source" — allow HLS as fallback
            // Do NOT append &rq= — the source URL already has it if needed,
            // and adding it would break non-HLS source URLs.
            // StreamSource now has an HLS client, so HLS URLs are returned.
            if let Some(url) = obj.get("source").and_then(|v| v.as_str()) {
                if !url.is_empty() {
                    if is_hls_url(url) {
                        tracing::info!(url = %url, "Voe method8: HLS URL in 'source' — returning as fallback (StreamSource has HLS client)");
                    } else {
                        tracing::info!(url = %url, "Voe method8: extracted URL from 'source' (using as-is)");
                    }
                    return Some(url.to_owned());
                }
            }

            // Priority 4: "hls" key — return HLS URL as final fallback
            // StreamSource now has an HLS client that can download segments
            // and feed them as MPEG-TS to appsrc/parsebin.
            if let Some(url) = obj.get("hls").and_then(|v| v.as_str()) {
                if !url.is_empty() {
                    tracing::info!(url = %url, "Voe method8: extracted URL from 'hls' key — returning as fallback (StreamSource has HLS client)");
                    return Some(url.to_owned());
                }
            }
        }
    }

    // Fallback: regex search for media URLs (MP4 first, then HLS)
    if let Some(url) = extract_mp4_url_from_text(&step6) {
        return Some(url);
    }
    extract_m3u8_url_from_text(&step6)
}

// ── Method 7: MKGMa-encoded source ────────────────────────────────

/// Try Method 7: decode the MKGMa-encoded source variable.
///
/// Decode pipeline: ROT13 → strip underscores → Base64 decode →
/// shift chars by -3 → reverse → Base64 decode.
fn try_method7(html: &str) -> Option<String> {
    let re = regex_lite::Regex::new(r#"MKGMa="(.*?)""#).ok()?;
    let cap = re.captures(html)?;
    let raw = cap.get(1)?.as_str();

    let step1 = rot13(raw);
    let step2 = step1.replace('_', "");
    let step3 = safe_b64_decode(&step2)?;
    let step4 = shift_chars(&step3, 3);
    let step5: String = step4.chars().rev().collect();
    let step6 = safe_b64_decode(&step5)?;

    // Try JSON parse — same priority as method 8:
    // mp4 first, then direct_access_url, then source. HLS allowed as fallback.
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&step6) {
        if let Some(obj) = parsed.as_object() {
            // Priority 1: "mp4" key
            if let Some(mp4_val) = obj.get("mp4") {
                if let Some(url) = extract_media_from_json_value(mp4_val) {
                    if !is_bait_source(&url) {
                        return Some(url);
                    }
                }
            }

            // Priority 2: "direct_access_url" — allow HLS as fallback
            if let Some(url) = obj.get("direct_access_url").and_then(|v| v.as_str()) {
                if !url.is_empty() && !is_bait_source(url) {
                    if is_hls_url(url) {
                        tracing::info!(url = %url, "Voe method7: HLS URL in 'direct_access_url' — returning as fallback (StreamSource has HLS client)");
                    }
                    return Some(url.to_owned());
                }
            }

            // Priority 3: "source" — allow HLS as fallback
            if let Some(url) = obj.get("source").and_then(|v| v.as_str()) {
                if !url.is_empty() && !is_bait_source(url) {
                    if is_hls_url(url) {
                        tracing::info!(url = %url, "Voe method7: HLS URL in 'source' — returning as fallback (StreamSource has HLS client)");
                    }
                    return Some(url.to_owned());
                }
            }

            // Priority 4: "hls" key — return HLS URL as final fallback
            if let Some(url) = obj.get("hls").and_then(|v| v.as_str()) {
                if !url.is_empty() && !is_bait_source(url) {
                    tracing::info!(url = %url, "Voe method7: extracted URL from 'hls' key — returning as fallback (StreamSource has HLS client)");
                    return Some(url.to_owned());
                }
            }
        }
    }

    // Fallback: regex search (MP4 first, then HLS)
    if let Some(url) = extract_mp4_url_from_text(&step6) {
        if !is_bait_source(&url) {
            return Some(url);
        }
    }
    if let Some(url) = extract_m3u8_url_from_text(&step6) {
        if !is_bait_source(&url) {
            return Some(url);
        }
    }
    None
}

// ── Method 6: a168c Base64-encoded source ─────────────────────────

/// Try Method 6: decode the `a168c` Base64-encoded source.
///
/// Decode pipeline: clean Base64 → decode → reverse → JSON parse or
/// regex search.
fn try_method6(html: &str) -> Option<String> {
    let re = regex_lite::Regex::new(r#"a168c\s*=\s*'([^']+)'"#).ok()?;
    let cap = re.captures(html)?;
    let raw = cap.get(1)?.as_str();

    let cleaned = clean_base64(raw)?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&cleaned)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())?;
    let reversed: String = decoded.chars().rev().collect();

    // Try JSON parse — same priority as method 8:
    // mp4 first, then direct_access_url, then source. HLS allowed as fallback.
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&reversed) {
        if let Some(obj) = parsed.as_object() {
            // Priority 1: "mp4" key
            if let Some(mp4_val) = obj.get("mp4") {
                if let Some(url) = extract_media_from_json_value(mp4_val) {
                    if !is_bait_source(&url) {
                        return Some(url);
                    }
                }
            }

            // Priority 2: "direct_access_url" — allow HLS as fallback
            if let Some(url) = obj.get("direct_access_url").and_then(|v| v.as_str()) {
                if !url.is_empty() && !is_bait_source(url) {
                    if is_hls_url(url) {
                        tracing::info!(url = %url, "Voe method6: HLS URL in 'direct_access_url' — returning as fallback (StreamSource has HLS client)");
                    }
                    return Some(url.to_owned());
                }
            }

            // Priority 3: "source" — allow HLS as fallback
            if let Some(url) = obj.get("source").and_then(|v| v.as_str()) {
                if !url.is_empty() && !is_bait_source(url) {
                    if is_hls_url(url) {
                        tracing::info!(url = %url, "Voe method6: HLS URL in 'source' — returning as fallback (StreamSource has HLS client)");
                    }
                    return Some(url.to_owned());
                }
            }

            // Priority 4: "hls" key — return HLS URL as final fallback
            if let Some(url) = obj.get("hls").and_then(|v| v.as_str()) {
                if !url.is_empty() && !is_bait_source(url) {
                    tracing::info!(url = %url, "Voe method6: extracted URL from 'hls' key — returning as fallback (StreamSource has HLS client)");
                    return Some(url.to_owned());
                }
            }
        }
    }

    // Fallback: regex search (MP4 first, then HLS)
    if let Some(url) = extract_mp4_url_from_text(&reversed) {
        if !is_bait_source(&url) {
            return Some(url);
        }
    }
    if let Some(url) = extract_m3u8_url_from_text(&reversed) {
        if !is_bait_source(&url) {
            return Some(url);
        }
    }
    None
}

// ── DoodStream-specific extraction ─────────────────────────────────

/// Find the `/e/` embed iframe URL in a DoodStream page.
fn find_embed_iframe(document: &Html, _base_url: &str) -> Option<String> {
    let iframe_selector = Selector::parse("iframe").ok()?;

    for iframe in document.select(&iframe_selector) {
        if let Some(src) = iframe.value().attr("src") {
            if src.starts_with("/e/") || src.contains("/e/") {
                return Some(src.to_owned());
            }
        }
    }
    None
}

/// Extract the direct media URL from the DoodStream embed page HTML.
fn extract_doodstream_media(embed_html: &str, embed_url: &str) -> Option<String> {
    // Look for the pass/dl API pattern that DoodStream uses
    // The embed page typically makes a request to /pass_md5/... endpoint
    // and constructs the download URL

    // Try to find the pass_md5 token in the page
    let re = regex_lite::Regex::new(r#"/pass_md5/([^"'\s]+)"#).ok()?;
    if let Some(cap) = re.captures(embed_html) {
        let pass_path = cap.get(1)?.as_str();

        // Construct the base URL from the embed URL
        let parsed = url::Url::parse(embed_url).ok()?;
        let base = format!("{}://{}", parsed.scheme(), parsed.host_str()?);

        // The pass_md5 URL returns the CDN base, then we append filename + token
        let pass_url = format!("{}/pass_md5/{}", base, pass_path);

        // For now, return the pass URL; the actual CDN URL is constructed
        // by fetching this endpoint. This is a simplification — a full
        // implementation would make the HTTP request here.
        if !is_bait_source(&pass_url) {
            return Some(pass_url);
        }
    }

    // Try to find direct .mp4 URL in the page (no HLS — appsrc can't handle it)
    extract_mp4_url_from_text(embed_html)
}

// ── Fallback URL extraction ────────────────────────────────────────

/// Try fallback methods: look for `var source = '...'`, direct mp4 URLs.
///
/// HLS (.m3u8) URLs are intentionally EXCLUDED because the appsrc pipeline
/// cannot handle them. Only direct MP4 URLs are returned.
fn try_fallback_urls(html: &str) -> Option<String> {
    // Look for var source = 'https://...' — skip if HLS
    let re = regex_lite::Regex::new(r#"var\s+source\s*=\s*['"]([^'"]+)['"]"#).ok()?;
    if let Some(cap) = re.captures(html) {
        let url = cap.get(1)?.as_str();
        if !is_bait_source(url) && !url.is_empty() && !is_hls_url(url) {
            return Some(url.to_owned());
        }
        if is_hls_url(url) {
            tracing::debug!(url = %url, "Voe fallback: skipping HLS URL in var source");
        }
    }

    // Look for direct mp4 URLs (preferred — works with appsrc pipeline)
    let re_mp4 = regex_lite::Regex::new(r#"(https?://[^"'<>]+\.mp4[^"'<>\s]*)"#).ok()?;
    for cap in re_mp4.captures_iter(html) {
        if let Some(m) = cap.get(1) {
            let url = m.as_str();
            if !is_bait_source(url) {
                return Some(url.to_owned());
            }
        }
    }

    // NOTE: We intentionally do NOT search for .m3u8 URLs here.
    // The appsrc + StreamSource pipeline cannot handle HLS streams —
    // HLS requires fetching multiple segment URLs, which appsrc's
    // sequential byte-stream model can't provide. Returning an HLS URL
    // would result in the pipeline downloading the tiny playlist text
    // and pushing it into appsrc, where parsebin either fails to parse
    // it or hlsdemux can't fetch the segments (no souphttpsrc available).

    None
}

// ── Helper functions ───────────────────────────────────────────────

/// Apply ROT13 cipher (letters only).
fn rot13(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch.is_ascii_uppercase() {
                let o = ch as u32;
                char::from_u32(((o - 65 + 13) % 26) + 65).unwrap_or(ch)
            } else if ch.is_ascii_lowercase() {
                let o = ch as u32;
                char::from_u32(((o - 97 + 13) % 26) + 97).unwrap_or(ch)
            } else {
                ch
            }
        })
        .collect()
}

/// Strip marker substrings used as obfuscation separators.
fn replace_patterns(txt: &str) -> String {
    let mut result = txt.to_owned();
    for pat in &["@$", "^^", "~@", "%?", "*~", "!!", "#&"] {
        result = result.replace(pat, "");
    }
    result
}

/// Shift character code-points by `-shift` (decode).
fn shift_chars(text: &str, shift: u32) -> String {
    text.chars()
        .map(|c| {
            let code = c as u32;
            if code >= shift {
                char::from_u32(code - shift).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

/// Base64 decode with safe padding.
fn safe_b64_decode(s: &str) -> Option<String> {
    let padded = {
        let pad = s.len() % 4;
        let mut s = s.to_owned();
        if pad > 0 {
            for _ in 0..(4 - pad) {
                s.push('=');
            }
        }
        s
    };
    base64::engine::general_purpose::STANDARD
        .decode(&padded)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
}

/// Clean and pad a Base64 string for safe decoding.
fn clean_base64(s: &str) -> Option<String> {
    let cleaned = s.replace('\\', "");
    let pad = cleaned.len() % 4;
    let padded = if pad > 0 {
        let mut s = cleaned;
        for _ in 0..(4 - pad) {
            s.push('=');
        }
        s
    } else {
        cleaned
    };
    // Validate
    base64::engine::general_purpose::STANDARD.decode(&padded).ok()?;
    Some(padded)
}

fn is_bait_source(source: &str) -> bool {
    let lower = source.to_lowercase();
    if BAIT_FILENAMES.iter().any(|fn_| lower.contains(&fn_.to_lowercase())) {
        return true;
    }
    if let Ok(parsed) = url::Url::parse(source) {
        if let Some(host) = parsed.host_str() {
            if BAIT_DOMAINS.iter().any(|d| host.contains(d)) {
                return true;
            }
        }
    }
    false
}

/// Extract a direct MP4 media URL from arbitrary text using regex.
///
/// Only returns .mp4 URLs — .m3u8 (HLS) URLs are excluded because the
/// StreamSource now has an HLS client, but MP4 is still preferred.
fn extract_mp4_url_from_text(text: &str) -> Option<String> {
    // Try mp4 only
    let re_mp4 = regex_lite::Regex::new(r#"(https?://[^\s"']+\.mp4[^\s"']*)"#).ok()?;
    for cap in re_mp4.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let url = m.as_str();
            if !is_bait_source(url) {
                return Some(url.to_owned());
            }
        }
    }

    // If no MP4 URL is found, return None (caller may try extract_m3u8_url_from_text).
    None
}

/// Extract an HLS (.m3u8) playlist URL from arbitrary text.
///
/// Used as a fallback when no MP4 URL is found. StreamSource's HLS client
/// will fetch the master playlist, select the best quality variant, and
/// download each .ts segment sequentially.
fn extract_m3u8_url_from_text(text: &str) -> Option<String> {
    let re_m3u8 = regex_lite::Regex::new(r#"(https?://[^\s"']+\.m3u8[^\s"']*)"#).ok()?;
    for cap in re_m3u8.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let url = m.as_str();
            if !is_bait_source(url) {
                tracing::info!(url = %url, "Voe: found .m3u8 URL in text — returning as fallback (StreamSource has HLS client)");
                return Some(url.to_owned());
            }
        }
    }
    None
}

/// Fetch a page's HTML content via HTTP GET with browser-like headers.
///
/// `referer` is sent as the Referer header when provided (helps bypass
/// hotlink-protection on embed pages).
///
/// Returns a tuple of (HTML content, cookies from Set-Cookie headers).
async fn fetch_page(
    client: &reqwest::Client,
    url: &str,
    referer: Option<&str>,
) -> Result<(String, Vec<String>), ResolveError> {
    let mut req = client
        .get(url)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        )
        .header("Accept-Language", "en-US,en;q=0.5")
        .header("Sec-Fetch-Dest", "document")
        .header("Sec-Fetch-Mode", "navigate")
        .header("Sec-Fetch-Site", if referer.is_some() { "same-origin" } else { "none" })
        .header("Upgrade-Insecure-Requests", "1");

    if let Some(ref_url) = referer {
        req = req.header("Referer", ref_url);
    }

    let result = timeout(Duration::from_secs(CUSTOM_RESOLVER_TIMEOUT_SECS), req.send())
        .await
        .map_err(|_| ResolveError::Network("custom resolver: HTTP request timed out".into()))?
        .map_err(|e| {
            ResolveError::Network(format!("custom resolver: HTTP request failed: {}", e))
        })?;

    let status = result.status();
    if !status.is_success() {
        // Log response body for debugging (truncated).
        let body = result.text().await.unwrap_or_default();
        let snippet = if body.len() > 500 { &body[..500] } else { &body };
        tracing::warn!(status = %status, url = url, body_snippet = snippet, "custom resolver: non-2xx response");
        return Err(ResolveError::Network(format!("custom resolver: HTTP {} for {}", status, url)));
    }

    // Capture Set-Cookie headers for CDN session cookies
    let cookies: Vec<String> = result
        .headers()
        .iter()
        .filter(|(name, _)| *name == "set-cookie")
        .filter_map(|(_, value)| value.to_str().ok())
        .map(|v| {
            // Extract just the cookie name=value part (before the first ;)
            v.split(';').next().unwrap_or(v).trim().to_string()
        })
        .collect();

    if !cookies.is_empty() {
        tracing::info!(
            cookie_count = cookies.len(),
            url = url,
            "custom resolver: captured cookies from HTTP response"
        );
    }

    let html = result.text().await.map_err(|e| {
        ResolveError::Network(format!("custom resolver: failed to read response: {}", e))
    })?;

    Ok((html, cookies))
}

/// Send a POST to `/engine/update` on the Voe domain to activate the CDN
/// download session.
///
/// Voe's JavaScript makes this POST before the video player starts. The POST
/// sends telemetry data (bot detection, fingerprint, GPU check results) that
/// tells the Voe server "a real browser loaded this page." Without this POST,
/// the CDN may reject the download URL with 403/404 — the server hasn't
/// registered the user's session, so the CDN's signed URL is not yet
/// "activated."
///
/// The POST data is encoded using Voe's custom obfuscation:
/// JSON.stringify → Base64 encode → Reverse string → Shift charCode by +3
///
/// We send minimal but plausible telemetry data. The CDN likely just checks
/// that a POST was made with the correct `file_code`, not the exact telemetry
/// content.
async fn voe_engine_update(
    client: &reqwest::Client,
    page_url: &str,
    file_code: Option<&str>,
    _page_html: &str,
) {
    // Construct the /engine/update URL from the page's domain.
    let update_url = match url::Url::parse(page_url) {
        Ok(parsed) => {
            let base = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or("localhost"));
            format!("{}/engine/update", base)
        },
        Err(_) => {
            tracing::debug!("voe_engine_update: could not parse page URL, skipping");
            return;
        },
    };

    // Build the telemetry payload. The field names are obfuscated on Voe's
    // side (a1b, c3d, e5f, g7h, i9j, k1l, m2n, o3p, etc.). We send the
    // minimum required fields with plausible values:
    //   - c3d: file_code (video ID)
    //   - g7h: bot detection result (0 = not a bot)
    //   - i9j: fingerprint result (empty/fake)
    //   - k1l: GPU check result (fake GPU name)
    let payload = serde_json::json!({
        "c3d": file_code.unwrap_or(""),
        "g7h": "0",
        "i9j": "",
        "k1l": "ANGLE (NVIDIA, NVIDIA GeForce GTX 1060 6GB Direct3D11 vs_5_0 ps_5_0, D3D11)",
        "a1b": "",
        "e5f": "",
        "m2n": "",
        "o3p": ""
    });

    // Encode the payload using Voe's custom obfuscation:
    // 1. JSON.stringify
    // 2. Base64 encode
    // 3. Reverse string
    // 4. Shift each character code by +3
    let json_str = match serde_json::to_string(&payload) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, "voe_engine_update: failed to serialize payload");
            return;
        },
    };

    let b64 = base64::engine::general_purpose::STANDARD.encode(&json_str);
    let reversed: String = b64.chars().rev().collect();
    let encoded: String = reversed.chars().map(|c| char::from_u32(c as u32 + 3).unwrap_or(c)).collect();

    tracing::info!(
        update_url = %update_url,
        file_code = ?file_code,
        "Voe: sending /engine/update POST to activate CDN session"
    );

    // Send the POST. We don't treat failure as fatal — the CDN URL might
    // work without this POST (some Voe pages don't require it). But if it
    // fails, we log a warning so we can diagnose.
    let req = client
        .post(&update_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Content-Cache", "no-cache")
        .header("Accept", "*/*")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Sec-Fetch-Dest", "empty")
        .header("Sec-Fetch-Mode", "cors")
        .header("Sec-Fetch-Site", "same-origin")
        .header("Referer", page_url)
        .body(format!("data={}", encoded));

    match timeout(Duration::from_secs(CUSTOM_RESOLVER_TIMEOUT_SECS), req.send()).await {
        Ok(Ok(response)) => {
            let status = response.status();
            tracing::info!(
                status = %status,
                "Voe: /engine/update POST completed — CDN session should be activated"
            );
        },
        Ok(Err(e)) => {
            tracing::warn!(
                error = %e,
                "Voe: /engine/update POST failed (network error) — CDN download may fail"
            );
        },
        Err(_) => {
            tracing::warn!(
                "Voe: /engine/update POST timed out — CDN download may fail"
            );
        },
    }
}

/// Follow JavaScript `window.location.href = '...'` redirects in the HTML.
/// Returns the redirected URL if found, otherwise the original URL.
fn follow_js_redirect(html: &str, original_url: &str) -> String {
    let patterns = [
        r#"window\.location\.href\s*=\s*['"]([^'"]+)['"]"#,
        r#"window\.location\s*=\s*['"]([^'"]+)['"]"#,
        r#"location\.href\s*=\s*['"]([^'"]+)['"]"#,
        r#"window\.location\.replace\(['"]([^'"]+)['"]\)"#,
    ];

    for pattern in &patterns {
        if let Ok(re) = regex_lite::Regex::new(pattern) {
            if let Some(cap) = re.captures(html) {
                if let Some(m) = cap.get(1) {
                    let redirect_url = m.as_str();
                    if redirect_url.starts_with("http") {
                        return redirect_url.to_owned();
                    }
                    // Relative URL — resolve against original
                    if let Ok(base) = url::Url::parse(original_url) {
                        if let Ok(joined) = base.join(redirect_url) {
                            return joined.to_string();
                        }
                    }
                }
            }
        }
    }

    original_url.to_owned()
}

/// Extract the content attribute of a `<meta>` tag by property or name.
fn extract_meta_content(document: &Html, property: &str) -> Option<String> {
    let selector_str = format!("meta[property='{}'], meta[name='{}']", property, property);
    let selector = Selector::parse(&selector_str).ok()?;
    for element in document.select(&selector) {
        if let Some(content) = element.value().attr("content") {
            if !content.is_empty() {
                return Some(content.to_owned());
            }
        }
    }
    None
}

/// Extract the `<title>` text from the document.
fn extract_document_title(document: &Html) -> Option<String> {
    let selector = Selector::parse("title").ok()?;
    document
        .select(&selector)
        .next()
        .map(|el| el.text().collect::<String>().trim().to_owned())
        .filter(|t| !t.is_empty())
}

/// Build a `ResolveResult` from the extracted data.
fn build_result(
    source_url: &str,
    media_url: &str,
    title: &Option<String>,
    thumbnail: &Option<String>,
) -> ResolveResult {
    let is_hls = media_url.contains(".m3u8");
    let mime_type = if is_hls {
        Some("application/vnd.apple.mpegurl".to_string())
    } else {
        Some("video/mp4".to_string())
    };

    let category = if is_hls {
        UrlCategory::HlsManifest
    } else {
        UrlCategory::DirectMedia
    };

    if is_hls {
        tracing::warn!(
            url = %media_url,
            "Voe resolver: returned HLS URL — this should not happen! \
             The appsrc pipeline cannot handle HLS streams. \
             The mp4-first priority logic should have prevented this."
        );
    } else {
        tracing::info!(
            url = %media_url,
            "Voe resolver: resolved direct MP4 URL (compatible with appsrc pipeline)"
        );
    }

    ResolveResult {
        source_url: source_url.to_owned(),
        direct_url: media_url.to_owned(),
        audio_url: None,
        category,
        mime_type,
        content_length: None,
        used_tor: false,
        title: title.clone(),
        duration: None,
        thumbnail: thumbnail.clone(),
        vcodec: None,
        acodec: None,
        width: None,
        height: None,
        subtitle_tracks: vec![],
        cookies: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rot13() {
        assert_eq!(rot13("Hello"), "Uryyb");
        assert_eq!(rot13("Uryyb"), "Hello");
        assert_eq!(rot13("abc123"), "nop123");
    }

    #[test]
    fn test_is_hls_url() {
        assert!(is_hls_url("https://cdn.example.com/stream.m3u8"));
        assert!(is_hls_url("https://cdn.example.com/stream.m3u8?token=abc"));
        assert!(is_hls_url("https://cdn.example.com/stream.M3U8")); // case-insensitive
        assert!(!is_hls_url("https://cdn.example.com/video.mp4"));
        assert!(!is_hls_url("https://cdn.example.com/video.mp4?token=abc"));
    }

    #[test]
    fn test_extract_media_from_json_value_string() {
        // Simple string URL — should return it
        let value = serde_json::json!("https://cdn.example.com/video.mp4?token=abc");
        let result = extract_media_from_json_value(&value);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "https://cdn.example.com/video.mp4?token=abc");
    }

    #[test]
    fn test_extract_media_from_json_value_skips_hls() {
        // HLS string URL — should be skipped
        let value = serde_json::json!("https://cdn.example.com/stream.m3u8?token=abc");
        let result = extract_media_from_json_value(&value);
        assert!(result.is_none(), "HLS URLs should be skipped");
    }

    #[test]
    fn test_extract_media_from_json_value_object() {
        // Multi-quality object — should prefer 720p
        let value = serde_json::json!({
            "1080": "https://cdn.example.com/1080.mp4",
            "720": "https://cdn.example.com/720.mp4",
            "480": "https://cdn.example.com/480.mp4"
        });
        let result = extract_media_from_json_value(&value);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "https://cdn.example.com/720.mp4");
    }

    #[test]
    fn test_extract_media_from_json_value_object_hls_only() {
        // Multi-quality object with only HLS URLs — should return None
        let value = serde_json::json!({
            "720": "https://cdn.example.com/720.m3u8",
            "480": "https://cdn.example.com/480.m3u8"
        });
        let result = extract_media_from_json_value(&value);
        assert!(result.is_none(), "HLS-only quality objects should return None");
    }

    #[test]
    fn test_extract_media_from_json_value_object_mixed() {
        // Multi-quality object with some HLS, some MP4 — should skip HLS
        let value = serde_json::json!({
            "1080": "https://cdn.example.com/1080.m3u8",
            "720": "https://cdn.example.com/720.mp4"
        });
        let result = extract_media_from_json_value(&value);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "https://cdn.example.com/720.mp4");
    }

    #[test]
    fn test_replace_patterns() {
        let input = "DROH#&nJjm^^AJIg%?BSMq~@AGkj";
        let result = replace_patterns(input);
        assert!(!result.contains("#&"));
        assert!(!result.contains("^^"));
        assert!(!result.contains("%?"));
        assert!(!result.contains("~@"));
    }

    #[test]
    fn test_shift_chars() {
        let input = "def";
        let result = shift_chars(input, 3);
        assert_eq!(result, "abc");
    }

    #[test]
    fn test_is_bait_source() {
        assert!(is_bait_source("https://test-videos.co.uk/vids/bigbuckbunny/mp4/av1/1080/Big_Buck_Bunny_1080_10s_5MB.mp4"));
        assert!(is_bait_source("https://sample-videos.com/video.mp4"));
        assert!(!is_bait_source("https://cdn.example.com/video.mp4"));
    }

    #[test]
    fn test_is_voe_domain() {
        // Canonical domain
        assert!(is_voe_domain("voe.sx"));
        // Unblock proxies
        assert!(is_voe_domain("voe-unblock.com"));
        assert!(is_voe_domain("voeunblock.com"));
        // Heuristic: Voe-style concatenated word domains
        assert!(is_voe_domain("charlessheimprove.com"));
        assert!(is_voe_domain("brittanyaheadnew.com"));
        assert!(is_voe_domain("maryspecialwatch.com"));
        assert!(is_voe_domain("maxfinishseveral.com"));
        assert!(is_voe_domain("cactusheadroomscaling.com"));
        assert!(is_voe_domain("emberexactly.com"));
        assert!(is_voe_domain("butterflyblow.com"));
        assert!(is_voe_domain("antelopeheat.com"));
        assert!(is_voe_domain("lightninglight.com"));
        assert!(is_voe_domain("smartfityoga.com"));
        // Subdomain of known domain
        assert!(is_voe_domain("sub.voe.sx"));
        // NOT Voe domains
        assert!(!is_voe_domain("youtube.com"));
        assert!(!is_voe_domain("google.com"));
        assert!(!is_voe_domain("vimeo.com"));
    }

    #[test]
    fn test_append_rq_token() {
        // Basic case: append &rq= to URL with existing query params
        let url = "https://cdn.example.com/video.mp4?t=abc&sp=380";
        let result = append_rq_token(url, Some("REQ123"));
        assert_eq!(result, "https://cdn.example.com/video.mp4?t=abc&sp=380&rq=REQ123");

        // URL already has &rq= — should not add another
        let url_with_rq = "https://cdn.example.com/video.mp4?t=abc&rq=EXISTING";
        let result = append_rq_token(url_with_rq, Some("REQ123"));
        assert_eq!(result, url_with_rq);

        // URL already has ?rq= — should not add another
        let url_with_rq_start = "https://cdn.example.com/video.mp4?rq=EXISTING";
        let result = append_rq_token(url_with_rq_start, Some("REQ123"));
        assert_eq!(result, url_with_rq_start);

        // No token provided — return URL unchanged
        let url_no_token = "https://cdn.example.com/video.mp4?t=abc";
        let result = append_rq_token(url_no_token, None);
        assert_eq!(result, url_no_token);

        // Empty token — return URL unchanged
        let result = append_rq_token(url_no_token, Some(""));
        assert_eq!(result, url_no_token);

        // URL without any query params — use ? separator
        let url_no_query = "https://cdn.example.com/video.mp4";
        let result = append_rq_token(url_no_query, Some("REQ123"));
        assert_eq!(result, "https://cdn.example.com/video.mp4?rq=REQ123");
    }

    #[test]
    fn test_is_doodstream_domain() {
        assert!(is_doodstream_domain("playmogo.com"));
        assert!(is_doodstream_domain("doodstream.com"));
        assert!(!is_doodstream_domain("youtube.com"));
    }

    #[test]
    fn test_follow_js_redirect() {
        let html =
            r#"<script>window.location.href = 'https://charlessheimprove.com/abc';</script>"#;
        let result = follow_js_redirect(html, "https://voe.sx/abc");
        assert_eq!(result, "https://charlessheimprove.com/abc");
    }

    #[test]
    fn test_follow_js_redirect_no_redirect() {
        let html = r#"<html><body>Normal page</body></html>"#;
        let result = follow_js_redirect(html, "https://example.com/page");
        assert_eq!(result, "https://example.com/page");
    }

    #[test]
    fn test_extract_mp4_url_from_text_mp4() {
        let text = "Some text with https://cdn.example.com/video123.mp4?token=abc embedded";
        let result = extract_mp4_url_from_text(text);
        assert!(result.is_some());
        assert!(result.unwrap().contains(".mp4"));
    }

    #[test]
    fn test_extract_mp4_url_from_text_ignores_m3u8() {
        // HLS URLs should NOT be returned — the appsrc pipeline can't handle them
        let text = "Source: https://cdn.example.com/stream.m3u8?session=xyz";
        let result = extract_mp4_url_from_text(text);
        assert!(result.is_none(), "HLS URLs should not be returned by extract_mp4_url_from_text");
    }

    #[test]
    fn test_extract_mp4_url_ignores_bait() {
        let text = "https://test-videos.co.uk/vids/bigbuckbunny/Big_Buck_Bunny_1080_10s_5MB.mp4";
        let result = extract_mp4_url_from_text(text);
        assert!(result.is_none());
    }

    #[test]
    fn test_build_result_mp4() {
        let result = build_result(
            "https://voe.sx/abc",
            "https://cdn.example.com/video.mp4",
            &Some("Test Video".into()),
            &None,
        );
        assert_eq!(result.category, UrlCategory::DirectMedia);
        assert_eq!(result.mime_type, Some("video/mp4".into()));
        assert_eq!(result.title, Some("Test Video".into()));
    }

    #[test]
    fn test_build_result_m3u8() {
        let result =
            build_result("https://voe.sx/abc", "https://cdn.example.com/stream.m3u8", &None, &None);
        assert_eq!(result.category, UrlCategory::HlsManifest);
        assert_eq!(result.mime_type, Some("application/vnd.apple.mpegurl".into()));
    }

    #[test]
    fn test_derive_embed_url() {
        let result = derive_embed_url("https://playmogo.com/d/rqhficu74ut4");
        assert_eq!(result, Some("https://playmogo.com/e/rqhficu74ut4".to_string()));

        let result = derive_embed_url("https://dood.to/d/abc123");
        assert_eq!(result, Some("https://dood.to/e/abc123".to_string()));

        // Non-/d/ URL should return None
        let result = derive_embed_url("https://example.com/watch/abc");
        assert!(result.is_none());
    }
}
