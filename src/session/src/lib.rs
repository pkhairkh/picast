//! PiCast Session Management
//!
//! The session layer sits at the centre of the PiCast architecture.
//! It owns the SQLite-backed [`MediaSession`] store and coordinates the
//! four subsystems through the trait interfaces defined in [`interfaces`].
//!
//! ```text
//!                  ┌──────────────┐
//!  Protocols ────►│ SessionMgr   │◄─── HTTP / WS / DLNA
//!                  │  (SQLite)    │
//!                  └──┬─┬─┬─┬────┘
//!                     │ │ │ │
//!          ┌──────────┘ │ │ └──────────┐
//!          ▼            ▼ ▼            ▼
//!      Resolver    Playback  Display    Tor
//! ```
//!
//! ## State Machine
//!
//! ```text
//!  IDLE ──load()──► RESOLVING ──resolve ok──► BUFFERING ──buffer full──► PLAYING
//!    ▲                                                          │   │
//!    │                                                          │   │
//!    └──stop()──────────────────────────────────────────────────┘   │
//!    ┌──pause()─────────────────────────────────────────────────────┘
//!    │
//!    ▼
//!  PAUSED ──resume()──► PLAYING
//! ```
//!
//! ## Thread Safety
//!
//! `SessionManager` is `Send + Sync`. All mutable state is protected
//! by a `tokio::sync::Mutex`. The session can be safely wrapped in
//! `Arc` and shared across protocol handlers.

pub mod interfaces;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{Mutex, broadcast};
use uuid::Uuid;

// ── Errors ───────────────────────────────────────────────────────────

/// Errors that can originate from the session layer.
#[derive(Error, Debug)]
pub enum SessionError {
    /// No active session was found for the requested operation.
    #[error("no active session")]
    NoActiveSession,

    /// An operation was attempted while a session is already active.
    #[error("session already active — stop the current session first")]
    AlreadyActive,

    /// The requested session ID does not exist.
    #[error("session not found: {0}")]
    NotFound(Uuid),

    /// A database operation failed.
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    /// A subsystem returned an error.
    #[error("subsystem error: {0}")]
    Subsystem(String),

    /// URL resolution failed.
    #[error("resolution failed: {0}")]
    ResolutionFailed(String),

    /// Playback engine error.
    #[error("playback error: {0}")]
    PlaybackError(String),
}

// ── Player State ─────────────────────────────────────────────────────

/// Possible states of the media player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlayerState {
    /// No media loaded; idle and ready for a new load command.
    Idle,
    /// URL is being resolved through the resolver.
    Resolving,
    /// Media has been resolved and is buffering.
    Buffering,
    /// Media has been loaded but is not yet playing.
    Loaded,
    /// Actively decoding and rendering media.
    Playing,
    /// Playback is paused; can be resumed.
    Paused,
    /// An unrecoverable error occurred during playback.
    Error,
}

impl std::fmt::Display for PlayerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlayerState::Idle => write!(f, "IDLE"),
            PlayerState::Resolving => write!(f, "RESOLVING"),
            PlayerState::Buffering => write!(f, "BUFFERING"),
            PlayerState::Loaded => write!(f, "LOADED"),
            PlayerState::Playing => write!(f, "PLAYING"),
            PlayerState::Paused => write!(f, "PAUSED"),
            PlayerState::Error => write!(f, "ERROR"),
        }
    }
}

// ── Session Event ────────────────────────────────────────────────────

/// Events emitted by the session manager when state changes.
///
/// Protocol layers (HTTP API, WebSocket, DLNA) subscribe to these
/// events to push updates to connected clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// A new session was created.
    Created { id: Uuid, url: String },
    /// URL resolution is in progress.
    Resolving { id: Uuid },
    /// Resolution succeeded; playback is starting.
    Resolved {
        id: Uuid,
        direct_url: String,
        title: Option<String>,
    },
    /// Playback has begun.
    Playing { id: Uuid },
    /// Playback is paused.
    Paused { id: Uuid },
    /// Playback has stopped.
    Stopped { id: Uuid },
    /// An error occurred.
    Error { id: Uuid, message: String },
    /// Buffering progress.
    Buffering { id: Uuid, percent: u8 },
    /// Position update.
    PositionUpdate {
        id: Uuid,
        position_ms: u64,
        duration_ms: Option<u64>,
    },
    /// Volume changed.
    VolumeChanged { id: Uuid, volume: u8 },
}

// ── Media Session ────────────────────────────────────────────────────

/// Persistent representation of a media playback session.
///
/// Each time a client loads a URL, a new `MediaSession` is created and
/// stored in SQLite so that state survives restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaSession {
    /// Unique session identifier.
    pub id: Uuid,
    /// The original URL the client requested to cast.
    pub source_url: String,
    /// The resolved direct media URL (after redirect / Tor resolution).
    pub resolved_url: Option<String>,
    /// Current player state.
    pub state: PlayerState,
    /// Playback position in milliseconds from the start.
    pub position_ms: u64,
    /// Total duration in milliseconds (if known).
    pub duration_ms: Option<u64>,
    /// Volume level 0–100.
    pub volume: u8,
    /// Media title (from resolver).
    pub title: Option<String>,
    /// When the session was first created.
    pub created_at: DateTime<Utc>,
    /// When the session was last updated.
    pub updated_at: DateTime<Utc>,
}

impl MediaSession {
    /// Create a brand-new session for the given `source_url`.
    pub fn new(source_url: String) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            source_url,
            resolved_url: None,
            state: PlayerState::Idle,
            position_ms: 0,
            duration_ms: None,
            volume: 100,
            title: None,
            created_at: now,
            updated_at: now,
        }
    }
}

// ── Session State (internal) ─────────────────────────────────────────

/// Internal session state, tracking the active session and its
/// lifecycle. Only one session can be active at a time (single-player).
struct SessionState {
    /// The active session, if any.
    active: Option<MediaSession>,
}

impl SessionState {
    fn new() -> Self {
        Self { active: None }
    }
}

// ── Session Manager ──────────────────────────────────────────────────

/// Central coordinator that owns the session database and dispatches
/// commands to the resolver, playback engine, display, and Tor subsystems.
///
/// All public methods are async and thread-safe; the manager is intended
/// to be wrapped in an `Arc` and shared across protocol handlers.
pub struct SessionManager {
    /// SQLite connection handle (wrapped for Send + Sync).
    db: Arc<Mutex<rusqlite::Connection>>,
    /// Internal mutable state.
    state: Arc<Mutex<SessionState>>,
    /// Event broadcast channel.
    event_tx: broadcast::Sender<SessionEvent>,
    /// Resolver subsystem.
    resolver: Arc<dyn interfaces::ResolverTrait>,
    /// Playback subsystem.
    playback: Arc<dyn interfaces::PlaybackTrait>,
    /// Display subsystem.
    display: Arc<dyn interfaces::DisplayTrait>,
    /// Tor subsystem.
    tor: Arc<dyn interfaces::TorTrait>,
}

impl SessionManager {
    /// Open (or create) the session database and return a ready manager.
    ///
    /// The database file is created at `db_path` if it does not exist.
    /// All four subsystems must be provided as trait objects.
    pub fn new(
        db_path: &str,
        resolver: Arc<dyn interfaces::ResolverTrait>,
        playback: Arc<dyn interfaces::PlaybackTrait>,
        display: Arc<dyn interfaces::DisplayTrait>,
        tor: Arc<dyn interfaces::TorTrait>,
    ) -> Result<Self, SessionError> {
        let conn = rusqlite::Connection::open(db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                 id            TEXT PRIMARY KEY,
                 source_url    TEXT NOT NULL,
                 resolved_url  TEXT,
                 state         TEXT NOT NULL DEFAULT 'IDLE',
                 position_ms   INTEGER NOT NULL DEFAULT 0,
                 duration_ms   INTEGER,
                 volume        INTEGER NOT NULL DEFAULT 100,
                 title         TEXT,
                 created_at    TEXT NOT NULL,
                 updated_at    TEXT NOT NULL
             );",
        )?;

        let (event_tx, _) = broadcast::channel(128);

        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
            state: Arc::new(Mutex::new(SessionState::new())),
            event_tx,
            resolver,
            playback,
            display,
            tor,
        })
    }

    /// Load a new media URL: resolve it, create a session, and start playback.
    ///
    /// The workflow is:
    /// 1. Check no session is already active.
    /// 2. Create a `MediaSession` in `Resolving` state.
    /// 3. Resolve the URL via the resolver.
    /// 4. Start playback via the playback engine.
    /// 5. Transition to `Playing` state.
    pub async fn load(&self, url: &str) -> Result<Uuid, SessionError> {
        let mut state = self.state.lock().await;

        // Enforce single-session constraint.
        if state.active.is_some() {
            return Err(SessionError::AlreadyActive);
        }

        // Create new session.
        let mut session = MediaSession::new(url.to_owned());
        session.state = PlayerState::Resolving;
        let session_id = session.id;

        tracing::info!(
            session_id = %session_id,
            url = url,
            "loading new session"
        );

        // Persist to database.
        self.persist_session(&session).await?;

        // Broadcast event.
        let _ = self.event_tx.send(SessionEvent::Created {
            id: session_id,
            url: url.to_owned(),
        });
        let _ = self.event_tx.send(SessionEvent::Resolving { id: session_id });

        // Store session in state.
        state.active = Some(session);
        drop(state);

        // Resolve the URL.
        let direct_url = self
            .resolver
            .resolve(url)
            .await
            .map_err(|e| SessionError::ResolutionFailed(e.to_string()))?;

        // Update session with resolved URL.
        {
            let mut state = self.state.lock().await;
            if let Some(ref mut session) = state.active {
                session.resolved_url = Some(direct_url.clone());
                session.state = PlayerState::Buffering;
                session.updated_at = Utc::now();
                self.persist_session(session).await?;
            }
        }

        let _ = self.event_tx.send(SessionEvent::Resolved {
            id: session_id,
            direct_url: direct_url.clone(),
            title: None, // TODO: get title from resolver
        });

        // Ensure Tor is running for the playback proxy.
        self.tor
            .ensure_running()
            .await
            .map_err(|e| SessionError::Subsystem(format!("Tor: {}", e)))?;

        // Acquire the display.
        self.display
            .acquire()
            .await
            .map_err(|e| SessionError::Subsystem(format!("Display: {}", e)))?;

        // Start playback through Tor.
        let socks_addr = self.tor.socks_addr();
        let isolation = self.tor.isolation_username(
            url::Url::parse(url)
                .ok()
                .and_then(|u| u.host_str().map(|h| h.to_owned()))
                .unwrap_or_default()
                .as_str(),
        );

        self.playback
            .play(&direct_url, &socks_addr, &isolation)
            .await
            .map_err(|e| SessionError::PlaybackError(e.to_string()))?;

        // Transition to Playing.
        {
            let mut state = self.state.lock().await;
            if let Some(ref mut session) = state.active {
                session.state = PlayerState::Playing;
                session.updated_at = Utc::now();
                self.persist_session(session).await?;
            }
        }

        let _ = self.event_tx.send(SessionEvent::Playing { id: session_id });

        Ok(session_id)
    }

    /// Pause playback on the current session.
    pub async fn pause(&self) -> Result<(), SessionError> {
        let mut state = self.state.lock().await;
        let session = state.active.as_mut().ok_or(SessionError::NoActiveSession)?;

        if session.state != PlayerState::Playing {
            return Err(SessionError::Subsystem(
                "cannot pause — not playing".into(),
            ));
        }

        self.playback
            .pause()
            .await
            .map_err(|e| SessionError::PlaybackError(e.to_string()))?;

        session.state = PlayerState::Paused;
        session.updated_at = Utc::now();
        self.persist_session(session).await?;

        let id = session.id;
        let _ = self.event_tx.send(SessionEvent::Paused { id });

        Ok(())
    }

    /// Resume playback on the current session.
    pub async fn resume(&self) -> Result<(), SessionError> {
        let mut state = self.state.lock().await;
        let session = state.active.as_mut().ok_or(SessionError::NoActiveSession)?;

        if session.state != PlayerState::Paused {
            return Err(SessionError::Subsystem(
                "cannot resume — not paused".into(),
            ));
        }

        self.playback
            .resume()
            .await
            .map_err(|e| SessionError::PlaybackError(e.to_string()))?;

        session.state = PlayerState::Playing;
        session.updated_at = Utc::now();
        self.persist_session(session).await?;

        let id = session.id;
        let _ = self.event_tx.send(SessionEvent::Playing { id });

        Ok(())
    }

    /// Stop playback and destroy the session.
    pub async fn stop(&self) -> Result<(), SessionError> {
        let mut state = self.state.lock().await;
        let session = state.active.take().ok_or(SessionError::NoActiveSession)?;

        self.playback
            .stop()
            .await
            .map_err(|e| SessionError::PlaybackError(e.to_string()))?;

        self.display
            .release()
            .await
            .map_err(|e| SessionError::Subsystem(format!("Display: {}", e)))?;

        // Delete from database.
        self.delete_session(session.id).await?;

        let id = session.id;
        let _ = self.event_tx.send(SessionEvent::Stopped { id });

        tracing::info!(session_id = %id, "session stopped");
        Ok(())
    }

    /// Seek to an absolute position in milliseconds.
    pub async fn seek(&self, position_ms: u64) -> Result<(), SessionError> {
        let state = self.state.lock().await;
        let session = state.active.as_ref().ok_or(SessionError::NoActiveSession)?;

        if session.state != PlayerState::Playing && session.state != PlayerState::Paused {
            return Err(SessionError::Subsystem(
                "cannot seek — not playing or paused".into(),
            ));
        }

        drop(state);

        self.playback
            .seek(position_ms)
            .await
            .map_err(|e| SessionError::PlaybackError(e.to_string()))?;

        // Update session position.
        let mut state = self.state.lock().await;
        if let Some(ref mut session) = state.active {
            session.position_ms = position_ms;
            session.updated_at = Utc::now();
            self.persist_session(session).await?;
        }

        Ok(())
    }

    /// Set the volume (0–100).
    pub async fn set_volume(&self, volume: u8) -> Result<(), SessionError> {
        let clamped = volume.min(100);

        self.playback
            .set_volume(clamped as f64 / 100.0)
            .await
            .map_err(|e| SessionError::PlaybackError(e.to_string()))?;

        let mut state = self.state.lock().await;
        if let Some(ref mut session) = state.active {
            session.volume = clamped;
            session.updated_at = Utc::now();
            self.persist_session(session).await?;

            let id = session.id;
            let _ = self.event_tx.send(SessionEvent::VolumeChanged { id, volume: clamped });
        }

        Ok(())
    }

    /// Retrieve the current session state.
    pub async fn status(&self) -> Result<MediaSession, SessionError> {
        let state = self.state.lock().await;
        state
            .active
            .clone()
            .ok_or(SessionError::NoActiveSession)
    }

    /// Subscribe to session events.
    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.event_tx.subscribe()
    }

    // ── Private helpers ──────────────────────────────────────────────

    /// Persist a session to SQLite.
    async fn persist_session(&self, session: &MediaSession) -> Result<(), SessionError> {
        let db = self.db.lock().await;
        db.execute(
            "INSERT OR REPLACE INTO sessions
             (id, source_url, resolved_url, state, position_ms, duration_ms, volume, title, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                session.id.to_string(),
                session.source_url,
                session.resolved_url,
                session.state.to_string(),
                session.position_ms as i64,
                session.duration_ms.map(|d| d as i64),
                session.volume as i32,
                session.title,
                session.created_at.to_rfc3339(),
                session.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Delete a session from SQLite.
    async fn delete_session(&self, id: Uuid) -> Result<(), SessionError> {
        let db = self.db.lock().await;
        db.execute(
            "DELETE FROM sessions WHERE id = ?1",
            rusqlite::params![id.to_string()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_session_new() {
        let session = MediaSession::new("https://example.com/video.mp4".into());
        assert_eq!(session.source_url, "https://example.com/video.mp4");
        assert!(session.resolved_url.is_none());
        assert_eq!(session.state, PlayerState::Idle);
        assert_eq!(session.position_ms, 0);
        assert!(session.duration_ms.is_none());
        assert_eq!(session.volume, 100);
        assert!(session.title.is_none());
    }

    #[test]
    fn player_state_serde() {
        for state in [
            PlayerState::Idle,
            PlayerState::Resolving,
            PlayerState::Buffering,
            PlayerState::Loaded,
            PlayerState::Playing,
            PlayerState::Paused,
            PlayerState::Error,
        ] {
            let json = serde_json::to_string(&state).expect("serialize");
            let decoded: PlayerState = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded, state, "round-trip failed for {:?}", state);
        }

        assert_eq!(
            serde_json::to_string(&PlayerState::Idle).unwrap(),
            r#""IDLE""#
        );
        assert_eq!(
            serde_json::to_string(&PlayerState::Playing).unwrap(),
            r#""PLAYING""#
        );
        assert_eq!(
            serde_json::to_string(&PlayerState::Resolving).unwrap(),
            r#""RESOLVING""#
        );
    }

    #[test]
    fn player_state_display() {
        assert_eq!(PlayerState::Idle.to_string(), "IDLE");
        assert_eq!(PlayerState::Playing.to_string(), "PLAYING");
        assert_eq!(PlayerState::Buffering.to_string(), "BUFFERING");
    }

    #[test]
    fn media_session_serialization_roundtrip() {
        let session = MediaSession::new("https://example.com/video.mp4".into());
        let json = serde_json::to_string(&session).expect("serialize");
        let decoded: MediaSession = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.source_url, session.source_url);
        assert_eq!(decoded.state, session.state);
        assert_eq!(decoded.volume, session.volume);
    }

    #[test]
    fn session_event_serde() {
        let events = vec![
            SessionEvent::Created {
                id: Uuid::new_v4(),
                url: "https://example.com".into(),
            },
            SessionEvent::Playing { id: Uuid::new_v4() },
            SessionEvent::Buffering {
                id: Uuid::new_v4(),
                percent: 75,
            },
            SessionEvent::Error {
                id: Uuid::new_v4(),
                message: "failed".into(),
            },
        ];

        for event in events {
            let json = serde_json::to_string(&event).expect("serialize");
            let decoded: SessionEvent = serde_json::from_str(&json).expect("deserialize");
            let re = serde_json::to_string(&decoded).expect("re-serialize");
            assert_eq!(json, re, "round-trip failed for {:?}", event);
        }
    }

    #[test]
    fn session_error_variants() {
        let e = SessionError::NoActiveSession;
        assert!(e.to_string().contains("no active session"));

        let e = SessionError::AlreadyActive;
        assert!(e.to_string().contains("already active"));

        let e = SessionError::NotFound(Uuid::new_v4());
        assert!(e.to_string().contains("session not found"));

        let e = SessionError::ResolutionFailed("timeout".into());
        assert!(e.to_string().contains("resolution failed"));

        let e = SessionError::PlaybackError("gst error".into());
        assert!(e.to_string().contains("playback error"));
    }
}
