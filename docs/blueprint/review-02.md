---
doc: code_review
project: picast
version: 1
phase: code_review
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T11:00:00Z
---

# Code Review: `src/protocols/src/ws.rs`

**File:** `src/protocols/src/ws.rs`
**Lines:** 884
**Reviewer:** agent
**Date:** 2026-07-30

## Summary

The WebSocket server provides a bidirectional event stream for real-time UIs (browser extension, web dashboard). Clients subscribe to player-state events and send control commands through a single long-lived socket. The implementation uses `tokio-tungstenite` with optional TLS, a `Semaphore`-based connection limit, and both WS-level and application-level ping/pong. The code is well-structured with good event mapping, thorough serialization tests, and reasonable connection management. However, there are several bugs, security concerns, and design issues that should be addressed.

## Endpoints Reviewed

| Direction | Message Type | Fields | Notes |
|-----------|-------------|--------|-------|
| Client → Server | `CAST` | `url` | URL validation duplicated from http.rs |
| Client → Server | `STOP` | — | — |
| Client → Server | `PAUSE` | — | — |
| Client → Server | `RESUME` | — | — |
| Client → Server | `SEEK` | `position_ms` | No `position_seconds` alternative (unlike http.rs) |
| Client → Server | `VOLUME` | `volume` | Clamped to 0–100 |
| Client → Server | `PING` | — | Application-level keep-alive |
| Server → Client | `MEDIA_STATUS` | state, position, duration, volume, source, title | — |
| Server → Client | `RESOLVE_PROGRESS` | `percent` | — |
| Server → Client | `ERROR` | `message` | — |
| Server → Client | `CONNECTED` | — | Sent on connect |
| Server → Client | `PONG` | — | Response to application-level PING |

## Findings

### Bugs

#### BUG-001: `MAX_CONNECTIONS` constant (50) contradicts doc comment (32)
- **Severity:** Low
- **Location:** Line 54 (`const MAX_CONNECTIONS: usize = 50;`) and lines 66–67 (`enum ClientCommand { Cast { ... }`)
- **Description:** The module-level doc comment says "A maximum of 32 concurrent WebSocket clients are allowed." The constant `MAX_CONNECTIONS` is set to `50`. The `WebSocketServer` struct doc comment says "Connection limit: `MAX_CONNECTIONS` (default 50) concurrent clients."
- **Impact:** Documentation inconsistency; the actual limit is 50, not 32 as the module doc states.
- **Recommendation:** Update the module doc comment to say 50, or change `MAX_CONNECTIONS` to 32 if 32 was the intended limit.

#### BUG-002: Connection limit rejection does WS handshake before sending error
- **Severity:** Medium
- **Location:** Lines 195–215 (connection limit rejection path)
- **Description:** When the connection limit is reached, the code calls `accept_ws()` to complete the WebSocket handshake, sends an `Error` event, then sends a `Close` frame. This means a flood of connection attempts when at capacity will each complete a full TLS handshake + WS upgrade before being rejected — consuming CPU and memory for rejected connections.
- **Impact:** A connection flood at capacity can DoS the server by forcing it to complete handshakes for connections it will immediately reject.
- **Recommendation:** For plain WS, reject at the HTTP upgrade level by returning a 429 status before the WS handshake completes. For WSS, this is harder (the TLS handshake must complete first), but at minimum the error path should be fast. Consider tracking connection count at the TCP accept level and rejecting before the handshake.

#### BUG-003: `ClientCommand::Ping` has a dead arm in `handle_command`
- **Severity:** Low
- **Location:** Line 461 (`ClientCommand::Ping => { ... }` arm in `handle_command`, which starts at line 430)
- **Description:** The `ClientCommand::Ping` variant is handled in `handle_client` (line 339) before `handle_command` is called, so the `Ping` arm in `handle_command` is never reached. The code acknowledges this with a comment, but the arm exists with a no-op body.
- **Impact:** Dead code; no functional impact, but confusing to readers.
- **Recommendation:** Either remove the `Ping` arm from `handle_command` (and let the match be exhaustive via `#[allow(unreachable_patterns)]` or by restructuring), or document more clearly why it must exist for exhaustiveness.

#### BUG-004: No timeout on idle connections
- **Severity:** Medium
- **Location:** `handle_client` function (lines 306–430 — `async fn handle_client` at line 306)
- **Description:** The server sends WS-level pings every 30 seconds but does not enforce a timeout if the client never responds to pings. A client that connects, sends nothing, and ignores pings will hold its connection permit indefinitely. The doc comment says "Clients that don't respond within 10 seconds are disconnected" but there is no code implementing this.
- **Impact:** A malicious or buggy client can hold a connection slot forever, eventually exhausting the 50-connection limit. The documented 10-second pong timeout is not implemented.
- **Recommendation:** Track the last pong receipt time per connection. If no pong is received within `2 × WS_PING_INTERVAL_SECS` (60 seconds), close the connection. Use `tokio::time::timeout` around the `ws.next()` future.

#### BUG-005: `broadcast::error::RecvError::Lagged` does not send a recovery event to the client
- **Severity:** Low
- **Location:** Lines 408–409 (`Err(broadcast::error::RecvError::Lagged(count)) => { ... }`)
- **Description:** When the broadcast channel lags (client is slow), the server logs a warning but does not inform the client that events were dropped. The client may have a stale view of the state with no indication.
- **Impact:** Clients with slow connections may miss state transitions silently.
- **Recommendation:** After a Lagged error, query the session manager for the current state and send a `MEDIA_STATUS` event to resynchronize the client. This matches the spec's note (OD-002 in the fine draft) about "events_dropped: true in first replayed event."

### Security

#### SEC-001: No URL validation for `javascript:` scheme
- **Severity:** Low
- **Location:** Lines 432–453 (`handle_command` `Cast` arm — `ClientCommand::Cast { url } => {` at line 432, `session.load(&url)` at line 443)
- **Description:** The URL validation in `handle_command` rejects `file://`, `data:`, and unknown schemes, but does not explicitly reject `javascript:`. The `url::Url::parse` function parses `javascript:alert(1)` as a valid URL with scheme `javascript`, which would fall through to the `scheme =>` catch-all arm and be rejected. This is functionally correct but less explicit than the http.rs implementation.
- **Impact:** No actual vulnerability (the catch-all rejects it), but the intent is less clear.
- **Recommendation:** Add an explicit `javascript` arm for consistency with `http.rs` and clarity of intent.

#### SEC-002: No authentication or origin check
- **Severity:** Medium (acceptable for v1, must fix for v2)
- **Location:** `start()` and `handle_client()`
- **Description:** Any client that can reach port 8586 can connect and send commands. There is no `Origin` header check (WebSocket's built-in CSRF protection) and no authentication token.
- **Impact:** Any website on the LAN can open a WebSocket to the appliance and control playback. Unlike HTTP, WebSocket doesn't have CORS, but the `Origin` header check is the standard mitigation.
- **Recommendation:** For v1, document as a known limitation. For v2, check the `Origin` header during the WS handshake and reject connections from untrusted origins.

#### SEC-003: Binary message parsing accepts arbitrary UTF-8
- **Severity:** Low
- **Location:** Lines 360–375 (`Some(Ok(Message::Binary(data))) => { ... }` at line 360)
- **Description:** Binary messages are parsed as UTF-8 JSON. This is a convenience for clients that send binary frames, but it expands the attack surface — a binary flood could consume CPU on UTF-8 validation and JSON parsing.
- **Impact:** Minimal, given the 1 MB message size limit and 50-connection cap.
- **Recommendation:** Acceptable for v1. Consider rejecting binary messages in v2 unless a use case emerges.

### Design Issues

#### DESIGN-001: URL validation duplicated between http.rs and ws.rs
- **Severity:** Medium
- **Location:** `http.rs` `is_safe_cast_url()` (lines 710–725) vs `ws.rs` `handle_command` `Cast` arm (lines 432–453)
- **Description:** Both files validate cast URLs, but with different implementations. `http.rs` has a dedicated `is_safe_cast_url()` function with explicit `javascript` handling and clear error messages. `ws.rs` has inline validation with a catch-all. They could diverge over time.
- **Impact:** Inconsistent URL validation; a URL accepted via one protocol could be rejected via the other, or vice versa.
- **Recommendation:** Extract `is_safe_cast_url()` into a shared module (e.g., `src/protocols/src/validation.rs` or `src/session/src/lib.rs`) and use it from both handlers.

#### DESIGN-002: `SeekRequest` inconsistency between HTTP and WebSocket
- **Severity:** Low
- **Location:** `http.rs` `SeekRequest` (lines 57–60) vs `ws.rs` `ClientCommand::Seek` (line 79)
- **Description:** The HTTP API accepts both `position_ms` and `position_seconds` for seek. The WebSocket API only accepts `position_ms`. A client that uses both interfaces must handle two different seek interfaces.
- **Impact:** Minor inconsistency; clients using both interfaces need different code paths.
- **Recommendation:** Either add `position_seconds` to the WS `Seek` command, or document that WS is the low-latency path (milliseconds only) while HTTP is the human-friendly path (both units).

#### DESIGN-003: `map_session_event` queries `current_status()` on every state event
- **Severity:** Low
- **Location:** Lines 377–381 (event handling)
- **Description:** For every `Playing`, `Paused`, `Stopped`, `VolumeChanged`, `Seeking`, or `PositionUpdate` event, the handler calls `session.current_status().await` to get a fresh snapshot. This is an async call per event per connected client. With 50 clients and frequent `PositionUpdate` events (e.g., every second), this is 50 async calls per second.
- **Impact:** Potential performance concern at high client counts with frequent position updates.
- **Recommendation:** Consider including the relevant fields (position, volume, source, title) directly in the `SessionEvent` enum variants, so the snapshot query is unnecessary. This is a session-layer change but would eliminate the per-event query.

#### DESIGN-004: `SessionEvent::Created/Resolving/Resolved` all map to `ResolveProgress { percent: 0 }`
- **Severity:** Low
- **Location:** Lines 535–538 (inside `map_session_event`, which starts at line 476 — the `ServerEvent::MediaStatus` arm)
- **Description:** Three different session lifecycle events (`Created`, `Resolving`, `Resolved`) all map to the same `ResolveProgress { percent: 0 }` event. The client cannot distinguish between "resolution starting" and "resolution complete."
- **Impact:** Clients can't show accurate resolution progress; they only know resolution is "in progress" with 0%.
- **Recommendation:** Map `Created` → `ResolveProgress { percent: 0 }`, `Resolving` → `ResolveProgress { percent: 10 }` (or similar), `Resolved` → `ResolveProgress { percent: 100 }`. Or add a `state` field to `ResolveProgress` to distinguish phases.

### Missing Tests

#### TEST-001: No integration tests for WebSocket server
- **Severity:** Medium
- **Description:** All tests are unit tests for serialization and event mapping. There are no tests that start the WebSocket server, connect a client, and verify event flow.
- **Impact:** Connection handling, ping/pong, connection limits, and event forwarding are untested.
- **Recommendation:** Add integration tests using `tokio_tungstenite::connect_async` against a test server instance. Test at minimum: connect and receive `CONNECTED` event, send `STOP` command and verify it's processed, verify connection limit rejection, verify ping/pong.

#### TEST-002: No test for URL validation in `handle_command`
- **Severity:** Low
- **Description:** The URL validation in `handle_command` (Cast arm) is not tested. It should reject `file://`, `data:`, and unknown schemes.
- **Impact:** URL validation could regress without detection.
- **Recommendation:** Add tests that call `handle_command` with various URLs and verify acceptance/rejection. This requires a mock `SessionManager`.

#### TEST-003: No test for connection limit rejection
- **Severity:** Low
- **Description:** The connection limit rejection path (lines 195–215) is not tested.
- **Impact:** The rejection behavior (send error, close) could regress.
- **Recommendation:** Add a test that creates a server with `with_max_connections(1)`, connects 2 clients, and verifies the second receives an error.

#### TEST-004: No test for binary message handling
- **Severity:** Low
- **Description:** The binary message parsing path (lines 366–375) is not tested.
- **Recommendation:** Add a test that sends a valid JSON command as a binary frame and verifies it's processed.

## Positive Observations

1. **Connection limiting is well-designed** — the `Semaphore`-based approach correctly holds the permit for the connection's lifetime via the `_permit` parameter, ensuring slots are released on disconnect.
2. **Dual ping/pong mechanism** — both WS-level pings (for protocol compliance) and application-level PING/PONG (for clients that can't send WS pings, like some browser extensions) is a thoughtful design.
3. **Message size limits** — `WS_MAX_MESSAGE_SIZE` and `WS_MAX_FRAME_SIZE` (both 1 MB) prevent memory exhaustion from large messages.
4. **TLS support is optional but clean** — the `WsStream` enum abstracts over plain and TLS connections without complicating the handler logic.
5. **Event mapping is comprehensive** — all `SessionEvent` variants are mapped to `ServerEvent` with appropriate field population, and the fallback behavior (no active session) is handled gracefully.
6. **Serialization tests are thorough** — 18 unit tests cover all `ClientCommand` and `ServerEvent` variants, plus `map_session_event` for major event types.
7. **Error events on command failure** — when a command fails, the error is sent back to the client as an `ERROR` event rather than silently dropping it, which is good UX for real-time clients.
8. **Broadcast channel lag handling** — the `Lagged` error is caught and logged rather than crashing the connection, which is the right behavior.

## Recommendations Summary

| Priority | Finding | Effort |
|----------|---------|--------|
| Medium | BUG-002: Reject connections before handshake at capacity | M (2–4 h) |
| Medium | BUG-004: Implement pong timeout (documented but missing) | M (2–3 h) |
| Medium | SEC-002: Add Origin header check for v2 | M (2–4 h) |
| Medium | DESIGN-001: Extract shared URL validation | S (1 h) |
| Medium | TEST-001: Add WebSocket integration tests | L (4–8 h) |
| Low | BUG-001: Fix MAX_CONNECTIONS doc inconsistency | S (5 min) |
| Low | BUG-003: Remove or document dead Ping arm | S (15 min) |
| Low | BUG-005: Send resync event after broadcast lag | S (1 h) |
| Low | SEC-001: Add explicit `javascript` scheme rejection | S (15 min) |
| Low | DESIGN-002: Reconcile SeekRequest between HTTP and WS | S (30 min) |
| Low | DESIGN-003: Include fields in SessionEvent to avoid per-event query | M (2–4 h, session-layer change) |
| Low | DESIGN-004: Distinguish Created/Resolving/Resolved in events | S (30 min) |
| Low | TEST-002: Add URL validation tests for WS | S (1 h) |
| Low | TEST-003: Add connection limit rejection test | S (1 h) |
| Low | TEST-004: Add binary message handling test | S (30 min) |
