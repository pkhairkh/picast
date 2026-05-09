//! Progressive download source for GStreamer appsrc.
//!
//! ## Problem
//!
//! The previous architecture piped CDN data through a real-time proxy chain
//! that required `download_speed ≥ video_bitrate` at all times — an assumption
//! that fails through Tor where throughput is variable (1-5 Mbps) and CDNs may
//! rate-limit (the `sp=380` URL parameter matches the observed 379 kbps).
//!
//! ## Solution
//!
//! Replace the real-time proxy chain with a progressive download
//! architecture that feeds data into GStreamer via `appsrc`:
//!
//! ```text
//! CDN → Tor → SOCKS Forwarder → reqwest → shared buffer → appsrc → queue2
//! ```
//!
//! Benefits:
//! - Eliminates the HTTP server relay hop (one fewer user-space relay)
//! - Eliminates souphttpsrc (no more HTTP/1.1 client overhead)
//! - Measures throughput BEFORE starting playback
//! - Pre-buffers data aggressively when throughput < video bitrate
//! - Provides download progress to the user
//!
//! ## Flow Control
//!
//! Data flows from the CDN download task into a bounded channel, then into
//! appsrc. When appsrc's internal queue is full (enough-data signal), the
//! download task pauses reading. When appsrc needs more data (need-data
//! signal), the download task resumes. This provides natural backpressure
//! without dropping data.

use crate::socks_forwarder::SocksForwarder;
use crate::DownloadProgress;
use futures_util::StreamExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

/// Browser-like User-Agent string. Must match the resolver's UA.
const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Maximum number of buffered chunks in the channel between the download
/// task and the appsrc push task. Each chunk is typically 32-256 KB
/// (reqwest's internal buffer size). 128 chunks × 256 KB = 32 MB of
/// buffer, enough to smooth out Tor's bursty delivery.
const CHANNEL_CAPACITY: usize = 128;

/// A chunk of downloaded data from the CDN.
#[derive(Debug)]
pub struct DataChunk {
    pub data: bytes::Bytes,
    pub offset: u64,
}

/// A progressive download source that streams CDN data into a bounded channel.
///
/// The source handles:
/// - Starting the SOCKS Forwarder for Tor circuit isolation
/// - Building a reqwest client with browser-like headers
/// - Downloading from the CDN with throughput measurement
/// - Providing data chunks via a channel
/// - Preflight CDN checks (403 detection)
pub struct StreamSource {
    /// Receiver end of the data channel. The consumer (appsrc push task)
    /// reads from this to get downloaded data.
    data_rx: mpsc::Receiver<DataChunk>,
    /// Sender end, kept here so we can clone it for reconnection scenarios.
    data_tx: mpsc::Sender<DataChunk>,
    /// Keeps the SOCKS forwarder alive for the download's lifetime.
    _socks_forwarder: SocksForwarder,
    /// The reqwest client used for CDN requests.
    client: reqwest::Client,
    /// CDN URL being downloaded (may be the speed-unlimited or original URL
    /// depending on fallback state).
    cdn_url: String,
    /// The original CDN URL (with sp= if present). Used as fallback if
    /// all sp= bypass URLs are rejected by the CDN (403).
    original_cdn_url: String,
    /// Remaining sp= bypass URLs to try. Each entry is (url, strategy_name).
    /// When a bypass URL is rejected (403), we pop it and try the next.
    /// When all bypasses are exhausted, we fall back to original_cdn_url.
    /// Empty Vec means the original URL had no sp= (no bypass needed).
    sp_bypass_urls: Vec<(String, String)>,
    /// Source URL for Referer header.
    source_url: String,
    /// Cookies from the resolver session.
    cookies: Vec<String>,
    /// Download progress (shared with the download task).
    progress: Arc<ProgressState>,
    /// Cancel token for the download task.
    cancel: Arc<AtomicBool>,
}

/// Shared progress state, updated by the download task and read by
/// the pipeline for download metrics reporting.
pub struct ProgressState {
    pub downloaded_bytes: AtomicU64,
    pub total_bytes: std::sync::Mutex<Option<u64>>,
    pub throughput_kbps: AtomicU64,
    pub start_time: std::sync::Mutex<Option<Instant>>,
    pub http_status: std::sync::Mutex<Option<u16>>,
    pub content_type: std::sync::Mutex<Option<String>>,
    /// CDN rate limit extracted from the URL's `sp=` query parameter.
    /// When `Some(380)`, the CDN caps throughput at ~380 kbps.
    /// When `None`, the CDN has no explicit rate limit.
    /// Used by the pipeline to log bitrate mismatch warnings and adjust
    /// buffering strategy.
    pub cdn_rate_limit_kbps: std::sync::Mutex<Option<u64>>,
}

impl ProgressState {
    /// Create a new progress state with all fields zeroed/empty.
    pub fn new() -> Self {
        Self {
            downloaded_bytes: AtomicU64::new(0),
            total_bytes: std::sync::Mutex::new(None),
            throughput_kbps: AtomicU64::new(0),
            start_time: std::sync::Mutex::new(None),
            http_status: std::sync::Mutex::new(None),
            content_type: std::sync::Mutex::new(None),
            cdn_rate_limit_kbps: std::sync::Mutex::new(None),
        }
    }

    /// Take a snapshot of the current download progress.
    pub fn snapshot(&self) -> DownloadProgress {
        let downloaded_bytes = self.downloaded_bytes.load(Ordering::Relaxed);
        let throughput_kbps = self.throughput_kbps.load(Ordering::Relaxed);
        let total_bytes = *self.total_bytes.lock().unwrap();
        let http_status = *self.http_status.lock().unwrap();
        let content_type = self.content_type.lock().unwrap().clone();
        let elapsed_secs =
            self.start_time.lock().unwrap().map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);

        DownloadProgress {
            downloaded_bytes,
            total_bytes,
            throughput_kbps,
            elapsed_secs,
            http_status: http_status.unwrap_or(0),
            content_type,
        }
    }
}

impl StreamSource {
    /// Start a new progressive download source.
    ///
    /// This creates the SOCKS Forwarder, builds the reqwest client, and
    /// prepares the data channel. Call `start_download()` to begin
    /// downloading from the CDN.
    pub async fn start(
        cdn_url: String,
        source_url: String,
        socks_addr: String,
        isolation_username: String,
        cookies: Vec<String>,
        progress: Arc<ProgressState>,
    ) -> Result<Self, String> {
        // Start SOCKS forwarder for reqwest's Tor routing.
        let socks_forwarder =
            SocksForwarder::start(socks_addr.clone(), isolation_username.clone()).await?;

        let proxy_url = socks_forwarder.proxy_url();

        // Build reqwest client that routes through the SOCKS forwarder.
        let reqwest_proxy = reqwest::Proxy::all(&proxy_url)
            .map_err(|e| format!("stream source: failed to configure HTTP proxy: {}", e))?;

        let client = reqwest::Client::builder()
            .user_agent(BROWSER_UA)
            .proxy(reqwest_proxy)
            .connect_timeout(std::time::Duration::from_secs(15))
            // No .timeout() — streaming a 400+ MB file takes minutes
            .no_gzip()
            .no_brotli()
            .use_rustls_tls()
            .build()
            .map_err(|e| format!("stream source: failed to build reqwest client: {}", e))?;

        // Extract CDN rate limit from URL's sp= parameter for diagnostics.
        // The sp= parameter caps CDN download speed (e.g. sp=380 = 380 kbps).
        // This information is used by the pipeline to log bitrate mismatch
        // warnings and adjust buffering strategy.
        let cdn_rate_limit = extract_cdn_speed_param(&cdn_url);
        if let Some(speed) = cdn_rate_limit {
            tracing::warn!(
                sp_kbps = speed,
                cdn_url = %cdn_url,
                "stream source: CDN URL contains speed-limit parameter (sp=). \
                 Throughput will be capped at ~{} kbps — may cause stuttering if video bitrate exceeds this",
                speed
            );
        } else {
            tracing::info!(
                cdn_url = %cdn_url,
                "stream source: CDN URL has no speed-limit parameter (sp=) — no explicit rate limit"
            );
        }

        // Store CDN rate limit in shared progress state for the pipeline
        *progress.cdn_rate_limit_kbps.lock().unwrap() = cdn_rate_limit;

        // Try to bypass the CDN speed limit (sp= parameter).
        //
        // Strategy: Instead of just stripping sp= (which causes CDN 403),
        // try replacing sp=380 with sp=99999 first. The CDN may accept a
        // higher value, effectively removing the speed cap. If the CDN
        // rejects the modified value (403), fall back to the original URL.
        //
        // We generate up to two bypass URLs to try:
        //   1. sp=99999 (replace with high value — CDN may accept)
        //   2. sp= stripped entirely (last resort — usually 403)
        // If all bypasses fail, we fall back to the original rate-limited URL.
        let original_cdn_url = cdn_url.clone();
        let sp_bypass_urls = generate_sp_bypass_urls(&cdn_url);
        let cdn_url = if let Some((ref first_bypass, _strategy)) = sp_bypass_urls.first() {
            tracing::info!(
                original_url = %original_cdn_url,
                bypass_url = %first_bypass,
                "stream source: generated sp= bypass URLs — will try speed-unlimited URL first"
            );
            first_bypass.clone()
        } else {
            cdn_url
        };

        let (data_tx, data_rx) = mpsc::channel(CHANNEL_CAPACITY);

        let cancel = Arc::new(AtomicBool::new(false));

        // Wrap sp_bypass_urls in a Vec of (url, strategy_name) to track
        // which bypass URLs we've tried. When all bypasses fail, we fall
        // back to the original URL.
        //   Empty Vec   — original URL had no sp= (no bypass possible)
        //   Non-empty   — bypass URLs to try; pop as each fails
        let sp_bypass_field: Vec<(String, String)> = sp_bypass_urls;

        Ok(Self {
            data_rx,
            data_tx,
            _socks_forwarder: socks_forwarder,
            client,
            cdn_url,
            original_cdn_url,
            sp_bypass_urls: sp_bypass_field,
            source_url,
            cookies,
            progress,
            cancel,
        })
    }

    /// Get the CDN rate limit extracted from the URL's `sp=` parameter.
    ///
    /// Returns `Some(kbps)` if the CDN URL has an `sp=` parameter,
    /// indicating the CDN rate-limits downloads. Returns `None` if
    /// there's no explicit rate limit.
    pub fn cdn_rate_limit_kbps(&self) -> Option<u64> {
        *self.progress.cdn_rate_limit_kbps.lock().unwrap()
    }

    /// Preflight check: verify the CDN accepts requests from this
    /// Tor circuit before starting the download.
    ///
    /// Returns the CDN's Content-Length header if available, which
    /// is used to estimate the video bitrate.
    ///
    /// ## Implementation
    ///
    /// Uses GET with `Range: bytes=0-0` instead of HEAD because many
    /// CDNs (including Voe's CDN) return 404 for HEAD requests on
    /// download URLs. The Range header limits the response to 1 byte,
    /// so the bandwidth cost is negligible. A 206 Partial Content
    /// response confirms the URL is valid and the CDN supports Range.
    ///
    /// ## Bypass URL Strategy
    ///
    /// If the CDN URL has an `sp=` rate-limit parameter, this method
    /// first tries bypass URLs (sp=99999, sp= stripped) in order.
    /// If a bypass URL is accepted, the CDN rate limit is effectively
    /// removed and playback proceeds at full speed.
    ///
    /// If ALL bypass URLs are rejected (403), this method falls back
    /// to the original unmodified URL (with sp=380 or similar). The
    /// CDN should accept the original URL since it was generated by
    /// the CDN itself. Playback will proceed at the rate-limited speed.
    ///
    /// Only if the **original** URL also returns 403 is an error
    /// returned — this indicates an IP block, not a bypass failure,
    /// and the session layer should re-resolve through a different
    /// Tor circuit.
    pub async fn preflight_check(&mut self) -> Result<Option<u64>, String> {
        loop {
            let result = self.do_preflight_request().await;

            match result {
                Ok(content_length) => {
                    // If a bypass URL was accepted, the CDN rate limit no
                    // longer applies — clear it so the pipeline doesn't use
                    // rate-limited buffering parameters.
                    if !self.sp_bypass_urls.is_empty() {
                        tracing::info!(
                            "stream source: CDN accepted sp= bypass URL — \
                             rate limit bypassed, clearing cdn_rate_limit_kbps"
                        );
                        *self.progress.cdn_rate_limit_kbps.lock().unwrap() = None;
                    }
                    return Ok(content_length);
                },
                Err(e) if e.contains("403") => {
                    // CDN returned 403. Try next bypass URL if available.
                    if let Some((next_url, strategy)) = self.try_next_bypass_url() {
                        tracing::info!(
                            error = %e,
                            next_bypass_url = %next_url,
                            strategy = %strategy,
                            "stream source: CDN rejected bypass URL, trying next bypass"
                        );
                        self.cdn_url = next_url;
                        continue; // Try next bypass URL
                    }

                    // All bypass URLs exhausted — try the original
                    // unmodified URL (with sp=380). The CDN generated this
                    // URL and should accept it for the correct exit IP.
                    if self.fallback_to_rate_limited_url() {
                        tracing::info!(
                            "stream source: all sp= bypass URLs rejected (403), \
                             trying original rate-limited URL as final fallback"
                        );
                        continue; // Try original URL
                    }

                    // Already using the original URL and it returned 403 —
                    // this is a genuine IP block or CDN rejection, not a
                    // bypass URL issue. Return error so the session layer
                    // can re-resolve through a different Tor circuit.
                    tracing::warn!(
                        "stream source: CDN 403 on ORIGINAL URL (not a bypass) — \
                         exit IP may be blocked by CDN anti-bot, re-resolve needed"
                    );
                    return Err(e);
                },
                Err(e) => return Err(e), // Non-403 errors
            }
        }
    }

    /// Internal: perform a single preflight HTTP request.
    ///
    /// Uses GET with `Range: bytes=0-0` instead of HEAD because many
    /// CDNs (including Voe's CDN) return 404 for HEAD requests on
    /// download URLs — they only support GET. The Range header ensures
    /// we only download 1 byte, so bandwidth is minimal.
    async fn do_preflight_request(&self) -> Result<Option<u64>, String> {
        let mut req = self
            .client
            .get(&self.cdn_url)
            .header("Accept", "*/*")
            .header("Accept-Encoding", "identity;q=1, *;q=0")
            .header("Range", "bytes=0-0")
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

        let response = self
            .client
            .execute(req.build().map_err(|e| format!("preflight: build request: {}", e))?)
            .await
            .map_err(|e| format!("preflight: CDN request failed: {}", e))?;

        let status = response.status();
        let headers = response.headers().clone();

        tracing::info!(
            status = %status,
            http_version = ?response.version(),
            content_length = ?headers.get("content-length").and_then(|v| v.to_str().ok()),
            content_type = ?headers.get("content-type").and_then(|v| v.to_str().ok()),
            content_range = ?headers.get("content-range").and_then(|v| v.to_str().ok()),
            "stream source: preflight CDN check"
        );

        if status.as_u16() == 403 {
            return Err(
                "CDN 403 Forbidden — re-resolve needed (exit IP may be blocked by CDN anti-bot)"
                    .into(),
            );
        }

        // 206 Partial Content = CDN supports Range and URL is valid.
        // 200 OK = CDN doesn't support Range but URL is valid.
        if !status.is_success() && status.as_u16() != 206 {
            return Err(format!(
                "CDN returned {} {} — cannot stream",
                status.as_u16(),
                status.canonical_reason().unwrap_or("Unknown")
            ));
        }

        // Consume the tiny response body for 206 (just 1 byte) to keep
        // the connection clean. For 200 (CDN ignored Range header), do
        // NOT consume the full body — just drop the response to abort
        // the download. The connection will be closed, which is fine
        // since we'll start a fresh download connection anyway.
        if status.as_u16() == 206 {
            let _ = response.bytes().await;
        }
        // For 200 OK, response is dropped here without consuming body

        // Extract total file size for bitrate estimation.
        //
        // For 206 Partial Content: Content-Range header has the format
        // "bytes 0-0/12345678" where 12345678 is the total file size.
        // Content-Length is just the size of the partial content (1 byte).
        //
        // For 200 OK: Content-Length is the full file size.
        let content_length = if status.as_u16() == 206 {
            // Parse Content-Range: "bytes 0-0/<total>"
            headers
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split('/').last())
                .and_then(|v| v.parse::<u64>().ok())
        } else {
            // 200 OK — Content-Length is the full file size
            headers
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
        };

        // Store file size in shared progress state for the pipeline.
        if let Some(cl) = content_length {
            *self.progress.total_bytes.lock().unwrap() = Some(cl);
        }

        Ok(content_length)
    }

    /// Start downloading from the CDN. Data chunks are sent to the
    /// channel and can be read via `recv_chunk()`.
    ///
    /// This spawns a background tokio task that:
    /// 1. Sends a GET request to the CDN with browser-like headers
    /// 2. Streams the response body into the data channel
    /// 3. Measures throughput and updates shared progress state
    /// 4. Handles CDN errors (403, etc.)
    pub fn start_download(&mut self, range: Option<String>) {
        let client = self.client.clone();
        let cdn_url = self.cdn_url.clone();
        let source_url = self.source_url.clone();
        let cookies = self.cookies.clone();
        let progress = self.progress.clone();
        let cancel = self.cancel.clone();
        let data_tx = self.data_tx.clone();

        // Reset cancel token and progress
        cancel.store(false, Ordering::Relaxed);
        progress.downloaded_bytes.store(0, Ordering::Relaxed);
        progress.throughput_kbps.store(0, Ordering::Relaxed);
        *progress.start_time.lock().unwrap() = Some(Instant::now());

        tokio::spawn(async move {
            // Build CDN request
            let mut req = client
                .get(&cdn_url)
                .header("Accept", "*/*")
                .header("Accept-Language", "en-US,en;q=0.9")
                .header("Accept-Encoding", "identity;q=1, *;q=0")
                .header("sec-ch-ua", r#""Chromium";v="131", "Not_A Brand";v="24""#)
                .header("sec-ch-ua-mobile", "?0")
                .header("sec-ch-ua-platform", "\"Windows\"")
                .header("Sec-Fetch-Dest", "video")
                .header("Sec-Fetch-Mode", "no-cors")
                .header("Sec-Fetch-Site", "cross-site");

            if !source_url.is_empty() {
                req = req.header("Referer", &source_url);
            }

            if let Some(range) = &range {
                req = req.header("Range", range);
                tracing::info!(range = %range, "stream source: forwarding Range header to CDN");
            }

            if !cookies.is_empty() {
                let cookie_header = cookies.join("; ");
                req = req.header("Cookie", &cookie_header);
                tracing::info!(
                    cookie_count = cookies.len(),
                    "stream source: forwarding cookies from resolver session"
                );
            }

            // Send request
            let built_req = match req.build() {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "stream source: failed to build request");
                    return;
                },
            };
            let response = match client.execute(built_req).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "stream source: CDN request failed");
                    return;
                },
            };

            let status = response.status();
            let headers = response.headers().clone();

            // Store HTTP status and content metadata
            *progress.http_status.lock().unwrap() = Some(status.as_u16());
            *progress.content_type.lock().unwrap() =
                headers.get("content-type").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
            *progress.total_bytes.lock().unwrap() = headers
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());

            tracing::info!(
                status = %status,
                http_version = ?response.version(),
                content_type = ?headers.get("content-type").and_then(|v| v.to_str().ok()),
                content_length = ?headers.get("content-length").and_then(|v| v.to_str().ok()),
                "stream source: CDN response received"
            );

            if status.as_u16() == 403 {
                tracing::warn!("stream source: CDN returned 403 Forbidden");
                return;
            }

            // Stream the response body
            let mut body_stream = response.bytes_stream();
            let mut offset: u64 = 0;
            let mut last_progress_update = Instant::now();
            let mut bytes_since_last_update: u64 = 0;
            let progress_update_interval = std::time::Duration::from_secs(2);

            while let Some(chunk_result) = body_stream.next().await {
                if cancel.load(Ordering::Relaxed) {
                    tracing::info!(total_bytes = offset, "stream source: download cancelled");
                    return;
                }

                match chunk_result {
                    Ok(chunk) => {
                        if chunk.is_empty() {
                            continue;
                        }

                        let chunk_len = chunk.len() as u64;
                        let chunk_offset = offset;

                        // Send chunk to the channel. If the channel is full
                        // (appsrc's queue is full), this will wait until
                        // space is available, providing natural backpressure.
                        if data_tx
                            .send(DataChunk { data: chunk, offset: chunk_offset })
                            .await
                            .is_err()
                        {
                            // Receiver dropped — pipeline was destroyed
                            tracing::debug!(
                                total_bytes = offset,
                                "stream source: receiver dropped, stopping download"
                            );
                            return;
                        }

                        offset += chunk_len;
                        bytes_since_last_update += chunk_len;
                        progress.downloaded_bytes.store(offset, Ordering::Relaxed);

                        // Update throughput measurement periodically
                        if last_progress_update.elapsed() >= progress_update_interval {
                            let elapsed = last_progress_update.elapsed().as_secs_f64();
                            if elapsed > 0.0 {
                                let kbps =
                                    (bytes_since_last_update * 8) / (elapsed * 1000.0) as u64;
                                progress.throughput_kbps.store(kbps, Ordering::Relaxed);
                            }
                            bytes_since_last_update = 0;
                            last_progress_update = Instant::now();

                            // Log progress periodically (every ~10 MB)
                            if offset % (10 * 1024 * 1024) < chunk_len {
                                let total_elapsed = progress
                                    .start_time
                                    .lock()
                                    .unwrap()
                                    .map(|t| t.elapsed().as_secs())
                                    .unwrap_or(0);
                                let throughput = progress.throughput_kbps.load(Ordering::Relaxed);
                                let total = progress.total_bytes.lock().unwrap();
                                tracing::info!(
                                    total_bytes = offset,
                                    file_size = ?total,
                                    throughput_kbps = throughput,
                                    elapsed_s = total_elapsed,
                                    "stream source: download progress"
                                );
                            }
                        }
                    },
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            total_bytes = offset,
                            "stream source: error reading from CDN stream"
                        );
                        break;
                    },
                }
            }

            let total_elapsed =
                progress.start_time.lock().unwrap().map(|t| t.elapsed().as_secs()).unwrap_or(0);
            tracing::info!(
                total_bytes = offset,
                elapsed_s = total_elapsed,
                "stream source: download completed"
            );
        });
    }

    /// Receive the next data chunk from the download task.
    ///
    /// Returns `None` if the download has completed and all chunks
    /// have been consumed.
    pub async fn recv_chunk(&mut self) -> Option<DataChunk> {
        self.data_rx.recv().await
    }

    /// Fall back to the original rate-limited CDN URL.
    ///
    /// Called when all sp= bypass URLs are rejected by the CDN (403).
    /// Switches `cdn_url` back to the original URL that includes
    /// the sp= parameter and clears the bypass URL list.
    ///
    /// Returns `true` if fallback happened (was using a bypass URL),
    /// `false` if already using the original URL (no fallback needed).
    pub fn fallback_to_rate_limited_url(&mut self) -> bool {
        if !self.sp_bypass_urls.is_empty() || self.cdn_url != self.original_cdn_url {
            tracing::info!(
                bypass_url = %self.cdn_url,
                original_url = %self.original_cdn_url,
                "stream source: falling back from sp= bypass URL to original rate-limited URL"
            );
            self.cdn_url = self.original_cdn_url.clone();
            self.sp_bypass_urls.clear(); // all bypasses exhausted
            // Update rate limit in progress state since we're now using
            // the rate-limited URL again
            let rate_limit = extract_cdn_speed_param(&self.cdn_url);
            *self.progress.cdn_rate_limit_kbps.lock().unwrap() = rate_limit;
            true
        } else {
            false
        }
    }

    /// Try the next sp= bypass URL. Called when the current bypass URL
    /// is rejected by the CDN (403). Pops the current URL and returns
    /// the next one, or None if all bypasses are exhausted.
    fn try_next_bypass_url(&mut self) -> Option<(String, String)> {
        // Pop the current bypass URL (the one that failed)
        if !self.sp_bypass_urls.is_empty() {
            self.sp_bypass_urls.remove(0);
        }
        // Return the next bypass URL if available
        if let Some((url, strategy)) = self.sp_bypass_urls.first() {
            Some((url.clone(), strategy.clone()))
        } else {
            None
        }
    }

    /// Get the current download progress.
    pub fn progress(&self) -> DownloadProgress {
        self.progress.snapshot()
    }

    /// Cancel the download task.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Check if the download has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

impl Drop for StreamSource {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Extract the CDN speed-limit parameter (`sp=`) from a URL's query string.
///
/// Many video CDNs (e.g. Voe) embed a rate-limit token as `&sp=NNN` where
/// NNN is the maximum download speed in kbps. When `sp=380`, the CDN caps
/// throughput at ~380 kbps.
///
/// Returns `None` if the URL has no `sp=` parameter (no rate limit — best case)
/// or if the value cannot be parsed as a number.
fn extract_cdn_speed_param(url: &str) -> Option<u64> {
    for prefix in &["&sp=", "?sp="] {
        if let Some(pos) = url.find(prefix) {
            let after = &url[pos + prefix.len()..];
            let value = after.split('&').next().unwrap_or("");
            if let Ok(speed) = value.parse::<u64>() {
                return Some(speed);
            }
        }
    }
    None
}

/// Generate a list of sp= bypass URLs to try before falling back to the
/// original rate-limited URL.
///
/// Returns a Vec of (url, strategy_name) pairs, in order of preference:
///   1. sp=99999 — replace the speed limit with a very high value.
///      The CDN may accept a higher sp= value, effectively removing
///      the speed cap while keeping the required parameter present.
///   2. sp= stripped — remove the parameter entirely. This usually
///      results in 403 from the CDN but is tried as a last resort.
///
/// Returns an empty Vec if the URL has no sp= parameter (no bypass needed).
fn generate_sp_bypass_urls(url: &str) -> Vec<(String, String)> {
    let has_sp = url.contains("?sp=") || url.contains("&sp=");
    if !has_sp {
        return Vec::new();
    }

    let mut bypasses = Vec::new();

    // Strategy 1: Replace sp=NNN with sp=99999
    if let Some(modified) = replace_cdn_speed_param(url, 99999) {
        bypasses.push((modified, "sp=99999 (high-value replacement)".to_string()));
    }

    // Strategy 2: Strip sp= entirely (usually 403 but worth trying)
    if let Some(modified) = strip_cdn_speed_param(url) {
        bypasses.push((modified, "sp= stripped".to_string()));
    }

    bypasses
}

/// Replace the CDN speed-limit parameter (`sp=`) value with a new value.
///
/// Returns `Some(modified_url)` if the `sp=` parameter was found and
/// replaced, or `None` if there was no `sp=` parameter in the URL.
///
/// # Examples
/// ```
/// assert_eq!(
///     replace_cdn_speed_param("https://cdn.example.com/video.mp4?t=abc&sp=380&i=199.195", 99999),
///     Some("https://cdn.example.com/video.mp4?t=abc&sp=99999&i=199.195".to_string())
/// );
/// ```
fn replace_cdn_speed_param(url: &str, new_value: u64) -> Option<String> {
    // Try &sp= first (middle or end of query string)
    if let Some(pos) = url.find("&sp=") {
        let before = &url[..pos];
        let after = &url[pos + 4..]; // skip "&sp="
        let value_len = after.find('&').unwrap_or(after.len());
        let rest = &after[value_len..];
        return Some(format!("{}&sp={}{}", before, new_value, rest));
    }

    // Try ?sp= (start of query string)
    if let Some(pos) = url.find("?sp=") {
        let before = &url[..pos];
        let after = &url[pos + 4..]; // skip "?sp="
        let value_len = after.find('&').unwrap_or(after.len());
        let rest = &after[value_len..];
        if rest.is_empty() {
            Some(format!("{}?sp={}", before, new_value))
        } else {
            Some(format!("{}?sp={}{}", before, new_value, rest))
        }
    } else {
        None
    }
}

/// Strip the CDN speed-limit parameter (`sp=`) from a URL's query string.
///
/// Returns `Some(modified_url)` if the `sp=` parameter was found and removed,
/// or `None` if there was no `sp=` parameter in the URL.
///
/// Handles all positions of `sp=` in the query string:
/// - `?sp=380` at the start of query string
/// - `&sp=380` in the middle of query string
/// - `&sp=380` at the end of query string (no trailing &)
///
/// # Examples
/// ```
/// // Middle of query string
/// assert_eq!(
///     strip_cdn_speed_param("https://cdn.example.com/video.mp4?t=abc&sp=380&i=199.195"),
///     Some("https://cdn.example.com/video.mp4?t=abc&i=199.195".to_string())
/// );
/// // Start of query string
/// assert_eq!(
///     strip_cdn_speed_param("https://cdn.example.com/video.mp4?sp=380&t=abc"),
///     Some("https://cdn.example.com/video.mp4?t=abc".to_string())
/// );
/// // End of query string
/// assert_eq!(
///     strip_cdn_speed_param("https://cdn.example.com/video.mp4?t=abc&sp=380"),
///     Some("https://cdn.example.com/video.mp4?t=abc".to_string())
/// );
/// // No sp= parameter
/// assert_eq!(strip_cdn_speed_param("https://cdn.example.com/video.mp4?t=abc"), None);
/// ```
fn strip_cdn_speed_param(url: &str) -> Option<String> {
    // First check if there's an sp= parameter at all
    let has_sp = url.contains("?sp=") || url.contains("&sp=");
    if !has_sp {
        return None;
    }

    // Try &sp= first (middle or end of query string)
    if let Some(pos) = url.find("&sp=") {
        let before = &url[..pos];
        let after = &url[pos + 4..]; // skip "&sp="
        let value_len = after.find('&').unwrap_or(after.len());
        let rest = &after[value_len..];
        return Some(format!("{}{}", before, rest));
    }

    // Try ?sp= (start of query string)
    if let Some(pos) = url.find("?sp=") {
        let before = &url[..pos];
        let after = &url[pos + 4..]; // skip "?sp="
        let value_len = after.find('&').unwrap_or(after.len());
        let rest = &after[value_len..];
        if rest.is_empty() {
            // sp= was the only query parameter — remove the ? entirely
            Some(before.to_string())
        } else {
            // Replace ?sp=...& with ? (next param becomes first)
            Some(format!("{}?{}", before, &rest[1..])) // rest starts with &
        }
    } else {
        None
    }
}
