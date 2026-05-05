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
//!  PLAYING ──seek()──► SEEKING ──seek done──► PLAYING
//! ```
//!
//! ## Thread Safety
//!
//! `SessionManager` is `Send + Sync`. All mutable state is protected
//! by `std::sync::Mutex`. The session can be safely wrapped in
//! `Arc` and shared across protocol handlers.

pub mod interfaces;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tokio::sync::broadcast;
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

    /// An invalid state transition was attempted.
    #[error("invalid state transition: cannot go from {from} to {to}")]
    InvalidTransition { from: PlayerState, to: PlayerState },
}

// ── Player State ─────────────────────────────────────────────────────

/// Possible states of the media player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayerState {
    /// No media loaded; idle and ready for a new load command.
    Idle,
    /// URL is being resolved through the resolver.
    Resolving,
    /// Media has been resolved and is buffering.
    Buffering,
    /// Actively decoding and rendering media.
    Playing,
    /// Playback is paused; can be resumed.
    Paused,
    /// A seek operation is in progress.
    Seeking,
    /// An unrecoverable error occurred during playback.
    Error,
}

impl PlayerState {
    /// Returns `true` if a transition from `self` to `target` is valid.
    ///
    /// Valid transitions:
    /// ```text
    /// Idle      → Resolving, Error
    /// Resolving → Buffering, Error, Idle
    /// Buffering → Playing, Paused, Error, Idle
    /// Playing   → Paused, Seeking, Buffering, Error, Idle
    /// Paused    → Playing, Seeking, Error, Idle
    /// Seeking   → Playing, Error, Idle
    /// Error     → Idle
    /// ```
    pub fn can_transition_to(&self, target: PlayerState) -> bool {
        match self {
            PlayerState::Idle => matches!(target, PlayerState::Resolving | PlayerState::Error),
            PlayerState::Resolving => {
                matches!(target, PlayerState::Buffering | PlayerState::Error | PlayerState::Idle)
            }
            PlayerState::Buffering => {
                matches!(
                    target,
                    PlayerState::Playing | PlayerState::Paused | PlayerState::Error | PlayerState::Idle
                )
            }
            PlayerState::Playing => {
                matches!(
                    target,
                    PlayerState::Paused
                        | PlayerState::Seeking
                        | PlayerState::Buffering
                        | PlayerState::Error
                        | PlayerState::Idle
                )
            }
            PlayerState::Paused => {
                matches!(
                    target,
                    PlayerState::Playing | PlayerState::Seeking | PlayerState::Error | PlayerState::Idle
                )
            }
            PlayerState::Seeking => {
                matches!(target, PlayerState::Playing | PlayerState::Error | PlayerState::Idle)
            }
            PlayerState::Error => matches!(target, PlayerState::Idle),
        }
    }

    /// Attempt a state transition, returning `Ok(target)` if valid
    /// or `Err(SessionError::InvalidTransition)` if not.
    pub fn transition(&self, target: PlayerState) -> Result<PlayerState, SessionError> {
        if self.can_transition_to(target) {
            Ok(target)
        } else {
            Err(SessionError::InvalidTransition {
                from: *self,
                to: target,
            })
        }
    }
}

impl std::fmt::Display for PlayerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Output matches snake_case serde serialization for DB consistency.
        match self {
            PlayerState::Idle => write!(f, "idle"),
            PlayerState::Resolving => write!(f, "resolving"),
            PlayerState::Buffering => write!(f, "buffering"),
            PlayerState::Playing => write!(f, "playing"),
            PlayerState::Paused => write!(f, "paused"),
            PlayerState::Seeking => write!(f, "seeking"),
            PlayerState::Error => write!(f, "error"),
        }
    }
}

impl std::str::FromStr for PlayerState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Parse the snake_case string back to a PlayerState.
        // We leverage serde for this to stay consistent.
        let json = format!("\"{}\"", s);
        serde_json::from_str(&json).map_err(|e| e.to_string())
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
    /// A seek operation is in progress.
    Seeking { id: Uuid, position_ms: u64 },
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

// ── Session Manager ──────────────────────────────────────────────────

/// Central coordinator that owns the session database and dispatches
/// commands to the resolver, playback engine, display, and Tor subsystems.
///
/// All public methods are thread-safe; the manager is intended to be
/// wrapped in an `Arc` and shared across protocol handlers.
pub struct SessionManager {
    /// SQLite connection handle (wrapped in std::sync::Mutex for Send + Sync).
    db: Mutex<rusqlite::Connection>,
    /// Current active session ID (only one session active at a time).
    active_session_id: Arc<Mutex<Option<Uuid>>>,
    /// Event broadcast channel.
    event_tx: broadcast::Sender<SessionEvent>,
}

impl SessionManager {
    /// Open (or create) the session database and return a ready manager.
    ///
    /// The database file is created at `db_path` if it does not exist.
    /// Pass `":memory:"` for an in-memory database (useful for tests).
    pub fn new(db_path: &str) -> Result<Self, SessionError> {
        let conn = rusqlite::Connection::open(db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                 id            TEXT PRIMARY KEY,
                 source_url    TEXT NOT NULL,
                 resolved_url  TEXT,
                 state         TEXT NOT NULL DEFAULT 'idle',
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
            db: Mutex::new(conn),
            active_session_id: Arc::new(Mutex::new(None)),
            event_tx,
        })
    }

    /// Attempt a state transition on the session identified by `session_id`.
    ///
    /// Reads the current state from SQLite, validates the transition via
    /// [`PlayerState::can_transition_to`], and if valid, updates the
    /// database and returns the new state. Returns
    /// [`SessionError::InvalidTransition`] if the transition is not allowed.
    pub fn try_transition(
        &self,
        session_id: Uuid,
        target: PlayerState,
    ) -> Result<PlayerState, SessionError> {
        let db = self.db.lock().map_err(|e| {
            SessionError::Subsystem(format!("db lock poisoned: {}", e))
        })?;

        // Read current state from SQLite.
        let current_state_str: String = db
            .query_row(
                "SELECT state FROM sessions WHERE id = ?1",
                rusqlite::params![session_id.to_string()],
                |row| row.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => SessionError::NotFound(session_id),
                other => SessionError::Database(other),
            })?;

        let current_state: PlayerState = current_state_str
            .parse()
            .map_err(|e| SessionError::Subsystem(format!("corrupt state in db: {}", e)))?;

        // Validate transition.
        if !current_state.can_transition_to(target) {
            return Err(SessionError::InvalidTransition {
                from: current_state,
                to: target,
            });
        }

        // Update SQLite with new state.
        let now = Utc::now().to_rfc3339();
        db.execute(
            "UPDATE sessions SET state = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![target.to_string(), now, session_id.to_string()],
        )?;

        Ok(target)
    }

    /// Insert a session into the database.
    pub fn insert_session(&self, session: &MediaSession) -> Result<(), SessionError> {
        let db = self.db.lock().map_err(|e| {
            SessionError::Subsystem(format!("db lock poisoned: {}", e))
        })?;
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

    /// Load a session from the database by ID.
    pub fn load_session(&self, id: Uuid) -> Result<MediaSession, SessionError> {
        let db = self.db.lock().map_err(|e| {
            SessionError::Subsystem(format!("db lock poisoned: {}", e))
        })?;

        let session = db.query_row(
            "SELECT id, source_url, resolved_url, state, position_ms, duration_ms,
                    volume, title, created_at, updated_at
             FROM sessions WHERE id = ?1",
            rusqlite::params![id.to_string()],
            |row| {
                let id_str: String = row.get(0)?;
                let source_url: String = row.get(1)?;
                let resolved_url: Option<String> = row.get(2)?;
                let state_str: String = row.get(3)?;
                let position_ms: i64 = row.get(4)?;
                let duration_ms: Option<i64> = row.get(5)?;
                let volume: i32 = row.get(6)?;
                let title: Option<String> = row.get(7)?;
                let created_at_str: String = row.get(8)?;
                let updated_at_str: String = row.get(9)?;

                Ok(MediaSession {
                    id: Uuid::parse_str(&id_str).unwrap(),
                    source_url,
                    resolved_url,
                    state: state_str.parse().unwrap(),
                    position_ms: position_ms as u64,
                    duration_ms: duration_ms.map(|d| d as u64),
                    volume: volume as u8,
                    title,
                    created_at: created_at_str.parse().unwrap(),
                    updated_at: updated_at_str.parse().unwrap(),
                })
            },
        ).map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => SessionError::NotFound(id),
            other => SessionError::Database(other),
        })?;

        Ok(session)
    }

    /// Retrieve the state of a specific session by ID.
    pub async fn status(&self, session_id: Uuid) -> Result<MediaSession, SessionError> {
        self.load_session(session_id)
    }

    /// Retrieve the currently active session's state.
    ///
    /// Returns [`SessionError::NoActiveSession`] if no session is active.
    pub async fn current_status(&self) -> Result<MediaSession, SessionError> {
        let id = {
            let guard = self.active_session_id.lock()
                .map_err(|e| SessionError::Subsystem(format!("lock poisoned: {}", e)))?;
            guard.ok_or(SessionError::NoActiveSession)?
        };
        self.load_session(id)
    }

    /// Subscribe to session events.
    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.event_tx.subscribe()
    }

    // ── Stub methods (subsystems not yet implemented) ────────────────

    /// Load a new media URL.
    ///
    /// **Stub**: returns an error until the resolver subsystem is implemented.
    pub async fn load(&self, _url: &str) -> Result<Uuid, SessionError> {
        Err(SessionError::Subsystem(
            "resolver subsystem not implemented".into(),
        ))
    }

    /// Pause playback on the current session.
    ///
    /// **Stub**: returns an error until the playback subsystem is implemented.
    pub async fn pause(&self) -> Result<(), SessionError> {
        Err(SessionError::Subsystem(
            "playback subsystem not implemented".into(),
        ))
    }

    /// Resume playback on the current session.
    ///
    /// **Stub**: returns an error until the playback subsystem is implemented.
    pub async fn resume(&self) -> Result<(), SessionError> {
        Err(SessionError::Subsystem(
            "playback subsystem not implemented".into(),
        ))
    }

    /// Stop playback and destroy the session.
    ///
    /// **Stub**: returns an error until the playback subsystem is implemented.
    pub async fn stop(&self) -> Result<(), SessionError> {
        Err(SessionError::Subsystem(
            "playback subsystem not implemented".into(),
        ))
    }

    /// Seek to an absolute position in milliseconds.
    ///
    /// **Stub**: returns an error until the playback subsystem is implemented.
    pub async fn seek(&self, _position_ms: u64) -> Result<(), SessionError> {
        Err(SessionError::Subsystem(
            "playback subsystem not implemented".into(),
        ))
    }

    /// Set the volume (0–100).
    ///
    /// **Stub**: returns an error until the playback subsystem is implemented.
    pub async fn set_volume(&self, _volume: u8) -> Result<(), SessionError> {
        Err(SessionError::Subsystem(
            "playback subsystem not implemented".into(),
        ))
    }

    // ── Private helpers ──────────────────────────────────────────────

    /// Delete a session from SQLite.
    #[allow(dead_code)]
    fn delete_session(&self, id: Uuid) -> Result<(), SessionError> {
        let db = self.db.lock().map_err(|e| {
            SessionError::Subsystem(format!("db lock poisoned: {}", e))
        })?;
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

    // ── PlayerState transitions ─────────────────────────────────────

    #[test]
    fn test_idle_can_resolve() {
        assert!(PlayerState::Idle.can_transition_to(PlayerState::Resolving));
    }

    #[test]
    fn test_idle_cannot_play() {
        assert!(!PlayerState::Idle.can_transition_to(PlayerState::Playing));
    }

    #[test]
    fn test_resolving_can_buffer() {
        assert!(PlayerState::Resolving.can_transition_to(PlayerState::Buffering));
    }

    #[test]
    fn test_resolving_can_error() {
        assert!(PlayerState::Resolving.can_transition_to(PlayerState::Error));
    }

    #[test]
    fn test_buffering_can_play() {
        assert!(PlayerState::Buffering.can_transition_to(PlayerState::Playing));
    }

    #[test]
    fn test_buffering_can_pause() {
        assert!(PlayerState::Buffering.can_transition_to(PlayerState::Paused));
    }

    #[test]
    fn test_playing_can_pause() {
        assert!(PlayerState::Playing.can_transition_to(PlayerState::Paused));
    }

    #[test]
    fn test_playing_can_seek() {
        assert!(PlayerState::Playing.can_transition_to(PlayerState::Seeking));
    }

    #[test]
    fn test_playing_can_buffer() {
        assert!(PlayerState::Playing.can_transition_to(PlayerState::Buffering));
    }

    #[test]
    fn test_paused_can_play() {
        assert!(PlayerState::Paused.can_transition_to(PlayerState::Playing));
    }

    #[test]
    fn test_paused_can_seek() {
        assert!(PlayerState::Paused.can_transition_to(PlayerState::Seeking));
    }

    #[test]
    fn test_seeking_can_play() {
        assert!(PlayerState::Seeking.can_transition_to(PlayerState::Playing));
    }

    #[test]
    fn test_error_can_reset() {
        assert!(PlayerState::Error.can_transition_to(PlayerState::Idle));
    }

    #[test]
    fn test_error_cannot_play() {
        assert!(!PlayerState::Error.can_transition_to(PlayerState::Playing));
    }

    #[test]
    fn test_idle_cannot_seek() {
        assert!(!PlayerState::Idle.can_transition_to(PlayerState::Seeking));
    }

    #[test]
    fn test_playing_cannot_resolve() {
        assert!(!PlayerState::Playing.can_transition_to(PlayerState::Resolving));
    }

    // ── transition() method ─────────────────────────────────────────

    #[test]
    fn test_transition_success() {
        let result = PlayerState::Idle.transition(PlayerState::Resolving);
        assert_eq!(result.unwrap(), PlayerState::Resolving);
    }

    #[test]
    fn test_transition_failure() {
        let result = PlayerState::Idle.transition(PlayerState::Playing);
        assert!(result.is_err());
        match result.unwrap_err() {
            SessionError::InvalidTransition { from, to } => {
                assert_eq!(from, PlayerState::Idle);
                assert_eq!(to, PlayerState::Playing);
            }
            other => panic!("Expected InvalidTransition, got {:?}", other),
        }
    }

    // ── Full playback lifecycle ──────────────────────────────────────

    #[test]
    fn test_full_lifecycle_transitions() {
        let state = PlayerState::Idle;
        let state = state.transition(PlayerState::Resolving).unwrap();
        assert_eq!(state, PlayerState::Resolving);
        let state = state.transition(PlayerState::Buffering).unwrap();
        assert_eq!(state, PlayerState::Buffering);
        let state = state.transition(PlayerState::Playing).unwrap();
        assert_eq!(state, PlayerState::Playing);
        let state = state.transition(PlayerState::Paused).unwrap();
        assert_eq!(state, PlayerState::Paused);
        let state = state.transition(PlayerState::Playing).unwrap();
        assert_eq!(state, PlayerState::Playing);
        let state = state.transition(PlayerState::Seeking).unwrap();
        assert_eq!(state, PlayerState::Seeking);
        let state = state.transition(PlayerState::Playing).unwrap();
        assert_eq!(state, PlayerState::Playing);
        let state = state.transition(PlayerState::Idle).unwrap();
        assert_eq!(state, PlayerState::Idle);
    }

    // ── Error recovery ───────────────────────────────────────────────

    #[test]
    fn test_error_recovery() {
        let state = PlayerState::Playing;
        let state = state.transition(PlayerState::Error).unwrap();
        assert_eq!(state, PlayerState::Error);
        let state = state.transition(PlayerState::Idle).unwrap();
        assert_eq!(state, PlayerState::Idle);
        // Can start again after error
        let state = state.transition(PlayerState::Resolving).unwrap();
        assert_eq!(state, PlayerState::Resolving);
    }

    // ── MediaSession ─────────────────────────────────────────────────

    #[test]
    fn test_media_session_new() {
        let session = MediaSession::new("https://example.com/video.mp4".into());
        assert_eq!(session.state, PlayerState::Idle);
        assert_eq!(session.source_url, "https://example.com/video.mp4");
        assert!(session.resolved_url.is_none());
        assert_eq!(session.volume, 100);
        assert_eq!(session.position_ms, 0);
    }

    #[test]
    fn test_media_session_serialization() {
        let session = MediaSession::new("https://example.com/video.mp4".into());
        let json = serde_json::to_string(&session).unwrap();
        let parsed: MediaSession = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.source_url, session.source_url);
        assert_eq!(parsed.state, session.state);
    }

    #[test]
    fn test_player_state_serde_roundtrip() {
        for state in [
            PlayerState::Idle,
            PlayerState::Resolving,
            PlayerState::Buffering,
            PlayerState::Playing,
            PlayerState::Paused,
            PlayerState::Seeking,
            PlayerState::Error,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let parsed: PlayerState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, parsed, "Failed roundtrip for {:?}", state);
        }
    }

    // ── SessionManager with SQLite ───────────────────────────────────

    #[tokio::test]
    async fn test_session_manager_new_in_memory() {
        let mgr = SessionManager::new(":memory:").unwrap();
        // No sessions exist
        let status = mgr.status(Uuid::new_v4()).await;
        assert!(status.is_err());
    }

    // ── try_transition with SQLite ───────────────────────────────────

    #[tokio::test]
    async fn test_try_transition_valid() {
        let mgr = SessionManager::new(":memory:").unwrap();
        // Insert a session manually
        let session = MediaSession::new("https://example.com/video.mp4".into());
        let id = session.id;
        mgr.insert_session(&session).unwrap();

        let new_state = mgr.try_transition(id, PlayerState::Resolving).unwrap();
        assert_eq!(new_state, PlayerState::Resolving);

        let loaded = mgr.load_session(id).unwrap();
        assert_eq!(loaded.state, PlayerState::Resolving);
    }

    #[tokio::test]
    async fn test_try_transition_invalid() {
        let mgr = SessionManager::new(":memory:").unwrap();
        let session = MediaSession::new("https://example.com/video.mp4".into());
        let id = session.id;
        mgr.insert_session(&session).unwrap();

        let result = mgr.try_transition(id, PlayerState::Playing);
        assert!(result.is_err());
    }
}
