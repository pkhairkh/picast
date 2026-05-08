//! Resolver-side HTTP CONNECT → SOCKS5 proxy forwarder.
//!
//! This is the **mirror** of `playback::socks_forwarder`, used by the
//! resolver's reqwest HTTP client instead of reqwest's built-in SOCKS5
//! support.
//!
//! ## Why not use reqwest's built-in SOCKS5?
//!
//! reqwest's SOCKS5 implementation (via `tokio-socks`) offers BOTH
//! no-auth (0x00) and username/password (0x02) in its SOCKS5 greeting.
//! When Tor's `IsolateSOCKSAuth` is enabled and both methods are offered,
//! Tor may choose no-auth (0x00), which means the isolation username is
//! **never sent**. The stream gets assigned to a DIFFERENT circuit than
//! streams that use username/password auth (0x02).
//!
//! The playback path uses our custom `SocksForwarder` which ONLY offers
//! 0x02, guaranteeing the isolation username is sent. If the resolver
//! uses reqwest's built-in SOCKS5 (which may negotiate 0x00), the
//! resolver and playback end up on DIFFERENT Tor circuits with DIFFERENT
//! exit IPs. The CDN URL token is bound to the resolver's exit IP, but
//! the playback connects from a different exit IP → **403 Forbidden**.
//!
//! ## Solution
//!
//! Start a local HTTP CONNECT proxy (like the playback's `SocksForwarder`)
//! that uses our own `socks5_connect()` with ONLY username/password auth
//! (0x02). Give reqwest this proxy as an HTTP proxy. This guarantees
//! the resolver and playback use the same SOCKS5 auth method and thus
//! the same Tor circuit.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// A locally-bound HTTP CONNECT proxy that forwards through Tor's SOCKS5
/// with ONLY username/password auth (0x02), ensuring the same Tor circuit
/// as the playback path.
pub struct ResolverSocksForwarder {
    /// The local address the proxy is listening on (e.g. "127.0.0.1:42321").
    local_addr: String,
    /// Sender to signal shutdown.
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl ResolverSocksForwarder {
    /// Start a local HTTP CONNECT→SOCKS5 forwarder for the resolver.
    ///
    /// - `socks_addr`: Tor SOCKS5 address (e.g. "127.0.0.1:9050")
    /// - `isolation_username`: SOCKS5 username for Tor circuit isolation
    ///
    /// Returns the forwarder with its local address. Configure reqwest
    /// to use `http://{local_addr}` as its HTTP proxy.
    pub async fn start(socks_addr: String, isolation_username: String) -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("bind resolver forwarder: {}", e))?;
        let local_addr =
            listener.local_addr().map(|a| a.to_string()).unwrap_or_else(|_| "127.0.0.1:0".into());

        tracing::info!(
            local_addr = %local_addr,
            socks_addr = %socks_addr,
            username = %isolation_username,
            "resolver SOCKS5 forwarder: local HTTP CONNECT proxy started (auth=0x02 only)"
        );

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let username_clone = isolation_username.clone();
        let socks_clone = socks_addr.clone();

        tokio::spawn(async move {
            let listener = listener;
            tokio::pin!(shutdown_rx);

            loop {
                tokio::select! {
                    accept_result = listener.accept() => {
                        match accept_result {
                            Ok((stream, peer)) => {
                                let username = username_clone.clone();
                                let socks = socks_clone.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = handle_connect(stream, &socks, &username).await {
                                        tracing::debug!(
                                            peer = %peer,
                                            error = %e,
                                            "resolver SOCKS5 forwarder: connection failed"
                                        );
                                    }
                                });
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "resolver SOCKS5 forwarder: accept failed");
                                break;
                            }
                        }
                    }
                    _ = &mut shutdown_rx => {
                        tracing::debug!("resolver SOCKS5 forwarder: shutdown signal received");
                        break;
                    }
                }
            }
        });

        Ok(Self { local_addr, shutdown_tx: Some(shutdown_tx) })
    }

    /// The HTTP proxy URL for reqwest (e.g. "http://127.0.0.1:42321").
    pub fn proxy_url(&self) -> String {
        format!("http://{}", self.local_addr)
    }

    /// Shut down the forwarder.
    pub fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for ResolverSocksForwarder {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Handle a single HTTP CONNECT request from reqwest.
async fn handle_connect(
    mut client: TcpStream,
    socks_addr: &str,
    isolation_username: &str,
) -> Result<(), String> {
    let mut buf = vec![0u8; 4096];
    let mut total = 0usize;

    loop {
        if total >= buf.len() {
            return Err("CONNECT request too large".into());
        }
        let n = client.read(&mut buf[total..]).await.map_err(|e| format!("read CONNECT: {}", e))?;
        if n == 0 {
            return Err("client disconnected before sending CONNECT".into());
        }
        total += n;

        if total >= 4 && &buf[total - 4..total] == b"\r\n\r\n" {
            break;
        }
    }

    let request = std::str::from_utf8(&buf[..total])
        .map_err(|e| format!("CONNECT request not UTF-8: {}", e))?;

    let first_line = request.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 3 || parts[0] != "CONNECT" {
        return Err(format!("invalid CONNECT request: {:?}", first_line));
    }

    let target = parts[1];

    tracing::debug!(
        target = %target,
        username = %isolation_username,
        "resolver forwarder: connecting through Tor SOCKS5"
    );

    let remote = socks5_connect(socks_addr, target, isolation_username).await?;

    // Send "200 Connection Established" to reqwest.
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .map_err(|e| format!("write 200: {}", e))?;

    // Tunnel data bidirectionally.
    let (cr, cw) = client.into_split();
    let (rr, rw) = remote.into_split();

    let mut cr = tokio::io::BufReader::with_capacity(64 * 1024, cr);
    let mut rw = tokio::io::BufWriter::with_capacity(64 * 1024, rw);
    let mut rr = tokio::io::BufReader::with_capacity(64 * 1024, rr);
    let mut cw = tokio::io::BufWriter::with_capacity(64 * 1024, cw);

    let client_to_remote = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut cr, &mut rw).await;
        let _ = rw.shutdown().await;
    });
    let remote_to_client = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut rr, &mut cw).await;
        let _ = cw.shutdown().await;
    });

    let _ = client_to_remote.await;
    let _ = remote_to_client.await;

    Ok(())
}

/// Connect to `target` through a SOCKS5h proxy with ONLY username/password
/// auth (0x02). This is identical to the playback's `socks5_connect()`
/// and guarantees the same Tor circuit as playback.
async fn socks5_connect(
    socks_addr: &str,
    target: &str,
    username: &str,
) -> Result<TcpStream, String> {
    let (host, port) = parse_host_port(target)?;

    let mut stream = TcpStream::connect(socks_addr)
        .await
        .map_err(|e| format!("connect to SOCKS5 {}: {}", socks_addr, e))?;

    // ONLY offer username/password auth (0x02).
    // This is CRITICAL for Tor's IsolateSOCKSAuth to work correctly.
    // See the module-level documentation for the full explanation.
    stream
        .write_all(&[0x05, 0x01, 0x02]) // VER=5, NMETHODS=1, METHOD=0x02
        .await
        .map_err(|e| format!("SOCKS5 greet: {}", e))?;

    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).await.map_err(|e| format!("SOCKS5 greet reply: {}", e))?;

    if reply[0] != 0x05 {
        return Err(format!("not SOCKS5: version {}", reply[0]));
    }

    if reply[1] != 0x02 {
        return Err(format!(
            "SOCKS5: expected username/password auth (0x02), got 0x{:02x}",
            reply[1]
        ));
    }

    // Username/password authentication (RFC 1929).
    let username_bytes = username.as_bytes();
    let password_bytes = b"";
    if username_bytes.len() > 255 {
        return Err("SOCKS5 username too long".into());
    }
    let mut auth_req = vec![0x01, username_bytes.len() as u8];
    auth_req.extend_from_slice(username_bytes);
    auth_req.push(password_bytes.len() as u8);
    auth_req.extend_from_slice(password_bytes);

    stream.write_all(&auth_req).await.map_err(|e| format!("SOCKS5 auth write: {}", e))?;

    let mut auth_reply = [0u8; 2];
    stream.read_exact(&mut auth_reply).await.map_err(|e| format!("SOCKS5 auth reply: {}", e))?;

    if auth_reply[1] != 0x00 {
        return Err(format!("SOCKS5 auth rejected: status {}", auth_reply[1]));
    }

    tracing::debug!(username = %username, "resolver SOCKS5: authenticated with isolation username");

    // SOCKS5 CONNECT request (domain name, not resolved).
    let host_bytes = host.as_bytes();
    if host_bytes.len() > 255 {
        return Err(format!("hostname too long: {} bytes", host_bytes.len()));
    }

    let mut connect_req = vec![0x05, 0x01, 0x00, 0x03, host_bytes.len() as u8];
    connect_req.extend_from_slice(host_bytes);
    connect_req.extend_from_slice(&[(port >> 8) as u8, (port & 0xFF) as u8]);

    stream.write_all(&connect_req).await.map_err(|e| format!("SOCKS5 CONNECT write: {}", e))?;

    let mut reply_header = [0u8; 4];
    stream
        .read_exact(&mut reply_header)
        .await
        .map_err(|e| format!("SOCKS5 CONNECT reply: {}", e))?;

    if reply_header[0] != 0x05 {
        return Err(format!("SOCKS5 reply version {}", reply_header[0]));
    }
    if reply_header[1] != 0x00 {
        return Err(format!(
            "SOCKS5 CONNECT failed: status {} ({})",
            reply_header[1],
            socks5_reply_name(reply_header[1])
        ));
    }

    // Consume the bound address based on ATYP.
    let atyp = reply_header[3];
    match atyp {
        0x01 => {
            let mut addr = [0u8; 6];
            stream.read_exact(&mut addr).await.map_err(|e| format!("SOCKS5 addr read: {}", e))?;
        },
        0x03 => {
            let mut len_buf = [0u8; 1];
            stream
                .read_exact(&mut len_buf)
                .await
                .map_err(|e| format!("SOCKS5 domain len: {}", e))?;
            let mut domain = vec![0u8; len_buf[0] as usize + 2];
            stream
                .read_exact(&mut domain)
                .await
                .map_err(|e| format!("SOCKS5 domain read: {}", e))?;
        },
        0x04 => {
            let mut addr = [0u8; 18];
            stream.read_exact(&mut addr).await.map_err(|e| format!("SOCKS5 IPv6 addr: {}", e))?;
        },
        other => {
            return Err(format!("SOCKS5 unknown ATYP {}", other));
        },
    }

    tracing::debug!(
        host = %host,
        port = port,
        "resolver SOCKS5: CONNECT established through Tor"
    );

    Ok(stream)
}

/// Parse "host:port" from a CONNECT request target.
fn parse_host_port(target: &str) -> Result<(String, u16), String> {
    if let Some(close) = target.find(']') {
        let host = &target[..=close];
        let port_str = target[close + 1..].trim_start_matches(':');
        let port: u16 =
            port_str.parse().map_err(|_| format!("invalid port in CONNECT target: {}", target))?;
        return Ok((host.to_string(), port));
    }

    let colon =
        target.rfind(':').ok_or_else(|| format!("no port in CONNECT target: {}", target))?;
    let host = &target[..colon];
    let port: u16 = target[colon + 1..]
        .parse()
        .map_err(|_| format!("invalid port in CONNECT target: {}", target))?;
    Ok((host.to_string(), port))
}

/// Human-readable SOCKS5 reply code.
fn socks5_reply_name(code: u8) -> &'static str {
    match code {
        0x00 => "succeeded",
        0x01 => "general SOCKS server failure",
        0x02 => "connection not allowed by ruleset",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "TTL expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown",
    }
}
