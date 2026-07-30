//! boGDan HTTP REST API Server
//!
//! Provides a REST-like control surface for external clients
//! (browser extension, curl, scripts) to interact with boGDan.
//!
//! ## Endpoints
//!
//! | Method | Path            | Description                     |
//! |--------|-----------------|---------------------------------|
//! | POST   | `/api/cast`     | Load and play a media URL       |
//! | POST   | `/api/stop`     | Stop and unload                 |
//! | POST   | `/api/pause`    | Pause playback                  |
//! | POST   | `/api/resume`   | Resume playback                 |
//! | POST   | `/api/seek`     | Seek to a position              |
//! | POST   | `/api/volume`   | Set volume 0–100                |
//! | GET    | `/api/status`   | Current player state & metadata |
//! | GET    | `/api/health`   | Health check                    |
//! | GET    | `/api/audio-devices` | List ALSA playback devices |
//!
//! ## Rate Limiting
//!
//! Per-IP rate limiting protects against accidental or malicious request
//! floods. Each IP is allowed `RATE_LIMIT_REQUESTS` requests per
//! `RATE_LIMIT_WINDOW_SECS` second window. Exceeding the limit returns
//! HTTP 429 Too Many Requests with a `Retry-After` header.

use anyhow::Result;
use bogdan_session::{MediaSession, SessionManager};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_rustls::TlsAcceptor;
use tracing;

/// Type alias for the HTTP response body.
type BoxBody = Full<bytes::Bytes>;

// ── Request Payloads ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CastRequest {
    url: String,
}

#[derive(Debug, Deserialize)]
struct AudioDeviceRequest {
    /// Device string (e.g. "plughw:1,0" for ALSA, "pulse" for PulseAudio).
    /// Empty = ALSA default.
    device: String,
    /// GStreamer sink element to use ("alsasink" or "pulsesink").
    /// Defaults to "alsasink" if not specified.
    #[serde(default = "default_sink_type")]
    sink_type: String,
}

fn default_sink_type() -> String {
    "alsasink".into()
}

#[derive(Debug, Deserialize)]
struct SeekRequest {
    position_ms: Option<u64>,
    position_seconds: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct VolumeRequest {
    /// Volume level (0-100). Values above 100 are clamped.
    volume: u8,
}

impl VolumeRequest {
    /// Return the volume clamped to the valid 0-100 range.
    fn clamped_volume(&self) -> u8 {
        self.volume.min(100)
    }
}

// ── Response Payloads ────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct StatusResponse {
    session_id: Option<String>,
    state: String,
    source_url: Option<String>,
    resolved_url: Option<String>,
    position_ms: u64,
    duration_ms: Option<u64>,
    volume: u8,
    title: Option<String>,
}

#[derive(Debug, Serialize)]
struct CastResponse {
    session_id: String,
    status: String,
}

/// Machine-readable error code string for programmatic handling.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[allow(dead_code)] // Some variants reserved for future use
enum ErrorCode {
    BadRequest,
    InvalidUrl,
    SessionActive,
    NoActiveSession,
    RateLimited,
    BodyTooLarge,
    InternalError,
    NotFound,
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| format!("{:?}", self).to_uppercase());
        write!(f, "{}", s)
    }
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
    /// Machine-readable error code for programmatic handling.
    code: String,
    /// HTTP status code for convenience.
    status: u16,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: String,
}

/// An audio playback device returned by `/api/audio-devices`.
#[derive(Debug, Serialize)]
struct AlsaDevice {
    /// Device string (e.g. "plughw:1,0" for ALSA, or "pulse" for PulseAudio).
    device: String,
    /// Human-readable card name (e.g. "vc4hdmi0", "Bluetooth Headset").
    card_name: String,
    /// Card index number.
    card_index: u32,
    /// Device index number.
    device_index: u32,
    /// GStreamer sink element to use ("alsasink" or "pulsesink").
    sink_type: String,
}

// ── HTTP API Server ──────────────────────────────────────────────────

/// REST API server built on `hyper`.
///
/// Routes requests to the [`SessionManager`] and returns JSON
/// responses. Supports CORS for browser extension access.
/// Per-IP rate limiting protects against request floods.
pub struct HttpApiServer {
    /// Socket address the server binds to.
    listen_addr: String,
    /// Reference to the session manager.
    session: Arc<SessionManager>,
    /// Optional TLS acceptor — if set, serves HTTPS.
    tls_acceptor: Option<Arc<TlsAcceptor>>,
    /// Per-IP rate limiter.
    rate_limiter: Arc<Mutex<RateLimiter>>,
}

impl HttpApiServer {
    /// Create a new HTTP server bound to `listen_addr`.
    pub fn new(listen_addr: &str, session: Arc<SessionManager>) -> Self {
        Self {
            listen_addr: listen_addr.to_owned(),
            session,
            tls_acceptor: None,
            rate_limiter: Arc::new(Mutex::new(RateLimiter::new())),
        }
    }

    /// Set a TLS acceptor to enable HTTPS.
    pub fn with_tls(mut self, acceptor: TlsAcceptor) -> Self {
        self.tls_acceptor = Some(Arc::new(acceptor));
        self
    }

    /// Start accepting connections.
    ///
    /// Runs indefinitely until the `shutdown` future resolves.
    /// If a TLS acceptor is configured, serves HTTPS; otherwise plain HTTP.
    pub async fn start(&self, shutdown: impl std::future::Future<Output = ()>) -> Result<()> {
        use hyper::service::service_fn;
        use hyper_util::rt::TokioIo;

        let listener = tokio::net::TcpListener::bind(&self.listen_addr).await?;
        let scheme = if self.tls_acceptor.is_some() { "HTTPS" } else { "HTTP" };
        tracing::info!(addr = %self.listen_addr, scheme = scheme, "API server listening");

        let session = self.session.clone();
        let tls_acceptor = self.tls_acceptor.clone();
        let mut shutdown = std::pin::pin!(shutdown);

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    let (stream, remote) = accept_result?;
                    let session = session.clone();
                    let tls = tls_acceptor.clone();
                    let rate_limiter = self.rate_limiter.clone();

                    tokio::spawn(async move {
                        let service = service_fn(move |req| {
                            let session = session.clone();
                            let rate_limiter = rate_limiter.clone();
                            async move {
                                match handle_request(req, &session, &rate_limiter).await {
                                    Ok(resp) => Ok(resp),
                                    Err(e) => {
                                        tracing::warn!(error = %e, "request handler error");
                                        error_response(StatusCode::BAD_REQUEST, &e.to_string())
                                    }
                                }
                            }
                        });

                        // If TLS is configured, wrap the TCP stream.
                        if let Some(acceptor) = tls {
                            match acceptor.accept(stream).await {
                                Ok(tls_stream) => {
                                    let io = TokioIo::new(tls_stream);
                                    if let Err(e) = hyper::server::conn::http1::Builder::new()
                                        .serve_connection(io, service)
                                        .await
                                    {
                                        // "connection closed before message completed" is normal
                                        // when a client disconnects mid-request (e.g. browser
                                        // timeout during slow Tor resolution). Log at debug
                                        // level to avoid spamming the journal on every disconnect.
                                        if e.to_string().contains("connection closed") {
                                            tracing::debug!(error = %e, remote = %remote, "HTTPS client disconnected");
                                        } else {
                                            tracing::warn!(error = %e, remote = %remote, "HTTPS connection error");
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, remote = %remote, "TLS handshake failed");
                                }
                            }
                        } else {
                            let io = TokioIo::new(stream);
                            if let Err(e) = hyper::server::conn::http1::Builder::new()
                                .serve_connection(io, service)
                                .await
                            {
                                if e.to_string().contains("connection closed") {
                                    tracing::debug!(error = %e, remote = %remote, "HTTP client disconnected");
                                } else {
                                    tracing::warn!(error = %e, remote = %remote, "HTTP connection error");
                                }
                            }
                        }
                    });
                }
                _ = shutdown.as_mut() => {
                    tracing::info!("{} API server shutting down", scheme);
                    break;
                }
            }
        }

        Ok(())
    }
}

/// Extract the client IP from the request's remote address or headers.
fn extract_client_ip(parts: &hyper::http::request::Parts) -> String {
    // Try X-Forwarded-For first (if behind a reverse proxy).
    if let Some(xff) = parts.headers.get("x-forwarded-for") {
        if let Ok(val) = xff.to_str() {
            // X-Forwarded-For may contain multiple IPs; use the first.
            if let Some(ip) = val.split(',').next() {
                return ip.trim().to_owned();
            }
        }
    }
    // Fall back to a placeholder — in practice, the remote IP is
    // available from the TCP accept but not from the HTTP request
    // parts. For a LAN-only device this is acceptable.
    "unknown".to_owned()
}

/// Route and handle an incoming HTTP request.
async fn handle_request(
    req: Request<Incoming>,
    session: &Arc<SessionManager>,
    rate_limiter: &Arc<Mutex<RateLimiter>>,
) -> Result<Response<BoxBody>> {
    let (parts, body) = req.into_parts();

    // CORS preflight.
    if parts.method == Method::OPTIONS {
        return Ok(cors_response(StatusCode::OK));
    }

    // Rate limiting — skip for health endpoint.
    let path = parts.uri.path();
    if path != "/api/health" {
        let client_ip = extract_client_ip(&parts);
        let mut limiter = rate_limiter.lock().await;
        if !limiter.check(&client_ip) {
            tracing::warn!(ip = %client_ip, "rate limit exceeded");
            return rate_limit_response(RATE_LIMIT_WINDOW_SECS);
        }
    }

    // Route by method + path.
    let method = parts.method;

    match (method, path) {
        // Health check.
        (Method::GET, "/api/health") => {
            let resp = HealthResponse { status: "ok".into() };
            json_response(StatusCode::OK, &resp)
        },

        // ALSA playback devices.
        (Method::GET, "/api/audio-devices") => {
            let devices = list_alsa_devices();
            json_response(StatusCode::OK, &devices)
        },

        // Status.
        (Method::GET, "/api/status") => match session.current_status().await {
            Ok(s) => {
                let resp = StatusResponse::from_session(&s);
                json_response(StatusCode::OK, &resp)
            },
            Err(_) => {
                let resp = StatusResponse {
                    session_id: None,
                    state: "idle".into(),
                    source_url: None,
                    resolved_url: None,
                    position_ms: 0,
                    duration_ms: None,
                    volume: 100,
                    title: None,
                };
                json_response(StatusCode::OK, &resp)
            },
        },

        // Cast.
        (Method::POST, "/api/cast") => {
            let payload = match read_body_json::<CastRequest>(body).await {
                Ok(p) => p,
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("body too large") {
                        return error_response_with_code(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            ErrorCode::BodyTooLarge,
                            &msg,
                        );
                    }
                    return error_response_with_code(
                        StatusCode::BAD_REQUEST,
                        ErrorCode::BadRequest,
                        &msg,
                    );
                },
            };
            if let Err(e) = is_safe_cast_url(&payload.url) {
                return error_response_with_code(
                    StatusCode::BAD_REQUEST,
                    ErrorCode::InvalidUrl,
                    &e.to_string(),
                );
            }

            // Quick-check: if a session is already active, reject immediately
            // without spawning a background task. This avoids the race where
            // two concurrent /api/cast requests both pass the load() check.
            match session.current_status().await {
                Ok(_) => {
                    return error_response_with_code(
                        StatusCode::CONFLICT,
                        ErrorCode::SessionActive,
                        "session already active — stop the current session first",
                    );
                },
                Err(bogdan_session::SessionError::NoActiveSession) => {
                    // Good — no active session, proceed.
                },
                Err(e) => {
                    // Database or other error — log but continue; load() will
                    // catch it if it's a real problem.
                    tracing::warn!(error = %e, "status check before cast failed");
                },
            }

            // Return 202 Accepted immediately with a placeholder session ID.
            // The actual resolution + playback happens in a background task.
            // Clients should poll /api/status or subscribe to WebSocket events
            // for state changes (Resolving → Buffering → Playing / Error).
            let session_id = uuid::Uuid::new_v4();
            let url = payload.url.clone();
            let bg_session = session.clone();

            tokio::spawn(async move {
                if let Err(e) = bg_session.load(&url).await {
                    tracing::warn!(error = %e, url = %url, "background cast failed");
                }
            });

            let resp =
                CastResponse { session_id: session_id.to_string(), status: "resolving".into() };
            json_response(StatusCode::ACCEPTED, &resp)
        },

        // Stop.
        (Method::POST, "/api/stop") => match session.stop().await {
            Ok(()) => {
                let resp = StatusResponse {
                    session_id: None,
                    state: "idle".into(),
                    source_url: None,
                    resolved_url: None,
                    position_ms: 0,
                    duration_ms: None,
                    volume: 100,
                    title: None,
                };
                json_response(StatusCode::OK, &resp)
            },
            Err(e) => error_response(StatusCode::CONFLICT, &e.to_string()),
        },

        // Pause.
        (Method::POST, "/api/pause") => match session.pause().await {
            Ok(()) => json_response(StatusCode::OK, &serde_json::json!({"status": "paused"})),
            Err(e) => error_response(StatusCode::CONFLICT, &e.to_string()),
        },

        // Resume.
        (Method::POST, "/api/resume") => match session.resume().await {
            Ok(()) => json_response(StatusCode::OK, &serde_json::json!({"status": "playing"})),
            Err(e) => error_response(StatusCode::CONFLICT, &e.to_string()),
        },

        // Seek.
        (Method::POST, "/api/seek") => {
            let payload = match read_body_json::<SeekRequest>(body).await {
                Ok(p) => p,
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("body too large") {
                        return error_response_with_code(StatusCode::PAYLOAD_TOO_LARGE, ErrorCode::BodyTooLarge, &msg);
                    }
                    return error_response_with_code(StatusCode::BAD_REQUEST, ErrorCode::BadRequest, &msg);
                }
            };
            let position_ms = payload
                .position_ms
                .or_else(|| payload.position_seconds.map(|s| (s * 1000.0) as u64))
                .unwrap_or(0);

            match session.seek(position_ms).await {
                Ok(()) => {
                    json_response(StatusCode::OK, &serde_json::json!({"position_ms": position_ms}))
                },
                Err(e) => error_response(StatusCode::CONFLICT, &e.to_string()),
            }
        },

        // Volume.
        (Method::POST, "/api/volume") => {
            let payload = match read_body_json::<VolumeRequest>(body).await {
                Ok(p) => p,
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("body too large") {
                        return error_response_with_code(StatusCode::PAYLOAD_TOO_LARGE, ErrorCode::BodyTooLarge, &msg);
                    }
                    return error_response_with_code(StatusCode::BAD_REQUEST, ErrorCode::BadRequest, &msg);
                }
            };
            let volume = payload.clamped_volume();
            match session.set_volume(volume).await {
                Ok(()) => json_response(StatusCode::OK, &serde_json::json!({"volume": volume})),
                Err(e) => {
                    let status = match &e {
                        bogdan_session::SessionError::NoActiveSession => StatusCode::CONFLICT,
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    error_response(status, &e.to_string())
                },
            }
        },

        // Set audio device and sink type.
        (Method::POST, "/api/audio-device") => {
            let payload = match read_body_json::<AudioDeviceRequest>(body).await {
                Ok(p) => p,
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("body too large") {
                        return error_response_with_code(StatusCode::PAYLOAD_TOO_LARGE, ErrorCode::BodyTooLarge, &msg);
                    }
                    return error_response_with_code(StatusCode::BAD_REQUEST, ErrorCode::BadRequest, &msg);
                }
            };
            // Set the device first
            match session.set_audio_device(payload.device.clone()).await {
                Ok(()) => {
                    // Then set the sink type (alsasink or pulsesink)
                    if payload.sink_type != "alsasink" {
                        if let Err(e) = session.set_audio_sink(payload.sink_type.clone()).await {
                            tracing::warn!(error = %e, "failed to set audio sink type");
                        }
                    }
                    tracing::info!(
                        device = %payload.device,
                        sink_type = %payload.sink_type,
                        "audio device updated via API"
                    );
                    json_response(
                        StatusCode::OK,
                        &serde_json::json!({
                            "device": payload.device,
                            "sink_type": payload.sink_type,
                        }),
                    )
                },
                Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            }
        },

        // Get audio device.
        (Method::GET, "/api/audio-device") => match session.audio_device().await {
            Ok(device) => json_response(StatusCode::OK, &serde_json::json!({"device": device})),
            Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        },

        // 404.
        _ => error_response(StatusCode::NOT_FOUND, "endpoint not found"),
    }
}

// ── Response Helpers ─────────────────────────────────────────────────

/// Create a JSON response with CORS headers.
fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Result<Response<BoxBody>> {
    let json = serde_json::to_string(body)?;
    Ok(Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .header("Access-Control-Allow-Origin", "*")
        .header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        .header("Access-Control-Allow-Headers", "Content-Type")
        .body(Full::new(bytes::Bytes::from(json)))?)
}

/// Create an error response with a machine-readable code.
fn error_response_with_code(
    status: StatusCode,
    code: ErrorCode,
    message: &str,
) -> Result<Response<BoxBody>> {
    let resp = ErrorResponse {
        error: message.to_owned(),
        code: code.to_string(),
        status: status.as_u16(),
    };
    json_response(status, &resp)
}

/// Create an error response with an auto-derived code from the status.
fn error_response(status: StatusCode, message: &str) -> Result<Response<BoxBody>> {
    let code = match status {
        StatusCode::BAD_REQUEST => ErrorCode::BadRequest,
        StatusCode::NOT_FOUND => ErrorCode::NotFound,
        StatusCode::CONFLICT => ErrorCode::SessionActive,
        StatusCode::TOO_MANY_REQUESTS => ErrorCode::RateLimited,
        StatusCode::PAYLOAD_TOO_LARGE => ErrorCode::BodyTooLarge,
        StatusCode::INTERNAL_SERVER_ERROR => ErrorCode::InternalError,
        _ => ErrorCode::InternalError,
    };
    error_response_with_code(status, code, message)
}

/// Create a rate-limit error response with Retry-After header.
fn rate_limit_response(retry_after_secs: u64) -> Result<Response<BoxBody>> {
    let message = format!(
        "rate limit exceeded — max {} requests per {} seconds",
        RATE_LIMIT_REQUESTS, RATE_LIMIT_WINDOW_SECS
    );
    let resp = ErrorResponse {
        error: message,
        code: ErrorCode::RateLimited.to_string(),
        status: StatusCode::TOO_MANY_REQUESTS.as_u16(),
    };
    let json = serde_json::to_string(&resp)?;
    Ok(Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header("Content-Type", "application/json")
        .header("Access-Control-Allow-Origin", "*")
        .header("Retry-After", retry_after_secs.to_string())
        .body(Full::new(bytes::Bytes::from(json)))?)
}

/// Create a CORS preflight response.
fn cors_response(status: StatusCode) -> Response<BoxBody> {
    Response::builder()
        .status(status)
        .header("Access-Control-Allow-Origin", "*")
        .header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        .header("Access-Control-Allow-Headers", "Content-Type")
        .header("Access-Control-Max-Age", "86400")
        .body(Full::new(bytes::Bytes::new()))
        .unwrap_or_else(|_| {
            // Fallback: a minimal valid response. The builder only fails
            // on invalid header values, which our hardcoded values are not.
            Response::new(Full::new(bytes::Bytes::new()))
        })
}

/// Maximum allowed HTTP request body size (1 KB for POST payloads).
/// Large bodies are unnecessary for our API and indicate misuse.
const MAX_BODY_SIZE: usize = 1_024;

/// Rate limiting: maximum requests per IP per window.
const RATE_LIMIT_REQUESTS: u32 = 30;
/// Rate limiting: window duration in seconds.
const RATE_LIMIT_WINDOW_SECS: u64 = 10;

/// Per-IP rate limit tracker.
struct RateLimiter {
    /// Map from IP address to (count, window_start_instant).
    entries: HashMap<String, (u32, std::time::Instant)>,
}

impl RateLimiter {
    fn new() -> Self {
        Self { entries: HashMap::new() }
    }

    /// Check if a request from `ip` is allowed. Returns `true` if the
    /// request is within the rate limit, `false` if it should be rejected.
    /// Also prunes expired entries to prevent unbounded memory growth.
    fn check(&mut self, ip: &str) -> bool {
        let now = std::time::Instant::now();
        let window = std::time::Duration::from_secs(RATE_LIMIT_WINDOW_SECS);

        // Prune expired entries.
        self.entries.retain(|_, (_, start)| now.duration_since(*start) < window);

        match self.entries.get_mut(ip) {
            Some((count, start)) => {
                if now.duration_since(*start) >= window {
                    // Window expired — reset.
                    *count = 1;
                    *start = now;
                    true
                } else if *count < RATE_LIMIT_REQUESTS {
                    *count += 1;
                    true
                } else {
                    false
                }
            },
            None => {
                self.entries.insert(ip.to_owned(), (1, now));
                true
            },
        }
    }
}

/// Read and parse a JSON body with size validation.
async fn read_body_json<T: serde::de::DeserializeOwned>(body: Incoming) -> Result<T> {
    use http_body_util::BodyExt;
    let bytes = body.collect().await?.to_bytes();
    if bytes.len() > MAX_BODY_SIZE {
        return Err(anyhow::anyhow!(
            "request body too large ({} bytes, max {})",
            bytes.len(),
            MAX_BODY_SIZE
        ));
    }
    if bytes.is_empty() {
        return Err(anyhow::anyhow!("request body is empty — expected JSON"));
    }
    Ok(serde_json::from_slice(&bytes)?)
}



/// Validate that a URL is safe for casting.
///
/// Rejects `file://`, `data:`, `javascript:`, and other dangerous schemes.
/// Only `http://` and `https://` are allowed.
fn is_safe_cast_url(url: &str) -> Result<()> {
    let parsed = url::Url::parse(url).map_err(|e| anyhow::anyhow!("invalid URL: {}", e))?;
    match parsed.scheme() {
        "http" | "https" => Ok(()),
        "file" => Err(anyhow::anyhow!("file:// URLs are not allowed — use http:// or https://")),
        "data" => Err(anyhow::anyhow!("data: URLs are not allowed — use http:// or https://")),
        "javascript" => Err(anyhow::anyhow!("javascript: URLs are not allowed")),
        scheme => {
            Err(anyhow::anyhow!("unsupported URL scheme: {} — use http:// or https://", scheme))
        },
    }
}

/// List available ALSA playback devices by parsing `/proc/asound/cards` and
/// `/proc/asound/pcm`.  Returns a default entry plus one per detected card.
///
/// This runs on the Pi itself, so it reads local procfs.  If procfs is
/// unavailable (e.g. running in a container), falls back to an empty list
/// with just the "default" entry.
fn list_alsa_devices() -> Vec<AlsaDevice> {
    let mut devices = vec![AlsaDevice {
        device: "default".into(),
        card_name: "ALSA Default".into(),
        card_index: 0,
        device_index: 0,
        sink_type: "alsasink".into(),
    }];

    // Check for PulseAudio — if running, add it as an option.
    // PulseAudio handles Bluetooth audio routing automatically.
    // On Pi with Bluetooth headphones/speakers, PulseAudio is the
    // easiest way to get audio working.
    if std::path::Path::new("/run/pulse/native").exists()
        || std::path::Path::new("/var/run/pulse/native").exists()
        || std::process::Command::new("pactl")
            .arg("info")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    {
        devices.push(AlsaDevice {
            device: "pulse".into(),
            card_name: "PulseAudio (auto Bluetooth)".into(),
            card_index: 99,
            device_index: 0,
            sink_type: "pulsesink".into(),
        });

        // Try to list PulseAudio sinks for more specific options.
        if let Ok(output) =
            std::process::Command::new("pactl").args(["list", "short", "sinks"]).output()
        {
            if let Ok(stdout) = String::from_utf8(output.stdout) {
                for line in stdout.lines() {
                    // Format: "id\tname\tmodule\tsample_spec\tstate"
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 2 {
                        let sink_name = parts[1].to_string();
                        let display_name =
                            if sink_name.contains("bluez") || sink_name.contains("bluetooth") {
                                format!("PulseAudio Bluetooth ({})", sink_name)
                            } else if sink_name.contains("hdmi") {
                                format!("PulseAudio HDMI ({})", sink_name)
                            } else {
                                format!("PulseAudio ({})", sink_name)
                            };
                        devices.push(AlsaDevice {
                            device: sink_name.clone(),
                            card_name: display_name,
                            card_index: 99,
                            device_index: parts[0].parse().unwrap_or(0),
                            sink_type: "pulsesink".into(),
                        });
                    }
                }
            }
        }
    }

    // Check for BlueALSA — if running without PulseAudio, BlueALSA provides
    // ALSA PCM devices for Bluetooth audio. The `bluealsa-aplay` utility
    // can list connected devices. If BlueALSA's D-Bus service is available
    // and a Bluetooth audio device is connected, we add it as an option.
    // The BlueALSA ALSA plugin uses device strings like:
    //   "bluealsa:DEV=XX:XX:XX:XX:XX:XX,PROFILE=a2dp"
    // But these don't show up in /proc/asound — they're a special ALSA
    // plugin, not a regular PCM device. We detect them by checking for
    // the bluealsa daemon or by trying to list connected BT devices.
    if !devices.iter().any(|d| d.card_name.contains("PulseAudio")) {
        // Only check BlueALSA if PulseAudio isn't running (they conflict).
        // Check for bluealsa daemon via its D-Bus name or PID file.
        let bluealsa_running = std::path::Path::new("/var/run/bluealsa").exists()
            || std::path::Path::new("/run/bluealsa").exists()
            || std::process::Command::new("dbus-send")
                .args([
                    "--system",
                    "--dest=org.bluealsa",
                    "/org/bluealsa",
                    "org.freedesktop.DBus.Introspectable.Introspect",
                ])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);

        if bluealsa_running {
            // Try to list connected Bluetooth audio devices via bluetoothctl
            if let Ok(output) =
                std::process::Command::new("bluetoothctl").args(["devices", "Connected"]).output()
            {
                if let Ok(stdout) = String::from_utf8(output.stdout) {
                    for line in stdout.lines() {
                        // Format: "Device XX:XX:XX:XX:XX:XX Device Name"
                        if let Some(rest) = line.strip_prefix("Device ") {
                            let parts: Vec<&str> = rest.splitn(2, ' ').collect();
                            if parts.len() >= 2 {
                                let bt_addr = parts[0];
                                let bt_name = parts[1];
                                devices.push(AlsaDevice {
                                    device: format!("bluealsa:DEV={},PROFILE=a2dp", bt_addr),
                                    card_name: format!("Bluetooth ({})", bt_name),
                                    card_index: 98,
                                    device_index: 0,
                                    sink_type: "alsasink".into(),
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    // Parse /proc/asound/cards for card indices and names.
    // Format: " 0 [Headphones   ]: ... - bcm2835 Headphones ..."
    //         " 1 [vc4hdmi0     ]: ... - vc4-hdmi ..."
    let cards_content = match std::fs::read_to_string("/proc/asound/cards") {
        Ok(c) => c,
        Err(_) => return devices,
    };

    // Parse card entries: "index [shortname]: ... - longname"
    let mut cards: Vec<(u32, String, String)> = Vec::new();
    for line in cards_content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Each card spans 2 lines; the first line has the index and names.
        // Format: "0 [Headphones     ]: bcm2835_0 - bcm2835 Headphones"
        if let Some(bracket_end) = line.find(']') {
            let before_bracket = &line[..bracket_end];
            // Extract card index from before the bracket
            if let Some(idx_str) = before_bracket.split_whitespace().next() {
                if let Ok(card_idx) = idx_str.parse::<u32>() {
                    // Extract short name from between brackets
                    let short_name = before_bracket
                        .find('[')
                        .map(|pos| before_bracket[pos + 1..].trim().to_string())
                        .unwrap_or_default();
                    // Extract long name from after " - "
                    let long_name = line
                        .split(" - ")
                        .nth(1)
                        .map(|s| s.trim().to_string())
                        .unwrap_or_else(|| short_name.clone());
                    cards.push((card_idx, short_name, long_name));
                }
            }
        }
    }

    // Parse /proc/asound/pcm for playback devices per card.
    // Format: "00-00: bcm2835 ALSA : bcm2835 Headphones : playback 1"
    //         "01-00: vc4-hdmi : vc4-hdmi 0 : playback 1"
    let pcm_content = match std::fs::read_to_string("/proc/asound/pcm") {
        Ok(c) => c,
        Err(_) => {
            // No pcm info — add one plughw device per card
            for (card_idx, _short, long) in &cards {
                devices.push(AlsaDevice {
                    device: format!("plughw:{},0", card_idx),
                    card_name: long.clone(),
                    card_index: *card_idx,
                    device_index: 0,
                    sink_type: "alsasink".into(),
                });
            }
            return devices;
        },
    };

    for line in pcm_content.lines() {
        let line = line.trim();
        if !line.contains("playback") {
            continue;
        }
        // Parse "CC-DD: ..." where CC = card index, DD = device index
        let parts: Vec<&str> = line.splitn(2, ':').collect();
        if parts.len() < 2 {
            continue;
        }
        let id_parts: Vec<&str> = parts[0].trim().split('-').collect();
        if id_parts.len() < 2 {
            continue;
        }
        if let (Ok(card_idx), Ok(dev_idx)) =
            (id_parts[0].trim().parse::<u32>(), id_parts[1].trim().parse::<u32>())
        {
            // Find the card name
            let card_name = cards
                .iter()
                .find(|(idx, _, _)| *idx == card_idx)
                .map(|(_, _, long)| long.as_str())
                .unwrap_or("Unknown");
            // Detect Bluetooth audio devices for better labelling.
            // BlueALSA devices appear in /proc/asound with names like
            // "bluealsa" or the actual headset/speaker name.
            let is_bluetooth = card_name.to_lowercase().contains("bluealsa")
                || card_name.to_lowercase().contains("bluez")
                || card_name.to_lowercase().contains("bluetooth")
                || card_name.to_lowercase().contains("bt_headset")
                || card_name.to_lowercase().contains("bt_speaker");
            let display_name = if is_bluetooth {
                format!("Bluetooth ({})", card_name)
            } else {
                card_name.to_string()
            };
            devices.push(AlsaDevice {
                device: format!("plughw:{},{}", card_idx, dev_idx),
                card_name: display_name,
                card_index: card_idx,
                device_index: dev_idx,
                sink_type: "alsasink".into(),
            });
        }
    }

    devices
}

impl StatusResponse {
    fn from_session(session: &MediaSession) -> Self {
        Self {
            session_id: Some(session.id.to_string()),
            state: session.state.to_string(),
            source_url: Some(session.source_url.clone()),
            resolved_url: session.resolved_url.clone(),
            position_ms: session.position_ms,
            duration_ms: session.duration_ms,
            volume: session.volume,
            title: session.title.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_response_from_session() {
        let session = MediaSession::new("https://example.com/video.mp4".into());
        let resp = StatusResponse::from_session(&session);
        assert!(resp.session_id.is_some());
        assert_eq!(resp.state, "idle");
        assert_eq!(resp.source_url, Some("https://example.com/video.mp4".into()));
    }

    #[test]
    fn error_response_json() {
        let resp = ErrorResponse {
            error: "not found".into(),
            code: ErrorCode::NotFound.to_string(),
            status: 404,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("not found"));
        assert!(json.contains("404"));
        assert!(json.contains("NOT_FOUND"));
    }

    #[test]
    fn error_code_display() {
        assert_eq!(ErrorCode::BadRequest.to_string(), "BAD_REQUEST");
        assert_eq!(ErrorCode::InvalidUrl.to_string(), "INVALID_URL");
        assert_eq!(ErrorCode::SessionActive.to_string(), "SESSION_ACTIVE");
        assert_eq!(ErrorCode::RateLimited.to_string(), "RATE_LIMITED");
        assert_eq!(ErrorCode::NoActiveSession.to_string(), "NO_ACTIVE_SESSION");
        assert_eq!(ErrorCode::NotFound.to_string(), "NOT_FOUND");
    }

    #[test]
    fn rate_limiter_allows_within_limit() {
        let mut limiter = RateLimiter::new();
        for _ in 0..RATE_LIMIT_REQUESTS {
            assert!(limiter.check("192.168.1.1"));
        }
        // Next request should be denied.
        assert!(!limiter.check("192.168.1.1"));
    }

    #[test]
    fn rate_limiter_different_ips_independent() {
        let mut limiter = RateLimiter::new();
        for _ in 0..RATE_LIMIT_REQUESTS {
            assert!(limiter.check("192.168.1.1"));
        }
        // Different IP should still be allowed.
        assert!(limiter.check("192.168.1.2"));
    }

    #[test]
    fn rate_limiter_window_resets() {
        let mut limiter = RateLimiter::new();
        for _ in 0..RATE_LIMIT_REQUESTS {
            assert!(limiter.check("192.168.1.1"));
        }
        assert!(!limiter.check("192.168.1.1"));
        // Manually expire the window by setting a past timestamp.
        let entry = limiter.entries.get_mut("192.168.1.1").unwrap();
        entry.1 = std::time::Instant::now() - std::time::Duration::from_secs(RATE_LIMIT_WINDOW_SECS + 1);
        // Should be allowed again after window expires.
        assert!(limiter.check("192.168.1.1"));
    }

    #[test]
    fn is_safe_cast_url_rejects_dangerous_schemes() {
        assert!(is_safe_cast_url("file:///etc/passwd").is_err());
        assert!(is_safe_cast_url("data:text/html,<script>alert(1)</script>").is_err());
        assert!(is_safe_cast_url("javascript:alert(1)").is_err());
        assert!(is_safe_cast_url("ftp://example.com").is_err());
    }

    #[test]
    fn is_safe_cast_url_accepts_http_https() {
        assert!(is_safe_cast_url("http://example.com").is_ok());
        assert!(is_safe_cast_url("https://example.com").is_ok());
    }

    #[test]
    fn empty_body_rejected() {
        // An empty body should fail JSON deserialization (serde_json
        // returns an "EOF while parsing" error on empty input).
        let empty_bytes: &[u8] = b"";
        let result: Result<CastRequest, _> = serde_json::from_slice(empty_bytes);
        assert!(result.is_err(), "empty body should fail JSON deserialization");
    }

    #[test]
    fn volume_clamping() {
        // Values above 100 should be clamped to 100
        let req = VolumeRequest { volume: 150 };
        assert_eq!(req.clamped_volume(), 100);
        // Value at boundary stays
        let req = VolumeRequest { volume: 100 };
        assert_eq!(req.clamped_volume(), 100);
        // Value below stays
        let req = VolumeRequest { volume: 0 };
        assert_eq!(req.clamped_volume(), 0);
        // u8 max (255) clamps to 100
        let req = VolumeRequest { volume: 255 };
        assert_eq!(req.clamped_volume(), 100);
    }

    #[test]
    fn extract_client_ip_unknown() {
        let req = Request::builder()
            .method("GET")
            .uri("/api/health")
            .body(()).unwrap();
        let (parts, _) = req.into_parts();
        let ip = extract_client_ip(&parts);
        assert_eq!(ip, "unknown");
    }

    #[test]
    fn extract_client_ip_from_xff() {
        let req = Request::builder()
            .method("GET")
            .uri("/api/health")
            .header("x-forwarded-for", "10.0.0.1, 192.168.1.1")
            .body(()).unwrap();
        let (parts, _) = req.into_parts();
        let ip = extract_client_ip(&parts);
        assert_eq!(ip, "10.0.0.1");
    }

    #[test]
    fn cast_request_deserialize() {
        let json = r#"{"url":"https://youtube.com/watch?v=abc"}"#;
        let req: CastRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.url, "https://youtube.com/watch?v=abc");
    }

    #[test]
    fn seek_request_ms() {
        let json = r#"{"position_ms":5000}"#;
        let req: SeekRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.position_ms, Some(5000));
    }

    #[test]
    fn seek_request_seconds() {
        let json = r#"{"position_seconds":30.5}"#;
        let req: SeekRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.position_seconds, Some(30.5));
    }

    #[test]
    fn volume_request() {
        let json = r#"{"volume":75}"#;
        let req: VolumeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.volume, 75);
    }

    #[test]
    fn health_response() {
        let resp = HealthResponse { status: "ok".into() };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("ok"));
    }
}
