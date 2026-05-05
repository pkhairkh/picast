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

/// Known video hosting domains that require yt-dlp for resolution.
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
}
