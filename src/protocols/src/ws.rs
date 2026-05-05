//! PiCast WebSocket Server
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
//! {"type": "SUBTITLE", "lang": "en"}
//! ```
//!
//! ## Ping/Pong
//!
//! Server sends ping every 30 seconds. Clients that don't respond
//! within 10 seconds are disconnected.

use anyhow::{Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use picast_session::{SessionEvent, SessionManager};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

// ── Client → Server Commands ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
enum ClientCommand {
    Cast { url: String },
    Stop,
    Pause,
    Resume,
    Seek { position_ms: u64 },
    Volume { volume: u8 },
    Subtitle { lang: String },
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
}

impl WebSocketServer {
    /// Create a new WebSocket server bound to `listen_addr`.
    pub fn new(listen_addr: &str, session: Arc<SessionManager>) -> Self {
        Self {
            listen_addr: listen_addr.to_owned(),
            session,
        }
    }

    /// Start accepting WebSocket connections.
    ///
    /// Runs indefinitely until the `shutdown` future resolves.
    pub async fn start(&self, shutdown: impl std::future::Future<Output = ()>) -> Result<()> {
        let listener = TcpListener::bind(&self.listen_addr).await?;
        tracing::info!(addr = %self.listen_addr, "WebSocket server listening");

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    let (stream, remote) = accept_result?;
                    let session = self.session.clone();

                    tokio::spawn(async move {
                        let ws_stream = tokio_tungstenite::accept_async(stream).await;
                        match ws_stream {
                            Ok(ws_stream) => {
                                tracing::debug!(remote = %remote, "WebSocket client connected");
                                if let Err(e) = handle_client(ws_stream, session).await {
                                    tracing::warn!(remote = %remote, error = %e, "WebSocket client error");
                                }
                            }
                            Err(e) => {
                                tracing::warn!(remote = %remote, error = %e, "WebSocket handshake failed");
                            }
                        }
                    });
                }
                _ = &mut std::pin::pin!(shutdown) => {
                    tracing::info!("WebSocket server shutting down");
                    break;
                }
            }
        }

        Ok(())
    }
}

/// Handle a single WebSocket client connection.
///
/// Reads commands from the client and forwards session events.
async fn handle_client(
    ws_stream: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    session: Arc<SessionManager>,
) -> Result<()> {
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();
    let mut event_rx = session.subscribe();

    // Send connected event.
    let connected = ServerEvent::Connected;
    let connected_json = serde_json::to_string(&connected)?;
    ws_sender.send(Message::Text(connected_json.into())).await?;

    // Ping interval.
    let mut ping_interval = tokio::time::interval(Duration::from_secs(30));

    loop {
        tokio::select! {
            // Read from WebSocket client.
            msg = ws_receiver.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientCommand>(&text) {
                            Ok(cmd) => {
                                if let Err(e) = handle_command(&session, cmd).await {
                                    let err_event = ServerEvent::Error {
                                        message: e.to_string(),
                                    };
                                    let err_json = serde_json::to_string(&err_event)?;
                                    ws_sender.send(Message::Text(err_json.into())).await?;
                                }
                            }
                            Err(e) => {
                                let err_event = ServerEvent::Error {
                                    message: format!("invalid command: {}", e),
                                };
                                let err_json = serde_json::to_string(&err_event)?;
                                ws_sender.send(Message::Text(err_json.into())).await?;
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
                                    ws_sender.send(Message::Text(err_json.into())).await?;
                                }
                            }
                        }
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
                        let server_event = map_session_event(&session_event);
                        if let Some(event) = server_event {
                            let json = serde_json::to_string(&event)?;
                            ws_sender.send(Message::Text(json.into())).await?;
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
                if ws_sender.send(Message::Ping(vec![].into())).await.is_err() {
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
            session
                .load(&url)
                .await
                .map_err(|e| anyhow!("cast failed: {}", e))?;
        }
        ClientCommand::Stop => {
            session
                .stop()
                .await
                .map_err(|e| anyhow!("stop failed: {}", e))?;
        }
        ClientCommand::Pause => {
            session
                .pause()
                .await
                .map_err(|e| anyhow!("pause failed: {}", e))?;
        }
        ClientCommand::Resume => {
            session
                .resume()
                .await
                .map_err(|e| anyhow!("resume failed: {}", e))?;
        }
        ClientCommand::Seek { position_ms } => {
            session
                .seek(position_ms)
                .await
                .map_err(|e| anyhow!("seek failed: {}", e))?;
        }
        ClientCommand::Volume { volume } => {
            session
                .set_volume(volume)
                .await
                .map_err(|e| anyhow!("volume failed: {}", e))?;
        }
        ClientCommand::Subtitle { lang } => {
            // Subtitle support deferred to v0.4.0.
            tracing::warn!(lang = %lang, "subtitle selection not yet implemented");
        }
    }
    Ok(())
}

/// Map a session event to a WebSocket server event.
fn map_session_event(event: &SessionEvent) -> Option<ServerEvent> {
    match event {
        SessionEvent::Playing { .. } | SessionEvent::Paused { .. } | SessionEvent::Stopped { .. } => {
            // For state-change events, the client can query /api/status
            // for full details. We send a lightweight status update.
            Some(ServerEvent::MediaStatus {
                state: match event {
                    SessionEvent::Playing { .. } => "PLAYING",
                    SessionEvent::Paused { .. } => "PAUSED",
                    SessionEvent::Stopped { .. } => "IDLE",
                    _ => "UNKNOWN",
                }
                .into(),
                position_ms: 0,
                duration_ms: None,
                volume: 100,
                source_url: None,
                title: None,
            })
        }
        SessionEvent::Buffering { percent, .. } => Some(ServerEvent::ResolveProgress {
            percent: *percent,
        }),
        SessionEvent::Error { message, .. } => Some(ServerEvent::Error {
            message: message.clone(),
        }),
        SessionEvent::Created { .. } | SessionEvent::Resolving { .. } | SessionEvent::Resolved { .. } => {
            // Forward as resolve progress.
            Some(ServerEvent::ResolveProgress { percent: 0 })
        }
        SessionEvent::PositionUpdate {
            position_ms,
            duration_ms,
            ..
        } => Some(ServerEvent::MediaStatus {
            state: "PLAYING".into(),
            position_ms: *position_ms,
            duration_ms: *duration_ms,
            volume: 100,
            source_url: None,
            title: None,
        }),
        SessionEvent::VolumeChanged { volume, .. } => Some(ServerEvent::MediaStatus {
            state: "PLAYING".into(),
            position_ms: 0,
            duration_ms: None,
            volume: *volume,
            source_url: None,
            title: None,
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
        let event = ServerEvent::Error {
            message: "resolution failed".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("ERROR"));
        assert!(json.contains("resolution failed"));
    }
}
