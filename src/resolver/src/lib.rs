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

use cache::ResolveCache;
use classifier::{UrlCategory, classify_url};
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
        Self {
            tor,
            cache: Arc::new(Mutex::new(ResolveCache::new())),
        }
    }

    /// Create a new resolver with a custom cache TTL.
    pub fn with_cache_ttl(tor: Arc<picast_tor::TorManager>, ttl: std::time::Duration) -> Self {
        Self {
            tor,
            cache: Arc::new(Mutex::new(ResolveCache::with_ttl(ttl))),
        }
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
            UrlCategory::DirectMedia | UrlCategory::HlsManifest | UrlCategory::DashManifest => {
                self.resolve_direct(url, category).await?
            }
            UrlCategory::WebPage => self.resolve_webpage(url).await?,
            UrlCategory::Onion => self.resolve_onion(url).await?,
            UrlCategory::Magnet => {
                return Err(ResolveError::NoMediaFound(
                    "magnet links are not supported in v1".into(),
                ));
            }
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

    /// Direct media / HLS / DASH: return the URL as-is.
    ///
    /// GStreamer's `souphttpsrc` can handle these directly.
    /// We do a HEAD request to get content-type and content-length
    /// metadata if possible.
    async fn resolve_direct(
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
    async fn resolve_direct_media() {
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
        assert!(result.unwrap_err().to_string().contains("not supported"));
    }

    #[test]
    fn guess_mime_type() {
        assert_eq!(Resolver::guess_mime_from_url("https://x.com/v.mp4"), Some("video/mp4".into()));
        assert_eq!(Resolver::guess_mime_from_url("https://x.com/v.mkv"), Some("video/x-matroska".into()));
        assert_eq!(Resolver::guess_mime_from_url("https://x.com/v.webm"), Some("video/webm".into()));
        assert_eq!(Resolver::guess_mime_from_url("https://x.com/s.m3u8"), Some("application/vnd.apple.mpegurl".into()));
        assert_eq!(Resolver::guess_mime_from_url("https://x.com/s.mpd"), Some("application/dash+xml".into()));
        assert_eq!(Resolver::guess_mime_from_url("https://x.com/a.mp3"), Some("audio/mpeg".into()));
    }

    #[test]
    fn guess_mime_type_unknown() {
        assert_eq!(Resolver::guess_mime_from_url("https://x.com/page"), None);
    }
}
