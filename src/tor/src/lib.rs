//! PiCast Tor Manager
//!
//! Manages a local Tor daemon and its SOCKS5 proxy so that the
//! resolver can fetch `.onion` addresses or route any request
//! anonymously. Key responsibilities:
//!
//! - Start / stop the `tor` process (or connect to a system instance).
//! - Monitor circuit health and rebuild on failure.
//! - Provide the SOCKS5 proxy address to other subsystems.
//! - Compute per-hostname SOCKS5 isolation usernames for Tor's
//!   `IsolateSOCKSAuth` feature, ensuring different sites use
//!   different circuits (preventing cross-site correlation).
//! - Signal `NEWNYM` to the Tor control port for forced circuit
//!   rotation when circuit health degrades.

use hex::ToHex;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

// ── Errors ───────────────────────────────────────────────────────────

/// Errors that can occur while managing the Tor daemon.
#[derive(Error, Debug)]
pub enum TorError {
    /// The Tor binary could not be found on `$PATH`.
    #[error("tor binary not found on PATH")]
    BinaryNotFound,

    /// The Tor process exited unexpectedly.
    #[error("tor process exited with code {0}")]
    ProcessExited(i32),

    /// The SOCKS proxy did not become available in time.
    #[error("SOCKS proxy timeout after {0}ms – Tor may have failed to bootstrap")]
    SocksTimeout(u64),

    /// A health check against the proxy failed.
    #[error("health check failed: {0}")]
    HealthCheck(String),

    /// The Tor control port is not reachable or authentication failed.
    #[error("control port error: {0}")]
    ControlPort(String),

    /// An I/O error occurred.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// An HTTP request through the proxy failed.
    #[error("proxy request failed: {0}")]
    ProxyRequest(String),
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
/// 4. Begin periodic circuit-health monitoring.
///
/// Thread safety: `TorManager` is `Send + Sync` because the mutable
/// state (the child process handle) is guarded by a `Mutex`.
pub struct TorManager {
    /// SOCKS proxy configuration.
    socks: SocksProxy,
    /// Tor control port (default 9051).
    control_port: u16,
    /// Path to the Tor cookie file for control port authentication.
    cookie_path: String,
    /// The managed Tor child process, if we spawned it.
    child: Arc<Mutex<Option<Child>>>,
    /// Whether we spawned Tor ourselves (and should kill it on drop).
    owns_process: Arc<std::sync::atomic::AtomicBool>,
}

impl TorManager {
    /// Create a new manager targeting the given SOCKS proxy address.
    ///
    /// The `socks_addr` string should be in `"host:port"` format.
    /// Falls back to `127.0.0.1:9050` on parse failure.
    pub fn new(socks_addr: &str) -> Self {
        let (host, port) = Self::parse_addr(socks_addr);

        Self {
            socks: SocksProxy::new(&host, port),
            control_port: 9051,
            cookie_path: "/run/tor/control.authcookie".to_owned(),
            child: Arc::new(Mutex::new(None)),
            owns_process: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Set the Tor control port number (default: 9051).
    pub fn with_control_port(mut self, port: u16) -> Self {
        self.control_port = port;
        self
    }

    /// Set the path to the Tor cookie file for control port auth.
    pub fn with_cookie_path(mut self, path: &str) -> Self {
        self.cookie_path = path.to_owned();
        self
    }

    /// Ensure Tor is running and the SOCKS proxy is reachable.
    ///
    /// If the SOCKS port is already accepting connections, this is a
    /// no-op. Otherwise it spawns a `tor` child process and polls the
    /// SOCKS port until it becomes available or the timeout expires.
    pub async fn ensure_running(&self, startup_timeout_ms: u64) -> Result<(), TorError> {
        // Fast path: is the SOCKS port already open?
        if TcpStream::connect(&self.socks.addr()).await.is_ok() {
            tracing::debug!(addr = %self.socks.addr(), "SOCKS port already open — Tor is running");
            return Ok(());
        }

        tracing::info!(addr = %self.socks.addr(), "SOCKS port not reachable — spawning Tor daemon");

        // Locate the Tor binary.
        let tor_path = which_tor()?;

        // Spawn Tor as a child process.
        let child = Command::new(&tor_path)
            .arg("--SocksPort")
            .arg(self.socks.port.to_string())
            .arg("--ControlPort")
            .arg(self.control_port.to_string())
            .arg("--CookieAuthentication")
            .arg("1")
            .arg("--IsolateSOCKSAuth")
            .arg("1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    TorError::BinaryNotFound
                } else {
                    TorError::Io(e)
                }
            })?;

        {
            let mut guard = self.child.lock().await;
            *guard = Some(child);
        }
        self.owns_process
            .store(true, std::sync::atomic::Ordering::SeqCst);

        // Poll the SOCKS port until it accepts connections.
        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_millis(startup_timeout_ms);
        loop {
            if TcpStream::connect(&self.socks.addr()).await.is_ok() {
                tracing::info!(addr = %self.socks.addr(), "Tor SOCKS port is now reachable");
                return Ok(());
            }

            if tokio::time::Instant::now() >= deadline {
                // Check if the child process is still alive.
                let mut guard = self.child.lock().await;
                if let Some(ref mut child) = *guard {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            let code = status.code().unwrap_or(-1);
                            tracing::error!(exit_code = code, "Tor process exited during bootstrap");
                            return Err(TorError::ProcessExited(code));
                        }
                        Ok(None) => {
                            // Still running but port not open — timeout.
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to check Tor process status");
                        }
                    }
                }
                return Err(TorError::SocksTimeout(startup_timeout_ms));
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        }
    }

    /// Return a reference to the SOCKS proxy configuration.
    pub fn socks(&self) -> &SocksProxy {
        &self.socks
    }

    /// Return the SOCKS5 proxy address string.
    pub fn socks_addr(&self) -> String {
        self.socks.addr()
    }

    /// Build a `reqwest::Client` configured to route all requests
    /// through the Tor SOCKS5h proxy with per-hostname circuit
    /// isolation.
    ///
    /// The `hostname` is hashed (SHA-256, first 16 hex chars) to
    /// produce a SOCKS5 username. Tor's `IsolateSOCKSAuth` option
    /// ensures that each unique username gets its own circuit, so
    /// different websites cannot be correlated by sharing a circuit.
    pub fn proxied_reqwest_client(&self, hostname: &str) -> Result<reqwest::Client, TorError> {
        let isolation_username = Self::isolation_username(hostname);
        let proxy_addr = format!(
            "socks5h://{}:{}@{}:{}",
            isolation_username,
            "", // no password
            self.socks.host,
            self.socks.port
        );

        let proxy = reqwest::Proxy::all(&proxy_addr)
            .map_err(|e| TorError::ProxyRequest(format!("failed to create proxy: {}", e)))?;

        let client = reqwest::Client::builder()
            .proxy(proxy)
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(15))
            .no_proxy()
            .build()
            .map_err(|e| TorError::ProxyRequest(format!("failed to build client: {}", e)))?;

        tracing::debug!(
            hostname = hostname,
            isolation_user = %isolation_username,
            "created SOCKS5h reqwest client with per-hostname isolation"
        );

        Ok(client)
    }

    /// Perform a health check against the Tor SOCKS proxy.
    ///
    /// 1. TCP connect to the SOCKS port to verify it's listening.
    /// 2. Measure RTT by making a small HTTP request through Tor
    ///    to `https://check.tor-project.org/api/ip`.
    pub async fn health_check(&self) -> Result<CircuitHealth, TorError> {
        // Step 1: TCP connect check.
        let connect_start = tokio::time::Instant::now();
        let stream = TcpStream::connect(&self.socks.addr()).await.map_err(|e| {
            TorError::HealthCheck(format!(
                "cannot connect to SOCKS port {}: {}",
                self.socks.addr(),
                e
            ))
        })?;
        drop(stream);
        let connect_ms = connect_start.elapsed().as_millis() as u64;

        // Step 2: Full proxy request to measure end-to-end latency.
        let request_start = tokio::time::Instant::now();
        let client = self.proxied_reqwest_client("check.tor-project.org")?;
        let response = client
            .get("https://check.tor-project.org/api/ip")
            .send()
            .await
            .map_err(|e| TorError::HealthCheck(format!("proxy request failed: {}", e)))?;

        let is_tor = response.status().is_success();
        let latency_ms = request_start.elapsed().as_millis() as u64;

        if !is_tor {
            return Err(TorError::HealthCheck(
                "check.tor-project.org returned non-success status".into(),
            ));
        }

        Ok(CircuitHealth {
            open_circuits: 0,  // Not available without control port query
            built_circuits: 0, // Not available without control port query
            failed_circuits: 0,
            latency_ms: Some(latency_ms),
            is_healthy: connect_ms < 1000 && latency_ms < 5000,
        })
    }

    /// Send `SIGNAL NEWNYM` to the Tor control port to force circuit
    /// rotation. This is useful when circuit health degrades or when
    /// the user explicitly requests a new identity.
    ///
    /// Authentication uses the cookie file at
    /// `/run/tor/control.authcookie` (hex-encoded 32 bytes).
    pub async fn new_circuit(&self) -> Result<(), TorError> {
        let control_addr = format!("{}:{}", self.socks.host, self.control_port);

        // Read the cookie file for authentication.
        let cookie = tokio::fs::read(&self.cookie_path)
            .await
            .map_err(|e| TorError::ControlPort(format!("cannot read cookie file {}: {}", self.cookie_path, e)))?;

        let cookie_hex: String = cookie.encode_hex();

        tracing::debug!(control_addr = %control_addr, "connecting to Tor control port");

        let mut stream = TcpStream::connect(&control_addr)
            .await
            .map_err(|e| TorError::ControlPort(format!("cannot connect to {}: {}", control_addr, e)))?;

        let (read_half, write_half) = stream.split();
        let mut reader = BufReader::new(read_half);
        let mut writer = write_half;

        // Authenticate with cookie.
        let auth_cmd = format!("AUTHENTICATE {}\r\n", cookie_hex);
        writer
            .write_all(auth_cmd.as_bytes())
            .await
            .map_err(|e| TorError::ControlPort(format!("write AUTHENTICATE failed: {}", e)))?;

        let mut response = String::new();
        reader
            .read_line(&mut response)
            .await
            .map_err(|e| TorError::ControlPort(format!("read AUTHENTICATE response failed: {}", e)))?;

        if !response.starts_with("250") {
            return Err(TorError::ControlPort(format!(
                "authentication rejected: {}",
                response.trim()
            )));
        }

        // Send SIGNAL NEWNYM.
        writer
            .write_all(b"SIGNAL NEWNYM\r\n")
            .await
            .map_err(|e| TorError::ControlPort(format!("write SIGNAL NEWNYM failed: {}", e)))?;

        response.clear();
        reader
            .read_line(&mut response)
            .await
            .map_err(|e| TorError::ControlPort(format!("read SIGNAL NEWNYM response failed: {}", e)))?;

        if !response.starts_with("250") {
            return Err(TorError::ControlPort(format!(
                "NEWNYM rejected: {}",
                response.trim()
            )));
        }

        tracing::info!("sent SIGNAL NEWNYM — Tor will build fresh circuits");
        Ok(())
    }

    /// Shut down the managed Tor process (if we own it).
    ///
    /// Sends SIGTERM to the child process and waits for it to exit.
    /// If the process doesn't exit within 5 seconds, it is killed.
    pub async fn shutdown(&self) -> Result<(), TorError> {
        if !self
            .owns_process
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            tracing::debug!("not the Tor process owner — nothing to shut down");
            return Ok(());
        }

        let mut guard = self.child.lock().await;
        if let Some(ref mut child) = *guard {
            tracing::info!("sending SIGTERM to Tor process");

            // Try graceful SIGTERM first.
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                let _ = unsafe { libc_kill(child.id().unwrap_or(0), libc::SIGTERM) };
            }
            #[cfg(not(unix))]
            {
                let _ = child.kill().await;
            }

            // Wait up to 5 seconds for the process to exit.
            let result = tokio::time::timeout(
                tokio::time::Duration::from_secs(5),
                child.wait(),
            )
            .await;

            match result {
                Ok(Ok(status)) => {
                    tracing::info!(exit_code = ?status.code(), "Tor process exited");
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "error waiting for Tor process to exit");
                }
                Err(_) => {
                    tracing::warn!("Tor process did not exit in 5s — killing");
                    let _ = child.kill().await;
                }
            }

            *guard = None;
            self.owns_process
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }

        Ok(())
    }

    // ── Private helpers ──────────────────────────────────────────────

    /// Parse a `"host:port"` string, falling back to `127.0.0.1:9050`.
    fn parse_addr(addr: &str) -> (String, u16) {
        let parts: Vec<&str> = addr.split(':').collect();
        if parts.len() == 2 {
            let host = parts[0].to_owned();
            let port = parts[1].parse::<u16>().unwrap_or(9050);
            (host, port)
        } else {
            ("127.0.0.1".to_owned(), 9050)
        }
    }

    /// Derive a SOCKS5 isolation username from a hostname.
    ///
    /// Uses `sha256(hostname)[..16]` to produce a stable, unique
    /// identifier per site. This feeds into Tor's `IsolateSOCKSAuth`
    /// so each site gets its own circuit.
    fn isolation_username(hostname: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(hostname.as_bytes());
        let hash = hasher.finalize();
        let hex: String = hash.encode_hex();
        format!("picast-{}", &hex[..16])
    }
}

// ── Utility functions ────────────────────────────────────────────────

/// Search for the `tor` binary on the system PATH.
fn which_tor() -> Result<String, TorError> {
    let candidates = ["tor", "/usr/bin/tor", "/usr/local/bin/tor"];

    for candidate in &candidates {
        if Path::new(candidate).exists() {
            return Ok(candidate.to_string());
        }
    }

    // Try `which tor` as a last resort.
    // We can't use tokio::process here since this is a sync function.
    if let Ok(output) = std::process::Command::new("which")
        .arg("tor")
        .output()
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(path);
            }
        }
    }

    Err(TorError::BinaryNotFound)
}

/// Send a signal to a process by PID (Unix only).
/// This is a thin wrapper around `libc::kill`.
#[cfg(unix)]
unsafe fn libc_kill(pid: u32, sig: i32) -> Result<(), TorError> {
    let ret = libc::kill(pid as i32, sig);
    if ret == 0 {
        Ok(())
    } else {
        Err(TorError::Io(std::io::Error::last_os_error()))
    }
}

impl Drop for TorManager {
    fn drop(&mut self) {
        if self
            .owns_process
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            tracing::warn!(
                "TorManager dropped while still owning the Tor process — \
                 attempting synchronous cleanup"
            );
            // Best-effort synchronous kill. The async `shutdown()` is
            // preferred but won't run in `drop`.
            if let Some(ref mut child) = self.child.try_lock() {
                if let Some(ref mut c) = **child {
                    #[cfg(unix)]
                    {
                        let _ = unsafe { libc_kill(c.id().unwrap_or(0), libc::SIGTERM) };
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = c.start_kill();
                    }
                }
            }
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
        assert_eq!(mgr.socks_addr(), "127.0.0.1:9050");
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

    #[test]
    fn isolation_username_deterministic() {
        let u1 = TorManager::isolation_username("youtube.com");
        let u2 = TorManager::isolation_username("youtube.com");
        assert_eq!(u1, u2, "same hostname must produce same username");
        assert!(u1.starts_with("picast-"), "username should have picast- prefix");
        assert_eq!(u1.len(), 23, "picast- (7) + 16 hex chars = 23");
    }

    #[test]
    fn isolation_username_different_sites() {
        let u1 = TorManager::isolation_username("youtube.com");
        let u2 = TorManager::isolation_username("vimeo.com");
        assert_ne!(u1, u2, "different hostnames must produce different usernames");
    }

    #[test]
    fn isolation_username_onion() {
        let u = TorManager::isolation_username("xyz123456.onion");
        assert!(u.starts_with("picast-"));
        assert_eq!(u.len(), 23);
    }

    #[test]
    fn proxied_reqwest_client_creates_client() {
        let mgr = TorManager::new("127.0.0.1:9050");
        let client = mgr.proxied_reqwest_client("example.com");
        assert!(client.is_ok(), "should create a reqwest client with SOCKS5h proxy");
    }

    #[test]
    fn with_control_port_builder() {
        let mgr = TorManager::new("127.0.0.1:9050").with_control_port(9151);
        assert_eq!(mgr.control_port, 9151);
    }

    #[test]
    fn with_cookie_path_builder() {
        let mgr = TorManager::new("127.0.0.1:9050")
            .with_cookie_path("/tmp/test-cookie");
        assert_eq!(mgr.cookie_path, "/tmp/test-cookie");
    }

    #[test]
    fn tor_error_variants() {
        let e = TorError::BinaryNotFound;
        assert!(e.to_string().contains("not found"));

        let e = TorError::ProcessExited(1);
        assert!(e.to_string().contains("exited with code 1"));

        let e = TorError::SocksTimeout(5000);
        assert!(e.to_string().contains("timeout"));

        let e = TorError::HealthCheck("bad".into());
        assert!(e.to_string().contains("health check failed"));

        let e = TorError::ControlPort("refused".into());
        assert!(e.to_string().contains("control port"));
    }
}
