---
doc: code_review
project: picast
version: 1
phase: code_review
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# Code Review: `src/playback/src/events.rs` and `src/protocols/src/lib.rs`

**Files:** `src/playback/src/events.rs` (129 lines) + `src/protocols/src/lib.rs` (19 lines)
**Reviewer:** agent
**Date:** 2026-07-30

## Summary

Two small, clean files: `events.rs` defines the `PlaybackEvent` enum for playback engine events, and `lib.rs` is the protocols crate root that re-exports the three protocol modules. Both are low-risk with minimal issues.

---

## Part 1: `src/playback/src/events.rs` (129 lines)

### Summary

Defines the `PlaybackEvent` enum with 11 variants covering playback state changes, errors, buffering, position updates, latency, and download progress. Events are delivered via `mpsc` channel to the session layer. The enum uses serde tagged JSON (`{"type": "buffering", "percent": 75}`) for wire serialization.

### Findings

#### DESIGN-001: `PlaybackEvent` and `SessionEvent` have overlapping variants
- **Severity:** Low
- **Location:** Throughout the enum
- **Description:** `PlaybackEvent` has `Playing`, `Paused`, `Stopped`, `Error`, `CdnForbidden`, `AudioDeviceError`, `Buffering`, `PositionUpdate` — and `SessionEvent` (in `session/lib.rs`) has the same variants (plus `Created`, `Resolving`, `Resolved`, `VolumeChanged`, `Seeking`). There's significant overlap between the two enums.
- **Impact:** The session layer must translate `PlaybackEvent` to `SessionEvent`, adding boilerplate. The two enums can diverge if one is updated without the other.
- **Recommendation:** Consider unifying into a single event enum, or have `SessionEvent` wrap `PlaybackEvent` for the overlapping variants. Document the relationship clearly.

#### DESIGN-002: `DownloadProgress` event has 4 fields — could be a struct
- **Severity:** Low
- **Location:** `PlaybackEvent::DownloadProgress` variant (lines 95–105)
- **Description:** The `DownloadProgress` variant has 4 fields: `downloaded_bytes`, `total_bytes`, `throughput_kbps`, `elapsed_secs`. This is a lot of fields for an enum variant.
- **Impact:** Constructing the variant is verbose; accessing fields requires pattern matching.
- **Recommendation:** Extract a `DownloadProgress` struct (which may already exist — the stream source imports `DownloadProgress`) and use it as the variant's payload: `DownloadProgress(DownloadProgress)`.

#### BUG-001: No `VolumeChanged` event
- **Severity:** Low
- **Location:** Missing from the enum
- **Description:** `SessionEvent` has `VolumeChanged`, but `PlaybackEvent` doesn't. Volume changes initiated by the playback engine (e.g., via GStreamer volume element) don't generate a `PlaybackEvent`. The session layer won't know about volume changes unless they come through the HTTP/WS API.
- **Impact:** Volume changes from sources other than the API (e.g., a hardware volume knob, or a GStreamer-internal change) won't be propagated to clients.
- **Recommendation:** Add `VolumeChanged { volume: u8 }` to `PlaybackEvent`, or document that volume changes only flow from the session layer to the playback engine (one-directional).

#### DESIGN-003: `#![cfg(feature = "hw")]` gates the entire file
- **Severity:** Low
- **Location:** Line 1
- **Description:** The entire file is behind `#![cfg(feature = "hw")]`, meaning it's not compiled in non-hw builds. But `PlaybackEvent` is a pure data type with no hardware dependencies — it could be available in all builds.
- **Impact:** Non-hw builds can't use `PlaybackEvent`, even for testing or mocking.
- **Recommendation:** Move the `cfg` gate to only the hw-specific impls, not the enum definition. The enum itself is pure data and should be available everywhere.

### Positive Observations (events.rs)

1. **Serde tagged JSON** — `#[serde(tag = "type", rename_all = "snake_case")]` produces clean, consistent JSON.
2. **Comprehensive variant set** — covers all playback states, errors, progress, and latency.
3. **`CdnForbidden` variant** — explicit event for CDN 403, enabling the session layer's retry logic.
4. **`AudioDeviceError` is non-fatal** — the doc comment explicitly says "The session layer should NOT treat this as a fatal error," preventing unnecessary playback termination.
5. **2 tests** — serde round-trip and tagged JSON format, ensuring wire compatibility.
6. **Clear doc comments** — each variant has a description, and fields are documented.

---

## Part 2: `src/protocols/src/lib.rs` (19 lines)

### Summary

The protocols crate root. Re-exports `HttpApiServer`, `WebSocketServer`, `DlnaRenderer`, `load_tls_acceptor`, and `run_dlna_sync` from their sub-modules.

### Findings

#### DESIGN-001: No crate-level documentation beyond the module comment
- **Severity:** Low
- **Location:** Lines 1–10
- **Description:** The crate root has a brief module comment but no `//!` crate-level documentation explaining the protocol layer's architecture, design decisions, or how the three protocols interact.
- **Impact:** New contributors need to read each module to understand the protocol layer.
- **Recommendation:** Add a crate-level doc comment explaining: the three-protocol architecture, the shared session backend, the CORS/security model, and how to add a new protocol.

#### DESIGN-002: `run_dlna_sync` exported but not documented
- **Severity:** Low
- **Location:** Line 17 (`pub use dlna::{run_dlna_sync, DlnaRenderer}`)
- **Description:** `run_dlna_sync` is exported at the crate level but its purpose isn't documented in the crate root. It's the session-event sync function for DLNA.
- **Impact:** Users of the crate don't know what `run_dlna_sync` does without reading the dlna module.
- **Recommendation:** Add a brief doc comment in the crate root: `/// `run_dlna_sync` synchronizes DLNA state with the session manager.`

### Positive Observations (lib.rs)

1. **Clean re-exports** — the public API is clearly defined.
2. **Module organization** — `dlna`, `http`, `tls`, `ws` are well-separated.
3. **Minimal surface** — only 5 items are exported, keeping the API small.

## Recommendations Summary

| File | Priority | Finding | Effort |
|------|----------|---------|--------|
| events.rs | Low | DESIGN-001: Unify PlaybackEvent and SessionEvent | M (2–3 h) |
| events.rs | Low | DESIGN-002: Use DownloadProgress struct | S (30 min) |
| events.rs | Low | BUG-001: Add VolumeChanged event | S (30 min) |
| events.rs | Low | DESIGN-003: Remove hw cfg gate from enum | S (15 min) |
| lib.rs | Low | DESIGN-001: Add crate-level documentation | S (1 h) |
| lib.rs | Low | DESIGN-002: Document run_dlna_sync export | S (5 min) |
