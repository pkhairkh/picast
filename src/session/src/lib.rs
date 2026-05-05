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
    /// Resolver subsystem for URL resolution.
    resolver: Option<Arc<dyn interfaces::ResolverTrait>>,
    /// Playback subsystem for media pipeline control.
    playback: Option<Arc<dyn interfaces::PlaybackTrait>>,
    /// Display subsystem for DRM/KMS control.
    display: Option<Arc<dyn interfaces::DisplayTrait>>,
    /// Tor subsystem for SOCKS proxy management.
    tor: Option<Arc<dyn interfaces::TorTrait>>,
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
            resolver: None,
            playback: None,
            display: None,
            tor: None,
        })
    }

    /// Create a manager with all subsystems wired in.
    ///
    /// This is the preferred constructor for production use.
    /// The individual subsystems are passed as trait objects so
    /// they can be mocked in tests or swapped at runtime.
    pub fn with_subsystems(
        db_path: &str,
        resolver: Arc<dyn interfaces::ResolverTrait>,
        playback: Arc<dyn interfaces::PlaybackTrait>,
        display: Arc<dyn interfaces::DisplayTrait>,
        tor: Arc<dyn interfaces::TorTrait>,
    ) -> Result<Self, SessionError> {
        let mut mgr = Self::new(db_path)?;
        mgr.resolver = Some(resolver);
        mgr.playback = Some(playback);
        mgr.display = Some(display);
        mgr.tor = Some(tor);
        Ok(mgr)
    }

    /// Attempt a state transition on the session identified by `session_id`.
    ///
    /// Reads the current state from SQLite, validates the transition via
    /// [`PlayerState::can_transition_to`], and if valid, updates the
    /// database and broadcasts the state change event. Returns
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

        // Broadcast state-change event.
        let event = match target {
            PlayerState::Resolving => SessionEvent::Resolving { id: session_id },
            PlayerState::Playing => SessionEvent::Playing { id: session_id },
            PlayerState::Paused => SessionEvent::Paused { id: session_id },
            PlayerState::Idle => SessionEvent::Stopped { id: session_id },
            PlayerState::Error => SessionEvent::Error {
                id: session_id,
                message: "entered error state".into(),
            },
            _ => return Ok(target), // No broadcast for Buffering/Seeking here
        };
        let _ = self.event_tx.send(event);

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

    /// Update specific fields of a session in the database.
    ///
    /// Helper for updating resolved_url, position_ms, etc.
    #[allow(dead_code)]
    fn update_session_field(
        &self,
        session_id: Uuid,
        field: &str,
        value: &dyn rusqlite::types::ToSql,
    ) -> Result<(), SessionError> {
        let db = self.db.lock().map_err(|e| {
            SessionError::Subsystem(format!("db lock poisoned: {}", e))
        })?;
        let now = Utc::now().to_rfc3339();
        let sql = format!("UPDATE sessions SET {} = ?, updated_at = ? WHERE id = ?", field);
        db.execute(&sql, rusqlite::params![value, now, session_id.to_string()])?;
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

    // ── Command methods ───────────────────────────────────────────────

    /// Load a new media URL and start the resolution/playback flow.
    ///
    /// This method:
    /// 1. Validates that no session is already active (PiCast is single-session).
    /// 2. Creates a new `MediaSession` in the `Idle` state.
    /// 3. Transitions to `Resolving` and calls the resolver subsystem.
    /// 4. On successful resolution, transitions to `Buffering` and starts
    ///    playback through the Tor SOCKS proxy.
    /// 5. Transitions to `Playing` when the pipeline is confirmed active.
    ///
    /// Returns the new session's UUID on success.
    pub async fn load(&self, url: &str) -> Result<Uuid, SessionError> {
        // Check if a session is already active.
        {
            let guard = self.active_session_id.lock()
                .map_err(|e| SessionError::Subsystem(format!("lock poisoned: {}", e)))?;
            if guard.is_some() {
                return Err(SessionError::AlreadyActive);
            }
        }

        // Create the session in Idle state.
        let mut session = MediaSession::new(url.to_owned());
        session.state = PlayerState::Idle;
        self.insert_session(&session)?;

        // Mark as the active session.
        {
            let mut guard = self.active_session_id.lock()
                .map_err(|e| SessionError::Subsystem(format!("lock poisoned: {}", e)))?;
            *guard = Some(session.id);
        }

        let id = session.id;

        // Broadcast: session created.
        let _ = self.event_tx.send(SessionEvent::Created {
            id,
            url: url.to_owned(),
        });

        // Transition: Idle → Resolving.
        self.try_transition(id, PlayerState::Resolving)?;

        // Resolve the URL via the resolver subsystem.
        let direct_url = if let Some(ref resolver) = self.resolver {
            resolver.resolve(url).await.map_err(|e| {
                // Transition to Error state on resolution failure.
                let _ = self.try_transition(id, PlayerState::Error);
                SessionError::ResolutionFailed(e.to_string())
            })?
        } else {
            // Without a resolver, treat the URL as a direct media URL.
            tracing::warn!("no resolver subsystem — using URL as direct media");
            url.to_owned()
        };

        // Update the session with the resolved URL.
        let direct_url_sql: String = direct_url.clone();
        {
            let db = self.db.lock().map_err(|e| {
                SessionError::Subsystem(format!("db lock poisoned: {}", e))
            })?;
            let now = Utc::now().to_rfc3339();
            db.execute(
                "UPDATE sessions SET resolved_url = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![direct_url_sql, now, id.to_string()],
            )?;
        }

        // Broadcast: resolved.
        let _ = self.event_tx.send(SessionEvent::Resolved {
            id,
            direct_url: direct_url.clone(),
            title: None,
        });

        // Transition: Resolving → Buffering.
        self.try_transition(id, PlayerState::Buffering)?;

        // Start playback via the playback subsystem.
        if let Some(ref playback) = self.playback {
            let socks_addr = self.tor.as_ref().map(|t| t.socks_addr()).unwrap_or_default();
            let isolation_username = self.tor.as_ref()
                .map(|t| t.isolation_username(
                    url::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.to_owned())).unwrap_or_default().as_str()
                ))
                .unwrap_or_default();

            playback.play(&direct_url, &socks_addr, &isolation_username).await.map_err(|e| {
                let _ = self.try_transition(id, PlayerState::Error);
                SessionError::PlaybackError(e.to_string())
            })?;
        }

        // Transition: Buffering → Playing.
        self.try_transition(id, PlayerState::Playing)?;

        Ok(id)
    }

    /// Pause playback on the current session.
    ///
    /// Valid only when the session is in the `Playing` state.
    pub async fn pause(&self) -> Result<(), SessionError> {
        let id = self.active_session_id()?;

        // Transition: Playing → Paused.
        self.try_transition(id, PlayerState::Paused)?;

        // Delegate to playback subsystem.
        if let Some(ref playback) = self.playback {
            playback.pause().await.map_err(|e| {
                SessionError::PlaybackError(e.to_string())
            })?;
        }

        Ok(())
    }

    /// Resume playback on the current session.
    ///
    /// Valid only when the session is in the `Paused` state.
    pub async fn resume(&self) -> Result<(), SessionError> {
        let id = self.active_session_id()?;

        // Transition: Paused → Playing.
        self.try_transition(id, PlayerState::Playing)?;

        // Delegate to playback subsystem.
        if let Some(ref playback) = self.playback {
            playback.resume().await.map_err(|e| {
                SessionError::PlaybackError(e.to_string())
            })?;
        }

        Ok(())
    }

    /// Stop playback and destroy the current session.
    ///
    /// Can be called from any active state. Transitions to `Idle`,
    /// stops the playback pipeline, and clears the active session.
    pub async fn stop(&self) -> Result<(), SessionError> {
        let id = self.active_session_id()?;

        // Load current state to check what transition is needed.
        let session = self.load_session(id)?;

        // If we're in Playing/Paused/Buffering, stop the pipeline first.
        if matches!(session.state, PlayerState::Playing | PlayerState::Paused | PlayerState::Buffering) {
            if let Some(ref playback) = self.playback {
                let _ = playback.stop().await;
            }
        }

        // Transition to Idle (valid from any non-Error state).
        if session.state != PlayerState::Idle {
            if session.state.can_transition_to(PlayerState::Idle) {
                let _ = self.try_transition(id, PlayerState::Idle);
            } else if session.state.can_transition_to(PlayerState::Error) {
                let _ = self.try_transition(id, PlayerState::Error);
                let _ = self.try_transition(id, PlayerState::Idle);
            }
        }

        // Clear the active session.
        {
            let mut guard = self.active_session_id.lock()
                .map_err(|e| SessionError::Subsystem(format!("lock poisoned: {}", e)))?;
            *guard = None;
        }

        // Delete the session from the database.
        self.delete_session(id)?;

        Ok(())
    }

    /// Seek to an absolute position in milliseconds.
    ///
    /// Valid when the session is in `Playing` or `Paused` state.
    /// Transitions through `Seeking` and back to the previous state.
    pub async fn seek(&self, position_ms: u64) -> Result<(), SessionError> {
        let id = self.active_session_id()?;

        // Load current state to determine the return state.
        let session = self.load_session(id)?;
        let return_state = session.state;

        // Transition to Seeking.
        self.try_transition(id, PlayerState::Seeking)?;

        // Broadcast seek event.
        let _ = self.event_tx.send(SessionEvent::Seeking {
            id,
            position_ms,
        });

        // Delegate to playback subsystem.
        if let Some(ref playback) = self.playback {
            playback.seek(position_ms).await.map_err(|e| {
                SessionError::PlaybackError(e.to_string())
            })?;
        }

        // Transition back to the previous state (Playing or Paused).
        if return_state == PlayerState::Paused {
            // Seeking from Paused returns to Paused (not Playing).
            // But our state machine says Seeking → Playing is valid,
            // and then Playing → Paused is valid. So we do both.
            self.try_transition(id, PlayerState::Playing)?;
            self.try_transition(id, PlayerState::Paused)?;
        } else {
            self.try_transition(id, PlayerState::Playing)?;
        }

        Ok(())
    }

    /// Set the volume (0–100).
    ///
    /// Can be called in any active state. Does not trigger a state
    /// transition.
    pub async fn set_volume(&self, volume: u8) -> Result<(), SessionError> {
        let id = self.active_session_id()?;

        let clamped = volume.min(100);

        // Update the session in the database.
        {
            let db = self.db.lock().map_err(|e| {
                SessionError::Subsystem(format!("db lock poisoned: {}", e))
            })?;
            let now = Utc::now().to_rfc3339();
            db.execute(
                "UPDATE sessions SET volume = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![clamped as i32, now, id.to_string()],
            )?;
        }

        // Delegate to playback subsystem.
        if let Some(ref playback) = self.playback {
            playback.set_volume(clamped as f64 / 100.0).await.map_err(|e| {
                SessionError::PlaybackError(e.to_string())
            })?;
        }

        // Broadcast volume change event.
        let _ = self.event_tx.send(SessionEvent::VolumeChanged {
            id,
            volume: clamped,
        });

        Ok(())
    }

    // ── Private helpers ──────────────────────────────────────────────

    /// Get the active session ID or return NoActiveSession.
    fn active_session_id(&self) -> Result<Uuid, SessionError> {
        let guard = self.active_session_id.lock()
            .map_err(|e| SessionError::Subsystem(format!("lock poisoned: {}", e)))?;
        guard.ok_or(SessionError::NoActiveSession)
    }

    /// Delete a session from SQLite.
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
    use interfaces::{DisplayTrait, PlaybackTrait, ResolverTrait, TorTrait};
    use std::sync::atomic::{AtomicBool, AtomicU16, Ordering as AtomicOrdering};

    // ── Mock subsystems ──────────────────────────────────────────────

    /// Mock resolver that returns a predictable direct URL.
    struct MockResolver {
        should_fail: AtomicBool,
    }

    impl MockResolver {
        fn new() -> Self {
            Self { should_fail: AtomicBool::new(false) }
        }
        fn with_failure() -> Self {
            Self { should_fail: AtomicBool::new(true) }
        }
    }

    #[async_trait::async_trait]
    impl ResolverTrait for MockResolver {
        async fn resolve(&self, url: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
            if self.should_fail.load(AtomicOrdering::Relaxed) {
                return Err("mock resolution failure".into());
            }
            Ok(format!("{}?direct=1", url))
        }
    }

    /// Mock playback that tracks state transitions.
    struct MockPlayback {
        is_playing: AtomicBool,
        is_paused: AtomicBool,
        volume: std::sync::Mutex<f64>,
        last_seek_ms: std::sync::Mutex<u64>,
        call_count: std::sync::Mutex<std::collections::HashMap<String, u32>>,
    }

    impl MockPlayback {
        fn new() -> Self {
            Self {
                is_playing: AtomicBool::new(false),
                is_paused: AtomicBool::new(false),
                volume: std::sync::Mutex::new(1.0),
                last_seek_ms: std::sync::Mutex::new(0),
                call_count: std::sync::Mutex::new(std::collections::HashMap::new()),
            }
        }

        fn call_count(&self, method: &str) -> u32 {
            self.call_count.lock().unwrap().get(method).copied().unwrap_or(0)
        }

        fn inc_call(&self, method: &str) {
            *self.call_count.lock().unwrap().entry(method.to_string()).or_insert(0) += 1;
        }
    }

    #[async_trait::async_trait]
    impl PlaybackTrait for MockPlayback {
        async fn play(&self, _url: &str, _socks_addr: &str, _isolation_username: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.inc_call("play");
            self.is_playing.store(true, AtomicOrdering::Relaxed);
            self.is_paused.store(false, AtomicOrdering::Relaxed);
            Ok(())
        }
        async fn pause(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.inc_call("pause");
            self.is_playing.store(false, AtomicOrdering::Relaxed);
            self.is_paused.store(true, AtomicOrdering::Relaxed);
            Ok(())
        }
        async fn resume(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.inc_call("resume");
            self.is_playing.store(true, AtomicOrdering::Relaxed);
            self.is_paused.store(false, AtomicOrdering::Relaxed);
            Ok(())
        }
        async fn stop(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.inc_call("stop");
            self.is_playing.store(false, AtomicOrdering::Relaxed);
            self.is_paused.store(false, AtomicOrdering::Relaxed);
            Ok(())
        }
        async fn seek(&self, position_ms: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.inc_call("seek");
            *self.last_seek_ms.lock().unwrap() = position_ms;
            Ok(())
        }
        async fn set_volume(&self, volume: f64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.inc_call("set_volume");
            *self.volume.lock().unwrap() = volume;
            Ok(())
        }
        async fn position_ms(&self) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
            Ok(0)
        }
        async fn duration_ms(&self) -> Result<Option<u64>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(Some(300000))
        }
    }

    /// Mock display that tracks acquire/release.
    struct MockDisplay {
        acquired: AtomicBool,
    }

    impl MockDisplay {
        fn new() -> Self {
            Self { acquired: AtomicBool::new(false) }
        }
    }

    #[async_trait::async_trait]
    impl DisplayTrait for MockDisplay {
        async fn acquire(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.acquired.store(true, AtomicOrdering::Relaxed);
            Ok(())
        }
        async fn release(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.acquired.store(false, AtomicOrdering::Relaxed);
            Ok(())
        }
        async fn resolution(&self) -> Result<(u32, u32), Box<dyn std::error::Error + Send + Sync>> {
            Ok((1920, 1080))
        }
    }

    /// Mock Tor manager.
    struct MockTor {
        socks_port: AtomicU16,
    }

    impl MockTor {
        fn new() -> Self {
            Self { socks_port: AtomicU16::new(9050) }
        }
    }

    #[async_trait::async_trait]
    impl TorTrait for MockTor {
        async fn ensure_running(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
        fn socks_addr(&self) -> String {
            format!("127.0.0.1:{}", self.socks_port.load(AtomicOrdering::Relaxed))
        }
        async fn health_check(&self) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
            Ok(true)
        }
        fn isolation_username(&self, hostname: &str) -> String {
            picast_tor::stream_isolation_id(hostname)
        }
    }

    /// Helper: create a fully-wired SessionManager with mock subsystems.
    fn session_manager_with_mocks() -> SessionManager {
        let resolver = Arc::new(MockResolver::new());
        let playback = Arc::new(MockPlayback::new());
        let display = Arc::new(MockDisplay::new());
        let tor = Arc::new(MockTor::new());

        SessionManager::with_subsystems(
            ":memory:",
            resolver,
            playback,
            display,
            tor,
        ).unwrap()
    }

    /// Helper: create a SessionManager without subsystems.
    fn session_manager_bare() -> SessionManager {
        SessionManager::new(":memory:").unwrap()
    }

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

    // ── load() with mock subsystems ──────────────────────────────────

    #[tokio::test]
    async fn test_load_creates_session_and_resolves() {
        let mgr = session_manager_with_mocks();
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        // Session should be in Playing state.
        let session = mgr.load_session(id).unwrap();
        assert_eq!(session.state, PlayerState::Playing);
        assert_eq!(session.source_url, "https://example.com/video.mp4");
        assert!(session.resolved_url.is_some());
        assert!(session.resolved_url.unwrap().contains("direct=1"));
    }

    #[tokio::test]
    async fn test_load_already_active_returns_conflict() {
        let mgr = session_manager_with_mocks();
        let _ = mgr.load("https://example.com/video1.mp4").await.unwrap();
        let result = mgr.load("https://example.com/video2.mp4").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SessionError::AlreadyActive => {},
            other => panic!("Expected AlreadyActive, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_load_without_subsystems_uses_url_directly() {
        let mgr = session_manager_bare();
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        let session = mgr.load_session(id).unwrap();
        assert_eq!(session.state, PlayerState::Playing);
        assert_eq!(session.resolved_url, Some("https://example.com/video.mp4".to_string()));
    }

    // ── pause/resume with mock subsystems ────────────────────────────

    #[tokio::test]
    async fn test_pause_resumes_session() {
        let mgr = session_manager_with_mocks();
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        // Pause
        mgr.pause().await.unwrap();
        let session = mgr.load_session(id).unwrap();
        assert_eq!(session.state, PlayerState::Paused);

        // Resume
        mgr.resume().await.unwrap();
        let session = mgr.load_session(id).unwrap();
        assert_eq!(session.state, PlayerState::Playing);
    }

    #[tokio::test]
    async fn test_pause_without_active_session_fails() {
        let mgr = session_manager_with_mocks();
        let result = mgr.pause().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SessionError::NoActiveSession => {},
            other => panic!("Expected NoActiveSession, got {:?}", other),
        }
    }

    // ── stop with mock subsystems ────────────────────────────────────

    #[tokio::test]
    async fn test_stop_clears_session() {
        let mgr = session_manager_with_mocks();
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        mgr.stop().await.unwrap();

        // Session should be deleted.
        let result = mgr.load_session(id);
        assert!(result.is_err());

        // No active session.
        let status = mgr.current_status().await;
        assert!(status.is_err());
    }

    #[tokio::test]
    async fn test_stop_without_active_session_fails() {
        let mgr = session_manager_with_mocks();
        let result = mgr.stop().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_stop_allows_new_load() {
        let mgr = session_manager_with_mocks();
        let _ = mgr.load("https://example.com/video1.mp4").await.unwrap();
        mgr.stop().await.unwrap();

        // Should be able to load again.
        let id2 = mgr.load("https://example.com/video2.mp4").await.unwrap();
        let session = mgr.load_session(id2).unwrap();
        assert_eq!(session.source_url, "https://example.com/video2.mp4");
    }

    // ── seek with mock subsystems ────────────────────────────────────

    #[tokio::test]
    async fn test_seek_from_playing() {
        let mgr = session_manager_with_mocks();
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        mgr.seek(60000).await.unwrap();

        // Should return to Playing after seek.
        let session = mgr.load_session(id).unwrap();
        assert_eq!(session.state, PlayerState::Playing);
    }

    #[tokio::test]
    async fn test_seek_from_paused() {
        let mgr = session_manager_with_mocks();
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        mgr.pause().await.unwrap();
        mgr.seek(30000).await.unwrap();

        // Should return to Paused after seeking from Paused.
        let session = mgr.load_session(id).unwrap();
        assert_eq!(session.state, PlayerState::Paused);
    }

    // ── set_volume with mock subsystems ──────────────────────────────

    #[tokio::test]
    async fn test_set_volume() {
        let mgr = session_manager_with_mocks();
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        mgr.set_volume(50).await.unwrap();

        let session = mgr.load_session(id).unwrap();
        assert_eq!(session.volume, 50);
    }

    #[tokio::test]
    async fn test_set_volume_clamps_to_100() {
        let mgr = session_manager_with_mocks();
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        mgr.set_volume(200).await.unwrap();

        let session = mgr.load_session(id).unwrap();
        assert_eq!(session.volume, 100);
    }

    // ── current_status ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_current_status_no_active_session() {
        let mgr = session_manager_with_mocks();
        let result = mgr.current_status().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SessionError::NoActiveSession => {},
            other => panic!("Expected NoActiveSession, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_current_status_returns_active_session() {
        let mgr = session_manager_with_mocks();
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        let status = mgr.current_status().await.unwrap();
        assert_eq!(status.id, id);
        assert_eq!(status.state, PlayerState::Playing);
    }

    // ── event broadcasting ───────────────────────────────────────────

    #[tokio::test]
    async fn test_load_broadcasts_events() {
        let mgr = session_manager_with_mocks();
        let mut rx = mgr.subscribe();

        let _ = mgr.load("https://example.com/video.mp4").await.unwrap();

        // Should receive Created, Resolving, Resolved, Playing events.
        let mut received = Vec::new();
        while let Ok(event) = rx.try_recv() {
            received.push(event);
        }

        assert!(received.iter().any(|e| matches!(e, SessionEvent::Created { .. })), "should have Created event");
        assert!(received.iter().any(|e| matches!(e, SessionEvent::Resolving { .. })), "should have Resolving event");
        assert!(received.iter().any(|e| matches!(e, SessionEvent::Resolved { .. })), "should have Resolved event");
        assert!(received.iter().any(|e| matches!(e, SessionEvent::Playing { .. })), "should have Playing event");
    }

    #[tokio::test]
    async fn test_pause_broadcasts_paused_event() {
        let mgr = session_manager_with_mocks();
        let _ = mgr.load("https://example.com/video.mp4").await.unwrap();

        let mut rx = mgr.subscribe();
        // Drain pending events from load.
        while rx.try_recv().is_ok() {}

        mgr.pause().await.unwrap();

        let event = rx.try_recv().unwrap();
        assert!(matches!(event, SessionEvent::Paused { .. }));
    }

    // ── Full lifecycle with mock subsystems ──────────────────────────

    #[tokio::test]
    async fn test_full_lifecycle_load_pause_resume_stop() {
        let mgr = session_manager_with_mocks();

        // Load
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();
        let session = mgr.load_session(id).unwrap();
        assert_eq!(session.state, PlayerState::Playing);

        // Pause
        mgr.pause().await.unwrap();
        let session = mgr.load_session(id).unwrap();
        assert_eq!(session.state, PlayerState::Paused);

        // Resume
        mgr.resume().await.unwrap();
        let session = mgr.load_session(id).unwrap();
        assert_eq!(session.state, PlayerState::Playing);

        // Seek
        mgr.seek(60000).await.unwrap();
        let session = mgr.load_session(id).unwrap();
        assert_eq!(session.state, PlayerState::Playing);

        // Set volume
        mgr.set_volume(75).await.unwrap();
        let session = mgr.load_session(id).unwrap();
        assert_eq!(session.volume, 75);

        // Stop
        mgr.stop().await.unwrap();
        assert!(mgr.current_status().await.is_err());
    }

    // ── Resolution failure ───────────────────────────────────────────

    #[tokio::test]
    async fn test_load_resolution_failure_transitions_to_error() {
        let resolver = Arc::new(MockResolver::with_failure());
        let playback = Arc::new(MockPlayback::new());
        let display = Arc::new(MockDisplay::new());
        let tor = Arc::new(MockTor::new());

        let mgr = SessionManager::with_subsystems(
            ":memory:",
            resolver,
            playback,
            display,
            tor,
        ).unwrap();

        let result = mgr.load("https://example.com/broken.mp4").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SessionError::ResolutionFailed(msg) => {
                assert!(msg.contains("mock resolution failure"));
            }
            other => panic!("Expected ResolutionFailed, got {:?}", other),
        }
    }

    // ── PlayerState Display and FromStr ──────────────────────────────

    #[test]
    fn test_player_state_display() {
        assert_eq!(PlayerState::Idle.to_string(), "idle");
        assert_eq!(PlayerState::Resolving.to_string(), "resolving");
        assert_eq!(PlayerState::Buffering.to_string(), "buffering");
        assert_eq!(PlayerState::Playing.to_string(), "playing");
        assert_eq!(PlayerState::Paused.to_string(), "paused");
        assert_eq!(PlayerState::Seeking.to_string(), "seeking");
        assert_eq!(PlayerState::Error.to_string(), "error");
    }

    #[test]
    fn test_player_state_from_str() {
        assert_eq!("idle".parse::<PlayerState>().unwrap(), PlayerState::Idle);
        assert_eq!("playing".parse::<PlayerState>().unwrap(), PlayerState::Playing);
        assert_eq!("paused".parse::<PlayerState>().unwrap(), PlayerState::Paused);
        assert!("invalid".parse::<PlayerState>().is_err());
    }
}
