//! Streaming HTTP media proxy for CDN anti-bot bypass.
//!
//! ## Problem
//!
//! GStreamer's `souphttpsrc` uses libsoup2.4, which:
//! - Only supports HTTP/1.1 (no HTTP/2)
//! - Uses GnuTLS for TLS (fingerprint differs from Chrome's BoringSSL)
//!
//! Video CDNs (Voe, DoodStream, Cloudflare-fronted hosts) run anti-bot
//! systems that fingerprint TLS handshakes and HTTP protocol versions.
//! A Chrome User-Agent string over HTTP/1.1 + GnuTLS is a dead giveaway
//! that the request is NOT from a real browser. The CDN returns 403.
//!
//! ## Solution
//!
//! Start a local HTTP server that:
//! 1. Accepts GET requests from `souphttpsrc` over plain HTTP on localhost
//! 2. Forwards each request to the CDN via `reqwest` (which uses HTTP/2
//!    + rustls — matching Chrome's real TLS fingerprint)
//! 3. Streams the CDN response body back to `souphttpsrc`
//!
//! ```text
//! souphttpsrc ──HTTP/1.1──► localhost:PORT ──HTTP/2+rustls──► CDN
//!                              (this code)       via Tor SOCKS5
//! ```
//!
//! `souphttpsrc` sees a plain HTTP response from localhost — no TLS
//! fingerprinting, no HTTP version mismatch. The CDN sees reqwest's
//! HTTP/2 + rustls connection — matching what a real Chrome browser
//! would send.

use crate::socks_forwarder::SocksForwarder;
use futures_util::StreamExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// Browser-like User-Agent string. Must match the resolver's UA.
const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// A locally-bound streaming HTTP proxy that fetches CDN content via
/// reqwest and serves it to souphttpsrc.
///
/// The proxy lives for the duration of a single playback session. It
/// is started before the GStreamer pipeline is constructed and shut
/// down when the pipeline is destroyed.
pub struct MediaProxy {
    /// The local address the proxy is listening on (e.g. "127.0.0.1:42321").
    local_addr: String,
    /// Sender to signal shutdown.
    shutdown_tx: Option<oneshot::Sender<()>>,
    /// Keeps the SOCKS forwarder alive for the proxy's lifetime.
    /// The SOCKS forwarder ensures reqwest's requests go through
    /// the same Tor circuit as the resolver (same exit IP → CDN
    /// IP-binding token matches).
    _socks_forwarder: SocksForwarder,
    /// The reqwest client used for CDN requests. Kept here so the
    /// preflight check can use the same client that the streaming
    /// server uses.
    client: reqwest::Client,
    /// CDN URL being proxied.
    cdn_url: String,
    /// Source URL for Referer header.
    source_url: String,
    /// Cookies from the resolver session.
    cookies: Vec<String>,
}

impl MediaProxy {
    /// Start a local streaming HTTP media proxy.
    ///
    /// - `cdn_url`: The direct CDN media URL to fetch
    /// - `source_url`: The originating page URL (used for Referer header)
    /// - `socks_addr`: Tor SOCKS5 address (e.g. "127.0.0.1:9050")
    /// - `isolation_username`: SOCKS5 username for Tor circuit isolation
    /// - `cookies`: Session cookies from the resolver to forward
    ///
    /// Returns the proxy with its local URL. Set souphttpsrc's `location`
    /// to `proxy.local_url()`.
    pub async fn start(
        cdn_url: String,
        source_url: String,
        socks_addr: String,
        isolation_username: String,
        cookies: Vec<String>,
    ) -> Result<Self, String> {
        // Start SOCKS forwarder for reqwest's Tor routing.
        // This ensures reqwest uses the same Tor circuit as the resolver,
        // so the CDN sees the same exit IP as the one bound to the URL
        // token.
        let socks_forwarder = SocksForwarder::start(
            socks_addr.clone(),
            isolation_username.clone(),
        )
        .await?;

        let proxy_url = socks_forwarder.proxy_url();

        // Build reqwest client that routes through the SOCKS forwarder.
        let reqwest_proxy = reqwest::Proxy::all(&proxy_url)
            .map_err(|e| format!("media proxy: failed to configure HTTP proxy for reqwest: {}", e))?;

        let client = reqwest::Client::builder()
            .user_agent(BROWSER_UA)
            .proxy(reqwest_proxy)
            // IMPORTANT: Do NOT set .timeout() here!
            //
            // reqwest's .timeout() is a TOTAL request timeout that covers
            // connection + reading the entire response body. For streaming
            // a 400+ MB video file, a 30-second timeout kills the stream
            // after ~11 MB, causing "error decoding response body" and
            // constant rebuffering.
            //
            // Instead, we rely on:
            //   - .connect_timeout() for the initial TCP+TLS handshake
            //   - TCP keepalive + Tor's own timeouts for detecting dead
            //     connections during streaming
            //   - The cancel token mechanism for graceful connection
            //     handoff when souphttpsrc reconnects
            .connect_timeout(std::time::Duration::from_secs(15))
            // Don't auto-decompress — we send Accept-Encoding: identity
            // for video (already compressed). Also, souphttpsrc needs
            // the raw bytes, not decompressed content.
            .no_gzip()
            .no_brotli()
            .build()
            .map_err(|e| format!("media proxy: failed to build reqwest client: {}", e))?;

        // Start local HTTP server on a random port.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("media proxy: bind: {}", e))?;
        let local_addr = listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "127.0.0.1:0".into());

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        tracing::info!(
            local_addr = %local_addr,
            cdn_url = %cdn_url,
            proxy_url = %proxy_url,
            "media proxy: local streaming HTTP server started (reqwest→CDN via Tor, souphttpsrc→localhost)"
        );

        // Cancel token for the currently active connection.
        // When souphttpsrc reconnects (e.g., after a CDN disconnect or for
        // seeking), we cancel the old connection and accept the new one.
        // This is critical: previously, a 429 rejection was sent to the
        // new connection, which souphttpsrc treated as a fatal error,
        // causing the pipeline to die entirely.
        let active_cancel: Arc<Mutex<Option<Arc<AtomicBool>>>> =
            Arc::new(Mutex::new(None));

        // Spawn the server task.
        tokio::spawn(async move {
            let listener = listener;
            tokio::pin!(shutdown_rx);

            loop {
                tokio::select! {
                    accept_result = listener.accept() => {
                        match accept_result {
                            Ok((stream, peer)) => {
                                let client = client.clone();
                                let cdn_url = cdn_url.clone();
                                let source_url = source_url.clone();
                                let cookies = cookies.clone();
                                let active_cancel = active_cancel.clone();

                                // Cancel any previous connection to make room
                                // for this new one. souphttpsrc reconnects when
                                // the CDN stream dies or when seeking. We must
                                // accept the new connection — rejecting with 429
                                // causes a fatal GStreamer error that kills the
                                // entire pipeline.
                                {
                                    let mut guard = active_cancel.lock().unwrap();
                                    if let Some(old_cancel) = guard.take() {
                                        tracing::info!(
                                            peer = %peer,
                                            "media proxy: cancelling previous connection — souphttpsrc reconnected"
                                        );
                                        old_cancel.store(true, Ordering::Relaxed);
                                    }
                                }

                                // Create a new cancel token for this connection.
                                let cancel = Arc::new(AtomicBool::new(false));
                                {
                                    let mut guard = active_cancel.lock().unwrap();
                                    *guard = Some(cancel.clone());
                                }

                                tokio::spawn(async move {
                                    if let Err(e) = handle_connection(
                                        stream,
                                        &client,
                                        &cdn_url,
                                        &source_url,
                                        &cookies,
                                        &cancel,
                                    )
                                    .await
                                    {
                                        tracing::warn!(
                                            error = %e,
                                            "media proxy: connection handler error"
                                        );
                                    }

                                    // Clean up: if we're still the active connection,
                                    // clear the cancel token.
                                    {
                                        let mut guard = active_cancel.lock().unwrap();
                                        let is_current = guard
                                            .as_ref()
                                            .map(|c| Arc::ptr_eq(c, &cancel))
                                            .unwrap_or(false);
                                        if is_current {
                                            *guard = None;
                                        }
                                    }
                                });
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "media proxy: accept failed");
                                break;
                            }
                        }
                    }
                    _ = &mut shutdown_rx => {
                        tracing::info!("media proxy: shutdown signal received");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            local_addr,
            shutdown_tx: Some(shutdown_tx),
            _socks_forwarder: socks_forwarder,
            client: client.clone(),
            cdn_url,
            source_url,
            cookies,
        })
    }

    /// Preflight check: verify the CDN accepts requests from this
    /// Tor circuit before building the GStreamer pipeline.
    ///
    /// Makes a quick HEAD request to the CDN with the same headers
    /// and cookies that the streaming proxy would use. If the CDN
    /// returns 403, we return an error immediately — before the
    /// pipeline is constructed — so the session layer's retry loop
    /// can re-resolve through a different Tor circuit.
    ///
    /// This is critical because the CDN 403 detection in the
    /// GStreamer bus watch is asynchronous: by the time the 403
    /// arrives, `play()` has already returned `Ok(())`, so the
    /// session's retry loop never triggers. The preflight check
    /// makes CDN 403s synchronous errors that the retry loop can
    /// handle.
    pub async fn preflight_check(&self) -> Result<(), String> {
        let mut req = self.client
            .head(&self.cdn_url)
            .header("Accept", "*/*")
            .header("Accept-Encoding", "identity;q=1, *;q=0")
            .header("sec-ch-ua", r#""Chromium";v="131", "Not_A Brand";v="24""#)
            .header("sec-ch-ua-mobile", "?0")
            .header("sec-ch-ua-platform", "\"Windows\"")
            .header("Sec-Fetch-Dest", "video")
            .header("Sec-Fetch-Mode", "no-cors")
            .header("Sec-Fetch-Site", "cross-site");

        if !self.source_url.is_empty() {
            req = req.header("Referer", &self.source_url);
        }

        if !self.cookies.is_empty() {
            let cookie_header = self.cookies.join("; ");
            req = req.header("Cookie", &cookie_header);
        }

        let response = self.client
            .execute(req.build().map_err(|e| format!("preflight: build request: {}", e))?)
            .await
            .map_err(|e| format!("preflight: CDN request failed: {}", e))?;

        let status = response.status();
        tracing::info!(
            status = %status,
            "media proxy: preflight CDN check"
        );

        if status.as_u16() == 403 {
            return Err(format!(
                "CDN 403 Forbidden — re-resolve needed (exit IP may be blocked by CDN anti-bot)"
            ));
        }

        // Accept 200 OK and 206 Partial Content (Range request).
        // Other status codes (4xx, 5xx) are also errors.
        if !status.is_success() && status.as_u16() != 206 {
            return Err(format!(
                "CDN returned {} {} — cannot stream",
                status.as_u16(),
                status.canonical_reason().unwrap_or("Unknown")
            ));
        }

        Ok(())
    }

    /// The HTTP URL for souphttpsrc to connect to (e.g. "http://127.0.0.1:42321/").
    ///
    /// Set souphttpsrc's `location` property to this URL.
    pub fn local_url(&self) -> String {
        format!("http://{}/", self.local_addr)
    }

    /// Shut down the proxy.
    pub fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for MediaProxy {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Handle a single HTTP/1.1 connection from souphttpsrc.
///
/// Reads the request, forwards it to the CDN via reqwest, and streams
/// the response body back. The `cancel` token is checked between chunks;
/// if a new souphttpsrc connection arrives, the old one is cancelled
/// gracefully.
async fn handle_connection(
    mut stream: TcpStream,
    client: &reqwest::Client,
    cdn_url: &str,
    source_url: &str,
    cookies: &[String],
    cancel: &AtomicBool,
) -> Result<(), String> {
    // Read HTTP request from souphttpsrc.
    let mut buf = vec![0u8; 4096];
    let n = stream
        .read(&mut buf)
        .await
        .map_err(|e| format!("read request: {}", e))?;

    if n == 0 {
        return Err("client disconnected before sending request".into());
    }

    let request = String::from_utf8_lossy(&buf[..n]);
    let request_str = request.to_string(); // owned copy for parsing

    // Parse Range header from souphttpsrc's request.
    // souphttpsrc sends Range headers when seeking.
    let range_header = extract_header(&request_str, "Range");

    tracing::info!(
        range = ?range_header,
        request_line = %request_str.lines().next().unwrap_or(""),
        "media proxy: received request from souphttpsrc"
    );

    // Build the CDN request with browser-like headers.
    let mut req = client
        .get(cdn_url)
        // Chrome's <video> element headers
        .header("Accept", "*/*")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Accept-Encoding", "identity;q=1, *;q=0")
        // Chrome Client Hints (must match User-Agent)
        .header("sec-ch-ua", r#""Chromium";v="131", "Not_A Brand";v="24""#)
        .header("sec-ch-ua-mobile", "?0")
        .header("sec-ch-ua-platform", "\"Windows\"")
        // Sec-Fetch-* headers for video element
        .header("Sec-Fetch-Dest", "video")
        .header("Sec-Fetch-Mode", "no-cors")
        .header("Sec-Fetch-Site", "cross-site");

    // Referer: send the FULL source URL. Some CDNs check the Referer
    // against the specific page that embedded the video. Chrome's default
    // referrer policy for cross-origin requests is
    // "strict-origin-when-cross-origin" which only sends the origin.
    // However, some CDNs (Voe) require the full page URL to validate
    // the request. Sending the full URL is more permissive and works
    // in both cases.
    if !source_url.is_empty() {
        req = req.header("Referer", source_url);
    }

    // Forward Range header for seeking.
    if let Some(range) = &range_header {
        req = req.header("Range", range);
        tracing::info!(range = %range, "media proxy: forwarding Range header to CDN");
    }

    // Forward cookies from the resolver session.
    if !cookies.is_empty() {
        let cookie_header = cookies.join("; ");
        req = req.header("Cookie", &cookie_header);
        tracing::info!(
            cookie_count = cookies.len(),
            "media proxy: forwarding cookies from resolver session"
        );
    }

    // Send the request to the CDN.
    let response = client
        .execute(req.build().map_err(|e| format!("build request: {}", e))?)
        .await
        .map_err(|e| format!("CDN request failed: {}", e))?;

    let status = response.status();
    let headers = response.headers().clone();

    tracing::info!(
        status = %status,
        content_type = ?headers.get("content-type").and_then(|v| v.to_str().ok()),
        content_length = ?headers.get("content-length").and_then(|v| v.to_str().ok()),
        content_range = ?headers.get("content-range").and_then(|v| v.to_str().ok()),
        "media proxy: CDN response received"
    );

    // If the CDN returned 403, forward it to souphttpsrc so the
    // CdnForbidden detection in the bus watch still works.
    if status.as_u16() == 403 {
        tracing::warn!("media proxy: CDN returned 403 Forbidden — forwarding to souphttpsrc");
        let body = response.bytes().await.unwrap_or_default();
        let body_len = body.len();
        let resp = format!(
            "HTTP/1.1 403 Forbidden\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body_len
        );
        stream.write_all(resp.as_bytes()).await.map_err(|e| format!("write 403 headers: {}", e))?;
        stream.write_all(&body).await.map_err(|e| format!("write 403 body: {}", e))?;
        return Ok(());
    }

    // Build HTTP/1.1 response for souphttpsrc.
    let status_code = status.as_u16();
    let status_text = status.canonical_reason().unwrap_or("OK");
    let mut response_header = format!("HTTP/1.1 {} {}\r\n", status_code, status_text);

    // Forward essential response headers.
    for name in &[
        "content-type",
        "content-length",
        "content-range",
        "accept-ranges",
        "cache-control",
    ] {
        if let Some(value) = headers.get(*name) {
            if let Ok(v) = value.to_str() {
                response_header.push_str(&format!("{}: {}\r\n", name, v));
            }
        }
    }

    // If no content-length, use chunked transfer encoding.
    let has_content_length = headers.contains_key("content-length");
    if !has_content_length {
        response_header.push_str("Transfer-Encoding: chunked\r\n");
    }

    response_header.push_str("Connection: close\r\n\r\n");

    // Send response headers to souphttpsrc.
    stream
        .write_all(response_header.as_bytes())
        .await
        .map_err(|e| format!("write response headers: {}", e))?;

    // Stream the response body to souphttpsrc.
    let mut body_stream = response.bytes_stream();
    let mut total_bytes: u64 = 0;
    let start = std::time::Instant::now();

    while let Some(chunk_result) = body_stream.next().await {
        // Check if this connection has been cancelled by a newer
        // souphttpsrc reconnection request.
        if cancel.load(Ordering::Relaxed) {
            tracing::info!(
                total_bytes = total_bytes,
                "media proxy: connection cancelled — souphttpsrc opened a new connection"
            );
            // Drop the response body (which cancels the CDN download)
            // and close the stream gracefully.
            return Ok(());
        }

        match chunk_result {
            Ok(chunk) => {
                if chunk.is_empty() {
                    continue;
                }

                if has_content_length {
                    // Direct write — souphttpsrc reads Content-Length bytes.
                    if let Err(e) = stream.write_all(&chunk).await {
                        tracing::debug!(
                            total_bytes = total_bytes,
                            error = %e,
                            "media proxy: souphttpsrc disconnected during stream"
                        );
                        break;
                    }
                } else {
                    // Chunked transfer encoding: size\r\n data\r\n
                    let size_line = format!("{:x}\r\n", chunk.len());
                    if let Err(e) = stream.write_all(size_line.as_bytes()).await {
                        tracing::debug!(
                            total_bytes = total_bytes,
                            error = %e,
                            "media proxy: souphttpsrc disconnected during chunked write"
                        );
                        break;
                    }
                    if let Err(e) = stream.write_all(&chunk).await {
                        tracing::debug!(
                            total_bytes = total_bytes,
                            error = %e,
                            "media proxy: souphttpsrc disconnected during chunked data"
                        );
                        break;
                    }
                    if let Err(e) = stream.write_all(b"\r\n").await {
                        tracing::debug!(
                            total_bytes = total_bytes,
                            error = %e,
                            "media proxy: souphttpsrc disconnected during chunked terminator"
                        );
                        break;
                    }
                }

                total_bytes += chunk.len() as u64;

                // Periodic progress logging (every ~10 MB)
                if total_bytes % (10 * 1024 * 1024) < chunk.len() as u64 {
                    let elapsed = start.elapsed();
                    let throughput_kbps = if elapsed.as_secs() > 0 {
                        (total_bytes / 1024) / elapsed.as_secs()
                    } else {
                        0
                    };
                    tracing::info!(
                        total_bytes = total_bytes,
                        throughput_kbps = throughput_kbps,
                        elapsed_s = elapsed.as_secs(),
                        "media proxy: streaming progress"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    total_bytes = total_bytes,
                    elapsed_s = start.elapsed().as_secs(),
                    "media proxy: error reading from CDN stream"
                );
                break;
            }
        }
    }

    // Finalize chunked transfer encoding.
    if !has_content_length {
        let _ = stream.write_all(b"0\r\n\r\n").await;
    }

    tracing::info!(
        total_bytes = total_bytes,
        elapsed_s = start.elapsed().as_secs(),
        "media proxy: stream completed"
    );

    Ok(())
}

/// Extract a header value from an HTTP request string.
///
/// Returns `Some(value)` if the header is found, `None` otherwise.
/// Header name matching is case-insensitive.
fn extract_header(request: &str, header_name: &str) -> Option<String> {
    let lower_name = header_name.to_lowercase();
    for line in request.lines() {
        if let Some(colon_pos) = line.find(':') {
            let name = line[..colon_pos].trim().to_lowercase();
            if name == lower_name {
                let value = line[colon_pos + 1..].trim().to_string();
                if !value.is_empty() {
                    return Some(value);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_header() {
        let request = "GET / HTTP/1.1\r\nHost: localhost\r\nRange: bytes=0-1024\r\n\r\n";
        assert_eq!(
            extract_header(request, "Range"),
            Some("bytes=0-1024".to_string())
        );
        assert_eq!(extract_header(request, "Host"), Some("localhost".to_string()));
        assert_eq!(extract_header(request, "X-Custom"), None);
    }

    #[test]
    fn test_extract_header_case_insensitive() {
        let request = "GET / HTTP/1.1\r\nrange: bytes=0-\r\n\r\n";
        assert_eq!(
            extract_header(request, "Range"),
            Some("bytes=0-".to_string())
        );
    }
}
