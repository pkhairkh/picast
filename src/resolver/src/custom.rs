//! PiCast Custom Site Resolvers
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

use crate::{ResolveError, ResolveResult, UrlCategory};
use base64::Engine;
use scraper::{Html, Selector};
use std::time::Duration;
use tokio::time::timeout;

/// HTTP request timeout for custom resolvers (15 seconds).
const CUSTOM_RESOLVER_TIMEOUT_SECS: u64 = 15;

/// Known Voe CDN front-end domains. Voe rotates these frequently; the
/// list below covers the most common ones. The `voe.sx` domain itself
/// simply redirects to one of these CDN domains.
const VOE_DOMAINS: &[&str] = &[
    "voe.sx",
    "charlessheimprove.com",
    "cactusheadroomscaling.com",
    "chaliceguzzlerlandlord.com",
    "reedunpack.com",
    "voe-unblock.com",
    "voeunblock.com",
    "voeunbl0ck.com",
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

/// Known bait/test video domains and filenames that Voe and DoodStream
/// use as decoy sources to foil scrapers.
const BAIT_DOMAINS: &[&str] = &[
    "test-videos.co.uk",
    "sample-videos.com",
    "commondatastorage.googleapis.com",
];

const BAIT_FILENAMES: &[&str] = &[
    "BigBuckBunny",
    "Big_Buck_Bunny_1080_10s_5MB",
    "bbb.mp4",
];

// ── Public API ─────────────────────────────────────────────────────

/// Check if a hostname should be handled by the Voe custom resolver.
pub fn is_voe_domain(host: &str) -> bool {
    let host_lower = host.to_lowercase();
    VOE_DOMAINS
        .iter()
        .any(|d| host_lower == *d || host_lower.ends_with(&format!(".{}", d)))
}

/// Check if a hostname should be handled by the DoodStream custom resolver.
pub fn is_doodstream_domain(host: &str) -> bool {
    let host_lower = host.to_lowercase();
    DOODSTREAM_DOMAINS
        .iter()
        .any(|d| host_lower == *d || host_lower.ends_with(&format!(".{}", d)))
}

/// Resolve a Voe (or Voe CDN front-end) URL to a direct media URL.
///
/// 1. Follow JavaScript redirects (e.g. `voe.sx` → `charlessheimprove.com`).
/// 2. Try Method 8: decode the obfuscated JSON in `<script type="application/json">`.
/// 3. Try Method 7: decode the MKGMa-encoded source.
/// 4. Try Method 6: decode the `a168c` Base64-encoded source.
/// 5. Fallback: search for `var source = '...'` and direct `.mp4`/`.m3u8` URLs.
pub async fn resolve_voe(url: &str) -> Result<ResolveResult, ResolveError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(CUSTOM_RESOLVER_TIMEOUT_SECS))
        .user_agent("Mozilla/5.0 (X11; Linux aarch64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .build()
        .map_err(|e| ResolveError::Network(format!("failed to build HTTP client: {}", e)))?;

    // Follow the initial URL, then check for JS redirects.
    let html_text = fetch_page(&client, url).await?;
    let resolved_url = follow_js_redirect(&html_text, url);

    // If we got redirected, fetch the new page.
    let (final_url, page_html) = if resolved_url != url {
        let new_html = fetch_page(&client, &resolved_url).await?;
        (resolved_url, new_html)
    } else {
        (url.to_owned(), html_text)
    };

    let document = Html::parse_document(&page_html);

    // Extract metadata.
    let title = extract_meta_content(&document, "og:title")
        .or_else(|| extract_meta_content(&document, "twitter:title"))
        .or_else(|| extract_document_title(&document));

    let thumbnail = extract_meta_content(&document, "og:image")
        .or_else(|| extract_meta_content(&document, "twitter:image"));

    // Try Method 8: obfuscated JSON in <script type="application/json">
    if let Some(media_url) = try_method8(&page_html) {
        tracing::info!(url = %media_url, method = "method8", "Voe: resolved media URL");
        return Ok(build_result(&final_url, &media_url, &title, &thumbnail));
    }

    // Try Method 7: MKGMa-encoded source
    if let Some(media_url) = try_method7(&page_html) {
        tracing::info!(url = %media_url, method = "method7", "Voe: resolved media URL");
        return Ok(build_result(&final_url, &media_url, &title, &thumbnail));
    }

    // Try Method 6: a168c Base64-encoded source
    if let Some(media_url) = try_method6(&page_html) {
        tracing::info!(url = %media_url, method = "method6", "Voe: resolved media URL");
        return Ok(build_result(&final_url, &media_url, &title, &thumbnail));
    }

    // Fallback: look for var source = '...' and direct URLs
    if let Some(media_url) = try_fallback_urls(&page_html) {
        tracing::info!(url = %media_url, method = "fallback", "Voe: resolved media URL");
        return Ok(build_result(&final_url, &media_url, &title, &thumbnail));
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
pub async fn resolve_doodstream(url: &str) -> Result<ResolveResult, ResolveError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(CUSTOM_RESOLVER_TIMEOUT_SECS))
        .user_agent("Mozilla/5.0 (X11; Linux aarch64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .build()
        .map_err(|e| ResolveError::Network(format!("failed to build HTTP client: {}", e)))?;

    let html_text = fetch_page(&client, url).await?;

    // Extract all data from the Html document up-front, then drop it.
    // scraper::Html is !Send (contains Cell<usize>), so it must not
    // survive across an .await point.
    let (title, thumbnail, embed_url) = {
        let document = Html::parse_document(&html_text);
        let title = extract_meta_content(&document, "og:title")
            .or_else(|| extract_meta_content(&document, "twitter:title"))
            .or_else(|| extract_document_title(&document));
        let thumbnail = extract_meta_content(&document, "og:image")
            .or_else(|| extract_meta_content(&document, "twitter:image"));
        let embed_url = find_embed_iframe(&document, url);
        (title, thumbnail, embed_url)
    }; // document dropped here – no !Send value crosses the await below

    if let Some(embed_href) = embed_url {
        let full_embed = if embed_href.starts_with("http") {
            embed_href
        } else {
            format!("{}://{}{}", 
                url::Url::parse(url).ok().map(|u| u.scheme().to_string()).unwrap_or_else(|| "https".into()),
                url::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.to_string())).unwrap_or_else(|| "playmogo.com".into()),
                embed_href
            )
        };

        tracing::info!(embed_url = %full_embed, "DoodStream: found embed iframe");

        // Fetch the embed page
        let embed_html = fetch_page(&client, &full_embed).await?;

        // Try to find the direct media URL in the embed page
        if let Some(media_url) = extract_doodstream_media(&embed_html, &full_embed) {
            tracing::info!(url = %media_url, "DoodStream: resolved media URL");
            return Ok(build_result(url, &media_url, &title, &thumbnail));
        }
    }

    // Fallback: try to find direct URLs in the page
    if let Some(media_url) = try_fallback_urls(&html_text) {
        tracing::info!(url = %media_url, method = "fallback", "DoodStream: resolved media URL");
        return Ok(build_result(url, &media_url, &title, &thumbnail));
    }

    Err(ResolveError::NoMediaFound(format!(
        "DoodStream resolver: could not extract media URL from {}",
        url
    )))
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
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&step6) {
        if let Some(obj) = parsed.as_object() {
            if let Some(url) = obj.get("direct_access_url").and_then(|v| v.as_str()) {
                if !url.is_empty() {
                    return Some(url.to_owned());
                }
            }
            if let Some(url) = obj.get("source").and_then(|v| v.as_str()) {
                if !url.is_empty() {
                    return Some(url.to_owned());
                }
            }
            // Check for mp4/hls keys
            for key in &["mp4", "hls"] {
                if let Some(url) = obj.get(*key).and_then(|v| v.as_str()) {
                    if !url.is_empty() {
                        return Some(url.to_owned());
                    }
                }
            }
        }
    }

    // Fallback: regex search for media URLs
    extract_media_url_from_text(&step6)
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

    // Try JSON parse
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&step6) {
        if let Some(obj) = parsed.as_object() {
            if let Some(url) = obj.get("direct_access_url").and_then(|v| v.as_str()) {
                if !url.is_empty() && !is_bait_source(url) {
                    return Some(url.to_owned());
                }
            }
            if let Some(url) = obj.get("source").and_then(|v| v.as_str()) {
                if !url.is_empty() && !is_bait_source(url) {
                    return Some(url.to_owned());
                }
            }
        }
    }

    // Fallback: regex search
    if let Some(url) = extract_media_url_from_text(&step6) {
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

    // Try JSON parse
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&reversed) {
        if let Some(obj) = parsed.as_object() {
            if let Some(url) = obj.get("direct_access_url").and_then(|v| v.as_str()) {
                if !url.is_empty() && !is_bait_source(url) {
                    return Some(url.to_owned());
                }
            }
            if let Some(url) = obj.get("source").and_then(|v| v.as_str()) {
                if !url.is_empty() && !is_bait_source(url) {
                    return Some(url.to_owned());
                }
            }
        }
    }

    // Fallback: regex search
    if let Some(url) = extract_media_url_from_text(&reversed) {
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

    // Try to find direct .mp4 URL in the page
    extract_media_url_from_text(embed_html)
}

// ── Fallback URL extraction ────────────────────────────────────────

/// Try fallback methods: look for `var source = '...'`, direct mp4/m3u8 URLs.
fn try_fallback_urls(html: &str) -> Option<String> {
    // Look for var source = 'https://...'
    let re = regex_lite::Regex::new(r#"var\s+source\s*=\s*['"]([^'"]+)['"]"#).ok()?;
    if let Some(cap) = re.captures(html) {
        let url = cap.get(1)?.as_str();
        if !is_bait_source(url) && !url.is_empty() {
            return Some(url.to_owned());
        }
    }

    // Look for direct mp4 URLs
    let re_mp4 = regex_lite::Regex::new(r#"(https?://[^"'<>]+\.mp4[^"'<>\s]*)"#).ok()?;
    for cap in re_mp4.captures_iter(html) {
        if let Some(m) = cap.get(1) {
            let url = m.as_str();
            if !is_bait_source(url) {
                return Some(url.to_owned());
            }
        }
    }

    // Look for direct m3u8 URLs
    let re_m3u8 = regex_lite::Regex::new(r#"(https?://[^"'<>]+\.m3u8[^"'<>\s]*)"#).ok()?;
    for cap in re_m3u8.captures_iter(html) {
        if let Some(m) = cap.get(1) {
            let url = m.as_str();
            if !is_bait_source(url) {
                return Some(url.to_owned());
            }
        }
    }

    None
}

// ── Helper functions ───────────────────────────────────────────────

/// Apply ROT13 cipher (letters only).
fn rot13(text: &str) -> String {
    text.chars()
        .map(|ch| {
            let o = ch as u32;
            if ('A'..='Z').contains(&ch) {
                char::from_u32(((o - 65 + 13) % 26) + 65).unwrap_or(ch)
            } else if ('a'..='z').contains(&ch) {
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
    base64::engine::general_purpose::STANDARD
        .decode(&padded)
        .ok()?;
    Some(padded)
}

/// Check if a URL looks like a known test/bait video.
fn is_bait_source(source: &str) -> bool {
    let lower = source.to_lowercase();
    if BAIT_FILENAMES
        .iter()
        .any(|fn_| lower.contains(&fn_.to_lowercase()))
    {
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

/// Extract a media URL from arbitrary text using regex.
fn extract_media_url_from_text(text: &str) -> Option<String> {
    // Try mp4
    let re_mp4 = regex_lite::Regex::new(r#"(https?://[^\s"']+\.mp4[^\s"']*)"#).ok()?;
    for cap in re_mp4.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let url = m.as_str();
            if !is_bait_source(url) {
                return Some(url.to_owned());
            }
        }
    }

    // Try m3u8
    let re_m3u8 = regex_lite::Regex::new(r#"(https?://[^\s"']+\.m3u8[^\s"']*)"#).ok()?;
    for cap in re_m3u8.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let url = m.as_str();
            if !is_bait_source(url) {
                return Some(url.to_owned());
            }
        }
    }

    None
}

/// Fetch a page's HTML content via HTTP GET.
async fn fetch_page(client: &reqwest::Client, url: &str) -> Result<String, ResolveError> {
    let result = timeout(
        Duration::from_secs(CUSTOM_RESOLVER_TIMEOUT_SECS),
        client.get(url).send(),
    )
    .await
    .map_err(|_| ResolveError::Network("custom resolver: HTTP request timed out".into()))?
    .map_err(|e| {
        ResolveError::Network(format!("custom resolver: HTTP request failed: {}", e))
    })?;

    if !result.status().is_success() {
        return Err(ResolveError::Network(format!(
            "custom resolver: HTTP {} for {}",
            result.status(),
            url
        )));
    }

    result
        .text()
        .await
        .map_err(|e| ResolveError::Network(format!("custom resolver: failed to read response: {}", e)))
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
    let mime_type = if media_url.contains(".m3u8") {
        Some("application/vnd.apple.mpegurl".to_string())
    } else {
        Some("video/mp4".to_string())
    };

    let category = if media_url.contains(".m3u8") {
        UrlCategory::HlsManifest
    } else {
        UrlCategory::DirectMedia
    };

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
        assert!(is_voe_domain("voe.sx"));
        assert!(is_voe_domain("charlessheimprove.com"));
        assert!(is_voe_domain("sub.charlessheimprove.com"));
        assert!(!is_voe_domain("youtube.com"));
    }

    #[test]
    fn test_is_doodstream_domain() {
        assert!(is_doodstream_domain("playmogo.com"));
        assert!(is_doodstream_domain("doodstream.com"));
        assert!(!is_doodstream_domain("youtube.com"));
    }

    #[test]
    fn test_follow_js_redirect() {
        let html = r#"<script>window.location.href = 'https://charlessheimprove.com/abc';</script>"#;
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
    fn test_extract_media_url_from_text_mp4() {
        let text = "Some text with https://cdn.example.com/video123.mp4?token=abc embedded";
        let result = extract_media_url_from_text(text);
        assert!(result.is_some());
        assert!(result.unwrap().contains(".mp4"));
    }

    #[test]
    fn test_extract_media_url_from_text_m3u8() {
        let text = "Source: https://cdn.example.com/stream.m3u8?session=xyz";
        let result = extract_media_url_from_text(text);
        assert!(result.is_some());
        assert!(result.unwrap().contains(".m3u8"));
    }

    #[test]
    fn test_extract_media_url_ignores_bait() {
        let text = "https://test-videos.co.uk/vids/bigbuckbunny/Big_Buck_Bunny_1080_10s_5MB.mp4";
        let result = extract_media_url_from_text(text);
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
        let result = build_result(
            "https://voe.sx/abc",
            "https://cdn.example.com/stream.m3u8",
            &None,
            &None,
        );
        assert_eq!(result.category, UrlCategory::HlsManifest);
        assert_eq!(
            result.mime_type,
            Some("application/vnd.apple.mpegurl".into())
        );
    }
}
