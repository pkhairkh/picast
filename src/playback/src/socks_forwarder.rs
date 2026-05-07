//! Local HTTP CONNECT → SOCKS5 proxy forwarder.
//!
//! `souphttpsrc` (libsoup2.4) cannot use SOCKS5 proxy URIs. Tor's
//! HTTPTunnelPort uses a DIFFERENT circuit than the SOCKS5 port, so
//! CDN IP-bound tokens see a mismatch → 403 Forbidden.
//!
//! This module runs a tiny HTTP CONNECT proxy on a local port that
//! forwards each CONNECT request through Tor's SOCKS5 proxy WITH the
//! session's isolation username. Same username = same Tor circuit =
//! same exit IP as the resolver → CDN token matches → no 403.
//!
//! ## Architecture
//!
//! ```text
//! souphttpsrc ──CONNECT──► local:PORT ──SOCKS5h──► tor:9050 ──► CDN
//!                            (this code)    username=picast-HASH
//! ```
//!
//! The resolver (yt-dlp / reqwest) also uses `socks5h://picast-HASH@127.0.0.1:9050`
//! with the SAME username. Tor's `IsolateSOCKSAuth` maps identical usernames
//! to the same circuit, so both resolution and fetch exit through the same IP.
//!
//! ## Critical: Auth Method Selection
//!
//! We ONLY offer username/password auth (0x02) in the SOCKS5 greeting.
//! Previously, we offered both no-auth (0x00) and username/password (0x02),
//! which allowed Tor to choose no-auth. When Tor chose no-auth, the
//! isolation username was never sent, and the stream was assigned to a
//! different circuit than the resolver's — causing CDN 403 errors due to
//! IP mismatch. By offering only username/password auth, we guarantee
//! Tor uses the isolation username for circuit selection.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// A locally-bound HTTP CONNECT proxy that forwards through Tor's SOCKS5.
///
/// The proxy listens on a random port and handles one session at a time.
/// When the session ends, call `shutdown()` to stop the listener.
pub struct SocksForwarder {
    /// The local address the proxy is listening on (e.g. "127.0.0.1:42321").
    local_addr: String,
    /// Sender to signal shutdown.
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl SocksForwarder {
    /// Start a local HTTP CONNECT→SOCKS5 forwarder.
    ///
    /// - `socks_addr`: Tor SOCKS5 address (e.g. "127.0.0.1:9050")
    /// - `isolation_username`: SOCKS5 username for Tor circuit isolation
    ///   (e.g. "picast-a804e89b1ec4a1d7"). Same username = same circuit
    ///   = same exit IP as the resolver.
    ///
    /// Returns the forwarder with its local address. Set souphttpsrc's
    /// `proxy` property to `http://{local_addr}`.
    pub async fn start(
        socks_addr: String,
        isolation_username: String,
    ) -> Result<Self, String> {
        // Bind to localhost on a random port.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("bind forwarder: {}", e))?;
        let local_addr = listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "127.0.0.1:0".into());

        tracing::info!(
            local_addr = %local_addr,
            socks_addr = %socks_addr,
            username = %isolation_username,
            "SOCKS5 forwarder: local HTTP CONNECT proxy started, forwarding through Tor SOCKS5"
        );

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let username_clone = isolation_username.clone();
        let socks_clone = socks_addr.clone();

        tokio::spawn(async move {
            // Use a tokio::select! to handle both incoming connections
            // and shutdown signals.
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
                                        tracing::warn!(
                                            peer = %peer,
                                            error = %e,
                                            "SOCKS5 forwarder: connection failed"
                                        );
                                    }
                                });
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "SOCKS5 forwarder: accept failed");
                                break;
                            }
                        }
                    }
                    _ = &mut shutdown_rx => {
                        tracing::info!("SOCKS5 forwarder: shutdown signal received");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            local_addr,
            shutdown_tx: Some(shutdown_tx),
        })
    }

    /// Check the Tor exit IP by connecting through SOCKS5 to an IP echo service.
    /// Returns the exit IP as a string (e.g. "185.100.87.166") if successful.
    ///
    /// This is used to verify that the resolver and fetcher are using
    /// the same Tor circuit (and thus the same exit IP). If the exit IP
    /// doesn't match the `i=` parameter in the CDN URL, the CDN will
    /// return 403 Forbidden.
    pub async fn check_exit_ip(socks_addr: &str, isolation_username: &str) -> Option<String> {
        tracing::info!("Checking Tor exit IP for circuit isolation diagnostic...");

        // Connect to api.ipify.org:80 through Tor SOCKS5
        match socks5_connect(socks_addr, "api.ipify.org:80", isolation_username).await {
            Ok(mut stream) => {
                // Send a simple HTTP GET request
                let request = b"GET / HTTP/1.1\r\nHost: api.ipify.org\r\nConnection: close\r\n\r\n";
                if let Err(e) = stream.write_all(request).await {
                    tracing::warn!(error = %e, "exit IP check: failed to send HTTP request");
                    return None;
                }

                // Read the response
                let mut buf = vec![0u8; 1024];
                match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    stream.read(&mut buf),
                ).await {
                    Ok(Ok(n)) if n > 0 => {
                        let response = String::from_utf8_lossy(&buf[..n]);
                        // Parse the IP from the HTTP response body
                        // Response format: HTTP/1.1 200 OK\r\n...\r\n<IP>
                        if let Some(body) = response.split("\r\n\r\n").nth(1) {
                            let ip = body.trim().to_string();
                            tracing::info!(
                                exit_ip = %ip,
                                username = %isolation_username,
                                "Tor exit IP diagnostic: this is the IP the CDN will see. \
                                 Compare with the 'i=' parameter in the CDN URL."
                            );
                            Some(ip)
                        } else {
                            tracing::warn!(
                                response = %response,
                                "exit IP check: couldn't parse HTTP response body"
                            );
                            None
                        }
                    },
                    Ok(Ok(_)) => {
                        tracing::warn!("exit IP check: empty response from ipify");
                        None
                    },
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "exit IP check: failed to read response");
                        None
                    },
                    Err(_) => {
                        tracing::warn!("exit IP check: timed out waiting for response");
                        None
                    },
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "exit IP check: failed to connect through Tor SOCKS5");
                None
            },
        }
    }

    /// The local HTTP proxy address (e.g. "127.0.0.1:42321").
    /// Set souphttpsrc's `proxy` property to `http://{local_addr}`.
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

impl Drop for SocksForwarder {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Handle a single HTTP CONNECT request: parse the target host:port,
/// connect through SOCKS5, and tunnel data bidirectionally.
async fn handle_connect(
    mut client: TcpStream,
    socks_addr: &str,
    isolation_username: &str,
) -> Result<(), String> {
    // Read the CONNECT request line: "CONNECT host:port HTTP/1.1\r\n"
    // followed by headers until "\r\n\r\n".
    let mut buf = vec![0u8; 4096];
    let mut total = 0usize;

    loop {
        if total >= buf.len() {
            return Err("CONNECT request too large".into());
        }
        let n = client
            .read(&mut buf[total..])
            .await
            .map_err(|e| format!("read CONNECT: {}", e))?;
        if n == 0 {
            return Err("client disconnected before sending CONNECT".into());
        }
        total += n;

        // Check if we've received the full headers (terminated by \r\n\r\n).
        if total >= 4 && &buf[total - 4..total] == b"\r\n\r\n" {
            break;
        }
    }

    let request = std::str::from_utf8(&buf[..total])
        .map_err(|e| format!("CONNECT request not UTF-8: {}", e))?;

    // Parse "CONNECT host:port HTTP/1.1"
    let first_line = request.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 3 || parts[0] != "CONNECT" {
        return Err(format!("invalid CONNECT request: {:?}", first_line));
    }

    let target = parts[1]; // e.g. "cdn.example.com:443"

    // Connect to the target through Tor's SOCKS5 proxy.
    tracing::info!(
        target = %target,
        username = %isolation_username,
        socks_addr = %socks_addr,
        "SOCKS5 forwarder: connecting through Tor (same circuit as resolver)"
    );

    let remote = socks5_connect(socks_addr, target, isolation_username).await?;

    tracing::info!(
        target = %target,
        "SOCKS5 forwarder: tunnel established through Tor — CDN will see resolver's exit IP"
    );

    // Send "200 Connection Established" to souphttpsrc.
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .map_err(|e| format!("write 200: {}", e))?;

    // Tunnel data bidirectionally between client and remote.
    // Use into_split() to get owned halves (required for tokio::spawn which
    // needs 'static). split() returns borrowed halves tied to &mut TcpStream
    // which can't outlive this function.
    let (mut cr, mut cw) = client.into_split();
    let (mut rr, mut rw) = remote.into_split();

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

/// Connect to `target` through a SOCKS5h proxy with username authentication.
///
/// Implements the minimum RFC 1928 (SOCKS5) and RFC 1929 (username/password
/// auth) needed to talk to Tor's SOCKS5 port with IsolateSOCKSAuth.
async fn socks5_connect(
    socks_addr: &str,
    target: &str,
    username: &str,
) -> Result<TcpStream, String> {
    // Parse target into host and port.
    let (host, port) = parse_host_port(target)?;

    // Connect to the SOCKS5 proxy.
    let mut stream = TcpStream::connect(socks_addr)
        .await
        .map_err(|e| format!("connect to SOCKS5 {}: {}", socks_addr, e))?;

    // ── SOCKS5 handshake: greeting ─────────────────────────────
    // CRITICAL: We ONLY offer username/password auth (0x02).
    //
    // Previously we offered both no-auth (0x00) and username/password
    // (0x02). When Tor's IsolateSOCKSAuth is enabled and we offer both,
    // Tor may choose no-auth (0x00), which means the isolation username
    // is NEVER sent. The stream then gets assigned to a DIFFERENT
    // circuit (one for empty/no auth) than the resolver's circuit (which
    // used username/password auth with the isolation username). The CDN
    // sees a different Tor exit IP → 403 Forbidden.
    //
    // By offering ONLY username/password auth, we guarantee that Tor
    // uses the isolation username, which assigns the stream to the same
    // circuit as the resolver. Same circuit = same exit IP = CDN token
    // matches = no 403.
    //
    // This matches reqwest's behavior: when a SOCKS5 URL includes a
    // username (socks5h://picast-HASH@...), reqwest also only offers
    // username/password auth.
    stream
        .write_all(&[0x05, 0x01, 0x02]) // VER=5, NMETHODS=1, METHOD=0x02
        .await
        .map_err(|e| format!("SOCKS5 greet: {}", e))?;

    let mut reply = [0u8; 2];
    stream
        .read_exact(&mut reply)
        .await
        .map_err(|e| format!("SOCKS5 greet reply: {}", e))?;

    if reply[0] != 0x05 {
        return Err(format!("not SOCKS5: version {}", reply[0]));
    }

    // Tor MUST choose username/password auth since that's all we offered.
    if reply[1] != 0x02 {
        return Err(format!(
            "SOCKS5: expected username/password auth (0x02), got {} — \
             Tor may not support IsolateSOCKSAuth or SOCKS port is misconfigured",
            reply[1]
        ));
    }

    // ── SOCKS5 username/password authentication (RFC 1929) ─────
    let username_bytes = username.as_bytes();
    let password_bytes = b""; // Tor doesn't check the password
    if username_bytes.len() > 255 {
        return Err("SOCKS5 username too long".into());
    }
    let mut auth_req = vec![0x01, username_bytes.len() as u8];
    auth_req.extend_from_slice(username_bytes);
    auth_req.push(password_bytes.len() as u8);
    auth_req.extend_from_slice(password_bytes);

    stream
        .write_all(&auth_req)
        .await
        .map_err(|e| format!("SOCKS5 auth write: {}", e))?;

    let mut auth_reply = [0u8; 2];
    stream
        .read_exact(&mut auth_reply)
        .await
        .map_err(|e| format!("SOCKS5 auth reply: {}", e))?;

    if auth_reply[1] != 0x00 {
        return Err(format!("SOCKS5 auth rejected: status {}", auth_reply[1]));
    }
    tracing::info!(username = %username, "SOCKS5: authenticated with isolation username (same circuit as resolver)");

    // ── SOCKS5 CONNECT request (domain name, not resolved) ────
    // This is SOCKS5h — we send the domain name and let Tor resolve
    // it through the Tor network (preventing DNS leaks).
    let host_bytes = host.as_bytes();
    if host_bytes.len() > 255 {
        return Err(format!("hostname too long: {} bytes", host_bytes.len()));
    }

    // VER=5, CMD=1(CONNECT), RSV=0, ATYP=3(DOMAINNAME), LEN, HOST, PORT
    let mut connect_req = vec![0x05, 0x01, 0x00, 0x03, host_bytes.len() as u8];
    connect_req.extend_from_slice(host_bytes);
    connect_req.extend_from_slice(&[(port >> 8) as u8, (port & 0xFF) as u8]);

    stream
        .write_all(&connect_req)
        .await
        .map_err(|e| format!("SOCKS5 CONNECT write: {}", e))?;

    // Read CONNECT reply: VER(1) + REP(1) + RSV(1) + ATYP(1) + ADDR(varies)
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
    let bound_addr_str = match atyp {
        0x01 => {
            // IPv4: 4 bytes + 2 port
            let mut addr = [0u8; 6];
            stream
                .read_exact(&mut addr)
                .await
                .map_err(|e| format!("SOCKS5 addr read: {}", e))?;
            format!("{}.{}.{}.{}:{}", addr[0], addr[1], addr[2], addr[3], u16::from_be_bytes([addr[4], addr[5]]))
        }
        0x03 => {
            // Domain name: 1 len + name + 2 port
            let mut len_buf = [0u8; 1];
            stream
                .read_exact(&mut len_buf)
                .await
                .map_err(|e| format!("SOCKS5 domain len: {}", e))?;
            let mut domain = vec![0u8; len_buf[0] as usize + 2]; // +2 for port
            stream
                .read_exact(&mut domain)
                .await
                .map_err(|e| format!("SOCKS5 domain read: {}", e))?;
            format!("<domain>")
        }
        0x04 => {
            // IPv6: 16 bytes + 2 port
            let mut addr = [0u8; 18];
            stream
                .read_exact(&mut addr)
                .await
                .map_err(|e| format!("SOCKS5 IPv6 addr: {}", e))?;
            format!("<ipv6>")
        }
        other => {
            return Err(format!("SOCKS5 unknown ATYP {}", other));
        }
    };

    tracing::info!(
        host = %host,
        port = port,
        bound_addr = %bound_addr_str,
        "SOCKS5 CONNECT established through Tor (bound_addr is the exit-side address)"
    );

    Ok(stream)
}

/// Parse "host:port" from a CONNECT request target.
fn parse_host_port(target: &str) -> Result<(String, u16), String> {
    // Handle [IPv6]:port format.
    if let Some(close) = target.find(']') {
        let host = &target[..=close];
        let port_str = target[close + 1..].trim_start_matches(':');
        let port: u16 = port_str
            .parse()
            .map_err(|_| format!("invalid port in CONNECT target: {}", target))?;
        return Ok((host.to_string(), port));
    }

    // Standard host:port format.
    let colon = target
        .rfind(':')
        .ok_or_else(|| format!("no port in CONNECT target: {}", target))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_host_port() {
        assert_eq!(
            parse_host_port("cdn.example.com:443").unwrap(),
            ("cdn.example.com".into(), 443)
        );
        assert_eq!(
            parse_host_port("192.168.1.1:8080").unwrap(),
            ("192.168.1.1".into(), 8080)
        );
        assert_eq!(
            parse_host_port("[::1]:443").unwrap(),
            ("[::1]".into(), 443)
        );
    }

    #[test]
    fn test_socks5_reply_name() {
        assert_eq!(socks5_reply_name(0x00), "succeeded");
        assert_eq!(socks5_reply_name(0x01), "general SOCKS server failure");
        assert_eq!(socks5_reply_name(0x05), "connection refused");
        assert_eq!(socks5_reply_name(0xFF), "unknown");
    }
}
