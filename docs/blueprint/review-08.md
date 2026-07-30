---
doc: code_review
project: picast
version: 1
phase: code_review
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# Code Review: `src/resolver/src/ytdlp.rs`

**File:** `src/resolver/src/ytdlp.rs`
**Lines:** 1172
**Reviewer:** agent
**Date:** 2026-07-30

## Summary

The yt-dlp integration module spawns `yt-dlp` as a subprocess to resolve web page URLs (YouTube, Vimeo, etc.) into direct media stream URLs. It constructs the yt-dlp command with Tor SOCKS5 proxy, H.264 format selection, subtitle extraction, and a 30-second timeout. The parsed JSON output is converted into a `ResolveResult`. This is the primary content resolution mechanism for the appliance — if yt-dlp fails or returns wrong formats, the user can't watch their video. The module has 53 tests (good coverage), but there is a **critical bug in the format string** that likely causes resolution failures or wrong-quality selection on many sites.

## Key Components Reviewed

| Component | Lines | Purpose |
|-----------|-------|---------|
| `H264_FORMAT_STRING` | 48–52 | Format selection string for yt-dlp `--format` |
| `YtdlpOutput` struct | 100–125 | Deserialized yt-dlp JSON (12 fields) |
| `resolve_with_ytdlp()` | 199–210 | Public entry point |
| `resolve_with_ytdlp_and_subs()` | 215–330 | Full implementation: spawn, parse, convert |
| `determine_category()` | 335–360 | Classify output as HLS/DASH/direct |
| `determine_mime_type()` | 365–400 | MIME type from codecs |
| `YtdlpOutput::resolve_urls()` | 125–195 | Extract URL from pre-merged or split formats |

## Findings

### Critical Bugs

#### CRIT-001: `H264_FORMAT_STRING` is malformed — `eight<=1080` instead of `height<=1080`
- **Severity:** **Critical**
- **Location:** Lines 48–52
- **Description:** The format string is:
  ```
  best[vcodec^=avc1]eight<=1080]/besteight<=1080]/bestvideo[vcodec^=avc1]eight<=1080]+bestaudio
  ```
  It contains `eight<=1080` — the leading `h` is missing. It should be `height<=1080`. Additionally, the bracket structure is wrong: `best[vcodec^=avc1]eight<=1080]` has a closing `]` without a matching opening `[` for the height filter.

  The correct format string should be:
  ```
  best[vcodec^=avc1][height<=1080]/best[height<=1080]/bestvideo[vcodec^=avc1][height<=1080]+bestaudio
  ```

  The doc comment at lines 28–30 also shows a broken version: `bestvideo[vcodec^=avc1]eight<=1080]+bestaudio/best[vcodec^=avc1]eight<=1080]/besteight<=1080]` — same `eight` typo.

  The tests at lines 429–430 check `H264_FORMAT_STRING.contains("height<=1080")` which **should fail** given the actual string contains `eight<=1080`. Either the tests are not being run, or there's a discrepancy. Let me verify: `contains("height<=1080")` on `"best[vcodec^=avc1]eight<=1080]/..."` — this would return `false` because the string has `eight` not `height`. **The test should be failing.**
- **Impact:** yt-dlp will likely fail to parse the format string or select wrong formats (e.g., 4K video that the Pi can't hardware-decode, or non-H.264 codecs that fall back to software decode). This could cause:
  - Resolution failures (yt-dlp exits with an error)
  - Playback of 4K HEVC video (thermal throttling, stuttering)
  - Playback of VP9 video (software decode, ~30fps, overheating)
- **Recommendation:** Fix the format string immediately:
  ```rust
  const H264_FORMAT_STRING: &str = concat!(
      "best[vcodec^=avc1][height<=1080]/",
      "best[height<=1080]/",
      "bestvideo[vcodec^=avc1][height<=1080]+bestaudio"
  );
  ```
  Also fix the doc comment at lines 28–30 to match. Verify the test at line 430 actually passes (it should be failing with the current code).

### Bugs

#### BUG-001: `TorUnavailable` error returned when yt-dlp binary is not found
- **Severity:** Low
- **Location:** Lines 275–280 (spawn error handling)
- **Description:** When the `yt-dlp` binary is not found (`ErrorKind::NotFound`), the code returns `ResolveError::TorUnavailable("yt-dlp binary not found")`. This is misleading — the error has nothing to do with Tor; it's a missing dependency.
- **Impact:** Users see a confusing error message blaming Tor when the actual problem is a missing yt-dlp installation.
- **Recommendation:** Add a `ResolveError::BinaryNotFound` variant, or use `ResolveError::NoMediaFound("yt-dlp binary not found — install with: pip install yt-dlp")`.

#### BUG-002: Only the first line of stderr is included in the error
- **Severity:** Low
- **Location:** Line 287 (`stderr.lines().next().unwrap_or("unknown error")`)
- **Description:** When yt-dlp exits non-zero, only the first line of stderr is captured for the error message. yt-dlp's error messages often span multiple lines, with the most useful diagnostic on the second or third line.
- **Impact:** Error messages are less helpful than they could be, making debugging harder.
- **Recommendation:** Include the full stderr (or the last 3–5 lines) in the error message. Truncate to a reasonable length (e.g., 500 chars) to avoid log bloat.

#### BUG-003: No retry on yt-dlp timeout
- **Severity:** Low
- **Location:** Lines 268–273 (timeout handling)
- **Description:** If yt-dlp times out after 30 seconds, the error is returned immediately without retry. Tor circuit congestion can cause yt-dlp to be slow on the first attempt but succeed on a retry with a fresh circuit.
- **Impact:** Transient Tor congestion causes resolution failure when a retry might succeed.
- **Recommendation:** Retry once with a fresh isolation username (appending a counter) to force a new Tor circuit. The session layer's CDN retry logic handles playback failures, but resolution failures are not retried.

#### BUG-004: Subtitle files are written but never read
- **Severity:** Low
- **Location:** Lines 240–250 (subtitle extraction flags) and the `subtitle_tracks` field
- **Description:** The code passes `--write-subs --write-auto-subs --sub-langs --sub-format` to yt-dlp, which writes subtitle files to the temp directory. However, the code only extracts the *keys* of the `subtitles` HashMap (language codes) for `subtitle_tracks` — it never reads the actual subtitle file contents or their paths. The subtitle files are deleted when `temp_dir` is dropped.
- **Impact:** Subtitles are extracted by yt-dlp but immediately discarded. The GStreamer pipeline never receives subtitle data.
- **Recommendation:** Either (a) read the subtitle file paths from the `subtitles` HashMap and pass them to the playback engine, or (b) remove the subtitle extraction flags until subtitle support is actually implemented (saves 2–5 seconds per resolution).

### Design Issues

#### DESIGN-001: 30-second timeout may be too short for slow Tor circuits
- **Severity:** Medium
- **Location:** Line 35 (`YTDLP_TIMEOUT_SECS: u64 = 30`)
- **Description:** The yt-dlp timeout is 30 seconds. Through Tor, yt-dlp must: connect through the SOCKS proxy, resolve DNS through Tor, fetch the page, follow redirects, and extract media URLs. On a slow circuit, this can take 20–25 seconds just for the page fetch, leaving little time for extraction.
- **Impact:** Legitimate resolutions that take 31–40 seconds on slow circuits are killed and reported as failures.
- **Recommendation:** Increase the timeout to 45 or 60 seconds. Alternatively, make it configurable via `bogdan.toml`. The architecture doc (§8.2) notes Tor bandwidth varies, and the spec (R-014) allows 10 seconds for YouTube resolution — but that's the target, not the timeout.

#### DESIGN-002: `--no-warnings` suppresses useful diagnostic output
- **Severity:** Low
- **Location:** Line 231 (`--no-warnings`)
- **Description:** The `--no-warnings` flag suppresses yt-dlp's warning messages. While this keeps stderr clean, warnings often contain useful diagnostics (e.g., "format not available, falling back to...") that would help debug resolution issues.
- **Impact:** Debugging resolution failures is harder because warnings are hidden.
- **Recommendation:** Remove `--no-warnings` and instead filter the stderr at the application level (log warnings at `debug` level, errors at `warn` level).

#### DESIGN-003: Format string hardcodes 1080p max — no config override
- **Severity:** Low
- **Location:** Lines 48–52 (`H264_FORMAT_STRING`)
- **Description:** The format string hardcodes `height<=1080` as the maximum resolution. The browser extension's `maxResolution` setting (240p–1080p) is documented in the spec but not respected by the resolver.
- **Impact:** Users who set `maxResolution=720p` in the extension still get 1080p video, wasting Tor bandwidth.
- **Recommendation:** Make the max height a parameter to `resolve_with_ytdlp()`, and construct the format string dynamically: `format!("best[vcodec^=avc1][height<={}]", max_height)`.

### Security

#### SEC-001: URL passed directly to yt-dlp command line without sanitization
- **Severity:** Low
- **Location:** Line 255 (`cmd.arg(url)`)
- **Description:** The URL is passed directly as a command-line argument to yt-dlp. While `tokio::process::Command` handles argument escaping correctly (no shell injection), a URL containing shell metacharacters or very long strings could cause issues in yt-dlp's own argument parser.
- **Impact:** Low — `Command::arg` is safe from shell injection. But yt-dlp may behave unexpectedly with malformed URLs.
- **Recommendation:** Validate the URL format before passing it to yt-dlp (already done by `Url::parse` in the caller). Add a length check (see resolver review SEC-001).

#### SEC-002: Temp directory created in system default location
- **Severity:** Low
- **Location:** Line 222 (`tempfile::tempdir()`)
- **Description:** `tempfile::tempdir()` creates a directory in the system's default temp location (`/tmp` on Linux). On a multi-user system, another user could potentially read the subtitle files before they're deleted.
- **Impact:** Low on the appliance model (single `bogdan` user), but subtitle files may contain sensitive content (user's viewing choices).
- **Recommendation:** Create the temp directory in the boGDan runtime directory (`/var/lib/bogdan/tmp/` or similar) with restrictive permissions (`0o700`).

### Missing Tests

#### TEST-001: Test at line 430 should be failing but apparently isn't
- **Severity:** **Critical** (related to CRIT-001)
- **Location:** Lines 429–430
- **Description:** The test `assert!(H264_FORMAT_STRING.contains("height<=1080"))` should fail because the actual string contains `eight<=1080`, not `height<=1080`. If this test is passing, either the test is not being run, or there's a discrepancy between the code and the tests.
- **Impact:** The critical format string bug (CRIT-001) is not caught by the test suite.
- **Recommendation:** Verify the test actually runs and fails. If it's being skipped, fix the CI configuration. If the string was recently changed, the test may have been correct before and is now stale.

#### TEST-002: No integration test with real yt-dlp
- **Severity:** Medium
- **Description:** All 53 tests are unit tests for parsing and format string construction. There are no integration tests that spawn a real yt-dlp process (even with a mock URL).
- **Impact:** The actual subprocess invocation, argument construction, and JSON parsing are untested end-to-end.
- **Recommendation:** Add integration tests in `src/resolver/tests/` that mock yt-dlp with a shell script returning canned JSON, verifying the command construction and output parsing.

## Positive Observations

1. **`kill_on_drop(true)`** — the yt-dlp process is killed when the future is dropped, preventing orphaned processes on timeout or cancellation.
2. **Timeout enforcement** — `tokio::time::timeout` prevents yt-dlp from hanging indefinitely.
3. **Temp directory cleanup** — `tempfile::tempdir()` automatically cleans up subtitle files on drop, including on error paths.
4. **Robust duration handling** — negative, NaN, and infinite durations (from live streams) are filtered to `None`.
5. **Pre-merged vs split format handling** — `resolve_urls()` correctly handles both top-level `url` and `requested_formats` array.
6. **53 unit tests** — good coverage of parsing, format string construction, and category determination.
7. **H.264 forcing** — the format string (when correct) forces H.264 for V4L2 hardware decode compatibility.
8. **Tor proxy with isolation** — `socks5h://` with the per-host isolation username ensures circuit consistency.
9. **Clear error messages** — the "install with: pip install yt-dlp" hint (though mislabeled as TorUnavailable) is helpful for users.
10. **Subtitle language list** — `en,es,fr,de` covers the most common languages.

## Recommendations Summary

| Priority | Finding | Effort |
|----------|---------|--------|
| **Critical** | **CRIT-001: Fix H264_FORMAT_STRING — `eight` → `height`, fix brackets** | **S (15 min)** |
| **Critical** | **TEST-001: Verify test at line 430 actually runs and fails** | **S (30 min)** |
| Medium | DESIGN-001: Increase timeout to 45–60s or make configurable | S (30 min) |
| Medium | TEST-002: Add integration tests with mock yt-dlp | L (4–8 h) |
| Low | BUG-001: Fix misleading TorUnavailable error for missing binary | S (15 min) |
| Low | BUG-002: Include more stderr in error messages | S (15 min) |
| Low | BUG-003: Retry once on timeout with fresh circuit | S (1 h) |
| Low | BUG-004: Read subtitle files or remove extraction flags | S (1–2 h) |
| Low | DESIGN-002: Remove --no-warnings, filter at app level | S (30 min) |
| Low | DESIGN-003: Make max resolution configurable | S (1 h) |
| Low | SEC-001: Validate URL before passing to yt-dlp | S (15 min) |
| Low | SEC-002: Create temp dir in runtime directory with 0o700 | S (30 min) |
