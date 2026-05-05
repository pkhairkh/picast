//! PiCast URL Resolver
//!
//! Takes a user-supplied URL and resolves it to a direct, playable
//! media URL. The resolver can:
//!
//! - Follow HTTP redirects (including JavaScript-based redirect pages).
//! - Extract embedded video/audio stream URLs from web pages.
//! - Route requests through the Tor SOCKS proxy when the target is on
//!   an `.onion` address or when the user explicitly requests anonymity.
//!
//! The output of a successful resolution is a [`ResolveResult`] that
//! carries the direct URL plus metadata useful for the playback engine.

use serde::{Deserialize, Serialize};
use thiserror::Error;
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

    /// A network request failed (DNS, TCP, TLS, timeout, …).
    #[error("network error: {0}")]
    Network(String),

    /// The Tor proxy was required but not available.
    #[error("Tor proxy unavailable: {0}")]
    TorUnavailable(String),
}

// ── URL Category ─────────────────────────────────────────────────────

/// High-level classification of the resolved URL.
///
/// Helps downstream components choose the right handling strategy
/// (e.g. HLS needs adaptive bitrate logic; a plain file can be
/// streamed directly).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UrlCategory {
    /// Direct link to a media file (mp4, mkv, mp3, …).
    DirectMedia,
    /// HTTP Live Streaming manifest (.m3u8).
    HlsManifest,
    /// MPEG-DASH manifest (.mpd).
    DashManifest,
    /// A web page that embeds media (YouTube, Vimeo, …).
    WebPage,
    /// A magnet / torrent link (not yet supported).
    Magnet,
    /// An `.onion` address that must be fetched over Tor.
    Onion,
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
}

// ── Resolver ─────────────────────────────────────────────────────────

/// The main resolver that orchestrates URL resolution.
///
/// Holds a reference to the [`picast_tor::TorManager`] so it can route
/// `.onion` requests (or any request the user tags as "anonymous")
/// through the Tor SOCKS proxy.
pub struct Resolver {
    /// Reference to the Tor subsystem for anonymous resolution.
    _tor: std::sync::Arc<()>, // TODO: Arc<picast_tor::TorManager>
}

impl Resolver {
    /// Create a new resolver.
    ///
    /// `tor` is an `Arc` to the [`picast_tor::TorManager`] used for
    /// `.onion` and privacy-routed requests.
    pub fn new(_tor: std::sync::Arc<()>) -> Self {
        Self { _tor }
    }

    /// Resolve `url` into a [`ResolveResult`].
    ///
    /// 1. Parse the URL and classify it ([`UrlCategory`]).
    /// 2. If `.onion`, route through Tor.
    /// 3. Follow redirects and extract the direct media URL.
    pub async fn resolve(&self, url: &str) -> Result<ResolveResult, ResolveError> {
        let parsed = Url::parse(url).map_err(|e| ResolveError::InvalidUrl(e.to_string()))?;

        let category = self.classify(&parsed);

        // TODO: actual network resolution logic
        Ok(ResolveResult {
            source_url: url.to_owned(),
            direct_url: url.to_owned(),
            category,
            mime_type: None,
            content_length: None,
            used_tor: category == UrlCategory::Onion,
        })
    }

    /// Classify a parsed URL into a [`UrlCategory`].
    fn classify(&self, url: &Url) -> UrlCategory {
        match url.host_str() {
            Some(h) if h.ends_with(".onion") => UrlCategory::Onion,
            _ => match url.path().rsplit('.').next() {
                Some("m3u8") => UrlCategory::HlsManifest,
                Some("mpd") => UrlCategory::DashManifest,
                Some("mp4" | "mkv" | "webm" | "mp3" | "flac" | "ogg") => UrlCategory::DirectMedia,
                Some("magnet") => UrlCategory::Magnet,
                _ => UrlCategory::WebPage,
            },
        }
    }
}
