---
doc: code_review
project: picast
version: 1
phase: code_review
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# Code Review: `src/resolver/src/resolver_socks.rs`

**File:** `src/resolver/src/resolver_socks.rs`
**Lines:** 359
**Reviewer:** agent
**Date:** 2026-07-30

## Summary

The resolver SOCKS forwarder is a mirror of `playback::socks_forwarder`, used by the resolver's reqwest HTTP client. It exists because reqwest's built-in SOCKS5 support (via `tokio-socks`) offers both no-auth (0x00) and username/password (0x02) auth methods, which allows Tor to choose no-auth and skip the isolation username — causing the resolver to use a different Tor circuit than playback, leading to CDN 403 errors. This forwarder, like the playback one, only offers username/password auth (0x02), guaranteeing the same Tor circuit. The implementation is nearly identical to `socks_forwarder.rs` and shares the same strengths and issues.

## Key Components Reviewed

| Component | Lines | Purpose |
|-----------|-------|---------|
| `ResolverSocksForwarder` struct | 38–45 | Local HTTP CONNECT proxy |
| `start()` | 53–105 | Bind, spawn accept loop |
| `handle_connect()` | (inferred) | Parse CONNECT, SOCKS5 tunnel |
| `socks5_connect()` | (inferred) | Manual SOCKS5 handshake (0x02 only) |

## Findings

### Bugs

#### BUG-001: Code is duplicated from `playback::socks_forwarder`
- **Severity:** Medium
- **Location**: Entire file
- **Description:** This file is nearly identical to `src/playback/src/socks_forwarder.rs` (557 lines). The `ResolverSocksForwarder` struct, `start()`, `handle_connect()`, `socks5_connect()`, `parse_host_port()`, and `socks5_reply_name()` are all duplicated. The only difference is the struct name and the logging prefix ("resolver SOCKS5 forwarder" vs "SOCKS5 forwarder").
- **Impact**: Code duplication means bugs must be fixed in two places. If one is updated and the other isn't, they can diverge, breaking the critical "same circuit" invariant.
- **Recommendation**: Extract the shared logic into a common module (e.g., `src/shared/socks_forwarder.rs` or a `bogdan-socks` crate). Both the resolver and playback forwarders should use the same implementation with different logging tags.

#### BUG-002: No tests (0 tests)
- **Severity:** Medium
- **Location**: No test module
- **Description**: The file has 0 tests, while `socks_forwarder.rs` has 2 tests (`test_parse_host_port` and `test_socks5_reply_name`). The resolver forwarder doesn't even have those basic tests.
- **Impact**: The resolver's SOCKS5 handshake is completely untested.
- **Recommendation**: At minimum, add the same 2 tests that exist in the playback forwarder. Better: extract the shared code and test it once (see BUG-001).

#### BUG-003: Same issues as `socks_forwarder.rs` (IPv6 brackets, buffer size, etc.)
- **Severity:** Low
- **Location**: Throughout (same code as socks_forwarder.rs)
- **Description**: All the bugs found in the playback `socks_forwarder.rs` review (review-05.md) apply here: IPv6 bracket stripping in `parse_host_port`, fixed 4096-byte CONNECT buffer, sequential await on bidirectional copy, etc.
- **Impact**: Same as the playback forwarder.
- **Recommendation**: Fix both files together (see BUG-001). When the shared module is extracted, all fixes apply to both consumers.

### Design Issues

#### DESIGN-001: Two forwarders running simultaneously — port confusion
- **Severity:** Low
- **Location**: `start()` binds to `127.0.0.1:0` (random port)
- **Description**: Both the resolver forwarder and the playback forwarder bind to random local ports. With two forwarders running, there are two local HTTP CONNECT proxies. If one is misconfigured to use the other's port, traffic would be routed incorrectly.
- **Impact**: Low — the ports are random and assigned by the OS, so collisions are unlikely. But debugging is harder with two forwarders.
- **Recommendation**: Document that two forwarders exist (one for resolution, one for playback) and that they must use the same `isolation_username` to share a Tor circuit. Consider logging both ports together at startup.

#### DESIGN-002: No `check_exit_ip` diagnostic (unlike playback forwarder)
- **Severity:** Low
- **Location**: Missing method
- **Description**: The playback `SocksForwarder` has a `check_exit_ip()` method for diagnostics. The resolver forwarder doesn't. This is fine (the resolver doesn't need the diagnostic), but it means the resolver's exit IP can only be checked via the playback path.
- **Impact**: Minor — if the resolver's circuit needs diagnosis, the user must cast something to use the playback path's diagnostic.
- **Recommendation**: Acceptable for v1. If shared code is extracted (BUG-001), the diagnostic would be available to both.

### Security

#### SEC-001: Same security concerns as `socks_forwarder.rs`
- **Severity:** Low
- **Location**: Throughout
- **Description**: Same concerns as the playback forwarder: no auth on the local proxy, `check_exit_ip` (if added) would create traffic fingerprint, etc.
- **Recommendation**: See review-05.md (socks_forwarder.rs) for security recommendations. Apply to both files.

### Positive Observations

1. **Excellent rationale documentation** — the module doc explains *why* this forwarder exists (reqwest's built-in SOCKS5 offers both auth methods, Tor may choose no-auth) and *what happens if you don't use it* (CDN 403 from circuit mismatch).
2. **Correct auth method (0x02 only)** — matches the playback forwarder, ensuring the same Tor circuit.
3. **Mirror architecture** — uses the same `oneshot` shutdown, `tokio::select!` accept loop, and bidirectional tunneling as the playback forwarder.
4. **`Drop` impl calls `shutdown()`** — cleanup on drop, preventing orphaned listeners.
5. **Clear logging** — "auth=0x02 only" in the startup log makes the auth method explicit.
6. **Explicit purpose statement** — the doc says "This is the **mirror** of `playback::socks_forwarder`," making the relationship clear.

## Recommendations Summary

| Priority | Finding | Effort |
|----------|---------|--------|
| Medium | BUG-001: Extract shared SOCKS forwarder code | M (3–4 h) |
| Medium | BUG-002: Add tests (at least the 2 from playback forwarder) | S (1 h) |
| Low | BUG-003: Fix same bugs as socks_forwarder.rs (IPv6, buffer, etc.) | S (1–2 h, if shared) |
| Low | DESIGN-001: Document dual-forwarder architecture | S (30 min) |
| Low | DESIGN-002: Add check_exit_ip if shared code is extracted | S (30 min) |
| Low | SEC-001: Apply security fixes from review-05.md | S (1 h, if shared) |
