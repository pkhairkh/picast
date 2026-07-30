---
doc: code_review
project: picast
version: 1
phase: code_review
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# Code Review: `src/session/src/lib.rs`

**File:** `src/session/src/lib.rs`
**Lines:** 2928 (including ~1450 lines of tests, so ~1480 lines of production code)
**Reviewer:** agent
**Date:** 2026-07-30

## Summary

The session layer is the central coordinator of the boGDan media appliance. It owns the SQLite-backed `MediaSession` store, enforces a single-session-at-a-time invariant, drives the resolver → playback → display → Tor subsystems through trait objects, and exposes both a broadcast event stream (for WebSocket) and a watch channel (for HTTP polling). The state machine is well-defined and exhaustively tested. Crash recovery and stale-session cleanup on startup are thoughtful production touches. However, there are several consistency bugs in the command methods, a data-integrity bug in volume handling, lock-handling patterns that could deadlock under load, and meaningful gaps in the test suite around the CDN retry logic that is the file's most complex code path.

## Scope Reviewed

| Concern | Implementation | Notes |
|---------|----------------|-------|
| State machine | `PlayerState` enum + `can_transition_to` / `transition` | 7 states, explicit transition table |
| Persistence | SQLite via `rusqlite::Connection` wrapped in `std::sync::Mutex` | WAL journal mode; single-row-per-session schema |
| Concurrency | `std::sync::Mutex` for `db` and `active_session_id`; `tokio::sync::{broadcast, watch}` for events | No async locks — all locks are short-lived |
| Event distribution | `broadcast::Sender<SessionEvent>` (128-slot ring) + `watch::Sender<Option<MediaSession>>` | Dual-channel design serves streaming and polling clients |
| Subsystem wiring | `Option<Arc<dyn {Resolver,Playback,Display,Tor}Trait>>` | Traits defined in `interfaces.rs`; mockable |
| CDN 403 retry | Inline loop in `load()` with `is_cdn_retryable_error` + `re_resolve` | Max 2 retries; cache invalidation between attempts |
| Crash recovery | `recover_crashed_sessions()` runs on `new()` | Resets non-idle/non-error sessions to Idle |
| Stale cleanup | `cleanup_stale_sessions()` runs on `new()` | Deletes rows older than 24h |

## Findings

### Bugs

#### BUG-001: Volume corruption window — `load_session` clamps to 255, not 100
- **Severity:** Medium
- **Location:** Lines 547–557 (`load_session` — `volume_u8` computation, specifically the `if !(0..=255).contains(&volume)` check at line 551)
- **Description:** When loading a session from SQLite, the volume is read as `i32` and clamped into the `0..=255` range before being cast to `u8`. The code path:
  ```rust
  let volume_u8 = if !(0..=255).contains(&volume) {
      tracing::warn!(volume = volume, "corrupt volume in DB — clamping to 100");
      100u8
  } else {
      volume as u8
  };
  ```
  A stored value of, say, `200` (which is in `0..=255` but invalid as a volume — the public API only accepts 0–100 via `set_volume`'s `.min(100)` clamp) will be loaded as `200u8` without correction. The warning message even says "clamping to 100" but the code does not actually clamp — it falls through to the `else` branch and returns the raw value.
- **Impact:** A volume value that was somehow corrupted in the DB (or written by a future code path that doesn't clamp) will be loaded as-is and used as the playback volume, which can exceed 100%. The warning message is misleading — it claims to clamp but doesn't.
- **Recommendation:** Either clamp to `0..=100` (matching the public API contract) and update the warning to match the actual range, or introduce a `Volume` newtype that enforces 0–100 at the type level. Suggested fix:
  ```rust
  let volume_u8 = if volume < 0 || volume > 100 {
      tracing::warn!(volume = volume, "corrupt volume in DB — clamping to 100");
      100u8
  } else {
      volume as u8
  };
  ```

#### BUG-002: `pause()` / `resume()` leave the playback subsystem in a divergent state if the DB transition fails
- **Severity:** Medium
- **Location:** `pause()` lines 946–962 (subsystem call at line 952, `try_transition` at line 956), `resume()` lines 971–987 (subsystem call at line 977, `try_transition` at line 981)
- **Description:** Both methods follow the pattern: (1) call `playback.pause()/resume().await` first, (2) then call `try_transition()` to update the DB state. The comment says "only update DB state on success" — but it only checks the *subsystem's* success, not the DB transition's. If `try_transition` fails (e.g. because another concurrent operation already moved the state, or the session was concurrently stopped and the row deleted), the playback engine is already paused/resumed but the DB still reflects the old state. Subsequent `current_status()` calls return stale state.
- **Impact:** The DB and the playback engine disagree about the current state. A subsequent `stop()` will skip calling `playback.stop()` because the DB state doesn't match `Playing|Paused|Buffering|Seeking`, leaving the pipeline running while the session is deleted.
- **Recommendation:** Either (a) make the transition atomic by checking the current state before calling the subsystem and using a compare-and-swap pattern, or (b) on `try_transition` failure, roll back the subsystem call (e.g. call `playback.resume()` if `pause()` succeeded but the transition failed). Option (a) is cleaner:
  ```rust
  pub async fn pause(&self) -> Result<(), SessionError> {
      let id = self.active_session_id()?;
      let session = self.load_session(id)?;
      if session.state != PlayerState::Playing {
          return Err(SessionError::InvalidTransition {
              from: session.state,
              to: PlayerState::Paused,
          });
      }
      if let Some(ref playback) = self.playback {
          playback.pause().await.map_err(|e| SessionError::PlaybackError(e.to_string()))?;
      }
      // Transition should now succeed because we hold no lock between
      // the check and the call — but if it still fails (concurrent stop),
      // attempt to roll back the subsystem call.
      if let Err(e) = self.try_transition(id, PlayerState::Paused) {
          if let Some(ref playback) = self.playback {
              let _ = playback.resume().await; // best-effort rollback
          }
          return Err(e);
      }
      // ... refresh + broadcast
      Ok(())
  }
  ```

#### BUG-003: `stop()` ignores `playback.stop()` errors but transitions to Idle anyway
- **Severity:** Medium
- **Location:** Line 1013 (`stop()` — `let _ = playback.stop().await;` inside the `if matches!(session.state, ...)` block at lines 1006–1014)
- **Description:** When stopping, the code calls `playback.stop().await` and discards the result with `let _ =`. If the playback engine fails to stop (e.g. GStreamer pipeline teardown hangs or errors), the DB is still transitioned to Idle and the session is deleted. The pipeline may continue running in the background, holding DRM master and consuming CPU.
- **Impact:** A failed pipeline teardown leaves a zombie pipeline running with no DB record. The next `load()` will try to acquire DRM master and fail because the zombie still holds it.
- **Recommendation:** Log the error at minimum, and consider retrying or forcing the teardown. At the very least:
  ```rust
  if let Some(ref playback) = self.playback {
      if let Err(e) = playback.stop().await {
          tracing::error!(error = %e, "playback.stop() failed during stop — pipeline may be orphaned");
      }
  }
  ```
  For a v1.1 hardening, consider adding a `force_stop()` method to `PlaybackTrait` that destroys the pipeline without waiting for graceful teardown.

#### BUG-004: `seek()` from `Paused` emits spurious `Playing` → `Paused` events
- **Severity:** Low
- **Location:** Lines 1078–1082 (`seek()` — `if return_state == PlayerState::Paused` branch)
- **Description:** When seeking from Paused, the code does:
  ```rust
  if return_state == PlayerState::Paused {
      self.try_transition(id, PlayerState::Playing)?;  // emits Playing event
      self.try_transition(id, PlayerState::Paused)?;   // emits Paused event
  }
  ```
  This generates a `Playing` event followed immediately by a `Paused` event, even though the user only sought. Subscribers (WebSocket clients, DLNA renderers) will briefly see "Playing" and then "Paused", which can cause UI flicker or trigger unwanted side effects (e.g. a DLNA controller showing "now playing" then immediately "paused").
- **Impact:** UI flicker on clients; potential side effects in DLNA controllers that react to state transitions.
- **Recommendation:** Either (a) add a `Seeking → Paused` transition to the state machine (currently `Seeking` can only go to `Playing|Error|Idle`), or (b) skip the broadcast for the intermediate `Playing` state. Option (a) is cleaner:
  ```rust
  // In can_transition_to:
  PlayerState::Seeking => {
      matches!(target, PlayerState::Playing | PlayerState::Paused | PlayerState::Error | PlayerState::Idle)
  }
  ```
  Then `seek()` becomes:
  ```rust
  self.try_transition(id, return_state)?;
  ```

#### BUG-005: `load()` always sleeps 300ms before acquiring the display
- **Severity:** Low
- **Location:** Line 786 (`load()` — `tokio::time::sleep(Duration::from_millis(300))`)
- **Description:** A fixed 300ms sleep is inserted unconditionally before `display.acquire()` to give the kernel time to release DRM master after gmediarender exits. The comment acknowledges this is a "conservative safety net". The sleep runs on every `load()`, including first-boot (no gmediarender ever ran) and any load where DLNA isn't in use.
- **Impact:** Adds 300ms of latency to every cast operation. On a device already on the edge of the 10s resolution budget, this is meaningful.
- **Recommendation:** Make the sleep conditional on whether gmediarender was recently active. Track a `last_gmediarender_exit` timestamp (set by the DLNA sync when it stops gmediarender) and only sleep if that timestamp is within the last few seconds. Alternatively, rely on the display manager's internal retry loop (the comment says it already has exponential backoff) and remove the sleep entirely.

#### BUG-006: `recover_crashed_sessions` does not clear `active_session_id` (but doesn't need to)
- **Severity:** Low (informational)
- **Location:** Lines 1339–1380 (`recover_crashed_sessions`)
- **Description:** On startup, this method resets all non-idle/non-error sessions in the DB to Idle. However, `active_session_id` is an in-memory field initialized to `None` in `new()`, so there's no risk of it pointing to a stale session. The recovery is correct, but the relationship between the in-memory state and the DB state is implicit — a future change that persists `active_session_id` across restarts would silently break this invariant.
- **Impact:** None today. Future maintenance hazard.
- **Recommendation:** Add a comment in `recover_crashed_sessions` explicitly noting that `active_session_id` is in-memory only and is intentionally reset to `None` on startup. If `active_session_id` is ever persisted, this method must be updated to clear it.

#### BUG-007: `try_transition` holds the DB lock across the broadcast
- **Severity:** Low
- **Location:** Lines 415–465 (`try_transition` — `let _ = self.event_tx.send(event);` at line 462 while the `db` guard from line 422 is still in scope)
- **Description:** The `db` Mutex guard is held until the end of `try_transition`, including across the `event_tx.send()` call. While `broadcast::Sender::send` is non-blocking (it returns `Err` if there are no receivers or the ring is full, but doesn't wait), holding the lock during the call is unnecessary and adds contention.
- **Impact:** Minor contention under high event rates. With 50 WebSocket clients and per-second position updates, the DB lock is held slightly longer than necessary.
- **Recommendation:** Drop the DB guard before broadcasting:
  ```rust
  {
      let db = self.db.lock()...;
      // ... read, validate, update ...
  } // db guard dropped here
  let event = match target { ... };
  let _ = self.event_tx.send(event);
  Ok(target)
  ```

### Concurrency

#### CONC-001: `load()` check-and-set is atomic, but the subsequent `insert_session` can fail and leave a stale reservation
- **Severity:** Low
- **Location:** Lines 689–740 (`load()` — reservation at lines 691–701, `insert_session` at line 712, cleanup at lines 716–722)
- **Description:** The code atomically checks `active_session_id.is_none()` and sets it to a new UUID inside a single Mutex guard. If the subsequent `insert_session(&session)` fails (e.g. disk full, DB locked), the code clears the reservation. However, between the reservation and the cleanup, another concurrent `load()` call will see `Some(id)` and return `AlreadyActive` — even though the first load is about to fail and clear the slot.
- **Impact:** A transient DB error can cause a brief window where new loads are rejected even though no session is actually active. The window is tiny (only as long as the DB insert takes to fail), but on a slow Pi SD card this could be tens of milliseconds.
- **Recommendation:** Acceptable for v1. Document the behavior. For v2, consider a `tokio::sync::Mutex` around the entire load path (check + insert + resolve + play) so that concurrent loads serialize cleanly.

#### CONC-002: `current_status()` acquires the DB lock on every poll
- **Severity:** Low
- **Location:** Lines 586–596 (`current_status` → `load_session` → `db.lock()`)
- **Description:** `current_status()` is called by HTTP `/api/status` on every poll, by WebSocket's `map_session_event` on every event, and by the background position-update task. Each call acquires the DB Mutex, runs a `SELECT`, and releases. With 50 WS clients and per-second position updates, this is 50+ DB lock acquisitions per second.
- **Impact:** Minor contention. SQLite with WAL mode handles concurrent reads well, but the `std::sync::Mutex` serializes all access (including writes).
- **Recommendation:** For v1, acceptable. For v2, consider caching the latest `MediaSession` in an `ArcSwap<Option<MediaSession>>` updated by `broadcast_state_update()`, so reads don't need to touch the DB at all.

#### CONC-003: `active_session_id` uses `Arc<Mutex<Option<Uuid>>>` but the Arc is never cloned
- **Severity:** Low (informational)
- **Location:** Line 319 (`active_session_id: Arc<Mutex<Option<Uuid>>>`)
- **Description:** The field is wrapped in `Arc`, but a search of the codebase shows no `self.active_session_id.clone()` calls — the `Arc` is unnecessary. The `SessionManager` itself is always wrapped in `Arc` by callers, so the inner `Arc` adds a redundant allocation and indirection.
- **Impact:** Negligible runtime cost; minor confusion for readers.
- **Recommendation:** Remove the `Arc`: `active_session_id: Mutex<Option<Uuid>>`. (The `db` field already follows this pattern.)

### Security

#### SEC-001: No URL length validation in `load()`
- **Severity:** Low
- **Location:** Line 688 (`load()` — `pub async fn load(&self, url: &str)`)
- **Description:** `load()` accepts an arbitrary `&str` URL and stores it in SQLite without length validation. A malicious client could send a multi-megabyte URL that bloats the DB row and the in-memory `MediaSession`. The HTTP layer (`http.rs`) has a 1KB body size limit (`MAX_BODY_SIZE`), which indirectly caps the URL length, but the session layer doesn't enforce its own limit.
- **Impact:** Minimal in v1 (the HTTP body limit protects the session layer). If a future caller bypasses the HTTP layer (e.g. a DLNA handler that constructs URLs differently), the session layer is exposed.
- **Recommendation:** Add a URL length check at the top of `load()`:
  ```rust
  const MAX_URL_LEN: usize = 8192;
  if url.len() > MAX_URL_LEN {
      return Err(SessionError::Subsystem(format!(
          "URL too long: {} bytes (max {})", url.len(), MAX_URL_LEN
      )));
  }
  ```

#### SEC-002: SQL injection surface is zero (positive observation)
- **Severity:** Informational
- **Location:** All DB queries
- **Description:** Every SQL query uses parameterized statements (`rusqlite::params![...]` with `?N` placeholders). No string concatenation is used to build queries. This is correct and should be maintained.
- **Impact:** No SQL injection risk.
- **Recommendation:** None. Add a `#[cfg(test)]` test that attempts a SQL-injection payload in a URL (e.g. `' OR 1=1 --`) and verifies it's stored as a literal string, to guard against future regressions.

### Design Issues

#### DESIGN-001: `try_transition` doesn't broadcast a `Seeking` event
- **Severity:** Low
- **Location:** Line 463 (`try_transition` — `_ => return Ok(target)` arm)
- **Description:** The match in `try_transition` broadcasts events for `Resolving`, `Playing`, `Paused`, `Idle`, `Error`, and `Buffering`, but the `Seeking` arm is `_ => return Ok(target)` (no broadcast). The `seek()` method compensates by broadcasting `SessionEvent::Seeking` separately before calling `try_transition`. This means `Seeking` is the only state transition that requires the caller to manually broadcast.
- **Impact:** Inconsistency — callers must remember to broadcast Seeking separately. If a future code path transitions to Seeking without broadcasting, subscribers won't see the event.
- **Recommendation:** Add a `PlayerState::Seeking` arm to the match in `try_transition` that broadcasts `SessionEvent::Seeking { id, position_ms: 0 }` (or include the position in the transition call). Then remove the manual broadcast from `seek()`.

#### DESIGN-002: `recover_crashed_sessions` doesn't broadcast `Stopped` events
- **Severity:** Low
- **Location:** Lines 1339–1380 (`recover_crashed_sessions`)
- **Description:** When sessions are recovered from `Playing`/`Buffering`/etc. to `Idle` on startup, no `SessionEvent::Stopped` is broadcast. This is correct for the startup case (no subscribers exist yet — `subscribe()` hasn't been called), but if `recover_crashed_sessions` is ever called at runtime (e.g. by a health check), subscribers won't be notified.
- **Impact:** None today. Future maintenance hazard.
- **Recommendation:** Add a comment noting that this method is startup-only and must not be called at runtime without broadcasting events. Or, broadcast `Stopped` events unconditionally — the startup case has no subscribers, so the broadcast is a no-op.

#### DESIGN-003: `MediaSession::new()` always sets `volume: 100`
- **Severity:** Low
- **Location:** Lines 291–313 (`MediaSession::new` — `volume: 100` at line 303)
- **Description:** Every new session starts at volume 100. If a user casts at volume 50, stops, and casts again, the volume resets to 100. This may be surprising — most media players persist the last-used volume.
- **Impact:** Minor UX surprise. Acceptable for v1 (documented behavior).
- **Recommendation:** For v2, consider persisting the last-used volume in a separate settings table and loading it in `MediaSession::new()`. For v1, document in the user guide that volume resets to 100 on each new cast.

#### DESIGN-004: `cleanup_stale_sessions` SQL truncates to seconds
- **Severity:** Low
- **Location:** Lines 1318–1330 (`cleanup_stale_sessions` — `strftime('%Y-%m-%dT%H:%M:%S+00:00', 'now', '-24 hours')` at line 1325)
- **Description:** The `strftime` format string truncates to whole seconds. Sessions updated in the last second before the 24-hour cutoff will be deleted even though they're technically younger than 24 hours. The `updated_at` column stores RFC3339 with microseconds (`to_rfc3339()`), so the comparison is microsecond-precision on the left side but second-precision on the right.
- **Impact:** Negligible — at most 1 second of error in the cleanup window. Sessions are only deleted if they're 24h old, so a 1-second error is immaterial.
- **Recommendation:** For correctness, use `strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '-24 hours')` (millisecond precision) or compare against a stored `DateTime<Utc>` in Rust rather than in SQL. Low priority.

#### DESIGN-005: No DB indexes on `updated_at` or `state`
- **Severity:** Low
- **Location:** `CREATE TABLE sessions` (lines 351–365)
- **Description:** The `sessions` table has no indexes other than the primary key on `id`. The `cleanup_stale_sessions` query filters on `updated_at`, and `recover_crashed_sessions` filters on `state`. Both queries are currently fast because the table is small (single-session-at-a-time means at most a few rows), but if the cleanup window is ever extended or if multiple sessions accumulate, these queries will degrade.
- **Impact:** None today (table size is bounded by the 24h cleanup). Future-proofing concern.
- **Recommendation:** Add indexes:
  ```sql
  CREATE INDEX IF NOT EXISTS idx_sessions_updated_at ON sessions(updated_at);
  CREATE INDEX IF NOT EXISTS idx_sessions_state ON sessions(state);
  ```
  Low priority — only worth doing if the table is expected to grow beyond a few hundred rows.

#### DESIGN-006: No method to list all sessions (history)
- **Severity:** Low
- **Location:** Public API surface
- **Description:** `SessionManager` exposes `load_session(id)`, `current_status()`, and `status(id)`, but no `list_sessions()` method. There's no way to build an admin UI showing session history without direct DB access.
- **Impact:** Limits future admin/observability features.
- **Recommendation:** For v2, add `list_sessions(limit: u32) -> Result<Vec<MediaSession>, SessionError>` that returns the most recent N sessions. Useful for a debug dashboard.

#### DESIGN-007: `SessionEvent::Buffering { percent: u8 }` always sends 0 from `try_transition`
- **Severity:** Low
- **Location:** Line 462 (`try_transition` — `PlayerState::Buffering => SessionEvent::Buffering { id: session_id, percent: 0 }`)
- **Description:** When transitioning to `Buffering` via `try_transition`, the event is hardcoded to `percent: 0`. The actual buffering percentage comes later from the playback engine's bus watch (in `lib.rs` of the playback crate), which calls `broadcast_event(SessionEvent::Buffering { percent, ... })` directly. This is correct but means the initial `Buffering` event from `try_transition` is always 0%, then a subsequent event from the bus watch updates it.
- **Impact:** Minor — clients see a 0% buffering event followed by the real percentage. Acceptable for v1.
- **Recommendation:** Document that the initial `Buffering` event is always 0% and that subsequent events come from the playback engine. Or, don't broadcast a `Buffering` event from `try_transition` at all — let the playback engine be the sole source of buffering progress.

### Missing Tests

#### TEST-001: No test for the CDN 403 retry logic in `load()`
- **Severity:** High
- **Description:** The most complex code path in the file — the retry loop at lines 795–925 — has no test coverage. The loop handles `is_cdn_retryable_error`, `invalidate_cache`, `re_resolve`, and the `used_tor` flag flipping between attempts. None of this is tested.
- **Impact:** A regression in the retry logic (e.g. wrong retry count, wrong isolation username after re-resolve, infinite loop on certain errors) would not be caught by CI. This is the file's highest-risk untested code.
- **Recommendation:** Add a `MockPlayback` variant that fails the first N `play()` calls with a "CDN IP mismatch" error and succeeds on the Nth. Verify:
  - `resolver.invalidate_cache()` is called between attempts.
  - `re_resolve()` is called and the new URL is used on retry.
  - The retry count matches `max_retries` (2).
  - After exhaustion, the session transitions to `Error` and `active_session_id` is cleared.
  - When `used_tor` flips to `false` after re-resolve, the retry uses empty `socks_addr` and `isolation_username`.

#### TEST-002: No test for `is_cdn_retryable_error`
- **Severity:** Medium
- **Description:** The helper function `is_cdn_retryable_error` (lines 60–65) matches on three string patterns: `"CDN IP mismatch"`, `"re-resolve needed"`, and `"Forbidden"`. It's not tested.
- **Impact:** A change to the matching patterns (e.g. narrowing "Forbidden" to a more specific pattern) could silently break the retry logic.
- **Recommendation:** Add a unit test:
  ```rust
  #[test]
  fn test_is_cdn_retryable_error() {
      assert!(is_cdn_retryable_error(&"CDN IP mismatch: 1.2.3.4 vs 5.6.7.8".into()));
      assert!(is_cdn_retryable_error(&"re-resolve needed".into()));
      assert!(is_cdn_retryable_error(&"HTTP 403 Forbidden".into()));
      assert!(!is_cdn_retryable_error(&"network timeout".into()));
      assert!(!is_cdn_retryable_error(&"pipeline error".into()));
  }
  ```

#### TEST-003: No test for `set_audio_device` / `set_audio_sink` / `audio_device`
- **Severity:** Medium
- **Description:** The audio device/sink methods (lines 601, 613, 628) delegate to the playback subsystem but have no tests verifying that the delegation works or that errors are mapped correctly.
- **Impact:** A regression in the delegation (e.g. wrong error mapping, missing `await`) wouldn't be caught.
- **Recommendation:** Add tests that:
  - `set_audio_device("plughw:1,0")` calls `MockPlayback::set_audio_device` with the same value.
  - `set_audio_sink("pulsesink")` calls `MockPlayback::set_audio_sink` with the same value.
  - `audio_device()` returns the value from `MockPlayback::audio_device`.
  - When no playback subsystem is configured, all three return `SessionError::Subsystem`.

#### TEST-004: No test for `broadcast_event` (public method)
- **Severity:** Low
- **Description:** The public `broadcast_event` method (line 664) allows external code (e.g. the playback event listener in `main.rs`) to broadcast events through the session's channel. It's not tested.
- **Impact:** Low — the method is a one-liner wrapper around `event_tx.send()`. But a test would document the intended use.
- **Recommendation:** Add a test that calls `mgr.broadcast_event(SessionEvent::AudioDeviceError { id, message: "test".into() })` and verifies a subscriber receives it.

#### TEST-005: No test for the `pause()` / `resume()` rollback path
- **Severity:** Low
- **Description:** The inconsistency described in BUG-002 (subsystem paused but DB transition fails) is not tested. A test that forces `try_transition` to fail after `playback.pause()` succeeds would document the current (broken) behavior and catch a future fix.
- **Impact:** Low — the behavior is a known bug (BUG-002). But a regression test would ensure the fix doesn't break.
- **Recommendation:** Add a test after fixing BUG-002 that verifies the rollback path.

#### TEST-006: No test for `load()` without a display subsystem
- **Severity:** Low
- **Description:** `load()` has a `if let Some(ref display) = self.display` guard around `display.acquire()`. The "no display" path (headless mode) is not tested.
- **Impact:** A regression that requires a display (e.g. removing the `if let Some`) wouldn't be caught in headless CI.
- **Recommendation:** Add a test that constructs a `SessionManager` with resolver + playback + tor but no display, and verifies `load()` succeeds.

## Positive Observations

1. **State machine is rigorously defined** — the `can_transition_to` table is exhaustive, the `transition()` method returns a typed `InvalidTransition` error, and 30+ tests cover every valid and invalid transition. This is exemplary for a Rust state machine.

2. **Dual-channel event distribution** — using `broadcast` for streaming (WebSocket) and `watch` for polling (HTTP `/api/status`) is the right design. Each channel type matches its consumer's access pattern, and the watch channel's "latest value only" semantics avoid unbounded buffering for pollers.

3. **Crash recovery on startup** — `recover_crashed_sessions` correctly resets non-idle sessions to Idle, recognizing that the playback pipeline is gone after a process restart. The test using a file-based DB (`test_crash_recovery_recover_crashed_sessions`) verifies this end-to-end across manager instances.

4. **CDN 403 retry logic** — the inline retry loop in `load()` handles a subtle case correctly: after re-resolve, the URL is bound to the resolver's exit IP, so playback must use the *same* base isolation circuit (not a retry-specific circuit). The code comment explains this clearly. The logic is correct, even though it lacks tests (TEST-001).

5. **Subsystem trait abstraction** — `ResolverTrait`, `PlaybackTrait`, `DisplayTrait`, `TorTrait` cleanly invert dependencies. The session layer has no compile-time dependency on GStreamer, DRM, or Tor concrete types. This makes the session layer testable in isolation (as the comprehensive mock-based test suite demonstrates).

6. **Parameterized SQL everywhere** — no string concatenation in any query. SQL injection surface is zero.

7. **WAL journal mode** — explicitly enabled for better concurrent read performance, with a test (`test_wal_mode_enabled`) verifying the PRAGMA succeeds.

8. **`active_session_id` reservation pattern** — the check-and-set is atomic within a single Mutex guard, preventing the race where two concurrent `load()` calls both pass the "no active session" check.

9. **Comprehensive lifecycle tests** — the test suite covers load → pause → resume → seek → set_volume → stop, plus error paths (resolution failure, already-active rejection, no-active-session errors). The `test_full_lifecycle_load_pause_resume_stop` test is a good integration-style test.

10. **`refresh_playback_position` after every state change** — ensures the DB stays roughly in sync with the actual playback position, even though the background position-update task is the primary source of position updates.

## Recommendations Summary

| Priority | Finding | Effort |
|----------|---------|--------|
| High | TEST-001: Add CDN 403 retry logic tests | M (3–4 h) |
| Medium | BUG-001: Fix volume clamping range (255 → 100) | S (15 min) |
| Medium | BUG-002: Roll back subsystem call if DB transition fails | M (2–3 h) |
| Medium | BUG-003: Log `playback.stop()` errors during `stop()` | S (15 min) |
| Medium | TEST-002: Add `is_cdn_retryable_error` unit test | S (15 min) |
| Medium | TEST-003: Add audio device/sink delegation tests | S (1 h) |
| Low | BUG-004: Add `Seeking → Paused` transition (eliminate spurious events) | S (30 min) |
| Low | BUG-005: Make the 300ms display-acquire sleep conditional | S (1 h) |
| Low | BUG-006: Document `active_session_id` in-memory invariant | S (5 min) |
| Low | BUG-007: Drop DB guard before broadcasting in `try_transition` | S (15 min) |
| Low | CONC-001: Document the brief reservation window in `load()` | S (5 min) |
| Low | CONC-002: Cache latest session in `ArcSwap` for reads (v2) | M (2–3 h) |
| Low | CONC-003: Remove redundant `Arc` on `active_session_id` | S (5 min) |
| Low | SEC-001: Add URL length validation in `load()` | S (15 min) |
| Low | DESIGN-001: Broadcast `Seeking` from `try_transition` | S (30 min) |
| Low | DESIGN-002: Document startup-only constraint on `recover_crashed_sessions` | S (5 min) |
| Low | DESIGN-003: Persist last-used volume across sessions (v2) | M (2 h) |
| Low | DESIGN-004: Use millisecond precision in `cleanup_stale_sessions` | S (15 min) |
| Low | DESIGN-005: Add indexes on `updated_at` and `state` | S (15 min) |
| Low | DESIGN-006: Add `list_sessions()` for admin UI (v2) | S (1 h) |
| Low | DESIGN-007: Document initial `Buffering` event is always 0% | S (5 min) |
| Low | TEST-004: Add `broadcast_event` test | S (15 min) |
| Low | TEST-005: Add rollback path test (after BUG-002 fix) | S (30 min) |
| Low | TEST-006: Add `load()` without display subsystem test | S (30 min) |
