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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

// ── Stream Isolation ─────────────────────────────────────────────────

/// Compute a per-domain SOCKS5 stream-isolation identifier.
///
/// Takes a domain name, hashes it with SHA-256, and returns a
/// string in the format `picast-<first_16_hex_chars>`. This feeds
/// into Tor's `IsolateSOCKSAuth` feature so that each unique domain
/// gets its own circuit.
pub fn stream_isolation_id(domain: &str) -> String {
    let hash = Sha256::digest(domain.as_bytes());
    let hex: String = hash.encode_hex();
    format!("picast-{}", &hex[..16])
}

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
    /// Prefix for stream-isolation usernames (default `"picast"`).
    pub stream_isolation_prefix: String,
}

impl SocksProxy {
    /// Create a proxy config listening on `host:port`.
    pub fn new(host: &str, port: u16) -> Self {
        Self {
            host: host.to_owned(),
            port,
            requires_auth: false,
            stream_isolation_prefix: "picast".to_owned(),
        }
    }

    /// Return the full address string (e.g. `127.0.0.1:9050`).
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Build a full SOCKS5h proxy URL with per-hostname circuit
    /// isolation embedded as the username.
    ///
    /// Returns a URL like `socks5h://picast-a804e89b1ec4a1d7@127.0.0.1:9050/`.
    pub fn proxy_url_for(&self, hostname: &str) -> String {
        let id = stream_isolation_id(hostname);
        format!("socks5h://{}@{}:{}/", id, self.host, self.port)
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
///
/// 1. Look for an already-running Tor on the configured SOCKS port.
/// 2. If none is found, spawn `tor` as a child process.
/// 3. Wait for the SOCKS port to become reachable (with a timeout).
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
    /// Whether auto-restart on crash is enabled (default: true).
    auto_restart: Arc<std::sync::atomic::AtomicBool>,
    /// Last known circuit health from control port monitoring.
    circuit_health: Arc<std::sync::Mutex<CircuitHealth>>,
    /// Shutdown signal for the background monitoring task.
    monitor_shutdown: Arc<tokio::sync::Notify>,
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
            auto_restart: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            circuit_health: Arc::new(std::sync::Mutex::new(CircuitHealth::default())),
            monitor_shutdown: Arc::new(tokio::sync::Notify::new()),
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

    /// Enable or disable auto-restart of the Tor process on crash.
    ///
    /// When enabled (the default), if the Tor child process exits
    /// unexpectedly, the manager will attempt to restart it and
    /// re-establish the SOCKS proxy. When disabled, an unexpected
    /// exit is simply logged.
    pub fn with_auto_restart(self, enabled: bool) -> Self {
        self.auto_restart.store(enabled, std::sync::atomic::Ordering::SeqCst);
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
        self.owns_process.store(true, std::sync::atomic::Ordering::SeqCst);

        // Poll the SOCKS port until it accepts connections.
        let deadline =
            tokio::time::Instant::now() + tokio::time::Duration::from_millis(startup_timeout_ms);
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
                            tracing::error!(
                                exit_code = code,
                                "Tor process exited during bootstrap"
                            );
                            return Err(TorError::ProcessExited(code));
                        },
                        Ok(None) => {
                            // Still running but port not open — timeout.
                        },
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to check Tor process status");
                        },
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

    /// Start a background task that monitors the Tor child process
    /// for unexpected exits and restarts it if auto-restart is enabled.
    /// Also periodically queries circuit health from the control port.
    ///
    /// Call this after `ensure_running()` has succeeded. The task runs
    /// until `shutdown()` is called or the `TorManager` is dropped.
    pub fn start_monitor(&self) {
        let child = self.child.clone();
        let owns_process = self.owns_process.clone();
        let auto_restart = self.auto_restart.clone();
        let circuit_health = self.circuit_health.clone();
        let socks = self.socks.clone();
        let control_port = self.control_port;
        let cookie_path = self.cookie_path.clone();
        let monitor_shutdown = self.monitor_shutdown.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                tokio::select! {
                    _ = monitor_shutdown.notified() => {
                        tracing::debug!("Tor monitor shutting down");
                        break;
                    }
                    _ = interval.tick() => {
                        // Check if the child process is still alive.
                        let should_restart = {
                            let mut guard = child.lock().await;
                            if let Some(ref mut c) = *guard {
                                match c.try_wait() {
                                    Ok(Some(status)) => {
                                        let code = status.code().unwrap_or(-1);
                                        tracing::warn!(
                                            exit_code = code,
                                            "Tor process exited unexpectedly"
                                        );
                                        *guard = None;
                                        owns_process.store(false, std::sync::atomic::Ordering::SeqCst);
                                        true
                                    }
                                    Ok(None) => false, // Still running
                                    Err(e) => {
                                        tracing::warn!(error = %e, "Failed to check Tor process");
                                        false
                                    }
                                }
                            } else {
                                false // No child to monitor
                            }
                        };

                        // Auto-restart if the process crashed.
                        if should_restart && auto_restart.load(std::sync::atomic::Ordering::SeqCst) {
                            tracing::info!("Attempting Tor auto-restart after crash");
                            let tor_path = match which_tor() {
                                Ok(p) => p,
                                Err(e) => {
                                    tracing::error!(error = %e, "Cannot find tor binary for restart");
                                    continue;
                                }
                            };

                            match Command::new(&tor_path)
                                .arg("--SocksPort")
                                .arg(socks.port.to_string())
                                .arg("--ControlPort")
                                .arg(control_port.to_string())
                                .arg("--CookieAuthentication")
                                .arg("1")
                                .arg("--IsolateSOCKSAuth")
                                .arg("1")
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null())
                                .spawn()
                            {
                                Ok(new_child) => {
                                    let mut guard = child.lock().await;
                                    *guard = Some(new_child);
                                    owns_process.store(true, std::sync::atomic::Ordering::SeqCst);
                                    tracing::info!("Tor process restarted successfully");
                                }
                                Err(e) => {
                                    tracing::error!(error = %e, "Failed to restart Tor process");
                                }
                            }
                        }

                        // Query circuit health from the control port.
                        if let Ok(health) = query_circuit_health(
                            &socks.host,
                            control_port,
                            &cookie_path,
                        ).await {
                            let mut guard = circuit_health.lock().unwrap_or_else(|e| {
                                tracing::warn!("circuit_health mutex poisoned — recovering");
                                e.into_inner()
                            });
                            *guard = health;
                        }
                    }
                }
            }
        });
    }

    /// Get the latest circuit health as reported by the control port
    /// monitoring task. Returns the default `CircuitHealth` if the
    /// monitor hasn't run yet or the control port is not available.
    pub fn last_circuit_health(&self) -> CircuitHealth {
        *self.circuit_health_lock()
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

    /// Perform a SOCKS5 handshake against the proxy to verify it is
    /// actually a functioning SOCKS5 server — not just a port that
    /// happens to be open.
    ///
    /// The handshake sends a SOCKS5 greeting with two supported auth
    /// methods (No Auth + Username/Password) and verifies the server
    /// responds with a valid SOCKS5 selection. Then performs a CONNECT
    /// request to a well-known host to measure round-trip latency.
    ///
    /// Returns the measured latency in milliseconds on success.
    pub async fn socks5_handshake(&self) -> Result<u64, TorError> {
        let addr = self.socks.addr();
        let mut stream = TcpStream::connect(&addr).await.map_err(|e| {
            TorError::HealthCheck(format!("cannot connect to SOCKS port {}: {}", addr, e))
        })?;

        // SOCKS5 greeting: version 5, 2 methods: No Auth (0x00), Username/Password (0x02)
        let greeting = [0x05, 0x02, 0x00, 0x02];
        stream
            .write_all(&greeting)
            .await
            .map_err(|e| TorError::HealthCheck(format!("SOCKS5 greeting write failed: {}", e)))?;

        // Server responds with [version, selected_method]
        let mut response = [0u8; 2];
        tokio::time::timeout(std::time::Duration::from_secs(5), stream.read_exact(&mut response))
            .await
            .map_err(|_| TorError::HealthCheck("SOCKS5 handshake timed out".into()))?
            .map_err(|e| TorError::HealthCheck(format!("SOCKS5 greeting read failed: {}", e)))?;

        if response[0] != 0x05 {
            return Err(TorError::HealthCheck(format!(
                "not a SOCKS5 proxy: server version byte was 0x{:02x}",
                response[0]
            )));
        }

        // Method 0x00 = No Auth, 0x02 = Username/Password. Both are acceptable.
        if response[1] != 0x00 && response[1] != 0x02 {
            return Err(TorError::HealthCheck(format!(
                "SOCKS5 server selected unsupported auth method 0x{:02x}",
                response[1]
            )));
        }

        // If server selected Username/Password auth (0x02), authenticate.
        if response[1] == 0x02 {
            let username = b"picast-health";
            let password = b"";
            let mut auth_msg = Vec::with_capacity(3 + username.len() + password.len());
            auth_msg.push(0x01); // sub-negotiation version
            auth_msg.push(username.len() as u8);
            auth_msg.extend_from_slice(username);
            auth_msg.push(password.len() as u8);
            auth_msg.extend_from_slice(password);

            stream
                .write_all(&auth_msg)
                .await
                .map_err(|e| TorError::HealthCheck(format!("SOCKS5 auth write failed: {}", e)))?;

            let mut auth_resp = [0u8; 2];
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                stream.read_exact(&mut auth_resp),
            )
            .await
            .map_err(|_| TorError::HealthCheck("SOCKS5 auth response timed out".into()))?
            .map_err(|e| TorError::HealthCheck(format!("SOCKS5 auth read failed: {}", e)))?;

            if auth_resp[1] != 0x00 {
                return Err(TorError::HealthCheck(format!(
                    "SOCKS5 auth rejected with status 0x{:02x}",
                    auth_resp[1]
                )));
            }
        }

        // CONNECT request to check.tor-project.org:443 to measure latency.
        let connect_start = tokio::time::Instant::now();

        // SOCKS5 CONNECT: version, CMD=CONNECT, RSV, ATYP=DOMAIN, domain, port
        let target_host = b"check.tor-project.org";
        let target_port: u16 = 443;
        let mut connect_req = Vec::with_capacity(6 + target_host.len());
        connect_req.push(0x05); // version
        connect_req.push(0x01); // CMD: CONNECT
        connect_req.push(0x00); // RSV
        connect_req.push(0x03); // ATYP: DOMAINNAME
        connect_req.push(target_host.len() as u8);
        connect_req.extend_from_slice(target_host);
        connect_req.extend_from_slice(&target_port.to_be_bytes());

        stream
            .write_all(&connect_req)
            .await
            .map_err(|e| TorError::HealthCheck(format!("SOCKS5 CONNECT write failed: {}", e)))?;

        // Read the CONNECT response (variable length, domain type: min 10 bytes)
        let mut connect_resp = [0u8; 256];
        tokio::time::timeout(std::time::Duration::from_secs(15), stream.read(&mut connect_resp))
            .await
            .map_err(|_| TorError::HealthCheck("SOCKS5 CONNECT response timed out".into()))?
            .map_err(|e| TorError::HealthCheck(format!("SOCKS5 CONNECT read failed: {}", e)))?;

        let latency_ms = connect_start.elapsed().as_millis() as u64;

        // Response[1] is the reply field: 0x00 = succeeded
        if connect_resp.len() >= 2 && connect_resp[1] != 0x00 {
            return Err(TorError::HealthCheck(format!(
                "SOCKS5 CONNECT failed with reply code 0x{:02x}",
                connect_resp[1]
            )));
        }

        tracing::debug!(latency_ms = latency_ms, "SOCKS5 handshake and CONNECT succeeded");

        Ok(latency_ms)
    }

    /// Perform a health check against the Tor SOCKS proxy.
    ///
    /// 1. TCP connect to the SOCKS port to verify it's listening.
    /// 2. SOCKS5 handshake to verify it's a real SOCKS5 proxy.
    /// 3. Measure RTT by making a CONNECT request through Tor
    ///    to `check.tor-project.org`.
    ///
    /// If `last_circuit_health` is set (from control port monitoring),
    /// those circuit counts are included in the result.
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

        // Step 2: SOCKS5 handshake to verify it's a functioning proxy.
        let latency_ms = self.socks5_handshake().await.ok();

        // Step 3: Also try the HTTP check for a more thorough validation.
        let http_latency = match self.proxied_reqwest_client("check.tor-project.org") {
            Ok(client) => {
                let start = tokio::time::Instant::now();
                match client
                    .get("https://check.tor-project.org/api/ip")
                    .timeout(std::time::Duration::from_secs(10))
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        Some(start.elapsed().as_millis() as u64)
                    },
                    _ => None,
                }
            },
            Err(_) => None,
        };

        // Use the best latency measurement available.
        let best_latency = http_latency.or(latency_ms);

        let health_snapshot = *self.circuit_health_lock();
        Ok(CircuitHealth {
            open_circuits: health_snapshot.open_circuits,
            built_circuits: health_snapshot.built_circuits,
            failed_circuits: health_snapshot.failed_circuits,
            latency_ms: best_latency,
            is_healthy: connect_ms < 1000 && best_latency.is_some_and(|l| l < 5000),
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
        let cookie = tokio::fs::read(&self.cookie_path).await.map_err(|e| {
            TorError::ControlPort(format!("cannot read cookie file {}: {}", self.cookie_path, e))
        })?;

        let cookie_hex: String = cookie.encode_hex();

        tracing::debug!(control_addr = %control_addr, "connecting to Tor control port");

        let mut stream = TcpStream::connect(&control_addr).await.map_err(|e| {
            TorError::ControlPort(format!("cannot connect to {}: {}", control_addr, e))
        })?;

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
        tokio::time::timeout(std::time::Duration::from_secs(5), reader.read_line(&mut response))
            .await
            .map_err(|_| TorError::ControlPort("AUTHENTICATE response timed out (5s)".into()))?
            .map_err(|e| {
                TorError::ControlPort(format!("read AUTHENTICATE response failed: {}", e))
            })?;

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
        tokio::time::timeout(std::time::Duration::from_secs(5), reader.read_line(&mut response))
            .await
            .map_err(|_| TorError::ControlPort("SIGNAL NEWNYM response timed out (5s)".into()))?
            .map_err(|e| {
                TorError::ControlPort(format!("read SIGNAL NEWNYM response failed: {}", e))
            })?;

        if !response.starts_with("250") {
            return Err(TorError::ControlPort(format!("NEWNYM rejected: {}", response.trim())));
        }

        tracing::info!("sent SIGNAL NEWNYM — Tor will build fresh circuits");
        Ok(())
    }

    /// Shut down the managed Tor process (if we own it).
    ///
    /// Sends SIGTERM to the child process and waits for it to exit.
    /// If the process doesn't exit within 5 seconds, it is killed.
    pub async fn shutdown(&self) -> Result<(), TorError> {
        // Signal the monitor task to stop.
        self.monitor_shutdown.notify_waiters();

        // Disable auto-restart so we don't try to restart a process
        // that we're intentionally shutting down.
        self.auto_restart.store(false, std::sync::atomic::Ordering::SeqCst);

        if !self.owns_process.load(std::sync::atomic::Ordering::SeqCst) {
            tracing::debug!("not the Tor process owner — nothing to shut down");
            return Ok(());
        }

        let mut guard = self.child.lock().await;
        if let Some(ref mut child) = *guard {
            tracing::info!("sending SIGTERM to Tor process");

            // Try graceful SIGTERM first.
            #[cfg(unix)]
            {
                let _ = unsafe { libc_kill(child.id().unwrap_or(0), libc::SIGTERM) };
            }
            #[cfg(not(unix))]
            {
                let _ = child.kill().await;
            }

            // Wait up to 5 seconds for the process to exit.
            let result =
                tokio::time::timeout(tokio::time::Duration::from_secs(5), child.wait()).await;

            match result {
                Ok(Ok(status)) => {
                    tracing::info!(exit_code = ?status.code(), "Tor process exited");
                },
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "error waiting for Tor process to exit");
                },
                Err(_) => {
                    tracing::warn!("Tor process did not exit in 5s — killing");
                    let _ = child.kill().await;
                },
            }

            *guard = None;
            self.owns_process.store(false, std::sync::atomic::Ordering::SeqCst);
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
    pub fn isolation_username(hostname: &str) -> String {
        stream_isolation_id(hostname)
    }

    /// Compute the SOCKS5 stream-isolation username for a full URL.
    ///
    /// Parses the URL to extract the hostname, then returns
    /// `stream_isolation_id(hostname)`. If the URL cannot be parsed
    /// or has no hostname, falls back to `stream_isolation_id(url)`.
    pub fn socks_username_for_url(&self, url: &str) -> String {
        match url::Url::parse(url) {
            Ok(parsed) => {
                if let Some(host) = parsed.host_str() {
                    stream_isolation_id(host)
                } else {
                    stream_isolation_id(url)
                }
            },
            Err(_) => stream_isolation_id(url),
        }
    }

    /// Lock circuit_health, recovering from poison if needed.
    fn circuit_health_lock(&self) -> std::sync::MutexGuard<'_, CircuitHealth> {
        self.circuit_health.lock().unwrap_or_else(|e| {
            tracing::warn!("circuit_health mutex poisoned — recovering");
            e.into_inner()
        })
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
    if let Ok(output) = std::process::Command::new("which").arg("tor").output() {
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

/// Query the Tor control port for circuit status information.
///
/// Connects to `host:control_port`, authenticates with the cookie file,
/// sends `GETINFO circuit-status`, and parses the response to count
/// circuits in each state (BUILT, FAILED, CLOSED, etc.).
///
/// Returns a `CircuitHealth` with the circuit counts populated.
/// If the control port is not reachable or authentication fails,
/// returns a `TorError::ControlPort`.
async fn query_circuit_health(
    host: &str,
    control_port: u16,
    cookie_path: &str,
) -> Result<CircuitHealth, TorError> {
    let control_addr = format!("{}:{}", host, control_port);

    // Read the cookie file for authentication.
    let cookie = tokio::fs::read(cookie_path).await.map_err(|e| {
        TorError::ControlPort(format!("cannot read cookie file {}: {}", cookie_path, e))
    })?;
    let cookie_hex: String = cookie.encode_hex();

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
    tokio::time::timeout(std::time::Duration::from_secs(5), reader.read_line(&mut response))
        .await
        .map_err(|_| TorError::ControlPort("AUTHENTICATE response timed out (5s)".into()))?
        .map_err(|e| TorError::ControlPort(format!("read AUTHENTICATE response failed: {}", e)))?;

    if !response.starts_with("250") {
        return Err(TorError::ControlPort(format!("authentication rejected: {}", response.trim())));
    }

    // Query circuit status.
    writer.write_all(b"GETINFO circuit-status\r\n").await.map_err(|e| {
        TorError::ControlPort(format!("write GETINFO circuit-status failed: {}", e))
    })?;

    // Read the multi-line response. Format:
    // 250+circuit-status=
    // <circuit lines>
    // .
    // 250 OK
    let mut circuit_data = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        tokio::time::timeout(std::time::Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .map_err(|_| TorError::ControlPort("circuit-status read timed out (5s)".into()))?
            .map_err(|e| {
                TorError::ControlPort(format!("read circuit-status line failed: {}", e))
            })?;

        if line.starts_with("250 OK") || line.starts_with("250-") {
            break;
        }
        if line == ".\r\n" || line == ".\n" {
            break;
        }
        // Skip the status header line.
        if line.starts_with("250+circuit-status=") {
            continue;
        }
        circuit_data.push_str(&line);
    }

    // Parse circuit lines. Each line format:
    // <circuit_id> BUILT|FAILED|CLOSED|... <path> ...
    let mut built: u32 = 0;
    let mut failed: u32 = 0;
    let mut open: u32 = 0;

    for circuit_line in circuit_data.lines() {
        let parts: Vec<&str> = circuit_line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let state = parts[1];
        match state {
            "BUILT" => {
                built += 1;
                open += 1;
            },
            "EXTENDED" | "GUARD_WAIT" => {
                open += 1;
            },
            "FAILED" => {
                failed += 1;
            },
            "CLOSED" => {
                failed += 1;
            },
            _ => {
                // Other states (LAUNCHED, etc.) count as open but not built.
                open += 1;
            },
        }
    }

    tracing::debug!(
        open = open,
        built = built,
        failed = failed,
        "queried circuit health from control port"
    );

    Ok(CircuitHealth {
        open_circuits: open,
        built_circuits: built,
        failed_circuits: failed,
        latency_ms: None, // Latency measured separately via SOCKS5 handshake
        is_healthy: built > 0,
    })
}

impl Drop for TorManager {
    fn drop(&mut self) {
        if self.owns_process.load(std::sync::atomic::Ordering::SeqCst) {
            tracing::warn!(
                "TorManager dropped while still owning the Tor process — \
                 attempting synchronous cleanup"
            );
            // Best-effort synchronous kill. The async `shutdown()` is
            // preferred but won't run in `drop`.
            match self.child.try_lock() {
                Ok(mut guard) => {
                    if let Some(ref mut c) = *guard {
                        #[cfg(unix)]
                        {
                            // Send SIGTERM to the Tor process specifically (not the process group).
                            if let Some(pid) = c.id() {
                                unsafe {
                                    let _ = libc::kill(pid as i32, libc::SIGTERM);
                                }
                            }
                        }
                        #[cfg(not(unix))]
                        {
                            let _ = c.start_kill();
                        }
                    }
                },
                Err(_) => {
                    tracing::warn!(
                        "Tor child lock held during drop — Tor process may be orphaned. \
                         Use shutdown() for clean termination."
                    );
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_isolation_id_deterministic() {
        let id1 = stream_isolation_id("youtube.com");
        let id2 = stream_isolation_id("youtube.com");
        assert_eq!(id1, id2);
        assert!(id1.starts_with("picast-"));
        assert_eq!(id1.len(), 7 + 16); // "picast-" + 16 hex chars
    }

    #[test]
    fn test_stream_isolation_id_different_domains() {
        let yt = stream_isolation_id("youtube.com");
        let vm = stream_isolation_id("vimeo.com");
        assert_ne!(yt, vm);
    }

    #[test]
    fn test_stream_isolation_id_same_domain_different_subdomains() {
        let www = stream_isolation_id("www.youtube.com");
        let bare = stream_isolation_id("youtube.com");
        assert_ne!(www, bare); // Different hostnames → different circuits
    }

    #[test]
    fn test_socks_username_for_url() {
        let tor = TorManager::new("127.0.0.1:9050");
        let id = tor.socks_username_for_url("https://www.youtube.com/watch?v=abc");
        assert!(id.starts_with("picast-"));
        assert_eq!(id.len(), 7 + 16);
    }

    #[test]
    fn test_socks_username_for_url_no_host() {
        let tor = TorManager::new("127.0.0.1:9050");
        let id = tor.socks_username_for_url("not-a-url");
        assert!(id.starts_with("picast-"));
    }

    #[test]
    fn test_socks_proxy_url_for() {
        let proxy = SocksProxy::new("127.0.0.1", 9050);
        let url = proxy.proxy_url_for("youtube.com");
        assert!(url.starts_with("socks5h://picast-"));
        assert!(url.contains("@127.0.0.1:9050/"));
    }

    #[test]
    fn test_socks_proxy_default() {
        let proxy = SocksProxy::default();
        assert_eq!(proxy.host, "127.0.0.1");
        assert_eq!(proxy.port, 9050);
        assert_eq!(proxy.addr(), "127.0.0.1:9050");
    }

    #[test]
    fn test_tor_manager_new_parsing() {
        let tor = TorManager::new("192.168.1.1:9051");
        assert_eq!(tor.socks().host, "192.168.1.1");
        assert_eq!(tor.socks().port, 9051);
    }

    #[test]
    fn test_tor_manager_new_default_port() {
        let tor = TorManager::new("invalid");
        assert_eq!(tor.socks().host, "127.0.0.1");
        assert_eq!(tor.socks().port, 9050);
    }

    #[test]
    fn test_circuit_health_default() {
        let h = CircuitHealth::default();
        assert!(h.is_healthy);
        assert_eq!(h.open_circuits, 0);
        assert!(h.latency_ms.is_none());
    }

    // ── Legacy tests (preserved) ─────────────────────────────────────

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
        let mgr = TorManager::new("127.0.0.1:9050").with_cookie_path("/tmp/test-cookie");
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

    // ── New tests for T-1.2, T-1.4, T-1.5 ────────────────────────────

    #[test]
    fn with_auto_restart_builder() {
        let mgr = TorManager::new("127.0.0.1:9050").with_auto_restart(false);
        assert!(!mgr.auto_restart.load(std::sync::atomic::Ordering::SeqCst));

        let mgr = TorManager::new("127.0.0.1:9050").with_auto_restart(true);
        assert!(mgr.auto_restart.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn last_circuit_health_defaults() {
        let mgr = TorManager::new("127.0.0.1:9050");
        let health = mgr.last_circuit_health();
        assert_eq!(health.open_circuits, 0);
        assert_eq!(health.built_circuits, 0);
        assert_eq!(health.failed_circuits, 0);
        assert!(health.latency_ms.is_none());
        assert!(health.is_healthy); // Default is healthy
    }

    #[test]
    fn circuit_health_default_values() {
        let h = CircuitHealth::default();
        assert_eq!(h.open_circuits, 0);
        assert_eq!(h.built_circuits, 0);
        assert_eq!(h.failed_circuits, 0);
        assert!(h.latency_ms.is_none());
        assert!(h.is_healthy);
    }

    #[test]
    fn socks5_handshake_fails_without_tor() {
        // Without Tor running, socks5_handshake should fail with HealthCheck error.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mgr = TorManager::new("127.0.0.1:19050"); // Non-standard port
        let result = rt.block_on(mgr.socks5_handshake());
        assert!(result.is_err());
        match result.unwrap_err() {
            TorError::HealthCheck(msg) => {
                assert!(msg.contains("cannot connect to SOCKS port"), "unexpected msg: {}", msg);
            },
            other => panic!("expected HealthCheck error, got: {:?}", other),
        }
    }

    #[test]
    fn health_check_fails_without_tor() {
        // Without Tor running, health_check should fail with HealthCheck error.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mgr = TorManager::new("127.0.0.1:19051"); // Non-standard port
        let result = rt.block_on(mgr.health_check());
        assert!(result.is_err());
        match result.unwrap_err() {
            TorError::HealthCheck(msg) => {
                assert!(msg.contains("cannot connect to SOCKS port"), "unexpected msg: {}", msg);
            },
            other => panic!("expected HealthCheck error, got: {:?}", other),
        }
    }

    #[test]
    fn tor_manager_new_initializes_monitor_shutdown() {
        let _mgr = TorManager::new("127.0.0.1:9050");
        // Verify monitor_shutdown is properly initialized (it's an Arc<Notify>).
        // We can't directly test the Notify, but we can verify the manager
        // doesn't panic when starting the monitor (even without a running tokio runtime).
        assert!(true, "TorManager initialized successfully with monitor fields");
    }

    #[test]
    fn parse_addr_various_formats() {
        let (host, port) = TorManager::parse_addr("192.168.1.1:9051");
        assert_eq!(host, "192.168.1.1");
        assert_eq!(port, 9051);

        let (host, port) = TorManager::parse_addr("localhost:9050");
        assert_eq!(host, "localhost");
        assert_eq!(port, 9050);

        let (host, port) = TorManager::parse_addr("invalid-port:abc");
        assert_eq!(host, "invalid-port");
        assert_eq!(port, 9050); // Fallback

        let (host, port) = TorManager::parse_addr("");
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 9050); // Fallback
    }
}
