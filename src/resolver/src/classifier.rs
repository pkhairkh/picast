//! PiCast URL Classifier
//!
//! Pure URL-based classification without any network access.
//! Determines the [`UrlCategory`] for a given URL based on
//! hostname patterns, path extensions, and known site domains.

use url::Url;

/// High-level classification of the resolved URL.
///
/// Helps downstream components choose the right handling strategy
/// (e.g. HLS needs adaptive bitrate logic; a plain file can be
/// streamed directly).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UrlCategory {
    /// Direct link to a media file (mp4, mkv, mp3, etc.).
    DirectMedia,
    /// HTTP Live Streaming manifest (.m3u8).
    HlsManifest,
    /// MPEG-DASH manifest (.mpd).
    DashManifest,
    /// A web page that embeds media (YouTube, Vimeo, etc.).
    WebPage,
    /// A magnet / torrent link (not yet supported).
    Magnet,
    /// An `.onion` address that must be fetched over Tor.
    Onion,
}

impl std::fmt::Display for UrlCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UrlCategory::DirectMedia => write!(f, "direct_media"),
            UrlCategory::HlsManifest => write!(f, "hls_manifest"),
            UrlCategory::DashManifest => write!(f, "dash_manifest"),
            UrlCategory::WebPage => write!(f, "web_page"),
            UrlCategory::Magnet => write!(f, "magnet"),
            UrlCategory::Onion => write!(f, "onion"),
        }
    }
}

impl UrlCategory {
    /// Parse a category string (as produced by `Display`) back into a
    /// `UrlCategory`. Returns `DirectMedia` for unrecognized strings.
    ///
    /// This is a non-fallible parse (unlike `std::str::FromStr`), so it
    /// is intentionally named `parse_name` to avoid confusion with the
    /// standard library trait.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "direct_media" => UrlCategory::DirectMedia,
            "hls_manifest" => UrlCategory::HlsManifest,
            "dash_manifest" => UrlCategory::DashManifest,
            "web_page" => UrlCategory::WebPage,
            "magnet" => UrlCategory::Magnet,
            "onion" => UrlCategory::Onion,
            _ => {
                tracing::warn!(
                    category = s,
                    "unknown UrlCategory string — defaulting to DirectMedia"
                );
                UrlCategory::DirectMedia
            },
        }
    }
}

/// Known video hosting domains that require yt-dlp for resolution.
///
/// Voe front-end domains are NOT listed here — they're detected dynamically
/// by the Voe heuristic in `custom::is_voe_domain()`. Voe rotates domains
/// constantly, so a static list is futile. The default classification rule
/// (Rule 7: unknown URLs → WebPage) catches any Voe domain not matched by
/// earlier rules, and the Voe resolver tries content-based detection for
/// ALL WebPage URLs regardless of domain.
const WEB_PAGE_DOMAINS: &[&str] = &[
    "youtube.com",
    "www.youtube.com",
    "m.youtube.com",
    "youtu.be",
    "vimeo.com",
    "player.vimeo.com",
    "twitch.tv",
    "www.twitch.tv",
    "clips.twitch.tv",
    "dailymotion.com",
    "www.dailymotion.com",
    "facebook.com",
    "www.facebook.com",
    "instagram.com",
    "www.instagram.com",
    "twitter.com",
    "x.com",
    "www.x.com",
    "tiktok.com",
    "www.tiktok.com",
    "reddit.com",
    "www.reddit.com",
    "streamable.com",
    // Voe canonical domain (front-end domains detected by heuristic)
    "voe.sx",
    // DoodStream front-end domains (handled by custom resolver)
    "playmogo.com",
    "doodstream.com",
    "dood.to",
    "dood.watch",
    "dood.la",
    "dood.ws",
];

/// File extensions that indicate a direct media file.
const DIRECT_MEDIA_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "webm", "avi", "mov", "flv", "wmv", "m4v", // video
    "mp3", "flac", "ogg", "opus", "wav", "aac", "m4a", "wma", // audio
    "ts",  // MPEG-TS segment
];

/// Classify a parsed URL into a [`UrlCategory`].
///
/// This function performs **no network access** — it relies solely
/// on URL structure (hostname, path, query parameters).
///
/// ## Classification Rules (in priority order)
///
/// 1. `.onion` host → `Onion`
/// 2. `magnet:` scheme → `Magnet`
/// 3. Known web-page domains → `WebPage`
/// 4. `.m3u8` path extension → `HlsManifest`
/// 5. `.mpd` path extension → `DashManifest`
/// 6. Direct media extension → `DirectMedia`
/// 7. Default → `WebPage`
pub fn classify_url(url: &Url) -> UrlCategory {
    // Rule 1: .onion host
    if let Some(host) = url.host_str() {
        if host.ends_with(".onion") {
            return UrlCategory::Onion;
        }
    }

    // Rule 2: magnet scheme
    if url.scheme() == "magnet" {
        return UrlCategory::Magnet;
    }

    // Rule 3: Known web-page domains
    if let Some(host) = url.host_str() {
        let host_lower = host.to_lowercase();
        for domain in WEB_PAGE_DOMAINS {
            if host_lower == *domain || host_lower.ends_with(&format!(".{}", domain)) {
                return UrlCategory::WebPage;
            }
        }

        // Rule 3b: Voe-style rotating front-end domains.
        // Voe rotates domains constantly (e.g. "maryspecialwatch.com",
        // "cactusheadroomscaling.com"). The heuristic in
        // `custom::is_voe_domain()` catches these dynamically without
        // needing a static list. Classifying them as WebPage here
        // avoids the fallback to Rule 7 (default), making the
        // classification more explicit and saving a string comparison.
        if crate::custom::is_voe_domain(host) {
            return UrlCategory::WebPage;
        }
    }

    // Rule 4-6: Path extension
    let path = url.path().to_lowercase();
    let extension = path.rsplit('.').next().unwrap_or("");

    if extension == "m3u8" {
        return UrlCategory::HlsManifest;
    }

    if extension == "mpd" {
        return UrlCategory::DashManifest;
    }

    for ext in DIRECT_MEDIA_EXTENSIONS {
        if extension == *ext {
            return UrlCategory::DirectMedia;
        }
    }

    // Rule 7: Default
    UrlCategory::WebPage
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(url: &str) -> UrlCategory {
        let parsed = Url::parse(url).expect("URL should parse");
        classify_url(&parsed)
    }

    #[test]
    fn onion_domain() {
        assert_eq!(classify("http://xyz123456.onion/video.mp4"), UrlCategory::Onion);
        assert_eq!(classify("https://mysecret.onion/"), UrlCategory::Onion);
    }

    #[test]
    fn magnet_link() {
        assert_eq!(classify("magnet:?xt=urn:btih:abc123&dn=test"), UrlCategory::Magnet);
    }

    #[test]
    fn youtube_webpage() {
        assert_eq!(classify("https://www.youtube.com/watch?v=dQw4w9WgXcQ"), UrlCategory::WebPage);
        assert_eq!(classify("https://youtu.be/dQw4w9WgXcQ"), UrlCategory::WebPage);
    }

    #[test]
    fn vimeo_webpage() {
        assert_eq!(classify("https://vimeo.com/123456789"), UrlCategory::WebPage);
    }

    #[test]
    fn twitch_webpage() {
        assert_eq!(classify("https://www.twitch.tv/videos/12345"), UrlCategory::WebPage);
    }

    #[test]
    fn hls_manifest() {
        assert_eq!(classify("https://cdn.example.com/live/stream.m3u8"), UrlCategory::HlsManifest);
    }

    #[test]
    fn dash_manifest() {
        assert_eq!(classify("https://cdn.example.com/vod/stream.mpd"), UrlCategory::DashManifest);
    }

    #[test]
    fn direct_media_video() {
        for ext in &["mp4", "mkv", "webm", "avi", "mov", "flv"] {
            assert_eq!(
                classify(&format!("https://example.com/media.{}", ext)),
                UrlCategory::DirectMedia,
                ".{} should be DirectMedia",
                ext
            );
        }
    }

    #[test]
    fn direct_media_audio() {
        for ext in &["mp3", "flac", "ogg", "opus", "wav", "aac"] {
            assert_eq!(
                classify(&format!("https://example.com/audio.{}", ext)),
                UrlCategory::DirectMedia,
                ".{} should be DirectMedia",
                ext
            );
        }
    }

    #[test]
    fn direct_media_ts_segment() {
        assert_eq!(classify("https://cdn.example.com/segment0.ts"), UrlCategory::DirectMedia);
    }

    #[test]
    fn unknown_domain_defaults_to_webpage() {
        assert_eq!(classify("https://some-random-site.com/page"), UrlCategory::WebPage);
    }

    #[test]
    fn url_category_display() {
        assert_eq!(UrlCategory::DirectMedia.to_string(), "direct_media");
        assert_eq!(UrlCategory::HlsManifest.to_string(), "hls_manifest");
        assert_eq!(UrlCategory::Onion.to_string(), "onion");
    }

    #[test]
    fn url_category_serde() {
        for cat in [
            UrlCategory::DirectMedia,
            UrlCategory::HlsManifest,
            UrlCategory::DashManifest,
            UrlCategory::WebPage,
            UrlCategory::Magnet,
            UrlCategory::Onion,
        ] {
            let json = serde_json::to_string(&cat).unwrap();
            let decoded: UrlCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, cat);
        }
    }

    #[test]
    fn case_insensitive_host() {
        assert_eq!(classify("https://WWW.YOUTUBE.COM/watch?v=abc"), UrlCategory::WebPage);
    }

    #[test]
    fn case_insensitive_extension() {
        assert_eq!(classify("https://example.com/video.MP4"), UrlCategory::DirectMedia);
        assert_eq!(classify("https://cdn.example.com/STREAM.M3U8"), UrlCategory::HlsManifest);
    }

    // ── Comprehensive classifier tests ──────────────────────────────────

    #[test]
    fn direct_media_all_video_extensions() {
        for ext in &["mp4", "webm", "mkv", "avi", "mov", "flv", "wmv", "m4v"] {
            assert_eq!(
                classify(&format!("https://example.com/media.{}", ext)),
                UrlCategory::DirectMedia,
                ".{} should be DirectMedia",
                ext
            );
        }
    }

    #[test]
    fn direct_media_all_audio_extensions() {
        for ext in &["mp3", "flac", "ogg", "opus", "wav", "aac", "m4a", "wma"] {
            assert_eq!(
                classify(&format!("https://example.com/audio.{}", ext)),
                UrlCategory::DirectMedia,
                ".{} should be DirectMedia",
                ext
            );
        }
    }

    #[test]
    fn hls_manifest_m3u8() {
        assert_eq!(classify("https://cdn.example.com/live/stream.m3u8"), UrlCategory::HlsManifest);
    }

    #[test]
    fn dash_manifest_mpd() {
        assert_eq!(classify("https://cdn.example.com/vod/stream.mpd"), UrlCategory::DashManifest);
    }

    #[test]
    fn webpage_youtube() {
        assert_eq!(classify("https://www.youtube.com/watch?v=dQw4w9WgXcQ"), UrlCategory::WebPage);
        assert_eq!(classify("https://youtube.com/watch?v=abc"), UrlCategory::WebPage);
        assert_eq!(classify("https://m.youtube.com/watch?v=abc"), UrlCategory::WebPage);
        assert_eq!(classify("https://youtu.be/dQw4w9WgXcQ"), UrlCategory::WebPage);
    }

    #[test]
    fn webpage_vimeo() {
        assert_eq!(classify("https://vimeo.com/123456789"), UrlCategory::WebPage);
        assert_eq!(classify("https://player.vimeo.com/video/123456789"), UrlCategory::WebPage);
    }

    #[test]
    fn webpage_twitch() {
        assert_eq!(classify("https://www.twitch.tv/videos/12345"), UrlCategory::WebPage);
        assert_eq!(classify("https://twitch.tv/videos/12345"), UrlCategory::WebPage);
        assert_eq!(classify("https://clips.twitch.tv/Slug123"), UrlCategory::WebPage);
    }

    #[test]
    fn webpage_dailymotion() {
        assert_eq!(classify("https://www.dailymotion.com/video/x123abc"), UrlCategory::WebPage);
        assert_eq!(classify("https://dailymotion.com/video/x123abc"), UrlCategory::WebPage);
    }

    #[test]
    fn webpage_other_known_domains() {
        // Test a selection of other known domains from the WEB_PAGE_DOMAINS list.
        assert_eq!(classify("https://www.facebook.com/watch/?v=123"), UrlCategory::WebPage);
        assert_eq!(classify("https://www.instagram.com/reel/abc123/"), UrlCategory::WebPage);
        assert_eq!(classify("https://twitter.com/user/status/123"), UrlCategory::WebPage);
        assert_eq!(classify("https://x.com/user/status/123"), UrlCategory::WebPage);
        assert_eq!(classify("https://www.tiktok.com/@user/video/123"), UrlCategory::WebPage);
        assert_eq!(classify("https://www.reddit.com/r/videos/comments/abc/"), UrlCategory::WebPage);
        assert_eq!(classify("https://streamable.com/abc123"), UrlCategory::WebPage);
    }

    #[test]
    fn onion_domains() {
        assert_eq!(classify("http://xyz123456.onion/video.mp4"), UrlCategory::Onion);
        assert_eq!(classify("https://mysecret.onion/"), UrlCategory::Onion);
        assert_eq!(classify("http://subdomain.example.onion/path"), UrlCategory::Onion);
    }

    #[test]
    fn magnet_links() {
        assert_eq!(classify("magnet:?xt=urn:btih:abc123&dn=test"), UrlCategory::Magnet);
        assert_eq!(
            classify("magnet:?xt=urn:btih:deadbeef&tr=udp://tracker.example.com:1337"),
            UrlCategory::Magnet
        );
    }

    #[test]
    fn url_with_query_string_preserves_category() {
        // Direct media with query string.
        assert_eq!(
            classify("https://cdn.example.com/video.mp4?token=abc123&expires=999"),
            UrlCategory::DirectMedia
        );
        // HLS with query string.
        assert_eq!(
            classify("https://cdn.example.com/stream.m3u8?session=xyz"),
            UrlCategory::HlsManifest
        );
        // DASH with query string.
        assert_eq!(
            classify("https://cdn.example.com/stream.mpd?id=123"),
            UrlCategory::DashManifest
        );
    }

    #[test]
    fn url_with_fragment_preserves_category() {
        // Direct media with fragment.
        assert_eq!(classify("https://cdn.example.com/video.mp4#t=30"), UrlCategory::DirectMedia);
        // HLS with fragment.
        assert_eq!(
            classify("https://cdn.example.com/stream.m3u8#chapter1"),
            UrlCategory::HlsManifest
        );
    }

    #[test]
    fn url_with_query_and_fragment() {
        assert_eq!(
            classify("https://cdn.example.com/video.mp4?token=abc#t=30"),
            UrlCategory::DirectMedia
        );
    }

    #[test]
    fn case_insensitive_domains() {
        assert_eq!(classify("https://WWW.YOUTUBE.COM/watch?v=abc"), UrlCategory::WebPage);
        assert_eq!(classify("https://YOUTUBE.COM/watch?v=abc"), UrlCategory::WebPage);
        assert_eq!(classify("https://VIMEO.COM/123"), UrlCategory::WebPage);
        assert_eq!(classify("https://WWW.TWITCH.TV/videos/123"), UrlCategory::WebPage);
        assert_eq!(classify("https://DAILYMOTION.COM/video/x123"), UrlCategory::WebPage);
        assert_eq!(classify("https://TWITCH.TV/videos/123"), UrlCategory::WebPage);
    }

    #[test]
    fn case_insensitive_extensions_all() {
        assert_eq!(classify("https://example.com/video.MP4"), UrlCategory::DirectMedia);
        assert_eq!(classify("https://example.com/video.Mp4"), UrlCategory::DirectMedia);
        assert_eq!(classify("https://example.com/video.WebM"), UrlCategory::DirectMedia);
        assert_eq!(classify("https://example.com/audio.FLAC"), UrlCategory::DirectMedia);
        assert_eq!(classify("https://example.com/audio.Mp3"), UrlCategory::DirectMedia);
        assert_eq!(classify("https://cdn.example.com/STREAM.M3U8"), UrlCategory::HlsManifest);
        assert_eq!(classify("https://cdn.example.com/stream.Mpd"), UrlCategory::DashManifest);
        assert_eq!(classify("https://cdn.example.com/segment.TS"), UrlCategory::DirectMedia);
    }

    #[test]
    fn onion_takes_priority_over_media_extension() {
        // An .onion domain with a .mp4 path should classify as Onion, not DirectMedia.
        assert_eq!(classify("http://xyz123456.onion/video.mp4"), UrlCategory::Onion);
    }

    #[test]
    fn magnet_takes_priority_over_other_rules() {
        // magnet: scheme should classify as Magnet even though the rest
        // doesn't look like a URL with a host.
        assert_eq!(classify("magnet:?xt=urn:btih:abc123"), UrlCategory::Magnet);
    }

    #[test]
    fn webpage_takes_priority_over_default() {
        // Known domain should be WebPage, not default to WebPage for unknown reasons.
        assert_eq!(classify("https://youtube.com/"), UrlCategory::WebPage);
        assert_eq!(classify("https://vimeo.com/"), UrlCategory::WebPage);
    }

    #[test]
    fn invalid_malformed_urls() {
        // URLs that cannot be parsed should fail at the Url::parse level.
        // Our classify() helper panics on parse failure, so we test parse directly.
        assert!(Url::parse("not a url at all").is_err());
        assert!(Url::parse("://missing-scheme").is_err());
        assert!(Url::parse("").is_err());
    }

    #[test]
    fn valid_url_with_no_path_defaults_to_webpage() {
        assert_eq!(classify("https://example.com"), UrlCategory::WebPage);
    }

    #[test]
    fn webpage_subdomain_of_known_domain() {
        // Subdomains of known domains should also classify as WebPage.
        assert_eq!(classify("https://player.vimeo.com/video/123"), UrlCategory::WebPage);
        assert_eq!(classify("https://clips.twitch.tv/Slug123"), UrlCategory::WebPage);
    }

    #[test]
    fn direct_media_with_nested_path() {
        assert_eq!(
            classify("https://cdn.example.com/videos/2024/01/my.video.mp4"),
            UrlCategory::DirectMedia
        );
    }

    #[test]
    fn hls_with_nested_path() {
        assert_eq!(
            classify("https://cdn.example.com/live/2024/stream.m3u8"),
            UrlCategory::HlsManifest
        );
    }

    #[test]
    fn dash_with_nested_path() {
        assert_eq!(
            classify("https://cdn.example.com/vod/2024/manifest.mpd"),
            UrlCategory::DashManifest
        );
    }

    #[test]
    fn url_category_display_all_variants() {
        assert_eq!(UrlCategory::DirectMedia.to_string(), "direct_media");
        assert_eq!(UrlCategory::HlsManifest.to_string(), "hls_manifest");
        assert_eq!(UrlCategory::DashManifest.to_string(), "dash_manifest");
        assert_eq!(UrlCategory::WebPage.to_string(), "web_page");
        assert_eq!(UrlCategory::Magnet.to_string(), "magnet");
        assert_eq!(UrlCategory::Onion.to_string(), "onion");
    }

    #[test]
    fn url_category_serde_roundtrip_all_variants() {
        for cat in [
            UrlCategory::DirectMedia,
            UrlCategory::HlsManifest,
            UrlCategory::DashManifest,
            UrlCategory::WebPage,
            UrlCategory::Magnet,
            UrlCategory::Onion,
        ] {
            let json = serde_json::to_string(&cat).unwrap();
            let decoded: UrlCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, cat, "serde roundtrip failed for {:?}", cat);
        }
    }

    #[test]
    fn url_category_serde_snake_case() {
        // Verify that serde uses snake_case as specified by the rename_all attribute.
        let json = serde_json::to_string(&UrlCategory::DirectMedia).unwrap();
        assert_eq!(json, "\"direct_media\"");
        let json = serde_json::to_string(&UrlCategory::HlsManifest).unwrap();
        assert_eq!(json, "\"hls_manifest\"");
        let json = serde_json::to_string(&UrlCategory::DashManifest).unwrap();
        assert_eq!(json, "\"dash_manifest\"");
        let json = serde_json::to_string(&UrlCategory::WebPage).unwrap();
        assert_eq!(json, "\"web_page\"");
        let json = serde_json::to_string(&UrlCategory::Magnet).unwrap();
        assert_eq!(json, "\"magnet\"");
        let json = serde_json::to_string(&UrlCategory::Onion).unwrap();
        assert_eq!(json, "\"onion\"");
    }

    #[test]
    fn classification_priority_onion_over_webpage() {
        // Onion domain should take priority even if it's also a known WebPage domain pattern.
        // (Not realistic, but verifies priority ordering.)
        assert_eq!(classify("http://youtube.onion/watch?v=abc"), UrlCategory::Onion);
    }

    #[test]
    fn classification_priority_magnet_over_webpage() {
        // magnet: scheme should take priority over everything else after onion.
        // A magnet link doesn't have a host, so this just verifies the scheme check works.
        assert_eq!(classify("magnet:?xt=urn:btih:abc123"), UrlCategory::Magnet);
    }

    #[test]
    fn classification_priority_webpage_over_media_extension() {
        // A YouTube URL ending in .mp4 should still be WebPage (known domain takes priority).
        assert_eq!(classify("https://www.youtube.com/video.mp4"), UrlCategory::WebPage);
        // A Vimeo URL ending in .mp4 should also be WebPage.
        assert_eq!(classify("https://vimeo.com/video.mp4"), UrlCategory::WebPage);
    }

    #[test]
    fn http_vs_https_same_classification() {
        assert_eq!(classify("http://example.com/video.mp4"), UrlCategory::DirectMedia);
        assert_eq!(classify("https://example.com/video.mp4"), UrlCategory::DirectMedia);
        assert_eq!(classify("http://cdn.example.com/stream.m3u8"), UrlCategory::HlsManifest);
        assert_eq!(classify("https://cdn.example.com/stream.m3u8"), UrlCategory::HlsManifest);
    }
}
