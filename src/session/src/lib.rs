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

pub mod interfaces;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

// ── Errors ───────────────────────────────────────────────────────────

/// Errors that can originate from the session layer.
#[derive(Error, Debug)]
pub enum SessionError {
    /// No active session was found for the requested operation.
    #[error("no active session")]
    NoActiveSession,

    /// The requested session ID does not exist.
    #[error("session not found: {0}")]
    NotFound(Uuid),

    /// A database operation failed.
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    /// A subsystem returned an error.
    #[error("subsystem error: {0}")]
    Subsystem(String),
}

// ── Player State ─────────────────────────────────────────────────────

/// Possible states of the media player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlayerState {
    /// No media loaded; idle and ready for a new load command.
    Idle,
    /// Media has been loaded but is not yet playing (e.g. buffering).
    Loaded,
    /// Actively decoding and rendering media.
    Playing,
    /// Playback is paused; can be resumed.
    Paused,
    /// Buffer underrun – waiting for more data.
    Buffering,
    /// An unrecoverable error occurred during playback.
    Error,
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
            created_at: now,
            updated_at: now,
        }
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
    _db: Arc<rusqlite::Connection>,
    // TODO: add typed handles to each subsystem:
    // resolver: Arc<dyn interfaces::ResolverTrait>,
    // playback: Arc<dyn interfaces::PlaybackTrait>,
    // display: Arc<dyn interfaces::DisplayTrait>,
    // tor:     Arc<dyn interfaces::TorTrait>,
}

impl SessionManager {
    /// Open (or create) the session database and return a ready manager.
    ///
    /// The database file is created at `db_path` if it does not exist.
    pub fn new(db_path: &str) -> Result<Self, SessionError> {
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
                 created_at    TEXT NOT NULL,
                 updated_at    TEXT NOT NULL
             );",
        )?;
        Ok(Self {
            _db: Arc::new(conn),
        })
    }

    /// Load a new media URL: resolve it through the resolver, create a
    /// session, and hand the direct URL to the playback engine.
    pub async fn load(&self, _url: &str) -> Result<Uuid, SessionError> {
        // TODO: resolve → insert session → start playback
        Err(SessionError::NoActiveSession)
    }

    /// Resume playback on the current session.
    pub async fn play(&self, _session_id: Uuid) -> Result<(), SessionError> {
        // TODO: delegate to playback subsystem
        Err(SessionError::NoActiveSession)
    }

    /// Pause playback on the current session.
    pub async fn pause(&self, _session_id: Uuid) -> Result<(), SessionError> {
        // TODO: delegate to playback subsystem
        Err(SessionError::NoActiveSession)
    }

    /// Stop playback and destroy the session.
    pub async fn stop(&self, _session_id: Uuid) -> Result<(), SessionError> {
        // TODO: delegate to playback subsystem, then delete session row
        Err(SessionError::NoActiveSession)
    }

    /// Seek to an absolute position in milliseconds.
    pub async fn seek(&self, _session_id: Uuid, _position_ms: u64) -> Result<(), SessionError> {
        // TODO: delegate to playback subsystem
        Err(SessionError::NoActiveSession)
    }

    /// Set the volume (0–100).
    pub async fn set_volume(&self, _session_id: Uuid, _volume: u8) -> Result<(), SessionError> {
        // TODO: delegate to playback subsystem
        Err(SessionError::NoActiveSession)
    }

    /// Retrieve the current state of a session.
    pub async fn status(&self, _session_id: Uuid) -> Result<MediaSession, SessionError> {
        // TODO: query SQLite
        Err(SessionError::NotFound(_session_id))
    }
}
