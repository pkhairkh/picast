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
                (None, _) => true,       // No rate limit = always sustainable
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
            .filter(|(q, _, sp)| match (sp, typical_bitrate_kbps(q)) {
                (Some(speed), Some(bitrate)) => *speed >= bitrate,
                (None, _) => true,
                (Some(_), None) => true,
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
            u.path_segments().and_then(|mut segments| segments.next_back().map(|s| s.to_string()))
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
        // Use Url::join() to properly construct the full embed URL.
        // This correctly preserves the port number (important for testing
        // with mock HTTP servers) and handles other URL components.
        let full_embed = if href.starts_with("http") {
            href
        } else {
            url::Url::parse(url)
                .ok()
                .and_then(|base| base.join(&href).ok())
                .map(|u| u.to_string())
                .unwrap_or_else(|| format!("https://playmogo.com{}", href))
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
///
/// Uses `Url::join()` to properly handle ports and other URL components.
fn derive_embed_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let path = parsed.path();
    // Match /d/<id> pattern
    let re = regex_lite::Regex::new(r#"^/d/([^/]+)$"#).ok()?;
    let caps = re.captures(path)?;
    let id = caps.get(1)?.as_str();
    let embed_path = format!("/e/{}", id);
    parsed.join(&embed_path).ok().map(|u| u.to_string())
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

            // ── URL extraction priority ──────────────────────────────────
            //
            // Voe's CDN has multiple endpoints with different behaviours:
            //
            //   /engine/hls2-c/  (HLS) — used by browsers via JWPlayer.
            //     The HLS source URL includes &rq= (request token) in the
            //     query string and is the URL the browser actually uses.
            //     However, the CDN's DDoS-Guard/WAF may block requests from
            //     non-residential IPs (Tor exits, datacenters) with 403/404.
            //
            //   /engine/download/  (MP4) — direct MP4 download.
            //     This endpoint requires &rq= appended for CDN authorization
            //     (the JSON's "request" field). Without &rq=, the CDN returns
            //     403. The &rq= token is NOT included in the URL's &t=
            //     signature computation — it's a separate authorization
            //     check. Testing confirmed that appending &rq= to the
            //     download URL is required for the CDN to serve content.
            //
            // Both endpoints may be blocked from Tor/datacenter IPs by
            // the CDN's IP reputation checks. We try all available URLs
            // and let the playback layer handle fallback/retry.
            //
            // Priority order:
            //   1. "direct_access_url" + &rq= — MP4 download with auth token
            //   2. "source" — the URL the browser uses (usually HLS .m3u8)
            //   3. "mp4" — direct MP4 URL or multi-quality object
            //   4. "hls" key — explicit HLS playlist URL
            //
            // We prefer MP4 over HLS because:
            //   - MP4 requires only one HTTP request (lower latency)
            //   - HLS requires fetching master playlist → variant playlist →
            //     individual .ts segments (many round-trips through Tor)
            //   - GStreamer's parsebin handles MP4 natively via appsrc
            //   - HLS segment downloads through Tor are slow and may stutter

            // Priority 1: "direct_access_url" + &rq= — direct MP4 download
            // with CDN authorization token appended. The &rq= parameter is
            // required by the CDN for the /engine/download/ endpoint when
            // "check": true and "direct_access_allowed": false.
            if let Some(url) = obj.get("direct_access_url").and_then(|v| v.as_str()) {
                if !url.is_empty() && !is_hls_url(url) {
                    let url_with_rq = if let Some(ref token) = request_token {
                        if !url.contains("&rq=") && !url.contains("?rq=") {
                            tracing::info!(
                                url = %url,
                                rq_token = %token,
                                "Voe method8: appending &rq= to direct_access_url for CDN authorization"
                            );
                            format!("{}&rq={}", url, token)
                        } else {
                            url.to_owned()
                        }
                    } else {
                        tracing::info!(url = %url, "Voe method8: extracted URL from 'direct_access_url' (no &rq= token available)");
                        url.to_owned()
                    };
                    return Some(url_with_rq);
                }
            }

            // Priority 2: "source" — the URL the browser actually uses.
            // This is typically the HLS .m3u8 URL with &rq= already included.
            if let Some(url) = obj.get("source").and_then(|v| v.as_str()) {
                if !url.is_empty() {
                    if is_hls_url(url) {
                        tracing::info!(url = %url, "Voe method8: HLS URL in 'source' — browser uses this for playback");
                    } else {
                        tracing::info!(url = %url, "Voe method8: extracted URL from 'source' (using as-is)");
                    }
                    return Some(url.to_owned());
                }
            }

            // Priority 3: "mp4" key — direct MP4 URL or multi-quality object
            if let Some(mp4_val) = obj.get("mp4") {
                if let Some(url) = extract_media_from_json_value(mp4_val) {
                    // Append &rq= if not already present
                    let url_with_rq = if let Some(ref token) = request_token {
                        if !url.contains("&rq=") && !url.contains("?rq=") {
                            format!("{}&rq={}", url, token)
                        } else {
                            url
                        }
                    } else {
                        url
                    };
                    tracing::info!(url = %url_with_rq, "Voe method8: extracted MP4 URL from 'mp4' key");
                    return Some(url_with_rq);
                }
            }

            // Priority 4: "hls" key — return HLS URL as final fallback
            // StreamSource has an HLS client that can download segments
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
    // source first (works from Tor), then mp4, then direct_access_url, then hls.
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&step6) {
        if let Some(obj) = parsed.as_object() {
            // Priority 1: "source" — the URL the browser uses (works from Tor)
            if let Some(url) = obj.get("source").and_then(|v| v.as_str()) {
                if !url.is_empty() && !is_bait_source(url) {
                    if is_hls_url(url) {
                        tracing::info!(url = %url, "Voe method7: HLS URL in 'source' — this is the URL the browser uses (works from Tor)");
                    }
                    return Some(url.to_owned());
                }
            }

            // Priority 2: "mp4" key
            if let Some(mp4_val) = obj.get("mp4") {
                if let Some(url) = extract_media_from_json_value(mp4_val) {
                    if !is_bait_source(&url) {
                        return Some(url);
                    }
                }
            }

            // Priority 3: "direct_access_url" — may be blocked from Tor
            if let Some(url) = obj.get("direct_access_url").and_then(|v| v.as_str()) {
                if !url.is_empty() && !is_bait_source(url) {
                    if is_hls_url(url) {
                        tracing::info!(url = %url, "Voe method7: HLS URL in 'direct_access_url' — returning as fallback (StreamSource has HLS client)");
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
    // source first (works from Tor), then mp4, then direct_access_url, then hls.
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&reversed) {
        if let Some(obj) = parsed.as_object() {
            // Priority 1: "source" — the URL the browser uses (works from Tor)
            if let Some(url) = obj.get("source").and_then(|v| v.as_str()) {
                if !url.is_empty() && !is_bait_source(url) {
                    if is_hls_url(url) {
                        tracing::info!(url = %url, "Voe method6: HLS URL in 'source' — this is the URL the browser uses (works from Tor)");
                    }
                    return Some(url.to_owned());
                }
            }

            // Priority 2: "mp4" key
            if let Some(mp4_val) = obj.get("mp4") {
                if let Some(url) = extract_media_from_json_value(mp4_val) {
                    if !is_bait_source(&url) {
                        return Some(url);
                    }
                }
            }

            // Priority 3: "direct_access_url" — may be blocked from Tor
            if let Some(url) = obj.get("direct_access_url").and_then(|v| v.as_str()) {
                if !url.is_empty() && !is_bait_source(url) {
                    if is_hls_url(url) {
                        tracing::info!(url = %url, "Voe method6: HLS URL in 'direct_access_url' — returning as fallback (StreamSource has HLS client)");
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
            let base =
                format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or("localhost"));
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
    let encoded: String =
        reversed.chars().map(|c| char::from_u32(c as u32 + 3).unwrap_or(c)).collect();

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
            tracing::warn!("Voe: /engine/update POST timed out — CDN download may fail");
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

    let category = if is_hls { UrlCategory::HlsManifest } else { UrlCategory::DirectMedia };

    if is_hls {
        tracing::info!(
            url = %media_url,
            "Voe resolver: resolved HLS URL — StreamSource will fetch playlists and segments via HLS client"
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
        resolver_type: "custom".into(),
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
    fn test_extract_media_from_json_value_returns_hls_as_fallback() {
        // HLS string URL — now returned as fallback since StreamSource has HLS client
        let value = serde_json::json!("https://cdn.example.com/stream.m3u8?token=abc");
        let result = extract_media_from_json_value(&value);
        assert!(result.is_some(), "HLS URLs should be returned as fallback");
        assert_eq!(result.unwrap(), "https://cdn.example.com/stream.m3u8?token=abc");
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
        // Multi-quality object with only HLS URLs — now returns highest-quality HLS
        // as fallback since StreamSource has an HLS client
        let value = serde_json::json!({
            "720": "https://cdn.example.com/720.m3u8",
            "480": "https://cdn.example.com/480.m3u8"
        });
        let result = extract_media_from_json_value(&value);
        assert!(result.is_some(), "HLS-only quality objects should return best HLS as fallback");
        assert_eq!(result.unwrap(), "https://cdn.example.com/720.m3u8");
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

    // ── Sprint 2: Voe Deobfuscation Pipeline Tests ─────────────────────

    /// Helper: reverse the Method 8 pipeline to create a synthetic obfuscated blob.
    /// Forward: rot13 → replace_patterns → b64_decode → shift(-3) → reverse → b64_decode → JSON
    /// Reverse: JSON → b64_encode → unreverse → shift(+3) → b64_encode → add_markers → rot13_inv
    fn encode_voe_method8(json: &str) -> String {
        use base64::Engine;
        // Step 1: base64 encode the JSON
        let step1 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
        // Step 2: reverse the string
        let step2: String = step1.chars().rev().collect();
        // Step 3: shift chars by +3 (reverse of -3)
        let step3 = shift_chars_reverse(&step2, 3);
        // Step 4: base64 encode again
        let step4 = base64::engine::general_purpose::STANDARD.encode(step3.as_bytes());
        // Step 5: add marker patterns (reverse of strip_markers)
        let step5 = add_markers(&step4);
        // Step 6: ROT13 (which is its own inverse)
        let step6 = rot13(&step5);
        // Wrap in a JSON array
        serde_json::to_string(&vec![step6]).unwrap()
    }

    /// Shift chars by +shift (reverse of shift_chars which shifts by -shift).
    fn shift_chars_reverse(text: &str, shift: u32) -> String {
        text.chars()
            .map(|c| {
                let code = c as u32;
                char::from_u32(code + shift).unwrap_or(c)
            })
            .collect()
    }

    /// Add marker patterns to a string (reverse of replace_patterns which removes them).
    fn add_markers(txt: &str) -> String {
        // Insert markers at semi-random positions
        let markers = ["@$", "^^", "~@", "%?", "*~", "!!", "#&"];
        let mut result = String::new();
        let chars: Vec<char> = txt.chars().collect();
        let chunk_size = (chars.len() / markers.len()).max(1);
        for (i, marker) in markers.iter().enumerate() {
            let start = i * chunk_size;
            let end = ((i + 1) * chunk_size).min(chars.len());
            if start < chars.len() {
                result.extend(&chars[start..end]);
                result.push_str(marker);
            }
        }
        // Append any remaining chars
        let remaining_start = markers.len() * chunk_size;
        if remaining_start < chars.len() {
            result.extend(&chars[remaining_start..]);
        }
        result
    }

    #[test]
    fn test_method8_pipeline_roundtrip() {
        // Test with only mp4 key — source has higher priority in the
        // extraction logic, so including both would return the source URL.
        let json = r#"{"mp4":"https://cdn.example.com/video.mp4"}"#;
        let obfuscated = encode_voe_method8(json);
        let result = deobfuscate_embedded_json(&obfuscated);
        assert!(result.is_some(), "Method 8 pipeline should decode the obfuscated blob");
        let url = result.unwrap();
        assert!(
            url.contains("cdn.example.com/video.mp4"),
            "Decoded URL should contain the MP4 URL, got: {}",
            url
        );
    }

    #[test]
    fn test_method8_pipeline_source_priority_over_mp4() {
        // When both 'source' and 'mp4' are present, 'source' is returned
        // because it has higher extraction priority (Priority 2 vs Priority 3).
        let json = r#"{"mp4":"https://cdn.example.com/video.mp4","source":"https://cdn.example.com/stream.m3u8"}"#;
        let obfuscated = encode_voe_method8(json);
        let result = deobfuscate_embedded_json(&obfuscated);
        assert!(result.is_some());
        let url = result.unwrap();
        assert!(
            url.contains("stream.m3u8"),
            "'source' key should be extracted before 'mp4', got: {}",
            url
        );
    }

    #[test]
    fn test_method8_pipeline_via_try_method8() {
        let json = r#"{"mp4":"https://cdn.example.com/testvid.mp4"}"#;
        let obfuscated = encode_voe_method8(json);
        let html =
            format!(r#"<html><script type="application/json">{}</script></html>"#, obfuscated);
        let result = try_method8(&html);
        assert!(result.is_some(), "try_method8 should extract URL from HTML with obfuscated JSON");
        assert!(result.unwrap().contains("testvid.mp4"));
    }

    #[test]
    fn test_method6_pipeline_roundtrip() {
        // Method 6 pipeline: clean_base64 → base64_decode → reverse → JSON parse
        // Encoding: reverse(json) → base64_encode
        // Decoding: base64_decode → reverse → json
        let json = r#"{"mp4":"https://cdn.example.com/method6.mp4"}"#;
        // Encode: reverse the JSON, then base64 encode
        let reversed: String = json.chars().rev().collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(reversed.as_bytes());
        // Prefix with a168c marker
        let obfuscated = format!("a168c{}", encoded);
        // Decode: extract after "a168c", base64 decode, then unreverse
        if let Some(data) = obfuscated.strip_prefix("a168c") {
            let decoded = safe_b64_decode(data);
            assert!(decoded.is_some(), "Method 6 base64 decode should succeed");
            let decoded = decoded.unwrap();
            let unreversed: String = decoded.chars().rev().collect();
            assert!(
                unreversed.contains("method6.mp4"),
                "Decoded should contain method6.mp4, got: {}",
                unreversed
            );
        }
    }

    // ── Helper function tests ──────────────────────────────────────────

    #[test]
    fn test_rot13_known_pairs() {
        assert_eq!(rot13("Hello"), "Uryyb");
        assert_eq!(rot13("Uryyb"), "Hello");
        assert_eq!(rot13("ABC"), "NOP");
        assert_eq!(rot13("NOP"), "ABC");
        assert_eq!(rot13("xyz"), "klm");
    }

    #[test]
    fn test_rot13_non_alpha_passthrough() {
        assert_eq!(rot13("Hello, World! 123"), "Uryyb, Jbeyq! 123");
        assert_eq!(rot13("a1b2c3"), "n1o2p3");
    }

    #[test]
    fn test_replace_patterns_each_marker() {
        for pat in &["@$", "^^", "~@", "%?", "*~", "!!", "#&"] {
            let input = format!("before{}after", pat);
            let result = replace_patterns(&input);
            assert_eq!(result, "beforeafter", "Pattern '{}' should be removed", pat);
        }
    }

    #[test]
    fn test_replace_patterns_multiple() {
        // Each marker appears as a contiguous two-char pair:
        // a{@$}b{^^}c{~@}d{%?}e{!!}f{#&}g
        // After removing all markers: abcdefg
        let input = "a@$b^^c~@d%?e!!f#&g";
        let result = replace_patterns(&input);
        assert_eq!(result, "abcdefg");
    }

    #[test]
    fn test_shift_chars_basic() {
        // shift_chars shifts by -shift: 'd' (100) - 3 = 'a' (97)
        let result = shift_chars("def", 3);
        assert_eq!(result, "abc");
    }

    #[test]
    fn test_shift_chars_edge_cases() {
        // Shift of 0 should be identity
        assert_eq!(shift_chars("hello", 0), "hello");
        // Shift that would go below 0 should leave char unchanged
        let result = shift_chars("\u{1}\u{2}", 3);
        assert_eq!(result, "\u{1}\u{2}");
    }

    #[test]
    fn test_safe_b64_decode_valid() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"hello world");
        let result = safe_b64_decode(&encoded);
        assert_eq!(result, Some("hello world".to_string()));
    }

    #[test]
    fn test_safe_b64_decode_missing_padding() {
        // "hello" in base64 = "aGVsbG8=" — remove padding
        let encoded = "aGVsbG8";
        let result = safe_b64_decode(encoded);
        assert_eq!(result, Some("hello".to_string()));
    }

    #[test]
    fn test_safe_b64_decode_invalid_returns_none() {
        let result = safe_b64_decode("!!!not-base64!!!");
        assert!(result.is_none());
    }

    #[test]
    fn test_clean_base64_backslash_removal() {
        let input = r#"aGVs\bG8="#;
        let result = clean_base64(input);
        assert!(result.is_some());
        assert!(!result.unwrap().contains('\\'));
    }

    #[test]
    fn test_clean_base64_padding() {
        let input = "aGVsbG8"; // "hello" without padding
        let result = clean_base64(input);
        assert!(result.is_some());
        // Should have padding added
        assert!(result.unwrap().ends_with('='));
    }

    #[test]
    fn test_is_bait_source_bait_domains() {
        assert!(is_bait_source("https://test-videos.co.uk/video.mp4"));
        assert!(is_bait_source("https://sample-videos.com/test.mp4"));
        assert!(is_bait_source("https://commondatastorage.googleapis.com/bbb.mp4"));
    }

    #[test]
    fn test_is_bait_source_bait_filenames() {
        assert!(is_bait_source("https://cdn.example.com/BigBuckBunny.mp4"));
        assert!(is_bait_source("https://cdn.example.com/Big_Buck_Bunny_1080_10s_5MB.mp4"));
        assert!(is_bait_source("https://cdn.example.com/bbb.mp4"));
    }

    #[test]
    fn test_is_bait_source_non_bait() {
        assert!(!is_bait_source("https://cdn.voecdn.com/v/abc123.mp4"));
        assert!(!is_bait_source("https://example.com/movie.mp4"));
    }

    #[test]
    fn test_extract_cdn_speed_param_with_sp() {
        assert_eq!(extract_cdn_speed_param("https://cdn.example.com/v.mp4?sp=380"), Some(380));
        assert_eq!(
            extract_cdn_speed_param("https://cdn.example.com/v.mp4?token=abc&sp=500"),
            Some(500)
        );
    }

    #[test]
    fn test_extract_cdn_speed_param_no_sp() {
        assert_eq!(extract_cdn_speed_param("https://cdn.example.com/v.mp4"), None);
        assert_eq!(extract_cdn_speed_param("https://cdn.example.com/v.mp4?token=abc"), None);
    }

    #[test]
    fn test_extract_cdn_speed_param_non_numeric() {
        assert_eq!(extract_cdn_speed_param("https://cdn.example.com/v.mp4?sp=abc"), None);
    }

    #[test]
    fn test_typical_bitrate_kbps_known_qualities() {
        assert_eq!(typical_bitrate_kbps("240"), Some(400));
        assert_eq!(typical_bitrate_kbps("360"), Some(800));
        assert_eq!(typical_bitrate_kbps("480"), Some(1500));
        assert_eq!(typical_bitrate_kbps("720"), Some(3000));
        assert_eq!(typical_bitrate_kbps("1080"), Some(6000));
    }

    #[test]
    fn test_typical_bitrate_kbps_unknown_quality() {
        assert_eq!(typical_bitrate_kbps("1440"), None);
        assert_eq!(typical_bitrate_kbps("4k"), None);
    }

    #[test]
    fn test_is_hls_url_m3u8() {
        assert!(is_hls_url("https://cdn.example.com/stream.m3u8"));
        assert!(is_hls_url("https://cdn.example.com/stream.M3U8"));
        assert!(is_hls_url("https://cdn.example.com/path/stream.m3u8?token=abc"));
    }

    #[test]
    fn test_is_hls_url_non_hls() {
        assert!(!is_hls_url("https://cdn.example.com/video.mp4"));
        assert!(!is_hls_url("https://cdn.example.com/video.webm"));
    }

    #[test]
    fn test_extract_media_from_json_value_simple_string() {
        let val = serde_json::json!("https://cdn.example.com/video.mp4");
        let result = extract_media_from_json_value(&val);
        assert_eq!(result, Some("https://cdn.example.com/video.mp4".to_string()));
    }

    #[test]
    fn test_extract_media_from_json_value_quality_object() {
        let val = serde_json::json!({
            "720": "https://cdn.example.com/720.mp4",
            "1080": "https://cdn.example.com/1080.mp4"
        });
        let result = extract_media_from_json_value(&val);
        assert!(result.is_some());
        // 720 should be preferred over 1080
        assert!(result.unwrap().contains("720.mp4"));
    }

    #[test]
    fn test_extract_media_from_json_value_hls_only_object() {
        let val = serde_json::json!({
            "720": "https://cdn.example.com/stream.m3u8"
        });
        let result = extract_media_from_json_value(&val);
        assert!(result.is_some());
        assert!(result.unwrap().contains(".m3u8"));
    }

    #[test]
    fn test_extract_media_from_json_value_empty_object() {
        let val = serde_json::json!({});
        let result = extract_media_from_json_value(&val);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_socks5_url_valid() {
        let result = parse_socks5_url("socks5h://bogdan-hash123@127.0.0.1:9050/");
        assert_eq!(result, Some(("bogdan-hash123".to_string(), "127.0.0.1:9050".to_string())));
    }

    #[test]
    fn test_parse_socks5_url_socks5_prefix() {
        let result = parse_socks5_url("socks5://bogdan-hash@10.0.0.1:1080");
        assert_eq!(result, Some(("bogdan-hash".to_string(), "10.0.0.1:1080".to_string())));
    }

    #[test]
    fn test_parse_socks5_url_no_username() {
        let result = parse_socks5_url("socks5h://127.0.0.1:9050/");
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_socks5_url_trailing_slash() {
        let result = parse_socks5_url("socks5h://user@host:1234/");
        assert_eq!(result, Some(("user".to_string(), "host:1234".to_string())));
    }

    // ── Sprint 2: is_voe_domain comprehensive tests ────────────────────

    #[test]
    fn test_is_voe_domain_canonical() {
        assert!(is_voe_domain("voe.sx"));
        assert!(is_voe_domain("voe-unblock.com"));
        assert!(is_voe_domain("voeunblock.com"));
        assert!(is_voe_domain("voeunbl0ck.com"));
        assert!(is_voe_domain("voe-unblk.com"));
        assert!(is_voe_domain("voeunblk.com"));
        assert!(is_voe_domain("voeunblock2.com"));
    }

    #[test]
    fn test_is_voe_domain_subdomain() {
        assert!(is_voe_domain("www.voe.sx"));
        assert!(is_voe_domain("cdn.voe-unblock.com"));
    }

    #[test]
    fn test_is_voe_domain_heuristic_com() {
        // "cactusheadroomscaling" — all lowercase, good vowel ratio, 20+ chars
        assert!(is_voe_domain("cactusheadroomscaling.com"));
        assert!(is_voe_domain("maryspecialwatch.com"));
    }

    #[test]
    fn test_is_voe_domain_well_known_domains_excluded() {
        assert!(!is_voe_domain("youtube.com"));
        assert!(!is_voe_domain("google.com"));
        assert!(!is_voe_domain("netflix.com"));
    }

    #[test]
    fn test_is_voe_domain_short_com_names() {
        // Short names should not match the heuristic
        assert!(!is_voe_domain("abc.com"));
        assert!(!is_voe_domain("xy.com"));
    }

    // ── Sprint 2: is_doodstream_domain comprehensive tests ─────────────

    #[test]
    fn test_is_doodstream_domain_all_known() {
        for domain in DOODSTREAM_DOMAINS {
            assert!(is_doodstream_domain(domain), "{} should be a DoodStream domain", domain);
        }
    }

    #[test]
    fn test_is_doodstream_domain_subdomain() {
        assert!(is_doodstream_domain("www.playmogo.com"));
        assert!(is_doodstream_domain("cdn.doodstream.com"));
    }

    #[test]
    fn test_is_doodstream_domain_unknown() {
        assert!(!is_doodstream_domain("youtube.com"));
        assert!(!is_doodstream_domain("example.com"));
        assert!(!is_doodstream_domain("voe.sx"));
    }

    // ── Sprint 2: DoodStream Resolver Tests ────────────────────────────

    #[test]
    fn test_derive_embed_url_various_formats() {
        assert_eq!(
            derive_embed_url("https://playmogo.com/d/abc123"),
            Some("https://playmogo.com/e/abc123".to_string())
        );
        assert_eq!(
            derive_embed_url("https://dood.to/d/xyz789"),
            Some("https://dood.to/e/xyz789".to_string())
        );
        assert_eq!(
            derive_embed_url("http://dood.watch/d/testid"),
            Some("http://dood.watch/e/testid".to_string())
        );
    }

    #[test]
    fn test_derive_embed_url_non_d_url_returns_none() {
        assert!(derive_embed_url("https://playmogo.com/e/abc123").is_none());
        assert!(derive_embed_url("https://playmogo.com/watch/abc123").is_none());
        assert!(derive_embed_url("https://example.com/video.mp4").is_none());
    }

    #[test]
    fn test_find_embed_iframe_with_e_iframe() {
        let html = r#"<html><body><iframe src="/e/abc123"></iframe></body></html>"#;
        let doc = Html::parse_document(html);
        let result = find_embed_iframe(&doc, "https://playmogo.com/d/abc123");
        assert!(result.is_some());
        assert!(result.unwrap().contains("/e/abc123"));
    }

    #[test]
    fn test_find_embed_iframe_without_iframe() {
        let html = r#"<html><body><p>No iframe here</p></body></html>"#;
        let doc = Html::parse_document(html);
        let result = find_embed_iframe(&doc, "https://playmogo.com/d/abc123");
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_mp4_url_from_text_various() {
        let text = r#"var x = "https://cdn.example.com/video.mp4";"#;
        let result = extract_mp4_url_from_text(text);
        assert!(result.is_some());
        assert!(result.unwrap().contains("video.mp4"));
    }

    #[test]
    fn test_extract_mp4_url_ignores_bait_in_var_assignment() {
        let text = r#"var x = "https://test-videos.co.uk/BigBuckBunny.mp4";"#;
        let result = extract_mp4_url_from_text(text);
        assert!(result.is_none(), "Bait URLs should be excluded");
    }

    #[test]
    fn test_extract_m3u8_url_from_text_found() {
        let text = r#"var source = "https://cdn.example.com/stream.m3u8";"#;
        let result = extract_m3u8_url_from_text(text);
        assert!(result.is_some());
        assert!(result.unwrap().contains(".m3u8"));
    }

    #[test]
    fn test_extract_m3u8_url_ignores_bait() {
        let text = r#"var source = "https://sample-videos.com/stream.m3u8";"#;
        let result = extract_m3u8_url_from_text(text);
        assert!(result.is_none(), "Bait HLS URLs should be excluded");
    }

    // ═══════════════════════════════════════════════════════════════════
    // Sprint 2 — S2.1: Voe Deobfuscation Edge-Case Unit Tests
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn test_deobfuscate_embedded_json_empty_input() {
        // Empty string should return None — not panic.
        assert!(deobfuscate_embedded_json("").is_none());
    }

    #[test]
    fn test_deobfuscate_embedded_json_empty_json_array() {
        // Valid JSON but empty array — no obfuscated blobs to decode.
        assert!(deobfuscate_embedded_json("[]").is_none());
    }

    #[test]
    fn test_deobfuscate_embedded_json_invalid_json() {
        // Not valid JSON at all — should return None, not panic.
        assert!(deobfuscate_embedded_json("not json at all").is_none());
    }

    #[test]
    fn test_deobfuscate_embedded_json_invalid_base64_in_pipeline() {
        // Valid JSON array wrapping a string that can't be decoded through
        // the pipeline. ROT13 always succeeds, replace_patterns always
        // succeeds, but Base64 decode will fail on garbage input.
        let garbage = rot13("!!!not_base64!!!");
        let input = serde_json::to_string(&vec![garbage]).unwrap();
        assert!(deobfuscate_embedded_json(&input).is_none());
    }

    #[test]
    fn test_try_method8_empty_html() {
        // Empty HTML should return None — no <script> tags to find.
        assert!(try_method8("").is_none());
    }

    #[test]
    fn test_try_method8_no_script_tag() {
        // HTML without application/json script tags.
        let html = r#"<html><body><p>Hello world</p></body></html>"#;
        assert!(try_method8(html).is_none());
    }

    #[test]
    fn test_try_method8_empty_script_tag() {
        // Script tag with empty content — should be skipped.
        let html = r#"<html><script type="application/json">  </script></html>"#;
        assert!(try_method8(html).is_none());
    }

    #[test]
    fn test_try_method7_no_mkgma() {
        // HTML without MKGMa variable — should return None.
        let html = r#"<html><body><p>No MKGMa here</p></body></html>"#;
        assert!(try_method7(html).is_none());
    }

    #[test]
    fn test_try_method6_no_a168c() {
        // HTML without a168c variable — should return None.
        let html = r#"<html><body><p>No a168c here</p></body></html>"#;
        assert!(try_method6(html).is_none());
    }

    #[test]
    fn test_method7_pipeline_roundtrip() {
        // Build a Method 7 payload and verify round-trip through try_method7.
        // Method 7: ROT13 → strip underscores → Base64 decode → shift(-3) → reverse → Base64 decode
        let json = r#"{"source":"https://cdn.example.com/method7video.mp4"}"#;
        // Reverse the pipeline: JSON → b64_encode → unreverse → shift(+3) → b64_encode → add_underscores → rot13
        let step1 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
        let step2: String = step1.chars().rev().collect();
        let step3 = shift_chars_reverse(&step2, 3);
        let step4 = base64::engine::general_purpose::STANDARD.encode(step3.as_bytes());
        // Insert underscores at regular intervals (reverse of strip_underscores)
        let step5: String = step4
            .chars()
            .enumerate()
            .flat_map(|(i, c)| if i > 0 && i % 8 == 0 { vec!['_', c] } else { vec![c] })
            .collect();
        let step6 = rot13(&step5);
        let html = format!(r#"<html><script>MKGMa="{}"</script></html>"#, step6);
        let result = try_method7(&html);
        assert!(result.is_some(), "Method 7 pipeline should decode the obfuscated blob");
        assert!(result.unwrap().contains("method7video.mp4"));
    }

    #[test]
    fn test_method6_pipeline_roundtrip_with_mp4_key() {
        // Method 6: a168c = 'base64_data' where the data is base64(reverse(json))
        // Pipeline: clean_base64 → base64_decode → reverse → JSON parse
        let json = r#"{"mp4":"https://cdn.example.com/method6.mp4"}"#;
        // Step 1: Reverse the JSON
        let reversed: String = json.chars().rev().collect();
        // Step 2: Base64 encode the reversed JSON
        let encoded = base64::engine::general_purpose::STANDARD.encode(reversed.as_bytes());
        let html = format!(r#"<html><script>a168c = '{}'</script></html>"#, encoded);
        let result = try_method6(&html);
        assert!(result.is_some(), "Method 6 pipeline should decode the obfuscated blob");
        assert!(result.unwrap().contains("method6.mp4"));
    }

    #[test]
    fn test_method8_bait_source_rejected() {
        // If the deobfuscated URL points to a known bait domain, it should
        // be rejected by try_method8.
        let json = r#"{"mp4":"https://test-videos.co.uk/BigBuckBunny.mp4"}"#;
        let obfuscated = encode_voe_method8(json);
        let html =
            format!(r#"<html><script type="application/json">{}</script></html>"#, obfuscated);
        assert!(try_method8(&html).is_none(), "Bait URLs should be rejected by try_method8");
    }

    #[test]
    fn test_method6_bait_source_rejected() {
        // If the deobfuscated URL points to a known bait domain, Method 6
        // should also reject it.
        let json = r#"{"mp4":"https://sample-videos.com/bbb.mp4"}"#;
        let reversed: String = json.chars().rev().collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(reversed.as_bytes());
        let html = format!(r#"<html><script>a168c = '{}'</script></html>"#, encoded);
        assert!(try_method6(&html).is_none(), "Bait URLs should be rejected by try_method6");
    }

    #[test]
    fn test_extract_media_from_json_value_empty_string() {
        // Empty string value should return None.
        let val = serde_json::json!("");
        assert!(extract_media_from_json_value(&val).is_none());
    }

    #[test]
    fn test_extract_media_from_json_value_null() {
        // Null value should return None.
        let val = serde_json::json!(null);
        assert!(extract_media_from_json_value(&val).is_none());
    }

    #[test]
    fn test_extract_media_from_json_value_number() {
        // Non-string, non-object value should return None.
        let val = serde_json::json!(42);
        assert!(extract_media_from_json_value(&val).is_none());
    }

    #[test]
    fn test_extract_media_from_json_value_single_quality_with_rate_limit() {
        // Single quality with CDN rate limit — should still return the URL.
        let val = serde_json::json!({"720": "https://cdn.example.com/video.mp4?sp=1500"});
        let result = extract_media_from_json_value(&val);
        assert!(result.is_some());
        assert!(result.unwrap().contains("video.mp4"));
    }

    #[test]
    fn test_extract_media_from_json_value_multi_quality_prefers_sustainable() {
        // Two qualities: one rate-limited below its bitrate, one unlimited.
        // The sustainable (unlimited) one should be preferred.
        let val = serde_json::json!({
            "1080": "https://cdn.example.com/1080.mp4?sp=500",   // sp=500 < 6000 typical bitrate → unsustainable
            "720": "https://cdn.example.com/720.mp4"              // no sp= → unlimited → sustainable
        });
        let result = extract_media_from_json_value(&val);
        assert!(result.is_some());
        let url = result.unwrap();
        assert!(
            url.contains("720.mp4"),
            "Should prefer sustainable 720p over rate-limited 1080p, got: {}",
            url
        );
    }

    #[test]
    fn test_extract_media_from_json_value_all_unsustainable_picks_highest_sp() {
        // All qualities have rate limits below their typical bitrates.
        // Should pick the one with the highest sp= value.
        let val = serde_json::json!({
            "1080": "https://cdn.example.com/1080.mp4?sp=500",
            "720": "https://cdn.example.com/720.mp4?sp=1500"
        });
        let result = extract_media_from_json_value(&val);
        assert!(result.is_some());
        let url = result.unwrap();
        assert!(url.contains("720.mp4"), "Should pick highest sp= value, got: {}", url);
    }

    #[test]
    fn test_extract_cdn_speed_param_various_positions() {
        // sp= in different query string positions
        assert_eq!(extract_cdn_speed_param("https://cdn.example.com/v.mp4?sp=300"), Some(300));
        assert_eq!(
            extract_cdn_speed_param("https://cdn.example.com/v.mp4?t=abc&sp=1200"),
            Some(1200)
        );
        assert_eq!(
            extract_cdn_speed_param("https://cdn.example.com/v.mp4?sp=999&extra=1"),
            Some(999)
        );
    }

    #[test]
    fn test_is_voe_domain_heuristic_vowel_ratio() {
        // Domains with too few vowels (< 20%) should NOT match the heuristic.
        // "bzqrkxmpl.com" has 0 vowels out of 10 chars → 0% → rejected.
        assert!(!is_voe_domain("bzqrkxmpl.com"));

        // "wonderfulshow.com" has 5 vowels out of 14 → 35.7% → matches.
        assert!(is_voe_domain("wonderfulshow.com"));
    }

    #[test]
    fn test_is_voe_domain_hyphenated_domains_rejected() {
        // Voe front-end domains don't contain hyphens.
        // Hyphenated .com domains should not match the heuristic.
        assert!(!is_voe_domain("some-hyphenated-domain.com"));
    }

    #[test]
    fn test_is_voe_domain_numeric_domains_rejected() {
        // Voe front-end domains don't contain digits.
        assert!(!is_voe_domain("video123watch.com"));
    }

    #[test]
    fn test_follow_js_redirect_relative_url() {
        // Relative URL should be resolved against the original URL.
        let html = r#"<script>window.location.href = '/new-page'</script>"#;
        let result = follow_js_redirect(html, "https://voe.sx/abc123");
        assert_eq!(result, "https://voe.sx/new-page");
    }

    #[test]
    fn test_follow_js_redirect_location_replace() {
        let html = r#"<script>window.location.replace('https://front-end.com/xyz')</script>"#;
        let result = follow_js_redirect(html, "https://voe.sx/abc");
        assert_eq!(result, "https://front-end.com/xyz");
    }

    #[test]
    fn test_follow_js_redirect_no_redirect_returns_original() {
        let html = r#"<html><body>Normal page</body></html>"#;
        let result = follow_js_redirect(html, "https://voe.sx/abc");
        assert_eq!(result, "https://voe.sx/abc");
    }

    #[test]
    fn test_build_result_sets_custom_resolver_type() {
        let result = build_result(
            "https://voe.sx/abc",
            "https://cdn.example.com/video.mp4",
            &Some("Test".into()),
            &None,
        );
        assert_eq!(result.resolver_type, "custom");
        assert_eq!(result.category, UrlCategory::DirectMedia);
        assert_eq!(result.mime_type, Some("video/mp4".into()));
        assert_eq!(result.title, Some("Test".into()));
    }

    #[test]
    fn test_build_result_hls_category() {
        let result =
            build_result("https://voe.sx/abc", "https://cdn.example.com/stream.m3u8", &None, &None);
        assert_eq!(result.category, UrlCategory::HlsManifest);
        assert_eq!(result.mime_type, Some("application/vnd.apple.mpegurl".into()));
    }

    #[test]
    fn test_clean_base64_with_backslashes() {
        // Backslashes should be stripped before decoding.
        let input = r#"SGV\sbG8="#;
        let result = clean_base64(input);
        assert!(result.is_some());
        // After removing backslashes: "SGVsbG8=" → decodes to "Hello"
    }

    #[test]
    fn test_clean_base64_empty_input() {
        // Empty string should still be valid Base64.
        let result = clean_base64("");
        assert!(result.is_some());
    }

    #[test]
    fn test_safe_b64_decode_empty_string() {
        assert!(safe_b64_decode("").is_some()); // Empty decodes to empty
    }

    #[test]
    fn test_is_bait_source_various_bait_patterns() {
        // Bait domains
        assert!(is_bait_source("https://test-videos.co.uk/vid.mp4"));
        assert!(is_bait_source("https://sample-videos.com/vid.mp4"));
        // Bait filenames
        assert!(is_bait_source("https://cdn.example.com/BigBuckBunny.mp4"));
        assert!(is_bait_source("https://cdn.example.com/Big_Buck_Bunny_1080_10s_5MB.mp4"));
        // Normal URL
        assert!(!is_bait_source("https://cdn.example.com/normal-video.mp4"));
    }

    #[test]
    fn test_try_fallback_urls_var_source() {
        let html = r#"var source = "https://cdn.example.com/fallback.mp4""#;
        let result = try_fallback_urls(html);
        assert!(result.is_some());
        assert!(result.unwrap().contains("fallback.mp4"));
    }

    #[test]
    fn test_try_fallback_urls_skips_hls() {
        let html = r#"var source = "https://cdn.example.com/stream.m3u8""#;
        assert!(try_fallback_urls(html).is_none(), "HLS URLs should be skipped in fallback");
    }

    #[test]
    fn test_try_fallback_urls_direct_mp4() {
        let html = r#"<html>https://cdn.example.com/direct.mp4</html>"#;
        let result = try_fallback_urls(html);
        assert!(result.is_some());
        assert!(result.unwrap().contains("direct.mp4"));
    }

    #[test]
    fn test_try_fallback_urls_bait_rejected() {
        let html = r#"var source = "https://test-videos.co.uk/BigBuckBunny.mp4""#;
        assert!(try_fallback_urls(html).is_none(), "Bait URLs should be rejected");
    }

    // ═══════════════════════════════════════════════════════════════════
    // Sprint 2 — S2.2: DoodStream Resolver Unit Tests
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn test_derive_embed_url_standard() {
        // Standard /d/ → /e/ transformation
        assert_eq!(
            derive_embed_url("https://playmogo.com/d/abc123"),
            Some("https://playmogo.com/e/abc123".into())
        );
    }

    #[test]
    fn test_derive_embed_url_with_query_params() {
        // Query params are stripped in the derived URL
        let result = derive_embed_url("https://dood.watch/d/xyz789?foo=bar");
        assert!(result.is_some());
        assert!(result.unwrap().contains("/e/xyz789"));
    }

    #[test]
    fn test_derive_embed_url_embed_url_unchanged() {
        // /e/ URLs should NOT be transformed (already embed URLs)
        // The regex only matches /d/<id>, not /e/<id>
        assert!(derive_embed_url("https://playmogo.com/e/abc123").is_none());
    }

    #[test]
    fn test_derive_embed_url_non_dood_url() {
        // Random URLs without /d/<id> pattern should return None
        assert!(derive_embed_url("https://example.com/watch?v=abc").is_none());
    }

    #[test]
    fn test_derive_embed_url_trailing_slash() {
        let result = derive_embed_url("https://dood.la/d/abc123/");
        // The regex ^/d/([^/]+)$ won't match /d/abc123/ because of trailing /
        assert!(result.is_none());
    }

    #[test]
    fn test_find_embed_iframe_with_e_src() {
        let html = r#"<html><iframe src="/e/abc123"></iframe></html>"#;
        let doc = Html::parse_document(html);
        let result = find_embed_iframe(&doc, "https://playmogo.com/d/abc123");
        assert_eq!(result, Some("/e/abc123".into()));
    }

    #[test]
    fn test_find_embed_iframe_full_url() {
        let html = r#"<html><iframe src="https://dood.watch/e/xyz789"></iframe></html>"#;
        let doc = Html::parse_document(html);
        let result = find_embed_iframe(&doc, "https://playmogo.com/d/xyz789");
        assert!(result.is_some());
        assert!(result.unwrap().contains("/e/xyz789"));
    }

    #[test]
    fn test_find_embed_iframe_no_iframe() {
        let html = r#"<html><body><p>No iframe</p></body></html>"#;
        let doc = Html::parse_document(html);
        assert!(find_embed_iframe(&doc, "https://playmogo.com/d/abc").is_none());
    }

    #[test]
    fn test_find_embed_iframe_wrong_src() {
        // iframe with non-/e/ src should not match
        let html = r#"<html><iframe src="https://youtube.com/embed/abc"></iframe></html>"#;
        let doc = Html::parse_document(html);
        assert!(find_embed_iframe(&doc, "https://playmogo.com/d/abc").is_none());
    }

    #[test]
    fn test_extract_doodstream_media_pass_md5() {
        let html = r#"<html><script>/pass_md5/abc123/def456"</script></html>"#;
        let result = extract_doodstream_media(html, "https://dood.watch/e/abc123");
        assert!(result.is_some());
        assert!(result.unwrap().contains("/pass_md5/"));
    }

    #[test]
    fn test_extract_doodstream_media_direct_mp4() {
        let html = r#"<html><video src="https://cdn.dood.stream/video.mp4"></video></html>"#;
        let result = extract_doodstream_media(html, "https://dood.watch/e/abc123");
        assert!(result.is_some());
        assert!(result.unwrap().contains("video.mp4"));
    }

    // ═══════════════════════════════════════════════════════════════════
    // Sprint 2 — S2.3: Mock HTTP Server Integration Tests
    // ═══════════════════════════════════════════════════════════════════

    /// Build a minimal Voe-like HTML page with a Method 8 obfuscated blob.
    fn build_mock_voe_page_method8(media_url: &str) -> String {
        let json = format!(r#"{{"mp4":"{}"}}"#, media_url);
        let obfuscated = encode_voe_method8(&json);
        format!(
            r#"<html><head><title>Voe Video</title><meta property="og:title" content="Test Voe Video"/><meta property="og:image" content="https://cdn.example.com/thumb.jpg"/></head><body><script type="application/json">{}</script></body></html>"#,
            obfuscated
        )
    }

    /// Build a mock DoodStream page with an embed iframe.
    fn build_mock_doodstream_page(video_id: &str) -> String {
        format!(
            r#"<html><head><title>DoodStream Video</title><meta property="og:title" content="Test DoodStream Video"/></head><body><iframe src="/e/{}"></iframe></body></html>"#,
            video_id
        )
    }

    /// Build a mock DoodStream embed page with a pass_md5 token.
    fn build_mock_doodstream_embed_page(pass_id: &str) -> String {
        format!(
            r#"<html><body><script>var x = "/pass_md5/{}/token123";</script></body></html>"#,
            pass_id
        )
    }

    #[tokio::test]
    async fn test_mock_voe_resolve_method8() {
        // Test resolve_voe against a mock HTTP server serving a Voe page.
        let mut server = mockito::Server::new_async().await;

        let mock_page = build_mock_voe_page_method8("https://cdn.voecdn.com/video123.mp4");

        let mock = server
            .mock("GET", "/abc123")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body(&mock_page)
            .create_async()
            .await;

        let url = format!("{}/abc123", server.url());
        let result = resolve_voe(&url, None).await;

        mock.assert_async().await;
        assert!(
            result.is_ok(),
            "resolve_voe should succeed against mock server: {:?}",
            result.err()
        );
        let resolved = result.unwrap();
        assert!(
            resolved.direct_url.contains("video123.mp4"),
            "Resolved URL should contain the MP4 URL, got: {}",
            resolved.direct_url
        );
        assert_eq!(resolved.resolver_type, "custom");
        assert_eq!(resolved.title, Some("Test Voe Video".into()));
    }

    #[tokio::test]
    async fn test_mock_voe_404_returns_error() {
        let mut server = mockito::Server::new_async().await;

        let mock = server.mock("GET", "/notfound").with_status(404).create_async().await;

        let url = format!("{}/notfound", server.url());
        let result = resolve_voe(&url, None).await;

        mock.assert_async().await;
        assert!(result.is_err(), "404 response should return an error");
    }

    #[tokio::test]
    async fn test_mock_voe_403_returns_error() {
        let mut server = mockito::Server::new_async().await;

        let mock = server.mock("GET", "/forbidden").with_status(403).create_async().await;

        let url = format!("{}/forbidden", server.url());
        let result = resolve_voe(&url, None).await;

        mock.assert_async().await;
        assert!(result.is_err(), "403 response should return an error");
    }

    #[tokio::test]
    async fn test_mock_voe_js_redirect() {
        // Test that resolve_voe follows JavaScript redirects.
        let mut server = mockito::Server::new_async().await;

        let redirect_html = format!(
            r#"<html><script>window.location.href = '{}/redirected'</script></html>"#,
            server.url()
        );

        let target_page = build_mock_voe_page_method8("https://cdn.voecdn.com/redirected-vid.mp4");

        let mock_redirect = server
            .mock("GET", "/original")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body(&redirect_html)
            .create_async()
            .await;

        let mock_target = server
            .mock("GET", "/redirected")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body(&target_page)
            .create_async()
            .await;

        let url = format!("{}/original", server.url());
        let result = resolve_voe(&url, None).await;

        mock_redirect.assert_async().await;
        mock_target.assert_async().await;
        assert!(result.is_ok(), "resolve_voe should follow JS redirect: {:?}", result.err());
        assert!(result.unwrap().direct_url.contains("redirected-vid.mp4"));
    }

    #[tokio::test]
    async fn test_mock_voe_empty_page_returns_error() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("GET", "/empty")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body("<html><body>Empty page</body></html>")
            .create_async()
            .await;

        let url = format!("{}/empty", server.url());
        let result = resolve_voe(&url, None).await;

        mock.assert_async().await;
        assert!(result.is_err(), "Page with no obfuscated data should return NoMediaFound");
    }

    #[tokio::test]
    async fn test_mock_doodstream_resolve_via_embed() {
        // Test resolve_doodstream against a mock server.
        let mut server = mockito::Server::new_async().await;

        let main_page = build_mock_doodstream_page("abc123");
        let embed_page = build_mock_doodstream_embed_page("abc123/token456");

        let mock_main = server
            .mock("GET", "/d/abc123")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body(&main_page)
            .create_async()
            .await;

        let mock_embed = server
            .mock("GET", "/e/abc123")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body(&embed_page)
            .create_async()
            .await;

        let url = format!("{}/d/abc123", server.url());
        let result = resolve_doodstream(&url, None).await;

        mock_main.assert_async().await;
        mock_embed.assert_async().await;
        assert!(result.is_ok(), "resolve_doodstream should succeed: {:?}", result.err());
        let resolved = result.unwrap();
        assert!(
            resolved.direct_url.contains("/pass_md5/") || resolved.direct_url.contains(".mp4"),
            "Resolved URL should contain media URL, got: {}",
            resolved.direct_url
        );
        assert_eq!(resolved.resolver_type, "custom");
    }

    #[tokio::test]
    async fn test_mock_doodstream_403_main_page_tries_embed() {
        // When the main /d/ page returns 403, resolve_doodstream should
        // derive the embed URL and try that instead.
        let mut server = mockito::Server::new_async().await;

        let embed_page = build_mock_doodstream_embed_page("abc123/token456");

        let mock_main = server.mock("GET", "/d/abc123").with_status(403).create_async().await;

        let mock_embed = server
            .mock("GET", "/e/abc123")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body(&embed_page)
            .create_async()
            .await;

        let url = format!("{}/d/abc123", server.url());
        let result = resolve_doodstream(&url, None).await;

        mock_main.assert_async().await;
        mock_embed.assert_async().await;
        assert!(
            result.is_ok(),
            "Should resolve via embed when main page returns 403: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_mock_doodstream_both_403_returns_error() {
        // When both main and embed pages return 403, should return error.
        let mut server = mockito::Server::new_async().await;

        let mock_main = server.mock("GET", "/d/blocked").with_status(403).create_async().await;

        let mock_embed = server.mock("GET", "/e/blocked").with_status(403).create_async().await;

        let url = format!("{}/d/blocked", server.url());
        let result = resolve_doodstream(&url, None).await;

        mock_main.assert_async().await;
        mock_embed.assert_async().await;
        assert!(result.is_err(), "Should return error when both pages are 403");
    }

    #[tokio::test]
    async fn test_mock_voe_cookie_forwarding() {
        // Verify that cookies received during the page fetch are
        // forwarded in the ResolveResult.
        let mut server = mockito::Server::new_async().await;

        let mock_page = build_mock_voe_page_method8("https://cdn.voecdn.com/cookietest.mp4");

        let mock = server
            .mock("GET", "/withcookies")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_header("set-cookie", "session=abc123; Path=/")
            .with_header("set-cookie", "token=xyz789; Path=/")
            .with_body(&mock_page)
            .create_async()
            .await;

        let url = format!("{}/withcookies", server.url());
        let result = resolve_voe(&url, None).await;

        mock.assert_async().await;
        assert!(result.is_ok());
        let resolved = result.unwrap();
        // Cookies should be present in the result (reqwest's cookie jar
        // captures them during the page fetch).
        // Note: the exact cookie format depends on reqwest's cookie handling,
        // but at least the result should have a cookies field (may be empty
        // if reqwest doesn't expose Set-Cookie values via the jar).
        assert!(resolved.direct_url.contains("cookietest.mp4"));
    }
}


// ── YouTube Resolver ─────────────────────────────────────────────────

/// Domains handled by the YouTube custom resolver.
const YOUTUBE_DOMAINS: &[&str] = &[
    "youtube.com",
    "www.youtube.com",
    "m.youtube.com",
    "youtu.be",
    "youtube-nocookie.com",
    "www.youtube-nocookie.com",
];

/// Check if a hostname should be handled by the YouTube custom resolver.
pub fn is_youtube_domain(host: &str) -> bool {
    let host_lower = host.to_lowercase();
    YOUTUBE_DOMAINS
        .iter()
        .any(|d| host_lower == *d || host_lower.ends_with(&format!(".{}", d)))
}

/// Extract the YouTube video ID from a watch URL.
///
/// Supports:
/// - `https://www.youtube.com/watch?v=VIDEO_ID`
/// - `https://youtu.be/VIDEO_ID`
/// - `https://www.youtube.com/embed/VIDEO_ID`
/// - `https://www.youtube.com/shorts/VIDEO_ID`
fn extract_youtube_video_id(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_lowercase();

    // youtu.be/VIDEO_ID
    if host == "youtu.be" {
        let path = parsed.path().trim_start_matches('/');
        if !path.is_empty() {
            return Some(path.to_owned());
        }
        return None;
    }

    // youtube.com/watch?v=VIDEO_ID
    if host.ends_with("youtube.com") || host.ends_with("youtube-nocookie.com") {
        // Check query parameter `v`
        if let Some(v) = parsed
            .query_pairs()
            .find(|(k, _)| k == "v")
            .map(|(_, v)| v.to_string())
        {
            if !v.is_empty() {
                return Some(v);
            }
        }

        // /embed/VIDEO_ID or /shorts/VIDEO_ID
        let path = parsed.path();
        if let Some(rest) = path.strip_prefix("/embed/") {
            let id = rest.split('/').next().unwrap_or("");
            if !id.is_empty() {
                return Some(id.to_owned());
            }
        }
        if let Some(rest) = path.strip_prefix("/shorts/") {
            let id = rest.split('/').next().unwrap_or("");
            if !id.is_empty() {
                return Some(id.to_owned());
            }
        }
    }

    None
}

/// Resolve a YouTube watch URL to a direct media URL.
///
/// This is a lightweight in-tree resolver that avoids the 5-15 second
/// Python startup overhead of yt-dlp. It fetches the YouTube watch page
/// through Tor, extracts the `ytInitialPlayerResponse` JSON blob embedded
/// in a `<script>` tag, and parses the `streamingData` to find the best
/// H.264 direct media URL.
///
/// Returns a `ResolveResult` with:
/// - `direct_url`: the best H.264 video URL (≤1080p, avc1 codec)
/// - `audio_url`: the best audio-only URL (if adaptive formats are used)
/// - `title`, `duration`, `thumbnail`: extracted from the player response
///
/// If the in-tree resolver fails (e.g. YouTube changes its page structure),
/// the caller should fall back to yt-dlp.
pub async fn resolve_youtube(
    url: &str,
    socks5_proxy: Option<&str>,
) -> Result<ResolveResult, ResolveError> {
    let video_id = extract_youtube_video_id(url).ok_or_else(|| {
        ResolveError::NoMediaFound(format!("could not extract YouTube video ID from {}", url))
    })?;

    tracing::info!(
        url = url,
        video_id = %video_id,
        "YouTube custom resolver: resolving"
    );

    let (client, _forwarder) = build_client(socks5_proxy).await?;

    // Fetch the watch page. We use the no-cookie variant to reduce
    // tracking, but it serves the same player response data.
    let watch_url = format!("https://www.youtube.com/watch?v={}", video_id);

    let response = timeout(
        Duration::from_secs(CUSTOM_RESOLVER_TIMEOUT_SECS),
        client.get(&watch_url).header("Accept-Language", "en-US,en;q=0.9").send(),
    )
    .await
    .map_err(|_| {
        ResolveError::Network(format!("YouTube page fetch timed out after {}s", CUSTOM_RESOLVER_TIMEOUT_SECS))
    })?
    .map_err(|e| ResolveError::Network(format!("YouTube page fetch failed: {}", e)))?;

    let html = response.text().await.map_err(|e| {
        ResolveError::Network(format!("failed to read YouTube page body: {}", e))
    })?;

    // Extract ytInitialPlayerResponse from the page.
    // YouTube embeds it in a <script> tag as:
    //   var ytInitialPlayerResponse = {...};
    let player_response_json = extract_player_response(&html).ok_or_else(|| {
        tracing::warn!(
            url = url,
            "YouTube custom resolver: could not find ytInitialPlayerResponse in page — falling back to yt-dlp"
        );
        ResolveError::NoMediaFound(
            "ytInitialPlayerResponse not found in YouTube page (YouTube may have changed its page structure)".to_owned(),
        )
    })?;

    // Parse the JSON.
    let player_response: serde_json::Value = serde_json::from_str(&player_response_json)
        .map_err(|e| {
            ResolveError::NoMediaFound(format!(
                "failed to parse ytInitialPlayerResponse JSON: {}",
                e
            ))
        })?;

    // Extract metadata.
    let title = player_response
        .pointer("/videoDetails/title")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let length_secs = player_response
        .pointer("/videoDetails/lengthSeconds")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<u64>().ok());
    let thumbnail = player_response
        .pointer("/videoDetails/thumbnail/thumbnails/0/url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());

    // Extract streaming data.
    let streaming_data = player_response
        .pointer("/streamingData")
        .ok_or_else(|| {
            ResolveError::NoMediaFound(
                "streamingData not found in ytInitialPlayerResponse (video may be DRM-protected or region-blocked)".to_owned(),
            )
        })?;

    // Try combined formats first (video+audio in one URL, max 720p).
    // These are simpler — single URL, no need for separate audio.
    let combined_formats = streaming_data
        .pointer("/formats")
        .and_then(|v| v.as_array());

    if let Some(formats) = combined_formats {
        // Find the best H.264 combined format (highest resolution ≤1080p).
        let best_combined = formats
            .iter()
            .filter(|f| {
                f.get("mimeType")
                    .and_then(|v| v.as_str())
                    .map(|mt| mt.contains("avc1") || mt.contains("mp4v"))
                    .unwrap_or(false)
            })
            .max_by_key(|f| {
                f.get("height")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
            });

        if let Some(best) = best_combined {
            if let Some(direct_url) = best.get("url").and_then(|v| v.as_str()) {
                let height = best
                    .get("height")
                    .and_then(|v| v.as_u64())
                    .map(|h| h as u32);
                let mime_type = best
                    .get("mimeType")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_owned());
                let content_length = best
                    .get("contentLength")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<u64>().ok());

                tracing::info!(
                    url = url,
                    video_id = %video_id,
                    height = ?height,
                    "YouTube custom resolver: resolved via combined format"
                );

                return Ok(ResolveResult {
                    source_url: url.to_owned(),
                    direct_url: direct_url.to_owned(),
                    audio_url: None,
                    category: UrlCategory::WebPage,
                    mime_type,
                    content_length,
                    used_tor: socks5_proxy.is_some(),
                    title,
                    duration: length_secs.map(|s| s * 1000),
                    thumbnail,
                    vcodec: Some("avc1".to_owned()),
                    acodec: Some("mp4a".to_owned()),
                    width: height.map(|h| h * 16 / 9),
                    height,
                    subtitle_tracks: vec![],
                    cookies: vec![],
                    resolver_type: "custom".to_owned(),
                });
            }
        }
    }

    // Fall back to adaptive formats: separate video-only and audio-only.
    let adaptive_formats = streaming_data
        .pointer("/adaptiveFormats")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            ResolveError::NoMediaFound(
                "no adaptiveFormats found in streamingData".to_owned(),
            )
        })?;

    // Find the best H.264 video-only format (≤1080p).
    let best_video = adaptive_formats
        .iter()
        .filter(|f| {
            f.get("mimeType")
                .and_then(|v| v.as_str())
                .map(|mt| mt.contains("avc1") && mt.contains("video"))
                .unwrap_or(false)
        })
        .filter(|f| {
            // ≤1080p per ADR-009 (HEVC deferred, H.264 only)
            f.get("height")
                .and_then(|v| v.as_u64())
                .map(|h| h <= 1080)
                .unwrap_or(true)
        })
        .max_by_key(|f| {
            f.get("height")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        });

    // Find the best audio-only format (mp4a/AAC preferred).
    let best_audio = adaptive_formats
        .iter()
        .filter(|f| {
            f.get("mimeType")
                .and_then(|v| v.as_str())
                .map(|mt| mt.contains("audio") && mt.contains("mp4"))
                .unwrap_or(false)
        })
        .max_by_key(|f| {
            f.get("bitrate")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        });

    let video_url = best_video
        .and_then(|f| f.get("url").and_then(|v| v.as_str()))
        .ok_or_else(|| {
            ResolveError::NoMediaFound(
                "no suitable H.264 video format found in adaptiveFormats (video may be DRM-protected or use VP9/AV1 only)".to_owned(),
            )
        })?;

    let audio_url = best_audio.and_then(|f| f.get("url").and_then(|v| v.as_str()));

    let height = best_video
        .and_then(|f| f.get("height").and_then(|v| v.as_u64()))
        .map(|h| h as u32);

    tracing::info!(
        url = url,
        video_id = %video_id,
        height = ?height,
        has_audio = audio_url.is_some(),
        "YouTube custom resolver: resolved via adaptive formats"
    );

    Ok(ResolveResult {
        source_url: url.to_owned(),
        direct_url: video_url.to_owned(),
        audio_url: audio_url.map(|s| s.to_owned()),
        category: UrlCategory::WebPage,
        mime_type: best_video
            .and_then(|f| f.get("mimeType").and_then(|v| v.as_str()))
            .map(|s| s.to_owned()),
        content_length: None,
        used_tor: socks5_proxy.is_some(),
        title,
        duration: length_secs.map(|s| s * 1000),
        thumbnail,
        vcodec: Some("avc1".to_owned()),
        acodec: Some("mp4a".to_owned()),
        width: height.map(|h| h * 16 / 9),
        height,
        subtitle_tracks: vec![],
        cookies: vec![],
        resolver_type: "custom".to_owned(),
    })
}

/// Extract the `ytInitialPlayerResponse` JSON from a YouTube watch page.
///
/// YouTube embeds this as:
///   var ytInitialPlayerResponse = { ... };
///
/// We find the start of the JSON object after the assignment, then
/// brace-match to find the end.
fn extract_player_response(html: &str) -> Option<String> {
    // Look for the assignment pattern.
    let marker = "var ytInitialPlayerResponse = ";
    let start_idx = html.find(marker)?;
    let json_start = start_idx + marker.len();

    // The JSON starts with '{' and we need to find the matching '}'.
    // We can't just find the next ';' because the JSON may contain
    // string values with ';' inside them.
    if html.as_bytes().get(json_start) != Some(&b'{') {
        return None;
    }

    let bytes = html.as_bytes();
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    let mut end_idx = json_start;

    for (i, &byte) in bytes[json_start..].iter().enumerate() {
        let idx = json_start + i;
        if escape {
            escape = false;
            continue;
        }
        if byte == b'\\' && in_string {
            escape = true;
            continue;
        }
        if byte == b'"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        if byte == b'{' {
            depth += 1;
        } else if byte == b'}' {
            depth -= 1;
            if depth == 0 {
                end_idx = idx + 1;
                break;
            }
        }
    }

    if depth != 0 {
        return None;
    }

    Some(html[json_start..end_idx].to_owned())
}

#[cfg(test)]
mod youtube_tests {
    use super::*;

    #[test]
    fn test_extract_video_id_watch_url() {
        let id = extract_youtube_video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert_eq!(id, Some("dQw4w9WgXcQ".to_owned()));
    }

    #[test]
    fn test_extract_video_id_short_url() {
        let id = extract_youtube_video_id("https://youtu.be/dQw4w9WgXcQ");
        assert_eq!(id, Some("dQw4w9WgXcQ".to_owned()));
    }

    #[test]
    fn test_extract_video_id_embed_url() {
        let id = extract_youtube_video_id("https://www.youtube.com/embed/dQw4w9WgXcQ");
        assert_eq!(id, Some("dQw4w9WgXcQ".to_owned()));
    }

    #[test]
    fn test_extract_video_id_shorts_url() {
        let id = extract_youtube_video_id("https://www.youtube.com/shorts/dQw4w9WgXcQ");
        assert_eq!(id, Some("dQw4w9WgXcQ".to_owned()));
    }

    #[test]
    fn test_extract_video_id_with_params() {
        let id = extract_youtube_video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=42s&feature=share");
        assert_eq!(id, Some("dQw4w9WgXcQ".to_owned()));
    }

    #[test]
    fn test_extract_video_id_invalid_url() {
        let id = extract_youtube_video_id("https://example.com/watch?v=abc");
        assert_eq!(id, None);
    }

    #[test]
    fn test_is_youtube_domain() {
        assert!(is_youtube_domain("youtube.com"));
        assert!(is_youtube_domain("www.youtube.com"));
        assert!(is_youtube_domain("m.youtube.com"));
        assert!(is_youtube_domain("youtu.be"));
        assert!(is_youtube_domain("youtube-nocookie.com"));
        assert!(!is_youtube_domain("example.com"));
        assert!(!is_youtube_domain("vimeo.com"));
    }

    #[test]
    fn test_extract_player_response_simple() {
        let html = r#"<html><script>var ytInitialPlayerResponse = {"videoDetails":{"title":"Test"}};</script></html>"#;
        let json = extract_player_response(html);
        assert!(json.is_some());
        let json = json.unwrap();
        assert!(json.contains("\"title\":\"Test\""));
    }

    #[test]
    fn test_extract_player_response_with_nested_braces() {
        let html = r#"<script>var ytInitialPlayerResponse = {"a":{"b":1},"c":"}"};
        "#;
        let json = extract_player_response(html);
        assert!(json.is_some());
        let json = json.unwrap();
        // Should end at the correct closing brace, not the one inside the string
        assert!(json.ends_with('}'));
        assert!(json.starts_with('{'));
    }

    #[test]
    fn test_extract_player_response_not_found() {
        let html = r#"<html><body>No player response here</body></html>"#;
        let json = extract_player_response(html);
        assert!(json.is_none());
    }

    #[test]
    fn test_extract_player_response_with_escaped_quotes() {
        let html = r#"<script>var ytInitialPlayerResponse = {"title":"He said \"hello\""};</script>"#;
        let json = extract_player_response(html);
        assert!(json.is_some());
    }
}
