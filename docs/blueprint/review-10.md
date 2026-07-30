---
doc: code_review
project: picast
version: 1
phase: code_review
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# Code Review: `src/server/src/config.rs`

**File:** `src/server/src/config.rs`
**Lines:** 494
**Reviewer:** agent
**Date:** 2026-07-30

## Summary

The configuration module loads application settings from a TOML file and/or environment variables, with a three-tier precedence: env vars > TOML file > built-in defaults. It covers server addresses, Tor, display, playback, DLNA, and logging. The implementation is clean and well-structured with sensible defaults, serde-based deserialization, and clear documentation. This is a relatively small, low-risk module, but there are a few issues worth addressing.

## Key Components Reviewed

| Component | Lines | Purpose |
|-----------|-------|---------|
| `AppConfig` struct | 45–60 | Top-level config with 6 sub-configs |
| `ServerConfig` | 65–110 | HTTP/WS addresses, DB path, TLS |
| `TorConfig` | 113–135 | SOCKS addr, control port, cookie path |
| `DisplayConfig` | 138–145 | DRM device path |
| `PlaybackConfig` | 148–155 | ALSA audio device |
| `DlnaConfig` | 158–170 | Friendly name, port |
| `LoggingConfig` | 173–180 | Log level |
| Default functions | 185–215 | Sensible defaults |
| `load()` | 225–235 | File + env merge |
| `load_from_file()` | 240–275 | TOML file search and parse |
| `merge_env()` | 280–340 | Env var overrides |

## Findings

### Bugs

#### BUG-001: `BOGDAN_CONFIG` path is not validated for path traversal
- **Severity:** Low
- **Location:** Lines 247–250 (explicit path handling)
- **Description:** When `BOGDAN_CONFIG` is set, the path is used directly without validation. If the env var is set by an attacker (e.g., via a compromised systemd unit), a path like `../../etc/shadow` could be read. The file is parsed as TOML, so it wouldn't directly leak secrets, but the error message might reveal file existence.
- **Impact:** Low — the env var is typically set by the systemd unit, not user-controlled. But defense-in-depth.
- **Recommendation:** Validate that the path ends with `.toml` and is within an allowed directory (`/etc/bogdan/`, `/usr/local/etc/bogdan/`, or the current directory). Reject paths with `..` components.

#### BUG-002: `merge_env()` silently ignores invalid port for `BOGDAN_TOR_CONTROL_PORT` but not for others
- **Severity:** Low
- **Location:** Lines 300–306 (port parsing)
- **Description:** `BOGDAN_TOR_CONTROL_PORT` is parsed as `u16` and a warning is logged on failure. However, `BOGDAN_DLNA_PORT` (if it exists) and other port-like env vars may not have the same validation. The handling is inconsistent — some env vars are validated, others are taken as raw strings.
- **Impact:** A typo in a port env var (e.g., `BOGDAN_DLNA_PORT=99999`) could cause a confusing runtime error instead of a clear startup warning.
- **Recommendation:** Audit all port-like env vars and apply consistent validation. Consider a helper function `parse_env_port(name, &mut target)`.

#### BUG-003: No validation that TLS cert and key paths exist when TLS is enabled
- **Severity:** Low
- **Location:** Line 91 (`pub fn tls_enabled(&self) -> bool`)
- **Description:** `tls_enabled()` returns `true` if both `tls_cert_path` and `tls_key_path` are non-empty, but doesn't check that the files exist. The server will fail at startup with a file-not-found error rather than a clear "TLS cert file not found" message.
- **Impact:** Confusing error message on misconfiguration.
- **Recommendation:** Add a `validate_tls_files(&self) -> Result<()>` method that checks file existence and readability when `tls_enabled()` is true. Call it in `load()` or at server startup.

### Design Issues

#### DESIGN-001: No validation of listen addresses
- **Severity:** Low
- **Location**: `ServerConfig.http_addr` and `ws_addr`
- **Description:** The listen addresses (`http_addr`, `ws_addr`) are strings like `"0.0.0.0:8585"`. There's no validation that they're valid socket addresses. A typo like `"0.0.0.0:8585extra"` would cause a runtime bind failure.
- **Impact:** Misconfiguration causes a runtime error rather than a startup error.
- **Recommendation:** Add a `validate()` method on `AppConfig` that parses all address strings with `std::net::SocketAddr::from_str` and returns errors for invalid formats. Call it at the end of `load()`.

#### DESIGN-002: `DisplayConfig.drm_device` defaults to empty string (auto-detect) but auto-detect logic is elsewhere
- **Severity:** Low
- **Location:** Line 134 (`pub struct DisplayConfig`)
- **Description:** When `drm_device` is empty, the display manager auto-detects the DRM device. However, the auto-detect logic lives in `src/display/src/lib.rs`, not in the config module. This means the config module doesn't know what the actual device will be.
- **Impact:** Minor — the separation of concerns is correct. But the config documentation should note that empty means "auto-detect" and point to where the auto-detect logic lives.
- **Recommendation:** Add a doc comment on `drm_device`: "When empty, the display manager auto-detects the first available DRM device (typically `/dev/dri/card0`). See `src/display/src/lib.rs` for the detection logic."

#### DESIGN-003: No config file schema versioning
- **Severity:** Low
- **Location**: `AppConfig` struct
- **Description:** There's no `version` field in the config file. If the config schema changes between versions (fields renamed, removed, or added), old config files may silently use defaults for renamed fields.
- **Impact:** A user upgrading boGDan with an old config file may not notice that a setting is no longer being read.
- **Recommendation:** Add an optional `version: u32` field to `AppConfig`. On load, if the version doesn't match the expected version, log a warning. Consider a migration function that updates old config formats.

#### DESIGN-004: `PlaybackConfig` only has `audio_device` — no video settings
- **Severity:** Low
- **Location:** Line 142 (`pub struct PlaybackConfig`)
- **Description:** `PlaybackConfig` only contains `audio_device`. Video-related settings (max resolution, hardware decode enable/disable, buffer size) are not configurable. They're hardcoded in the pipeline.
- **Impact:** Users can't tune video playback without recompiling. The `maxResolution` from the browser extension isn't respected (see ytdlp.rs review DESIGN-003).
- **Recommendation:** Add video settings: `max_resolution`, `enable_hw_decode`, `buffer_size_bytes`. These should flow from the config (or the cast request) to the pipeline.

### Security

#### SEC-001: Config file may contain sensitive paths (TLS key, Tor cookie) — no permission check
- **Severity:** Low
- **Location**: `tls_key_path` and `cookie_path` fields
- **Description:** The config file may point to sensitive files (TLS private key, Tor control cookie). There's no check that these files have restrictive permissions (e.g., `0600` for the TLS key). If the files are world-readable, an attacker on the LAN could read them.
- **Impact:** Low on the appliance model (single user), but defense-in-depth.
- **Recommendation:** Add a permission check at startup: warn if `tls_key_path` is readable by group/other, and warn if `cookie_path` is readable by non-`debian-tor` users.

#### SEC-002: Default `http_addr` is `0.0.0.0:8585` — binds to all interfaces
- **Severity:** Low (acceptable for LAN appliance)
- **Location:** Line 182 (`fn default_http_addr() -> String`)
- **Description:** The default HTTP address is `0.0.0.0:8585`, which binds to all network interfaces. On a LAN, this is the intended behavior (the appliance should be reachable). But if the Pi is on a public network, the API is exposed.
- **Impact:** Acceptable for the appliance model — the iptables rules restrict access to LAN. But worth documenting.
- **Recommendation:** Document in the config example that `0.0.0.0` binds to all interfaces and that iptables rules (in `config/iptables.rules`) restrict access to the LAN. For users on public networks, suggest binding to a specific interface.

### Missing Tests

#### TEST-001: No tests for `load_from_file()` or `merge_env()`
- **Severity:** Low
- **Description:** The config loading and env-merging logic is not tested. There are no tests for: parsing a TOML file, env var overrides, missing config file (defaults), invalid TOML, or invalid env var values.
- **Recommendation:** Add tests that create a temp TOML file, set env vars, and verify the merged config. Test: defaults when no file/env, file overrides defaults, env overrides file, invalid port in env (warning logged, default used).

#### TEST-002: No test for config file search path priority
- **Severity:** Low
- **Description:** The search order (`BOGDAN_CONFIG` > `./bogdan.toml` > `/etc/bogdan/bogdan.toml` > `/usr/local/etc/bogdan/bogdan.toml`) is not tested.
- **Recommendation:** Add tests that verify the correct file is found when multiple exist, and that `BOGDAN_CONFIG` takes priority.

## Positive Observations

1. **Three-tier precedence is clear** — env vars > TOML > defaults, with each tier well-documented.
2. **Sensible defaults** — all fields have defaults that work out of the box (`0.0.0.0:8585`, `127.0.0.1:9050`, `/var/lib/bogdan/sessions.db`, etc.).
3. **Serde-based deserialization** — using `#[serde(default = "...")]` for each field means partial config files work correctly.
4. **Multiple search paths** — looks in `./bogdan.toml`, `/etc/bogdan/bogdan.toml`, and `/usr/local/etc/bogdan/bogdan.toml`, plus `BOGDAN_CONFIG` for explicit override.
5. **Clear error messages** — `with_context(|| format!("failed to read config file: {}", path))` gives actionable errors.
6. **Env var validation for ports** — `BOGDAN_TOR_CONTROL_PORT` is parsed as `u16` with a warning on failure (though inconsistent — see BUG-002).
7. **`tls_enabled()` helper** — makes the TLS check explicit and reusable.
8. **Well-documented example** — the module doc comment includes a complete `bogdan.toml` example.
9. **Sub-config separation** — `ServerConfig`, `TorConfig`, `DisplayConfig`, etc. make the config modular and easy to extend.
10. **494 lines** — appropriately sized for a config module; not too large, not too small.

## Recommendations Summary

| Priority | Finding | Effort |
|----------|---------|--------|
| Low | BUG-001: Validate BOGDAN_CONFIG path | S (30 min) |
| Low | BUG-002: Consistent port validation across all env vars | S (1 h) |
| Low | BUG-003: Validate TLS file existence at startup | S (30 min) |
| Low | DESIGN-001: Add config validation method | S (1–2 h) |
| Low | DESIGN-002: Document drm_device auto-detect behavior | S (15 min) |
| Low | DESIGN-003: Add config schema versioning | S (1 h) |
| Low | DESIGN-004: Add video settings to PlaybackConfig | M (2–3 h) |
| Low | SEC-001: Check sensitive file permissions at startup | S (1 h) |
| Low | SEC-002: Document 0.0.0.0 binding behavior | S (15 min) |
| Low | TEST-001: Add config loading and env merge tests | M (2–3 h) |
| Low | TEST-002: Add config search path priority tests | S (1 h) |
