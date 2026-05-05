//! PiCast Subsystem Trait Interfaces
//!
//! These traits define the contracts that each subsystem must implement.
//! The session manager depends on the **traits** rather than concrete
//! types so that subsystems can be mocked in tests or swapped out
//! without touching the session layer.

use async_trait::async_trait;

// ── Resolver ─────────────────────────────────────────────────────────

/// Resolves a user-supplied URL into a direct, playable media URL.
///
/// Implementations may follow HTTP redirects, extract stream URLs from
/// web pages, or route traffic through the Tor network.
#[async_trait]
pub trait ResolverTrait: Send + Sync {
    /// Attempt to resolve `url` into a direct media URL.
    ///
    /// Returns `Ok(direct_url)` on success or an error describing why
    /// resolution failed.
    async fn resolve(&self, url: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>>;
}

// ── Playback ─────────────────────────────────────────────────────────

/// Controls the GStreamer-based media playback pipeline.
#[async_trait]
pub trait PlaybackTrait: Send + Sync {
    /// Begin playback of `url`.
    async fn play(&self, url: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Pause the running pipeline.
    async fn pause(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Resume a paused pipeline.
    async fn resume(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Stop and tear down the pipeline.
    async fn stop(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Seek to `position_ms` milliseconds from the beginning.
    async fn seek(&self, position_ms: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Set volume to a value between 0.0 and 1.0.
    async fn set_volume(&self, volume: f64) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Return the current playback position in milliseconds.
    async fn position_ms(&self) -> Result<u64, Box<dyn std::error::Error + Send + Sync>>;
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
    fn socks_addr(&self) -> &str;

    /// Check whether the proxy is currently responsive.
    async fn health_check(&self) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>;
}
