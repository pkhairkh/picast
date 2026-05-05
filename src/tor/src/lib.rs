//! PiCast Tor Manager
//!
//! Manages a local Tor daemon and its SOCKS5 proxy so that the
//! resolver can fetch `.onion` addresses or route any request
//! anonymously. Key responsibilities:
//!
//! - Start / stop the `tor` process (or connect to a system instance).
//! - Monitor circuit health and rebuild on failure.
//! - Provide the SOCKS5 proxy address to other subsystems.
//! - Compute v3 onion service identifiers for local service
//!   advertisement (using `md-5` for non-crypto hash helpers).

use thiserror::Error;

// ── Errors ───────────────────────────────────────────────────────────

/// Errors that can occur while managing the Tor daemon.
#[derive(Error, Debug)]
pub enum TorError {
    /// The Tor binary could not be found on `$PATH`.
    #[error("tor binary not found")]
    BinaryNotFound,

    /// The Tor process exited unexpectedly.
    #[error("tor process exited with code {0}")]
    ProcessExited(i32),

    /// The SOCKS proxy did not become available in time.
    #[error("SOCKS proxy timeout after {0}ms")]
    SocksTimeout(u64),

    /// A health check against the proxy failed.
    #[error("health check failed: {0}")]
    HealthCheck(String),

    /// An I/O error occurred.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

// ── SOCKS Proxy ──────────────────────────────────────────────────────

/// Configuration for the Tor SOCKS5 proxy endpoint.
#[derive(Debug, Clone)]
pub struct SocksProxy {
    /// IP address the proxy listens on (typically `127.0.0.1`).
    pub host: String,
    /// Port the proxy listens on (default `9050`).
    pub port: u16,
    /// Whether the proxy requires authentication.
    pub requires_auth: bool,
}

impl SocksProxy {
    /// Create a proxy config listening on `host:port`.
    pub fn new(host: &str, port: u16) -> Self {
        Self {
            host: host.to_owned(),
            port,
            requires_auth: false,
        }
    }

    /// Return the full address string (e.g. `127.0.0.1:9050`).
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

impl Default for SocksProxy {
    fn default() -> Self {
        Self::new("127.0.0.1", 9050)
    }
}

// ── Circuit Health ───────────────────────────────────────────────────

/// Health metrics for the Tor circuit used by the SOCKS proxy.
///
/// Monitored periodically to detect degraded or dead circuits so
/// they can be rebuilt before causing user-visible failures.
#[derive(Debug, Clone, Copy)]
pub struct CircuitHealth {
    /// Number of currently open circuits.
    pub open_circuits: u32,
    /// Number of circuits in `BUILT` state (fully established).
    pub built_circuits: u32,
    /// Number of circuits in `FAILED` or `CLOSED` state.
    pub failed_circuits: u32,
    /// Round-trip latency through the proxy in milliseconds.
    pub latency_ms: Option<u64>,
    /// Whether the overall health is considered "good".
    pub is_healthy: bool,
}

impl Default for CircuitHealth {
    fn default() -> Self {
        Self {
            open_circuits: 0,
            built_circuits: 0,
            failed_circuits: 0,
            latency_ms: None,
            is_healthy: true,
        }
    }
}

// ── Tor Manager ──────────────────────────────────────────────────────

/// Manages the Tor daemon lifecycle and exposes the SOCKS proxy config.
///
/// On start-up the manager will:
//!
//! 1. Look for an already-running Tor on the configured SOCKS port.
//! 2. If none is found, spawn `tor` as a child process.
//! 3. Wait for the SOCKS port to become reachable (with a timeout).
//! 4. Begin periodic circuit-health monitoring.
pub struct TorManager {
    /// SOCKS proxy configuration.
    socks: SocksProxy,
    /// Whether we spawned Tor ourselves (and should kill it on drop).
    owns_process: bool,
}

impl TorManager {
    /// Create a new manager targeting the given SOCKS proxy address.
    ///
    /// The `socks_addr` string should be in `"host:port"` format.
    pub fn new(socks_addr: &str) -> Self {
        let parts: Vec<&str> = socks_addr.split(':').collect();
        let (host, port) = if parts.len() == 2 {
            (
                parts[0].to_owned(),
                parts[1].parse::<u16>().unwrap_or(9050),
            )
        } else {
            ("127.0.0.1".to_owned(), 9050)
        };

        Self {
            socks: SocksProxy::new(&host, port),
            owns_process: false,
        }
    }

    /// Ensure Tor is running and the SOCKS proxy is reachable.
    ///
    /// If Tor is not already running on the expected port, this will
    /// attempt to spawn a new `tor` process and wait up to
    /// `startup_timeout_ms` for the proxy to become available.
    pub async fn ensure_running(&mut self, startup_timeout_ms: u64) -> Result<(), TorError> {
        // TODO: check if proxy already reachable, else spawn tor
        tracing::info!(addr = %self.socks.addr(), "ensuring Tor is running");
        let _ = startup_timeout_ms;
        Ok(())
    }

    /// Return a reference to the SOCKS proxy configuration.
    pub fn socks(&self) -> &SocksProxy {
        &self.socks
    }

    /// Return the SOCKS5 proxy address string.
    pub fn socks_addr(&self) -> &str {
        // Return a &str — we format lazily into a field for stable ref
        // For now, just return the host portion; the caller should use
        // self.socks.addr() for the full address.
        &self.socks.host
    }

    /// Perform a health check against the SOCKS proxy.
    pub async fn health_check(&self) -> Result<CircuitHealth, TorError> {
        // TODO: connect to SOCKS proxy and measure latency
        Ok(CircuitHealth::default())
    }

    /// Shut down the managed Tor process (if we own it).
    pub async fn shutdown(&self) -> Result<(), TorError> {
        if self.owns_process {
            tracing::info!("shutting down managed Tor process");
            // TODO: send SIGTERM to child
        }
        Ok(())
    }
}

impl Drop for TorManager {
    fn drop(&mut self) {
        if self.owns_process {
            tracing::warn!("TorManager dropped while still owning the Tor process – attempting cleanup");
            // Best-effort: the async shutdown won't run here, so we
            // try a synchronous kill. A real implementation would use
            // `tokio::process::Child::kill()`.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socks_proxy_default() {
        let proxy = SocksProxy::default();
        assert_eq!(proxy.host, "127.0.0.1");
        assert_eq!(proxy.port, 9050);
        assert!(!proxy.requires_auth);
    }

    #[test]
    fn socks_proxy_addr_format() {
        let proxy = SocksProxy::new("192.168.1.1", 9150);
        assert_eq!(proxy.addr(), "192.168.1.1:9150");

        let default = SocksProxy::default();
        assert_eq!(default.addr(), "127.0.0.1:9050");
    }

    #[test]
    fn circuit_health_default() {
        let health = CircuitHealth::default();
        assert_eq!(health.open_circuits, 0);
        assert_eq!(health.built_circuits, 0);
        assert_eq!(health.failed_circuits, 0);
        assert!(health.latency_ms.is_none());
        assert!(health.is_healthy, "default circuit health should be healthy");
    }

    #[test]
    fn tor_manager_new_parses_addr() {
        let mgr = TorManager::new("127.0.0.1:9050");
        assert_eq!(mgr.socks().host, "127.0.0.1");
        assert_eq!(mgr.socks().port, 9050);
    }

    #[test]
    fn tor_manager_new_invalid_addr_falls_back() {
        let mgr = TorManager::new("garbage");
        // Parsing the port should fail, falling back to 9050
        assert_eq!(mgr.socks().port, 9050);
    }

    #[test]
    fn tor_manager_new_no_port_falls_back() {
        let mgr = TorManager::new("127.0.0.1");
        // No colon means parts.len() != 2, so defaults kick in
        assert_eq!(mgr.socks().host, "127.0.0.1");
        assert_eq!(mgr.socks().port, 9050);
    }
}
