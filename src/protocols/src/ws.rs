//! boGDan WebSocket Server
//!
//! Low-latency bidirectional event stream for real-time UIs
//! (browser extension, web dashboard). Clients subscribe to
//! player-state events and send control commands through a
//! single long-lived socket.
//!
//! ## Protocol
//!
//! All messages are JSON with a `type` field.
//!
//! ### Server → Client (events)
//!
//! ```json
//! {"type": "MEDIA_STATUS", "state": "PLAYING", "position_ms": 5000, ...}
//! {"type": "RESOLVE_PROGRESS", "percent": 50}
//! {"type": "ERROR", "message": "resolution failed"}
//! ```
//!
//! ### Client → Server (commands)
//!
//! ```json
//! {"type": "CAST", "url": "https://youtube.com/..."}
//! {"type": "STOP"}
//! {"type": "PAUSE"}
//! {"type": "RESUME"}
//! {"type": "SEEK", "position_ms": 30000}
//! {"type": "VOLUME", "volume": 75}
//! ```
//!
//! ## Ping/Pong
//!
//! Server sends ping every 30 seconds. Clients that don't respond
//! within 10 seconds are disconnected.
//!
//! ## Connection Limit
//!
//! A maximum of 32 concurrent WebSocket clients are allowed.
//! Connections beyond this limit are rejected with a 429-style
//! error event before the socket is closed.

use anyhow::{anyhow, Result};
use bogdan_session::{MediaSession, SessionEvent, SessionManager};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, Semaphore};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::Message;

/// Maximum number of concurrent WebSocket clients.
const MAX_CONNECTIONS: usize = 32;

// ── Client → Server Commands ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
enum ClientCommand {
    Cast {
        url: String,
    },
    Stop,
    Pause,
    Resume,
    Seek {
        position_ms: u64,
    },
    Volume {
        volume: u8,
    },
    /// Application-level keep-alive. The client sends PING and the
    /// server responds with a PONG event. This is distinct from
    /// the WebSocket protocol-level ping/pong frames — some clients
    /// (especially browser extensions) can't send WS-level pings
    /// and need an application-level equivalent.
    Ping,
}

// ── Server → Client Events ───────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
enum ServerEvent {
    MediaStatus {
        state: String,
        position_ms: u64,
        duration_ms: Option<u64>,
        volume: u8,
        source_url: Option<String>,
        title: Option<String>,
    },
    ResolveProgress {
        percent: u8,
    },
    Error {
        message: String,
    },
    Connected,
    /// Response to a client PING — application-level keep-alive.
    Pong,
}

// ── WebSocket Server ─────────────────────────────────────────────────

/// WebSocket server for real-time, bidirectional communication.
///
/// Clients connect to `ws://<pi>:8586/ws` and receive player-state
/// events in real time while sending control commands.
pub struct WebSocketServer {
    /// Socket address the server binds to.
    listen_addr: String,
    /// Reference to the session manager.
    session: Arc<SessionManager>,
    /// Connection limiter — at most `MAX_CONNECTIONS` concurrent clients.
    connection_limit: Arc<Semaphore>,
    /// Optional TLS acceptor — if set, serves WSS.
    tls_acceptor: Option<Arc<TlsAcceptor>>,
}

impl WebSocketServer {
    /// Create a new WebSocket server bound to `listen_addr`.
    pub fn new(listen_addr: &str, session: Arc<SessionManager>) -> Self {
        Self {
            listen_addr: listen_addr.to_owned(),
            session,
            connection_limit: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            tls_acceptor: None,
        }
    }

    /// Set a TLS acceptor to enable WSS.
    pub fn with_tls(mut self, acceptor: TlsAcceptor) -> Self {
        self.tls_acceptor = Some(Arc::new(acceptor));
        self
    }

    /// Start accepting WebSocket connections.
    ///
    /// Runs indefinitely until the `shutdown` future resolves.
    /// If a TLS acceptor is configured, serves WSS; otherwise plain WS.
    pub async fn start(&self, shutdown: impl std::future::Future<Output = ()>) -> Result<()> {
        let listener = TcpListener::bind(&self.listen_addr).await?;
        let scheme = if self.tls_acceptor.is_some() { "WSS" } else { "WS" };
        tracing::info!(addr = %self.listen_addr, scheme = scheme, "WebSocket server listening");

        let mut shutdown = std::pin::pin!(shutdown);

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    let (stream, remote) = accept_result?;
                    let session = self.session.clone();
                    let connection_limit = self.connection_limit.clone();
                    let tls = self.tls_acceptor.clone();

                    tokio::spawn(async move {
                        // Try to acquire a connection permit before upgrading.
                        let permit = match connection_limit.try_acquire() {
                            Ok(permit) => permit,
                            Err(_) => {
                                tracing::warn!(
                                    remote = %remote,
                                    "WebSocket connection rejected — limit of {} reached",
                                    MAX_CONNECTIONS
                                );
                                // Do the handshake just to send an error, then close.
                                let ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
                                    max_message_size: Some(1_048_576),
                                    max_frame_size: Some(1_048_576),
                                    ..Default::default()
                                };
                                if let Ok(mut ws_err) = accept_ws(tls.as_deref(), stream, Some(ws_config)).await {
                                    let err = ServerEvent::Error {
                                        message: format!("too many connections (max {})", MAX_CONNECTIONS),
                                    };
                                    if let Ok(json) = serde_json::to_string(&err) {
                                        let _ = ws_err.send(Message::text(json)).await;
                                    }
                                    let _ = ws_err.send(Message::Close(None)).await;
                                }
                                return;
                            }
                        };

                        let ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
                            max_message_size: Some(1_048_576), // 1 MB
                            max_frame_size: Some(1_048_576),    // 1 MB
                            ..Default::default()
                        };
                        match accept_ws(tls.as_deref(), stream, Some(ws_config)).await {
                            Ok(ws_stream) => {
                                tracing::debug!(remote = %remote, "WebSocket client connected");
                                if let Err(e) = handle_client(ws_stream, session, permit).await {
                                    tracing::warn!(remote = %remote, error = %e, "WebSocket client error");
                                }
                            }
                            Err(e) => {
                                tracing::warn!(remote = %remote, error = %e, "WebSocket handshake failed");
                            }
                        }
                    });
                }
                _ = shutdown.as_mut() => {
                    tracing::info!("{} WebSocket server shutting down", scheme);
                    break;
                }
            }
        }

        Ok(())
    }
}

async fn accept_ws(
    tls_acceptor: Option<&TlsAcceptor>,
    stream: tokio::net::TcpStream,
    config: Option<tokio_tungstenite::tungstenite::protocol::WebSocketConfig>,
) -> Result<WsStream> {
    if let Some(acceptor) = tls_acceptor {
        accept_wss_stream(stream, acceptor, config).await
    } else {
        accept_ws_stream_plain(stream, config).await
    }
}

/// Accept a WebSocket connection over TLS.
async fn accept_wss_stream(
    stream: tokio::net::TcpStream,
    tls_acceptor: &TlsAcceptor,
    config: Option<tokio_tungstenite::tungstenite::protocol::WebSocketConfig>,
) -> Result<WsStream> {
    let tls_stream =
        tls_acceptor.accept(stream).await.map_err(|e| anyhow!("TLS handshake failed: {}", e))?;
    let ws_stream = tokio_tungstenite::accept_async_with_config(tls_stream, config).await?;
    Ok(WsStream::Tls(Box::new(ws_stream)))
}

/// Accept a plain WebSocket connection (no TLS).
async fn accept_ws_stream_plain(
    stream: tokio::net::TcpStream,
    config: Option<tokio_tungstenite::tungstenite::protocol::WebSocketConfig>,
) -> Result<WsStream> {
    let ws_stream = tokio_tungstenite::accept_async_with_config(stream, config).await?;
    Ok(WsStream::Plain(Box::new(ws_stream)))
}

/// Type-erased WebSocket stream that supports both plain WS and WSS.
enum WsStream {
    Tls(
        Box<
            tokio_tungstenite::WebSocketStream<
                tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
            >,
        >,
    ),
    Plain(Box<tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>>),
}

impl WsStream {
    async fn next(&mut self) -> Option<Result<Message, tungstenite::Error>> {
        match self {
            WsStream::Tls(s) => s.next().await,
            WsStream::Plain(s) => s.next().await,
        }
    }

    async fn send(&mut self, msg: Message) -> Result<(), tungstenite::Error> {
        match self {
            WsStream::Tls(s) => s.send(msg).await,
            WsStream::Plain(s) => s.send(msg).await,
        }
    }
}

/// Handle a single WebSocket client connection.
///
/// Reads commands from the client and forwards session events.
/// The `_permit` parameter holds the semaphore permit for the
/// connection's lifetime — dropping it releases the slot.
async fn handle_client(
    ws_stream: WsStream,
    session: Arc<SessionManager>,
    _permit: tokio::sync::SemaphorePermit<'_>,
) -> Result<()> {
    let mut ws = ws_stream;
    let mut event_rx = session.subscribe();

    // Send connected event.
    let connected = ServerEvent::Connected;
    let connected_json = serde_json::to_string(&connected)?;
    ws.send(Message::text(connected_json)).await?;

    // Ping interval.
    let mut ping_interval = tokio::time::interval(Duration::from_secs(30));

    loop {
        tokio::select! {
            // Read from WebSocket client.
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        // tungstenite 0.24: Text wraps Utf8Bytes which derefs to &str
                        match serde_json::from_str::<ClientCommand>(&text) {
                            Ok(ClientCommand::Ping) => {
                                // Application-level ping — respond with Pong immediately.
                                let pong_json = serde_json::to_string(&ServerEvent::Pong)?;
                                ws.send(Message::text(pong_json)).await?;
                            },
                            Ok(cmd) => {
                                if let Err(e) = handle_command(&session, cmd).await {
                                    let err_event = ServerEvent::Error {
                                        message: e.to_string(),
                                    };
                                    let err_json = serde_json::to_string(&err_event)?;
                                    ws.send(Message::text(err_json)).await?;
                                }
                            }
                            Err(e) => {
                                let err_event = ServerEvent::Error {
                                    message: format!("invalid command: {}", e),
                                };
                                let err_json = serde_json::to_string(&err_event)?;
                                ws.send(Message::text(err_json)).await?;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(_))) => {
                        // Auto-pong by tungstenite.
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::debug!("WebSocket client disconnected");
                        break;
                    }
                    Some(Ok(Message::Binary(data))) => {
                        // Try to parse binary as UTF-8 JSON.
                        if let Ok(text) = std::str::from_utf8(&data) {
                            if let Ok(cmd) = serde_json::from_str::<ClientCommand>(text) {
                                if let Err(e) = handle_command(&session, cmd).await {
                                    let err_event = ServerEvent::Error { message: e.to_string() };
                                    let err_json = serde_json::to_string(&err_event)?;
                                    ws.send(Message::text(err_json)).await?;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {
                        // Pong received — auto-handled by tungstenite.
                    }
                    Some(Ok(Message::Frame(_))) => {}
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "WebSocket receive error");
                        break;
                    }
                }
            }

            // Receive session events and forward to client.
            event = event_rx.recv() => {
                match event {
                    Ok(session_event) => {
                        // For state-change events that produce MediaStatus,
                        // query the session manager for the current snapshot
                        // so we include real position, volume, source, and title.
                        let current_session = match session_event {
                            SessionEvent::Playing { .. }
                            | SessionEvent::Paused { .. }
                            | SessionEvent::Stopped { .. }
                            | SessionEvent::VolumeChanged { .. }
                            | SessionEvent::Seeking { .. }
                            | SessionEvent::PositionUpdate { .. } => {
                                session.current_status().await.ok()
                            },
                            _ => None,
                        };

                        let server_event = map_session_event(&session_event, current_session.as_ref());
                        if let Some(event) = server_event {
                            let json = serde_json::to_string(&event)?;
                            ws.send(Message::text(json)).await?;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        tracing::warn!(count = count, "event stream lagged — client may be slow");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }

            // Ping.
            _ = ping_interval.tick() => {
                if ws.send(Message::Ping(vec![])).await.is_err() {
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Handle a client command by dispatching to the session manager.
async fn handle_command(session: &SessionManager, cmd: ClientCommand) -> Result<()> {
    match cmd {
        ClientCommand::Cast { url } => {
            // Validate URL scheme before casting.
            match url::Url::parse(&url) {
                Ok(parsed) => match parsed.scheme() {
                    "http" | "https" => {},
                    "file" => return Err(anyhow!("file:// URLs are not allowed")),
                    "data" => return Err(anyhow!("data: URLs are not allowed")),
                    scheme => return Err(anyhow!("unsupported URL scheme: {}", scheme)),
                },
                Err(e) => return Err(anyhow!("invalid URL: {}", e)),
            }
            session.load(&url).await.map_err(|e| anyhow!("cast failed: {}", e))?;
        },
        ClientCommand::Stop => {
            session.stop().await.map_err(|e| anyhow!("stop failed: {}", e))?;
        },
        ClientCommand::Pause => {
            session.pause().await.map_err(|e| anyhow!("pause failed: {}", e))?;
        },
        ClientCommand::Resume => {
            session.resume().await.map_err(|e| anyhow!("resume failed: {}", e))?;
        },
        ClientCommand::Seek { position_ms } => {
            session.seek(position_ms).await.map_err(|e| anyhow!("seek failed: {}", e))?;
        },
        ClientCommand::Volume { volume } => {
            let clamped = volume.min(100);
            session.set_volume(clamped).await.map_err(|e| anyhow!("volume failed: {}", e))?;
        },
        ClientCommand::Ping => {
            // Handled in handle_client directly (sends Pong event).
            // This arm is never reached because handle_client intercepts
            // Ping before calling handle_command.
        },
    }
    Ok(())
}

/// Map a session event to a WebSocket server event.
///
/// When `current_session` is `Some`, its fields (position, volume,
/// source URL, title) are used to populate the `MediaStatus` payload
/// instead of hardcoded placeholder values. This gives clients an
/// accurate snapshot of the player state on every event.
fn map_session_event(
    event: &SessionEvent,
    current_session: Option<&MediaSession>,
) -> Option<ServerEvent> {
    match event {
        SessionEvent::Playing { .. }
        | SessionEvent::Paused { .. }
        | SessionEvent::Stopped { .. } => {
            let state_str = match event {
                SessionEvent::Playing { .. } => "playing",
                SessionEvent::Paused { .. } => "paused",
                SessionEvent::Stopped { .. } => "idle",
                _ => "unknown",
            };

            if let Some(s) = current_session {
                Some(ServerEvent::MediaStatus {
                    state: state_str.into(),
                    position_ms: s.position_ms,
                    duration_ms: s.duration_ms,
                    volume: s.volume,
                    source_url: Some(s.source_url.clone()),
                    title: s.title.clone(),
                })
            } else {
                // No active session — send minimal status.
                Some(ServerEvent::MediaStatus {
                    state: state_str.into(),
                    position_ms: 0,
                    duration_ms: None,
                    volume: 100,
                    source_url: None,
                    title: None,
                })
            }
        },
        SessionEvent::Buffering { percent, .. } => {
            Some(ServerEvent::ResolveProgress { percent: *percent })
        },
        SessionEvent::Error { message, .. } => {
            Some(ServerEvent::Error { message: message.clone() })
        },
        SessionEvent::Created { .. }
        | SessionEvent::Resolving { .. }
        | SessionEvent::Resolved { .. } => {
            // Forward as resolve progress.
            Some(ServerEvent::ResolveProgress { percent: 0 })
        },
        SessionEvent::PositionUpdate { position_ms, duration_ms, .. } => {
            if let Some(s) = current_session {
                Some(ServerEvent::MediaStatus {
                    state: "playing".into(),
                    position_ms: *position_ms,
                    duration_ms: duration_ms.or(s.duration_ms),
                    volume: s.volume,
                    source_url: Some(s.source_url.clone()),
                    title: s.title.clone(),
                })
            } else {
                Some(ServerEvent::MediaStatus {
                    state: "playing".into(),
                    position_ms: *position_ms,
                    duration_ms: *duration_ms,
                    volume: 100,
                    source_url: None,
                    title: None,
                })
            }
        },
        SessionEvent::VolumeChanged { volume, .. } => {
            if let Some(s) = current_session {
                Some(ServerEvent::MediaStatus {
                    state: s.state.to_string(),
                    position_ms: s.position_ms,
                    duration_ms: s.duration_ms,
                    volume: *volume,
                    source_url: Some(s.source_url.clone()),
                    title: s.title.clone(),
                })
            } else {
                Some(ServerEvent::MediaStatus {
                    state: "playing".into(),
                    position_ms: 0,
                    duration_ms: None,
                    volume: *volume,
                    source_url: None,
                    title: None,
                })
            }
        },
        SessionEvent::Seeking { position_ms, .. } => {
            if let Some(s) = current_session {
                Some(ServerEvent::MediaStatus {
                    state: "seeking".into(),
                    position_ms: *position_ms,
                    duration_ms: s.duration_ms,
                    volume: s.volume,
                    source_url: Some(s.source_url.clone()),
                    title: s.title.clone(),
                })
            } else {
                Some(ServerEvent::MediaStatus {
                    state: "seeking".into(),
                    position_ms: *position_ms,
                    duration_ms: None,
                    volume: 100,
                    source_url: None,
                    title: None,
                })
            }
        },
        SessionEvent::CdnForbidden { .. } => Some(ServerEvent::Error {
            message: "CDN rejected request (403 Forbidden) — Tor exit IP mismatch, re-resolving…"
                .into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_command_cast() {
        let json = r#"{"type":"CAST","url":"https://youtube.com/watch?v=abc"}"#;
        let cmd: ClientCommand = serde_json::from_str(json).unwrap();
        match cmd {
            ClientCommand::Cast { url } => assert_eq!(url, "https://youtube.com/watch?v=abc"),
            _ => panic!("expected CAST"),
        }
    }

    #[test]
    fn client_command_stop() {
        let json = r#"{"type":"STOP"}"#;
        let cmd: ClientCommand = serde_json::from_str(json).unwrap();
        assert!(matches!(cmd, ClientCommand::Stop));
    }

    #[test]
    fn client_command_seek() {
        let json = r#"{"type":"SEEK","position_ms":30000}"#;
        let cmd: ClientCommand = serde_json::from_str(json).unwrap();
        match cmd {
            ClientCommand::Seek { position_ms } => assert_eq!(position_ms, 30000),
            _ => panic!("expected SEEK"),
        }
    }

    #[test]
    fn client_command_volume() {
        let json = r#"{"type":"VOLUME","volume":75}"#;
        let cmd: ClientCommand = serde_json::from_str(json).unwrap();
        match cmd {
            ClientCommand::Volume { volume } => assert_eq!(volume, 75),
            _ => panic!("expected VOLUME"),
        }
    }

    #[test]
    fn server_event_connected() {
        let event = ServerEvent::Connected;
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("CONNECTED"));
    }

    #[test]
    fn server_event_media_status() {
        let event = ServerEvent::MediaStatus {
            state: "PLAYING".into(),
            position_ms: 5000,
            duration_ms: Some(300000),
            volume: 80,
            source_url: Some("https://example.com".into()),
            title: Some("Test".into()),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("MEDIA_STATUS"));
        assert!(json.contains("PLAYING"));
    }

    #[test]
    fn server_event_error() {
        let event = ServerEvent::Error { message: "resolution failed".into() };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("ERROR"));
        assert!(json.contains("resolution failed"));
    }

    #[test]
    fn map_session_event_playing_with_session() {
        use bogdan_session::PlayerState;

        let mut session = MediaSession::new("https://example.com/video".into());
        session.state = PlayerState::Playing;
        session.position_ms = 5000;
        session.duration_ms = Some(300000);
        session.volume = 75;
        session.title = Some("Test Video".into());

        let event = SessionEvent::Playing { id: session.id };
        let result = map_session_event(&event, Some(&session));
        assert!(result.is_some());
        if let Some(ServerEvent::MediaStatus {
            state,
            position_ms,
            volume,
            source_url,
            title,
            ..
        }) = result
        {
            assert_eq!(state, "playing");
            assert_eq!(position_ms, 5000);
            assert_eq!(volume, 75);
            assert_eq!(source_url, Some("https://example.com/video".into()));
            assert_eq!(title, Some("Test Video".into()));
        } else {
            panic!("expected MediaStatus");
        }
    }

    #[test]
    fn map_session_event_playing_without_session() {
        let event = SessionEvent::Playing { id: uuid::Uuid::new_v4() };
        let result = map_session_event(&event, None);
        assert!(result.is_some());
        if let Some(ServerEvent::MediaStatus {
            state,
            position_ms,
            volume,
            source_url,
            title,
            ..
        }) = result
        {
            assert_eq!(state, "playing");
            assert_eq!(position_ms, 0);
            assert_eq!(volume, 100);
            assert_eq!(source_url, None);
            assert_eq!(title, None);
        } else {
            panic!("expected MediaStatus");
        }
    }
}
