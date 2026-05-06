//! PiCast HTTP REST API Server
//!
//! Provides a REST-like control surface for external clients
//! (browser extension, curl, scripts) to interact with PiCast.
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

use anyhow::Result;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use picast_session::{MediaSession, SessionManager};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
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

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
    code: u16,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: String,
}

// ── HTTP API Server ──────────────────────────────────────────────────

/// REST API server built on `hyper`.
///
/// Routes requests to the [`SessionManager`] and returns JSON
/// responses. Supports CORS for browser extension access.
pub struct HttpApiServer {
    /// Socket address the server binds to.
    listen_addr: String,
    /// Reference to the session manager.
    session: Arc<SessionManager>,
    /// Optional TLS acceptor — if set, serves HTTPS.
    tls_acceptor: Option<Arc<TlsAcceptor>>,
}

impl HttpApiServer {
    /// Create a new HTTP server bound to `listen_addr`.
    pub fn new(listen_addr: &str, session: Arc<SessionManager>) -> Self {
        Self { listen_addr: listen_addr.to_owned(), session, tls_acceptor: None }
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

                    tokio::spawn(async move {
                        let service = service_fn(move |req| {
                            let session = session.clone();
                            async move {
                                match handle_request(req, &session).await {
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
                                        tracing::error!(error = %e, remote = %remote, "HTTPS connection error");
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
                                tracing::error!(error = %e, remote = %remote, "HTTP connection error");
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

/// Route and handle an incoming HTTP request.
async fn handle_request(
    req: Request<Incoming>,
    session: &Arc<SessionManager>,
) -> Result<Response<BoxBody>> {
    let (parts, body) = req.into_parts();

    // CORS preflight.
    if parts.method == Method::OPTIONS {
        return Ok(cors_response(StatusCode::OK));
    }

    // Route by method + path.
    let path = parts.uri.path();
    let method = parts.method;

    match (method, path) {
        // Health check.
        (Method::GET, "/api/health") => {
            let resp = HealthResponse { status: "ok".into() };
            json_response(StatusCode::OK, &resp)
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
            let payload = read_body_json::<CastRequest>(body).await?;
            if let Err(e) = is_safe_cast_url(&payload.url) {
                return error_response(StatusCode::BAD_REQUEST, &e.to_string());
            }
            match session.load(&payload.url).await {
                Ok(id) => {
                    let resp =
                        CastResponse { session_id: id.to_string(), status: "resolving".into() };
                    json_response(StatusCode::ACCEPTED, &resp)
                },
                Err(e) => {
                    let (code, msg) = match &e {
                        picast_session::SessionError::AlreadyActive => {
                            (StatusCode::CONFLICT, e.to_string())
                        },
                        picast_session::SessionError::ResolutionFailed(_) => {
                            (StatusCode::UNPROCESSABLE_ENTITY, e.to_string())
                        },
                        _ => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
                    };
                    error_response(code, &msg)
                },
            }
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
            let payload = read_body_json::<SeekRequest>(body).await?;
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
            let payload = read_body_json::<VolumeRequest>(body).await?;
            let volume = payload.clamped_volume();
            match session.set_volume(volume).await {
                Ok(()) => {
                    json_response(StatusCode::OK, &serde_json::json!({"volume": volume}))
                },
                Err(e) => {
                    let status = match &e {
                        picast_session::SessionError::NoActiveSession => StatusCode::CONFLICT,
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    error_response(status, &e.to_string())
                },
            }
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

/// Create an error response.
fn error_response(status: StatusCode, message: &str) -> Result<Response<BoxBody>> {
    let resp = ErrorResponse { error: message.to_owned(), code: status.as_u16() };
    json_response(status, &resp)
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
        .unwrap()
}

/// Maximum allowed HTTP request body size (1 MB).
const MAX_BODY_SIZE: usize = 1_048_576;

/// Read and parse a JSON body with size validation.
async fn read_body_json<T: serde::de::DeserializeOwned>(body: Incoming) -> Result<T> {
    use http_body_util::BodyExt;
    let bytes = body.collect().await?.to_bytes();
    if bytes.len() > MAX_BODY_SIZE {
        return Err(anyhow::anyhow!("request body too large ({} bytes, max {})", bytes.len(), MAX_BODY_SIZE));
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
        scheme => Err(anyhow::anyhow!("unsupported URL scheme: {} — use http:// or https://", scheme)),
    }
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
        let resp = ErrorResponse { error: "not found".into(), code: 404 };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("not found"));
        assert!(json.contains("404"));
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
