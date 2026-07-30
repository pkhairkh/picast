---
doc: code_review
project: picast
version: 1
phase: code_review
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# Code Review: `src/display/src/lib.rs`

**File:** `src/display/src/lib.rs`
**Lines:** 2077
**Reviewer:** agent
**Date:** 2026-07-30

## Summary

The display manager provides direct DRM/KMS mode-setting for the Raspberry Pi, bypassing X11/Wayland entirely. It acquires DRM master, enumerates connectors/CRTCs/planes, configures atomic modesetting, and manages GBM buffer allocation for zero-copy video scanout. The implementation has two code paths: a `hw` feature path (real DRM) and a mock path (for x86_64 development and CI). This is a hardware-critical module — bugs here cause black screens, DRM master contention, or kernel panics. The code is well-structured with thorough error handling, excellent retry logic for DRM master acquisition, and 41 tests (though mostly for the mock path). However, there are several issues.

## Key Components Reviewed

| Component | Lines | Purpose |
|-----------|-------|---------|
| `fourcc` module | 60–100 | DRM pixel format constants (XR24, NV12, etc.) |
| `DrmPlane` struct | 172–210 | Plane metadata + format support queries |
| `DisplayMode` struct | 234–310 | Mode info + preference sorting (1080p60 preferred) |
| `DisplayConnector` struct | 308–360 | Connector info + best-mode selection |
| `DisplayManager` (hw) | 364–1130 | Real DRM: acquire, release, modeset, GBM |
| `DisplayManager` (mock) | 1330–1520 | No-op implementation for non-hw builds |
| `acquire()` | 585–720 | DRM master acquisition with 10-attempt exponential backoff |
| `release()` | 968–1060 | CRTC state restoration + DRM master release |

## Findings

### Bugs

#### BUG-001: `acquire()` uses `std::thread::sleep` in an async context
- **Severity:** Medium
- **Location:** Lines 640–645 (`acquire()` retry loop)
- **Description:** The `acquire()` method uses `std::thread::sleep` for the exponential backoff between DRM master acquisition attempts. While `acquire()` is not an `async fn` (it's a synchronous `pub fn`), it's called from async code (`session.load()` calls `display.acquire().await` via the trait). The `std::thread::sleep` blocks the entire Tokio runtime thread for up to 2 seconds per retry, potentially blocking other tasks.
- **Impact:** During DRM master contention (e.g., gmediarender just exited), the entire Tokio runtime stalls for up to 20 seconds (10 attempts × up to 2s each). HTTP requests, WebSocket events, and Tor monitoring all freeze during this window.
- **Recommendation:** Make `acquire()` an `async fn` and use `tokio::time::sleep` instead. This requires changing the `DisplayTrait` to be async, but it's the correct approach. Alternatively, use `tokio::task::spawn_blocking` to run the synchronous `acquire()` on a blocking thread pool.

#### BUG-002: `acquire()` closes the DRM fd to let kmssink become the first opener, but this is undocumented in the API
- **Severity:** Low
- **Location:** Lines 585–600 (idempotent check + re-open logic)
- **Description:** After the first `acquire()`, the DRM fd is closed so that `kmssink` can open it fresh and become the DRM master automatically. A second `acquire()` call re-opens the device. This behavior is noted in comments but not documented in the public API. Callers who hold a reference to the `DisplayManager` after `acquire()` may be surprised that `drm_fd()` returns `None`.
- **Impact:** Callers that check `drm_fd()` after `acquire()` will see `None`, which could cause confusion or null-pointer-like errors.
- **Recommendation:** Document this in the `acquire()` doc comment: "After acquisition, the DRM fd is closed to allow kmssink to become the DRM master. Subsequent calls to `drm_fd()` will return `None` until `release()` is called."

#### BUG-003: `release()` may skip CRTC restoration if DRM master can't be re-acquired
- **Severity:** Medium
- **Location:** Lines 990–1000 (`release()` master re-acquisition)
- **Description:** In `release()`, if the DRM fd was closed (after `acquire()`), it's re-opened. Then `acquire_master_lock()` is attempted. If it fails (e.g., kmssink still holds master), the method logs a warning and skips CRTC restoration entirely — the `saved_crtc` state is never restored.
- **Impact:** The display may be left in a non-standard mode (e.g., 1080p60 when the user's TV was set to 720p). On the next playback, this could cause a mode switch or a black screen.
- **Recommendation:** Retry master acquisition with a short backoff (like `acquire()` does). If it truly can't be acquired after N attempts, at least log the saved CRTC state so an administrator can manually restore it. Consider using `drmDropMaster` on the kmssink fd if the process has the capability.

#### BUG-004: Mock `DisplayManager` doesn't simulate the fd-closing behavior of the hw version
- **Severity:** Low
- **Location:** Lines 1446–1460 (mock `acquire()`)
- **Description:** The mock `acquire()` simply sets `active_crtc` to a dummy value and returns `Ok(())`. It doesn't simulate the hw version's behavior of closing the DRM fd after acquisition. Code that depends on the fd being `None` after `acquire()` will behave differently between mock and hw builds.
- **Impact:** Tests using the mock may pass but fail on real hardware.
- **Recommendation:** If the fd-closing behavior is part of the contract, the mock should simulate it. If it's an implementation detail, document it as such.

### Design Issues

#### DESIGN-001: Two separate `DisplayManager` structs (hw and mock) with no shared trait
- **Severity:** Medium
- **Location:** Lines 364 (hw `DisplayManager`) and 1330 (mock `DisplayManager`)
- **Description:** The file has two `DisplayManager` structs — one behind `#[cfg(feature = "hw")]` and one without. They have the same public API but are completely separate implementations with no shared trait. The `DisplayTrait` in `session/interfaces.rs` abstracts over them, but the two implementations can diverge.
- **Impact:** A method added to the hw `DisplayManager` but not the mock (or vice versa) won't be caught at compile time if the developer only tests with one configuration.
- **Recommendation:** Consider a single `DisplayManager` struct with `Option`-typed fields for the hw-specific state, or extract the common API into a trait that both implementations must satisfy. At minimum, add a CI check that compiles both configurations.

#### DESIGN-002: DRM master acquisition is "best effort" — proceeds without master on failure
- **Severity:** Medium
- **Location:** Lines 670–680 (acquire() final warning)
- **Description:** After 10 failed attempts to acquire DRM master, the method logs a warning and proceeds anyway ("kmssink will try during playback"). This is documented as intentional, but it means `acquire()` can return `Ok(())` even when the display is not properly configured.
- **Impact:** The session layer thinks the display is ready, but playback may fail with a confusing GStreamer error when kmssink can't acquire master either.
- **Recommendation:** Return a `DisplayError::MasterAcquire` on failure, and let the session layer decide whether to retry or abort. The "proceed anyway" behavior can be an explicit `acquire_best_effort()` method for cases where the caller accepts the risk.

#### DESIGN-003: `DisplayMode::preference_cmp` hardcodes 1080p60 as the best mode
- **Severity:** Low
- **Location:** Lines 284–295 (`preference_cmp`)
- **Description:** The mode preference comparator hardcodes 1080p60 as the highest priority, then sorts by resolution and refresh rate. This doesn't account for: (a) the TV's native resolution (some TVs report 1080p but are natively 720p), (b) the Pi's HDMI port limitations (Pi 4 can do 4K30 but not 4K60), or (c) user preferences.
- **Impact:** On a TV that doesn't support 1080p60, the display manager will try to set a mode the TV can't handle, resulting in a black screen.
- **Recommendation:** Prefer the connector's preferred mode (from the EDID) over hardcoded preferences. Fall back to 1080p60 only if no preferred mode is available. Add a config option for the user to override the mode.

#### DESIGN-004: No test for the hw code path
- **Severity:** Medium
- **Location:** Tests section (lines 1528+)
- **Description:** All 41 tests are for the mock code path or pure functions (`fourcc`, `DisplayMode`, `DrmPlane`). The hw `DisplayManager::acquire()`, `release()`, and modesetting code have no tests — they can only be tested on a Pi with DRM hardware.
- **Impact:** The most critical code (DRM master acquisition, CRTC restoration) is untested. Regressions can only be caught by manual Pi testing.
- **Recommendation:** Add hardware-in-the-loop tests in `tests/hw_display.rs` (behind a `hw` feature gate) that run on a Pi CI runner. Test: acquire master, enumerate connectors, set a mode, release, and verify CRTC restoration. The spec (§9.2, Test Strategy) mentions this but it's not implemented.

### Security

#### SEC-001: DRM device path not validated
- **Severity:** Low
- **Location:** Line 459 (`DisplayManager::new`, `OpenOptions::new().open(&self.device_path)`)
- **Description:** The `device_path` is passed directly to `OpenOptions::open()` without validation. If the path is attacker-controlled, any file could be opened (though DRM ioctls would fail on non-DRM devices).
- **Impact:** Low — the config file is root-owned and the path is typically `/dev/dri/card0`. But defense-in-depth.
- **Recommendation:** Validate that the path starts with `/dev/dri/` or is in an allowed list (`/dev/dri/card0`, `/dev/dri/card1`).

#### SEC-002: `drm_fd()` exposes the raw file descriptor
- **Severity:** Low
- **Location:** Line 1111 (`drm_fd() -> Option<i32>`)
- **Description:** The `drm_fd()` method returns the raw DRM file descriptor as an `i32`. This could be used by callers to perform DRM operations directly, bypassing the `DisplayManager`'s invariants.
- **Impact:** Low — the fd is only used by `kmssink` setup. But exposing raw fds is a potential safety issue.
- **Recommendation:** Return a `BorrowedFd` instead of `i32`, or remove the method and pass the fd to `kmssink` setup internally.

### Missing Tests

#### TEST-001: No hw-path tests (see DESIGN-004)
- **Severity:** Medium
- **Description:** The hw `DisplayManager` has no tests. All 41 tests are for the mock path or pure functions.
- **Recommendation:** Add `tests/hw_display.rs` with `#[cfg(feature = "hw")]` tests that run on a Pi CI runner.

#### TEST-002: No test for DRM master retry logic
- **Severity:** Low
- **Description:** The 10-attempt exponential backoff in `acquire()` is not tested. The retry timing and the "proceed without master" fallback are untested.
- **Recommendation:** This is hard to test without mocking the DRM ioctl layer. Consider extracting the retry logic into a testable function that takes a closure for the master acquisition attempt.

#### TEST-003: No test for CRTC restoration in `release()`
- **Severity:** Low
- **Description:** The `release()` method's CRTC restoration logic (including the failure paths where master can't be re-acquired) is not tested.
- **Recommendation:** Same as TEST-002 — requires mocking the DRM layer.

## Positive Observations

1. **Excellent DRM master retry logic** — the 10-attempt exponential backoff (200ms → 2s cap) handles both the "gmediarender just exited" and "fbcon holds master" scenarios. The comments explain *why* each scenario occurs and *how* the systemd unit mitigates the fbcon case.
2. **Idempotent `acquire()`** — calling `acquire()` twice doesn't fail; it returns `Ok(())` immediately if already acquired. This is important because the session layer may call it defensively.
3. **CRTC state restoration** — `release()` saves and restores the original CRTC state, so the console isn't left in a weird mode after playback stops.
4. **Mock implementation for x86_64 dev** — the `#[cfg(not(feature = "hw"))]` path allows development and CI on non-Pi hardware, which is essential for a volunteer project.
5. **Comprehensive fourcc support** — the `fourcc` module covers XRGB8888, ARGB8888, NV12, NV21, P030, RGB565, YUYV — all the formats the Pi's HVS can handle.
6. **Mode preference sorting** — `preference_cmp` sorts modes by a sensible priority (1080p60 first, then by resolution/refresh).
7. **Driver verification** — `verify_driver()` allows the caller to assert the expected DRM driver (e.g., `vc4`), catching misconfiguration early.
8. **41 tests** — good coverage of the mock path and pure functions, including mode sorting, format detection, and connector selection.
9. **`clear_screen()` and `allocate_osd_surface()`** — OSD support is included, addressing the "no compositor means no OSD" concern from ADR-011.
10. **Detailed error types** — `DisplayError` has specific variants for each failure mode (DeviceOpen, MasterAcquire, WrongDriver, Modeset, NoCrtc, NoConnector, NoPlane, etc.).

## Recommendations Summary

| Priority | Finding | Effort |
|----------|---------|--------|
| Medium | BUG-001: Use `tokio::time::sleep` instead of `std::thread::sleep` | M (2–3 h) |
| Medium | BUG-003: Retry CRTC restoration on master acquisition failure | M (2 h) |
| Medium | DESIGN-001: Unify hw/mock DisplayManager or add CI check | L (4–8 h) |
| Medium | DESIGN-002: Return error on master acquisition failure, don't proceed | S (1–2 h) |
| Medium | DESIGN-004/TEST-001: Add hw-path tests on Pi CI runner | L (4–8 h) |
| Low | BUG-002: Document fd-closing behavior in acquire() API | S (15 min) |
| Low | BUG-004: Make mock simulate fd-closing behavior | S (30 min) |
| Low | DESIGN-003: Prefer EDID preferred mode over hardcoded 1080p60 | M (2–3 h) |
| Low | SEC-001: Validate DRM device path | S (30 min) |
| Low | SEC-002: Return BorrowedFd instead of raw i32 | S (1 h) |
| Low | TEST-002: Test DRM master retry logic | M (2–3 h, requires mocking) |
| Low | TEST-003: Test CRTC restoration in release() | M (2–3 h, requires mocking) |
