---
doc: code_review
project: picast
version: 1
phase: code_review
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# Code Review: `src/playback/src/stream_source.rs`

**File:** `src/playback/src/stream_source.rs`
**Lines:** 1575
**Reviewer:** agent
**Date:** 2026-07-30

## Summary

The stream source provides progressive download for GStreamer's `appsrc` element. It replaces the previous real-time proxy chain with a buffered architecture: CDN → Tor → SOCKS Forwarder → reqwest → shared buffer → appsrc → queue2. This decouples download speed from playback bitrate, allowing pre-buffering when Tor throughput is low. It supports both MP4 (direct download) and HLS (segment-by-segment) modes, with CDN preflight checks, throughput measurement, and flow control via a bounded channel. The implementation is well-documented with clear rationale for the architecture change. However, there are several issues.

## Key Components Reviewed

| Component | Lines | Purpose |
|-----------|-------|---------|
| `StreamSourceConfig` | 54–70 | Preflight retry configuration |
| `DataChunk` | 77–85 | Downloaded data chunk with offset |
| `StreamSource` struct | 108–130 | Main source with channel, client, mode |
| `ProgressState` | 135–205 | Shared download progress tracking |
| `start()` | 214–330 | Build reqwest client, start SOCKS forwarder, detect HLS |
| `preflight_check()` | 343–595 | CDN reachability verification (GET Range:0-0) |
| `start_download()` | 595–1200 | Background download task |
| `recv_chunk()` | 1203–1207 | Receive next data chunk |
| `Drop` impl | 1223+ | Cancel download on drop |

## Findings

### Bugs

#### BUG-001: `cdn_rate_limit_kbps.lock().unwrap()` can panic on poisoned mutex
- **Severity:** Low
- **Location:** Lines 295, 335, and throughout (`progress.cdn_rate_limit_kbps.lock().unwrap()`)
- **Description:** The `cdn_rate_limit_kbps` mutex is locked with `.unwrap()` which panics if the mutex is poisoned. This pattern appears in multiple places. If any thread panics while holding this lock, all subsequent accesses will panic, cascading the failure.
- **Impact:** A single panic poisons the mutex, causing all subsequent rate-limit queries to panic.
- **Recommendation:** Use `.lock().unwrap_or_else(|e| e.into_inner())` to recover from poison, or switch to `tokio::sync::Mutex` (which doesn't poison). Or use `AtomicU64` instead of `Mutex<Option<u64>>` for the rate limit.

#### BUG-002: Browser User-Agent is hardcoded and may not match the resolver's UA
- **Severity:** Low
- **Location:** Line 71 (`BROWSER_UA` constant)
- **Description:** The `BROWSER_UA` is hardcoded as a Chrome 131 string. The comment says "Must match the resolver's UA" but there's no mechanism to ensure they match. If the resolver's UA is updated but this one isn't (or vice versa), the CDN will see different UAs for resolution and download, which could trigger anti-bot detection.
- **Impact:** CDN anti-bot systems may flag the mismatch between resolver UA and download UA, returning 403.
- **Recommendation:** Move the `BROWSER_UA` constant to a shared module (e.g., `src/playback/src/lib.rs` or a `constants` crate) and import it in both the resolver and the stream source. Add a test that verifies both use the same constant.

#### BUG-003: HLS segment download doesn't handle segment deletion
- **Severity:** Low
- **Location:** `start_download()` HLS mode (lines 595+)
- **Description:** For HLS live streams, segments are added to the playlist over time and eventually removed (sliding window). The stream source fetches the playlist once and downloads segments sequentially. It doesn't re-fetch the playlist to discover new segments or handle removed segments.
- **Impact:** HLS live streams will stop playing after the initial playlist's segments are exhausted.
- **Recommendation:** For HLS live streams, periodically re-fetch the playlist (every segment duration) to discover new segments. Use the `#EXT-X-MEDIA-SEQUENCE` tag to track which segments have been downloaded.

#### BUG-004: No timeout on individual segment downloads in HLS mode
- **Severity:** Low
- **Location**: HLS segment download in `start_download()`
- **Description:** In HLS mode, each segment is downloaded via reqwest. If a segment download hangs (e.g., Tor circuit stalls), there's no per-segment timeout. The overall download task has a cancel flag, but it's only checked between segments.
- **Impact:** A stalled segment download blocks all subsequent segments, causing playback to stall indefinitely.
- **Recommendation:** Wrap each segment download in `tokio::time::timeout` (e.g., 30 seconds per segment). On timeout, cancel the download and retry with a fresh circuit.

### Design Issues

#### DESIGN-001: 1575-line file is too large
- **Severity:** Medium
- **Location**: Entire file
- **Description:** The `stream_source.rs` file is 1575 lines, covering: reqwest client construction, SOCKS forwarder integration, HLS playlist parsing, segment download, MP4 download, flow control, progress tracking, and CDN preflight. This is too much for one file.
- **Impact:** Maintenance burden; high risk of merge conflicts.
- **Recommendation:** Split into sub-modules:
  - `stream_source/mp4.rs` — MP4 direct download
  - `stream_source/hls.rs` — HLS playlist parsing and segment download
  - `stream_source/preflight.rs` — CDN preflight check
  - `stream_source/progress.rs` — `ProgressState` and `DownloadProgress`

#### DESIGN-002: `start_download()` is a 600-line function
- **Severity:** Medium
- **Location**: Lines 595–1200 (`start_download`)
- **Description:** The `start_download()` method is approximately 600 lines long. It handles both MP4 and HLS modes, with inline logic for each. This is far too long for a single function.
- **Impact:** The function is extremely difficult to read, test, or modify. Bugs are hard to locate.
- **Recommendation:** Extract into separate functions: `download_mp4()`, `download_hls()`, `download_segment()`. Each should be 50–100 lines max. Use an enum dispatch pattern to route to the correct downloader.

#### DESIGN-003: Flow control relies on channel capacity — no explicit backpressure signal
- **Severity:** Low
- **Location**: Lines 285–290 (`CHANNEL_CAPACITY = 128`)
- **Description:** Flow control is implemented via the bounded channel's natural backpressure: when the channel is full, `data_tx.send()` awaits. The comment mentions "need-data" and "enough-data" signals from appsrc, but the actual implementation uses channel capacity, not appsrc signals.
- **Impact:** The buffer size (128 chunks × ~256 KB = ~32 MB) is fixed. For high-bitrate streams, this may be too small; for low-bitrate streams, it wastes memory.
- **Recommendation:** Make the channel capacity configurable. Consider using appsrc's `need-data`/`enough-data` signals for more precise flow control, as the comment suggests.

#### DESIGN-004: `sp=` bypass logic removed but diagnostic logging remains
- **Severity:** Low
- **Location**: Lines 275–280 (comment about removed bypass logic)
- **Description:** The comment notes that the `sp=` bypass logic (replacing/stripping the speed-limit parameter) was removed because it "ALWAYS fails — modifying the URL's query parameters invalidates the CDN's &t= signature." However, the `extract_cdn_speed_param()` function still parses `sp=` for diagnostic logging. This is correct (diagnostics are useful), but the architecture suggests the bypass was a significant effort that was abandoned.
- **Impact:** No functional issue. The diagnostic logging is helpful.
- **Recommendation:** Document the failed bypass attempt in an ADR so future developers don't re-attempt it. The comment is good; an ADR would be more discoverable.

### Security

#### SEC-001: CDN URL logged at info level
- **Severity:** Low
- **Location**: Lines 260, 278, 285 (`cdn_url = %cdn_url`)
- **Description:** The CDN URL (which may contain user-specific tokens) is logged at `info` level. On an appliance with journald persistence, these URLs are stored in the journal and could reveal the user's viewing history if the journal is accessed.
- **Impact:** Privacy concern — the journald log contains CDN URLs that could identify viewed content.
- **Recommendation:** Log the CDN URL at `debug` level only. At `info` level, log only the CDN hostname (not the full URL with tokens).

#### SEC-002: `cookie_store(true)` on reqwest client may retain cookies across sessions
- **Severity:** Low
- **Location**: Line 246 and 267 (`.cookie_store(true)`)
- **Description:** The reqwest client is built with `.cookie_store(true)`, which enables an in-memory cookie jar. Cookies from one download (e.g., a Set-Cookie from the CDN) will be sent on subsequent requests through the same client. If the client is reused across sessions, cookies from a previous session could leak.
- **Impact:** Low — the client is created per `StreamSource` instance, which is per-session. But if the client is ever reused, cookies could cross-contaminate.
- **Recommendation:** Verify that each `StreamSource` creates a fresh reqwest client (which it does in `start()`). Document that the cookie jar is per-session. For v2, consider explicitly clearing the cookie jar after download completes.

### Missing Tests

#### TEST-001: Only 11 tests for a 1575-line file
- **Severity:** Medium
- **Description:** The file has 11 tests, which is low coverage for 1575 lines. The tests likely cover pure functions (URL parsing, HLS detection) but not the download logic, preflight, or flow control.
- **Impact:** The core download and preflight logic is untested.
- **Recommendation:** Add tests with a mock HTTP server (using `wiremock` or `mockito`) that verify: preflight check success/failure, MP4 download, HLS segment download, flow control backpressure, and cancel behavior.

#### TEST-002: No test for HLS playlist parsing
- **Severity:** Low
- **Description**: HLS playlist parsing (master playlist → variant selection → segment URLs) is not tested.
- **Recommendation**: Add tests with sample HLS playlists (master + variant) and verify the correct variant is selected and segment URLs are extracted.

## Positive Observations

1. **Excellent architecture documentation** — the module doc comment explains *why* the progressive download architecture replaced the real-time proxy chain, with a clear problem statement and solution rationale.
2. **Flow control via bounded channel** — using `mpsc::channel(128)` provides natural backpressure without complex signaling.
3. **Direct mode vs Tor mode** — correctly handles both: when `socks_addr` is empty, connects directly (no Tor); when set, uses the SOCKS forwarder.
4. **CDN preflight check** — verifies the CDN accepts the URL before starting the full download, using `GET Range: bytes=0-0` (not HEAD, which CDNs reject).
5. **Throughput measurement** — measures download speed before playback starts, enabling bitrate-aware buffering.
6. **`sp=` diagnostic logging** — even though the bypass was removed, the speed-limit parameter is still detected and logged as a warning, helping users understand why playback stutters.
7. **Cancel flag** — `AtomicBool` allows the download to be cancelled from another thread.
8. **`Drop` impl cancels download** — when `StreamSource` is dropped, the cancel flag is set, preventing orphaned download tasks.
9. **Browser-like User-Agent** — uses a Chrome UA string to avoid CDN anti-bot detection.
10. **HLS support** — handles HLS playlists (master → variant → segments), not just direct MP4.

## Recommendations Summary

| Priority | Finding | Effort |
|----------|---------|--------|
| Medium | BUG-001: Replace `lock().unwrap()` with poison recovery | S (1 h) |
| Medium | DESIGN-001: Split 1575-line file into sub-modules | L (4–8 h) |
| Medium | DESIGN-002: Split 600-line `start_download()` function | M (3–4 h) |
| Medium | TEST-001: Add mock HTTP server tests | L (4–8 h) |
| Low | BUG-002: Share BROWSER_UA constant between resolver and stream source | S (30 min) |
| Low | BUG-003: Handle HLS live playlist refresh | M (2–3 h) |
| Low | BUG-004: Add per-segment timeout in HLS mode | S (1 h) |
| Low | DESIGN-003: Make channel capacity configurable | S (30 min) |
| Low | DESIGN-004: Document the failed sp= bypass in an ADR | S (30 min) |
| Low | SEC-001: Log CDN URL at debug level, not info | S (15 min) |
| Low | SEC-002: Document cookie jar per-session scope | S (15 min) |
| Low | TEST-002: Add HLS playlist parsing tests | S (1–2 h) |
