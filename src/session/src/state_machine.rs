//! State-machine core for `bogdan-session`.
//!
//! This module provides a focused, single-mutex state machine that
//! protocol handlers (HTTP, WebSocket, DLNA) can drive synchronously.
//! The full [`crate::SessionManager`] integrates the four subsystems
//! (resolver, playback, display, Tor) and persists state to SQLite —
//! it is the right choice for end-to-end orchestration. [`Session`]
//! is a thinner abstraction intended for callers that want a clean
//! FSM they can wrap in `Arc<Mutex<Session>>` and share across
//! threads without pulling in the full subsystem stack.
//!
//! ## State Diagram
//!
//! ```text
//!                                ┌─────────────┐
//!                                │   Idle      │ ◀── stop() from anywhere
//!                                └──────┬──────┘
//!                                       │ load(url)
//!                                       ▼
//!                                ┌─────────────┐
//!                                │  Resolving  │
//!                                └──────┬──────┘
//!                                       │ resolve_ok
//!                                       ▼
//!                                ┌─────────────┐
//!                       ┌────────│  Buffering  │────────┐
//!                       │        └─────────────┘        │
//!                       │ buffer_full                   │ stop
//!                       ▼                               │
//! ┌──────────┐  pause   ┌──────────┐  resume  ┌─────────┴──┐
//! │  Paused  │◀─────────│ Playing  │─────────▶│  Paused    │
//! └────┬─────┘          └────┬─────┘          └────────────┘
//!      │                     │ seek
//!      │                     ▼
//!      │                ┌──────────┐
//!      │                │ Seeking  │
//!      │                └────┬─────┘
//!      │                     │ seek_done
//!      │                     ▼
//!      │                ┌──────────┐
//!      │                │ Playing  │
//!      └────────────────▶│         │
//!                       └──────────┘
//!
//! Any state can transition to `Error { message }` and from there
//! back to `Idle` via `stop()`.
//! ```
//!
//! ## Thread Safety
//!
//! [`Session`] holds no internal locking — all mutable state lives
//! behind a single `Mutex` that the *caller* provides. The intended
//! shape is:
//!
//! ```rust,ignore
//! use std::sync::{Arc, Mutex};
//! use bogdan_session::state_machine::{Session, CastCommand};
//!
//! let session = Arc::new(Mutex::new(Session::new()));
//! // Share `session.clone()` across protocol handlers.
//! let mut s = session.lock().expect("mutex poisoned");
//! s.handle(CastCommand::Load { url: "https://example.org/v.mp4".into() })
//!     .expect("load");
//! ```
//!
//! Because the mutex is at the boundary, the FSM logic stays
//! single-threaded and easy to reason about. Callers that prefer
//! async locking can wrap `Session` in `tokio::sync::Mutex` instead.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

// ── Errors ───────────────────────────────────────────────────────────

/// Machine-readable error code emitted by the state machine.
///
/// Mirrors the convention used by the HTTP/WS layers (see
/// `bogdan_protocols::http::ErrorCode`): every variant serialises to a
/// `SCREAMING_SNAKE_CASE` string so clients can branch on it without
/// parsing prose. Keep this list in sync with the protocol-layer enum
/// so that a single set of error codes is exposed to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    /// No active session for the command (e.g. `Pause` while `Idle`).
    NoActiveSession,
    /// A session is already active and the command would conflict
    /// (e.g. `Load` while not `Idle`).
    SessionActive,
    /// The requested session id was not found.
    NotFound,
    /// The supplied URL is missing, malformed, or uses an
    /// unsupported scheme.
    InvalidUrl,
    /// The command is syntactically valid but not allowed in the
    /// current [`SessionState`] (e.g. `Resume` from `Idle`).
    InvalidState,
    /// The transition between two states is not permitted by the FSM.
    InvalidTransition,
    /// A subsystem (resolver / playback / display / Tor) returned an
    /// error. The human-readable message in [`StateMachineError`]
    /// carries the subsystem's diagnostic.
    Subsystem,
    /// URL resolution failed.
    ResolutionFailed,
    /// The playback engine rejected the command.
    Playback,
    /// The display subsystem rejected the command.
    Display,
    /// Catch-all for unexpected internal failures.
    Internal,
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Serialise through serde so the wire format matches the
        // #[serde(rename_all = "SCREAMING_SNAKE_CASE")] attribute. Falls
        // back to the debug representation only if serialisation fails,
        // which is unreachable for a unit-only enum.
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{:?}", self).to_uppercase());
        write!(f, "{}", s)
    }
}

/// Errors returned by the state machine.
///
/// Every variant carries an [`ErrorCode`] so that protocol handlers
/// can map the failure to an HTTP status or WebSocket error frame
/// without re-parsing the human-readable message.
#[derive(Error, Debug)]
pub enum StateMachineError {
    /// An invalid state transition was attempted.
    #[error("invalid state transition: cannot go from {from} to {to}")]
    InvalidTransition {
        from: SessionStateKind,
        to: SessionStateKind,
    },
    /// The command is not valid in the current state.
    #[error("invalid command {command} in state {state}")]
    InvalidCommand {
        state: SessionStateKind,
        command: &'static str,
    },
    /// No active session was found.
    #[error("no active session")]
    NoActiveSession,
    /// A session is already active and cannot be replaced.
    #[error("session already active — stop the current session first")]
    SessionActive,
    /// The supplied URL was rejected.
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    /// A subsystem returned an error.
    #[error("subsystem error ({code}): {message}")]
    Subsystem { code: ErrorCode, message: String },
}

impl StateMachineError {
    /// Return the canonical [`ErrorCode`] for this error.
    ///
    /// Protocol layers should prefer this over string-matching the
    /// `Display` output — the mapping is total and stable across
    /// releases.
    pub fn code(&self) -> ErrorCode {
        match self {
            StateMachineError::InvalidTransition { .. } => ErrorCode::InvalidTransition,
            StateMachineError::InvalidCommand { .. } => ErrorCode::InvalidState,
            StateMachineError::NoActiveSession => ErrorCode::NoActiveSession,
            StateMachineError::SessionActive => ErrorCode::SessionActive,
            StateMachineError::InvalidUrl(_) => ErrorCode::InvalidUrl,
            StateMachineError::Subsystem { code, .. } => *code,
        }
    }
}

// ── State ────────────────────────────────────────────────────────────

/// Discriminator for [`SessionState`]. Used by error variants that
/// need to report a state without carrying the full payload.
///
/// This is intentionally separate from the protocol-layer
/// [`crate::PlayerState`] so that the FSM can evolve independently
/// (e.g. carry an inline session id and error message) without
/// breaking the wire format exposed by `SessionManager`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStateKind {
    /// No media loaded; ready for a new `Load` command.
    Idle,
    /// URL is being resolved.
    Resolving,
    /// Media has been resolved and is buffering before playback.
    Buffering,
    /// Actively decoding and rendering media.
    Playing,
    /// Playback is paused; can be resumed.
    Paused,
    /// A seek operation is in progress.
    Seeking,
    /// An unrecoverable error occurred.
    Error,
}

impl std::fmt::Display for SessionStateKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionStateKind::Idle => write!(f, "idle"),
            SessionStateKind::Resolving => write!(f, "resolving"),
            SessionStateKind::Buffering => write!(f, "buffering"),
            SessionStateKind::Playing => write!(f, "playing"),
            SessionStateKind::Paused => write!(f, "paused"),
            SessionStateKind::Seeking => write!(f, "seeking"),
            SessionStateKind::Error => write!(f, "error"),
        }
    }
}

/// Full state of the FSM, including the active session id and any
/// error payload. The `Idle` variant carries no session id; every
/// other variant (except `Error` with `id: None`) does.
///
/// Variants match the spec in `docs/IMPLEMENTATION-PLAN.md` (T05):
/// `Idle`, `Resolving { id }`, `Buffering { id }`, `Playing { id }`,
/// `Paused { id }`, `Error { id, msg }`. We additionally include
/// `Seeking { id }` so that the seek transition can be modelled
/// without resorting to ad-hoc flags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionState {
    /// No media loaded; ready for a new `Load` command.
    Idle,
    /// URL is being resolved through the resolver subsystem.
    Resolving {
        /// Active session id.
        id: Uuid,
        /// The original URL the client requested.
        source_url: String,
    },
    /// Media has been resolved and is buffering.
    Buffering {
        /// Active session id.
        id: Uuid,
        /// The original URL the client requested.
        source_url: String,
        /// The resolved direct media URL.
        resolved_url: String,
    },
    /// Actively decoding and rendering media.
    Playing {
        /// Active session id.
        id: Uuid,
        /// The original URL the client requested.
        source_url: String,
        /// The resolved direct media URL.
        resolved_url: String,
    },
    /// Playback is paused; can be resumed.
    Paused {
        /// Active session id.
        id: Uuid,
        /// The original URL the client requested.
        source_url: String,
        /// The resolved direct media URL.
        resolved_url: String,
    },
    /// A seek operation is in progress.
    Seeking {
        /// Active session id.
        id: Uuid,
        /// The original URL the client requested.
        source_url: String,
        /// The resolved direct media URL.
        resolved_url: String,
        /// Target position (ms) the seek is heading to.
        target_ms: u64,
    },
    /// An unrecoverable error occurred during playback.
    Error {
        /// Active session id, if one had been created before the error.
        id: Option<Uuid>,
        /// Human-readable diagnostic.
        message: String,
    },
}

impl SessionState {
    /// Return the discriminator for this state.
    pub fn kind(&self) -> SessionStateKind {
        match self {
            SessionState::Idle => SessionStateKind::Idle,
            SessionState::Resolving { .. } => SessionStateKind::Resolving,
            SessionState::Buffering { .. } => SessionStateKind::Buffering,
            SessionState::Playing { .. } => SessionStateKind::Playing,
            SessionState::Paused { .. } => SessionStateKind::Paused,
            SessionState::Seeking { .. } => SessionStateKind::Seeking,
            SessionState::Error { .. } => SessionStateKind::Error,
        }
    }

    /// Return the active session id, if any.
    pub fn id(&self) -> Option<Uuid> {
        match self {
            SessionState::Idle => None,
            SessionState::Error { id, .. } => *id,
            SessionState::Resolving { id, .. }
            | SessionState::Buffering { id, .. }
            | SessionState::Playing { id, .. }
            | SessionState::Paused { id, .. }
            | SessionState::Seeking { id, .. } => Some(*id),
        }
    }

    /// Return `true` if the state represents an active (non-idle,
    /// non-error) session.
    pub fn is_active(&self) -> bool {
        !matches!(self, SessionState::Idle | SessionState::Error { .. })
    }

    /// Return `true` if the state is terminal-ish — i.e. the caller
    /// must explicitly `stop()` to return to `Idle`.
    pub fn requires_stop(&self) -> bool {
        matches!(self, SessionState::Error { .. })
    }

    /// Return the validity of transitioning from `self` to `target`.
    ///
    /// The transition table mirrors the diagram at the top of this
    /// module:
    ///
    /// ```text
    /// Idle      → Resolving, Error
    /// Resolving → Buffering, Error, Idle
    /// Buffering → Playing, Paused, Error, Idle
    /// Playing   → Paused, Seeking, Buffering, Error, Idle
    /// Paused    → Playing, Seeking, Error, Idle
    /// Seeking   → Playing, Error, Idle
    /// Error     → Idle
    /// ```
    pub fn can_transition_to(&self, target: SessionStateKind) -> bool {
        let from = self.kind();
        match from {
            SessionStateKind::Idle => matches!(target, SessionStateKind::Resolving | SessionStateKind::Error),
            SessionStateKind::Resolving => matches!(
                target,
                SessionStateKind::Buffering | SessionStateKind::Error | SessionStateKind::Idle
            ),
            SessionStateKind::Buffering => matches!(
                target,
                SessionStateKind::Playing
                    | SessionStateKind::Paused
                    | SessionStateKind::Error
                    | SessionStateKind::Idle
            ),
            SessionStateKind::Playing => matches!(
                target,
                SessionStateKind::Paused
                    | SessionStateKind::Seeking
                    | SessionStateKind::Buffering
                    | SessionStateKind::Error
                    | SessionStateKind::Idle
            ),
            SessionStateKind::Paused => matches!(
                target,
                SessionStateKind::Playing
                    | SessionStateKind::Seeking
                    | SessionStateKind::Error
                    | SessionStateKind::Idle
            ),
            SessionStateKind::Seeking => {
                matches!(target, SessionStateKind::Playing | SessionStateKind::Error | SessionStateKind::Idle)
            },
            SessionStateKind::Error => matches!(target, SessionStateKind::Idle),
        }
    }
}

impl Default for SessionState {
    fn default() -> Self {
        SessionState::Idle
    }
}

impl std::fmt::Display for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionState::Idle => write!(f, "idle"),
            SessionState::Resolving { id, .. } => write!(f, "resolving({})", id),
            SessionState::Buffering { id, .. } => write!(f, "buffering({})", id),
            SessionState::Playing { id, .. } => write!(f, "playing({})", id),
            SessionState::Paused { id, .. } => write!(f, "paused({})", id),
            SessionState::Seeking { id, target_ms, .. } => {
                write!(f, "seeking({}, {}ms)", id, target_ms)
            },
            SessionState::Error { id, message } => match id {
                Some(id) => write!(f, "error({}, {:?})", id, message),
                None => write!(f, "error({:?})", message),
            },
        }
    }
}

// ── Commands ─────────────────────────────────────────────────────────

/// A command that a protocol handler dispatches to the [`Session`] FSM.
///
/// Each variant maps to exactly one transition (or rejection) under
/// the rules encoded in [`SessionState::can_transition_to`]. Commands
/// that need additional context (e.g. resolution metadata returned by
/// the resolver subsystem) are issued through dedicated methods on
/// [`Session`] rather than through this enum — `CastCommand` is the
/// *external* surface, the methods are the *internal* one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CastCommand {
    /// Load a fresh URL into the session. Valid only from `Idle`.
    Load {
        /// The original URL the client requested to cast.
        url: String,
    },
    /// Pause playback. Valid from `Playing`, `Buffering`.
    Pause,
    /// Resume a paused or buffering session. Valid from `Paused`, `Buffering`.
    Resume,
    /// Stop the current session and return to `Idle`. Valid from any
    /// non-idle state (this is the only command that can transition
    /// out of `Error`).
    Stop,
    /// Seek to `position_ms` milliseconds from the start. Valid from
    /// `Playing` and `Paused`; enters the transient `Seeking` state
    /// until the caller confirms via [`Session::seek_done`].
    Seek {
        /// Target absolute position in milliseconds.
        position_ms: u64,
    },
    /// Adjust the playback volume. Does not change the FSM state; the
        /// new value is recorded on the [`Session`] for status queries.
    SetVolume {
        /// Volume in `[0, 100]`. Values outside this range are clamped.
        volume: u8,
    },
}

impl CastCommand {
    /// Return the canonical name of the command (used in error
    /// messages and structured logs).
    pub fn name(&self) -> &'static str {
        match self {
            CastCommand::Load { .. } => "load",
            CastCommand::Pause => "pause",
            CastCommand::Resume => "resume",
            CastCommand::Stop => "stop",
            CastCommand::Seek { .. } => "seek",
            CastCommand::SetVolume { .. } => "set_volume",
        }
    }
}

// ── Session ──────────────────────────────────────────────────────────

/// In-memory media-session state machine.
///
/// [`Session`] owns:
/// - the current [`SessionState`],
/// - the active session id (mirrored from the state for convenience),
/// - playback metadata (position, duration, volume),
/// - timestamps (`created_at`, `updated_at`).
///
/// It does **not** own subsystem handles or persistence — those stay
/// with [`crate::SessionManager`]. [`Session`] is the right
/// abstraction for tests, mock harnesses, and protocol layers that
/// want to validate command sequencing without spinning up the full
/// GStreamer/Tor stack.
///
/// ## Thread Safety
///
/// [`Session`] is `Send` but holds no internal locking. Wrap it in
/// `Arc<Mutex<Session>>` (or `Arc<tokio::sync::Mutex<Session>>` for
/// async callers) at the boundary. All state mutations go through a
/// single mutex, which keeps the FSM logic single-threaded and
/// deterministic.
///
/// ```rust,ignore
/// use std::sync::{Arc, Mutex};
/// use bogdan_session::state_machine::{Session, CastCommand};
///
/// let session = Arc::new(Mutex::new(Session::new()));
/// {
///     let mut s = session.lock().expect("mutex poisoned");
///     s.handle(CastCommand::Load { url: "https://example.org/v.mp4".into() })
///         .expect("load");
/// }
/// ```
#[derive(Debug, Clone)]
pub struct Session {
    /// Current FSM state.
    state: SessionState,
    /// The active session id, if any. Mirrored from `state` for O(1)
    /// access via [`Session::id`].
    id: Option<Uuid>,
    /// Current playback position in milliseconds from the start.
    position_ms: u64,
    /// Total duration in milliseconds, if known.
    duration_ms: Option<u64>,
    /// Volume level in `[0, 100]`. Default is `100`.
    volume: u8,
    /// When the active session was first created.
    created_at: Option<DateTime<Utc>>,
    /// When the session state was last updated.
    updated_at: DateTime<Utc>,
}

impl Session {
    /// Create a fresh session in the `Idle` state with default
    /// volume (`100`) and no playback metadata.
    pub fn new() -> Self {
        Self {
            state: SessionState::Idle,
            id: None,
            position_ms: 0,
            duration_ms: None,
            volume: 100,
            created_at: None,
            updated_at: Utc::now(),
        }
    }

    /// Return the current state.
    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Return the active session id, if any.
    pub fn id(&self) -> Option<Uuid> {
        self.id
    }

    /// Return the current playback position in milliseconds.
    pub fn position_ms(&self) -> u64 {
        self.position_ms
    }

    /// Return the media duration in milliseconds, if known.
    pub fn duration_ms(&self) -> Option<u64> {
        self.duration_ms
    }

    /// Return the current volume in `[0, 100]`.
    pub fn volume(&self) -> u8 {
        self.volume
    }

    /// Return when the active session was created, if any.
    pub fn created_at(&self) -> Option<DateTime<Utc>> {
        self.created_at
    }

    /// Return when the FSM was last updated.
    pub fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }

    /// Reset the FSM to `Idle`, discarding all session metadata.
    ///
    /// This is idempotent: calling `reset()` on an already-idle
    /// session is a no-op.
    pub fn reset(&mut self) {
        self.state = SessionState::Idle;
        self.id = None;
        self.position_ms = 0;
        self.duration_ms = None;
        // Keep volume across resets — clients typically want their
        // volume preference to persist between casts.
        self.created_at = None;
        self.updated_at = Utc::now();
    }

    /// Apply a [`CastCommand`], transitioning the FSM and updating
    /// metadata. Returns the new [`SessionState`] on success or a
    /// [`StateMachineError`] describing why the command was rejected.
    ///
    /// Volume commands are special-cased: they do not transition the
    /// FSM (any state except `Idle` accepts them) and the new volume
    /// is recorded on the session for status queries.
    pub fn handle(&mut self, cmd: CastCommand) -> Result<SessionState, StateMachineError> {
        match cmd {
            CastCommand::Load { url } => self.load(url),
            CastCommand::Pause => self.pause(),
            CastCommand::Resume => self.resume(),
            CastCommand::Stop => self.stop(),
            CastCommand::Seek { position_ms } => self.seek(position_ms),
            CastCommand::SetVolume { volume } => {
                self.set_volume(volume)?;
                // Volume changes do not transition the FSM; return the
                // current state so callers can chain.
                Ok(self.state.clone())
            },
        }
    }

    // ── Internal transition helpers ─────────────────────────────────

    fn load(&mut self, url: String) -> Result<SessionState, StateMachineError> {
        if !matches!(self.state, SessionState::Idle) {
            return Err(StateMachineError::SessionActive);
        }
        if !is_valid_cast_url(&url) {
            return Err(StateMachineError::InvalidUrl(url));
        }

        let id = Uuid::new_v4();
        let now = Utc::now();
        self.state = SessionState::Resolving { id, source_url: url };
        self.id = Some(id);
        self.created_at = Some(now);
        self.updated_at = now;
        self.position_ms = 0;
        self.duration_ms = None;
        Ok(self.state.clone())
    }

    fn pause(&mut self) -> Result<SessionState, StateMachineError> {
        match self.state.clone() {
            SessionState::Playing { id, source_url, resolved_url }
            | SessionState::Buffering { id, source_url, resolved_url } => {
                self.transition_to(SessionState::Paused {
                    id,
                    source_url,
                    resolved_url,
                })
            },
            _ => Err(StateMachineError::InvalidCommand {
                state: self.state.kind(),
                command: "pause",
            }),
        }
    }

    fn resume(&mut self) -> Result<SessionState, StateMachineError> {
        match self.state.clone() {
            SessionState::Paused { id, source_url, resolved_url }
            | SessionState::Buffering { id, source_url, resolved_url } => self
                .transition_to(SessionState::Playing {
                    id,
                    source_url,
                    resolved_url,
                }),
            _ => Err(StateMachineError::InvalidCommand {
                state: self.state.kind(),
                command: "resume",
            }),
        }
    }

    fn stop(&mut self) -> Result<SessionState, StateMachineError> {
        // `Stop` is valid from any non-idle state, including `Error`.
        if matches!(self.state, SessionState::Idle) {
            // Idempotent: stop on an idle session is a no-op success.
            return Ok(self.state.clone());
        }
        self.transition_to_kind(SessionStateKind::Idle);
        self.reset();
        Ok(self.state.clone())
    }

    fn seek(&mut self, position_ms: u64) -> Result<SessionState, StateMachineError> {
        match self.state.clone() {
            SessionState::Playing { id, source_url, resolved_url }
            | SessionState::Paused { id, source_url, resolved_url } => self.transition_to(
                SessionState::Seeking {
                    id,
                    source_url,
                    resolved_url,
                    target_ms: position_ms,
                },
            ),
            _ => Err(StateMachineError::InvalidCommand {
                state: self.state.kind(),
                command: "seek",
            }),
        }
    }

    fn set_volume(&mut self, volume: u8) -> Result<(), StateMachineError> {
        if matches!(self.state, SessionState::Idle) {
            return Err(StateMachineError::InvalidCommand {
                state: SessionStateKind::Idle,
                command: "set_volume",
            });
        }
        // `u8` already guarantees `[0, 255]`; we further clamp to
        // `[0, 100]` so callers cannot store out-of-range values.
        self.volume = volume.min(100);
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Transition into a fully-specified `SessionState` variant,
    /// validating that the kind-level transition is allowed.
    fn transition_to(
        &mut self,
        target: SessionState,
    ) -> Result<SessionState, StateMachineError> {
        let target_kind = target.kind();
        if !self.state.can_transition_to(target_kind) {
            return Err(StateMachineError::InvalidTransition {
                from: self.state.kind(),
                to: target_kind,
            });
        }
        self.state = target;
        self.updated_at = Utc::now();
        Ok(self.state.clone())
    }

    /// Transition into a kind, deriving the payload from the current
    /// state where appropriate. Used by `stop()` (→ `Idle`) where
    /// there is no payload to carry.
    fn transition_to_kind(&mut self, target: SessionStateKind) {
        // Only valid for transitions whose target carries no payload
        // (currently just `Idle`). Other targets must go through
        // `transition_to`.
        debug_assert_eq!(target, SessionStateKind::Idle);
        if self.state.can_transition_to(target) {
            self.state = SessionState::Idle;
            self.updated_at = Utc::now();
        }
    }

    // ── Subsystem-driven transitions (used by `SessionManager`) ──────

    /// Mark URL resolution as complete and enter `Buffering`.
    ///
    /// Called by the resolver subsystem after it has produced a
    /// direct media URL. The FSM transitions `Resolving → Buffering`
    /// and stashes the resolved URL on the state for downstream
    /// commands.
    pub fn resolve_ok(&mut self, resolved_url: String) -> Result<SessionState, StateMachineError> {
        match self.state.clone() {
            SessionState::Resolving { id, source_url } => self.transition_to(SessionState::Buffering {
                id,
                source_url,
                resolved_url,
            }),
            _ => Err(StateMachineError::InvalidCommand {
                state: self.state.kind(),
                command: "resolve_ok",
            }),
        }
    }

    /// Mark URL resolution as failed and enter `Error`.
    pub fn resolve_err(&mut self, message: String) -> Result<SessionState, StateMachineError> {
        let id = self.id;
        self.transition_to(SessionState::Error { id, message })
    }

    /// Mark buffering as complete and enter `Playing`.
    pub fn buffer_full(&mut self) -> Result<SessionState, StateMachineError> {
        match self.state.clone() {
            SessionState::Buffering { id, source_url, resolved_url } => self.transition_to(
                SessionState::Playing {
                    id,
                    source_url,
                    resolved_url,
                },
            ),
            _ => Err(StateMachineError::InvalidCommand {
                state: self.state.kind(),
                command: "buffer_full",
            }),
        }
    }

    /// Mark a seek as complete and return to `Playing`.
    pub fn seek_done(&mut self) -> Result<SessionState, StateMachineError> {
        match self.state.clone() {
            SessionState::Seeking {
                id,
                source_url,
                resolved_url,
                target_ms,
            } => {
                self.position_ms = target_ms;
                self.transition_to(SessionState::Playing {
                    id,
                    source_url,
                    resolved_url,
                })
            },
            _ => Err(StateMachineError::InvalidCommand {
                state: self.state.kind(),
                command: "seek_done",
            }),
        }
    }

    /// Report a playback position update (e.g. from the GStreamer
    /// bus). Does not transition the FSM; only updates `position_ms`.
    pub fn position_update(&mut self, position_ms: u64) {
        self.position_ms = position_ms;
        self.updated_at = Utc::now();
    }

    /// Report the media duration (e.g. from the GStreamer bus).
    pub fn set_duration(&mut self, duration_ms: u64) {
        self.duration_ms = Some(duration_ms);
        self.updated_at = Utc::now();
    }

    /// Mark an unrecoverable error from a subsystem.
    pub fn fail(&mut self, message: String) -> Result<SessionState, StateMachineError> {
        let id = self.id;
        self.transition_to(SessionState::Error { id, message })
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

// ── URL validation ───────────────────────────────────────────────────

/// Minimal URL validation for the `Load` command.
///
/// Rejects empty strings, missing schemes, and unsupported schemes.
/// Mirrors the protocol-layer check in
/// `bogdan_protocols::http::is_safe_cast_url` but is intentionally
/// more permissive (it does not block private IPs) — the
/// [`SessionManager`](crate::SessionManager) layer is responsible for
/// SSRF protection. This function only enforces *grammatical*
/// validity.
fn is_valid_cast_url(url: &str) -> bool {
    if url.trim().is_empty() {
        return false;
    }
    // Require a scheme separator. We accept any scheme here because
    // the resolver will reject unsupported schemes (e.g. `file://`)
    // upstream; the FSM only needs to know the string is structurally
    // a URL.
    if !url.contains("://") {
        return false;
    }
    let Some(scheme_end) = url.find("://") else {
        return false;
    };
    let scheme = &url[..scheme_end];
    if scheme.is_empty() {
        return false;
    }
    // Scheme must be ASCII-alpha followed by ASCII-alphanumeric/+/-/.,
    // per RFC 3986 §3.1.
    let mut chars = scheme.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return false;
    }
    // Host must be non-empty.
    let host_part = &url[scheme_end + 3..];
    if host_part.is_empty() {
        return false;
    }
    true
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_URL: &str = "https://example.org/video.mp4";

    // ── ErrorCode ───────────────────────────────────────────────────

    #[test]
    fn error_code_display_is_screaming_snake_case() {
        assert_eq!(ErrorCode::NoActiveSession.to_string(), "NO_ACTIVE_SESSION");
        assert_eq!(ErrorCode::SessionActive.to_string(), "SESSION_ACTIVE");
        assert_eq!(ErrorCode::InvalidUrl.to_string(), "INVALID_URL");
        assert_eq!(ErrorCode::InvalidState.to_string(), "INVALID_STATE");
        assert_eq!(ErrorCode::InvalidTransition.to_string(), "INVALID_TRANSITION");
        assert_eq!(ErrorCode::Subsystem.to_string(), "SUBSYSTEM");
        assert_eq!(ErrorCode::ResolutionFailed.to_string(), "RESOLUTION_FAILED");
        assert_eq!(ErrorCode::Playback.to_string(), "PLAYBACK");
        assert_eq!(ErrorCode::Display.to_string(), "DISPLAY");
        assert_eq!(ErrorCode::Internal.to_string(), "INTERNAL");
        assert_eq!(ErrorCode::NotFound.to_string(), "NOT_FOUND");
    }

    #[test]
    fn error_code_serialises_as_string() {
        let v = serde_json::to_value(ErrorCode::InvalidUrl).expect("serialise");
        assert_eq!(v, serde_json::json!("INVALID_URL"));
    }

    // ── SessionState ────────────────────────────────────────────────

    #[test]
    fn state_kind_matches_variant() {
        assert_eq!(SessionState::Idle.kind(), SessionStateKind::Idle);
        assert_eq!(
            SessionState::Resolving {
                id: Uuid::nil(),
                source_url: "u".into()
            }
            .kind(),
            SessionStateKind::Resolving
        );
        assert_eq!(
            SessionState::Error {
                id: None,
                message: "boom".into()
            }
            .kind(),
            SessionStateKind::Error
        );
    }

    #[test]
    fn state_id_returns_active_session_id() {
        let id = Uuid::new_v4();
        let s = SessionState::Playing {
            id,
            source_url: "u".into(),
            resolved_url: "r".into(),
        };
        assert_eq!(s.id(), Some(id));
        assert_eq!(SessionState::Idle.id(), None);
        assert_eq!(
            SessionState::Error {
                id: None,
                message: "x".into()
            }
            .id(),
            None
        );
    }

    #[test]
    fn state_is_active_flag() {
        assert!(!SessionState::Idle.is_active());
        assert!(
            SessionState::Buffering {
                id: Uuid::nil(),
                source_url: "u".into(),
                resolved_url: "r".into()
            }
            .is_active()
        );
        assert!(!SessionState::Error {
            id: None,
            message: "x".into()
        }
        .is_active());
    }

    #[test]
    fn state_can_transition_to_table() {
        // Idle → Resolving, Error
        assert!(SessionState::Idle.can_transition_to(SessionStateKind::Resolving));
        assert!(SessionState::Idle.can_transition_to(SessionStateKind::Error));
        assert!(!SessionState::Idle.can_transition_to(SessionStateKind::Playing));

        // Resolving → Buffering, Error, Idle
        let resolving = SessionState::Resolving {
            id: Uuid::nil(),
            source_url: "u".into(),
        };
        assert!(resolving.can_transition_to(SessionStateKind::Buffering));
        assert!(resolving.can_transition_to(SessionStateKind::Error));
        assert!(resolving.can_transition_to(SessionStateKind::Idle));
        assert!(!resolving.can_transition_to(SessionStateKind::Playing));

        // Error → Idle (and only Idle)
        let error = SessionState::Error {
            id: None,
            message: "x".into(),
        };
        assert!(error.can_transition_to(SessionStateKind::Idle));
        assert!(!error.can_transition_to(SessionStateKind::Playing));
        assert!(!error.can_transition_to(SessionStateKind::Resolving));
    }

    // ── CastCommand ─────────────────────────────────────────────────

    #[test]
    fn cast_command_name_is_stable() {
        assert_eq!(CastCommand::Load { url: String::new() }.name(), "load");
        assert_eq!(CastCommand::Pause.name(), "pause");
        assert_eq!(CastCommand::Resume.name(), "resume");
        assert_eq!(CastCommand::Stop.name(), "stop");
        assert_eq!(CastCommand::Seek { position_ms: 0 }.name(), "seek");
        assert_eq!(CastCommand::SetVolume { volume: 50 }.name(), "set_volume");
    }

    // ── Session: load → resolve → buffer → play lifecycle ───────────

    #[test]
    fn fresh_session_is_idle() {
        let s = Session::new();
        assert_eq!(s.state(), &SessionState::Idle);
        assert_eq!(s.id(), None);
        assert_eq!(s.position_ms(), 0);
        assert_eq!(s.duration_ms(), None);
        assert_eq!(s.volume(), 100);
        assert!(s.created_at().is_none());
    }

    #[test]
    fn load_transitions_to_resolving() {
        let mut s = Session::new();
        let state = s
            .handle(CastCommand::Load {
                url: SAMPLE_URL.into(),
            })
            .expect("load");
        assert_eq!(state.kind(), SessionStateKind::Resolving);
        assert!(s.id().is_some());
        assert!(s.created_at().is_some());
        if let SessionState::Resolving { source_url, .. } = &state {
            assert_eq!(source_url, SAMPLE_URL);
        } else {
            panic!("expected Resolving, got {:?}", state);
        }
    }

    #[test]
    fn load_while_active_returns_session_active() {
        let mut s = Session::new();
        s.handle(CastCommand::Load {
            url: SAMPLE_URL.into(),
        })
        .expect("first load");
        let err = s
            .handle(CastCommand::Load {
                url: SAMPLE_URL.into(),
            })
            .expect_err("second load should fail");
        assert_eq!(err.code(), ErrorCode::SessionActive);
    }

    #[test]
    fn load_rejects_invalid_url() {
        let mut s = Session::new();
        let err = s
            .handle(CastCommand::Load { url: "".into() })
            .expect_err("empty url");
        assert_eq!(err.code(), ErrorCode::InvalidUrl);

        let err = s
            .handle(CastCommand::Load {
                url: "not a url".into(),
            })
            .expect_err("no scheme");
        assert_eq!(err.code(), ErrorCode::InvalidUrl);

        let err = s
            .handle(CastCommand::Load {
                url: "://missing-scheme".into(),
            })
            .expect_err("missing scheme");
        assert_eq!(err.code(), ErrorCode::InvalidUrl);
    }

    #[test]
    fn full_lifecycle_load_resolve_buffer_play_pause_resume_stop() {
        let mut s = Session::new();

        // Idle → Resolving
        s.handle(CastCommand::Load {
            url: SAMPLE_URL.into(),
        })
        .expect("load");
        let id = s.id().expect("session id after load");

        // Resolving → Buffering (subsystem confirms resolution)
        let resolved_url = "https://cdn.example.org/direct.mp4";
        s.resolve_ok(resolved_url.into()).expect("resolve_ok");

        // Buffering → Playing (subsystem reports buffer full)
        s.buffer_full().expect("buffer_full");
        assert_eq!(s.state().kind(), SessionStateKind::Playing);

        // Playing → Paused
        s.handle(CastCommand::Pause).expect("pause");
        assert_eq!(s.state().kind(), SessionStateKind::Paused);

        // Paused → Playing
        s.handle(CastCommand::Resume).expect("resume");
        assert_eq!(s.state().kind(), SessionStateKind::Playing);

        // Playing → Idle (stop)
        s.handle(CastCommand::Stop).expect("stop");
        assert_eq!(s.state(), &SessionState::Idle);
        assert_eq!(s.id(), None);
        // Volume persists across resets.
        assert_eq!(s.volume(), 100);
        // id is forgotten.
        let _ = id;
    }

    // ── Session: seek lifecycle ─────────────────────────────────────

    #[test]
    fn seek_lifecycle() {
        let mut s = Session::new();
        s.handle(CastCommand::Load {
            url: SAMPLE_URL.into(),
        })
        .expect("load");
        s.resolve_ok("r".into()).expect("resolve_ok");
        s.buffer_full().expect("buffer_full");

        // Playing → Seeking
        s.handle(CastCommand::Seek {
            position_ms: 5_000,
        })
        .expect("seek");
        assert_eq!(s.state().kind(), SessionStateKind::Seeking);
        if let SessionState::Seeking { target_ms, .. } = s.state() {
            assert_eq!(*target_ms, 5_000);
        } else {
            panic!("expected Seeking");
        }

        // Seeking → Playing
        s.seek_done().expect("seek_done");
        assert_eq!(s.state().kind(), SessionStateKind::Playing);
        assert_eq!(s.position_ms(), 5_000);
    }

    #[test]
    fn seek_rejected_from_idle() {
        let mut s = Session::new();
        let err = s
            .handle(CastCommand::Seek { position_ms: 0 })
            .expect_err("seek from idle");
        assert_eq!(err.code(), ErrorCode::InvalidState);
    }

    // ── Session: stop is idempotent and universal ───────────────────

    #[test]
    fn stop_from_idle_is_noop_success() {
        let mut s = Session::new();
        let state = s.handle(CastCommand::Stop).expect("stop from idle");
        assert_eq!(state, SessionState::Idle);
    }

    #[test]
    fn stop_from_error_returns_to_idle() {
        let mut s = Session::new();
        s.handle(CastCommand::Load {
            url: SAMPLE_URL.into(),
        })
        .expect("load");
        s.fail("simulated playback error".into()).expect("fail");
        assert_eq!(s.state().kind(), SessionStateKind::Error);

        s.handle(CastCommand::Stop).expect("stop from error");
        assert_eq!(s.state(), &SessionState::Idle);
    }

    // ── Session: volume ─────────────────────────────────────────────

    #[test]
    fn set_volume_records_and_clamps() {
        let mut s = Session::new();
        s.handle(CastCommand::Load {
            url: SAMPLE_URL.into(),
        })
        .expect("load");

        s.handle(CastCommand::SetVolume { volume: 42 })
            .expect("set_volume");
        assert_eq!(s.volume(), 42);

        // u8 already caps at 255; we further clamp to 100.
        s.handle(CastCommand::SetVolume { volume: 200 })
            .expect("set_volume clamps");
        assert_eq!(s.volume(), 100);

        s.handle(CastCommand::SetVolume { volume: 0 })
            .expect("set_volume zero");
        assert_eq!(s.volume(), 0);
    }

    #[test]
    fn set_volume_rejected_from_idle() {
        let mut s = Session::new();
        let err = s
            .handle(CastCommand::SetVolume { volume: 50 })
            .expect_err("set_volume from idle");
        assert_eq!(err.code(), ErrorCode::InvalidState);
    }

    #[test]
    fn volume_persists_across_stop() {
        let mut s = Session::new();
        s.handle(CastCommand::Load {
            url: SAMPLE_URL.into(),
        })
        .expect("load");
        s.handle(CastCommand::SetVolume { volume: 30 })
            .expect("set_volume");
        s.handle(CastCommand::Stop).expect("stop");
        assert_eq!(s.volume(), 30);
    }

    // ── Session: invalid commands ───────────────────────────────────

    #[test]
    fn pause_from_idle_rejected() {
        let mut s = Session::new();
        let err = s.handle(CastCommand::Pause).expect_err("pause from idle");
        assert_eq!(err.code(), ErrorCode::InvalidState);
    }

    #[test]
    fn resume_from_playing_rejected() {
        let mut s = Session::new();
        s.handle(CastCommand::Load {
            url: SAMPLE_URL.into(),
        })
        .expect("load");
        s.resolve_ok("r".into()).expect("resolve_ok");
        s.buffer_full().expect("buffer_full");

        let err = s.handle(CastCommand::Resume).expect_err("resume from playing");
        assert_eq!(err.code(), ErrorCode::InvalidState);
    }

    // ── Session: reset ──────────────────────────────────────────────

    #[test]
    fn reset_clears_session_metadata() {
        let mut s = Session::new();
        s.handle(CastCommand::Load {
            url: SAMPLE_URL.into(),
        })
        .expect("load");
        s.position_update(1234);
        s.set_duration(9999);
        assert!(s.id().is_some());
        assert_eq!(s.position_ms(), 1234);

        s.reset();
        assert_eq!(s.state(), &SessionState::Idle);
        assert_eq!(s.id(), None);
        assert_eq!(s.position_ms(), 0);
        assert_eq!(s.duration_ms(), None);
    }

    // ── URL validation ──────────────────────────────────────────────

    #[test]
    fn url_validation_accepts_common_schemes() {
        assert!(is_valid_cast_url("https://example.org/v.mp4"));
        assert!(is_valid_cast_url("http://example.org/v.mp4"));
        assert!(is_valid_cast_url("ftp://example.org/v.mp4"));
        assert!(is_valid_cast_url("yt-dlp+test://example.org/v.mp4"));
    }

    #[test]
    fn url_validation_rejects_garbage() {
        assert!(!is_valid_cast_url(""));
        assert!(!is_valid_cast_url("   "));
        assert!(!is_valid_cast_url("no-scheme"));
        assert!(!is_valid_cast_url("://no-scheme"));
        assert!(!is_valid_cast_url("1bad://scheme-starts-with-digit"));
        assert!(!is_valid_cast_url("https://"));
    }
}
