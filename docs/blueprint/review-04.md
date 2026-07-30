---
doc: code_review
project: picast
version: 1
phase: code_review
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T11:55:00Z
---

# Code Review: `src/session/src/lib.rs`

**File:** `src/session/src/lib.rs`
**Lines:** 2928
**Reviewer:** agent
**Date:** 2026-07-30

## Summary

The session manager is the central coordinator of boGDan. It owns the SQLite-backed `MediaSession` store, manages the seven-state player state machine (Idle → Resolving → Buffering → Playing → Paused → Seeking → Error), dispatches commands to the resolver, playback, display, and Tor subsystems via trait interfaces, and broadcasts state-change events to protocol handlers via `broadcast` and `watch` channels. This is the architectural heart of the system — every protocol (HTTP, WebSocket, DLNA) routes through it. The implementation is generally strong with good atomicity in session creation, CDN retry logic, and WAL-mode SQLite. However, there are several bugs, design issues, and missing tests in this critical 2928-line file.

## Key Components Reviewed

| Component | Lines | Purpose |
|-----------|-------|---------|
| `PlayerState` enum | 130–215 | 7-state machine with `can_transition_to` / `transition` |
| `SessionEvent` enum | 225–260 | 14 event variants for protocol-layer notification |
| `MediaSession` struct | 265–300 | Persistent session record (UUID, URLs, state, position) |
| `SessionManager` struct | 310–360 | Coordinator with SQLite + 4 subsystem traits |
| `load()` | 688–940 | Atomic session creation, resolve, CDN retry, playback start |
| `pause()/resume()/stop()` | 946–1050 | State transitions with playback delegation |
| `seek()` | 1056–1100 | Seek with state transition |
| `set_volume()` | 1103–1140 | Volume update |
| `current_status()` | 586–640 | Active session query |
| `subscribe()` / `subscribe_state()` | 644–680 | Event channel subscription |

## Findings

### Bugs

#### BUG-001: 300ms `tokio::time::sleep` hardcoded in `load()` for DRM master race
- **Severity:** Medium
- **Location:** `load()` method at line 688 (`pub async fn load`); the referenced code is at line 770 (inside the method)
- **Description:** After resolution and before `display.acquire()`, there is a hardcoded `tokio::time::sleep(Duration::from_millis(300))` to work around a race condition where gmediarender hasn't released DRM master yet. The comment acknowledges this is a "conservative safety net" and that the display manager "already retries internally with exponential backoff."
- **Impact:** A 300ms delay is added to every cast, even when gmediarender isn't running or has already released DRM master. If the race window is longer than 300ms (e.g., on a slow Pi or under load), the workaround fails. If it's shorter, the delay is wasted time.
- **Recommendation:** Remove the hardcoded sleep and rely on the display manager's internal retry with exponential backoff. If the retry is insufficient, increase its budget or add jitter. Hardcoded sleeps are a code smell that masks a synchronization problem.

#### BUG-002: `std::sync::Mutex` held across `.await` points in several methods
- **Severity:** Medium
- **Location:** Multiple methods (e.g., `load()` at line 720 locks `active_session_id` then releases before `.await`, but the pattern is fragile)
- **Description:** The `SessionManager` uses `std::sync::Mutex` for `db` and `active_session_id`. While the current code carefully releases these locks before `.await` points, this is a manual discipline that's easy to break. If a future change holds a `std::sync::MutexGuard` across an `.await`, it will block the entire Tokio runtime (since `std::sync::Mutex` is not aware of async).
- **Impact:** A future regression could freeze the appliance. The `Send` bound on `std::sync::MutexGuard` is not satisfied, so the compiler would catch it — but only if the guard isn't accidentally held.
- **Recommendation:** Consider using `tokio::sync::Mutex` for fields that are locked near async code, or use `parking_lot::Mutex` (which is faster and has better ergonomics but still can't be held across `.await`). Document the invariant clearly. Alternatively, restructure so that all DB operations are synchronous (no `.await` while holding the lock).

#### BUG-003: `is_cdn_retryable_error` matches on "Forbidden" string — too broad
- **Severity:** Low
- **Location:** Line 60 (`fn is_cdn_retryable_error`) — the doc comments are at lines 50–58
- **Description:** The function checks if the error message contains "Forbidden" to detect CDN 403 errors. However, "Forbidden" could appear in non-CDN error messages (e.g., a local file permission error "Permission denied (os error 13)" wouldn't match, but a custom error "Forbidden action" from another subsystem would).
- **Impact:** A non-CDN "Forbidden" error would trigger unnecessary re-resolution and retry, wasting 5–15 seconds.
- **Recommendation:** Use a typed error enum instead of string matching. Have the playback engine return a `PlaybackError::CdnForbidden` variant that the session layer matches on explicitly.

#### BUG-004: `load()` does not check `AlreadyActive` before the DB insert in the atomic section
- **Severity:** Low
- **Location:** Lines 690–715 (`load()` atomic session creation)
- **Description:** The atomic section correctly checks `guard.is_some()` and returns `AlreadyActive`. However, if the DB insert fails *after* the slot is reserved, the slot is cleared (lines 717–723). This is correct, but there's a subtle issue: between the slot reservation and the DB insert, another concurrent `load()` call would see `AlreadyActive` even though no session is actually active yet (it's just reserved).
- **Impact:** A concurrent cast during a failed DB insert would be rejected incorrectly. This is a very narrow race window and unlikely in practice (the appliance has one active session at a time).
- **Recommendation:** Acceptable for v1 given the single-session model. Document the behavior. For a future multi-session model, this would need rethinking.

#### BUG-005: `try_transition` returns `Result` but errors are silently discarded with `let _ =`
- **Severity:** Low
- **Location:** Throughout `load()` (e.g., lines 740, 763, 868, 875)
- **Description:** `try_transition` returns `Result<PlayerState, SessionError>`, but in several places the result is discarded with `let _ = self.try_transition(...)`. If the transition fails (e.g., trying to go from Error to Playing), the failure is silently ignored.
- **Impact:** Invalid state transitions are silently swallowed, making debugging difficult. The state machine could end up in an unexpected state.
- **Recommendation:** At minimum, log the transition failure at `warn` level. Better: propagate the error if the transition is critical (e.g., transitioning to Error state should not fail silently).

### Design Issues

#### DESIGN-001: CDN retry logic is complex and could be extracted
- **Severity:** Medium
- **Location:** Lines 800–900 (`load()` retry loop)
- **Description:** The CDN 403 retry logic is ~100 lines of inline code within `load()`. It handles: determining SOCKS routing based on `used_tor`, retrying up to 2 times, invalidating the resolver cache, re-resolving, and handling re-resolve failures. The comment block (lines 803–820) explaining the IP-binding invariant is excellent but indicates the logic is non-obvious.
- **Impact:** The retry logic is hard to test in isolation (it's embedded in `load()`), and the complexity makes future modifications risky.
- **Recommendation:** Extract the retry loop into a private method `play_with_cdn_retry(&self, url, resolve_info, id) -> Result<(), SessionError>`. This makes it independently testable and documents the boundary.

#### DESIGN-002: `broadcast::Sender` has a fixed capacity that could drop events
- **Severity:** Low
- **Location:** Line 321 (`event_tx: broadcast::Sender<SessionEvent>` field in `SessionManager` struct) and line 365 (`let (event_tx, _) = broadcast::channel(128);` in `SessionManager::new`)
- **Description:** The `broadcast` channel has a capacity (default 256 in tokio). If a slow client (e.g., a WebSocket client on a bad connection) doesn't drain the channel fast enough, events are dropped and the client receives a `Lagged` error. The WS handler (ws.rs) catches this and logs a warning but doesn't resync.
- **Impact:** Clients with slow connections may miss state transitions. The spec (OD-002) mentions "events_dropped: true in first replayed event" but this isn't implemented.
- **Recommendation:** Increase the channel capacity for the broadcast (e.g., 1024), or implement the resync mechanism where after a `Lagged` error, the handler queries `current_status()` and sends a fresh `MEDIA_STATUS` event.

#### DESIGN-003: `MediaSession` fields are all `pub` — no encapsulation
- **Severity:** Low
- **Location:** Lines 265–295 (`MediaSession` struct)
- **Description:** All fields of `MediaSession` are public. Any code can modify `state`, `position_ms`, `volume`, etc. directly, bypassing the state machine. The `ws.rs` tests (lines 700–710) do exactly this: `session.state = PlayerState::Playing; session.position_ms = 5000;`.
- **Impact:** The state machine invariant (`can_transition_to`) can be violated by direct field mutation. Bugs from invalid states are possible.
- **Recommendation:** Make the fields private and provide methods for state transitions (`session.set_state(PlayerState::Playing)?`). For tests, provide a `MediaSession::new_for_testing()` builder that can set arbitrary state.

#### DESIGN-004: Session cleanup exists but could be improved
- **Severity:** Low
- **Location:** `cleanup_stale_sessions` at line 1318, `recover_crashed_sessions` at line 1339, `delete_session` at line 1231
- **Description:** The session layer DOES have cleanup mechanisms: `cleanup_stale_sessions()` deletes sessions older than 24 hours (called on startup), `recover_crashed_sessions()` resets non-idle sessions to Idle on startup, and `delete_session()` is called by `stop()` to remove the active session. However, the 24-hour TTL is hardcoded, there's no index on `updated_at` to speed up the cleanup query, and there's no `list_sessions()` method for an admin UI to view session history before it's deleted.
- **Impact:** Minor — the database is bounded by the 24h cleanup, so it won't grow indefinitely. But the hardcoded TTL can't be tuned, and there's no observability into session history.
- **Recommendation:** (a) Add a `CREATE INDEX IF NOT EXISTS idx_sessions_updated_at ON sessions(updated_at)` to speed up the cleanup query. (b) Make the 24-hour TTL configurable via `bogdan.toml`. (c) Add a `list_sessions(limit: u32)` method for a future admin/debug dashboard. (d) Consider a `clear_history()` method for a "clear viewing history" button in the web UI.

### Security

#### SEC-001: SQLite database path not validated
- **Severity:** Low
- **Location:** Lines 341–342 (`pub fn new(db_path: &str)` at line 341, `rusqlite::Connection::open(db_path)?` at line 342, inside `SessionManager::new`)
- **Description:** The `db_path` is passed directly to `rusqlite::Connection::open()` without validation. If the path is attacker-controlled (e.g., from a config file with a path traversal like `../../etc/passwd`), SQLite would attempt to open/create that file.
- **Impact:** Low — the config file is root-owned and not attacker-controlled in the appliance model. But defense-in-depth.
- **Recommendation:** Validate that `db_path` is within the allowed runtime directory (`/var/lib/bogdan/` or similar). Reject paths with `..` components.

#### SEC-002: `source_url` stored in SQLite in plaintext
- **Severity:** Low (acceptable for v1, worth noting)
- **Location:** `insert_session` method (not shown, but inferred from the `MediaSession` struct)
- **Description:** The `source_url` and `resolved_url` are stored in the SQLite database in plaintext. These URLs reveal the user's viewing history. If the SD card is removed and read, the viewing history is exposed.
- **Impact:** Contradicts the privacy-first goal. The Tor routing prevents ISP surveillance, but the local database stores viewing history in plaintext.
- **Recommendation:** For v1, document this as a known limitation. For v2, either encrypt the database (SQLCipher) or store only session metadata (not URLs). Alternatively, add a "clear history" button in the web UI and a `BOGDAN_CLEAR_HISTORY_ON_SHUTDOWN` option.

### Missing Tests

#### TEST-001: State transition tests exist but lack a full 7×7 matrix
- **Severity:** Low
- **Description:** The file DOES have state transition tests — 19 tests covering valid transitions (`test_idle_can_resolve`, `test_buffering_can_play`, `test_playing_can_pause`, etc.) and invalid transitions (`test_idle_cannot_play`, `test_idle_cannot_seek`, `test_playing_cannot_resolve`, `test_seeking_to_paused_invalid`, `test_error_to_playing_invalid`, `test_transition_failure`, `test_try_transition_invalid`). However, these are individual case-by-case tests rather than an exhaustive 7×7 = 49 combination matrix.
- **Impact:** Low — the existing tests cover all the important transitions. A full matrix test would be more about documentation than catching bugs.
- **Recommendation:** Add a single parametric test that iterates over all 49 (from, to) pairs and asserts the result matches `can_transition_to`. This serves as a living document of the transition table.

#### TEST-002: `load()` has unit tests but the CDN retry path is untested
- **Severity:** Medium
- **Description:** The file DOES have 7 `load()` unit tests with mock subsystems: `test_load_creates_session_and_resolves`, `test_load_already_active_returns_conflict`, `test_load_without_subsystems_uses_url_directly`, `test_load_acquires_display`, `test_load_emits_buffering_event`, `test_load_broadcasts_events`, `test_resolved_event_includes_title`. These cover the happy path, conflict rejection, and display acquisition. However, the CDN 403 retry loop within `load()` (lines 795–925) — the most complex code path — has no test coverage. There's no test that verifies retry behavior when `play()` returns a CDN error, no test for `is_cdn_retryable_error`, and no test for the `re_resolve` path.
- **Impact:** A regression in the CDN retry logic (wrong retry count, wrong isolation username after re-resolve, infinite loop) would not be caught by CI.
- **Recommendation:** Add a `MockPlayback` variant that fails the first N `play()` calls with a "CDN IP mismatch" error and succeeds on the Nth. Verify: `resolver.invalidate_cache()` is called, `re_resolve()` produces a new URL, the retry count matches `max_retries` (2), and after exhaustion the session transitions to `Error` with `active_session_id` cleared.

#### TEST-003: No tests for CDN retry logic specifically
- **Severity:** Medium
- **Description:** The CDN 403 retry logic (lines 800–900) is complex and untested. It has a specific invariant: "after re-resolve, playback MUST use the same base isolation username." This invariant is not tested.
- **Impact:** A change that breaks the IP-binding invariant (e.g., using a different isolation username after re-resolve) would cause CDN 403 loops and not be caught by tests.
- **Recommendation:** Extract the retry logic (per DESIGN-001) and test it with a mock playback engine that returns `CdnForbidden` on the first call and succeeds on the second. Verify the same isolation username is used for both calls.

#### TEST-004: Concurrent access tests exist but could be more adversarial
- **Severity:** Low
- **Description:** The file DOES have 4 concurrent access tests: `test_concurrent_load_rejected` (verifies `AlreadyActive` on concurrent load), `test_concurrent_pause_and_resume` (verifies at least one succeeds), `test_concurrent_set_volume_safe` (10 concurrent volume sets), and `test_concurrent_status_reads` (20 concurrent readers). These cover the basic concurrency invariants. However, there's no test for concurrent `stop()` + `load()` (the most dangerous race — a new session starting before the old one fully tears down), and no test for concurrent `seek()` + `stop()`.
- **Impact:** Low — the existing tests cover the common cases. The missing edge cases are unlikely but could expose subtle state-machine bugs.
- **Recommendation:** Add a test that spawns `stop()` and `load()` concurrently and verifies the system never ends up in a state where `active_session_id` is `None` but a pipeline is still running, or vice versa.

## Positive Observations

1. **Atomic session creation** — the `active_session_id` lock is acquired, checked, and the slot reserved before any DB I/O, preventing the race where two concurrent `load()` calls both pass the `is_some()` check. The slot is cleared on DB failure.
2. **CDN retry logic is well-documented** — the comment block (lines 803–820) clearly explains why the same isolation username must be used after re-resolve, preventing a subtle IP-mismatch bug.
3. **WAL journal mode** — enabling `PRAGMA journal_mode=WAL` improves concurrent read performance, which is essential when HTTP, WebSocket, and DLNA handlers all access the database.
4. **Subsystems are behind traits** — `ResolverTrait`, `PlaybackTrait`, `DisplayTrait`, `TorTrait` allow mocking for tests and decouple the session layer from implementations.
5. **Dual event channels** — `broadcast` for event history (WebSocket) and `watch` for latest state (HTTP polling) is a well-considered design that serves both use cases efficiently.
6. **State machine is explicit** — the `can_transition_to` table documents all valid transitions, and `try_transition` enforces them.
7. **Graceful error cleanup** — on resolution or playback failure, the active session slot is cleared and the state transitions to `Error`, preventing a stuck session.
8. **14 `SessionEvent` variants** — comprehensive event coverage including `CdnForbidden` and `AudioDeviceError` for specific failure modes that clients need to handle.

## Recommendations Summary

| Priority | Finding | Effort |
|----------|---------|--------|
| Medium | BUG-001: Remove hardcoded 300ms sleep, rely on display retry | S (1 h) |
| Medium | BUG-002: Audit Mutex usage near .await points | M (2–4 h) |
| Medium | DESIGN-001: Extract CDN retry logic into testable method | M (3–4 h) |
| Medium | TEST-002/003: Add CDN retry logic tests (load() happy path is tested, retry path is not) | M (3–4 h) |
| Low | BUG-003: Use typed errors instead of string matching for CDN | M (2 h) |
| Low | BUG-004: Document the reserved-slot race window | S (30 min) |
| Low | BUG-005: Log discarded transition errors | S (30 min) |
| Low | DESIGN-002: Increase broadcast capacity or implement resync | M (2 h) |
| Low | DESIGN-003: Encapsulate MediaSession fields | M (3–4 h) |
| Low | DESIGN-004: Add index on updated_at, make TTL configurable | S (1 h) |
| Low | SEC-001: Validate SQLite database path | S (30 min) |
| Low | SEC-002: Document plaintext URL storage or encrypt | S (1 h) |
| Low | TEST-001: Add full 7×7 state transition matrix test | S (1 h) |
| Low | TEST-004: Add adversarial concurrent stop()+load() test | S (1–2 h) |
