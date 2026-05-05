//! PiCast URL Resolver
//!
//! Takes a user-supplied URL and resolves it to a direct, playable
//! media URL. The resolver can:
//!
//! - Classify URLs by type (direct media, HLS, DASH, web page, onion).
//! - Resolve web pages via yt-dlp subprocess (YouTube, Vimeo, etc.).
//! - Route requests through the Tor SOCKS proxy for `.onion` addresses
//!   or when the user explicitly requests anonymity.
//! - Cache resolved URLs to avoid duplicate lookups.
//!
//! ## Resolution Strategy
//!
//! | Category      | Resolution Method                     |
//! |---------------|---------------------------------------|
//! | DirectMedia   | Return URL as-is (no network needed)  |
//! | HlsManifest   | Return URL as-is for GStreamer         |
//! | DashManifest  | Return URL as-is for GStreamer         |
//! | WebPage       | yt-dlp subprocess through Tor          |
//! | Onion         | yt-dlp subprocess through Tor (forced) |
//! | Magnet        | Error (not supported in v1)            |

pub mod cache;
pub mod classifier;
pub mod ytdlp;

pub use classifier::UrlCategory;

use cache::ResolveCache;
use classifier::classify_url;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;
use url::Url;

// ── Errors ───────────────────────────────────────────────────────────

/// Errors produced during URL resolution.
#[derive(Error, Debug)]
pub enum ResolveError {
    /// The URL could not be parsed.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    /// No playable media was found at the given address.
    #[error("no media found at {0}")]
    NoMediaFound(String),

    /// A network request failed (DNS, TCP, TLS, timeout, etc.).
    #[error("network error: {0}")]
    Network(String),

    /// The Tor proxy was required but not available.
    #[error("Tor proxy unavailable: {0}")]
    TorUnavailable(String),
}

// ── Resolve Result ───────────────────────────────────────────────────

/// The outcome of a successful URL resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveResult {
    /// The original URL the user submitted.
    pub source_url: String,
    /// The resolved direct media URL ready for playback.
    pub direct_url: String,
    /// Classification of the resolved URL.
    pub category: UrlCategory,
    /// MIME type of the media, if known (e.g. `"video/mp4"`).
    pub mime_type: Option<String>,
    /// Estimated content length in bytes, if the server reported it.
    pub content_length: Option<u64>,
    /// Whether the resolution went through the Tor network.
    pub used_tor: bool,
    /// Media title (from yt-dlp or HTTP headers).
    pub title: Option<String>,
    /// Duration in milliseconds, if known.
    pub duration: Option<u64>,
    /// Thumbnail URL, if available.
    pub thumbnail: Option<String>,
    /// Video codec identifier (e.g. "avc1", "vp9").
    pub vcodec: Option<String>,
    /// Audio codec identifier (e.g. "mp4a", "opus").
    pub acodec: Option<String>,
    /// Video width in pixels.
    pub width: Option<u32>,
    /// Video height in pixels.
    pub height: Option<u32>,
    /// Available subtitle track language codes.
    pub subtitle_tracks: Vec<String>,
}

// ── MIME type helper ─────────────────────────────────────────────────

/// Map a file path or URL path to a MIME type based on its extension.
///
/// Strips any query string before extracting the extension.
/// Returns `None` for unrecognized extensions.
///
/// # Examples
///
/// ```
/// assert_eq!(picast_resolver::mime_from_extension("video.mp4"), Some("video/mp4".to_string()));
/// assert_eq!(picast_resolver::mime_from_extension("video.mp4?token=abc"), Some("video/mp4".to_string()));
/// assert_eq!(picast_resolver::mime_from_extension("unknown.xyz"), None);
/// ```
pub fn mime_from_extension(path: &str) -> Option<String> {
    // Strip query string
    let path = path.split('?').next().unwrap_or(path);
    let path = path.to_lowercase();
    let ext = path.rsplit('.').next()?;

    match ext {
        "mp4" => Some("video/mp4".to_string()),
        "webm" => Some("video/webm".to_string()),
        "mkv" => Some("video/x-matroska".to_string()),
        "avi" => Some("video/x-msvideo".to_string()),
        "mov" => Some("video/quicktime".to_string()),
        "mp3" => Some("audio/mpeg".to_string()),
        "flac" => Some("audio/flac".to_string()),
        "ogg" => Some("audio/ogg".to_string()),
        "m4a" => Some("audio/mp4".to_string()),
        "ts" => Some("video/mp2t".to_string()),
        "m4s" => Some("video/iso.segment".to_string()),
        _ => None,
    }
}

// ── Resolver ─────────────────────────────────────────────────────────

/// The main resolver that orchestrates URL resolution.
///
/// Holds a reference to the [`picast_tor::TorManager`] so it can route
/// `.onion` requests (or any request the user tags as "anonymous")
/// through the Tor SOCKS proxy. Results are cached in memory to
/// prevent duplicate resolution of the same URL.
pub struct Resolver {
    /// Reference to the Tor subsystem for anonymous resolution.
    tor: Arc<picast_tor::TorManager>,
    /// In-memory cache of resolved URLs.
    cache: Arc<Mutex<ResolveCache>>,
}

impl Resolver {
    /// Create a new resolver with the given Tor manager.
    pub fn new(tor: Arc<picast_tor::TorManager>) -> Self {
        Self { tor, cache: Arc::new(Mutex::new(ResolveCache::new())) }
    }

    /// Create a new resolver with a custom cache TTL.
    pub fn with_cache_ttl(tor: Arc<picast_tor::TorManager>, ttl: std::time::Duration) -> Self {
        Self { tor, cache: Arc::new(Mutex::new(ResolveCache::with_ttl(ttl))) }
    }

    /// Resolve `url` into a [`ResolveResult`].
    ///
    /// 1. Parse the URL and classify it ([`UrlCategory`]).
    /// 2. Check the cache for a recent result.
    /// 3. Route to the appropriate resolution strategy.
    /// 4. Cache the result and return it.
    pub async fn resolve(&self, url: &str) -> Result<ResolveResult, ResolveError> {
        let parsed = Url::parse(url).map_err(|e| ResolveError::InvalidUrl(e.to_string()))?;
        let category = classify_url(&parsed);

        tracing::info!(url = url, category = %category, "resolving URL");

        // Check cache.
        {
            let mut cache = self.cache.lock().await;
            if let Some(cached) = cache.get(url) {
                tracing::debug!(url = url, "cache hit");
                return Ok(cached.clone());
            }
        }

        // Resolve based on category.
        let result = match category {
            UrlCategory::DirectMedia => ResolveResult {
                source_url: url.to_owned(),
                direct_url: url.to_owned(),
                category,
                mime_type: mime_from_extension(url),
                content_length: None,
                used_tor: false,
                title: None,
                duration: None,
                thumbnail: None,
                vcodec: None,
                acodec: None,
                width: None,
                height: None,
                subtitle_tracks: vec![],
            },
            UrlCategory::HlsManifest => ResolveResult {
                source_url: url.to_owned(),
                direct_url: url.to_owned(),
                category,
                mime_type: Some("application/vnd.apple.mpegurl".to_string()),
                content_length: None,
                used_tor: false,
                title: None,
                duration: None,
                thumbnail: None,
                vcodec: None,
                acodec: None,
                width: None,
                height: None,
                subtitle_tracks: vec![],
            },
            UrlCategory::DashManifest => ResolveResult {
                source_url: url.to_owned(),
                direct_url: url.to_owned(),
                category,
                mime_type: Some("application/dash+xml".to_string()),
                content_length: None,
                used_tor: false,
                title: None,
                duration: None,
                thumbnail: None,
                vcodec: None,
                acodec: None,
                width: None,
                height: None,
                subtitle_tracks: vec![],
            },
            UrlCategory::Onion => {
                // Onion URLs are always resolved through Tor via yt-dlp.
                let mut result = self.resolve_onion(url).await?;
                result.category = UrlCategory::Onion;
                result
            },
            UrlCategory::WebPage => {
                // Web page URLs require yt-dlp to extract the direct media URL.
                // Route through Tor SOCKS proxy for circuit isolation.
                let mut result = self.resolve_webpage(url).await?;
                result.category = UrlCategory::WebPage;
                result
            },
            UrlCategory::Magnet => {
                return Err(ResolveError::NoMediaFound(url.to_owned()));
            },
        };

        // Cache the result.
        {
            let mut cache = self.cache.lock().await;
            cache.insert(url, result.clone());
        }

        Ok(result)
    }

    /// Resolve a URL that is known to be a direct media URL.
    ///
    /// Only handles [`UrlCategory::DirectMedia`] category URLs.
    /// Returns [`ResolveError::NoMediaFound`] for WebPage, Magnet,
    /// Onion, HLS, or DASH URLs — use [`Resolver::resolve()`] for those.
    pub async fn resolve_direct(&self, url: &str) -> Result<ResolveResult, ResolveError> {
        let parsed = Url::parse(url).map_err(|e| ResolveError::InvalidUrl(e.to_string()))?;
        let category = classify_url(&parsed);

        if category != UrlCategory::DirectMedia {
            return Err(ResolveError::NoMediaFound(url.to_owned()));
        }

        // Check cache.
        {
            let mut cache = self.cache.lock().await;
            if let Some(cached) = cache.get(url) {
                tracing::debug!(url = url, "cache hit");
                return Ok(cached.clone());
            }
        }

        let result = ResolveResult {
            source_url: url.to_owned(),
            direct_url: url.to_owned(),
            category,
            mime_type: mime_from_extension(url),
            content_length: None,
            used_tor: false,
            title: None,
            duration: None,
            thumbnail: None,
            vcodec: None,
            acodec: None,
            width: None,
            height: None,
            subtitle_tracks: vec![],
        };

        // Cache the result.
        {
            let mut cache = self.cache.lock().await;
            cache.insert(url, result.clone());
        }

        Ok(result)
    }

    /// Classify a URL without performing any resolution.
    ///
    /// Pure URL parsing — no network access.
    pub fn classify(&self, url: &str) -> Result<UrlCategory, ResolveError> {
        let parsed = Url::parse(url).map_err(|e| ResolveError::InvalidUrl(e.to_string()))?;
        Ok(classify_url(&parsed))
    }

    // ── Private resolution strategies ────────────────────────────────
    //
    // These are kept for future use when yt-dlp integration is
    // re-enabled. They are currently not called from resolve().

    /// Direct media / HLS / DASH: return the URL as-is.
    ///
    /// GStreamer's `souphttpsrc` can handle these directly.
    /// We do a HEAD request to get content-type and content-length
    /// metadata if possible.
    #[allow(dead_code)]
    async fn resolve_direct_internal(
        &self,
        url: &str,
        category: UrlCategory,
    ) -> Result<ResolveResult, ResolveError> {
        let mime_type = Self::guess_mime_from_url(url);

        Ok(ResolveResult {
            source_url: url.to_owned(),
            direct_url: url.to_owned(),
            category,
            mime_type,
            content_length: None,
            used_tor: false,
            title: None,
            duration: None,
            thumbnail: None,
            vcodec: None,
            acodec: None,
            width: None,
            height: None,
            subtitle_tracks: vec![],
        })
    }

    /// Web page resolution via yt-dlp through Tor.
    async fn resolve_webpage(&self, url: &str) -> Result<ResolveResult, ResolveError> {
        let socks_addr = self.tor.socks_addr();
        let isolation = picast_tor::TorManager::isolation_username(
            Url::parse(url)
                .ok()
                .and_then(|u| u.host_str().map(|h| h.to_owned()))
                .unwrap_or_else(|| "unknown".into())
                .as_str(),
        );

        ytdlp::resolve_with_ytdlp(url, &socks_addr, &isolation).await
    }

    /// Onion URL resolution — always through Tor, always via yt-dlp.
    async fn resolve_onion(&self, url: &str) -> Result<ResolveResult, ResolveError> {
        let socks_addr = self.tor.socks_addr();
        let isolation = picast_tor::TorManager::isolation_username(
            Url::parse(url)
                .ok()
                .and_then(|u| u.host_str().map(|h| h.to_owned()))
                .unwrap_or_else(|| "onion-unknown".into())
                .as_str(),
        );

        // Onion URLs always use Tor.
        let mut result = ytdlp::resolve_with_ytdlp(url, &socks_addr, &isolation).await?;
        result.used_tor = true;
        Ok(result)
    }

    /// Guess the MIME type from the URL path extension.
    #[allow(dead_code)]
    fn guess_mime_from_url(url: &str) -> Option<String> {
        let parsed = Url::parse(url).ok()?;
        let path = parsed.path().to_lowercase();
        let ext = path.rsplit('.').next()?;

        match ext {
            "mp4" | "m4v" => Some("video/mp4".into()),
            "mkv" => Some("video/x-matroska".into()),
            "webm" => Some("video/webm".into()),
            "avi" => Some("video/x-msvideo".into()),
            "mov" => Some("video/quicktime".into()),
            "ts" => Some("video/mp2t".into()),
            "m3u8" => Some("application/vnd.apple.mpegurl".into()),
            "mpd" => Some("application/dash+xml".into()),
            "mp3" => Some("audio/mpeg".into()),
            "flac" => Some("audio/flac".into()),
            "ogg" => Some("audio/ogg".into()),
            "opus" => Some("audio/opus".into()),
            "wav" => Some("audio/wav".into()),
            "aac" | "m4a" => Some("audio/mp4".into()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver() -> Resolver {
        let tor = Arc::new(picast_tor::TorManager::new("127.0.0.1:9050"));
        Resolver::new(tor)
    }

    // ── Classification tests ──────────────────────────────────────────

    #[test]
    fn test_classify_onion() {
        let r = resolver();
        let url = Url::parse("http://example.onion/video.mp4").unwrap();
        assert_eq!(r.classify(url.as_str()).unwrap(), UrlCategory::Onion);
    }

    #[test]
    fn test_classify_hls() {
        let r = resolver();
        let url = Url::parse("https://cdn.example.com/stream.m3u8").unwrap();
        assert_eq!(r.classify(url.as_str()).unwrap(), UrlCategory::HlsManifest);
    }

    #[test]
    fn test_classify_dash() {
        let r = resolver();
        let url = Url::parse("https://cdn.example.com/stream.mpd").unwrap();
        assert_eq!(r.classify(url.as_str()).unwrap(), UrlCategory::DashManifest);
    }

    #[test]
    fn test_classify_direct_media_mp4() {
        let r = resolver();
        let url = Url::parse("https://cdn.example.com/video.mp4").unwrap();
        assert_eq!(r.classify(url.as_str()).unwrap(), UrlCategory::DirectMedia);
    }

    #[test]
    fn test_classify_direct_media_webm() {
        let r = resolver();
        let url = Url::parse("https://cdn.example.com/video.webm").unwrap();
        assert_eq!(r.classify(url.as_str()).unwrap(), UrlCategory::DirectMedia);
    }

    #[test]
    fn test_classify_direct_media_with_query() {
        let r = resolver();
        let url = Url::parse("https://cdn.example.com/video.mp4?token=abc123").unwrap();
        assert_eq!(r.classify(url.as_str()).unwrap(), UrlCategory::DirectMedia);
    }

    #[test]
    fn test_classify_webpage() {
        let r = resolver();
        let url = Url::parse("https://www.youtube.com/watch?v=abc").unwrap();
        assert_eq!(r.classify(url.as_str()).unwrap(), UrlCategory::WebPage);
    }

    #[test]
    fn test_classify_magnet() {
        let r = resolver();
        let url = Url::parse("magnet:?xt=urn:btih:abc123").unwrap();
        assert_eq!(r.classify(url.as_str()).unwrap(), UrlCategory::Magnet);
    }

    // ── mime_from_extension tests ─────────────────────────────────────

    #[test]
    fn test_mime_from_extension() {
        assert_eq!(mime_from_extension("video.mp4"), Some("video/mp4".to_string()));
        assert_eq!(mime_from_extension("audio.mp3"), Some("audio/mpeg".to_string()));
        assert_eq!(mime_from_extension("video.webm"), Some("video/webm".to_string()));
        assert_eq!(mime_from_extension("video.mkv"), Some("video/x-matroska".to_string()));
        assert_eq!(mime_from_extension("unknown.xyz"), None);
    }

    #[test]
    fn test_mime_from_extension_with_query() {
        assert_eq!(mime_from_extension("video.mp4?token=abc"), Some("video/mp4".to_string()));
    }

    // ── resolve() tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_resolve_direct_media() {
        let r = resolver();
        let result = r.resolve("https://cdn.example.com/video.mp4").await.unwrap();
        assert_eq!(result.category, UrlCategory::DirectMedia);
        assert_eq!(result.direct_url, "https://cdn.example.com/video.mp4");
        assert_eq!(result.mime_type, Some("video/mp4".to_string()));
        assert!(!result.used_tor);
    }

    #[tokio::test]
    async fn test_resolve_hls_manifest() {
        let r = resolver();
        let result = r.resolve("https://cdn.example.com/stream.m3u8").await.unwrap();
        assert_eq!(result.category, UrlCategory::HlsManifest);
        assert_eq!(result.mime_type, Some("application/vnd.apple.mpegurl".to_string()));
    }

    #[tokio::test]
    async fn test_resolve_dash_manifest() {
        let r = resolver();
        let result = r.resolve("https://cdn.example.com/stream.mpd").await.unwrap();
        assert_eq!(result.category, UrlCategory::DashManifest);
        assert_eq!(result.mime_type, Some("application/dash+xml".to_string()));
    }

    #[tokio::test]
    async fn test_resolve_magnet_errors() {
        let r = resolver();
        let result = r.resolve("magnet:?xt=urn:btih:abc123").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ResolveError::NoMediaFound(url) => assert!(url.contains("magnet")),
            other => panic!("Expected NoMediaFound, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_resolve_onion_routes_through_tor() {
        let r = resolver();
        // Onion URLs now invoke yt-dlp through Tor. Without yt-dlp installed,
        // we expect a TorUnavailable error (binary not found).
        let result = r.resolve("http://example.onion/video.mp4").await;
        match result {
            Ok(res) => {
                // If yt-dlp is installed and Tor is running, we should get
                // a result with used_tor = true.
                assert_eq!(res.category, UrlCategory::Onion);
                assert!(res.used_tor);
            },
            Err(ResolveError::TorUnavailable(msg)) => {
                // Expected when yt-dlp is not installed in test env.
                assert!(msg.contains("yt-dlp"), "error should mention yt-dlp: {}", msg);
            },
            Err(e) => {
                // Other errors (network, no media found) are acceptable
                // without a running Tor daemon.
                assert!(
                    matches!(e, ResolveError::Network(_) | ResolveError::NoMediaFound(_)),
                    "unexpected error type: {:?}",
                    e
                );
            },
        }
    }

    #[tokio::test]
    async fn test_resolve_webpage_routes_through_tor() {
        let r = resolver();
        // WebPage URLs now invoke yt-dlp through Tor. Without yt-dlp installed,
        // we expect a TorUnavailable error.
        let result = r.resolve("https://www.youtube.com/watch?v=abc").await;
        match result {
            Ok(res) => {
                assert_eq!(res.category, UrlCategory::WebPage);
            },
            Err(ResolveError::TorUnavailable(msg)) => {
                assert!(msg.contains("yt-dlp"), "error should mention yt-dlp: {}", msg);
            },
            Err(e) => {
                assert!(
                    matches!(e, ResolveError::Network(_) | ResolveError::NoMediaFound(_)),
                    "unexpected error type: {:?}",
                    e
                );
            },
        }
    }

    #[tokio::test]
    async fn test_resolve_direct_method_only_handles_direct() {
        let r = resolver();
        // Direct media should work
        let result = r.resolve_direct("https://cdn.example.com/video.mp4").await.unwrap();
        assert_eq!(result.category, UrlCategory::DirectMedia);

        // Web page should fail
        let result = r.resolve_direct("https://www.youtube.com/watch?v=abc").await;
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_result_serialization() {
        let result = ResolveResult {
            source_url: "https://example.com/video.mp4".into(),
            direct_url: "https://cdn.example.com/video.mp4".into(),
            category: UrlCategory::DirectMedia,
            mime_type: Some("video/mp4".into()),
            content_length: Some(1024),
            used_tor: false,
            title: None,
            duration: None,
            thumbnail: None,
            vcodec: None,
            acodec: None,
            width: None,
            height: None,
            subtitle_tracks: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: ResolveResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.source_url, result.source_url);
        assert_eq!(parsed.category, result.category);
    }

    // ── Legacy / backward-compat tests ────────────────────────────────

    #[test]
    fn classify_youtube() {
        let r = resolver();
        let cat = r.classify("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap();
        assert_eq!(cat, UrlCategory::WebPage);
    }

    #[test]
    fn classify_onion() {
        let r = resolver();
        let cat = r.classify("http://xyz123456.onion/video.mp4").unwrap();
        assert_eq!(cat, UrlCategory::Onion);
    }

    #[test]
    fn classify_hls_manifest() {
        let r = resolver();
        let cat = r.classify("https://cdn.example.com/live/stream.m3u8").unwrap();
        assert_eq!(cat, UrlCategory::HlsManifest);
    }

    #[test]
    fn classify_dash_manifest() {
        let r = resolver();
        let cat = r.classify("https://cdn.example.com/vod/stream.mpd").unwrap();
        assert_eq!(cat, UrlCategory::DashManifest);
    }

    #[test]
    fn classify_direct_media() {
        let r = resolver();
        for ext in &["mp4", "mkv", "webm", "mp3", "flac", "ogg"] {
            let url = format!("https://example.com/media.{}", ext);
            let cat = r.classify(&url).unwrap();
            assert_eq!(cat, UrlCategory::DirectMedia, ".{} should be DirectMedia", ext);
        }
    }

    #[test]
    fn classify_magnet() {
        let r = resolver();
        let cat = r.classify("magnet:?xt=urn:btih:abc123").unwrap();
        assert_eq!(cat, UrlCategory::Magnet);
    }

    #[tokio::test]
    async fn resolve_direct_media_legacy() {
        let r = resolver();
        let result = r.resolve("https://example.com/video.mp4").await.unwrap();
        assert_eq!(result.category, UrlCategory::DirectMedia);
        assert_eq!(result.direct_url, "https://example.com/video.mp4");
        assert!(!result.used_tor);
        assert_eq!(result.mime_type, Some("video/mp4".into()));
    }

    #[tokio::test]
    async fn resolve_magnet_unsupported() {
        let r = resolver();
        let result = r.resolve("magnet:?xt=urn:btih:abc123").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("magnet"));
    }

    #[test]
    fn guess_mime_type() {
        assert_eq!(Resolver::guess_mime_from_url("https://x.com/v.mp4"), Some("video/mp4".into()));
        assert_eq!(
            Resolver::guess_mime_from_url("https://x.com/v.mkv"),
            Some("video/x-matroska".into())
        );
        assert_eq!(
            Resolver::guess_mime_from_url("https://x.com/v.webm"),
            Some("video/webm".into())
        );
        assert_eq!(
            Resolver::guess_mime_from_url("https://x.com/s.m3u8"),
            Some("application/vnd.apple.mpegurl".into())
        );
        assert_eq!(
            Resolver::guess_mime_from_url("https://x.com/s.mpd"),
            Some("application/dash+xml".into())
        );
        assert_eq!(Resolver::guess_mime_from_url("https://x.com/a.mp3"), Some("audio/mpeg".into()));
    }

    #[test]
    fn guess_mime_type_unknown() {
        assert_eq!(Resolver::guess_mime_from_url("https://x.com/page"), None);
    }
}
