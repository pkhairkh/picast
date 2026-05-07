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
use tokio::sync::{broadcast, watch};
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

    /// Display subsystem error.
    #[error("display error: {0}")]
    DisplayError(String),

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
            },
            PlayerState::Buffering => {
                matches!(
                    target,
                    PlayerState::Playing
                        | PlayerState::Paused
                        | PlayerState::Error
                        | PlayerState::Idle
                )
            },
            PlayerState::Playing => {
                matches!(
                    target,
                    PlayerState::Paused
                        | PlayerState::Seeking
                        | PlayerState::Buffering
                        | PlayerState::Error
                        | PlayerState::Idle
                )
            },
            PlayerState::Paused => {
                matches!(
                    target,
                    PlayerState::Playing
                        | PlayerState::Seeking
                        | PlayerState::Error
                        | PlayerState::Idle
                )
            },
            PlayerState::Seeking => {
                matches!(target, PlayerState::Playing | PlayerState::Error | PlayerState::Idle)
            },
            PlayerState::Error => matches!(target, PlayerState::Idle),
        }
    }

    /// Attempt a state transition, returning `Ok(target)` if valid
    /// or `Err(SessionError::InvalidTransition)` if not.
    pub fn transition(&self, target: PlayerState) -> Result<PlayerState, SessionError> {
        if self.can_transition_to(target) {
            Ok(target)
        } else {
            Err(SessionError::InvalidTransition { from: *self, to: target })
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
    Resolved { id: Uuid, direct_url: String, title: Option<String> },
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
    PositionUpdate { id: Uuid, position_ms: u64, duration_ms: Option<u64> },
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
    /// Event broadcast channel (for WebSocket streaming — retains history).
    event_tx: broadcast::Sender<SessionEvent>,
    /// Watch channel for latest session state (for HTTP polling — only last value).
    /// Protocol handlers that need the current state without subscribing
    /// to the full event stream can use `watch_rx`.
    watch_tx: watch::Sender<Option<MediaSession>>,
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

        // Enable WAL journal mode for better concurrent read performance.
        // WAL allows readers to operate without blocking writers, which is
        // essential when HTTP, WebSocket, and DLNA handlers all access the
        // database concurrently.
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;

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
        let (watch_tx, _) = watch::channel(None);

        let mgr = Self {
            db: Mutex::new(conn),
            active_session_id: Arc::new(Mutex::new(None)),
            event_tx,
            watch_tx,
            resolver: None,
            playback: None,
            display: None,
            tor: None,
        };

        // Clean up stale sessions from a previous run that may have crashed.
        // Sessions older than 24 hours are deleted, and any session left in
        // a non-idle state (e.g. Playing, Buffering) is reset to Idle because
        // the playback pipeline is gone after a process restart.
        mgr.cleanup_stale_sessions()?;
        mgr.recover_crashed_sessions()?;

        Ok(mgr)
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
        let db = self
            .db
            .lock()
            .map_err(|e| SessionError::Subsystem(format!("db lock poisoned: {}", e)))?;

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
            return Err(SessionError::InvalidTransition { from: current_state, to: target });
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
            PlayerState::Error => {
                SessionEvent::Error { id: session_id, message: "entered error state".into() }
            },
            PlayerState::Buffering => {
                SessionEvent::Buffering { id: session_id, percent: 0 }
            },
            _ => return Ok(target), // No broadcast for Seeking here
        };
        let _ = self.event_tx.send(event);

        Ok(target)
    }

    /// Insert a session into the database.
    pub fn insert_session(&self, session: &MediaSession) -> Result<(), SessionError> {
        let db = self
            .db
            .lock()
            .map_err(|e| SessionError::Subsystem(format!("db lock poisoned: {}", e)))?;
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
        let db = self
            .db
            .lock()
            .map_err(|e| SessionError::Subsystem(format!("db lock poisoned: {}", e)))?;
        let now = Utc::now().to_rfc3339();
        let sql = format!("UPDATE sessions SET {} = ?, updated_at = ? WHERE id = ?", field);
        db.execute(&sql, rusqlite::params![value, now, session_id.to_string()])?;
        Ok(())
    }

    /// Load a session from the database by ID.
    pub fn load_session(&self, id: Uuid) -> Result<MediaSession, SessionError> {
        let db = self
            .db
            .lock()
            .map_err(|e| SessionError::Subsystem(format!("db lock poisoned: {}", e)))?;

        // Collect raw values from the row inside the closure (which returns
        // Result<_, rusqlite::Error>). Parsing/validation is done outside the
        // closure so we can return SessionError instead of rusqlite::Error.
        let raw = db
            .query_row(
                "SELECT id, source_url, resolved_url, state, position_ms, duration_ms,
                    volume, title, created_at, updated_at
             FROM sessions WHERE id = ?1",
                rusqlite::params![id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                        row.get::<_, i32>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                    ))
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => SessionError::NotFound(id),
                other => SessionError::Database(other),
            })?;

        let (id_str, source_url, resolved_url, state_str, position_ms, duration_ms, volume, title, created_at_str, updated_at_str) = raw;

        let id = Uuid::parse_str(&id_str).map_err(|e| {
            SessionError::Subsystem(format!("corrupt session ID '{}': {}", id_str, e))
        })?;
        let state: PlayerState = state_str.parse().map_err(|e| {
            SessionError::Subsystem(format!("corrupt session state '{}': {}", state_str, e))
        })?;
        let volume_u8 = if !(0..=255).contains(&volume) {
            tracing::warn!(volume = volume, "corrupt volume in DB — clamping to 100");
            100u8
        } else {
            volume as u8
        };
        let created_at = created_at_str.parse::<DateTime<Utc>>().map_err(|e| {
            SessionError::Subsystem(format!("corrupt created_at '{}': {}", created_at_str, e))
        })?;
        let updated_at = updated_at_str.parse::<DateTime<Utc>>().map_err(|e| {
            SessionError::Subsystem(format!("corrupt updated_at '{}': {}", updated_at_str, e))
        })?;

        Ok(MediaSession {
            id,
            source_url,
            resolved_url,
            state,
            position_ms: position_ms as u64,
            duration_ms: duration_ms.map(|d| d as u64),
            volume: volume_u8,
            title,
            created_at,
            updated_at,
        })
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
            let guard = self
                .active_session_id
                .lock()
                .map_err(|e| SessionError::Subsystem(format!("lock poisoned: {}", e)))?;
            guard.ok_or(SessionError::NoActiveSession)?
        };
        self.load_session(id)
    }

    /// Set the ALSA audio device for the next playback pipeline.
    ///
    /// The change takes effect on the next `play()` call and does not
    /// affect a currently-running pipeline.
    pub async fn set_audio_device(&self, device: String) -> Result<(), SessionError> {
        let playback = self
            .playback
            .as_ref()
            .ok_or(SessionError::Subsystem("playback not configured".into()))?;
        playback
            .set_audio_device(device)
            .await
            .map_err(|e| SessionError::Subsystem(format!("set_audio_device: {}", e)))
    }

    /// Get the current ALSA audio device string.
    pub async fn audio_device(&self) -> Result<String, SessionError> {
        let playback = self
            .playback
            .as_ref()
            .ok_or(SessionError::Subsystem("playback not configured".into()))?;
        playback
            .audio_device()
            .await
            .map_err(|e| SessionError::Subsystem(format!("audio_device: {}", e)))
    }

    /// Subscribe to session events (broadcast channel).
    ///
    /// Use this for real-time event streaming (e.g. WebSocket server).
    /// The broadcast channel retains a configurable number of messages
    /// so slow consumers may miss events if they lag.
    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.event_tx.subscribe()
    }

    /// Subscribe to the latest session state (watch channel).
    ///
    /// Use this for polling the current state without subscribing to
    /// the full event stream. The watch channel only keeps the most
    /// recent value, so readers always get the latest snapshot.
    /// Returns `None` when no session is active.
    pub fn subscribe_state(&self) -> watch::Receiver<Option<MediaSession>> {
        self.watch_tx.subscribe()
    }

    /// Get a clone of the current watch sender for external use.
    /// Useful for wiring into protocol servers that need to push
    /// state updates.
    pub fn state_sender(&self) -> watch::Sender<Option<MediaSession>> {
        self.watch_tx.clone()
    }

    // ── Command methods ───────────────────────────────────────────────

    /// Load a new media URL and start the resolution/playback flow.
    ///
    /// This method:
    /// 1. Validates that no session is already active (PiCast is single-session).
    /// 2. Creates a new `MediaSession` in the `Idle` state.
    /// 3. Transitions to `Resolving` and calls the resolver subsystem.
    /// 4. On successful resolution, transitions to `Buffering`, acquires
    ///    the display, and starts playback through the Tor SOCKS proxy.
    /// 5. Transitions to `Playing` when the pipeline is confirmed active.
    ///
    /// Returns the new session's UUID on success.
    pub async fn load(&self, url: &str) -> Result<Uuid, SessionError> {
        // Atomically check no active session and reserve the slot.
        let session_id = {
            let mut guard = self
                .active_session_id
                .lock()
                .map_err(|e| SessionError::Subsystem(format!("lock poisoned: {}", e)))?;
            if guard.is_some() {
                return Err(SessionError::AlreadyActive);
            }
            let id = Uuid::new_v4();
            *guard = Some(id);
            id
        };

        // Create the session in Idle state (outside the lock to avoid holding it during DB I/O).
        let mut session = MediaSession::new(url.to_owned());
        session.id = session_id;
        session.state = PlayerState::Idle;

        // If DB insert fails, clear the reserved slot.
        if let Err(e) = self.insert_session(&session) {
            {
                let mut guard = self
                    .active_session_id
                    .lock()
                    .map_err(|e2| SessionError::Subsystem(format!("lock poisoned: {}", e2)))?;
                *guard = None;
            }
            return Err(e);
        }

        let id = session.id;

        // Broadcast: session created.
        let _ = self.event_tx.send(SessionEvent::Created { id, url: url.to_owned() });

        // Transition: Idle → Resolving.
        self.try_transition(id, PlayerState::Resolving)?;

        // Resolve the URL via the resolver subsystem.
        let resolve_info = if let Some(ref resolver) = self.resolver {
            resolver.resolve(url).await.map_err(|e| {
                // Transition to Error state on resolution failure.
                let _ = self.try_transition(id, PlayerState::Error);
                let _ = self.clear_active_session();
                SessionError::ResolutionFailed(e.to_string())
            })?
        } else {
            // Without a resolver, treat the URL as a direct media URL.
            tracing::warn!("no resolver subsystem — using URL as direct media");
            interfaces::ResolveInfo {
                direct_url: url.to_owned(),
                title: None,
                duration_ms: None,
            }
        };

        // Update the session with the resolved URL, title, and duration.
        {
            let db = self
                .db
                .lock()
                .map_err(|e| SessionError::Subsystem(format!("db lock poisoned: {}", e)))?;
            let now = Utc::now().to_rfc3339();
            db.execute(
                "UPDATE sessions SET resolved_url = ?1, title = ?2, duration_ms = ?3, updated_at = ?4 WHERE id = ?5",
                rusqlite::params![
                    resolve_info.direct_url,
                    resolve_info.title,
                    resolve_info.duration_ms.map(|d| d as i64),
                    now,
                    id.to_string(),
                ],
            )?;
        }

        // Broadcast: resolved.
        let _ = self.event_tx.send(SessionEvent::Resolved {
            id,
            direct_url: resolve_info.direct_url.clone(),
            title: resolve_info.title.clone(),
        });

        // Acquire the display before starting playback.
        //
        // The DLNA sync stops gmediarender when the Resolving event fires
        // (on a separate task), but the kernel may not have released DRM
        // master yet by the time we reach here.  A brief pause gives the
        // kernel time to clean up after gmediarender's exit so that
        // DisplayManager::acquire() can obtain DRM master.
        //
        // This is a conservative safety net — the display manager already
        // retries internally with exponential backoff, but the extra delay
        // here ensures the race window is closed before we even start
        // trying.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        if let Some(ref display) = self.display {
            display.acquire().await.map_err(|e| {
                tracing::error!(error = %e, "display acquire failed");
                let _ = self.try_transition(id, PlayerState::Error);
                let _ = self.clear_active_session();
                SessionError::DisplayError(e.to_string())
            })?;
        }

        // Transition: Resolving → Buffering.
        self.try_transition(id, PlayerState::Buffering)?;

        // Start playback via the playback subsystem.
        if let Some(ref playback) = self.playback {
            let socks_addr = self.tor.as_ref().map(|t| t.socks_addr()).unwrap_or_default();
            let isolation_username = self
                .tor
                .as_ref()
                .map(|t| {
                    t.isolation_username(
                        url::Url::parse(url)
                            .ok()
                            .and_then(|u| u.host_str().map(|h| h.to_owned()))
                            .unwrap_or_default()
                            .as_str(),
                    )
                })
                .unwrap_or_default();

            playback.play(&resolve_info.direct_url, url, &socks_addr, &isolation_username).await.map_err(|e| SessionError::PlaybackError(e.to_string())).inspect_err(|_| {
                // Transition to Error state on playback failure.
                let _ = self.try_transition(id, PlayerState::Error);
                let _ = self.clear_active_session();
            })?;
        }

        // Transition: Buffering → Playing.
        self.try_transition(id, PlayerState::Playing)?;

        // Refresh position/duration from playback subsystem.
        self.refresh_playback_position(id).await;

        // Push latest state to watch channel.
        if let Ok(session) = self.load_session(id) {
            self.broadcast_state_update(&session);
        }

        Ok(id)
    }

    /// Pause playback on the current session.
    ///
    /// Valid only when the session is in the `Playing` state.
    pub async fn pause(&self) -> Result<(), SessionError> {
        let id = self.active_session_id()?;

        // Delegate to playback subsystem first — only update DB state on success.
        if let Some(ref playback) = self.playback {
            playback.pause().await.map_err(|e| SessionError::PlaybackError(e.to_string()))?;
        }

        // Transition: Playing → Paused (after subsystem confirms success).
        self.try_transition(id, PlayerState::Paused)?;

        // Refresh position/duration from playback subsystem.
        self.refresh_playback_position(id).await;

        // Push latest state to watch channel.
        if let Ok(session) = self.load_session(id) {
            self.broadcast_state_update(&session);
        }

        Ok(())
    }

    /// Resume playback on the current session.
    ///
    /// Valid only when the session is in the `Paused` state.
    pub async fn resume(&self) -> Result<(), SessionError> {
        let id = self.active_session_id()?;

        // Delegate to playback subsystem first — only update DB state on success.
        if let Some(ref playback) = self.playback {
            playback.resume().await.map_err(|e| SessionError::PlaybackError(e.to_string()))?;
        }

        // Transition: Paused → Playing (after subsystem confirms success).
        self.try_transition(id, PlayerState::Playing)?;

        // Refresh position/duration from playback subsystem.
        self.refresh_playback_position(id).await;

        // Push latest state to watch channel.
        if let Ok(session) = self.load_session(id) {
            self.broadcast_state_update(&session);
        }

        Ok(())
    }

    /// Stop playback and destroy the current session.
    ///
    /// Can be called from any active state. Transitions to `Idle`,
    /// stops the playback pipeline, releases the display, and clears
    /// the active session.
    pub async fn stop(&self) -> Result<(), SessionError> {
        let id = self.active_session_id()?;

        // Load current state to check what transition is needed.
        let session = self.load_session(id)?;

        // If we're in Playing/Paused/Buffering/Seeking, stop the pipeline first.
        if matches!(
            session.state,
            PlayerState::Playing | PlayerState::Paused | PlayerState::Buffering | PlayerState::Seeking
        ) {
            if let Some(ref playback) = self.playback {
                let _ = playback.stop().await;
            }
        }

        // Release the display (best-effort — log but don't fail).
        if let Some(ref display) = self.display {
            if let Err(e) = display.release().await {
                tracing::warn!(error = %e, "display release failed during stop — continuing");
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
            let mut guard = self
                .active_session_id
                .lock()
                .map_err(|e| SessionError::Subsystem(format!("lock poisoned: {}", e)))?;
            *guard = None;
        }

        // Delete the session from the database.
        self.delete_session(id)?;

        // Push idle state to watch channel.
        self.broadcast_idle();

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
        let _ = self.event_tx.send(SessionEvent::Seeking { id, position_ms });

        // Delegate to playback subsystem first — only update DB state on success.
        if let Some(ref playback) = self.playback {
            playback
                .seek(position_ms)
                .await
                .map_err(|e| SessionError::PlaybackError(e.to_string()))?;
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

        // Refresh position/duration from playback subsystem.
        self.refresh_playback_position(id).await;

        // Push latest state to watch channel.
        if let Ok(session) = self.load_session(id) {
            self.broadcast_state_update(&session);
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
            let db = self
                .db
                .lock()
                .map_err(|e| SessionError::Subsystem(format!("db lock poisoned: {}", e)))?;
            let now = Utc::now().to_rfc3339();
            db.execute(
                "UPDATE sessions SET volume = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![clamped as i32, now, id.to_string()],
            )?;
        }

        // Delegate to playback subsystem.
        if let Some(ref playback) = self.playback {
            playback
                .set_volume(clamped as f64 / 100.0)
                .await
                .map_err(|e| SessionError::PlaybackError(e.to_string()))?;
        }

        // Broadcast volume change event.
        let _ = self.event_tx.send(SessionEvent::VolumeChanged { id, volume: clamped });

        // Push latest state to watch channel.
        if let Ok(session) = self.load_session(id) {
            self.broadcast_state_update(&session);
        }

        Ok(())
    }

    // ── Private helpers ──────────────────────────────────────────────

    /// Get the active session ID or return NoActiveSession.
    fn active_session_id(&self) -> Result<Uuid, SessionError> {
        let guard = self
            .active_session_id
            .lock()
            .map_err(|e| SessionError::Subsystem(format!("lock poisoned: {}", e)))?;
        guard.ok_or(SessionError::NoActiveSession)
    }

    /// Clear the active session ID (used on error/cleanup paths).
    fn clear_active_session(&self) -> Result<(), SessionError> {
        let mut guard = self
            .active_session_id
            .lock()
            .map_err(|e| SessionError::Subsystem(format!("lock poisoned: {}", e)))?;
        *guard = None;
        Ok(())
    }

    // ── Public helpers (for main.rs background tasks) ───────────────

    /// Get the active session ID as an async-compatible public method.
    ///
    /// Used by the background position-update task in `main.rs`.
    pub async fn active_session_id_public(&self) -> Result<Uuid, SessionError> {
        self.active_session_id()
    }

    /// Refresh position and duration from the playback subsystem and
    /// broadcast a [`SessionEvent::PositionUpdate`].
    ///
    /// This is the public wrapper around the private
    /// `refresh_playback_position` method, exposed so that the
    /// background position-update task in `main.rs` can call it.
    pub async fn refresh_playback_position_public(&self, session_id: Uuid) {
        self.refresh_playback_position(session_id).await;
    }

    /// Delete a session from SQLite.
    fn delete_session(&self, id: Uuid) -> Result<(), SessionError> {
        let db = self
            .db
            .lock()
            .map_err(|e| SessionError::Subsystem(format!("db lock poisoned: {}", e)))?;
        db.execute("DELETE FROM sessions WHERE id = ?1", rusqlite::params![id.to_string()])?;
        Ok(())
    }

    /// Broadcast the current session state through the watch channel.
    ///
    /// Call this after any state change (load, pause, stop, seek, etc.)
    /// so that `subscribe_state()` receivers see the latest snapshot.
    fn broadcast_state_update(&self, session: &MediaSession) {
        let _ = self.watch_tx.send(Some(session.clone()));
    }

    /// Broadcast that no session is active (cleared watch channel).
    fn broadcast_idle(&self) {
        let _ = self.watch_tx.send(None);
    }

    /// Refresh position and duration from the playback subsystem and
    /// persist them to the database. Also emits a
    /// [`SessionEvent::PositionUpdate`] and updates the watch channel.
    ///
    /// This should be called after operations that change playback state
    /// (pause, resume, seek) so that the stored position/duration stays
    /// roughly in sync with the pipeline.
    async fn refresh_playback_position(&self, session_id: Uuid) {
        let pos = if let Some(ref playback) = self.playback {
            playback.position_ms().await.ok()
        } else {
            None
        };
        let dur = if let Some(ref playback) = self.playback {
            playback.duration_ms().await.ok().flatten()
        } else {
            None
        };

        if pos.is_none() && dur.is_none() {
            return;
        }

        // Update the database.
        {
            let db = match self.db.lock() {
                Ok(guard) => guard,
                Err(e) => {
                    tracing::warn!(error = %e, "db lock poisoned in refresh_playback_position");
                    return;
                },
            };
            let now = Utc::now().to_rfc3339();
            if let Some(p) = pos {
                let _ = db.execute(
                    "UPDATE sessions SET position_ms = ?1, updated_at = ?2 WHERE id = ?3",
                    rusqlite::params![p as i64, now, session_id.to_string()],
                );
            }
            if let Some(d) = dur {
                let _ = db.execute(
                    "UPDATE sessions SET duration_ms = ?1, updated_at = ?2 WHERE id = ?3",
                    rusqlite::params![d as i64, now, session_id.to_string()],
                );
            }
        }

        // Emit position update event.
        let _ = self.event_tx.send(SessionEvent::PositionUpdate {
            id: session_id,
            position_ms: pos.unwrap_or(0),
            duration_ms: dur,
        });

        // Update watch channel with latest state.
        if let Ok(session) = self.load_session(session_id) {
            self.broadcast_state_update(&session);
        }
    }

    /// Delete sessions older than 24 hours.
    ///
    /// Called during `SessionManager::new()` to keep the database
    /// from growing unbounded. Stale sessions are those whose
    /// `updated_at` timestamp is more than 24 hours in the past.
    fn cleanup_stale_sessions(&self) -> Result<(), SessionError> {
        let db = self
            .db
            .lock()
            .map_err(|e| SessionError::Subsystem(format!("db lock poisoned: {}", e)))?;
        let deleted = db.execute(
            "DELETE FROM sessions WHERE updated_at < strftime('%Y-%m-%dT%H:%M:%S+00:00', 'now', '-24 hours')",
            [],
        )?;
        if deleted > 0 {
            tracing::info!(count = deleted, "cleaned up stale sessions");
        }
        Ok(())
    }

    /// Recover sessions left in a non-idle state after a crash.
    ///
    /// If PiCast crashes while a session is in `Playing`, `Buffering`,
    /// `Resolving`, or `Seeking`, the playback pipeline is gone but
    /// the database row still exists. We reset all such sessions to
    /// `Idle` so they don't block new sessions from being created.
    fn recover_crashed_sessions(&self) -> Result<(), SessionError> {
        let db = self
            .db
            .lock()
            .map_err(|e| SessionError::Subsystem(format!("db lock poisoned: {}", e)))?;

        // Find sessions in non-terminal states.
        let crashed: Vec<(String, String)> = {
            let mut stmt =
                db.prepare("SELECT id, state FROM sessions WHERE state NOT IN ('idle', 'error')")?;
            let rows =
                stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
            rows.filter_map(|r| r.ok()).collect()
        };

        if !crashed.is_empty() {
            let now = Utc::now().to_rfc3339();
            for (id, old_state) in &crashed {
                tracing::warn!(
                    session_id = %id,
                    old_state = %old_state,
                    "recovering crashed session — resetting to idle"
                );
                db.execute(
                    "UPDATE sessions SET state = 'idle', updated_at = ?1 WHERE id = ?2",
                    rusqlite::params![now, id],
                )?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use interfaces::{DisplayTrait, PlaybackTrait, ResolverTrait, TorTrait};
    use std::sync::atomic::{AtomicBool, AtomicU16, Ordering as AtomicOrdering};

    // ── Mock subsystems ──────────────────────────────────────────────

    /// Mock resolver that returns a predictable direct URL with metadata.
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
        async fn resolve(
            &self,
            url: &str,
        ) -> Result<interfaces::ResolveInfo, Box<dyn std::error::Error + Send + Sync>> {
            if self.should_fail.load(AtomicOrdering::Relaxed) {
                return Err("mock resolution failure".into());
            }
            Ok(interfaces::ResolveInfo {
                direct_url: format!("{}?direct=1", url),
                title: Some("Mock Title".to_string()),
                duration_ms: Some(300000),
            })
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
        async fn play(
            &self,
            _url: &str,
            _source_url: &str,
            _socks_addr: &str,
            _isolation_username: &str,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
        async fn seek(
            &self,
            position_ms: u64,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.inc_call("seek");
            *self.last_seek_ms.lock().unwrap() = position_ms;
            Ok(())
        }
        async fn set_volume(
            &self,
            volume: f64,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.inc_call("set_volume");
            *self.volume.lock().unwrap() = volume;
            Ok(())
        }
        async fn position_ms(&self) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
            Ok(0)
        }
        async fn duration_ms(
            &self,
        ) -> Result<Option<u64>, Box<dyn std::error::Error + Send + Sync>> {
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

        SessionManager::with_subsystems(":memory:", resolver, playback, display, tor).unwrap()
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
            },
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
        // Title should be populated from resolver.
        assert_eq!(session.title, Some("Mock Title".to_string()));
        // Duration should be populated from resolver.
        assert_eq!(session.duration_ms, Some(300000));
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
        // Without resolver, title and duration are None.
        assert!(session.title.is_none());
        assert!(session.duration_ms.is_none());
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

        // Should receive Created, Resolving, Resolved, Buffering, Playing events.
        let mut received = Vec::new();
        while let Ok(event) = rx.try_recv() {
            received.push(event);
        }

        assert!(
            received.iter().any(|e| matches!(e, SessionEvent::Created { .. })),
            "should have Created event"
        );
        assert!(
            received.iter().any(|e| matches!(e, SessionEvent::Resolving { .. })),
            "should have Resolving event"
        );
        assert!(
            received.iter().any(|e| matches!(e, SessionEvent::Resolved { .. })),
            "should have Resolved event"
        );
        assert!(
            received.iter().any(|e| matches!(e, SessionEvent::Buffering { .. })),
            "should have Buffering event"
        );
        assert!(
            received.iter().any(|e| matches!(e, SessionEvent::Playing { .. })),
            "should have Playing event"
        );
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

        let mgr =
            SessionManager::with_subsystems(":memory:", resolver, playback, display, tor).unwrap();

        let result = mgr.load("https://example.com/broken.mp4").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SessionError::ResolutionFailed(msg) => {
                assert!(msg.contains("mock resolution failure"));
            },
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

    // ── T-5.5: Watch channel tests ────────────────────────────────────

    #[tokio::test]
    async fn test_watch_channel_receives_state_after_load() {
        let mgr = Arc::new(session_manager_with_mocks());
        let state_rx = mgr.subscribe_state();

        // Initially None (no session active).
        assert!(state_rx.borrow().is_none());

        // Load a URL — watch channel should update to Some(session).
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();
        // Allow the watch channel to propagate.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let state = state_rx.borrow().clone();
        assert!(state.is_some());
        let session = state.unwrap();
        assert_eq!(session.id, id);
        assert_eq!(session.state, PlayerState::Playing);
    }

    #[tokio::test]
    async fn test_watch_channel_updates_on_pause() {
        let mgr = Arc::new(session_manager_with_mocks());
        let state_rx = mgr.subscribe_state();

        let _ = mgr.load("https://example.com/video.mp4").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        mgr.pause().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let state = state_rx.borrow().clone();
        assert!(state.is_some());
        assert_eq!(state.unwrap().state, PlayerState::Paused);
    }

    #[tokio::test]
    async fn test_watch_channel_clears_on_stop() {
        let mgr = Arc::new(session_manager_with_mocks());
        let state_rx = mgr.subscribe_state();

        let _ = mgr.load("https://example.com/video.mp4").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        mgr.stop().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        assert!(state_rx.borrow().is_none());
    }

    #[tokio::test]
    async fn test_watch_channel_volume_update() {
        let mgr = Arc::new(session_manager_with_mocks());
        let state_rx = mgr.subscribe_state();

        let _ = mgr.load("https://example.com/video.mp4").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        mgr.set_volume(50).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let state = state_rx.borrow().clone();
        assert!(state.is_some());
        assert_eq!(state.unwrap().volume, 50);
    }

    #[tokio::test]
    async fn test_watch_channel_seek_update() {
        let mgr = Arc::new(session_manager_with_mocks());
        let state_rx = mgr.subscribe_state();

        let _ = mgr.load("https://example.com/video.mp4").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        mgr.seek(30000).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let state = state_rx.borrow().clone();
        assert!(state.is_some());
        // After seek, should be back in Playing state.
        assert_eq!(state.unwrap().state, PlayerState::Playing);
    }

    #[tokio::test]
    async fn test_state_sender_clone() {
        let mgr = Arc::new(session_manager_with_mocks());
        // Keep a receiver alive so send() succeeds (watch::Sender::send
        // returns Err when there are no receivers).
        let _state_rx = mgr.subscribe_state();
        let sender = mgr.state_sender();

        // Should be able to send a state update through the cloned sender.
        let session = MediaSession::new("test://example".into());
        assert!(sender.send(Some(session)).is_ok());
    }

    // ── T-5.6: Session cleanup & persistence tests ─────────────────────

    #[test]
    fn test_cleanup_stale_sessions() {
        let mgr = session_manager_bare();

        // Insert a session with an old updated_at timestamp.
        let mut old_session = MediaSession::new("https://example.com/old.mp4".into());
        old_session.updated_at = Utc::now() - chrono::Duration::hours(25);
        mgr.insert_session(&old_session).unwrap();

        // Insert a recent session.
        let recent_session = MediaSession::new("https://example.com/recent.mp4".into());
        mgr.insert_session(&recent_session).unwrap();

        // Run cleanup.
        mgr.cleanup_stale_sessions().unwrap();

        // Old session should be gone, recent session should remain.
        assert!(mgr.load_session(old_session.id).is_err());
        assert!(mgr.load_session(recent_session.id).is_ok());
    }

    #[test]
    fn test_recover_crashed_sessions() {
        let mgr = session_manager_bare();

        // Insert sessions in various non-idle states.
        let mut playing_session = MediaSession::new("https://example.com/playing.mp4".into());
        playing_session.state = PlayerState::Playing;
        mgr.insert_session(&playing_session).unwrap();

        let mut buffering_session = MediaSession::new("https://example.com/buffering.mp4".into());
        buffering_session.state = PlayerState::Buffering;
        mgr.insert_session(&buffering_session).unwrap();

        let mut resolving_session = MediaSession::new("https://example.com/resolving.mp4".into());
        resolving_session.state = PlayerState::Resolving;
        mgr.insert_session(&resolving_session).unwrap();

        // Idle and error sessions should NOT be touched.
        let mut idle_session = MediaSession::new("https://example.com/idle.mp4".into());
        idle_session.state = PlayerState::Idle;
        mgr.insert_session(&idle_session).unwrap();

        let mut error_session = MediaSession::new("https://example.com/error.mp4".into());
        error_session.state = PlayerState::Error;
        mgr.insert_session(&error_session).unwrap();

        // Run recovery.
        mgr.recover_crashed_sessions().unwrap();

        // All non-idle/error sessions should now be Idle.
        assert_eq!(mgr.load_session(playing_session.id).unwrap().state, PlayerState::Idle);
        assert_eq!(mgr.load_session(buffering_session.id).unwrap().state, PlayerState::Idle);
        assert_eq!(mgr.load_session(resolving_session.id).unwrap().state, PlayerState::Idle);

        // Idle and error sessions should be unchanged.
        assert_eq!(mgr.load_session(idle_session.id).unwrap().state, PlayerState::Idle);
        assert_eq!(mgr.load_session(error_session.id).unwrap().state, PlayerState::Error);
    }

    #[test]
    fn test_wal_mode_enabled() {
        let mgr = session_manager_bare();
        let db = mgr.db.lock().unwrap();
        let journal_mode: String =
            db.query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap();
        // In-memory databases use "memory" mode; file databases use "wal".
        // For :memory: we just check that the PRAGMA doesn't error.
        assert!(journal_mode == "wal" || journal_mode == "memory");
    }

    #[test]
    fn test_new_manager_cleans_up_on_start() {
        // This test verifies that SessionManager::new() calls cleanup and recovery.
        // If a stale session exists in the DB when we open it, it should be removed.
        let mgr = session_manager_bare();

        // Insert a stale session (25h old).
        let mut stale = MediaSession::new("https://example.com/stale.mp4".into());
        stale.updated_at = Utc::now() - chrono::Duration::hours(25);
        mgr.insert_session(&stale).unwrap();
        assert!(mgr.load_session(stale.id).is_ok());

        // Create a new manager pointing to the same :memory: DB won't work
        // (each :memory: is separate). Instead, verify the method directly.
        mgr.cleanup_stale_sessions().unwrap();
        assert!(mgr.load_session(stale.id).is_err());
    }

    // ── T-5.7: Thread safety / concurrent access tests ─────────────────

    #[tokio::test]
    async fn test_concurrent_load_rejected() {
        let mgr = Arc::new(session_manager_with_mocks());

        // Start a load.
        let mgr1 = mgr.clone();
        let handle1 =
            tokio::spawn(async move { mgr1.load("https://example.com/video1.mp4").await });

        // Allow first load to complete.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Second concurrent load should be rejected.
        let result = mgr.load("https://example.com/video2.mp4").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SessionError::AlreadyActive => {},
            other => panic!("expected AlreadyActive, got: {:?}", other),
        }

        let _ = handle1.await;
    }

    #[tokio::test]
    async fn test_concurrent_pause_and_resume() {
        let mgr = Arc::new(session_manager_with_mocks());
        let _ = mgr.load("https://example.com/video.mp4").await.unwrap();

        // Both pause and resume can race; one should succeed, the other may fail.
        let mgr1 = mgr.clone();
        let mgr2 = mgr.clone();

        let handle1 = tokio::spawn(async move { mgr1.pause().await });
        let handle2 = tokio::spawn(async move { mgr2.resume().await });

        // At least one should succeed (pause after load is valid).
        let r1 = handle1.await.unwrap();
        let r2 = handle2.await.unwrap();
        assert!(r1.is_ok() || r2.is_ok(), "at least one concurrent operation should succeed");

        // The session should still be in a valid state.
        let status = mgr.current_status().await.unwrap();
        assert!(matches!(status.state, PlayerState::Playing | PlayerState::Paused));
    }

    #[tokio::test]
    async fn test_sequential_load_stop_load() {
        let mgr = Arc::new(session_manager_with_mocks());

        // First load.
        let id1 = mgr.load("https://example.com/video1.mp4").await.unwrap();

        // Stop.
        mgr.stop().await.unwrap();

        // Second load should succeed after stop.
        let id2 = mgr.load("https://example.com/video2.mp4").await.unwrap();
        assert_ne!(id1, id2);
    }

    #[tokio::test]
    async fn test_concurrent_set_volume_safe() {
        let mgr = Arc::new(session_manager_with_mocks());
        let _ = mgr.load("https://example.com/video.mp4").await.unwrap();

        // Fire off 10 concurrent set_volume calls.
        let mut handles = Vec::new();
        for i in 0..10u8 {
            let m = mgr.clone();
            handles.push(tokio::spawn(async move { m.set_volume(i * 10).await }));
        }

        // All should succeed (set_volume doesn't change state).
        for handle in handles {
            assert!(handle.await.unwrap().is_ok());
        }

        // Final volume should be some valid value 0-100.
        let status = mgr.current_status().await.unwrap();
        assert!(status.volume <= 100);
    }

    #[tokio::test]
    async fn test_concurrent_status_reads() {
        let mgr = Arc::new(session_manager_with_mocks());
        let _ = mgr.load("https://example.com/video.mp4").await.unwrap();

        // Spawn many concurrent status readers.
        let mut handles = Vec::new();
        for _ in 0..20 {
            let m = mgr.clone();
            handles.push(tokio::spawn(async move { m.current_status().await }));
        }

        // All should succeed without data races.
        for handle in handles {
            let result = handle.await.unwrap();
            assert!(result.is_ok());
            assert_eq!(result.unwrap().state, PlayerState::Playing);
        }
    }

    // ══════════════════════════════════════════════════════════════════
    // NEW COMPREHENSIVE TESTS
    // ══════════════════════════════════════════════════════════════════

    // ── 1. State transition validation ───────────────────────────────

    #[test]
    fn test_idle_to_resolving_valid() {
        assert!(PlayerState::Idle.can_transition_to(PlayerState::Resolving));
        assert!(PlayerState::Idle.transition(PlayerState::Resolving).is_ok());
    }

    #[test]
    fn test_idle_to_playing_invalid() {
        assert!(!PlayerState::Idle.can_transition_to(PlayerState::Playing));
        let err = PlayerState::Idle.transition(PlayerState::Playing).unwrap_err();
        assert!(matches!(
            err,
            SessionError::InvalidTransition { from: PlayerState::Idle, to: PlayerState::Playing }
        ));
    }

    #[test]
    fn test_playing_to_paused_valid() {
        assert!(PlayerState::Playing.can_transition_to(PlayerState::Paused));
        assert!(PlayerState::Playing.transition(PlayerState::Paused).is_ok());
    }

    #[test]
    fn test_playing_to_seeking_valid() {
        assert!(PlayerState::Playing.can_transition_to(PlayerState::Seeking));
        assert!(PlayerState::Playing.transition(PlayerState::Seeking).is_ok());
    }

    #[test]
    fn test_paused_to_playing_valid() {
        assert!(PlayerState::Paused.can_transition_to(PlayerState::Playing));
        assert!(PlayerState::Paused.transition(PlayerState::Playing).is_ok());
    }

    #[test]
    fn test_paused_to_seeking_valid() {
        assert!(PlayerState::Paused.can_transition_to(PlayerState::Seeking));
        assert!(PlayerState::Paused.transition(PlayerState::Seeking).is_ok());
    }

    #[test]
    fn test_seeking_to_playing_valid() {
        assert!(PlayerState::Seeking.can_transition_to(PlayerState::Playing));
        assert!(PlayerState::Seeking.transition(PlayerState::Playing).is_ok());
    }

    #[test]
    fn test_seeking_to_paused_invalid() {
        assert!(!PlayerState::Seeking.can_transition_to(PlayerState::Paused));
        let err = PlayerState::Seeking.transition(PlayerState::Paused).unwrap_err();
        assert!(matches!(
            err,
            SessionError::InvalidTransition { from: PlayerState::Seeking, to: PlayerState::Paused }
        ));
    }

    #[test]
    fn test_error_to_idle_valid() {
        assert!(PlayerState::Error.can_transition_to(PlayerState::Idle));
        assert!(PlayerState::Error.transition(PlayerState::Idle).is_ok());
    }

    #[test]
    fn test_error_to_playing_invalid() {
        assert!(!PlayerState::Error.can_transition_to(PlayerState::Playing));
        let err = PlayerState::Error.transition(PlayerState::Playing).unwrap_err();
        assert!(matches!(
            err,
            SessionError::InvalidTransition { from: PlayerState::Error, to: PlayerState::Playing }
        ));
    }

    // ── 2. Session lifecycle with mock subsystems ────────────────────

    #[tokio::test]
    async fn test_lifecycle_load_with_mock_resolver_succeeds() {
        let mgr = session_manager_with_mocks();
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        let session = mgr.load_session(id).unwrap();
        assert_eq!(session.state, PlayerState::Playing);
        assert_eq!(
            session.resolved_url,
            Some("https://example.com/video.mp4?direct=1".to_string())
        );
    }

    #[tokio::test]
    async fn test_lifecycle_load_returns_already_active_if_called_twice() {
        let mgr = session_manager_with_mocks();
        let _ = mgr.load("https://example.com/video1.mp4").await.unwrap();
        let result = mgr.load("https://example.com/video2.mp4").await;
        assert!(matches!(result.unwrap_err(), SessionError::AlreadyActive));
    }

    #[tokio::test]
    async fn test_lifecycle_pause_after_load() {
        let mgr = session_manager_with_mocks();
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        mgr.pause().await.unwrap();
        let session = mgr.load_session(id).unwrap();
        assert_eq!(session.state, PlayerState::Paused);
    }

    #[tokio::test]
    async fn test_lifecycle_resume_after_pause() {
        let mgr = session_manager_with_mocks();
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        mgr.pause().await.unwrap();
        assert_eq!(mgr.load_session(id).unwrap().state, PlayerState::Paused);

        mgr.resume().await.unwrap();
        assert_eq!(mgr.load_session(id).unwrap().state, PlayerState::Playing);
    }

    #[tokio::test]
    async fn test_lifecycle_stop_transitions_to_idle() {
        let mgr = session_manager_with_mocks();
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        // Before stop, session exists and is Playing.
        assert_eq!(mgr.load_session(id).unwrap().state, PlayerState::Playing);

        mgr.stop().await.unwrap();

        // After stop, session is deleted from DB and no active session.
        assert!(mgr.load_session(id).is_err());
        assert!(mgr.current_status().await.is_err());
    }

    #[tokio::test]
    async fn test_lifecycle_seek_during_playing() {
        let mgr = session_manager_with_mocks();
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        mgr.seek(45000).await.unwrap();

        let session = mgr.load_session(id).unwrap();
        // After seek from Playing, should return to Playing.
        assert_eq!(session.state, PlayerState::Playing);
    }

    #[tokio::test]
    async fn test_lifecycle_set_volume_updates_volume() {
        let mgr = session_manager_with_mocks();
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        mgr.set_volume(42).await.unwrap();

        let session = mgr.load_session(id).unwrap();
        assert_eq!(session.volume, 42);
    }

    // ── 3. Session persistence ───────────────────────────────────────

    #[tokio::test]
    async fn test_persistence_session_stored_in_sqlite() {
        let mgr = session_manager_with_mocks();
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        // Verify the session can be loaded from the database.
        let loaded = mgr.load_session(id).unwrap();
        assert_eq!(loaded.id, id);
        assert_eq!(loaded.source_url, "https://example.com/video.mp4");
        assert_eq!(loaded.state, PlayerState::Playing);
        assert_eq!(loaded.volume, 100);
    }

    #[tokio::test]
    async fn test_persistence_current_status_returns_active_session() {
        let mgr = session_manager_with_mocks();
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        let status = mgr.current_status().await.unwrap();
        assert_eq!(status.id, id);
        assert_eq!(status.state, PlayerState::Playing);
    }

    #[tokio::test]
    async fn test_persistence_after_stop_current_status_no_active() {
        let mgr = session_manager_with_mocks();
        let _ = mgr.load("https://example.com/video.mp4").await.unwrap();

        mgr.stop().await.unwrap();

        let result = mgr.current_status().await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SessionError::NoActiveSession));
    }

    // ── 4. Crash recovery ───────────────────────────────────────────

    #[test]
    fn test_crash_recovery_recover_crashed_sessions() {
        // Use a file-based DB so a second SessionManager can see the same data.
        let tmp_dir = tempfile::tempdir().unwrap();
        let db_path = tmp_dir.path().join("crash_recovery.db");
        let db_path_str = db_path.to_str().unwrap();

        // First manager: create a session and manually set state to "playing".
        let mgr1 = SessionManager::new(db_path_str).unwrap();
        let mut session = MediaSession::new("https://example.com/video.mp4".into());
        session.state = PlayerState::Playing;
        let sid = session.id;
        mgr1.insert_session(&session).unwrap();

        // Verify it's in playing state.
        assert_eq!(mgr1.load_session(sid).unwrap().state, PlayerState::Playing);

        // Simulate a crash: drop the first manager without stopping the session.
        drop(mgr1);

        // Second manager: should recover crashed sessions to idle on startup.
        let mgr2 = SessionManager::new(db_path_str).unwrap();
        let recovered = mgr2.load_session(sid).unwrap();
        assert_eq!(recovered.state, PlayerState::Idle);
    }

    #[test]
    fn test_crash_recovery_multiple_crashed_states() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let db_path = tmp_dir.path().join("crash_multi.db");
        let db_path_str = db_path.to_str().unwrap();

        let mgr1 = SessionManager::new(db_path_str).unwrap();

        // Insert sessions in various active states.
        let mut resolving = MediaSession::new("https://example.com/r.mp4".into());
        resolving.state = PlayerState::Resolving;
        let rid = resolving.id;
        mgr1.insert_session(&resolving).unwrap();

        let mut buffering = MediaSession::new("https://example.com/b.mp4".into());
        buffering.state = PlayerState::Buffering;
        let bid = buffering.id;
        mgr1.insert_session(&buffering).unwrap();

        let mut seeking = MediaSession::new("https://example.com/s.mp4".into());
        seeking.state = PlayerState::Seeking;
        let sid = seeking.id;
        mgr1.insert_session(&seeking).unwrap();

        // Error and idle sessions should NOT be touched.
        let mut error_sess = MediaSession::new("https://example.com/e.mp4".into());
        error_sess.state = PlayerState::Error;
        let eid = error_sess.id;
        mgr1.insert_session(&error_sess).unwrap();

        drop(mgr1);

        let mgr2 = SessionManager::new(db_path_str).unwrap();

        // All active states should be recovered to Idle.
        assert_eq!(mgr2.load_session(rid).unwrap().state, PlayerState::Idle);
        assert_eq!(mgr2.load_session(bid).unwrap().state, PlayerState::Idle);
        assert_eq!(mgr2.load_session(sid).unwrap().state, PlayerState::Idle);
        // Error session should remain Error.
        assert_eq!(mgr2.load_session(eid).unwrap().state, PlayerState::Error);
    }

    // ── 5. Stale session cleanup ─────────────────────────────────────

    #[test]
    fn test_stale_session_cleanup_on_new_manager() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let db_path = tmp_dir.path().join("stale.db");
        let db_path_str = db_path.to_str().unwrap();

        // First manager: insert a stale session (25h old) and a recent one.
        let mgr1 = SessionManager::new(db_path_str).unwrap();

        let mut stale = MediaSession::new("https://example.com/old.mp4".into());
        stale.updated_at = Utc::now() - chrono::Duration::hours(25);
        let stale_id = stale.id;
        mgr1.insert_session(&stale).unwrap();

        let recent = MediaSession::new("https://example.com/recent.mp4".into());
        let recent_id = recent.id;
        mgr1.insert_session(&recent).unwrap();

        drop(mgr1);

        // Second manager: stale session should be cleaned up on startup.
        let mgr2 = SessionManager::new(db_path_str).unwrap();
        assert!(mgr2.load_session(stale_id).is_err(), "stale session should be deleted");
        assert!(mgr2.load_session(recent_id).is_ok(), "recent session should still exist");
    }

    // ── 6. Watch channel (subscribe_state) ───────────────────────────

    #[tokio::test]
    async fn test_watch_channel_load_receives_update() {
        let mgr = Arc::new(session_manager_with_mocks());
        let mut rx = mgr.subscribe_state();

        // Initially None (no session active).
        assert!(rx.borrow().is_none());

        // Load a URL — watch channel should update to Some(session).
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        // Wait for watch channel propagation.
        rx.changed().await.unwrap();
        let state = rx.borrow().clone();
        assert!(state.is_some());
        let session = state.unwrap();
        assert_eq!(session.id, id);
        assert_eq!(session.state, PlayerState::Playing);
    }

    #[tokio::test]
    async fn test_watch_channel_stop_receives_none() {
        let mgr = Arc::new(session_manager_with_mocks());
        let mut rx = mgr.subscribe_state();

        let _ = mgr.load("https://example.com/video.mp4").await.unwrap();
        // Wait for load to propagate.
        rx.changed().await.unwrap();
        assert!(rx.borrow().is_some());

        mgr.stop().await.unwrap();
        // Wait for stop to propagate.
        rx.changed().await.unwrap();
        assert!(rx.borrow().is_none());
    }

    // ── 7. Broadcast channel (subscribe) ─────────────────────────────

    #[tokio::test]
    async fn test_broadcast_receives_created_event_on_load() {
        let mgr = session_manager_with_mocks();
        let mut rx = mgr.subscribe();

        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        // Drain events and find Created.
        let mut found_created = false;
        while let Ok(event) = rx.try_recv() {
            if let SessionEvent::Created { id: eid, url } = event {
                assert_eq!(eid, id);
                assert_eq!(url, "https://example.com/video.mp4");
                found_created = true;
            }
        }
        assert!(found_created, "should have received Created event");
    }

    #[tokio::test]
    async fn test_broadcast_receives_paused_event() {
        let mgr = session_manager_with_mocks();
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        let mut rx = mgr.subscribe();
        // Drain pending events from load.
        while rx.try_recv().is_ok() {}

        mgr.pause().await.unwrap();

        let event = rx.try_recv().unwrap();
        match event {
            SessionEvent::Paused { id: eid } => assert_eq!(eid, id),
            other => panic!("Expected Paused event, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_broadcast_receives_stopped_event() {
        let mgr = session_manager_with_mocks();
        let id = mgr.load("https://example.com/video.mp4").await.unwrap();

        let mut rx = mgr.subscribe();
        // Drain pending events from load.
        while rx.try_recv().is_ok() {}

        mgr.stop().await.unwrap();

        let event = rx.try_recv().unwrap();
        match event {
            SessionEvent::Stopped { id: eid } => assert_eq!(eid, id),
            other => panic!("Expected Stopped event, got {:?}", other),
        }
    }

    // ── Display acquire/release tests ───────────────────────────────

    #[tokio::test]
    async fn test_load_acquires_display() {
        let resolver = Arc::new(MockResolver::new());
        let playback = Arc::new(MockPlayback::new());
        let display = Arc::new(MockDisplay::new());
        let tor = Arc::new(MockTor::new());

        // Check display is not acquired before load.
        assert!(!display.acquired.load(AtomicOrdering::Relaxed));

        let mgr =
            SessionManager::with_subsystems(":memory:", resolver, playback, display.clone(), tor)
                .unwrap();

        let _ = mgr.load("https://example.com/video.mp4").await.unwrap();

        // Display should be acquired after load.
        assert!(display.acquired.load(AtomicOrdering::Relaxed));
    }

    #[tokio::test]
    async fn test_stop_releases_display() {
        let resolver = Arc::new(MockResolver::new());
        let playback = Arc::new(MockPlayback::new());
        let display = Arc::new(MockDisplay::new());
        let tor = Arc::new(MockTor::new());

        let mgr =
            SessionManager::with_subsystems(":memory:", resolver, playback, display.clone(), tor)
                .unwrap();

        let _ = mgr.load("https://example.com/video.mp4").await.unwrap();
        assert!(display.acquired.load(AtomicOrdering::Relaxed));

        mgr.stop().await.unwrap();

        // Display should be released after stop.
        assert!(!display.acquired.load(AtomicOrdering::Relaxed));
    }

    // ── Buffering event test ─────────────────────────────────────────

    #[tokio::test]
    async fn test_load_emits_buffering_event() {
        let mgr = session_manager_with_mocks();
        let mut rx = mgr.subscribe();

        let _ = mgr.load("https://example.com/video.mp4").await.unwrap();

        // Drain events and find Buffering.
        let mut found_buffering = false;
        while let Ok(event) = rx.try_recv() {
            if let SessionEvent::Buffering { id: _, percent } = event {
                found_buffering = true;
                assert_eq!(percent, 0, "initial buffering should be at 0%");
            }
        }
        assert!(found_buffering, "should have received Buffering event during load");
    }

    // ── Resolved event includes title ────────────────────────────────

    #[tokio::test]
    async fn test_resolved_event_includes_title() {
        let mgr = session_manager_with_mocks();
        let mut rx = mgr.subscribe();

        let _ = mgr.load("https://example.com/video.mp4").await.unwrap();

        // Drain events and find Resolved.
        let mut found_resolved = false;
        while let Ok(event) = rx.try_recv() {
            if let SessionEvent::Resolved { id: _, direct_url: _, title } = event {
                found_resolved = true;
                assert_eq!(title, Some("Mock Title".to_string()));
            }
        }
        assert!(found_resolved, "should have received Resolved event with title");
    }

    // ── Position update event test ───────────────────────────────────

    #[tokio::test]
    async fn test_pause_emits_position_update() {
        let mgr = session_manager_with_mocks();
        let _ = mgr.load("https://example.com/video.mp4").await.unwrap();

        let mut rx = mgr.subscribe();
        // Drain pending events from load.
        while rx.try_recv().is_ok() {}

        mgr.pause().await.unwrap();

        // Should receive Paused and PositionUpdate events.
        let mut found_position_update = false;
        while let Ok(event) = rx.try_recv() {
            if let SessionEvent::PositionUpdate { .. } = event {
                found_position_update = true;
            }
        }
        assert!(found_position_update, "should have received PositionUpdate event after pause");
    }

    #[tokio::test]
    async fn test_seek_emits_position_update() {
        let mgr = session_manager_with_mocks();
        let _ = mgr.load("https://example.com/video.mp4").await.unwrap();

        let mut rx = mgr.subscribe();
        // Drain pending events from load.
        while rx.try_recv().is_ok() {}

        mgr.seek(30000).await.unwrap();

        // Should receive Seeking, Playing, and PositionUpdate events.
        let mut found_position_update = false;
        while let Ok(event) = rx.try_recv() {
            if let SessionEvent::PositionUpdate { .. } = event {
                found_position_update = true;
            }
        }
        assert!(found_position_update, "should have received PositionUpdate event after seek");
    }
}
