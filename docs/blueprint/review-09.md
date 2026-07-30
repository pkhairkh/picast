---
doc: code_review
project: picast
version: 1
phase: code_review
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T12:10:00Z
---

# Code Review: `src/playback/src/lib.rs`

**File:** `src/playback/src/lib.rs`
**Lines:** 3563 (including ~660 lines of tests, so ~2900 lines of production code)
**Reviewer:** agent
**Date:** 2026-07-30

## Summary

The playback engine is the largest source file in the boGDan project and the heart of the media pipeline. It wraps GStreamer into a high-level API that manages pipeline construction (souphttpsrc → queue2 → parsebin → V4L2 → kmssink), adaptive bitrate control, buffer health monitoring, stall detection, CDN IP-mismatch detection, and software-decode fallback. The file uses conditional compilation (`#[cfg(feature = "hw")]` vs `#[cfg(not(feature = "hw"))]`) to provide dual implementations: a real GStreamer path for Pi hardware and a mock path for x86_64 development/CI. The implementation is feature-rich and well-commented — the CDN IP-binding invariant, the V4L2 latency compensation rationale, and the audio-cascade not-negotiated detection are all thoroughly documented. However, the file's size and complexity are significant maintenance risks: the `play()` method alone is ~400 lines, the bus watch closure is ~500 lines with 10+ captured variables, and the hw code path has zero test coverage.

## Scope Reviewed

| Concern | Implementation | Notes |
|---------|----------------|-------|
| Pipeline lifecycle | `PlaybackEngine` struct with `Arc<Mutex<Option<GstPipeline>>>` | Dual hw/mock implementations via `cfg` |
| CDN IP check | `extract_cdn_ip_prefix()` + proactive check in `play()` | Prevents 403 before pipeline construction |
| SW fallback | `is_negotiation_error()` + retry with `hw_accel=false` | Automatic on V4L2 caps failure |
| Bus watch | `bus.add_watch()` closure (~500 lines) | Handles Error, EOS, Buffering, Warning, StateChanged |
| Stall detection | `stall_start` mutex + 30s timeout | Detects CDN disconnect during playback |
| Audio fallback | `is_audio_sink_error` + `is_audio_cascade_not_negotiated` | Video continues if audio device unavailable |
| Mock mode | 15 mock fields + parallel method implementations | For x86_64 CI without GStreamer hardware |
| Diagnostics | 10s post-play spawn task | Checks pipeline reached Playing state |

## Findings

### Bugs

#### BUG-001: `stop()` in hw mode doesn't clear state on pipeline teardown failure
- **Severity:** Medium
- **Location:** Lines 2502–2510 (`stop()` hw impl — `pipeline.stop()?` at line 2505)
- **Description:** The hw `stop()` method calls `pipeline.stop()?` using the `?` operator. If `pipeline.stop()` returns an error, the `?` propagates the error immediately — before `*guard = None` (line 2506), before `self.is_playing.store(false, ...)` (line 2507), and before the `Stopped` event is broadcast (line 2508). The pipeline remains in the `gst_pipeline` field, `is_playing` remains `true`, and subscribers don't receive a `Stopped` event.
- **Impact:** A failed pipeline teardown leaves the engine in an inconsistent state: `is_playing` says `true` but the pipeline may be partially destroyed. The next `play()` call will try to stop the existing pipeline again (line 1003: `existing.stop()`), potentially hitting the same error in a loop.
- **Recommendation:** Use best-effort cleanup regardless of the stop result:
  ```rust
  pub async fn stop(&self) -> Result<(), PlaybackError> {
      let mut guard = self.gst_pipeline.lock().await;
      if let Some(ref mut pipeline) = *guard {
          if let Err(e) = pipeline.stop() {
              tracing::warn!(error = %e, "pipeline.stop() failed — forcing cleanup");
          }
          *guard = None;
          self.is_playing.store(false, Ordering::Relaxed);
          let _ = self.event_tx.send(PlaybackEvent::Stopped);
      }
      Ok(())
  }
  ```

#### BUG-002: `extract_cdn_ip_prefix` accepts malformed IP prefixes
- **Severity:** Low
- **Location:** Lines 775–793 (`extract_cdn_ip_prefix` — validation at line 789)
- **Description:** The function validates the `&i=` parameter value with `value.contains('.') && value.chars().all(|c| c.is_ascii_digit() || c == '.')`. This accepts strings like `"..."`, `"1.2.3.4.5.6"`, `"192.42.abc"` (wait, no — `abc` would fail the `all` check). But it DOES accept `"..."`, `"1."`, `".1"`, `"1.2.3.4.5.6.7.8.9"`, etc. The comparison in `play()` (line 1058: `cdn_ip_prefix != exit_ip_prefix`) would then compare these malformed strings against the real exit IP prefix, potentially causing false negatives (mismatch when IPs actually match) or false positives (match when they don't).
- **Impact:** Low — in practice, CDN URLs contain well-formed IP prefixes like `192.42`. But a malformed CDN URL could bypass the check or cause spurious re-resolves.
- **Recommendation:** Validate the format more strictly:
  ```rust
  let parts: Vec<&str> = value.split('.').collect();
  if parts.len() == 2 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) {
      return Some(value.to_string());
  }
  ```

#### BUG-003: CDN exit IP prefix extraction doesn't handle IPv6 or malformed IPs
- **Severity:** Low
- **Location:** Lines 1053–1056 (`play()` — `exit_ip.split('.').next().unwrap_or("")` and `.nth(1).unwrap_or("")`)
- **Description:** The exit IP prefix is extracted by splitting on `.` and taking the first two octets. If `check_exit_ip()` returns an IPv6 address (e.g., `2001:db8::1`), the split on `.` returns a single element, and `nth(1)` returns `None` → `unwrap_or("")`. The resulting `exit_ip_prefix` would be `"2001:db8::1."` (first element + empty string), which would never match a CDN IP prefix like `"192.42"`, causing a spurious mismatch error.
- **Impact:** Low — Tor exit IPs are overwhelmingly IPv4. But if an IPv6 exit is used, every cast would fail with a CDN IP mismatch error.
- **Recommendation:** Check that the exit IP is IPv4 before extracting the prefix:
  ```rust
  let octets: Vec<&str> = exit_ip.split('.').collect();
  if octets.len() != 4 {
      tracing::warn!(exit_ip = %exit_ip, "non-IPv4 exit IP — skipping CDN IP check");
      // Skip the check rather than risk a false mismatch
  } else {
      let exit_ip_prefix = format!("{}.{}", octets[0], octets[1]);
      // ... compare
  }
  ```

#### BUG-004: 200ms hardcoded sleep before pipeline reconstruction
- **Severity:** Low
- **Location:** Line 1021 (`play()` — `tokio::time::sleep(Duration::from_millis(200))`)
- **Description:** After stopping the existing pipeline and dropping it (line 1006–1010), the code sleeps 200ms to allow the kernel's vc4 DRM driver to complete cleanup. The comment explains this is needed because "even after GStreamer's set_state(Null) returns, the kernel may need a few milliseconds to complete cleanup." This is the same pattern as the 300ms sleep in `session/lib.rs` (flagged in review-04 BUG-001).
- **Impact:** Adds 200ms latency to every cast operation (on top of the 300ms from session/lib.rs, totaling 500ms). The sleep runs unconditionally, even on first cast (no prior pipeline to clean up).
- **Recommendation:** Only sleep when there WAS an existing pipeline. Better: rely on the display manager's internal retry loop (which already has exponential backoff) and remove the sleep entirely. At minimum:
  ```rust
  if had_existing_pipeline {
      tokio::time::sleep(Duration::from_millis(200)).await;
  }
  ```

#### BUG-005: Bus watch captures `config` by move — stale `audio_enabled` after runtime change
- **Severity:** Low
- **Location:** Line 1164 (`bus.add_watch(move |_bus, msg| { ... })` — the closure captures `config` which contains `audio_enabled`)
- **Description:** The bus watch closure captures `config` (the `PipelineConfig` struct) by move. Inside the closure, `config.audio_enabled` is checked in the `is_audio_cascade_not_negotiated` branch (line ~1300). If `audio_enabled` is changed at runtime after the pipeline is created (e.g. by a future `set_audio_enabled()` method or by the SW fallback logic), the bus watch still uses the value from pipeline creation time.
- **Impact:** Low — currently `audio_enabled` is set at pipeline construction time and doesn't change during the pipeline's lifetime. But the design is fragile.
- **Recommendation:** If `audio_enabled` needs to be dynamic, wrap it in `Arc<AtomicBool>` and share between the engine and the bus watch. If it's static for the pipeline's lifetime, add a comment noting that the captured value is immutable for the closure's lifetime.

### Security

#### SEC-001: CDN URL logged at info level may contain IP-bound tokens
- **Severity:** Low
- **Location:** Line 1059 (`tracing::info!(url = url, ...)` in `play()`)
- **Description:** The full CDN URL is logged at `info` level when constructing the playback pipeline. CDN URLs from resolvers like Voe contain IP-bound tokens (e.g. `&i=192.42&__token=abc123`) that are sensitive — they're bound to a specific Tor exit IP and could be used to trace or replay requests. The journald log on the Pi persists these tokens indefinitely (until log rotation).
- **Impact:** Low — the logs are on the appliance itself (not transmitted). But if the SD card is removed and read, the tokens are exposed. This partially contradicts the privacy-first goal.
- **Recommendation:** Log a redacted URL (strip query parameters) at `info` level, and log the full URL at `debug` level only:
  ```rust
  let redacted = url::Url::parse(url).ok().map(|u| u.as_str().split('?').next().unwrap_or(url)).unwrap_or(url);
  tracing::info!(url = redacted, socks = socks_addr, "constructing playback pipeline");
  ```

#### SEC-002: `extract_cdn_ip_prefix` doesn't handle URL-encoded `&i=`
- **Severity:** Low
- **Location:** Lines 778–779 (`for prefix in &["&i=", "?i="]`)
- **Description:** The function searches for literal `&i=` or `?i=` in the URL. If the URL contains URL-encoded equivalents (`%26i%3D`), the function returns `None`, skipping the CDN IP check entirely. This is not a vulnerability (skipping the check just means no proactive mismatch detection — the CDN will still return 403 if the IP doesn't match), but it's a correctness gap.
- **Impact:** Low — resolvers typically return decoded URLs. But a resolver that returns encoded URLs would silently bypass the proactive check.
- **Recommendation:** Use `url::Url::parse()` to properly parse the query string, or URL-decode the URL before searching. Low priority.

### Design Issues

#### DESIGN-001: `play()` method is ~400 lines — too complex to maintain
- **Severity:** Medium
- **Location:** Lines 991–1390+ (`play()` hw impl)
- **Description:** The `play()` method handles: stopping the existing pipeline, sleeping for DRM cleanup, proactive CDN IP check, pipeline construction, bus watch setup (a ~500-line closure), preroll, diagnostic task spawning, and SW fallback. This is one of the longest methods in the codebase and is extremely hard to read, test, or modify safely.
- **Impact:** High maintenance cost. Any change to the play flow risks breaking subtle invariants (e.g. the auto-play flag, the buffering thresholds, the stall detection). The method is also impossible to unit-test in isolation.
- **Recommendation:** Break into smaller methods:
  - `stop_existing_pipeline(&self) -> Result<(), PlaybackError>`
  - `check_cdn_ip_match(url, socks_addr, isolation_username) -> Result<(), PlaybackError>`
  - `setup_bus_watch(pipeline, config, flags) -> BusWatch`
  - `spawn_diagnostic_task(pipeline, flags)`
  - `try_play_with_fallback(url, source_url, ...) -> Result<(), PlaybackError>`
  Each method is independently testable and the `play()` method becomes a readable sequence of calls.

#### DESIGN-002: Bus watch closure is ~500 lines with 10+ captured variables
- **Severity:** Medium
- **Location:** Lines 1164–1650+ (the `bus.add_watch(move |_bus, msg| { ... })` closure)
- **Description:** The bus watch closure handles `StateChanged`, `Eos`, `Error`, `Buffering`, and `Warning` messages. It captures: `event_tx`, `is_playing`, `pending_auto_play`, `initial_buffering`, `last_buffering_percent`, `stall_start`, `pipeline_weak`, `progress_state`, `cdn_forbidden_emitted`, `config`, `rate_limited_resume_percent`, `rate_limited_pause_percent`. The `Error` handler alone is ~120 lines with 4 branches (CDN forbidden, audio sink error, audio cascade not-negotiated, generic error).
- **Impact:** The closure is a single point of failure for all pipeline event handling. It's impossible to test individual handlers in isolation. A bug in any branch affects all message processing.
- **Recommendation:** Extract each message handler into a separate function:
  ```rust
  fn handle_state_changed(msg, event_tx, is_playing, pending_auto_play, ...) -> ControlFlow
  fn handle_error(msg, event_tx, is_playing, config, ...) -> ControlFlow
  fn handle_buffering(msg, event_tx, initial_buffering, ...) -> ControlFlow
  fn handle_eos(msg, event_tx, is_playing) -> ControlFlow
  ```
  The bus watch becomes a thin dispatcher. Each handler is independently testable.

#### DESIGN-003: Dual hw/mock implementations double the maintenance burden
- **Severity:** Medium
- **Location:** Throughout (every method has `#[cfg(feature = "hw")]` and `#[cfg(not(feature = "hw"))]` variants)
- **Description:** Every public method (`play`, `pause`, `resume`, `stop`, `seek`, `set_volume`, `position_ms`, `duration_ms`) has two implementations. The `PlaybackEngine` struct has 4 hw fields and 15 mock fields. Any API change must be made in both places, and the two implementations can diverge silently (e.g. the mock `play()` emits a `PlaybackState` event, while the hw `play()` emits `PlaybackEvent` events via the bus watch).
- **Impact:** High — the mock and hw implementations have already diverged in event types (`PlaybackState` vs `PlaybackEvent`). Tests run against the mock, so they don't catch hw-specific bugs.
- **Recommendation:** For v2, consider a trait-based design:
  ```rust
  trait PlaybackBackend: Send + Sync {
      async fn play(&self, url: &str, ...) -> Result<(), PlaybackError>;
      async fn pause(&self) -> Result<(), PlaybackError>;
      // ...
  }
  struct HwBackend { /* GStreamer fields */ }
  struct MockBackend { /* mock fields */ }
  ```
  `PlaybackEngine` holds a `Box<dyn PlaybackBackend>` and delegates all calls. The bus watch and event mapping live in `HwBackend`. This eliminates `cfg` duplication and makes the mock a true drop-in.

#### DESIGN-004: `STALL_TIMEOUT_SECS` is hardcoded
- **Severity:** Low
- **Location:** Line 1140 (`const STALL_TIMEOUT_SECS: u64 = 30;`)
- **Description:** The stall detection timeout is a hardcoded constant inside the `play()` method. It can't be tuned without recompiling. On a slow Tor circuit, 30 seconds may be too short (causing unnecessary re-resolves). On a fast connection, 30 seconds is too long (the user sits through a frozen screen).
- **Impact:** Low — the 30s value is reasonable for most cases. But it should be tunable.
- **Recommendation:** Move to `PipelineConfig`:
  ```rust
  pub stall_timeout_secs: u64, // default 30
  ```

#### DESIGN-005: Diagnostic task can't be cancelled
- **Severity:** Low
- **Location:** Lines 1669–1750+ (`tokio::spawn(async move { ... tokio::time::sleep(Duration::from_secs(10)).await; ... })`)
- **Description:** After pipeline preroll, a diagnostic task is spawned that sleeps 10s then checks the pipeline state. If the pipeline is destroyed before 10s (e.g. user stops playback), the task still wakes up, tries to upgrade the weak reference (which fails), and silently exits. This wastes a tokio task slot and a wakeup.
- **Impact:** Low — the task is lightweight and the weak upgrade correctly returns `None`. But it's a minor resource leak pattern.
- **Recommendation:** Use a `tokio::select!` with a `CancellationToken` or a `tokio::time::timeout` on the pipeline's lifetime:
  ```rust
  let cancel = CancellationToken::new();
  // store cancel in the pipeline so it's aborted on drop
  tokio::spawn(async move {
      tokio::select! {
          _ = tokio::time::sleep(Duration::from_secs(10)) => { /* check state */ }
          _ = cancel.cancelled() => { /* pipeline destroyed, abort */ }
      }
  });
  ```

#### DESIGN-006: Three bus watch implementations (lines 1164, 2129, 2278)
- **Severity:** Low
- **Location:** Lines 1164, 2129, 2278 (three `bus.add_watch()` calls)
- **Description:** There are three separate bus watch closures in the file, presumably for different feature flag combinations or pipeline variants. Each is a ~200-500 line closure with its own set of captured variables. They share most of their logic but are copy-pasted rather than shared.
- **Impact:** Maintenance burden — a fix in one bus watch must be replicated in the others. The three variants can diverge.
- **Recommendation:** Extract the shared bus watch logic into a function that takes feature-specific parameters, and call it from each variant. Or consolidate into a single bus watch with feature-flagged branches inside.

### Missing Tests

#### TEST-001: Zero test coverage for the hw code path
- **Severity:** High
- **Description:** All 40 tests in the file (line 2900+) are compiled under `#[cfg(not(feature = "hw"))]` — they test the mock implementation exclusively. The hw code path (`play()`, `stop()`, `pause()`, `resume()`, `seek()`, the bus watch, the CDN IP check, the SW fallback, the stall detection) has zero test coverage. CI runs without the `hw` feature, so hw code is never exercised in automated tests.
- **Impact:** Critical — the most complex and important code (the real GStreamer pipeline) is completely untested. Regressions in the bus watch, the CDN IP check, or the SW fallback would not be caught until deployment to a Pi.
- **Recommendation:** (a) Add a `hw-mock` feature that compiles the hw code path but replaces GStreamer calls with mock implementations (via trait abstraction — see DESIGN-003). (b) Add a Pi 4 self-hosted CI runner (as noted in the blueprint progress doc) that runs `cargo test --features hw` nightly. (c) At minimum, extract the pure logic (CDN IP check, stall detection, negotiation error detection) into testable functions that don't require GStreamer.

#### TEST-002: No test for `extract_cdn_ip_prefix`
- **Severity:** Medium
- **Description:** The `extract_cdn_ip_prefix` function (line 775) is pure logic (no GStreamer dependency) but has no test. It's `#[cfg(feature = "hw")]` so it's not compiled in CI.
- **Impact:** The function parses untrusted CDN URLs — a bug could cause false CDN IP mismatch errors or bypass the check.
- **Recommendation:** Move the function to a non-`cfg` module and add tests:
  ```rust
  #[test]
  fn test_extract_cdn_ip_prefix() {
      assert_eq!(extract_cdn_ip_prefix("https://cdn.example.com/v.mp4?i=192.42&token=abc"), Some("192.42".into()));
      assert_eq!(extract_cdn_ip_prefix("https://cdn.example.com/v.mp4&i=10.20"), Some("10.20".into()));
      assert_eq!(extract_cdn_ip_prefix("https://cdn.example.com/v.mp4"), None);
      assert_eq!(extract_cdn_ip_prefix("https://cdn.example.com/v.mp4?i=abc"), None); // non-numeric
  }
  ```

#### TEST-003: No test for `is_negotiation_error`
- **Severity:** Medium
- **Description:** The `is_negotiation_error` function (line 748) determines whether to trigger SW fallback. It's pure string matching but has no test.
- **Impact:** A change to the matching patterns could silently break the SW fallback trigger.
- **Recommendation:** Move to a non-`cfg` module and add tests for each pattern: "not negotiated", "negotiation", "not-negotiated", "v4l2h264dec", "no common format", "could not link".

#### TEST-004: No test for the CDN IP mismatch detection logic
- **Severity:** Medium
- **Description:** The proactive CDN IP check in `play()` (lines 1029–1075) compares the Tor exit IP with the CDN URL's `&i=` parameter. This logic is untested.
- **Impact:** A bug in the comparison (e.g. BUG-003 with IPv6) would cause spurious mismatches or missed matches.
- **Recommendation:** Extract the comparison into a testable function: `check_cdn_ip_match(exit_ip: &str, cdn_url: &str) -> Result<(), PlaybackError>` and test with various IP/URL combinations.

#### TEST-005: No test for the stall detection logic
- **Severity:** Low
- **Description:** The stall detection in the bus watch (lines 1140, 1560–1580) tracks how long the buffer has been at 0% and emits an error after 30 seconds. This logic is embedded in the bus watch closure and untestable.
- **Impact:** A bug in the stall timer could cause false stalls (interrupting playback) or missed stalls (leaving the user with a frozen screen).
- **Recommendation:** Extract the stall detection into a `StallDetector` struct with a `update(percent: u8) -> Option<StallError>` method. Test with simulated buffering sequences.

#### TEST-006: Mock tests don't verify event ordering
- **Severity:** Low
- **Description:** The 40 mock tests check state transitions and individual events, but none verify the ORDER of events emitted during `play()`. For example, `play()` should emit `Playing` after `Buffering` completes, but no test verifies this sequence.
- **Impact:** An event ordering regression (e.g. emitting `Playing` before `Buffering`) could confuse subscribers without being caught.
- **Recommendation:** Add a test that collects all events from `engine.events()` during a mock `play()` call and asserts the order: `[Buffering, Playing]` (or whatever the correct sequence is).

## Positive Observations

1. **Excellent documentation of the CDN IP-binding invariant** — the comment block at lines 1029–1045 explaining why the Tor exit IP must match the CDN URL's `&i=` parameter is one of the best security-critical comments in the codebase. It explains the failure mode (403 Forbidden), the root cause (circuit rotation), and the fix (proactive check + re-resolve).

2. **Audio cascade not-negotiated detection** — the `is_audio_cascade_not_negotiated` branch (lines ~1300–1320) is a sophisticated piece of error handling. It recognizes that a `not-negotiated` error from `appsrc` is often a cascading failure caused by the audio device being unavailable, and emits an `AudioDeviceError` event so the pipeline can be retried with audio disabled. This is exactly the right behavior for a consumer device.

3. **Rate-limited buffering thresholds** — the bus watch uses different buffering thresholds for rate-limited streams (95% resume / 5% pause) vs normal streams (80% resume / 10% pause). This is a thoughtful optimization for Tor-routed streams with variable throughput.

4. **SW decode fallback** — the automatic fallback from V4L2 hardware decode to software decode on `is_negotiation_error` is a good resilience pattern. It ensures playback continues even when the hardware decoder rejects the stream's caps.

5. **GLib main loop setup** — the `OnceLock`-based GLib main loop initialization (lines 893–920) correctly ensures the main loop runs exactly once in a background thread, and the comment clearly explains why it's needed (without it, bus watch callbacks are silently dropped).

6. **Buffering log rate-limiting** — the `last_buffering_percent` AtomicU8 (lines 1135–1137) prevents log spam during rapid buffer oscillation, which was a real problem on the Pi (hundreds of log lines per second). The rate-limiting logic (only log when percent changes by ≥5% or crosses key thresholds) is well-calibrated.

7. **Weak reference for diagnostic task** — the diagnostic task uses a `downgrade()` weak reference to the pipeline, so it doesn't prevent pipeline destruction and correctly handles the case where the pipeline is gone before the 10s check.

8. **Comprehensive mock mode** — the 15 mock fields and parallel method implementations allow the entire session layer to be tested without GStreamer hardware. While the dual implementation is a maintenance burden (DESIGN-003), the mock itself is thorough.

9. **`Drop` cleanup** — the pipeline's `Drop` impl (referenced in the comment at line 1006) sets bus_watch to None, sets state to Null, and cancels the appsrc push task. This ensures resources are released even if `stop()` isn't called explicitly.

10. **V4L2 latency compensation** — the `audio_ts_offset_ns` config field (lines 224–242) compensates for the ~100-160ms hardware decode latency that GStreamer's latency query under-reports. The comment thoroughly explains the root cause (V4L2 stateful decoder buffering + ISP conversion) and the fix (positive `ts-offset` on the audio sink).

## Recommendations Summary

| Priority | Finding | Effort |
|----------|---------|--------|
| High | TEST-001: Add hw code path test coverage (trait extraction + Pi CI) | L (8–16 h) |
| Medium | BUG-001: Fix `stop()` to clear state even on pipeline teardown failure | S (30 min) |
| Medium | DESIGN-001: Break `play()` into smaller methods | L (4–8 h) |
| Medium | DESIGN-002: Extract bus watch handlers into testable functions | L (4–8 h) |
| Medium | DESIGN-003: Trait-based backend to eliminate hw/mock duplication | L (8–16 h, v2) |
| Medium | TEST-002: Add `extract_cdn_ip_prefix` tests | S (30 min) |
| Medium | TEST-003: Add `is_negotiation_error` tests | S (30 min) |
| Medium | TEST-004: Add CDN IP mismatch detection tests | S (1 h) |
| Low | BUG-002: Validate IP prefix format more strictly | S (15 min) |
| Low | BUG-003: Handle IPv6 exit IPs in CDN IP check | S (30 min) |
| Low | BUG-004: Skip 200ms sleep when no prior pipeline existed | S (15 min) |
| Low | BUG-005: Document `config` capture lifetime in bus watch | S (15 min) |
| Low | SEC-001: Redact CDN URL tokens in info-level logs | S (30 min) |
| Low | SEC-002: Handle URL-encoded `&i=` in `extract_cdn_ip_prefix` | S (30 min) |
| Low | DESIGN-004: Make `STALL_TIMEOUT_SECS` configurable | S (15 min) |
| Low | DESIGN-005: Make diagnostic task cancellable | S (1 h) |
| Low | DESIGN-006: Consolidate three bus watch implementations | M (2–4 h) |
| Low | TEST-005: Extract and test stall detection logic | M (2 h) |
| Low | TEST-006: Add event ordering tests for mock play() | S (1 h) |
