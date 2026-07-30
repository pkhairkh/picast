---
doc: code_review
project: picast
version: 1
phase: code_review
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# Code Review: `src/playback/src/socks_forwarder.rs`

**File:** `src/playback/src/socks_forwarder.rs`
**Lines:** 557
**Reviewer:** agent
**Date:** 2026-07-30

## Summary

The SOCKS forwarder is a local HTTP CONNECT → SOCKS5 proxy that bridges `souphttpsrc` (which cannot use SOCKS5 URIs directly) to Tor's SOCKS5 port with per-session circuit isolation. This is a security-critical component: it ensures the media fetcher uses the *same* Tor circuit (and thus the same exit IP) as the resolver, preventing CDN IP-bound token mismatches that cause 403 Forbidden errors. The implementation is excellent — the comments thoroughly document the auth method selection invariant, the 256 KB buffering rationale, and the SOCKS5h DNS-leak prevention. This is one of the best-documented modules in the codebase. However, there are a few issues worth addressing.

## Key Components Reviewed

| Component | Lines | Purpose |
|-----------|-------|---------|
| `SocksForwarder` struct | 35–50 | Local HTTP CONNECT proxy with shutdown signaling |
| `start()` | 55–110 | Bind to random port, spawn accept loop |
| `check_exit_ip()` | 115–175 | Diagnostic: verify Tor exit IP via ipify |
| `handle_connect()` | 195–310 | Parse CONNECT, establish SOCKS5 tunnel, bidirectional copy |
| `socks5_connect()` | 315–470 | Manual SOCKS5 handshake with username/password auth only |
| `parse_host_port()` | 475–500 | Parse `host:port` and `[IPv6]:port` |
| `socks5_reply_name()` | 505–520 | Human-readable SOCKS5 error codes |

## Findings

### Security

#### SEC-001: `check_exit_ip()` connects to `api.ipify.org` — creates a traffic fingerprint
- **Severity:** Low
- **Location:** Lines 115–175 (`check_exit_ip`)
- **Description:** The `check_exit_ip()` diagnostic method connects through Tor to `api.ipify.org:80` (plain HTTP) to retrieve the exit IP. This creates a recognizable traffic pattern: the Tor exit relay sees a connection to `api.ipify.org`. While this is a diagnostic method (not called during normal playback), if it's called frequently, it could fingerprint boGDan appliances.
- **Impact:** Minimal — the method is diagnostic-only and not on the hot path. But it uses plain HTTP (`:80`), which means the exit relay can see the full request and response (the IP address).
- **Recommendation:** Use HTTPS (`api.ipify.org:443`) instead of HTTP. Consider making the diagnostic opt-in or removing it from production builds. Document that calling it creates observable traffic.

#### SEC-002: No authentication on the local HTTP CONNECT proxy
- **Severity:** Low
- **Location:** Lines 55–110 (`start`)
- **Description:** The forwarder listens on `127.0.0.1:0` (random port) with no authentication. Any local process can connect and use it to tunnel through Tor. On a single-user appliance, this is acceptable, but on a multi-user system, a malicious local process could use the forwarder to anonymize its traffic through boGDan's Tor circuit.
- **Impact:** Low on the appliance model (single `bogdan` user), but worth noting for defense-in-depth.
- **Recommendation:** Acceptable for v1. For v2, consider binding to a Unix domain socket with file permissions, or adding a random shared-secret token that `souphttpsrc` must include in the CONNECT request.

#### SEC-003: Username length not checked before `as u8` cast
- **Severity:** Low
- **Location:** Line 375 (`username_bytes.len() as u8`)
- **Description:** The username length is checked (`if username_bytes.len() > 255`) before the `as u8` cast, which is correct. However, the hostname length check (line 400) has the same pattern. Both are correct, but the pattern is worth verifying — a `> 255` check before `as u8` is the right approach.
- **Impact:** None — the check is present and correct.
- **Recommendation:** No action needed. The code is correct. Noting for completeness.

### Bugs

#### BUG-001: `handle_connect` reads CONNECT request with a fixed 4096-byte buffer
- **Severity:** Low
- **Location:** Lines 200–220 (`handle_connect`)
- **Description:** The CONNECT request is read into a 4096-byte buffer. If the request (including headers) exceeds 4096 bytes, the function returns "CONNECT request too large." While legitimate CONNECT requests are small (one line + a few headers), a malicious or buggy client could send a large request with many headers.
- **Impact:** Minimal — `souphttpsrc` sends minimal CONNECT requests. But the error message could be more helpful.
- **Recommendation:** Increase the buffer to 8192 or 16384 bytes (standard HTTP request sizes). Or implement streaming parsing that doesn't have a fixed limit. Document the expected maximum request size.

#### BUG-002: Bidirectional copy tasks are awaited sequentially, not concurrently
- **Severity:** Low
- **Location:** Lines 330–345 (`handle_connect`)
- **Description:** The client→remote and remote→client copy tasks are spawned, then `client_to_remote.await` is called, followed by `remote_to_client.await`. If the client→remote direction finishes first (e.g., client sends a short request then half-closes), the code waits for `remote_to_client` to also finish. This is correct behavior (wait for both directions), but the two `await`s are sequential — if the first panics, the second is never awaited, potentially leaking a task.
- **Impact:** Minimal — `tokio::spawn` tasks don't leak even if not awaited (they run to completion or are aborted on runtime shutdown). But the sequential await means a panic in one direction doesn't clean up the other.
- **Recommendation:** Use `tokio::join!(client_to_remote, remote_to_client)` for concurrent awaiting with proper cleanup. Or use `tokio::select!` to abort the other direction when one finishes.

#### BUG-003: `parse_host_port` returns `[::1]` with brackets for IPv6
- **Severity:** Low
- **Location:** Lines 475–495 (`parse_host_port`)
- **Description:** For IPv6 targets like `[::1]:443`, the function returns `("[::1]", 443)` — the host includes the brackets. When this host is passed to the SOCKS5 CONNECT request (line 410: `host.as_bytes()`), the brackets are included in the domain name sent to Tor. Tor may not recognize `[::1]` as a valid hostname.
- **Impact:** IPv6 CONNECT targets may fail. In practice, CDNs rarely use IPv6 literals in CONNECT requests (they use domain names), so this is unlikely to be hit. But it's a latent bug.
- **Recommendation:** Strip the brackets from the IPv6 host before returning:
  ```rust
  let host = &target[1..close]; // strip [ and ]
  ```

### Design Issues

#### DESIGN-001: Only one session at a time, but the proxy accepts multiple connections
- **Severity:** Low
- **Location:** Lines 55–110 (`start`)
- **Description:** The doc comment says "handles one session at a time," but the accept loop spawns a new task for each incoming connection (`tokio::spawn(async move { handle_connect(...) })`). Multiple concurrent connections would share the same `isolation_username`, all using the same Tor circuit. This is actually correct behavior (they should share the circuit), but the doc comment is misleading.
- **Impact:** No functional issue, but the documentation is inaccurate.
- **Recommendation:** Update the doc comment to say "handles multiple connections, all sharing the same isolation username (and thus the same Tor circuit)."

#### DESIGN-002: No connection timeout on the forwarder
- **Severity:** Low
- **Location**: `handle_connect` (lines 195–345)
- **Description:** Once a CONNECT tunnel is established, the bidirectional copy runs indefinitely until one side closes the connection. If `souphttpsrc` hangs (e.g., due to a GStreamer pipeline stall), the forwarder holds the Tor circuit open indefinitely.
- **Impact:** A stalled playback session holds a Tor circuit, preventing circuit rotation and consuming a circuit slot.
- **Recommendation:** Add an idle timeout (e.g., 60 seconds with no data transferred) that closes the tunnel and releases the circuit. Use `tokio::time::timeout` around the `io::copy` calls.

#### DESIGN-003: Logging at `info` level for every CONNECT is noisy
- **Severity:** Low
- **Location:** Lines 260, 285, 430
- **Description:** The forwarder logs at `info` level for every CONNECT request, every tunnel establishment, and every SOCKS5 authentication. During playback, this generates multiple log lines per second (one per CDN segment fetch).
- **Impact:** The journald log fills with forwarder messages, making it hard to find actual errors.
- **Recommendation:** Change the per-CONNECT logs to `debug` level. Keep `info` for the initial forwarder startup and shutdown. The SOCKS5 auth success log (line 430) could stay at `info` for the first connection, then drop to `debug` for subsequent ones.

### Missing Tests

#### TEST-001: No integration test for the forwarder
- **Severity:** Medium
- **Description:** There are only 2 unit tests (`test_parse_host_port` and `test_socks5_reply_name`), both for pure helper functions. The core `start()`, `handle_connect()`, and `socks5_connect()` functions have no tests.
- **Impact:** The SOCKS5 handshake, CONNECT parsing, and bidirectional tunneling are untested. A regression in the auth method selection (the critical invariant) would not be caught.
- **Recommendation:** Add integration tests with a mock SOCKS5 server (TCP server that speaks SOCKS5) and a mock HTTP CONNECT client. Verify: the forwarder only offers username/password auth (0x02), the correct isolation username is sent, and data tunnels correctly in both directions.

#### TEST-002: No test for the auth method invariant
- **Severity:** Medium
- **Description:** The critical invariant — "only offer username/password auth (0x02), never no-auth (0x00)" — is documented in comments but not tested. A future change that accidentally adds 0x00 to the greeting would break the CDN IP-matching property.
- **Impact:** The most important security property of this module is untested.
- **Recommendation:** Add a test that captures the SOCKS5 greeting bytes sent by `socks5_connect` and asserts the method list is exactly `[0x02]` (not `[0x00, 0x02]`). This requires a mock SOCKS5 server that records the greeting.

#### TEST-003: No test for `check_exit_ip`
- **Severity:** Low
- **Description:** The `check_exit_ip` diagnostic method is untested.
- **Recommendation:** Add a test with a mock SOCKS5 server and a mock HTTP server that returns an IP address. Verify the parsing logic.

## Positive Observations

1. **Excellent documentation of the auth method invariant** — the comment block at lines 325–345 explaining why only username/password auth (0x02) is offered is one of the best security-critical comments in the codebase. It explains the failure mode (Tor choosing no-auth → wrong circuit → CDN 403), the root cause, and the fix.
2. **SOCKS5h (not SOCKS5)** — the CONNECT request uses `ATYP=3 (DOMAINNAME)`, sending the hostname to Tor for resolution. This prevents DNS leaks. Correct.
3. **256 KB buffering rationale** — the comment at lines 290–310 explains why 256 KB BufReader/BufWriter buffers are used instead of the default 8 KB `io::copy` buffer. The reasoning (reducing syscall frequency by 32×, matching browser chunk sizes) is thorough and correct.
4. **`Drop` impl calls `shutdown()`** — the forwarder cleans up its listener when dropped, preventing orphaned listeners.
5. **`oneshot` channel for shutdown** — clean shutdown signaling without blocking.
6. **IPv6 address parsing** — `parse_host_port` handles `[IPv6]:port` format (though with the bracket-stripping bug noted above).
7. **Human-readable SOCKS5 error codes** — `socks5_reply_name` maps numeric reply codes to descriptive strings, improving error messages.
8. **TCP_NODELAY on remote** — disables Nagle's algorithm to reduce latency on the Tor tunnel, with a clear comment explaining why.
9. **`shutdown()` on write halves** — after `io::copy` completes, the write half is shut down, signaling EOF to the peer. This is correct TCP hygiene.

## Recommendations Summary

| Priority | Finding | Effort |
|----------|---------|--------|
| Medium | TEST-001: Add integration tests for the forwarder | L (4–8 h) |
| Medium | TEST-002: Test the auth method invariant (only 0x02) | M (2–3 h) |
| Low | SEC-001: Use HTTPS for check_exit_ip or make opt-in | S (30 min) |
| Low | SEC-002: Document local proxy auth assumption | S (15 min) |
| Low | BUG-001: Increase CONNECT buffer or document limit | S (15 min) |
| Low | BUG-002: Use `tokio::join!` for concurrent copy cleanup | S (30 min) |
| Low | BUG-003: Strip IPv6 brackets in parse_host_port | S (15 min) |
| Low | DESIGN-001: Fix misleading "one session" doc comment | S (5 min) |
| Low | DESIGN-002: Add idle timeout to tunnel copy | S (1 h) |
| Low | DESIGN-003: Reduce per-CONNECT log level to debug | S (30 min) |
| Low | TEST-003: Add check_exit_ip test | S (1 h) |
