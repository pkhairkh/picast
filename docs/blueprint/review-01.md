---
doc: code_review
project: picast
version: 1
phase: code_review
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# Code Review: `src/tor/src/lib.rs`

**File:** `src/tor/src/lib.rs`
**Lines:** 1334
**Reviewer:** agent
**Date:** 2026-07-30

## Summary

The Tor manager handles the Tor daemon lifecycle (start, monitor, restart, shut down), provides the SOCKS5 proxy configuration to other subsystems, computes per-hostname stream-isolation identifiers for `IsolateSOCKSAuth`, and exposes control-port operations (`NEWNYM`, circuit-status queries). This is the security-critical core of boGDan's privacy guarantee. The implementation is generally strong — correct use of `socks5h://`, SHA-256 hostname hashing for circuit isolation, cookie-based control-port auth, and graceful SIGTERM shutdown. However, there are several security concerns, bugs, and design issues that should be addressed.

## Key Components Reviewed

| Component | Lines | Purpose |
|-----------|-------|---------|
| `stream_isolation_id()` | 30–36 | SHA-256 hostname → `bogdan-<16hex>` username |
| `SocksProxy` | 80–120 | Proxy config + `proxy_url_for()` builder |
| `CircuitHealth` | 125–145 | Circuit count + latency metrics |
| `TorManager` | 150–500 | Daemon lifecycle: `ensure_running`, `start_monitor`, `shutdown` |
| `proxied_reqwest_client()` | 460–490 | Build `reqwest::Client` with per-host SOCKS5h isolation |
| `socks5_handshake()` | 500–610 | Manual SOCKS5 handshake for health check |
| `health_check()` | 615–665 | TCP + SOCKS5 + HTTP health validation |
| `new_circuit()` | 670–740 | Control-port `SIGNAL NEWNYM` |
| `query_circuit_health()` | 920–1050 | Control-port `GETINFO circuit-status` parser |
| `Drop` impl | 1060–1090 | Best-effort synchronous kill |

## Findings

### Security

#### SEC-001: `proxy_url_for()` omits password, but `proxied_reqwest_client()` uses empty password
- **Severity:** Medium
- **Location:** Lines 113–116 (`proxy_url_for`) vs lines 463–469 (`proxied_reqwest_client`)
- **Description:** `proxy_url_for()` builds `socks5h://bogdan-<hash>@127.0.0.1:9050/` (no password). `proxied_reqwest_client()` builds `socks5h://bogdan-<hash>:@127.0.0.1:9050` (empty password). Tor's `IsolateSOCKSAuth` uses only the username for isolation, so both work — but the inconsistency could confuse developers and might break a future reqwest version that requires a non-empty password when a username is present.
- **Impact:** No current vulnerability, but inconsistent URL construction between the two methods.
- **Recommendation:** Standardize on one format. The spec (§6.2 of SPECIFICATION.md) documents the password as always `x` (a placeholder). Use `:x@` consistently, or omit both username and password when not needed. Align with the spec.

#### SEC-002: Control port cookie path hardcoded to `/run/tor/control.authcookie`
- **Severity:** Low
- **Location:** Line 168 (`TorManager::new`)
- **Description:** The default cookie path is `/run/tor/control.authcookie`. On Debian/Raspberry Pi OS, the Tor cookie is typically at `/var/run/tor/control.authcookie` or `/run/tor/control.authcookie`. The `with_cookie_path()` builder exists for override, but the default may not work on all distributions.
- **Impact:** On systems where the cookie is elsewhere, `new_circuit()` and `query_circuit_health()` will fail silently (the monitor task catches the error and continues with default health).
- **Recommendation:** Try multiple candidate paths (`/run/tor/control.authcookie`, `/var/run/tor/control.authcookie`) in `query_circuit_health` if the primary path fails. Or read the cookie path from `torrc` at startup.

#### SEC-003: `socks5_handshake()` connects to `check.tor-project.org` on every health check
- **Severity:** Medium
- **Location:** Lines 570–580 (`socks5_handshake` CONNECT to `check.tor-project.org:443`)
- **Description:** Every `socks5_handshake()` call (invoked by `health_check()`) opens a Tor circuit to `check.tor-project.org:443`. This creates a recognizable traffic pattern: the Tor exit relay sees a connection to `check.tor-project.org` every time boGDan does a health check. An adversary observing the exit could fingerprint boGDan appliances.
- **Impact:** Reduces privacy by creating a predictable traffic fingerprint. Also, `check.tor-project.org` may rate-limit or block frequent checks.
- **Recommendation:** Use a less identifiable target for the CONNECT test (e.g., a random low-traffic HTTPS site, or just test the SOCKS5 handshake without a CONNECT). Alternatively, rely on the control-port circuit-status query alone for health, which doesn't generate external traffic.

#### SEC-004: Tor stdout/stderr are discarded (`Stdio::null()`)
- **Severity:** Low
- **Location:** Lines 235–236 and 335–336 (Tor spawn)
- **Description:** When spawning Tor, both stdout and stderr are set to `Stdio::null()`. If Tor fails to bootstrap (e.g., port conflict, corrupt state, missing dependencies), the error message is lost. The only feedback is "SOCKS proxy timeout."
- **Impact:** Debugging Tor startup failures is difficult; the actual error is not logged.
- **Recommendation:** Pipe stderr to the tracing subsystem (at debug or trace level) so Tor's bootstrap log is available for debugging. At minimum, capture stderr to a log file in the runtime directory.

#### SEC-005: No validation that the spawned Tor is actually the expected binary
- **Severity:** Low
- **Location:** Lines 870–890 (`which_tor`)
- **Description:** `which_tor()` searches for `tor`, `/usr/bin/tor`, `/usr/local/bin/tor`, and falls back to `which tor`. It does not verify the binary's integrity (e.g., package signature, hash). If `PATH` is compromised and a malicious `tor` is found first, boGDan would spawn it.
- **Impact:** Low on a properly configured Pi (Tor is installed via apt and `/usr/bin/tor` is root-owned), but worth noting for defense-in-depth.
- **Recommendation:** Prefer `/usr/bin/tor` over `tor` from `PATH` (the current order does this correctly for the first two candidates, but the `which` fallback could pick up a malicious binary). Document that the appliance should not have a writable `PATH` entry before `/usr/bin`.

### Bugs

#### BUG-001: `parse_addr` splits on `:` which breaks IPv6 addresses
- **Severity:** Low
- **Location:** Lines 850–860 (`parse_addr`)
- **Description:** `parse_addr` splits on `:` and expects exactly 2 parts. An IPv6 address like `[::1]:9050` would split into more than 2 parts, falling through to the default `127.0.0.1:9050`.
- **Impact:** IPv6 SOCKS addresses are not supported. On a LAN-only Pi using IPv4, this is unlikely to matter, but it's a latent bug.
- **Recommendation:** Use `std::net::ToSocketAddrs` or `hyper::Uri` parsing instead of manual splitting. Or at minimum, handle the `[ipv6]:port` format explicitly.

#### BUG-002: `health_check()` TCP connect then immediate drop is racy
- **Severity:** Low
- **Location:** Lines 625–630 (`health_check` step 1)
- **Description:** `health_check()` opens a TCP connection to the SOCKS port, measures the connect time, then `drop(stream)`s it immediately. This creates a connect-then-disconnect pattern that some firewalls flag as suspicious. It also doesn't verify the port is actually a SOCKS proxy (that's step 2, `socks5_handshake`).
- **Impact:** Minimal — step 2 does the real check. But the redundant TCP connect adds latency to the health check.
- **Recommendation:** Remove step 1 (the bare TCP connect) and rely on `socks5_handshake()` which already does a TCP connect as its first operation.

#### BUG-003: `Drop` impl uses `try_lock` which may fail silently
- **Severity:** Medium
- **Location:** Lines 1060–1090 (`Drop for TorManager`)
- **Description:** The `Drop` implementation uses `self.child.try_lock()` to attempt synchronous cleanup. If the lock is held (e.g., the monitor task is mid-restart), the `Err` branch logs a warning and the Tor process is orphaned. The warning says "Use shutdown() for clean termination" but `shutdown()` is async and can't be called from `drop`.
- **Impact:** If `TorManager` is dropped without calling `shutdown()` first (e.g., due to a panic), and the monitor task holds the lock, the Tor child process is orphaned and continues running.
- **Recommendation:** Use `std::sync::Mutex` instead of `tokio::sync::Mutex` for the `child` field (it doesn't need to be held across `.await` points in most cases), or use `parking_lot::Mutex` which has a non-poisoning `try_lock`. Alternatively, track the PID separately so `Drop` can kill by PID without needing the lock.

#### BUG-004: `query_circuit_health` counts `CLOSED` circuits as `failed`
- **Severity:** Low
- **Location:** Lines 1010–1015 (circuit state parsing)
- **Description:** The parser maps `CLOSED` circuits to `failed_circuits`. A `CLOSED` circuit is not necessarily failed — it could have been closed normally after `MaxCircuitDirtiness` expired. Counting all closed circuits as failed inflates the failure count and could trigger false health warnings.
- **Impact:** `is_healthy` may report `false` (because `built == 0` after all circuits have normally closed) even though Tor is functioning correctly.
- **Recommendation:** Distinguish `CLOSED` from `FAILED`. Track `closed_circuits` separately, or only count `FAILED` as failures. The `is_healthy` check should be `built > 0 || open > 0` (Tor has at least some circuits).

#### BUG-005: `is_healthy` in `query_circuit_health` doesn't consider latency
- **Severity:** Low
- **Location:** Lines 1025–1030 (return from `query_circuit_health`)
- **Description:** `query_circuit_health` returns `CircuitHealth { is_healthy: built > 0, ... }` — it only checks if any circuits are built. The `health_check()` method (lines 655–660) computes `is_healthy` using both connect time and latency. The two paths produce different `is_healthy` values for the same state.
- **Impact:** `last_circuit_health()` (from the monitor) and `health_check()` (on-demand) may disagree on health status.
- **Recommendation:** Unify the health criteria. Either both should use `built > 0`, or both should use latency + circuit count. Document which is authoritative.

### Design Issues

#### DESIGN-001: `NEWNYM` rotates all circuits, not per-site
- **Severity:** Medium
- **Location:** Lines 670–740 (`new_circuit`)
- **Description:** The `new_circuit()` method sends `SIGNAL NEWNYM` to the Tor control port, which rotates *all* circuits. This contradicts the per-site isolation design (ADR-010, BP-ADR-005) where each site has its own circuit. If a user casts a YouTube video and the circuit degrades, calling `new_circuit()` would also rotate the Vimeo circuit, disrupting any concurrent or future Vimeo playback.
- **Impact:** `NEWNYM` is a blunt instrument that breaks the per-site isolation model. The spec (BP-ADR-005) recommends re-resolution with a session-counter suffix on the SOCKS username to force a new circuit for a specific site only.
- **Recommendation:** Deprecate `new_circuit()` (or mark it "use only for debugging"). The per-site circuit rotation should be done by appending a counter to the SOCKS username (e.g., `bogdan-<hash>-2`) which forces Tor to build a new circuit for that site without affecting others.

#### DESIGN-002: `start_monitor` spawns a task with no handle for cancellation beyond `Notify`
- **Severity:** Low
- **Location:** Lines 290–400 (`start_monitor`)
- **Description:** `start_monitor()` spawns a background task that runs until `monitor_shutdown.notified()` fires. The `Notify` is triggered in `shutdown()`. However, if `shutdown()` is not called (e.g., the process is killed), the task continues running. There's no `JoinHandle` to await the task's completion.
- **Impact:** On ungraceful shutdown, the monitor task may briefly continue running and attempt to restart Tor.
- **Recommendation:** Return a `JoinHandle` from `start_monitor()` so callers can await task completion. Or use a `CancellationToken` from `tokio_util` which is more ergonomic than `Notify` for this pattern.

#### DESIGN-003: `socks5_handshake` uses `b"bogcast-health"` as the SOCKS5 username
- **Severity:** Low
- **Location:** Line 545 (health check auth)
- **Description:** The SOCKS5 handshake for health checks uses username `bogcast-health"`. This creates a separate Tor circuit (via `IsolateSOCKSAuth`) just for health checks. While this is intentional (don't pollute site circuits), it means every health check builds a new circuit, adding 5–15 seconds of latency.
- **Impact:** Health checks are slow and create extra Tor circuits.
- **Recommendation:** Use the `No Auth` method (0x00) for health checks instead of `Username/Password` (0x02). The greeting already offers both methods; just prefer No Auth if the server selects it. This avoids creating an isolation circuit for health checks.

#### DESIGN-004: Duplicate test coverage
- **Severity:** Low
- **Location:** Lines 1095–1334 (tests)
- **Description:** The test module has both a "new tests" section and a "Legacy tests (preserved)" section that test the same things (e.g., `test_socks_proxy_default` and `socks_proxy_default` both test `SocksProxy::default()`). This is harmless but inflates the test count without adding coverage.
- **Impact:** Maintenance burden; changes require updating two tests for the same logic.
- **Recommendation:** Remove the legacy tests or merge them with the new tests.

### Missing Tests

#### TEST-001: No test for `ensure_running` with a real or mock Tor
- **Severity:** Medium
- **Description:** `ensure_running()` is the critical startup path, but there's no test for it. The tests only cover `socks5_handshake` and `health_check` failing without Tor (which tests the error path, not the success path).
- **Impact:** A regression in `ensure_running` (e.g., wrong Tor arguments, missing `IsolateSOCKSAuth`) would not be caught by tests.
- **Recommendation:** Add integration tests in `src/tor/tests/integration_tor.rs` that spawn a real Tor instance (or a mock SOCKS5 server) and verify `ensure_running` succeeds and the SOCKS port becomes reachable.

#### TEST-002: No test for `new_circuit` (NEWNYM)
- **Severity:** Low
- **Description:** The control-port `NEWNYM` signaling is not tested.
- **Impact:** A bug in the control-port protocol handling would not be caught.
- **Recommendation:** Add a test with a mock control port (TCP server that speaks the Tor control protocol) and verify `new_circuit` sends the correct commands.

#### TEST-003: No test for `query_circuit_health` parsing
- **Severity:** Low
- **Description:** The circuit-status parser (lines 960–1020) is not tested. It parses multi-line Tor control-port output and counts circuits by state.
- **Impact:** A parsing bug (e.g., miscounting `CLOSED` vs `FAILED`) would not be caught.
- **Recommendation:** Add tests with sample circuit-status output and verify the counts. This can be done by extracting the parser into a pure function and testing it directly.

#### TEST-004: No test for `Drop` behavior
- **Severity:** Low
- **Description:** The `Drop` implementation's orphan-prevention logic is not tested.
- **Recommendation:** Add a test that spawns a Tor process, drops the `TorManager` without calling `shutdown()`, and verifies the process is killed.

## Positive Observations

1. **Correct use of `socks5h://`** — the `h` suffix ensures DNS resolution happens through Tor, preventing DNS leaks. This is the most critical privacy property and it's correct.
2. **SHA-256 hostname hashing** — `stream_isolation_id` correctly uses SHA-256 (first 16 hex chars) for deterministic, collision-resistant per-site usernames. Tests verify determinism and distinctness.
3. **Cookie-based control-port auth** — uses `CookieAuthentication 1` rather than password auth, which is the recommended Tor security practice.
4. **Graceful shutdown** — SIGTERM first, 5-second wait, then SIGKILL. The `Drop` impl also attempts cleanup as a safety net.
5. **Auto-restart** — the monitor task detects crashes and restarts Tor automatically, which is important for an always-on appliance.
6. **Comprehensive error types** — `TorError` has specific variants for each failure mode (BinaryNotFound, ProcessExited, SocksTimeout, etc.) with good error messages.
7. **Timeout on all control-port operations** — `AUTHENTICATE`, `SIGNAL NEWNYM`, and `GETINFO` all have 5-second timeouts, preventing indefinite hangs.
8. **`SafeLogging` not needed in code** — the cookie is read but never logged; the `tracing::debug!` calls log the isolation username (which is a hash, not sensitive) but never the cookie itself.
9. **Builder pattern** — `with_control_port`, `with_cookie_path`, `with_auto_restart` make configuration clean and testable.
10. **29 unit tests** — good coverage of the pure functions (isolation ID, URL parsing, address parsing, builder methods, error variants).

## Recommendations Summary

| Priority | Finding | Effort |
|----------|---------|--------|
| Medium | SEC-001: Standardize SOCKS5 URL password format | S (30 min) |
| Medium | SEC-003: Don't use check.tor-project.org for health checks | M (2–3 h) |
| Medium | BUG-003: Fix Drop orphan risk with std::sync::Mutex or PID tracking | M (2–4 h) |
| Medium | DESIGN-001: Deprecate NEWNYM in favor of per-site circuit rotation | M (3–4 h) |
| Medium | TEST-001: Add integration tests for ensure_running | L (4–8 h) |
| Low | SEC-002: Try multiple cookie paths | S (1 h) |
| Low | SEC-004: Capture Tor stderr for debugging | S (1 h) |
| Low | SEC-005: Prefer /usr/bin/tor over PATH | S (30 min) |
| Low | BUG-001: Handle IPv6 addresses in parse_addr | S (1 h) |
| Low | BUG-002: Remove redundant TCP connect in health_check | S (15 min) |
| Low | BUG-004: Don't count CLOSED circuits as failed | S (30 min) |
| Low | BUG-005: Unify is_healthy criteria | S (1 h) |
| Low | DESIGN-002: Return JoinHandle from start_monitor | S (30 min) |
| Low | DESIGN-003: Use No Auth for health check SOCKS5 | S (1 h) |
| Low | DESIGN-004: Remove duplicate legacy tests | S (30 min) |
| Low | TEST-002: Add NEWNYM test with mock control port | M (2–3 h) |
| Low | TEST-003: Add circuit-status parser tests | S (1–2 h) |
| Low | TEST-004: Add Drop behavior test | S (1 h) |
