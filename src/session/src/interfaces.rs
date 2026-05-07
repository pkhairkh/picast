//! PiCast Subsystem Trait Interfaces
//!
//! These traits define the contracts that each subsystem must implement.
//! The session manager depends on the **traits** rather than concrete
//! types so that subsystems can be mocked in tests or swapped out
//! without touching the session layer.
//!
//! ## Design Rationale
//!
//! Using trait objects (`Arc<dyn Trait>`) allows:
//! - Easy mocking for unit tests
//! - Swapping implementations without changing session code
//! - Clean dependency inversion (session doesn't know about GStreamer, DRM, etc.)
//!
//! ## Thread Safety
//!
//! All traits require `Send + Sync` so they can be wrapped in `Arc`
//! and shared across tokio tasks.

use async_trait::async_trait;

// ── Resolver ─────────────────────────────────────────────────────────

/// Metadata returned by the resolver alongside the direct URL.
#[derive(Debug, Clone)]
pub struct ResolveInfo {
    /// The direct, playable media URL.
    pub direct_url: String,
    /// Media title (e.g. from yt-dlp), if available.
    pub title: Option<String>,
    /// Duration in milliseconds, if known.
    pub duration_ms: Option<u64>,
}

/// Resolves a user-supplied URL into a direct, playable media URL.
///
/// Implementations may follow HTTP redirects, extract stream URLs from
/// web pages, or route traffic through the Tor network.
#[async_trait]
pub trait ResolverTrait: Send + Sync {
    /// Attempt to resolve `url` into a direct media URL with metadata.
    ///
    /// Returns `Ok(ResolveInfo)` on success or an error describing why
    /// resolution failed.
    async fn resolve(
        &self,
        url: &str,
    ) -> Result<ResolveInfo, Box<dyn std::error::Error + Send + Sync>>;
}

// ── Playback ─────────────────────────────────────────────────────────

/// Controls the GStreamer-based media playback pipeline.
#[async_trait]
pub trait PlaybackTrait: Send + Sync {
    /// Begin playback of `url` through the given SOCKS5 proxy.
    ///
    /// - `url`: Direct media URL (CDN URL) to stream.
    /// - `source_url`: Original page URL the user cast. Used for the
    ///   Referer header — CDNs like Voe require it to match the
    ///   originating site's domain, not the CDN's domain.
    /// - `socks_addr`: Tor SOCKS5 proxy address.
    /// - `isolation_username`: SOCKS5 username for circuit isolation.
    async fn play(
        &self,
        url: &str,
        source_url: &str,
        socks_addr: &str,
        isolation_username: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Pause the running pipeline.
    async fn pause(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Resume a paused pipeline.
    async fn resume(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Stop and tear down the pipeline.
    async fn stop(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Seek to `position_ms` milliseconds from the beginning.
    async fn seek(&self, position_ms: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Set volume to a value between 0.0 and 1.0.
    async fn set_volume(&self, volume: f64)
        -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Return the current playback position in milliseconds.
    async fn position_ms(&self) -> Result<u64, Box<dyn std::error::Error + Send + Sync>>;

    /// Return the duration in milliseconds.
    async fn duration_ms(&self) -> Result<Option<u64>, Box<dyn std::error::Error + Send + Sync>>;

    /// Set the ALSA audio device for the next pipeline (e.g. "plughw:1,0").
    /// Takes effect on the next play() call; does not affect a running pipeline.
    async fn set_audio_device(
        &self,
        device: String,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Get the current ALSA audio device string.
    async fn audio_device(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>>;
}

// ── Display ──────────────────────────────────────────────────────────

/// Manages the DRM/KMS display plane that the video sink renders into.
#[async_trait]
pub trait DisplayTrait: Send + Sync {
    /// Acquire the primary DRM plane and configure it for video output.
    async fn acquire(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Release the DRM plane back to the OS.
    async fn release(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Query the current display resolution as `(width, height)`.
    async fn resolution(&self) -> Result<(u32, u32), Box<dyn std::error::Error + Send + Sync>>;
}

// ── Tor ──────────────────────────────────────────────────────────────

/// Controls the Tor SOCKS proxy used for anonymous URL resolution.
#[async_trait]
pub trait TorTrait: Send + Sync {
    /// Ensure the Tor daemon is running and the SOCKS proxy is reachable.
    async fn ensure_running(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Return the SOCKS5 proxy address (e.g. `127.0.0.1:9050`).
    fn socks_addr(&self) -> String;

    /// Check whether the proxy is currently responsive.
    async fn health_check(&self) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>;

    /// Compute the per-hostname SOCKS5 isolation username.
    fn isolation_username(&self, hostname: &str) -> String;
}
