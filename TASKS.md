# boGDan Sprint-Based Task Breakdown

**Sprint-organized, actionable tasks derived from [ROADMAP.md](ROADMAP.md) and
[ARCHITECTURE.md](ARCHITECTURE.md). Each sprint has a Definition of Done (DoD),
explicit acceptance criteria, and estimated effort.**

**Status legend:** `[ ]` not started · `[~]` in progress · `[x]` done

**Last verified:** 2026-05-10 — `cargo check --workspace` · `cargo test --workspace` (328 resolver tests + others) · `cargo clippy --workspace -- -D warnings`

---

## Completed Work Summary

The following phases from the original task breakdown are fully implemented:

| Phase | Description | Status |
|-------|-------------|--------|
| 0 | Build & CI Foundation (T-0.1 through T-0.5) | Done |
| 1 | Tor Daemon Integration (T-1.1 through T-1.6) | Done |
| 2 | DRM/KMS Display Manager — mock mode only (T-2.6) | Partial |
| 3 | GStreamer Playback Engine (T-3.1, T-3.4 through T-3.10) | Partial |
| 4 | Content Resolver / yt-dlp (T-4.1 through T-4.9) | Done |
| 5 | Session Manager (T-5.1 through T-5.7) | Done |
| 6 | Protocol Servers (T-6.1 through T-6.9) | Done |
| 7 | Server Orchestration — init + config + health (T-7.1 through T-7.6) | Partial |

**Additionally implemented but not tracked in the original task list:**

- Custom Voe resolver with multi-method deobfuscation (`custom.rs`, 1908 lines)
- Custom DoodStream resolver (`custom.rs`)
- SOCKS5 forwarder for Tor circuit isolation (`socks_forwarder.rs`, 557 lines)
- Progressive download via StreamSource + appsrc (`stream_source.rs`, 1280 lines)
- HLS segment download and parsing
- CDN preflight check (GET Range: bytes=0-0)
- CDN speed-limit detection (sp= parameter)
- Bait source / decoy detection
- Dynamic pipeline construction with parsebin (replacing fixed pipeline)
- Cookie forwarding from resolver session to CDN download
- ResolverSocksForwarder (separate from playback SOCKS forwarder)
- Browser-like User-Agent and headers throughout

---

## Sprint 1 — Provider Extraction & Resolver Architecture ✅

**Duration:** 2 weeks (10 working days)
**Goal:** Make the custom resolver system config-driven and pluggable, eliminating
VOE/DoodStream-specific hardcoded logic from the resolver core.
**Status:** **COMPLETE** — committed as `dbb2a53`, pushed to main.

**Definition of Done (DoD):**
- All VOE-specific constants and domain lists moved out of Rust source into
  a TOML provider configuration file
- A `ProviderConfig` struct deserializes from TOML with provider name, domain
  patterns, deobfuscation pipeline steps, and URL extraction rules
- A `DeobfuscationPipeline` trait defines the interface for pluggable
  deobfuscation strategies (ROT13, Base64, char-shift, reverse, marker-strip)
- The existing Voe and DoodStream resolvers are refactored to use the
  pipeline trait; no functional regression
- `cargo test --workspace` passes; new unit tests for pipeline steps
- `cargo clippy --workspace -- -D warnings` clean
- Provider config file (`providers.d/voe.toml`, `providers.d/doodstream.toml`)
  validated at startup with clear error messages

**Sprint Acceptance Criteria:**
1. Adding a new video hosting provider requires ONLY a new `.toml` file under
   `providers.d/` — no Rust code changes for providers that use existing
   deobfuscation primitives
2. Removing `providers.d/voe.toml` causes the Voe resolver to be unavailable,
   but the system still starts and all other providers work
3. The deobfuscation pipeline steps are individually unit-tested with known
   input/output pairs
4. Domain pattern matching supports exact match, suffix match, and regex

### S1.1 Define ProviderConfig TOML schema

- **Crate:** `bogdan-resolver`
- **Depends on:** nothing
- **Effort:** 1.5 days
- **Description:** Design and implement a `ProviderConfig` struct that
  deserializes from TOML. Each provider config specifies:
  - `name`: human-readable provider name
  - `enabled`: bool (default true)
  - `domain_patterns`: list of exact, suffix, and regex patterns
  - `resolver_type`: "custom" | "yt-dlp" | "passthrough"
  - `deobfuscation_pipeline`: ordered list of steps, each with type and params
  - `url_extraction`: rules for extracting media URL from deobfuscated data
    (key priority list, quality preference, CDN token handling)
  - `cookies`: whether to forward cookies from page fetch to download
  - `request_headers`: custom headers for page fetches
  - `timeout_secs`: request timeout
- **Acceptance:** `ProviderConfig::load("providers.d/voe.toml")` returns a
  validated config; invalid TOML returns clear error messages.
- **Key steps:**
  1. Define `ProviderConfig` struct with serde derive
  2. Define `DeobfuscationStep` enum: Rot13, StripMarkers, Base64Decode,
     CharShift { amount: i32 }, Reverse, JsonParse, RegexExtract { pattern: String }
  3. Define `UrlExtractionRule` enum: JsonKey { key: String, priority: u32 },
     RegexUrl { pattern: String }
  4. Implement `validate()` method that checks required fields, regex validity
  5. Write unit tests for serialization/deserialization round-trip
  6. Write tests for validation errors on missing/invalid fields

### S1.2 Implement DeobfuscationPipeline trait

- **Crate:** `bogdan-resolver`
- **Depends on:** S1.1
- **Effort:** 2 days
- **Description:** Define and implement a `DeobfuscationPipeline` trait that
  takes raw obfuscated input and returns a deobfuscated string. Implement
  concrete step types for each deobfuscation primitive currently used by
  the Voe and DoodStream resolvers:
  - `Rot13Step`: applies ROT13 substitution
  - `StripMarkersStep`: removes marker patterns (configurable regex list)
  - `Base64DecodeStep`: standard Base64 decode
  - `CharShiftStep`: shifts ASCII characters by a configurable amount
  - `ReverseStep`: reverses the string
  - `JsonParseStep`: parses JSON and extracts a key
- **Acceptance:** Each step has at least 2 unit tests with known input/output.
  A pipeline composed of multiple steps correctly chains their outputs.
- **Key steps:**
  1. Define `trait DeobfuscationStep { fn apply(&self, input: &str) -> Option<String>; }`
  2. Implement each concrete step as a struct with `DeobfuscationStep` impl
  3. Implement `DeobfuscationPipeline` struct that holds `Vec<Box<dyn DeobfuscationStep>>`
  4. `Pipeline::run(&self, input: &str) -> Option<String>` chains steps,
     returning None if any step fails
  5. Implement `From<&ProviderConfig>` for `DeobfuscationPipeline` that
     builds the pipeline from config step definitions
  6. Unit test each step with Voe's known deobfuscation chains:
     Method 8: ROT13 → strip → Base64 → char-shift(-3) → reverse → Base64
     Method 7: similar chain
     Method 6: similar chain

### S1.3 Extract Voe resolver to config-driven provider

- **Crate:** `bogdan-resolver`
- **Depends on:** S1.1, S1.2
- **Effort:** 3 days
- **Description:** Refactor `custom.rs::resolve_voe()` to use the
  DeobfuscationPipeline trait and ProviderConfig. Move all VOE-specific
  constants (VOE_DOMAINS, BAIT_DOMAINS, BAIT_FILENAMES, deobfuscation
  step sequences) into `providers.d/voe.toml`. The resolver reads the
  config at startup and builds the pipeline dynamically.
- **Acceptance:** Voe resolution still works after refactoring (verified by
  existing test patterns). `VOE_DOMAINS` no longer appears in `custom.rs`.
  Adding a domain to `voe.toml` makes it recognized without recompilation.
- **Key steps:**
  1. Create `providers.d/voe.toml` with Voe's 3 method pipelines
  2. Create `providers.d/doodstream.toml` with DoodStream config
  3. Refactor `resolve_voe()` to accept `&ProviderConfig` and build
     `DeobfuscationPipeline` from config
  4. Replace `VOE_DOMAINS` constant with config-driven domain matching
  5. Replace `is_voe_domain()` heuristic with config lookup + heuristic fallback
  6. Move `extract_media_from_json_value()` logic to config-driven URL
     extraction rules
  7. Move CDN speed-limit logic (`extract_cdn_speed_param`,
     `typical_bitrate_kbps`) into provider config with quality preference
  8. Verify: `cargo test -p bogdan-resolver` passes with same results

### S1.4 Extract DoodStream resolver to config-driven provider

- **Crate:** `bogdan-resolver`
- **Depends on:** S1.1, S1.2
- **Effort:** 2 days
- **Description:** Refactor `custom.rs::resolve_doodstream()` to use
  ProviderConfig. Move DoodStream-specific constants and logic into
  `providers.d/doodstream.toml`.
- **Acceptance:** DoodStream resolution still works after refactoring.
  `DOODSTREAM_DOMAINS` no longer appears in `custom.rs`.
- **Key steps:**
  1. Define DoodStream provider config with embed URL derivation rules
  2. Refactor `resolve_doodstream()` to use config-driven logic
  3. Move `derive_embed_url()` regex pattern into config
  4. Replace `DOODSTREAM_DOMAINS` with config lookup
  5. Verify: existing DoodStream tests pass

### S1.5 Provider registry and startup loading

- **Crate:** `bogdan-resolver`
- **Depends on:** S1.3, S1.4
- **Effort:** 1.5 days
- **Description:** Implement a `ProviderRegistry` that loads all `.toml`
  files from `providers.d/` at startup, validates them, and builds
  resolver instances. The registry provides a method to find the matching
  provider for a given URL (domain matching). Integrate with the existing
  `Resolver` struct.
- **Acceptance:** On startup, the resolver logs loaded providers. An unknown
  domain falls through to yt-dlp. A known domain is routed to the correct
  provider. Invalid provider TOML produces a clear startup error.
- **Key steps:**
  1. Implement `ProviderRegistry::load_from_dir(path) -> Result<Self>`
  2. `ProviderRegistry::find_provider(&self, url: &Url) -> Option<&ProviderConfig>`
  3. Integrate into `Resolver::resolve()` dispatch logic
  4. Add startup log: "Loaded N providers: voe, doodstream, ..."
  5. Test: config with duplicate provider names → error
  6. Test: config with invalid regex → error with filename and line

---

## Sprint 2 — Resolver Testing & CDN Resilience ✅

**Duration:** 2 weeks (10 working days)
**Goal:** Comprehensive testing for custom resolvers, CDN preflight/retry
logic, and error handling hardening.
**Status:** **COMPLETE** — committed, pushed to main.

**Definition of Done (DoD):**
- Every deobfuscation step has unit tests with known input/output pairs
- Voe and DoodStream resolvers have integration tests using mock HTTP servers
- CDN preflight check has tests for 403, timeout, and success cases
- Retry logic for CDN failures (re-resolve on 403) is tested
- Cache integration for custom resolver results works
- `cargo test --workspace` passes with new tests added
- No `unwrap()` or `expect()` in production resolver paths that could panic

**Sprint Acceptance Criteria:**
1. Custom resolver code has >80% line coverage
2. A CDN 403 during download triggers automatic re-resolution with a new
   Tor circuit (different isolation username)
3. Mock HTTP server tests verify Voe Method 6, 7, 8 deobfuscation
4. Cache stores and returns custom resolver results correctly

### S2.1 Voe deobfuscation unit tests

- **Crate:** `bogdan-resolver`
- **Depends on:** S1.2
- **Effort:** 2 days
- **Description:** Write comprehensive unit tests for each Voe deobfuscation
  method. Use real obfuscated samples (captured from actual Voe pages,
  anonymized) as test inputs. Test each step individually and the full
  pipeline as a chain.
- **Acceptance:** Each method (6, 7, 8) has at least one test with real
  obfuscated input that produces a valid media URL. Edge cases (empty
  input, invalid Base64, missing JSON keys) return None without panicking.
- **Key steps:**
  1. Capture 3-5 real Voe page samples and extract the obfuscated blobs
  2. Write `test_method8_deobfuscation()` with known input → expected output
  3. Write `test_method7_deobfuscation()` with known input → expected output
  4. Write `test_method6_deobfuscation()` with known input → expected output
  5. Write edge case tests: empty string, invalid Base64, truncated JSON
  6. Write test for `is_voe_domain()` heuristic: real Voe domains pass,
     well-known domains reject, .com heuristic catches plausible domains

### S2.2 DoodStream resolver unit tests

- **Crate:** `bogdan-resolver`
- **Depends on:** S1.4
- **Effort:** 1.5 days
- **Description:** Write unit tests for DoodStream-specific logic:
  embed URL derivation, download token extraction, media URL construction.
- **Acceptance:** `derive_embed_url()` correctly transforms /d/ → /e/ URLs.
  Media URL construction from download tokens produces valid URLs.
- **Key steps:**
  1. Test `derive_embed_url()`: various /d/ URL formats → correct /e/ URLs
  2. Test `is_doodstream_domain()`: all known domains, subdomains, unknowns
  3. Test bait source detection with DoodStream bait patterns
  4. Test error paths: 403 on main page, 403 on embed page

### S2.3 Mock HTTP server integration tests

- **Crate:** `bogdan-resolver`
- **Depends on:** S2.1, S2.2
- **Effort:** 3 days
- **Description:** Set up a mock HTTP server (using `mockito` or `wiremock`)
  that simulates Voe and DoodStream pages. Write integration tests that
  hit the mock server instead of real sites, verifying the full resolve
  flow from URL to ResolveResult.
- **Acceptance:** `resolve_voe()` and `resolve_doodstream()` work against
  mock servers. No real network requests in test suite. Tests run in CI.
- **Key steps:**
  1. Add `mockito` dev-dependency to `bogdan-resolver/Cargo.toml`
  2. Create mock Voe page: HTML with obfuscated JSON blob (Method 8)
  3. Create mock DoodStream page: HTML with embed iframe
  4. Test: `resolve_voe(mock_url)` returns correct `ResolveResult`
  5. Test: `resolve_doodstream(mock_url)` returns correct `ResolveResult`
  6. Test: JS redirect following (Voe → front-end domain)
  7. Test: Cookie forwarding from page fetch to resolve result
  8. Test: 404/403 responses return `ResolveError::NoMediaFound`

### S2.4 CDN preflight and retry logic

- **Crate:** `bogdan-playback`
- **Depends on:** nothing
- **Effort:** 2 days
- **Description:** Harden the CDN preflight check and implement automatic
  re-resolution on CDN 403. When StreamSource's preflight returns 403
  (CDN IP-bound token mismatch with Tor exit), the system should:
  1. Generate a new stream isolation username (different Tor circuit)
  2. Re-resolve the URL through the new circuit
  3. Retry the CDN preflight with the new direct URL
  4. Give up after 3 attempts and return a clear error
- **Acceptance:** CDN 403 during preflight triggers re-resolution with a
  new circuit. After 3 failures, a clear error is returned to the user.
  Success on retry works correctly.
- **Key steps:**
  1. Add `preflight_retry_count` to `StreamSource` config
  2. On 403: call back to session manager for re-resolution
  3. Generate new isolation username: `bogdan-{hash}-{attempt}`
  4. Restart SOCKS forwarder with new username
  5. Retry preflight with new circuit
  6. Log each attempt with Tor exit IP (if available from control port)
  7. After max retries: return `PlaybackError::CdnForbidden`

### S2.5 Cache integration for custom resolvers

- **Crate:** `bogdan-resolver`
- **Depends on:** S1.3
- **Effort:** 1.5 days
- **Description:** Ensure custom resolver results (Voe, DoodStream) are
  stored in and served from the SQLite cache. Currently the cache is only
  populated by yt-dlp results. Custom resolver results should be cached
  with the same TTL (10 minutes) and same fields.
- **Acceptance:** Second call to `resolve()` with the same Voe URL returns
  cached result without making any HTTP requests. Cache entry includes
  all ResolveResult fields (direct_url, cookies, content_length, etc.).
- **Key steps:**
  1. Verify `ResolveResult` from custom resolvers includes all cache fields
  2. Add `resolver_type` column to cache table: "ytdlp" | "custom" | "direct"
  3. On custom resolve: INSERT OR REPLACE into cache with resolver_type="custom"
  4. On cache hit for custom URL: verify TTL, return cached result
  5. Test: resolve Voe URL → cache miss → resolve → cache hit on second call

---

## Sprint 3 — DRM/KMS Display & Pi Hardware Bringup ✅

**Duration:** 2 weeks (10 working days)
**Goal:** Working DRM display on Raspberry Pi hardware — the last major
unimplemented subsystem.
**Status:** **COMPLETE** — committed, pushed to main.

**Definition of Done (DoD):**
- `bogdan-display` opens `/dev/dri/card0`, acquires DRM master, verifies
  vc4 driver, and enumerates real planes/CRTCs on Pi 4
- `acquire()` performs atomic modesetting at 1080p60 on HDMI
- `release()` cleanly disables planes and restores state
- GBM surface allocation for Plane 1 (OSD overlay) succeeds
- All display operations work on Pi 4 with vc4 driver
- Mock mode continues to work on x86 for CI
- `cargo test -p bogdan-display --features hw` passes on Pi 4

**Sprint Acceptance Criteria:**
1. On Pi 4: `DisplayManager::new("/dev/dri/card0")` succeeds
2. On Pi 4: `acquire()` shows a black frame at 1080p60 on HDMI
3. On Pi 4: `release()` restores the display to previous state
4. On x86: all existing tests continue to pass without `hw` feature
5. No other process can hold DRM master while boGDan is running

### S3.1 DRM device open and master acquisition

- **Crate:** `bogdan-display`
- **Depends on:** nothing (parallel with Sprint 2)
- **Effort:** 1.5 days
- **Description:** Implement real DRM device opening behind `#[cfg(feature = "hw")]`.
  Open `/dev/dri/card0`, call `drmSetMaster()`, verify vc4 driver. Fail
  with clear error messages if hardware is missing or another process
  holds DRM master.
- **Acceptance:** `DisplayManager::new("/dev/dri/card0")` succeeds on Pi 4
  with vc4 driver; fails with `DisplayError::DeviceOpen` on non-Pi hardware
  or when another compositor is running.
- **Key steps:**
  1. `#[cfg(feature = "hw")]` branch: open DRM device with `drm-rs`
  2. Acquire DRM master: `drmSetMaster(fd)`
  3. Query driver version: verify name == "vc4"
  4. If not vc4: return `DisplayError::DeviceOpen("Expected vc4 driver")`
  5. If DRM master unavailable: return `DisplayError::DeviceOpen("DRM master busy — is another compositor running?")`
  6. Store DRM device file descriptor in `DisplayManager`

### S3.2 Plane and CRTC enumeration

- **Crate:** `bogdan-display`
- **Depends on:** S3.1
- **Effort:** 1.5 days
- **Description:** Enumerate DRM planes and CRTCs on Pi 4. Record plane
  IDs, supported formats, Z-positions. Record CRTC IDs and current modes.
- **Acceptance:** `planes()` returns at least 2 planes (Plane 0 for video,
  Plane 1 for OSD); `crtcs()` returns at least 1 CRTC on Pi 4.
- **Key steps:**
  1. `drmModeGetResources()` → enumerate CRTCs and connectors
  2. `drmModeGetPlaneResources()` → enumerate planes
  3. For each plane: record `plane_id`, `formats` (DRM fourcc), `possible_crtcs`
  4. Get `zpos` property for each plane
  5. For each CRTC: record current mode (width, height, refresh)
  6. Validate: at least one plane supports NV12 (video), one supports ARGB8888 (OSD)

### S3.3 HDMI connector detection and mode selection

- **Crate:** `bogdan-display`
- **Depends on:** S3.1
- **Effort:** 1 day
- **Description:** Find the connected HDMI connector, read EDID, select
  preferred display mode (1080p60). Fall back to best available mode.
- **Acceptance:** `acquire()` selects 1080p60 on a standard HDMI monitor.
  Falls back to highest available resolution/refresh if 1080p60 unavailable.
- **Key steps:**
  1. Enumerate connectors, filter for `DRM_MODE_CONNECTED`
  2. Prefer HDMIA connector type
  3. From available modes: prefer 1920x1080 @ 60Hz
  4. Store selected connector ID and mode
  5. Log available modes for debugging

### S3.4 Atomic modesetting implementation

- **Crate:** `bogdan-display`
- **Depends on:** S3.2, S3.3
- **Effort:** 3 days
- **Description:** Implement `acquire()` using `drmModeAtomicCommit` to set
  the CRTC mode and enable Plane 0 (video). Implement `release()` to
  disable planes and restore previous state.
- **Acceptance:** After `acquire()`, HDMI output shows a black frame at
  1080p60. After `release()`, display returns to previous state (or off).
  No visual tearing during plane updates.
- **Key steps:**
  1. Save previous CRTC/connector state before modesetting
  2. Create atomic request: `drmModeAtomicAlloc()`
  3. Set CRTC properties: mode, active
  4. Set Plane 0 properties: CRTC_ID, SRC_*, CRTC_*, FB_ID
  5. Commit with `DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_PAGE_FLIP_EVENT`
  6. Wait for vblank event via `drmHandleEvent()`
  7. `release()`: restore saved state, release DRM master

### S3.5 GBM device and surface initialization

- **Crate:** `bogdan-display`
- **Depends on:** S3.1
- **Effort:** 2 days
- **Description:** Initialize GBM on the DRM device. Allocate a GBM surface
  for Plane 1 (OSD overlay) with ARGB8888 format. Verify the surface can
  be imported into DRM for scanout.
- **Acceptance:** GBM surface allocated with `GBM_BO_USE_RENDERING |
  GBM_BO_USE_SCANOUT` flags. Buffer import into DRM via
  `drmModeAddFB2()` succeeds.
- **Key steps:**
  1. `gbm::Device::new(drm_device)` → create GBM device
  2. Allocate GBM surface: ARGB8888, 1920x1080, RENDERING + SCANOUT
  3. Test buffer lock/unlock cycle
  4. Import GBM buffer into DRM: `gbm_bo_get_handle()` → `drmModeAddFB2()`
  5. Store GBM device and surface in `DisplayManager`

### S3.6 Display integration test on Pi

- **Crate:** `bogdan-display`
- **Depends on:** S3.4, S3.5
- **Effort:** 1 day
- **Description:** On-Pi integration test that opens DRM, enumerates
  resources, acquires CRTC, verifies HDMI output, and releases cleanly.
  Skip in CI (requires Pi hardware).
- **Acceptance:** Test passes on Pi 4 with HDMI monitor connected. Test
  is skipped on x86 CI (`#[cfg(feature = "hw")]`).
- **Key steps:**
  1. `#[cfg(feature = "hw")] #[tokio::test] async fn test_display_lifecycle()`
  2. Open DRM, enumerate, acquire, verify resolution (1920x1080), release
  3. Verify Plane 0 and Plane 1 are available
  4. Verify GBM surface allocation succeeds
  5. Verify `release()` cleans up without errors

---

## Sprint 4 — Full Playback Pipeline on Pi ✅

**Duration:** 2 weeks (10 working days)
**Goal:** End-to-end video playback on Raspberry Pi with the appsrc/StreamSource
architecture, V4L2 hardware decode, and DRM/KMS output.
**Status:** **COMPLETE** — committed, pushed to main.

**Definition of Done (DoD):**
- The appsrc + parsebin + v4l2h264dec + kmssink pipeline works on Pi 4
- Progressive download through SOCKS forwarder delivers data to appsrc
- HLS segmented download works end-to-end on Pi
- Audio plays through HDMI or 3.5mm jack
- Software decode fallback works when V4L2 is unavailable
- Buffer health monitoring reports accurate state during Tor-routed playback
- `cargo test -p bogdan-playback --features hw` passes on Pi 4

**Sprint Acceptance Criteria:**
1. On Pi 4: Cast a YouTube URL → video plays at 1080p60 through V4L2 HW decode
2. On Pi 4: Cast a Voe URL → video plays through custom resolver + StreamSource
3. On Pi 4: HLS URL → segments download and play through appsrc
4. Pause/resume/seek/stop work correctly during playback
5. Audio is in sync with video (no measurable A/V drift after 5 minutes)

### S4.1 Validate appsrc + parsebin pipeline on Pi

- **Crate:** `bogdan-playback`
- **Depends on:** S3.4
- **Effort:** 3 days
- **Description:** Validate the current pipeline architecture (appsrc →
  queue2 → parsebin → dynamic decode chain → kmssink) on real Pi 4
  hardware. Test with local MP4 files first (no Tor), then with CDN URLs
  through StreamSource.
- **Acceptance:** Local MP4 file plays at 1080p60 with V4L2 HW decode on
  Pi 4. CPU usage < 10% during playback. No GStreamer warnings or errors.
- **Key steps:**
  1. Build `bogdan-server` with `--features hw` on Pi 4
  2. Test with local file: `curl -X POST /api/cast -d '{"url":"file:///tmp/test.mp4"}'`
  3. Verify V4L2 decoder is used: check GStreamer debug logs for "v4l2h264dec"
  4. Verify zero-copy: check for DMA-BUF in debug logs
  5. Verify kmssink uses vc4 driver
  6. Measure CPU usage with `top` during playback — target < 10%

### S4.2 StreamSource integration with appsrc on Pi

- **Crate:** `bogdan-playback`
- **Depends on:** S4.1
- **Effort:** 3 days
- **Description:** Validate the full StreamSource → appsrc data flow on Pi 4
  with Tor-routed CDN downloads. Test both MP4 and HLS download modes.
  Verify the SOCKS forwarder maintains circuit isolation throughout playback.
- **Acceptance:** CDN URL fetched through Tor, data delivered to appsrc,
  video plays without interruption (assuming sufficient bandwidth). HLS
  segments download and play sequentially.
- **Key steps:**
  1. Cast a direct MP4 URL through Tor
  2. Verify SOCKS forwarder starts and routes through correct circuit
  3. Verify data flows: CDN → reqwest → channel → appsrc → pipeline
  4. Monitor throughput: log download speed vs video bitrate
  5. Test HLS: cast .m3u8 URL, verify segments download and play
  6. Test cookie forwarding: Voe CDN download includes session cookies
  7. Test CDN 403: verify error propagates to user with clear message

### S4.3 Audio pipeline validation on Pi

- **Crate:** `bogdan-playback`
- **Depends on:** S4.1
- **Effort:** 2 days
- **Description:** Validate the audio branch of the GStreamer pipeline on
  Pi 4. Test HDMI audio output and 3.5mm jack output. Verify A/V sync
  with the `ts-offset` compensation.
- **Acceptance:** Audio plays in sync with video on HDMI output. Volume
  control works. Switching to 3.5mm jack output via config works.
- **Key steps:**
  1. Default: HDMI audio via `alsasink` (device `hw:0,0` or `default`)
  2. Test volume control: `POST /api/volume {"level": 0.5}` reduces volume
  3. Test mute: `POST /api/volume {"level": 0.0}` silences audio
  4. Verify A/V sync: play content with obvious lip sync, check for drift
  5. Test 3.5mm jack: configure `alsasink device=hw:1,0` in bogdan.toml
  6. If `avdec_aac` unavailable: verify fallback to `fdkaacdec` or `fakesink`

### S4.4 Software decode fallback validation on Pi

- **Crate:** `bogdan-playback`
- **Depends on:** S4.1
- **Effort:** 1 day
- **Description:** Validate the software decode fallback path on Pi 4.
  Force V4L2 failure (e.g., by sending non-H.264 content) and verify
  avdec_h264 + videoconvert + kmssink works at reduced resolution.
- **Acceptance:** When V4L2 decode fails, playback continues with software
  decode at 720p30. A warning is logged about higher CPU usage.
- **Key steps:**
  1. Send VP9 video URL (yt-dlp may return VP9 if H.264 unavailable)
  2. Verify GStreamer falls back to software decode
  3. Verify 720p30 caps filter is applied
  4. Verify CPU usage is higher but playback is smooth
  5. Verify warning logged: "Falling back to software decode"

### S4.5 End-to-end playback test script

- **Crate:** `bogdan-playback` / integration
- **Depends on:** S4.2, S4.3
- **Effort:** 1 day
- **Description:** Write a shell script that automates the full playback
  test on Pi 4: start boGDan, cast URLs of different types, verify
  playback state, stop, verify clean shutdown.
- **Acceptance:** Script runs on Pi 4 and reports pass/fail for each test
  case. Can be run manually or from CI with Pi runner.
- **Key steps:**
  1. `scripts/test-playback-pi.sh`
  2. Test cases: direct MP4, YouTube, Voe, HLS, DoodStream
  3. For each: cast → wait for Playing → pause → resume → seek → stop
  4. Verify HTTP API responses at each step
  5. Check for GStreamer errors in logs
  6. Check for resource leaks (open file descriptors, memory)

---

## Sprint 5 — Extension & Protocol Hardening

**Duration:** 2 weeks (10 working days)
**Goal:** Browser extension is production-ready. Protocol servers are hardened
with proper error handling, rate limiting, and security measures.

**Definition of Done (DoD):**
- Browser extension passes Chrome Web Store review and Firefox Add-on review
- WebSocket server handles reconnection, ping/pong, and client limits
- HTTP API returns proper error codes and messages for all edge cases
- DLNA renderer works with VLC and Home Assistant
- No unwraps in protocol handler code paths
- All extension JS passes linting

**Sprint Acceptance Criteria:**
1. Extension installs and works on Chrome 120+ and Firefox 120+
2. Casting from the extension to a running boGDan instance works end-to-end
3. WebSocket reconnection works after server restart
4. HTTP API returns 429 for >10 requests/second from a single IP
5. DLNA renderer appears in VLC's renderer list on the same LAN

### S5.1 Extension Chrome Web Store packaging

- **Crate:** `src/extension`
- **Depends on:** nothing
- **Effort:** 2 days
- **Description:** Prepare the browser extension for Chrome Web Store
  submission. Fix any issues found during review preparation. Create
  proper extension icons, screenshots, and store listing.
- **Acceptance:** Extension loads in Chrome via `chrome://extensions`
  developer mode. All APIs (cast, stop, pause, seek, volume, status) work.
  No console errors. Content Security Policy is properly configured.
- **Key steps:**
  1. Verify `manifest.json` is valid Manifest V3
  2. Add `icons/` directory with 16, 32, 48, 128px PNGs
  3. Verify content script doesn't conflict with page CSP
  4. Test: detect video URL on YouTube, click "Cast", verify API call
  5. Test: popup shows playback status via WebSocket
  6. Test: options page saves Pi address and port
  7. Run Chrome Lighthouse audit on extension pages

### S5.2 Extension Firefox compatibility

- **Crate:** `src/extension`
- **Depends on:** S5.1
- **Effort:** 1.5 days
- **Description:** Ensure Firefox compatibility using the `browser.*`
  namespace and dual manifest. Test on Firefox 120+.
- **Acceptance:** Extension loads in Firefox via `about:debugging`. All
  functionality works identically to Chrome version.
- **Key steps:**
  1. Verify `manifest-firefox.json` has correct format
  2. Use `browser=chrome || browser` pattern for API compatibility
  3. Test: same test suite as Chrome on Firefox
  4. Fix any Firefox-specific issues (e.g., `browser.storage` differences)
  5. Build script creates separate Chrome and Firefox ZIP packages

### S5.3 WebSocket reconnection and resilience

- **Crate:** `bogdan-protocols`
- **Depends on:** nothing
- **Effort:** 2 days
- **Description:** Harden the WebSocket server for real-world use. Add
  client limits, ping/pong keepalive, reconnection handling, and proper
  cleanup on client disconnect.
- **Acceptance:** 100+ concurrent WebSocket clients can connect without
  issues. Disconnected clients are cleaned up within 30 seconds. Server
  survives client flood (1000 connect/disconnect cycles).
- **Key steps:**
  1. Limit concurrent WebSocket connections (default: 50)
  2. Ping/pong every 30 seconds; disconnect after 10s no pong
  3. On client disconnect: remove from broadcast list, log
  4. On server shutdown: send close frame to all clients, wait 5s
  5. Test: rapid connect/disconnect doesn't leak memory or tasks
  6. Test: broadcast to 50 clients simultaneously

### S5.4 HTTP API error handling hardening

- **Crate:** `bogdan-protocols`
- **Depends on:** nothing
- **Effort:** 1.5 days
- **Description:** Audit all HTTP API endpoints for proper error handling.
  Ensure every error path returns a descriptive JSON error response with
  the correct HTTP status code. Remove all `unwrap()` and `expect()` from
  handler code.
- **Acceptance:** Every API endpoint returns proper error JSON for invalid
  input, missing resources, and internal errors. No handler panics.
  Response format: `{"error": "description", "code": "ERROR_CODE"}`.
- **Key steps:**
  1. Audit each endpoint: /api/cast, /api/stop, /api/pause, /api/seek,
     /api/volume, /api/status, /api/health
  2. Add `ErrorResponse` struct with `error`, `code`, `detail` fields
  3. Replace `unwrap()` with proper error mapping
  4. Add rate limiting: 10 req/s per IP, 429 Too Many Requests
  5. Add request body size limit: 1KB max for POST bodies
  6. Test: malformed JSON → 400 with clear message
  7. Test: rate limit exceeded → 429 with Retry-After header

### S5.5 DLNA renderer integration

- **Crate:** `bogdan-protocols`
- **Depends on:** nothing
- **Effort:** 3 days
- **Description:** Implement the DLNA MediaRenderer via gmediarender
  subprocess. Spawn gmediarender with custom GStreamer pipeline string.
  Monitor state via D-Bus. Implement SSDP advertisement on LAN.
- **Acceptance:** boGDan appears as a renderer in VLC's "Render" menu.
  Casting from VLC starts playback on Pi. Stop/pause from VLC works.
- **Key steps:**
  1. Spawn `gmediarender` with `--gstout-audiosink=alsasink`
     and `--gstout-videosink=kmssink`
  2. Monitor gmediarender process: restart on crash
  3. D-Bus integration: listen for PlaybackStatus changes
  4. Map D-Bus state to SessionManager state
  5. SSDP: verify gmediarender broadcasts on LAN
  6. Test: cast from VLC → verify playback starts
  7. Test: stop from VLC → verify playback stops

---

## Sprint 6 — Integration Testing & QA

**Duration:** 2 weeks (10 working days)
**Goal:** Comprehensive test coverage, soak tests, memory leak detection,
and security audit.

**Definition of Done (DoD):**
- Memory leak test: 8-hour playback with <10 MB/hour RSS growth
- Soak test: 100 cast/stop cycles without resource exhaustion
- Network isolation verified: all outbound traffic through Tor
- Security audit checklist complete
- `cargo test --workspace` passes with >400 tests
- Integration tests for full cast→play→control→stop flow

**Sprint Acceptance Criteria:**
1. 8-hour continuous playback shows <80 MB RSS growth
2. 100 cast/stop cycles complete without GStreamer pipeline leaks
3. `iptables -L` confirms no outbound traffic bypasses Tor
4. No root privileges required beyond DRM master
5. All error paths tested: Tor down, yt-dlp missing, CDN 403, bad URLs

### S6.1 Memory leak test

- **Crate:** integration
- **Depends on:** S4.2
- **Effort:** 2 days
- **Description:** Run an 8-hour continuous playback session on Pi 4.
  Monitor RSS growth, open file descriptors, and GStreamer buffer pool
  usage. Target <10 MB/hour leak rate.
- **Acceptance:** After 8 hours, RSS growth <80 MB. No open FD leak
  (check `ls /proc/<pid>/fd | wc -l`). No GStreamer pipeline leak
  (check `GST_TRACER` stats).
- **Key steps:**
  1. Write `scripts/mem-test.sh`: start boGDan, cast URL, monitor RSS hourly
  2. Log RSS, FD count, and GStreamer stats every 5 minutes
  3. After 8 hours: generate report with growth rate analysis
  4. If leak detected: use `valgrind --leak-check=full` to identify source
  5. Common leak sources: GStreamer element refs, tokio task handles,
     SQLite connections, SOCKS forwarder sockets

### S6.2 Soak test: 100 cast/stop cycles

- **Crate:** integration
- **Depends on:** S4.2
- **Effort:** 2 days
- **Description:** Run 100 cast/stop cycles with varying URLs (YouTube,
  Voe, direct MP4, HLS). Verify no resource exhaustion, no GStreamer
  pipeline leaks, and SQLite DB stays under 1 MB.
- **Acceptance:** 100 cycles complete without errors. RSS returns to
  baseline within 10 MB after each cycle. SQLite DB < 1 MB. No zombie
  processes.
- **Key steps:**
  1. Write `scripts/soak-test.sh`: loop 100 times, cast → play 30s → stop
  2. Vary URL types: 25 YouTube, 25 Voe, 25 direct MP4, 25 HLS
  3. After each cycle: check RSS, FD count, SQLite DB size
  4. After all cycles: verify RSS within 10 MB of start value
  5. Check for zombie processes: `ps aux | grep bogdan | grep Z`
  6. Check SQLite: `ls -la bogdan.db` < 1 MB

### S6.3 Network isolation verification

- **Crate:** integration
- **Depends on:** S4.2
- **Effort:** 1.5 days
- **Description:** Verify all outbound traffic goes through Tor. Set up
  iptables rules that REJECT any outbound traffic NOT going through the
  Tor SOCKS port. Run the full system and verify no traffic is blocked
  (which would indicate a leak).
- **Acceptance:** `iptables -L OUTPUT` shows REJECT rule for non-Tor
  traffic. No packets are rejected during normal operation. DNS queries
  go through Tor (socks5h, not socks5).
- **Key steps:**
  1. Add iptables rules: allow ESTABLISHED, allow lo, allow Tor SOCKS,
     REJECT everything else
  2. Start boGDan and cast URLs
  3. Monitor REJECT counter: should stay at 0
  4. Verify DNS: no UDP port 53 traffic from bogdan process
  5. Test: disable Tor → all requests should fail (no fallback)
  6. Document iptables rules in `docs/SECURITY.md`

### S6.4 Security audit checklist

- **Crate:** all
- **Depends on:** S6.3
- **Effort:** 2 days
- **Description:** Complete the security audit checklist covering: Tor
  circuit isolation, DNS leak prevention, iptables enforcement, privilege
  minimization, TLS configuration, input validation, and extension
  security model.
- **Acceptance:** All checklist items pass or have documented exceptions.
  No high-severity findings remain open.
- **Key steps:**
  1. Tor circuit isolation: verify different domains use different circuits
  2. DNS leak: verify all DNS goes through Tor SOCKS5h
  3. iptables: verify REJECT rules for non-Tor traffic
  4. Privilege minimization: verify only DRM requires root/CAP_SYS_ADMIN
  5. TLS: verify rustls config, no TLS 1.0/1.1
  6. Input validation: fuzz HTTP API endpoints with malformed input
  7. Extension: verify message passing is validated, no eval(), no innerHTML
  8. Document findings in `docs/SECURITY_AUDIT.md`

### S6.5 Integration test: full cast→play→control→stop flow

- **Crate:** integration (`src/server/tests/`)
- **Depends on:** S4.2
- **Effort:** 2.5 days
- **Description:** Write comprehensive integration tests that exercise the
  full user flow: cast URL → resolve → play → pause → resume → seek →
  stop. Test with mock subsystems (no Pi hardware required).
- **Acceptance:** Integration tests pass on x86 CI. Every API endpoint and
  state transition is tested. Error paths (resolve failure, pipeline error)
  are tested.
- **Key steps:**
  1. Create mock `ResolverTrait`, `PlaybackTrait`, `DisplayTrait`, `TorTrait`
  2. Test: `POST /api/cast` → `202` → `GET /api/status` → `Playing`
  3. Test: `POST /api/pause` → `Paused` → `POST /api/pause` → `Playing`
  4. Test: `POST /api/seek {"seconds": 60}` → `Seeking` → `Playing`
  5. Test: `POST /api/stop` → `Idle`
  6. Test: resolve failure → `422 Unprocessable Entity`
  7. Test: concurrent cast → `409 Conflict`
  8. Test: WebSocket receives `MEDIA_STATUS` on state change

---

## Sprint 7 — Distribution, Documentation & Release

**Duration:** 2 weeks (10 working days)
**Goal:** boGDan can be installed on a fresh Raspberry Pi OS image with a
single command and minimal configuration. Documentation is complete.

**Definition of Done (DoD):**
- `dpkg -i bogdan.deb` installs and configures everything
- User guide covers installation, configuration, troubleshooting
- Security hardening guide documents iptables, Tor verification
- Pre-built SD card image available for download
- GitHub Release with binary, deb, SHA-256 checksums, release notes
- README has quick-start guide: flash → boot → install extension → cast

**Sprint Acceptance Criteria:**
1. Fresh Pi OS Lite → `curl setup.sh | bash` → working boGDan in <15 minutes
2. `dpkg -i bogdan.deb` → `systemctl start bogdan` → working service
3. User can cast from browser extension within 5 minutes of installation
4. All documentation is accurate and matches current code behavior
5. GitHub Release v0.1.0-alpha is published with all artifacts

### S7.1 Debian package finalization

- **Crate:** `packaging/`
- **Depends on:** S4.2
- **Effort:** 2 days
- **Description:** Finalize the Debian package with all required files:
  binary, config, systemd service, torrc, provider configs, extension
  files. Ensure `dpkg -i` installs and configures everything correctly.
- **Acceptance:** `dpkg -i bogdan_0.1.0_arm64.deb` on a fresh Pi OS Lite
  installs everything and `systemctl start bogdan` works.
- **Key steps:**
  1. Verify `debian/control` has all dependencies
  2. Include provider configs: `providers.d/*.toml`
  3. Include extension files: `src/extension/`
  4. Include systemd service: `bogdan.service`
  5. Include torrc: `config/torrc`
  6. Include example config: `bogdan.toml.example` → `/etc/bogdan/bogdan.toml`
  7. Postinst script: enable and start service, configure iptables
  8. Prerm script: stop service, clean up iptables

### S7.2 User guide

- **Crate:** `docs/`
- **Depends on:** S7.1
- **Effort:** 2 days
- **Description:** Write comprehensive user guide covering installation,
  configuration, browser extension setup, daily usage, and troubleshooting.
- **Acceptance:** A new user can follow the guide from a fresh Pi to
  working boGDan without reading any other documentation.
- **Key steps:**
  1. Quick start: flash Pi OS → install boGDan → install extension → cast
  2. Configuration: bogdan.toml options explained
  3. Browser extension: install, configure Pi address, usage
  4. DLNA: connect from VLC, Home Assistant
  5. Troubleshooting: common errors and solutions
  6. FAQ: supported codecs, Tor requirements, network setup

### S7.3 Security hardening guide

- **Crate:** `docs/`
- **Depends on:** S6.4
- **Effort:** 1.5 days
- **Description:** Write security hardening guide documenting iptables
  rules, Tor verification procedures, physical security recommendations,
  and attack surface analysis.
- **Acceptance:** Guide includes copy-paste iptables rules that work on
  Pi OS. Tor verification procedure confirms all traffic is routed
  through Tor.
- **Key steps:**
  1. iptables rules: allow Tor SOCKS, allow lo, allow ESTABLISHED, REJECT rest
  2. Tor verification: `systemctl status tor`, check SOCKS port, test with curl
  3. DNS leak test: `tcpdump -i eth0 port 53` during resolve
  4. Physical security: disable SSH password auth, use keys only
  5. Attack surface: list all listening ports and their purposes
  6. Recommend: change default API port, use HTTPS for API

### S7.4 Pre-built SD card image

- **Crate:** `packaging/`
- **Depends on:** S7.1
- **Effort:** 2 days
- **Description:** Create a pre-built Raspberry Pi OS Lite image with
  boGDan pre-installed. Image should be flash-and-boot: insert SD card,
  power on, boGDan is running.
- **Acceptance:** Raspberry Pi Imager can flash the image. After boot,
  boGDan service is running and API is accessible. Image size < 2 GB.
- **Key steps:**
  1. Start with Pi OS Lite (64-bit) base image
  2. Install boGDan deb package
  3. Pre-configure: enable Tor, set up iptables, start boGDan
  4. Shrink filesystem to minimum size
  5. Create image with `dd` or `rpi-image-gen`
  6. Test: flash image → boot → verify boGDan running
  7. Compress with `xz` for download

### S7.5 Release checklist and GitHub Release

- **Crate:** project
- **Depends on:** S7.2, S7.3, S7.4
- **Effort:** 2.5 days
- **Description:** Create release checklist, tag v0.1.0-alpha, build all
  artifacts, compute SHA-256 checksums, write release notes, and publish
  GitHub Release.
- **Acceptance:** GitHub Release page has: binary, deb, SD image, checksums,
  release notes. All artifacts pass verification (`sha256sum -c`).
- **Key steps:**
  1. Release checklist: verify all Sprint 1-7 DoD items
  2. Build on Pi 4: `cargo build --release --features hw`
  3. Build deb: `cargo deb --target aarch64-unknown-linux-gnu`
  4. Build SD image: run S7.4 image creation
  5. Compute SHA-256 for all artifacts
  6. Write release notes: features, known issues, installation instructions
  7. Tag: `git tag v0.1.0-alpha && git push --tags`
  8. Create GitHub Release with all artifacts
  9. Update README with link to release

---

## Sprint Dependency Graph

```
Sprint 1 (Provider Extraction) ──── independent, start immediately
       │
Sprint 2 (Resolver Testing) ─────── depends on S1
       │
Sprint 3 (DRM/KMS Display) ──────── independent, can parallel with S1/S2
       │
Sprint 4 (Full Playback on Pi) ──── depends on S3
       │
Sprint 5 (Extension & Protocols) ── independent, can parallel with S3/S4
       │
Sprint 6 (Integration Testing) ──── depends on S4
       │
Sprint 7 (Distribution & Release) ─ depends on S5, S6
```

**Parallelism opportunity:** Sprints 1, 3, and 5 can run in parallel
(3 workstreams). Sprint 2 depends on Sprint 1. Sprint 4 depends on Sprint 3.
Sprint 6 depends on Sprint 4. Sprint 7 depends on Sprints 5 and 6.

**Critical path:** S3 → S4 → S6 → S7 (6 weeks minimum for Pi hardware bringup)

**Total estimated duration:**
- Serial (1 developer): 14 weeks
- Parallel (3 developers): 7-8 weeks

---

## Effort Summary

| Sprint | Description | Effort | Dependencies |
|--------|-------------|--------|--------------|
| 1 | Provider Extraction & Resolver Architecture | 10 days | None |
| 2 | Resolver Testing & CDN Resilience | 10 days | S1 |
| 3 | DRM/KMS Display & Pi Hardware | 10 days | None |
| 4 | Full Playback Pipeline on Pi | 10 days | S3 |
| 5 | Extension & Protocol Hardening | 10 days | None |
| 6 | Integration Testing & QA | 10 days | S4 |
| 7 | Distribution, Documentation & Release | 10 days | S5, S6 |
| **Total** | | **70 days** | |

---

## Historical Task Reference

The original phase-based task breakdown (T-0.x through T-10.x) is preserved
below for reference. Tasks marked with ✅ are complete; their implementation
details remain valid. Tasks without ✅ have been incorporated into the sprint
plan above with updated descriptions and acceptance criteria.

### Completed Tasks (Phase 0 through Phase 7)

<details>
<summary>Click to expand completed task list</summary>

- T-0.1 Workspace compilation fix ✅
- T-0.2 `.cargo/config.toml` cross-compilation ✅
- T-0.3 GitHub Actions CI workflow ✅
- T-0.4 Smoke test infrastructure ✅
- T-0.5 Conditional compilation for Pi deps ✅
- T-1.1 Tor process spawning ✅
- T-1.2 SOCKS5 connectivity verification ✅
- T-1.3 Stream isolation via SOCKS5 username ✅
- T-1.4 Tor process lifecycle management ✅
- T-1.5 Circuit health monitoring via control port ✅
- T-1.6 Tor integration test ✅
- T-2.6 Mock display mode for x86 testing ✅
- T-3.1 GStreamer initialization and pipeline construction ✅
- T-3.4 Play/Pause/Resume/Stop state transitions ✅
- T-3.5 Seek implementation ✅
- T-3.6 Volume control ✅
- T-3.7 Buffer health monitoring ✅
- T-3.8 Software decode fallback ✅
- T-3.9 Pipeline error recovery ✅
- T-3.10 Position and duration queries ✅
- T-4.1 yt-dlp subprocess invocation ✅
- T-4.2 yt-dlp JSON output parsing ✅
- T-4.3 Format selection: force H.264 ✅
- T-4.4 Tor SOCKS5h proxy routing for yt-dlp ✅
- T-4.5 Resolution cache with TTL ✅ (upgraded to SQLite)
- T-4.6 Subtitle extraction ✅
- T-4.7 Direct media passthrough ✅
- T-4.8 Error handling and timeout ✅
- T-4.9 Resolver integration test ✅
- T-5.1 Trait-object wiring ✅
- T-5.2 Load flow: resolve → create session → play ✅
- T-5.3 State machine implementation ✅
- T-5.4 Play/Pause/Stop/Seek/SetVolume delegation ✅
- T-5.5 Watch channel for state broadcasting ✅
- T-5.6 Session cleanup and persistence ✅
- T-5.7 Thread safety for concurrent access ✅
- T-6.1 HTTP API: POST /api/cast ✅
- T-6.2 HTTP API: POST /api/stop ✅
- T-6.3 HTTP API: POST /api/pause ✅
- T-6.4 HTTP API: POST /api/seek ✅
- T-6.5 HTTP API: GET /api/status ✅
- T-6.6 HTTP API: POST /api/volume ✅
- T-6.7 CORS headers for browser extension ✅
- T-6.8 WebSocket server ✅
- T-6.9 WebSocket: RESOLVE_PROGRESS messages ✅
- T-7.1 Real component initialization ✅
- T-7.2 Task spawning and concurrent execution ✅
- T-7.3 Graceful shutdown sequence ✅
- T-7.4 Startup ordering validation ✅
- T-7.5 Health check endpoint ✅
- T-7.6 Configuration file support ✅
- T-9.1 Unit test coverage for all crates ✅
- T-9.2 Integration test: full playback flow ✅
- T-9.3 Pi hardware smoke test script ✅
- T-9.4 Network isolation verification ✅
- T-9.7 Security audit checklist ✅
- T-9.8 CI pipeline finalization ✅
- T-10.1 Setup script overhaul ✅
- T-10.2 Debian package ✅
- T-10.4 README.md rewrite ✅

</details>

### Superseded Tasks

These tasks from the original breakdown have been superseded by the
appsrc/StreamSource architecture and are no longer directly applicable:

- **T-3.2 Pipeline playback with direct URL** — Superseded by S4.1/S4.2
  (appsrc + StreamSource replaces souphttpsrc direct URL)
- **T-3.3 Tor SOCKS5 proxy in souphttpsrc** — Superseded by S4.2
  (SOCKS forwarder + reqwest replaces souphttpsrc SOCKS5 properties)
