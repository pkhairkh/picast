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
//! ## HLS Support
//!
//! When the CDN URL is an HLS playlist (.m3u8), StreamSource fetches the
//! master playlist, selects the highest-bandwidth variant, then downloads
//! each .ts segment sequentially. The MPEG-TS data is pushed into the same
//! channel as MP4 data — parsebin handles both formats natively.
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

/// Configuration for StreamSource preflight behavior.
///
/// Controls how many times the preflight check retries on CDN 403
/// before giving up and returning `PlaybackError::CdnForbidden`.
#[derive(Debug, Clone)]
pub struct StreamSourceConfig {
    /// Maximum number of preflight retry attempts on CDN 403.
    /// Each retry generates a new isolation username and re-resolves
    /// the URL through the resolver. Default: 3.
    pub preflight_retry_count: u32,
}

impl Default for StreamSourceConfig {
    fn default() -> Self {
        Self { preflight_retry_count: 3 }
    }
}
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

/// Download mode: direct MP4 or HLS segmented.
#[derive(Debug, Clone, PartialEq)]
enum DownloadMode {
    /// Direct MP4 download — single URL, streaming response.
    Mp4,
    /// HLS download — fetch master playlist, then variant playlist,
    /// then download each .ts segment sequentially.
    Hls {
        /// Parsed segment URLs from the variant playlist.
        segment_urls: Vec<String>,
    },
}

/// A progressive download source that streams CDN data into a bounded channel.
///
/// The source handles:
/// - Starting the SOCKS Forwarder for Tor circuit isolation (optional)
/// - Building a reqwest client with browser-like headers
/// - Downloading from the CDN with throughput measurement
/// - Providing data chunks via a channel
/// - Preflight CDN checks (403 detection)
/// - HLS playlist parsing and segment downloading
///
/// When `socks_addr` is empty, the source connects directly to the CDN
/// without Tor (no SOCKS forwarder). This is used when the resolver
/// didn't use Tor either, so the CDN URL is bound to the local IP.
pub struct StreamSource {
    /// Receiver end of the data channel. The consumer (appsrc push task)
    /// reads from this to get downloaded data.
    data_rx: mpsc::Receiver<DataChunk>,
    /// Sender end, kept here so we can clone it for reconnection scenarios.
    data_tx: mpsc::Sender<DataChunk>,
    /// Keeps the SOCKS forwarder alive for the download's lifetime.
    /// `None` when connecting directly (no Tor).
    _socks_forwarder: Option<SocksForwarder>,
    /// The reqwest client used for CDN requests.
    client: reqwest::Client,
    /// CDN URL being downloaded.
    cdn_url: String,
    /// Download mode: MP4 (direct) or HLS (segmented).
    mode: DownloadMode,
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
    /// Whether the download completed with an error (mid-stream disconnect).
    /// When true, the appsrc push thread should NOT push EOS into the
    /// pipeline — the stream is incomplete and EOS would tell GStreamer
    /// the stream is finished, causing premature playback termination.
    /// Instead, the pipeline will stall when it runs out of data, which
    /// the session layer can detect and handle (e.g. by re-resolving
    /// the URL and restarting playback).
    pub download_errored: AtomicBool,
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
            download_errored: AtomicBool::new(false),
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

/// Check if a URL points to an HLS playlist (.m3u8).
fn is_hls_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.contains(".m3u8")
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
        // Build reqwest client, optionally routing through SOCKS forwarder.
        //
        // When socks_addr is empty (direct resolution, no Tor), we skip
        // the SOCKS forwarder entirely and connect directly to the CDN.
        // The CDN URL was generated for the local IP (not a Tor exit),
        // so direct download works without circuit isolation.
        let (socks_forwarder, client) = if socks_addr.is_empty() || isolation_username.is_empty() {
            tracing::info!(
                cdn_url = %cdn_url,
                "stream source: direct mode (no Tor) — connecting to CDN without SOCKS forwarder"
            );
            let client = reqwest::Client::builder()
                .user_agent(BROWSER_UA)
                .connect_timeout(std::time::Duration::from_secs(15))
                .no_gzip()
                .no_brotli()
                .use_rustls_tls()
                .cookie_store(true)
                .build()
                .map_err(|e| format!("stream source: failed to build reqwest client: {}", e))?;
            (None, client)
        } else {
            // Start SOCKS forwarder for reqwest's Tor routing.
            let forwarder =
                SocksForwarder::start(socks_addr.clone(), isolation_username.clone()).await?;

            let proxy_url = forwarder.proxy_url();

            // Build reqwest client that routes through the SOCKS forwarder.
            let reqwest_proxy = reqwest::Proxy::all(&proxy_url)
                .map_err(|e| format!("stream source: failed to configure HTTP proxy: {}", e))?;

            let client = reqwest::Client::builder()
                .user_agent(BROWSER_UA)
                .proxy(reqwest_proxy)
                .connect_timeout(std::time::Duration::from_secs(15))
                .no_gzip()
                .no_brotli()
                .use_rustls_tls()
                .cookie_store(true)
                .build()
                .map_err(|e| format!("stream source: failed to build reqwest client: {}", e))?;
            (Some(forwarder), client)
        };

        // Extract CDN rate limit from URL's sp= parameter for diagnostics.
        // The sp= parameter caps CDN download speed (e.g. sp=380 = 380 kbps).
        // This information is used by the pipeline to log bitrate mismatch
        // warnings and adjust buffering strategy.
        //
        // NOTE: The sp= bypass logic (replacing/stripping sp=) has been removed
        // because it ALWAYS fails — modifying the URL's query parameters
        // invalidates the CDN's &t= signature, causing 403/404. The sp= value
        // is kept only for diagnostic logging.
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

        // Determine download mode based on URL type.
        let mode = if is_hls_url(&cdn_url) {
            tracing::info!(
                cdn_url = %cdn_url,
                "stream source: HLS URL detected — will use HLS client (master playlist → variant playlist → .ts segments)"
            );
            // Preflight will parse the playlist and fill segment_urls.
            DownloadMode::Hls { segment_urls: Vec::new() }
        } else {
            DownloadMode::Mp4
        };

        let (data_tx, data_rx) = mpsc::channel(CHANNEL_CAPACITY);

        let cancel = Arc::new(AtomicBool::new(false));

        Ok(Self {
            data_rx,
            data_tx,
            _socks_forwarder: socks_forwarder,
            client,
            cdn_url,
            mode,
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
    /// For MP4 URLs: Uses GET with `Range: bytes=0-0` to verify the CDN
    /// accepts the URL. Returns Content-Length for bitrate estimation.
    ///
    /// For HLS URLs: Fetches the master playlist, selects the best quality
    /// variant, fetches the variant playlist, and parses segment URLs.
    /// The segment URLs are stored in the mode for later download.
    pub async fn preflight_check(&mut self) -> Result<Option<u64>, String> {
        match &self.mode {
            DownloadMode::Mp4 => self.preflight_mp4().await,
            DownloadMode::Hls { .. } => self.preflight_hls().await,
        }
    }

    /// Preflight for MP4: verify CDN accepts the URL via Range request.
    async fn preflight_mp4(&self) -> Result<Option<u64>, String> {
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
            if let Ok(parsed) = url::Url::parse(&self.source_url) {
                let origin = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));
                if parsed.port().is_some() {
                    req = req.header("Origin", &self.source_url);
                } else {
                    req = req.header("Origin", &origin);
                }
            }
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
            "stream source: preflight CDN check (MP4)"
        );

        if status.as_u16() == 403 {
            let body = response.text().await.unwrap_or_default();
            let body_snippet = if body.len() > 200 { &body[..200] } else { &body };
            tracing::warn!(
                body = %body_snippet,
                content_length = ?headers.get("content-length").and_then(|v| v.to_str().ok()),
                "stream source: CDN 403 Forbidden — response body may explain the rejection"
            );
            return Err(
                "CDN 403 Forbidden — re-resolve needed (exit IP may be blocked by CDN anti-bot)"
                    .into(),
            );
        }

        if !status.is_success() && status.as_u16() != 206 {
            let body = response.text().await.unwrap_or_default();
            let body_snippet = if body.len() > 200 { &body[..200] } else { &body };
            tracing::warn!(
                status = %status,
                body = %body_snippet,
                "stream source: CDN returned non-2xx status — response body may explain the error"
            );
            return Err(format!(
                "CDN returned {} {} — cannot stream",
                status.as_u16(),
                status.canonical_reason().unwrap_or("Unknown")
            ));
        }

        // Consume the tiny response body for 206 (just 1 byte).
        if status.as_u16() == 206 {
            let _ = response.bytes().await;
        }

        // Extract total file size for bitrate estimation.
        let content_length = if status.as_u16() == 206 {
            headers
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split('/').last())
                .and_then(|v| v.parse::<u64>().ok())
        } else {
            headers
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
        };

        if let Some(cl) = content_length {
            *self.progress.total_bytes.lock().unwrap() = Some(cl);
        }

        Ok(content_length)
    }

    /// Preflight for HLS: fetch master playlist, select best variant,
    /// fetch variant playlist, parse segment URLs.
    async fn preflight_hls(&mut self) -> Result<Option<u64>, String> {
        // Step 1: Fetch the master playlist
        let master_playlist = self.fetch_playlist_text(&self.cdn_url).await?;

        tracing::info!(
            playlist_len = master_playlist.len(),
            "stream source: HLS master playlist fetched"
        );

        // Step 2: Parse master playlist to find the best quality variant URL
        let variant_url = parse_master_playlist(&master_playlist, &self.cdn_url)
            .ok_or_else(|| "HLS: could not parse master playlist — no variant found".to_string())?;

        tracing::info!(
            variant_url = %variant_url,
            "stream source: HLS selected best quality variant"
        );

        // Step 3: Fetch the variant playlist
        let variant_playlist = self.fetch_playlist_text(&variant_url).await?;

        tracing::info!(
            playlist_len = variant_playlist.len(),
            "stream source: HLS variant playlist fetched"
        );

        // Step 4: Parse variant playlist to get segment URLs
        let segment_urls =
            parse_variant_playlist(&variant_playlist, &variant_url).ok_or_else(|| {
                "HLS: could not parse variant playlist — no segments found".to_string()
            })?;

        tracing::info!(
            segment_count = segment_urls.len(),
            "stream source: HLS parsed segment URLs from variant playlist"
        );

        if segment_urls.is_empty() {
            return Err("HLS: variant playlist contains no segments".to_string());
        }

        // Store parsed segment URLs in the mode
        let total_segments = segment_urls.len();
        self.mode = DownloadMode::Hls { segment_urls };

        // Estimate total size: we don't know segment sizes until we download,
        // so return None for content_length. The pipeline will estimate from
        // throughput instead.
        *self.progress.total_bytes.lock().unwrap() = None;

        tracing::info!(
            segments = total_segments,
            "stream source: HLS preflight complete — ready to download segments"
        );

        Ok(None)
    }

    /// Fetch a playlist (master or variant) as text via the reqwest client.
    async fn fetch_playlist_text(&self, url: &str) -> Result<String, String> {
        // HLS playlist requests should use browser-like headers that match
        // what JWPlayer sends via fetch()/XHR. The key differences from MP4:
        //   - Sec-Fetch-Dest: "empty" (XHR/fetch, not <video> element)
        //   - Sec-Fetch-Mode: "cors" (JWPlayer uses CORS-enabled fetch)
        //   - Accept: include HLS MIME types
        let mut req = self
            .client
            .get(url)
            .header("Accept", "application/vnd.apple.mpegurl, application/x-mpegurl, */*")
            .header("Accept-Encoding", "identity;q=1, *;q=0")
            .header("sec-ch-ua", r#""Chromium";v="131", "Not_A Brand";v="24""#)
            .header("sec-ch-ua-mobile", "?0")
            .header("sec-ch-ua-platform", "\"Windows\"")
            .header("Sec-Fetch-Dest", "empty")
            .header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Site", "cross-site");

        if !self.source_url.is_empty() {
            req = req.header("Referer", &self.source_url);
            if let Ok(parsed) = url::Url::parse(&self.source_url) {
                let origin = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));
                if parsed.port().is_some() {
                    req = req.header("Origin", &self.source_url);
                } else {
                    req = req.header("Origin", &origin);
                }
            }
        }

        // Forward cookies from the resolver session. The CDN may validate
        // that the request includes session cookies (XSRF-TOKEN, voe_session)
        // that were set when the page was visited. Even though these cookies
        // are scoped to the page domain, the CDN may check them as part of
        // the session validation.
        if !self.cookies.is_empty() {
            let cookie_header = self.cookies.join("; ");
            req = req.header("Cookie", &cookie_header);
        }

        let response = self
            .client
            .execute(req.build().map_err(|e| format!("HLS: build playlist request: {}", e))?)
            .await
            .map_err(|e| format!("HLS: playlist request failed: {}", e))?;

        let status = response.status();
        if status.as_u16() == 403 {
            let body = response.text().await.unwrap_or_default();
            let body_snippet = if body.len() > 200 { &body[..200] } else { &body };
            tracing::warn!(
                body = %body_snippet,
                "stream source: HLS playlist 403 Forbidden"
            );
            return Err("CDN 403 Forbidden on HLS playlist — re-resolve needed".into());
        }

        if !status.is_success() {
            return Err(format!(
                "HLS playlist request returned {} {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("Unknown")
            ));
        }

        response.text().await.map_err(|e| format!("HLS: failed to read playlist body: {}", e))
    }

    /// Start downloading from the CDN. Data chunks are sent to the
    /// channel and can be read via `recv_chunk()`.
    ///
    /// For MP4: streams the response body into the data channel.
    /// For HLS: downloads each .ts segment sequentially and pushes data.
    ///
    /// This spawns a background tokio task that:
    /// 1. Sends a GET request to the CDN with browser-like headers
    /// 2. Streams the response body into the data channel
    /// 3. Measures throughput and updates shared progress state
    /// 4. Handles CDN errors (403, etc.)
    pub fn start_download(&mut self, _range: Option<String>) {
        match &self.mode {
            DownloadMode::Mp4 => self.start_download_mp4(),
            DownloadMode::Hls { segment_urls } => {
                let segment_urls = segment_urls.clone();
                self.start_download_hls(segment_urls);
            },
        }
    }

    /// Start MP4 download: stream the CDN response into the data channel.
    ///
    /// Includes automatic reconnect with Range header when the CDN connection
    /// drops mid-stream. Up to `MAX_CDN_RECONNECT_ATTEMPTS` reconnection
    /// attempts are made, resuming from the last byte received. If all
    /// reconnection attempts fail, the download is marked as errored
    /// (via `ProgressState::download_errored`) so the appsrc push thread
    /// knows NOT to push EOS — the stream is incomplete.
    fn start_download_mp4(&mut self) {
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
        progress.download_errored.store(false, Ordering::Relaxed);
        *progress.start_time.lock().unwrap() = Some(Instant::now());

        tokio::spawn(async move {
            /// Maximum number of reconnection attempts when CDN connection
            /// drops mid-stream. Each attempt resumes from the last byte
            /// received using a Range header.
            const MAX_CDN_RECONNECT_ATTEMPTS: u32 = 3;
            /// Delay between reconnection attempts (seconds). Allows the
            /// Tor circuit to stabilise and CDN rate-limit counters to reset.
            const RECONNECT_DELAY_SECS: u64 = 2;

            let mut total_offset: u64 = 0;
            let mut attempt = 0;

            loop {
                attempt += 1;

                // Build CDN request. On reconnect (attempt > 1), add a
                // Range header to resume from the last byte received.
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

                // On reconnect, request remaining bytes via Range header.
                // CDN signed URLs typically allow Range requests — the
                // signature covers the URL, not the request headers.
                if total_offset > 0 {
                    req = req.header("Range", format!("bytes={}-", total_offset));
                    tracing::info!(
                        attempt = attempt,
                        offset = total_offset,
                        "stream source: reconnecting with Range header to resume download"
                    );
                }

                if !source_url.is_empty() {
                    req = req.header("Referer", &source_url);
                    if let Ok(parsed) = url::Url::parse(&source_url) {
                        let origin =
                            format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));
                        if parsed.port().is_some() {
                            req = req.header("Origin", &source_url);
                        } else {
                            req = req.header("Origin", &origin);
                        }
                    }
                }

                if !cookies.is_empty() {
                    let cookie_header = cookies.join("; ");
                    req = req.header("Cookie", &cookie_header);
                    if attempt == 1 {
                        tracing::info!(
                            cookie_count = cookies.len(),
                            "stream source: forwarding cookies from resolver session"
                        );
                    }
                }

                // Send request
                let built_req = match req.build() {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!(error = %e, "stream source: failed to build request");
                        break;
                    },
                };
                let response = match client.execute(built_req).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            attempt = attempt,
                            offset = total_offset,
                            "stream source: CDN request failed"
                        );
                        // Connection-level error — try reconnect if we have bytes
                        if total_offset > 0 && attempt <= MAX_CDN_RECONNECT_ATTEMPTS {
                            tracing::info!(
                                attempt = attempt,
                                delay_secs = RECONNECT_DELAY_SECS,
                                "stream source: waiting before reconnect attempt..."
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(RECONNECT_DELAY_SECS)).await;
                            continue;
                        }
                        break;
                    },
                };

                let status = response.status();
                let headers = response.headers().clone();

                // Store HTTP status and content metadata (only on first attempt)
                if attempt == 1 {
                    *progress.http_status.lock().unwrap() = Some(status.as_u16());
                    *progress.content_type.lock().unwrap() =
                        headers.get("content-type").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
                    *progress.total_bytes.lock().unwrap() = headers
                        .get("content-length")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok());
                }

                tracing::info!(
                    status = %status,
                    http_version = ?response.version(),
                    content_type = ?headers.get("content-type").and_then(|v| v.to_str().ok()),
                    content_length = ?headers.get("content-length").and_then(|v| v.to_str().ok()),
                    attempt = attempt,
                    "stream source: CDN response received (MP4)"
                );

                if status.as_u16() == 403 {
                    tracing::warn!("stream source: CDN returned 403 Forbidden");
                    break;
                }

                // For 206 Partial Content (Range response), verify the range
                // starts from our expected offset. If not, log a warning but
                // continue — the data is still valid for the pipeline.
                if status.as_u16() == 206 && total_offset > 0 {
                    if let Some(range) = headers.get("content-range").and_then(|v| v.to_str().ok()) {
                        tracing::info!(
                            content_range = %range,
                            "stream source: CDN resumed from Range request"
                        );
                    }
                }

                // Stream the response body
                let mut body_stream = response.bytes_stream();
                let mut attempt_bytes: u64 = 0;
                let mut last_progress_update = Instant::now();
                let mut bytes_since_last_update: u64 = 0;
                let progress_update_interval = std::time::Duration::from_secs(2);
                let mut stream_errored = false;

                while let Some(chunk_result) = body_stream.next().await {
                    if cancel.load(Ordering::Relaxed) {
                        tracing::info!(total_bytes = total_offset, "stream source: download cancelled");
                        return;
                    }

                    match chunk_result {
                        Ok(chunk) => {
                            if chunk.is_empty() {
                                continue;
                            }

                            let chunk_len = chunk.len() as u64;
                            let chunk_offset = total_offset;

                            if data_tx
                                .send(DataChunk { data: chunk, offset: chunk_offset })
                                .await
                                .is_err()
                            {
                                tracing::debug!(
                                    total_bytes = total_offset,
                                    "stream source: receiver dropped, stopping download"
                                );
                                return;
                            }

                            total_offset += chunk_len;
                            attempt_bytes += chunk_len;
                            bytes_since_last_update += chunk_len;
                            progress.downloaded_bytes.store(total_offset, Ordering::Relaxed);

                            if last_progress_update.elapsed() >= progress_update_interval {
                                let elapsed = last_progress_update.elapsed().as_secs_f64();
                                if elapsed > 0.0 {
                                    let kbps =
                                        (bytes_since_last_update * 8) / (elapsed * 1000.0) as u64;
                                    progress.throughput_kbps.store(kbps, Ordering::Relaxed);
                                }
                                bytes_since_last_update = 0;
                                last_progress_update = Instant::now();

                                if total_offset % (10 * 1024 * 1024) < chunk_len {
                                    let total_elapsed = progress
                                        .start_time
                                        .lock()
                                        .unwrap()
                                        .map(|t| t.elapsed().as_secs())
                                        .unwrap_or(0);
                                    let throughput = progress.throughput_kbps.load(Ordering::Relaxed);
                                    let total = progress.total_bytes.lock().unwrap();
                                    tracing::info!(
                                        total_bytes = total_offset,
                                        file_size = ?total,
                                        throughput_kbps = throughput,
                                        elapsed_s = total_elapsed,
                                        "stream source: download progress (MP4)"
                                    );
                                }
                            }
                        },
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                total_bytes = total_offset,
                                attempt = attempt,
                                "stream source: error reading from CDN stream"
                            );
                            stream_errored = true;
                            break;
                        },
                    }
                }

                // If the stream errored and we have received some data,
                // try to reconnect with a Range header to resume.
                if stream_errored && total_offset > 0 && attempt <= MAX_CDN_RECONNECT_ATTEMPTS {
                    tracing::info!(
                        attempt = attempt,
                        max_attempts = MAX_CDN_RECONNECT_ATTEMPTS,
                        offset = total_offset,
                        attempt_bytes = attempt_bytes,
                        "stream source: CDN stream error — attempting reconnect with Range header"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(RECONNECT_DELAY_SECS)).await;
                    continue;
                }

                // If we broke out of the stream loop without error, the
                // download is complete (full response consumed). Exit.
                // If we broke out due to an error and exhausted retries,
                // also exit (but mark as errored below).
                break;
            }

            // Check if the download completed successfully or errored.
            let total_bytes_expected = *progress.total_bytes.lock().unwrap();
            let download_complete = match total_bytes_expected {
                Some(expected) => total_offset >= expected,
                None => {
                    // No Content-Length — consider the download complete if
                    // the stream ended without error (no reconnect was attempted).
                    attempt <= 1
                },
            };

            let total_elapsed =
                progress.start_time.lock().unwrap().map(|t| t.elapsed().as_secs()).unwrap_or(0);

            if download_complete {
                tracing::info!(
                    total_bytes = total_offset,
                    elapsed_s = total_elapsed,
                    "stream source: download completed (MP4)"
                );
            } else {
                // Download is incomplete — mark as errored so the appsrc
                // push thread does NOT push EOS. The pipeline will stall
                // when it runs out of data, which the session layer can
                // detect and handle (e.g. by re-resolving and restarting).
                tracing::warn!(
                    total_bytes = total_offset,
                    expected_bytes = ?total_bytes_expected,
                    elapsed_s = total_elapsed,
                    "stream source: download incomplete (CDN disconnected) — marking as errored \
                     (appsrc will NOT push EOS — pipeline will stall when buffer depletes)"
                );
                progress.download_errored.store(true, Ordering::Relaxed);
            }
        });
    }

    /// Start HLS download: download each .ts segment and push data into
    /// the channel.
    fn start_download_hls(&mut self, segment_urls: Vec<String>) {
        let client = self.client.clone();
        let source_url = self.source_url.clone();
        let cookies = self.cookies.clone();
        let progress = self.progress.clone();
        let cancel = self.cancel.clone();
        let data_tx = self.data_tx.clone();
        let total_segments = segment_urls.len();

        // Reset cancel token and progress
        cancel.store(false, Ordering::Relaxed);
        progress.downloaded_bytes.store(0, Ordering::Relaxed);
        progress.throughput_kbps.store(0, Ordering::Relaxed);
        progress.download_errored.store(false, Ordering::Relaxed);
        *progress.start_time.lock().unwrap() = Some(Instant::now());

        // Store content type as MPEG-TS for HLS
        *progress.content_type.lock().unwrap() = Some("video/MP2T".to_string());

        tracing::info!(segments = total_segments, "stream source: starting HLS segment download");

        tokio::spawn(async move {
            let mut offset: u64 = 0;
            let mut last_progress_update = Instant::now();
            let mut bytes_since_last_update: u64 = 0;
            let progress_update_interval = std::time::Duration::from_secs(2);
            let mut segments_downloaded = 0usize;
            let mut had_error = false;

            for (idx, seg_url) in segment_urls.iter().enumerate() {
                // Check cancellation between segments
                if cancel.load(Ordering::Relaxed) {
                    tracing::info!(
                        total_bytes = offset,
                        segment = idx,
                        "stream source: HLS download cancelled"
                    );
                    return;
                }

                // Build segment request — use video element headers
                // (.ts segments are loaded by the <video> element's MSE,
                // so Sec-Fetch-Dest: "video" is appropriate for segments)
                let mut req = client
                    .get(seg_url)
                    .header("Accept", "*/*")
                    .header("Accept-Encoding", "identity;q=1, *;q=0")
                    .header("sec-ch-ua", r#""Chromium";v="131", "Not_A Brand";v="24""#)
                    .header("sec-ch-ua-mobile", "?0")
                    .header("sec-ch-ua-platform", "\"Windows\"")
                    .header("Sec-Fetch-Dest", "video")
                    .header("Sec-Fetch-Mode", "no-cors")
                    .header("Sec-Fetch-Site", "cross-site");

                if !source_url.is_empty() {
                    req = req.header("Referer", &source_url);
                    if let Ok(parsed) = url::Url::parse(&source_url) {
                        let origin =
                            format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));
                        if parsed.port().is_some() {
                            req = req.header("Origin", &source_url);
                        } else {
                            req = req.header("Origin", &origin);
                        }
                    }
                }

                if !cookies.is_empty() {
                    let cookie_header = cookies.join("; ");
                    req = req.header("Cookie", &cookie_header);
                }

                // Fetch segment
                let built_req = match req.build() {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            segment = idx,
                            url = %seg_url,
                            "stream source: HLS failed to build segment request"
                        );
                        had_error = true;
                        break;
                    },
                };

                let response = match client.execute(built_req).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            segment = idx,
                            url = %seg_url,
                            "stream source: HLS segment request failed"
                        );
                        had_error = true;
                        break;
                    },
                };

                let status = response.status();

                if status.as_u16() == 403 {
                    tracing::warn!(
                        segment = idx,
                        url = %seg_url,
                        "stream source: HLS segment 403 Forbidden"
                    );
                    had_error = true;
                    break;
                }

                if !status.is_success() {
                    tracing::warn!(
                        status = %status,
                        segment = idx,
                        url = %seg_url,
                        "stream source: HLS segment returned non-2xx"
                    );
                    had_error = true;
                    break;
                }

                // Store HTTP status from first segment
                if idx == 0 {
                    *progress.http_status.lock().unwrap() = Some(status.as_u16());
                }

                // Stream the segment body into the data channel
                let mut body_stream = response.bytes_stream();
                let mut segment_bytes: u64 = 0;

                while let Some(chunk_result) = body_stream.next().await {
                    if cancel.load(Ordering::Relaxed) {
                        tracing::info!(
                            total_bytes = offset,
                            segment = idx,
                            "stream source: HLS download cancelled during segment"
                        );
                        return;
                    }

                    match chunk_result {
                        Ok(chunk) => {
                            if chunk.is_empty() {
                                continue;
                            }

                            let chunk_len = chunk.len() as u64;
                            let chunk_offset = offset;

                            if data_tx
                                .send(DataChunk { data: chunk, offset: chunk_offset })
                                .await
                                .is_err()
                            {
                                tracing::debug!(
                                    total_bytes = offset,
                                    "stream source: receiver dropped, stopping HLS download"
                                );
                                return;
                            }

                            offset += chunk_len;
                            segment_bytes += chunk_len;
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
                            }
                        },
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                segment = idx,
                                segment_bytes = segment_bytes,
                                "stream source: error reading HLS segment stream"
                            );
                            had_error = true;
                            break;
                        },
                    }
                }

                tracing::debug!(
                    segment = idx,
                    total_segments = total_segments,
                    segment_bytes = segment_bytes,
                    total_bytes = offset,
                    "stream source: HLS segment downloaded"
                );
                segments_downloaded += 1;
            }

            let total_elapsed =
                progress.start_time.lock().unwrap().map(|t| t.elapsed().as_secs()).unwrap_or(0);

            if had_error || segments_downloaded < total_segments {
                // Download incomplete — mark as errored so the appsrc
                // push thread does NOT push EOS.
                tracing::warn!(
                    total_bytes = offset,
                    segments_downloaded = segments_downloaded,
                    total_segments = total_segments,
                    elapsed_s = total_elapsed,
                    "stream source: HLS download incomplete — marking as errored \
                     (appsrc will NOT push EOS — pipeline will stall when buffer depletes)"
                );
                progress.download_errored.store(true, Ordering::Relaxed);
            } else {
                tracing::info!(
                    total_bytes = offset,
                    segments = total_segments,
                    elapsed_s = total_elapsed,
                    "stream source: HLS download completed"
                );
            }
        });
    }

    /// Receive the next data chunk from the download task.
    ///
    /// Returns `None` if the download has completed and all chunks
    /// have been consumed.
    pub async fn recv_chunk(&mut self) -> Option<DataChunk> {
        self.data_rx.recv().await
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

// ── HLS Playlist Parsing ─────────────────────────────────────────────

/// Parse a master playlist and return the variant URL with the highest
/// bandwidth.
///
/// Master playlist format:
/// ```text
/// #EXTM3U
/// #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360
/// https://cdn.example.com/360p.m3u8
/// #EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720
/// https://cdn.example.com/720p.m3u8
/// ```
///
/// Returns `None` if no variant is found.
fn parse_master_playlist(playlist: &str, base_url: &str) -> Option<String> {
    let mut best_bandwidth: u64 = 0;
    let mut best_url: Option<String> = None;

    let lines: Vec<&str> = playlist.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();

        if line.starts_with("#EXT-X-STREAM-INF:") {
            // Parse BANDWIDTH from the attributes
            let bandwidth = parse_bandwidth_from_stream_inf(line);

            // The next non-empty, non-comment line is the variant URL
            let mut j = i + 1;
            while j < lines.len() {
                let next_line = lines[j].trim();
                if next_line.is_empty() || next_line.starts_with('#') {
                    j += 1;
                    continue;
                }
                // This is the variant URL
                if bandwidth > best_bandwidth {
                    best_bandwidth = bandwidth;
                    best_url = Some(resolve_url(next_line, base_url));
                }
                break;
            }
            i = j + 1;
            continue;
        }

        i += 1;
    }

    if let Some(ref url) = best_url {
        tracing::info!(
            bandwidth = best_bandwidth,
            url = %url,
            "HLS: selected highest bandwidth variant from master playlist"
        );
    } else {
        tracing::warn!("HLS: no variant found in master playlist");
    }

    best_url
}

/// Parse BANDWIDTH value from an #EXT-X-STREAM-INF line.
fn parse_bandwidth_from_stream_inf(line: &str) -> u64 {
    // Format: #EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720,CODECS="avc1..."
    for attr in line.split(',') {
        let attr = attr.trim();
        if let Some(rest) = attr.strip_prefix("BANDWIDTH=") {
            if let Ok(bw) = rest.parse::<u64>() {
                return bw;
            }
        }
    }
    // Also check the first attribute (after the colon, before the first comma)
    // Format: #EXT-X-STREAM-INF:BANDWIDTH=2800000
    if let Some(pos) = line.find("BANDWIDTH=") {
        let after = &line[pos + "BANDWIDTH=".len()..];
        let value = after.split(',').next().unwrap_or("0");
        if let Ok(bw) = value.parse::<u64>() {
            return bw;
        }
    }
    0
}

/// Parse a variant playlist and return the list of segment URLs.
///
/// Variant playlist format:
/// ```text
/// #EXTM3U
/// #EXT-X-VERSION:3
/// #EXT-X-TARGETDURATION:10
/// #EXTINF:9.9,
/// https://cdn.example.com/seg001.ts
/// #EXTINF:9.9,
/// https://cdn.example.com/seg002.ts
/// #EXT-X-ENDLIST
/// ```
///
/// Returns `None` if no segments are found.
fn parse_variant_playlist(playlist: &str, base_url: &str) -> Option<Vec<String>> {
    let mut segments = Vec::new();

    for line in playlist.lines() {
        let line = line.trim();

        // Skip empty lines and tags
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // This is a segment URL
        let resolved = resolve_url(line, base_url);
        segments.push(resolved);
    }

    if segments.is_empty() {
        None
    } else {
        Some(segments)
    }
}

/// Resolve a potentially relative URL against a base URL.
///
/// HLS segment URLs can be:
/// - Absolute: `https://cdn.example.com/seg001.ts`
/// - Relative: `seg001.ts` or `subdir/seg001.ts`
///
/// For relative URLs, we resolve against the directory of the base URL.
fn resolve_url(url: &str, base_url: &str) -> String {
    // If the URL is already absolute, return as-is
    if url.starts_with("http://") || url.starts_with("https://") {
        return url.to_string();
    }

    // Parse the base URL to get the directory portion
    // e.g. "https://cdn.example.com/path/to/playlist.m3u8?token=abc"
    //   → base directory is "https://cdn.example.com/path/to/"
    let parsed = match url::Url::parse(base_url) {
        Ok(p) => p,
        Err(_) => return url.to_string(),
    };

    // Join the relative URL against the base URL.
    // url::Url::join handles both relative paths and query strings.
    match parsed.join(url) {
        Ok(resolved) => resolved.to_string(),
        Err(_) => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_cdn_speed_param() {
        assert_eq!(
            extract_cdn_speed_param("https://cdn.example.com/video.mp4?t=abc&sp=380&i=199.195"),
            Some(380)
        );
        assert_eq!(extract_cdn_speed_param("https://cdn.example.com/video.mp4?sp=380"), Some(380));
        assert_eq!(extract_cdn_speed_param("https://cdn.example.com/video.mp4?t=abc"), None);
        assert_eq!(extract_cdn_speed_param("https://cdn.example.com/video.mp4?sp=abc"), None);
    }

    #[test]
    fn test_parse_bandwidth_from_stream_inf() {
        assert_eq!(
            parse_bandwidth_from_stream_inf(
                "#EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720"
            ),
            2800000
        );
        assert_eq!(parse_bandwidth_from_stream_inf("#EXT-X-STREAM-INF:BANDWIDTH=800000"), 800000);
        assert_eq!(parse_bandwidth_from_stream_inf("#EXT-X-STREAM-INF:RESOLUTION=640x360"), 0);
    }

    #[test]
    fn test_parse_master_playlist() {
        let playlist = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\n\
            https://cdn.example.com/360p.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720\n\
            https://cdn.example.com/720p.m3u8\n";

        let result = parse_master_playlist(playlist, "https://cdn.example.com/master.m3u8");
        assert_eq!(result, Some("https://cdn.example.com/720p.m3u8".to_string()));
    }

    #[test]
    fn test_parse_master_playlist_relative_urls() {
        let playlist = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=800000\n\
            360p.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2800000\n\
            720p.m3u8\n";

        let result = parse_master_playlist(playlist, "https://cdn.example.com/path/master.m3u8");
        assert_eq!(result, Some("https://cdn.example.com/path/720p.m3u8".to_string()));
    }

    #[test]
    fn test_parse_variant_playlist() {
        let playlist = "#EXTM3U\n\
            #EXT-X-VERSION:3\n\
            #EXT-X-TARGETDURATION:10\n\
            #EXTINF:9.9,\n\
            https://cdn.example.com/seg001.ts\n\
            #EXTINF:9.9,\n\
            https://cdn.example.com/seg002.ts\n\
            #EXT-X-ENDLIST\n";

        let result = parse_variant_playlist(playlist, "https://cdn.example.com/playlist.m3u8");
        assert_eq!(
            result,
            Some(vec![
                "https://cdn.example.com/seg001.ts".to_string(),
                "https://cdn.example.com/seg002.ts".to_string(),
            ])
        );
    }

    #[test]
    fn test_parse_variant_playlist_relative_urls() {
        let playlist = "#EXTM3U\n\
            #EXT-X-VERSION:3\n\
            #EXT-X-TARGETDURATION:10\n\
            #EXTINF:9.9,\n\
            seg001.ts\n\
            #EXTINF:9.9,\n\
            seg002.ts\n\
            #EXT-X-ENDLIST\n";

        let result = parse_variant_playlist(playlist, "https://cdn.example.com/path/playlist.m3u8");
        assert_eq!(
            result,
            Some(vec![
                "https://cdn.example.com/path/seg001.ts".to_string(),
                "https://cdn.example.com/path/seg002.ts".to_string(),
            ])
        );
    }

    #[test]
    fn test_resolve_url_absolute() {
        assert_eq!(
            resolve_url("https://cdn.example.com/seg.ts", "https://other.com/playlist.m3u8"),
            "https://cdn.example.com/seg.ts"
        );
    }

    #[test]
    fn test_resolve_url_relative() {
        assert_eq!(
            resolve_url("seg.ts", "https://cdn.example.com/path/playlist.m3u8"),
            "https://cdn.example.com/path/seg.ts"
        );
    }

    #[test]
    fn test_resolve_url_relative_with_query() {
        assert_eq!(
            resolve_url("seg.ts", "https://cdn.example.com/path/playlist.m3u8?token=abc"),
            "https://cdn.example.com/path/seg.ts"
        );
    }

    #[test]
    fn test_is_hls_url() {
        assert!(is_hls_url("https://cdn.example.com/stream.m3u8"));
        assert!(is_hls_url("https://cdn.example.com/stream.m3u8?token=abc"));
        assert!(is_hls_url("https://cdn.example.com/stream.M3U8"));
        assert!(!is_hls_url("https://cdn.example.com/video.mp4"));
        assert!(!is_hls_url("https://cdn.example.com/video.mp4?token=abc"));
    }
}
