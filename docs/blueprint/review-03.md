---
doc: code_review
project: picast
version: 1
phase: code_review
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# Code Review: `src/protocols/src/http.rs`

**File:** `src/protocols/src/http.rs`
**Lines:** 1083
**Reviewer:** agent
**Date:** 2026-07-30

## Summary

The HTTP REST API server provides a `hyper`-based control surface for the boGDan media appliance. It exposes 11 endpoints (cast, stop, pause, resume, seek, volume, status, health, audio-devices, audio-device GET/POST) with per-IP rate limiting, CORS support, and optional TLS. The code is generally well-structured with clear separation of concerns, good URL validation, and reasonable error responses. However, there are several bugs, security concerns, and missing tests that should be addressed before v1 release.

## Endpoints Reviewed

| Method | Path | Status Code (success) | Status Code (error) | Notes |
|--------|------|----------------------|---------------------|-------|
| POST | `/api/cast` | 202 Accepted | 400/409/413 | Async background task; returns placeholder session ID |
| POST | `/api/stop` | 200 OK | 409 | Error code mapping questionable |
| POST | `/api/pause` | 200 OK | 409 | Same CONFLICT mapping issue |
| POST | `/api/resume` | 200 OK | 409 | Same CONFLICT mapping issue |
| POST | `/api/seek` | 200 OK | 400/409 | Uses `?` operator — inconsistent error handling |
| POST | `/api/volume` | 200 OK | 409/500 | Best error handling pattern in the file |
| GET | `/api/status` | 200 OK | 200 (fallback) | Never returns error — falls back to idle |
| GET | `/api/health` | 200 OK | — | Excluded from rate limiting |
| GET | `/api/audio-devices` | 200 OK | — | Synchronous blocking I/O in async context |
| POST | `/api/audio-device` | 200 OK | 500 | No `sink_type` validation |
| GET | `/api/audio-device` | 200 OK | 500 | — |

## Findings

### Bugs

#### BUG-001: Session ID returned to client is a placeholder that doesn't match the actual session
- **Severity:** High
- **Location:** Lines 380–390 (`/api/cast` handler)
- **Description:** The `/api/cast` endpoint generates a `session_id` using `uuid::Uuid::new_v4()` and returns it in the `CastResponse` before `session.load(&url)` is called in a background task. The actual session ID is generated inside `load()` and is a different UUID. The client receives a session ID that doesn't correspond to any real session.
- **Impact:** Clients that use the returned `session_id` to poll `/api/status` or correlate WebSocket events will see a different session ID than what they received. This breaks the API contract.
- **Recommendation:** Either (a) generate the session ID before calling `load()` and pass it in, or (b) return a different field name (e.g., `request_id`) that doesn't imply it's the session ID, and document that the actual session ID is available via `/api/status`.

#### BUG-002: `/api/seek` with empty body silently seeks to position 0
- **Severity:** Medium
- **Location:** Lines 430–440 (`/api/seek` handler)
- **Description:** The `SeekRequest` struct has both `position_ms` and `position_seconds` as `Option`. If neither is provided, the code defaults to `unwrap_or(0)`, which seeks to the beginning of the video. A `POST /api/seek {}` request will restart playback from the start.
- **Impact:** Unexpected behavior — a malformed or empty seek request restarts the video instead of returning an error.
- **Recommendation:** Return a 400 error if neither `position_ms` nor `position_seconds` is provided:
  ```rust
  let position_ms = match (payload.position_ms, payload.position_seconds) {
      (Some(ms), _) => ms,
      (None, Some(s)) => (s * 1000.0) as u64,
      (None, None) => return error_response_with_code(
          StatusCode::BAD_REQUEST, ErrorCode::BadRequest,
          "either position_ms or position_seconds is required",
      ),
  };
  ```

#### BUG-003: Race condition between session check and `load()` in `/api/cast`
- **Severity:** Medium
- **Location:** Lines 350–390 (`/api/cast` handler)
- **Description:** The code checks `session.current_status()` to reject if a session is already active, then spawns a background task to call `session.load(&url)`. Between the status check and the `load()` call, another concurrent `/api/cast` request could pass the same check and both would proceed to `load()`. The comment acknowledges this but the mitigation is incomplete — the check and the load are not atomic.
- **Impact:** Two rapid cast requests could both start, leading to undefined behavior in the session manager.
- **Recommendation:** Use a `tokio::sync::Mutex` or `tokio::sync::Semaphore` to make the check-and-load atomic, or have `SessionManager::load()` itself reject if a session is already active (returning an error that the HTTP handler maps to 409).

#### BUG-004: `extract_client_ip` returns "unknown" for all non-proxied requests
- **Severity:** Medium
- **Location:** Lines 280–295 (`extract_client_ip`)
- **Description:** When there's no `X-Forwarded-For` header, the function returns the string `"unknown"`. This means all requests without the header share the same rate limit bucket. On a LAN-only device (boGDan's target deployment), no reverse proxy is expected, so no `X-Forwarded-For` header will be present, and all clients will share the `"unknown"` bucket.
- **Impact:** Rate limiting is effectively global rather than per-IP. A single noisy client can exhaust the rate limit for all clients on the LAN.
- **Recommendation:** Pass the remote TCP address (available from `TcpListener::accept`) through to `handle_request` and use it as the client IP when `X-Forwarded-For` is absent. The `remote` variable is already captured in `start()` but not passed to the service.

### Security

#### SEC-001: CORS allows any origin (`Access-Control-Allow-Origin: *`)
- **Severity:** Medium (acceptable for v1, must fix for v2)
- **Location:** Lines 530–535 (`json_response`), also in `cors_response` and `rate_limit_response`
- **Description:** All responses include `Access-Control-Allow-Origin: *`, allowing any website to make cross-origin requests to the API. On a LAN, any device or browser tab can control playback.
- **Impact:** A malicious webpage visited by a user on the same LAN as the boGDan appliance could silently cast URLs, change volume, or stop playback.
- **Recommendation:** For v1, document this as a known limitation. For v2, implement origin allowlisting (OD-004 in the spec) or require a shared secret/token.

#### SEC-002: `X-Forwarded-For` header trusted without a reverse proxy
- **Severity:** Medium
- **Location:** Lines 280–290 (`extract_client_ip`)
- **Description:** The `extract_client_ip` function trusts the `X-Forwarded-For` header unconditionally. An attacker can spoof this header to bypass rate limiting by sending a different IP in each request.
- **Impact:** Rate limiting is trivially bypassable by spoofing `X-Forwarded-For`.
- **Recommendation:** Only trust `X-Forwarded-For` when a known reverse proxy is in front (configurable via `bogdan.toml`). On a LAN-only device without a reverse proxy, use the TCP remote address directly.

#### SEC-003: No authentication on any endpoint
- **Severity:** Low (documented as v2, but worth noting)
- **Location:** Entire file
- **Description:** There is no API key, bearer token, or any form of authentication. Anyone who can reach port 8585 can control all playback functions.
- **Impact:** Any device on the LAN can control the appliance.
- **Recommendation:** Document as a known v1 limitation. Implement LAN authentication in v2 per OD-004.

#### SEC-004: `sink_type` not validated in `/api/audio-device`
- **Severity:** Low
- **Location:** Lines 460–480 (`/api/audio-device` handler)
- **Description:** The `AudioDeviceRequest.sink_type` field defaults to `"alsasink"` but is not validated against a whitelist. A client could set it to an arbitrary string like `"wrongtsink"` or a GStreamer element that doesn't exist.
- **Impact:** Setting an invalid sink type would cause audio playback to fail silently or with a confusing GStreamer error.
- **Recommendation:** Validate `sink_type` against `{"alsasink", "pulsesink"}` and return 400 for anything else.

### Response Code Issues

#### RESP-001: 409 CONFLICT used inconsistently for non-conflict errors
- **Severity:** Low
- **Location:** Lines 400–420 (stop, pause, resume handlers)
- **Description:** The `stop`, `pause`, and `resume` endpoints return 409 CONFLICT for any error from the session manager, including "no active session" (which is not a conflict — it's a not-found condition). The `error_response` function maps CONFLICT to `ErrorCode::SessionActive`, which is misleading when the actual error is `NoActiveSession`.
- **Impact:** Clients receive `SESSION_ACTIVE` error code when there is no active session, which is confusing.
- **Recommendation:** Map `NoActiveSession` to 404 NOT FOUND with `ErrorCode::NoActiveSession`, and reserve 409 CONFLICT for actual conflicts (session already active when casting).

#### RESP-002: Inconsistent body parse error handling
- **Severity:** Low
- **Location:** Lines 430, 450, 460 (seek, volume, audio-device handlers)
- **Description:** The `/api/cast` handler explicitly handles `read_body_json` errors, distinguishing body-too-large (413) from parse errors (400). The `/api/seek`, `/api/volume`, and `/api/audio-device` handlers use the `?` operator, which catches all errors as generic 400s.
- **Impact:** A body-too-large request to `/api/seek` returns 400 instead of 413. Clients can't distinguish between a malformed JSON body and an oversized body.
- **Recommendation:** Extract the error handling pattern from `/api/cast` into a helper function and use it consistently across all POST endpoints.

### Style and Design

#### STYLE-001: `list_alsa_devices` performs blocking I/O in async context
- **Severity:** Medium
- **Location:** Lines 660–870 (`list_alsa_devices`)
- **Description:** This function reads `/proc/asound/cards`, `/proc/asound/pcm`, and runs external commands (`pactl`, `bluetoothctl`, `dbus-send`) synchronously. It is called directly from the async `handle_request` function without `tokio::task::spawn_blocking`.
- **Impact:** Blocking I/O in an async context can stall the Tokio runtime, affecting all concurrent connections. The `pactl` and `bluetoothctl` commands can take 100–500 ms each.
- **Recommendation:** Wrap the call in `tokio::task::spawn_blocking`:
  ```rust
  (Method::GET, "/api/audio-devices") => {
      let devices = tokio::task::spawn_blocking(list_alsa_devices).await
          .map_err(|e| anyhow::anyhow!("task join error: {}", e))?;
      json_response(StatusCode::OK, &devices)
  }
  ```

#### STYLE-002: `ErrorCode::NoActiveSession` variant is dead code
- **Severity:** Low
- **Location:** Lines 115–130 (`ErrorCode` enum)
- **Description:** The `ErrorCode::NoActiveSession` variant is defined and marked `#[allow(dead_code)]`, but it is never used in any response. The volume handler matches on `bogdan_session::SessionError::NoActiveSession` but maps it to `ErrorCode::SessionActive` (via the `error_response` function's CONFLICT mapping).
- **Impact:** Dead code; the variant exists but is never serialized.
- **Recommendation:** Either use `ErrorCode::NoActiveSession` in the appropriate error paths (see RESP-001), or remove it.

#### STYLE-003: `CastRequest` missing `title` field from SPECIFICATION.md
- **Severity:** Low
- **Location:** Lines 45–48 (`CastRequest` struct)
- **Description:** The `CastRequest` struct only has a `url` field. The SPECIFICATION.md `POST /api/cast` request body includes `url`, `title`, `resumePosition`, and `torMode` fields. Only `url` is implemented.
- **Impact:** Clients that send `title` or other fields will have them silently ignored (serde ignores unknown fields by default). The OSD won't show a title even if the client provides one.
- **Recommendation:** Add the missing fields to `CastRequest` and pass them through to the session manager, or document that only `url` is supported in v1.

### Missing Tests

#### TEST-001: No integration tests for HTTP endpoints
- **Severity:** Medium
- **Description:** The test module (lines 940–1083) contains only unit tests for individual structs and functions. There are no tests that spin up the HTTP server and make requests against it.
- **Impact:** Routing logic, CORS handling, rate limiting integration, and endpoint behavior are untested.
- **Recommendation:** Add integration tests using `hyper::Request` against a test server instance. Test at minimum: each endpoint returns the expected status code, CORS headers are present, rate limiting triggers after N requests, and 404 for unknown paths.

#### TEST-002: `empty_body_rejected` test is a tautology
- **Severity:** Low
- **Location:** Lines 1020–1025
- **Description:** The `empty_body_rejected` test creates an empty byte slice and asserts it is empty. This is `assert!(b"".is_empty())` — always true, tests nothing.
- **Impact:** Gives false confidence that empty body rejection is tested.
- **Recommendation:** Replace with a test that calls `read_body_json::<CastRequest>` with an empty body and asserts it returns an error.

#### TEST-003: No test for volume clamping
- **Severity:** Low
- **Description:** The `clamped_volume()` method is not tested. Values above 100 should be clamped.
- **Impact:** Volume clamping behavior is untested.
- **Recommendation:** Add a test:
  ```rust
  #[test]
  fn volume_clamping() {
      let req = VolumeRequest { volume: 150 };
      assert_eq!(req.clamped_volume(), 100);
      let req = VolumeRequest { volume: 0 };
      assert_eq!(req.clamped_volume(), 0);
  }
  ```

#### TEST-004: No test for seek with missing position
- **Severity:** Low
- **Description:** There is no test for the BUG-002 scenario where neither `position_ms` nor `position_seconds` is provided.
- **Impact:** The bug (seeking to 0) is untested and could regress.
- **Recommendation:** Add a test once BUG-002 is fixed.

## Positive Observations

1. **URL validation is thorough** — `is_safe_cast_url` correctly rejects `file://`, `data:`, `javascript:`, and other dangerous schemes, with clear error messages.
2. **Rate limiter implementation is clean** — the `RateLimiter` struct is well-tested (3 unit tests covering within-limit, different-IP, and window-reset), uses `Instant` for monotonic time, and prunes expired entries.
3. **Error response format is well-designed** — the `ErrorResponse` struct includes both a human-readable `error` string and a machine-readable `code` field, making it easy for clients to handle errors programmatically.
4. **TLS support is optional but clean** — the `with_tls` builder pattern allows TLS to be enabled without complicating the non-TLS path.
5. **Connection error logging is well-calibrated** — "connection closed before message completed" is logged at debug level (common on client disconnect), while other errors are logged at warn level.
6. **Body size limit** — the 1 KB `MAX_BODY_SIZE` prevents memory exhaustion from large request bodies.
7. **Background task for cast** — returning 202 Accepted and resolving in the background is the right pattern for a long-running operation like URL resolution through Tor.

## Recommendations Summary

| Priority | Finding | Effort |
|----------|---------|--------|
| High | BUG-001: Fix session ID mismatch in /api/cast | S (1–2 h) |
| High | BUG-004: Pass TCP remote address for rate limiting | S (1–2 h) |
| Medium | BUG-002: Reject seek with missing position | S (30 min) |
| Medium | BUG-003: Make check-and-load atomic in /api/cast | M (2–4 h) |
| Medium | SEC-001: Document CORS `*` as v1 limitation | S (30 min) |
| Medium | SEC-002: Don't trust X-Forwarded-For without proxy | S (1 h) |
| Medium | STYLE-001: Wrap `list_alsa_devices` in `spawn_blocking` | S (30 min) |
| Medium | TEST-001: Add HTTP integration tests | L (4–8 h) |
| Low | RESP-001: Fix 409 CONFLICT mapping for NoActiveSession | S (1 h) |
| Low | RESP-002: Consistent body parse error handling | S (1 h) |
| Low | SEC-004: Validate `sink_type` in /api/audio-device | S (30 min) |
| Low | STYLE-002: Use or remove `ErrorCode::NoActiveSession` | S (15 min) |
| Low | STYLE-003: Add missing CastRequest fields or document | S (1 h) |
| Low | TEST-002: Fix tautological `empty_body_rejected` test | S (15 min) |
| Low | TEST-003: Add volume clamping test | S (15 min) |
| Low | TEST-004: Add seek missing-position test | S (15 min) |
