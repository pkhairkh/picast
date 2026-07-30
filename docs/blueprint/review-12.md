---
doc: code_review
project: picast
version: 1
phase: code_review
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# Code Review: `src/server/src/main.rs`

**File:** `src/server/src/main.rs`
**Lines:** 683
**Reviewer:** agent
**Date:** 2026-07-30

## Summary

The server entry point initializes tracing, loads configuration, wires up all subsystems (Tor, display, playback, resolver, session) via trait adapters, and runs the main event loop with graceful shutdown on SIGINT/SIGTERM. The startup order is carefully sequenced (Tor → Display → Playback → Resolver → Session → HTTP → WebSocket → DLNA) with each subsystem depending on the ones before it. The trait adapter pattern cleanly bridges concrete types to the session manager's trait-object requirements. The implementation is well-structured but has a few issues.

## Key Components Reviewed

| Component | Lines | Purpose |
|-----------|-------|---------|
| `TorAdapter` | 30–55 | TorManager → TorTrait bridge |
| `DisplayAdapter` | 58–85 | DisplayManager → DisplayTrait bridge |
| `PlaybackAdapter` | 88–110 | PlaybackEngine → PlaybackTrait bridge |
| `ResolverAdapter` | 112–130 | Resolver → ResolverTrait bridge |
| `main()` | 222–683 | Startup orchestration + event loop |

## Findings

### Bugs

#### BUG-001: Display acquire failure at startup is logged as a warning but doesn't stop startup
- **Severity:** Medium
- **Location:** Lines 285–290 (display acquire in main)
- **Description:** When `dm.acquire()` fails at startup, the code logs a warning ("display acquire failed at startup — kmssink will auto-detect display") and continues. This is intentional (kmssink can acquire DRM master itself during playback), but it means the `connector_id` will be `None`, and kmssink will auto-detect the display. On a multi-output Pi (e.g., HDMI + DSI), auto-detect may choose the wrong output.
- **Impact:** On a single-HDMI Pi (the common case), this works fine. On a multi-output Pi, video may appear on the wrong display.
- **Recommendation:** Acceptable for v1 (single-HDMI is the target). For v2, if `connector_id` is `None` after acquire failure, log a more prominent warning suggesting the user configure `display.drm_device` explicitly.

#### BUG-002: Resolver cache path hardcoded to `/var/lib/bogdan/resolve-cache.db`
- **Severity:** Low
- **Location:** Line 340 (`cache_path` hardcoded)
- **Description:** The resolver cache path is hardcoded to `/var/lib/bogdan/resolve-cache.db`. The config module has a `db_path` for the session database, but there's no config option for the resolver cache path. If `/var/lib/bogdan/` doesn't exist or isn't writable, the persistent cache fails silently (the resolver falls back to in-memory cache).
- **Impact:** On a misconfigured system (missing `/var/lib/bogdan/`), the cache doesn't persist across restarts, causing re-resolution of recently-cast URLs.
- **Recommendation:** Add a `resolver_cache_path` field to `ServerConfig` (or a new `ResolverConfig`), defaulting to `/var/lib/bogdan/resolve-cache.db`. Log an error if the directory doesn't exist or isn't writable.

#### BUG-003: TLS load failure falls back to plain HTTP without exiting
- **Severity:** Low
- **Location:** Lines 395–405 (TLS acceptor loading)
- **Description:** If TLS cert/key loading fails, the code logs a warning and falls back to plain HTTP/WS. This is documented as intentional, but a user who configured TLS expecting encryption might be surprised that their API is now accessible over plain HTTP.
- **Impact:** A TLS misconfiguration silently downgrades to unencrypted HTTP, potentially exposing the API to LAN sniffing.
- **Recommendation:** If `tls_enabled()` is true but the acceptor fails to load, exit with an error rather than falling back. The user explicitly configured TLS; a fallback to HTTP is a security downgrade they didn't consent to.

#### BUG-004: `expect()` on SIGTERM handler installation can panic at startup
- **Severity:** Low
- **Location:** Line 258 (`expect("failed to install SIGTERM handler")`)
- **Description:** The SIGTERM handler installation uses `.expect()` which panics on failure. While SIGTERM handler installation rarely fails, a panic at this point would exit the process without cleanup.
- **Impact:** Low — `signal::unix::signal` only fails on invalid signal kinds or resource exhaustion. But panicking is not graceful.
- **Recommendation:** Use `?` instead of `expect()` and return the error from `main()`. Or use `match` with a clear error message.

### Design Issues

#### DESIGN-001: Trait adapters add an unnecessary layer of indirection
- **Severity:** Low
- **Location:** Lines 30–130 (adapter structs)
- **Description:** The `TorAdapter`, `DisplayAdapter`, `PlaybackAdapter`, and `ResolverAdapter` structs wrap the concrete types and implement the session traits. This is necessary because the session manager uses `dyn Trait` objects. However, the adapters are trivial (they just forward calls). If the concrete types implemented the traits directly, the adapters would be unnecessary.
- **Impact:** Extra boilerplate; changes to the trait require updating both the adapter and the concrete type.
- **Recommendation:** Consider implementing the session traits directly on the concrete types (`impl TorTrait for TorManager`). This eliminates the adapters. The downside is that the concrete types would depend on the session crate (circular dependency risk), so the adapter pattern may be the lesser evil. Document the tradeoff.

#### DESIGN-002: No health check endpoint integration
- **Severity:** Low
- **Location**: Throughout main (no health check scheduling)
- **Description:** The Tor manager has a `start_monitor()` that checks circuit health every 30 seconds, but there's no periodic health check for the display, playback, or resolver subsystems. If a subsystem fails silently (e.g., display connector unplugged), it's not detected until a cast is attempted.
- **Impact:** Subsystem failures are detected lazily (on next use) rather than proactively.
- **Recommendation:** Add a periodic health check task that queries each subsystem's health and logs warnings. For v2, integrate with the `/api/health` endpoint to report subsystem status.

#### DESIGN-003: Startup order is sequential — no parallelism
- **Severity:** Low
- **Location:** Lines 270–360 (sequential subsystem init)
- **Description:** Subsystems are initialized sequentially: Tor, then display, then playback, then resolver. Some of these could be parallelized (e.g., Tor and display don't depend on each other). Sequential startup adds ~5–10 seconds to boot time.
- **Impact:** Slower boot time. On an always-on appliance, this is a minor concern.
- **Recommendation:** Use `tokio::join!` to initialize independent subsystems in parallel. Tor + display can be parallel; playback depends on display; resolver depends on Tor.

#### DESIGN-004: No version reporting in logs or API
- **Severity:** Low
- **Location:** Line 23 (`VERSION` constant)
- **Description:** The `VERSION` constant is defined from `CARGO_PKG_VERSION` but is never logged at startup or exposed via the `/api/health` endpoint. Debugging issues without knowing the version is harder.
- **Impact:** Support requests can't easily identify the running version.
- **Recommendation:** Log the version at startup: `info!(version = VERSION, "boGDan starting")`. Add a `version` field to the `HealthResponse` in the HTTP API.

### Security

#### SEC-001: No `prctl` or `setrlimit` hardening in main
- **Severity:** Low
- **Location**: `main()` function
- **Description:** The server doesn't set resource limits (e.g., `RLIMIT_CORE` to disable core dumps, `RLIMIT_NOFILE` to limit file descriptors). The systemd unit may set some of these, but defense-in-depth at the application level is also valuable.
- **Impact:** Low — the systemd unit (per the spec) sets `LimitNOFILE`, `CapabilityBoundingSet`, etc. But application-level hardening adds defense-in-depth.
- **Recommendation:** For v2, add `setrlimit` calls to disable core dumps (which could contain session data) and set a reasonable file descriptor limit.

#### SEC-002: No panic hook for cleanup
- **Severity:** Low
- **Location**: `main()` function
- **Description:** If a panic occurs in any task, the default panic hook prints a message but doesn't trigger cleanup (stopping Tor, releasing DRM master, etc.). The process exits without graceful shutdown.
- **Impact:** A panic could leave the Tor process orphaned or DRM master held.
- **Recommendation:** Install a panic hook that triggers the shutdown sequence before the process exits. Use `std::panic::set_hook` to wrap the default hook.

### Missing Tests

#### TEST-001: No tests for main.rs
- **Severity:** Low
- **Description:** `main.rs` has no tests. The startup orchestration, trait adapters, and shutdown logic are untested.
- **Impact:** Regressions in startup order, adapter wiring, or shutdown would not be caught until runtime.
- **Recommendation:** This is hard to test because `main()` is the entry point. Consider extracting the startup logic into a `fn startup(config: AppConfig) -> Result<ServerHandles>` that can be tested. The trait adapters can be tested by verifying they correctly forward calls.

## Positive Observations

1. **Clear startup order documentation** — the comment block shows the dependency order (Tor → Display → Playback → Resolver → Session → HTTP → WS → DLNA), making the initialization logic easy to follow.
2. **Trait adapter pattern** — cleanly bridges concrete types to the session manager's trait objects, allowing the session manager to be tested with mocks.
3. **Graceful shutdown** — `broadcast::channel` + `tokio::select!` on SIGINT/SIGTERM ensures all subsystems receive the shutdown signal.
4. **TLS fallback is documented** — the warning messages explain what happened and what the fallback is.
5. **Explicit connector_id passing** — the display's `connector_id` is passed to the playback engine so kmssink renders to the correct HDMI output, avoiding auto-detect ambiguity on multi-output setups.
6. **Persistent resolver cache** — the cache survives restarts, avoiding re-resolution.
7. **`#[cfg(feature = "hw")]` gates** — the mock mode for non-hw builds is correctly handled, allowing development on x86_64.
8. **Structured logging** — `info!(socks = %config.tor.socks_addr, ...)` uses structured fields, making logs queryable.
9. **Subsystem wiring is explicit** — each adapter is wrapped in `Arc<dyn Trait>` and passed to the session manager, making the dependency injection visible.
10. **Version constant** — defined from `CARGO_PKG_VERSION` (though not yet logged — see DESIGN-004).

## Recommendations Summary

| Priority | Finding | Effort |
|----------|---------|--------|
| Medium | BUG-001: Document multi-output display caveat | S (15 min) |
| Medium | BUG-003: Exit on TLS failure instead of falling back to HTTP | S (30 min) |
| Low | BUG-002: Make resolver cache path configurable | S (1 h) |
| Low | BUG-004: Replace expect() with graceful error handling | S (15 min) |
| Low | DESIGN-001: Document trait adapter tradeoff or eliminate adapters | S (1 h) |
| Low | DESIGN-002: Add periodic subsystem health checks | M (2–3 h) |
| Low | DESIGN-003: Parallelize independent subsystem startup | M (2 h) |
| Low | DESIGN-004: Log version at startup and in health endpoint | S (30 min) |
| Low | SEC-001: Add setrlimit hardening | S (1 h) |
| Low | SEC-002: Install panic hook for cleanup | S (1–2 h) |
| Low | TEST-001: Extract startup logic for testability | M (3–4 h) |
