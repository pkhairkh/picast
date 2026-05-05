# PiCast Task Breakdown

**Granular, actionable tasks derived from [ROADMAP.md](ROADMAP.md).**
Each task has a unique ID, phase, dependency, acceptance criteria, and estimated
effort. Tasks are ordered by execution sequence within each phase.

**Status legend:** `[ ]` not started · `[~]` in progress · `[x]` done

---

## Phase 0 — Build & CI Foundation

### T-0.1 Workspace compilation fix
- **Crate:** workspace
- **Depends on:** nothing
- **Effort:** 0.5 day
- **Description:** Fix all compilation errors across the workspace. Currently
  `server/main.rs` uses `Arc<()>` stubs instead of real crate types. Make all
  crates compile on x86_64 with feature flags that gate Pi-specific dependencies
  (`gstreamer`, `drm`, `gbm`, `nix`).
- **Acceptance:** `cargo check --workspace` exits 0 on x86_64.
- **Key steps:**
  1. Add feature flags to each crate's `Cargo.toml`: `default = []`, `hw = ["gstreamer", "drm-rs", "gbm", "nix"]`
  2. Gate `use gstreamer;` etc. behind `#[cfg(feature = "hw")]`
  3. Provide `compile_error!()` if `hw` feature is used on non-aarch64
  4. Fix any version mismatches in workspace `Cargo.toml`

### T-0.2 `.cargo/config.toml` cross-compilation
- **Crate:** workspace
- **Depends on:** T-0.1
- **Effort:** 0.5 day
- **Description:** Configure cross-compilation for `aarch64-unknown-linux-gnu`.
  Add `.cargo/config.toml` with target-specific linker. Verify sysroot and
  linker availability.
- **Acceptance:** `cargo check --target aarch64-unknown-linux-gnu --workspace` exits 0.
- **Key steps:**
  1. Install `aarch64-linux-gnu-gcc` on build host
  2. Create `.cargo/config.toml` with `[target.aarch64-unknown-linux-gnu] linker = "aarch64-linux-gnu-gcc"`
  3. Add `--target aarch64-unknown-linux-gnu` to CI
  4. Document cross-compilation setup in `docs/contributing.md`

### T-0.3 GitHub Actions CI workflow
- **Crate:** `.github/`
- **Depends on:** T-0.1
- **Effort:** 1 day
- **Description:** Create `.github/workflows/ci.yml` that runs on every push
  and PR. Pipeline: check, clippy, test, rustfmt.
- **Acceptance:** CI runs green on a test PR.
- **Key steps:**
  1. Create `.github/workflows/ci.yml`
  2. Jobs: `check-x86`, `check-aarch64`, `clippy`, `test`, `fmt`
  3. Install system deps: `libgstreamer1.0-dev`, `libgstreamer-plugins-base1.0-dev`, `libdrm-dev`, `libgbm-dev`, `libsqlite3-dev`
  4. Cache `~/.cargo/registry` and `target/`
  5. Run without `hw` feature on CI (x86 can't use Pi-specific libs)

### T-0.4 Smoke test infrastructure
- **Crate:** all
- **Depends on:** T-0.1
- **Effort:** 1 day
- **Description:** Add at least one `#[cfg(test)]` module to every crate. Tests
  can be trivial (testing `Default` impls, `Display` formatting, error variants)
  but they must exist and pass.
- **Acceptance:** `cargo test --workspace` runs and passes on x86_64.
- **Key steps:**
  1. `picast-tor`: test `SocksProxy::default()`, `SocksProxy::addr()`, `TorManager::new()` parsing
  2. `picast-display`: test `DisplayManager::new()` with mock path, `DrmPlane`/`DrmCrtc` construction
  3. `picast-playback`: test `PipelineConfig::default()`, `BufferHealth::default()`
  4. `picast-resolver`: test `Resolver::classify()` for each `UrlCategory`, `ResolveResult` serialization
  5. `picast-session`: test `MediaSession::new()`, `PlayerState` serialization, `SessionManager::new()` with in-memory SQLite
  6. `picast-protocols`: test `HttpApiServer::new()`, `WebSocketServer::new()`, `DlnaRenderer::new()`
  7. `picast-server`: test `AppConfig::from_env()`

### T-0.5 Conditional compilation for Pi deps
- **Crate:** playback, display
- **Depends on:** T-0.1
- **Effort:** 1 day
- **Description:** Gate all `gstreamer`, `drm`, `gbm`, `nix` imports behind
  `#[cfg(feature = "hw")]`. Provide stub implementations when the feature is
  off so the crate still compiles and tests pass on x86 dev machines.
- **Acceptance:** `cargo test -p picast-playback -p picast-display` passes without `hw` feature.
- **Key steps:**
  1. `picast-playback/Cargo.toml`: `[features] hw = ["gstreamer", "gstreamer-video"]`
  2. `PlaybackEngine::new()`: `#[cfg(feature = "hw")]` calls `gstreamer::init()`, else returns a mock engine
  3. `picast-display/Cargo.toml`: `[features] hw = ["drm-rs", "gbm", "nix"]`
  4. `DisplayManager::new()`: `#[cfg(feature = "hw")]` opens real DRM device, else returns mock with default resolution
  5. Ensure `cargo check -p picast-playback` works without `hw` feature

---

## Phase 1 — Tor Daemon Integration

### T-1.1 Tor process spawning
- **Crate:** `picast-tor`
- **Depends on:** T-0.5
- **Effort:** 1 day
- **Description:** Implement `TorManager::ensure_running()` to spawn the `tor`
  binary as a child process via `tokio::process::Command`. Detect if Tor is
  already running on the configured SOCKS port before spawning.
- **Acceptance:** Test binary spawns Tor, `ps` shows `tor` process, SOCKS port
  becomes reachable within `startup_timeout_ms`.
- **Key steps:**
  1. Check if SOCKS port is already open (`TcpStream::connect` with short timeout)
  2. If not, spawn `tor --defaults-torrc /etc/tor/torrc` (or embedded minimal torrc)
  3. Store `tokio::process::Child` in `TorManager` for lifecycle management
  4. Poll SOCKS port with exponential backoff until reachable or timeout
  5. Set `owns_process = true` only if we spawned the process

### T-1.2 SOCKS5 connectivity verification
- **Crate:** `picast-tor`
- **Depends on:** T-1.1
- **Effort:** 0.5 day
- **Description:** Implement a real SOCKS5 handshake test to verify the proxy
  is functional, not just that the port is open. Connect to the proxy, send
  SOCKS5 greeting, verify response.
- **Acceptance:** `health_check()` returns `Ok(CircuitHealth { is_healthy: true, .. })` when Tor is running.
- **Key steps:**
  1. Connect to SOCKS proxy via `TcpStream`
  2. Send SOCKS5 greeting: `[0x05, 0x02, 0x00, 0x02]` (no auth + username/auth)
  3. Verify server response
  4. If successful, measure round-trip latency via a `CONNECT` request to a known host
  5. Return `CircuitHealth { is_healthy: true, latency_ms: Some(measured), .. }`

### T-1.3 Stream isolation via SOCKS5 username
- **Crate:** `picast-tor`
- **Depends on:** T-1.2
- **Effort:** 1 day
- **Description:** Implement the SHA-256 hash-based stream isolation identifier.
  Each target domain gets a unique SOCKS5 username so Tor's `IsolateSOCKSAuth`
  assigns separate circuits. Expose a method to generate the username for a given URL.
- **Acceptance:** Two URLs from different domains produce different SOCKS5 usernames.
  Same domain produces the same username.
- **Key steps:**
  1. Add `md-5` (already in deps) or use `sha2` for SHA-256 hashing
  2. `pub fn stream_isolation_id(domain: &str) -> String` → SHA-256 of domain, hex-encoded
  3. Format: `picast-{hex_hash[:16]}`
  4. Expose `pub fn socks_username_for_url(&self, url: &Url) -> String`
  5. Add `socks5-proxy-username` field to `SocksProxy` config
  6. Unit test: same domain → same username; different domains → different usernames

### T-1.4 Tor process lifecycle management
- **Crate:** `picast-tor`
- **Depends on:** T-1.1
- **Effort:** 1 day
- **Description:** Implement clean shutdown and crash recovery for the Tor child
  process. `shutdown()` sends SIGTERM and waits. `Drop` does best-effort kill.
  Auto-restart on unexpected exit.
- **Acceptance:** `shutdown()` cleanly stops Tor; if Tor crashes mid-session,
  `TorManager` detects it and can restart.
- **Key steps:**
  1. `shutdown()`: send SIGTERM via `child.kill()`, `child.wait().await` with 10s timeout
  2. On timeout, SIGKILL
  3. Spawn background task: monitor `child.wait()`, set `owns_process = false` on exit
  4. If unexpected exit (non-zero, not from shutdown): log error, optionally auto-restart
  5. `Drop` impl: try `child.start_kill()` if process still alive (sync, best-effort)

### T-1.5 Circuit health monitoring via control port
- **Crate:** `picast-tor`
- **Depends on:** T-1.2
- **Effort:** 1.5 days
- **Description:** Connect to Tor's control port (`9051`), authenticate, and
  periodically query circuit status. Parse `GETINFO circuit-status` to populate
  `CircuitHealth` metrics.
- **Acceptance:** `health_check()` returns real `CircuitHealth` with
  `open_circuits`, `built_circuits`, `latency_ms`.
- **Key steps:**
  1. Add `ControlPort 9051` to torrc config
  2. Connect to `127.0.0.1:9051`, authenticate with cookie or password
  3. Send `GETINFO circuit-status\r\n`
  4. Parse response: count `BUILT`, `FAILED`, `CLOSED` circuits
  5. Spawn `tokio::spawn` background task polling every 30s
  6. Store latest `CircuitHealth` in `Arc<Mutex<CircuitHealth>>`
  7. `health_check()` reads from the shared state

### T-1.6 Tor integration test
- **Crate:** `picast-tor`
- **Depends on:** T-1.3, T-1.4, T-1.5
- **Effort:** 1 day
- **Description:** End-to-end test that spawns a real Tor daemon, verifies
  SOCKS5 connectivity, generates stream isolation IDs, and shuts down cleanly.
  Skip in CI if Tor is not installed (`#[cfg(feature = "tor-test")]`).
- **Acceptance:** Test passes on a machine with `tor` installed.
- **Key steps:**
  1. `#[tokio::test] async fn test_tor_lifecycle()`
  2. `TorManager::new("127.0.0.1:19050")` with non-standard port to avoid conflicts
  3. Use a temporary torrc with `DataDirectory` in `/tmp/picast-test-*`
  4. `ensure_running(60_000).await?`
  5. `health_check().await?` → verify `is_healthy`
  6. Generate stream IDs → verify determinism
  7. `shutdown().await?` → verify process gone

---

## Phase 2 — DRM/KMS Display Manager

### T-2.1 DRM device open and master acquisition
- **Crate:** `picast-display`
- **Depends on:** T-0.5
- **Effort:** 1 day
- **Description:** Open `/dev/dri/card0`, call `drmSetMaster()`, verify the
  vc4 driver is loaded. Fail with clear error messages if hardware is missing.
- **Acceptance:** `DisplayManager::new("/dev/dri/card0")` succeeds on Pi 4 with
  vc4 driver; fails gracefully on non-Pi hardware.
- **Key steps:**
  1. Use `drm-rs` crate: `drm::Device::open(path)` → get file descriptor
  2. `drmSetMaster(fd)` → acquire DRM master privilege
  3. Query driver: `drmGetVersion()` → verify name == "vc4"
  4. If not vc4: return `DisplayError::DeviceOpen("Expected vc4 driver, found ...")`
  5. Store `drm::Device` in `DisplayManager`
  6. `#[cfg(not(feature = "hw"))]`: return mock with default resolution

### T-2.2 Plane and CRTC enumeration
- **Crate:** `picast-display`
- **Depends on:** T-2.1
- **Effort:** 1 day
- **Description:** Enumerate DRM planes and CRTCs. For each plane, record its
  ID, supported formats, and Z-position. For each CRTC, record its current mode.
- **Acceptance:** `planes()` returns at least 2 planes; `crtcs()` returns at
  least 1 CRTC on Pi 4.
- **Key steps:**
  1. `drmModeGetResources()` → enumerate CRTCs, connectors
  2. `drmModeGetPlaneResources()` → enumerate planes
  3. For each plane: `drmModeGetPlane()` → `plane_id`, `formats`, `possible_crtcs`
  4. Get `zpos` property via `drmModeGetObjectProperties()` + `drmModeGetProperty()`
  5. Map DRM fourcc codes to `Vec<u32>` in `DrmPlane.formats`
  6. For each CRTC: `drmModeGetCrtc()` → `crtc_id`, `width`, `height`, `refresh_rate`

### T-2.3 HDMI connector detection and mode selection
- **Crate:** `picast-display`
- **Depends on:** T-2.1
- **Effort:** 1 day
- **Description:** Find the connected HDMI connector, read its EDID, and select
  the preferred display mode (1080p60). Fall back to the best available mode.
- **Acceptance:** `acquire()` selects 1080p60 on a standard HDMI monitor.
- **Key steps:**
  1. Enumerate connectors: `drmModeGetConnector()` for each
  2. Filter for `DRM_MODE_CONNECTED` status
  3. Prefer `DRM_MODE_CONNECTOR_HDMIA`
  4. From modes: prefer 1920×1080 @ 60Hz
  5. Store selected connector ID and mode in `DisplayManager`

### T-2.4 Atomic modesetting implementation
- **Crate:** `picast-display`
- **Depends on:** T-2.2, T-2.3
- **Effort:** 2 days
- **Description:** Implement `acquire()` using `drmModeAtomicCommit` to set
  the CRTC mode and enable Plane 0 (video). Implement `release()` to disable
  planes and restore previous state.
- **Acceptance:** After `acquire()`, HDMI output shows a black frame at 1080p60.
  After `release()`, display returns to previous state.
- **Key steps:**
  1. Create atomic request: `drmModeAtomicAlloc()`
  2. Set CRTC properties: mode, active, primary plane FB
  3. Set Plane 0 properties: CRTC_ID, SRC_* (source rect), CRTC_* (dest rect), FB_ID
  4. Commit with `DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_PAGE_FLIP_EVENT`
  5. Wait for vblank event via `drmHandleEvent()`
  6. `release()`: disable planes, clear CRTC FB

### T-2.5 GBM device and surface initialization
- **Crate:** `picast-display`
- **Depends on:** T-2.1
- **Effort:** 1.5 days
- **Description:** Initialize GBM (Generic Buffer Manager) on the DRM device.
  Allocate a GBM surface for Plane 1 (OSD overlay) with ARGB8888 format.
- **Acceptance:** GBM surface allocated with `GBM_BO_USE_RENDERING | GBM_BO_USE_SCANOUT`
  flags; buffer can be imported into DRM.
- **Key steps:**
  1. `gbm::Device::new(drm_device)` → create GBM device
  2. `gbm_device.create_surface(width, height, GBM_FORMAT_ARGB8888, GBM_BO_USE_RENDERING | GBM_BO_USE_SCANOUT)`
  3. Verify surface creation succeeds
  4. Test buffer lock/unlock cycle
  5. Import GBM buffer into DRM: `gbm_bo_get_handle()` → `drmModeAddFB2()`

### T-2.6 Mock display mode for x86 testing
- **Crate:** `picast-display`
- **Depends on:** T-0.5
- **Effort:** 0.5 day
- **Description:** Implement `DisplayManager::new("mock")` that skips real DRM
  and returns hardcoded values. This enables unit testing and CI on x86.
- **Acceptance:** All `DisplayManager` methods work without real hardware.
- **Key steps:**
  1. If `device_path == "mock"`, set `is_mock = true`
  2. `planes()` returns `[DrmPlane { plane_id: 0, zpos: 0, .. }, DrmPlane { plane_id: 1, zpos: 1, .. }]`
  3. `crtcs()` returns `[DrmCrtc { crtc_id: 0, width: 1920, height: 1080, .. }]`
  4. `acquire()` and `release()` are no-ops
  5. `resolution()` returns `(1920, 1080)`

### T-2.7 Display integration test on Pi
- **Crate:** `picast-display`
- **Depends on:** T-2.4, T-2.5
- **Effort:** 1 day
- **Description:** On-Pi test that opens DRM, enumerates resources, acquires
  CRTC, and verifies HDMI output. Skip in CI.
- **Acceptance:** Test passes on Pi 4 with HDMI monitor connected.
- **Key steps:**
  1. `#[cfg(feature = "hw")] #[tokio::test] async fn test_display_lifecycle()`
  2. Open DRM, enumerate, acquire, verify resolution, release
  3. Verify Plane 0 and Plane 1 are available
  4. Verify GBM surface allocation succeeds

---

## Phase 3 — GStreamer Playback Engine

### T-3.1 GStreamer initialization and pipeline construction
- **Crate:** `picast-playback`
- **Depends on:** T-0.5, T-2.6 (for mock display)
- **Effort:** 2 days
- **Description:** Initialize GStreamer (`gst::init()`), construct the pipeline
  from individual elements, and manage element lifecycle. Use programmatic
  element creation (not `parse_launch`) for type safety.
- **Acceptance:** `PlaybackEngine::new()` creates a `gst::Pipeline` with all
  elements linked.
- **Key steps:**
  1. `gst::init()` in `PlaybackEngine::new()`
  2. Create elements: `souphttpsrc`, `queue2`, `h264parse`, `v4l2h264dec`, `kmssink`, `alsasink`
  3. Add all elements to pipeline
  4. Link elements: `src → queue2 → parse → decoder → video_sink` (video branch)
  5. Add `audioconvert → volume → alsasink` for audio branch
  6. Use `gst::Pipeline::get_by_name()` for element access
  7. Handle link failures gracefully

### T-3.2 Pipeline playback with direct URL
- **Crate:** `picast-playback`
- **Depends on:** T-3.1
- **Effort:** 2 days
- **Description:** Set a URL on `souphttpsrc`, transition pipeline to `Playing`,
  and verify bus messages indicate successful playback.
- **Acceptance:** `play("http://...test.mp4")` returns `Ok(())` and pipeline
  reaches `Playing` state.
- **Key steps:**
  1. `souphttpsrc.set_property("location", url)`
  2. `pipeline.set_state(gst::State::Playing)`
  3. Listen on bus for `GST_MESSAGE_STATE_CHANGED` → confirm `Playing`
  4. Handle `GST_MESSAGE_ERROR` → return `PlaybackError::Gstreamer`
  5. Timeout: if not playing within 30s, return error

### T-3.3 Tor SOCKS5 proxy in souphttpsrc
- **Crate:** `picast-playback`
- **Depends on:** T-3.2, T-1.3
- **Effort:** 1 day
- **Description:** Configure `souphttpsrc` with SOCKS5 proxy from `TorManager`.
  Set `socks5-proxy-ip`, `socks5-proxy-port`, and `socks5-proxy-username` for
  stream isolation.
- **Acceptance:** Media fetched through Tor; `used_tor = true` in playback status.
- **Key steps:**
  1. `souphttpsrc.set_property("socks5-proxy-ip", proxy_ip)`
  2. `souphttpsrc.set_property("socks5-proxy-port", proxy_port)`
  3. `souphttpsrc.set_property("socks5-proxy-username", stream_id)`
  4. `souphttpsrc.set_property("proxy-id", "")` (disable HTTP proxy)
  5. Test: verify media URL resolves through Tor (check IPs if possible)

### T-3.4 Play/Pause/Resume/Stop state transitions
- **Crate:** `picast-playback`
- **Depends on:** T-3.2
- **Effort:** 2 days
- **Description:** Implement `pause()`, `resume()`, and `stop()` by setting
  GStreamer pipeline state and listening for confirmation on the bus.
- **Acceptance:** All four state transitions work and return correct state.
- **Key steps:**
  1. `pause()`: `pipeline.set_state(gst::State::Paused)` → wait for confirmation
  2. `resume()`: `pipeline.set_state(gst::State::Playing)` → wait for confirmation
  3. `stop()`: `pipeline.set_state(gst::State::Null)` → clean up resources
  4. Track current state internally, return `PlaybackError::InvalidState` for
     illegal transitions (e.g., pause when already paused)
  5. Add `current_state()` method returning `PlayerState`

### T-3.5 Seek implementation
- **Crate:** `picast-playback`
- **Depends on:** T-3.4
- **Effort:** 1 day
- **Description:** Implement `seek()` using `gst_element_seek_simple()` with
  `FLUSH_KEY_UNITS` flag. After seek, query position to confirm.
- **Acceptance:** `seek(60_000)` seeks to 1 minute; `position_ms()` returns ~60000.
- **Key steps:**
  1. `pipeline.seek_simple(gst::Format::Time, gst::SeekFlags::FLUSH_KEY_UNITS, position_ns)`
  2. Wait for `GST_MESSAGE_ASYNC_DONE` on bus
  3. Query position: `pipeline.query_position(gst::Format::Time)`
  4. Handle seek failures: `PlaybackError::SeekFailed`

### T-3.6 Volume control
- **Crate:** `picast-playback`
- **Depends on:** T-3.1
- **Effort:** 0.5 day
- **Description:** Insert a `volume` element before `alsasink`. Expose
  `set_volume(0.0–1.0)` by setting the `volume` property.
- **Acceptance:** `set_volume(0.5)` audibly reduces volume; `set_volume(0.0)` mutes.
- **Key steps:**
  1. Create `gst::ElementFactory::make("volume")` during pipeline construction
  2. Insert between `audioconvert` and `alsasink`
  3. `volume_element.set_property("volume", value as f64)`
  4. `get_volume()` reads the property back

### T-3.7 Buffer health monitoring
- **Crate:** `picast-playback`
- **Depends on:** T-3.2
- **Effort:** 1.5 days
- **Description:** Listen for `GST_MESSAGE_BUFFERING` from `queue2`. Parse
  buffering percentage and populate `BufferHealth` struct. Expose for polling
  by the session manager.
- **Acceptance:** `buffer_health()` returns real fill percentage during playback
  over Tor (where buffering is expected).
- **Key steps:**
  1. Set `queue2` property: `use-buffering=true`, `max-size-bytes=52428800`
  2. On `GST_MESSAGE_BUFFERING`: parse `percent` from message
  3. Store in `Arc<Mutex<BufferHealth>>`
  4. Calculate `buffered_seconds` from `fill_percent × buffer_duration_ms`
  5. Detect stalls: `is_buffering = true` when `percent < 100` and pipeline auto-pauses
  6. Resume playback when `percent >= 80` (high threshold)

### T-3.8 Software decode fallback
- **Crate:** `picast-playback`
- **Depends on:** T-3.2
- **Effort:** 1.5 days
- **Description:** If `v4l2h264dec` fails to negotiate (not available, wrong
  format, DMA-BUF allocation failure), fall back to software decode via
  `avdec_h264 → videoconvert → kmssink`.
- **Acceptance:** On a system without V4L2 M2M, playback still works (albeit
  with higher CPU usage).
- **Key steps:**
  1. Attempt `v4l2h264dec` pipeline first
  2. If `GST_MESSAGE_ERROR` from decoder: catch error, construct fallback pipeline
  3. Fallback: `souphttpsrc → queue2 → h264parse → avdec_h264 → videoconvert → kmssink`
  4. Log warning: "Falling back to software decode — higher CPU usage expected"
  5. Limit fallback to 720p30: add caps filter `video/x-raw, width<=1280, height<=720`

### T-3.9 Pipeline error recovery
- **Crate:** `picast-playback`
- **Depends on:** T-3.4
- **Effort:** 1 day
- **Description:** Handle `GST_MESSAGE_ERROR` and `GST_MESSAGE_WARNING` on the
  bus. On error: extract debug string, clean up pipeline, return error to caller.
  On warning: log but continue.
- **Acceptance:** A broken URL returns `PlaybackError::Gstreamer` with descriptive
  message; pipeline is in a clean state after error.
- **Key steps:**
  1. Spawn bus watch: `pipeline.bus().add_watch()`
  2. On `GST_MESSAGE_ERROR`: `err.parse()` → extract error message and debug string
  3. Set pipeline to `Null` state
  4. Return error via `watch::Sender` or `oneshot` channel
  5. On `GST_MESSAGE_WARNING`: log with `tracing::warn!`

### T-3.10 Position and duration queries
- **Crate:** `picast-playback`
- **Depends on:** T-3.4
- **Effort:** 0.5 day
- **Description:** Implement `position_ms()` and add `duration_ms()` method.
  Use GStreamer query API.
- **Acceptance:** During playback, `position_ms()` advances; `duration_ms()` matches known media length.
- **Key steps:**
  1. `position_ms()`: `pipeline.query_position(gst::Format::Time)` → convert nanos to millis
  2. `duration_ms()`: `pipeline.query_duration(gst::Format::Time)` → convert nanos to millis
  3. Return `0` if query fails (e.g., while seeking)

---

## Phase 4 — Content Resolver (yt-dlp)

### T-4.1 yt-dlp subprocess invocation
- **Crate:** `picast-resolver`
- **Depends on:** T-1.3 (for stream isolation)
- **Effort:** 1 day
- **Description:** Implement `Resolver::resolve()` to spawn `yt-dlp -J <url>`
  via `tokio::process::Command`. Capture stdout (JSON) and stderr (errors).
  Apply timeout (60s).
- **Acceptance:** `resolve("https://www.youtube.com/watch?v=...")` returns
  `ResolveResult` with a direct media URL.
- **Key steps:**
  1. Build command: `Command::new("yt-dlp").arg("-J").arg("--no-warnings").arg(url)`
  2. Add timeout: `.kill_on_drop(true)` + `tokio::time::timeout(Duration::from_secs(60), child.wait())`
  3. Capture stdout → parse as JSON
  4. Capture stderr → include in `ResolveError::Network` on failure
  5. Map exit codes: 0 = success, 1 = no video found, other = network error

### T-4.2 yt-dlp JSON output parsing
- **Crate:** `picast-resolver`
- **Depends on:** T-4.1
- **Effort:** 1.5 days
- **Description:** Parse yt-dlp's JSON info dict. Extract: `url`, `formats`,
  `title`, `duration`, `subtitles`, `thumbnail`. Select the best H.264 format.
- **Acceptance:** Parsed `ResolveResult` contains correct `direct_url`, `category`,
  `mime_type`, and subtitle list.
- **Key steps:**
  1. Deserialize yt-dlp JSON: `serde_json::from_value::<yt_dlp::Info>(json)?`
  2. Define `struct YtdlpInfo` with relevant fields (url, formats, title, etc.)
  3. From `formats[]`: find best `vcodec^=avc1` + `acodec^=mp4a` combo
  4. Construct direct URL from selected format's `url` field
  5. If merged format: yt-dlp returns `url` pointing to `manifest_url` — may need HLS/DASH handling

### T-4.3 Format selection: force H.264
- **Crate:** `picast-resolver`
- **Depends on:** T-4.1
- **Effort:** 0.5 day
- **Description:** Pass `--format` flag to yt-dlp to force H.264 video selection.
  Fallback chain: best H.264 → best any codec → error.
- **Acceptance:** Resolved URLs always point to H.264 streams when available.
- **Key steps:**
  1. `--format "bestvideo[vcodec^=avc1]+bestaudio/best[vcodec^=avc1]/best"`
  2. Verify in parsed JSON that selected format's `vcodec` starts with `avc1`
  3. If no H.264 available: try `av1` or `vp9` software decode (limited resolution)
  4. Log a warning if forced to use non-H.264 codec

### T-4.4 Tor SOCKS5h proxy routing for yt-dlp
- **Crate:** `picast-resolver`
- **Depends on:** T-4.1, T-1.3
- **Effort:** 1 day
- **Description:** Configure yt-dlp to route through Tor SOCKS proxy with
  stream isolation. Use `--proxy socks5h://username@host:port/` format.
- **Acceptance:** yt-dlp requests appear on Tor circuits; DNS doesn't leak.
- **Key steps:**
  1. Get `SocksProxy` from `TorManager`
  2. Generate stream isolation ID: `picast-<sha256(domain)>`
  3. `--proxy socks5h://picast-{hash}@127.0.0.1:9050/`
  4. `socks5h` (h = remote DNS) ensures DNS goes through Tor, not local resolver
  5. Test: resolve a URL, check Tor control port for circuit with matching username

### T-4.5 Resolution cache with TTL
- **Crate:** `picast-resolver`
- **Depends on:** T-4.2
- **Effort:** 1.5 days
- **Description:** Add a SQLite-backed cache for resolved URLs. Avoids repeated
  yt-dlp invocations for the same URL within the TTL window (10 minutes).
- **Acceptance:** Second call to `resolve()` with same URL returns cached result
  without spawning yt-dlp.
- **Key steps:**
  1. Create table: `resolved_urls (source_url TEXT PK, direct_url TEXT, category TEXT, mime_type TEXT, content_length INTEGER, used_tor BOOLEAN, resolved_at TEXT)`
  2. On `resolve()`: check cache first — `SELECT * FROM resolved_urls WHERE source_url = ? AND resolved_at > datetime('now', '-10 minutes')`
  3. Cache hit → return stored `ResolveResult`
  4. Cache miss → resolve via yt-dlp → INSERT into cache
  5. Cleanup: `DELETE FROM resolved_urls WHERE resolved_at < datetime('now', '-1 hour')` on each resolve

### T-4.6 Subtitle extraction
- **Crate:** `picast-resolver`
- **Depends on:** T-4.1
- **Effort:** 1 day
- **Description:** Configure yt-dlp to extract available subtitles. Parse
  subtitle list from JSON output. Download subtitle files to temp directory.
- **Acceptance:** `ResolveResult` includes `available_subtitles: ["en", "es", "fr"]`.
- **Key steps:**
  1. `--write-subs --sub-langs "en,es,fr,de" --sub-format vtt`
  2. Parse `subtitles` field from yt-dlp JSON: map of language code → subtitle URL list
  3. Add `available_subtitles: Vec<String>` to `ResolveResult`
  4. Download subtitle files to `/tmp/picast-subs/{session_id}/`
  5. Clean up subtitle files on session stop

### T-4.7 Direct media passthrough
- **Crate:** `picast-resolver`
- **Depends on:** T-0.5
- **Effort:** 0.5 day
- **Description:** URLs classified as `DirectMedia` or `HlsManifest` or
  `DashManifest` skip yt-dlp entirely and return immediately.
- **Acceptance:** `resolve("http://example.com/video.mp4")` returns in <1ms
  without spawning yt-dlp.
- **Key steps:**
  1. `classify()` already handles this — check `category` before spawning yt-dlp
  2. For `DirectMedia`: `direct_url = source_url`, `mime_type` from extension
  3. For `HlsManifest`/`DashManifest`: pass through to GStreamer's adaptive demuxer
  4. Only `WebPage` category triggers yt-dlp resolution

### T-4.8 Error handling and timeout
- **Crate:** `picast-resolver`
- **Depends on:** T-4.1
- **Effort:** 0.5 day
- **Description:** Map yt-dlp exit codes and stderr to `ResolveError` variants.
  Kill subprocess on timeout.
- **Acceptance:** All error paths return descriptive `ResolveError` variants.
- **Key steps:**
  1. Exit code 0 → `Ok(ResolveResult)`
  2. Exit code 1 + "Unsupported URL" → `ResolveError::NoMediaFound`
  3. Exit code 1 + "HTTP Error" → `ResolveError::Network`
  4. Timeout (60s) → kill child → `ResolveError::Network("yt-dlp timed out")`
  5. Binary not found → `ResolveError::TorUnavailable("yt-dlp not installed")` (or new variant)

### T-4.9 Resolver integration test
- **Crate:** `picast-resolver`
- **Depends on:** T-4.3, T-4.4, T-4.5
- **Effort:** 1 day
- **Description:** End-to-end test: resolve a real URL through Tor, verify
  result fields, test cache hit on second call. Skip in CI.
- **Acceptance:** Test resolves YouTube URL, gets H.264 direct URL, cache
  returns same result on second call.
- **Key steps:**
  1. `#[tokio::test] async fn test_resolve_youtube()`
  2. Requires running Tor + yt-dlp
  3. `resolver.resolve("https://www.youtube.com/watch?v=dQw4w9WgXcQ").await?`
  4. Verify `category == WebPage`, `direct_url` contains `googlevideo.com`
  5. Second resolve → cache hit (verify no new subprocess spawned)

---

## Phase 5 — Session Manager

### T-5.1 Trait-object wiring
- **Crate:** `picast-session`
- **Depends on:** T-1.3, T-2.6, T-3.4, T-4.2
- **Effort:** 1 day
- **Description:** Replace `Arc<()>` stubs in `SessionManager` with real
  `Arc<dyn ResolverTrait>`, `Arc<dyn PlaybackTrait>`, `Arc<dyn DisplayTrait>`,
  `Arc<dyn TorTrait>`. Update constructor.
- **Acceptance:** `SessionManager::new(resolver, playback, display, tor)` compiles
  and stores trait objects.
- **Key steps:**
  1. Update `SessionManager` struct: add `resolver`, `playback`, `display`, `tor` fields
  2. Implement `From` for concrete types → trait objects
  3. Ensure all trait methods are `async` and `Send`
  4. Write test: create mock implementations, verify wiring

### T-5.2 Load flow: resolve → create session → play
- **Crate:** `picast-session`
- **Depends on:** T-5.1
- **Effort:** 2 days
- **Description:** Implement `load()` to: (1) call `resolver.resolve()`,
  (2) create `MediaSession` in SQLite, (3) call `playback.play()`, (4) return
  session ID.
- **Acceptance:** `load("https://youtube.com/...")` returns a UUID; SQLite
  contains the session; playback starts.
- **Key steps:**
  1. `self.resolver.resolve(url).await?` → `ResolveResult`
  2. Create `MediaSession { source_url: url, resolved_url: Some(result.direct_url), state: Resolving, .. }`
  3. Insert into SQLite: `INSERT INTO sessions (...) VALUES (...)`
  4. Update state: `Resolving → Buffering`
  5. `self.playback.play(&result.direct_url).await?`
  6. Update state: `Buffering → Playing`
  7. Return `session.id`

### T-5.3 State machine implementation
- **Crate:** `picast-session`
- **Depends on:** T-5.2
- **Effort:** 2 days
- **Description:** Implement the 7-state state machine with valid transitions
  only. Invalid transitions return `SessionError`.
- **Acceptance:** Attempting `pause()` when `Idle` returns error; valid
  transitions update state in SQLite and broadcast via watch channel.
- **Key steps:**
  1. Define transition table: `(from_state, command) → Option<to_state>`
  2. `Idle + load → Resolving`, `Resolving + resolved → Buffering`, etc.
  3. Before each command: `validate_transition(current_state, command)?`
  4. After transition: `UPDATE sessions SET state = ? WHERE id = ?`
  5. Broadcast: `watch_tx.send(current_state)?`

### T-5.4 Play/Pause/Stop/Seek/SetVolume delegation
- **Crate:** `picast-session`
- **Depends on:** T-5.3
- **Effort:** 1.5 days
- **Description:** Implement each command method: validate state, delegate to
  subsystem, update SQLite, broadcast state change.
- **Acceptance:** Each command produces correct state transition and subsystem call.
- **Key steps:**
  1. `play()`: validate `Loaded/Paused` → `self.playback.resume()` → `Playing`
  2. `pause()`: validate `Playing/Buffering` → `self.playback.pause()` → `Paused`
  3. `stop()`: any state → `self.playback.stop()` → `Idle`, optionally delete session
  4. `seek()`: validate `Playing/Paused` → `self.playback.seek()` → `Seeking` → `Playing`
  5. `set_volume()`: any state → `self.playback.set_volume()` → update `volume` in SQLite

### T-5.5 Watch channel for state broadcasting
- **Crate:** `picast-session`
- **Depends on:** T-5.3
- **Effort:** 0.5 day
- **Description:** Add `tokio::sync::watch` channel to `SessionManager`. Protocol
  handlers subscribe to receive real-time state updates.
- **Acceptance:** Protocol handler receives state update within 10ms of transition.
- **Key steps:**
  1. `watch_tx: watch::Sender<MediaSession>`, `watch_rx: watch::Receiver<MediaSession>`
  2. On state transition: `watch_tx.send(updated_session)?`
  3. Expose `pub fn subscribe(&self) -> watch::Receiver<MediaSession>`
  4. Protocol handlers: `tokio::spawn(async { while rx.changed().await.is_ok() { ... } })`

### T-5.6 Session cleanup and persistence
- **Crate:** `picast-session`
- **Depends on:** T-5.2
- **Effort:** 1 day
- **Description:** On startup, clean up stale sessions (>24h). On stop,
  delete session or mark as stopped. Ensure SQLite is in WAL mode for
  concurrent access.
- **Acceptance:** After process restart, stale sessions are cleaned; active
  sessions are recoverable.
- **Key steps:**
  1. `SessionManager::new()`: `DELETE FROM sessions WHERE updated_at < datetime('now', '-24 hours')`
  2. Enable WAL mode: `PRAGMA journal_mode=WAL`
  3. On `stop()`: `DELETE FROM sessions WHERE id = ?`
  4. On process start: check if any session is in `Playing` → set to `Idle` (crash recovery)

### T-5.7 Thread safety for concurrent access
- **Crate:** `picast-session`
- **Depends on:** T-5.4
- **Effort:** 0.5 day
- **Description:** Ensure `SessionManager` is safe for concurrent access from
  HTTP, WebSocket, and DLNA handlers. Use `Arc<Mutex<SessionManager>>` or
  internal `Mutex` per field.
- **Acceptance:** Multiple concurrent API calls don't cause data races or SQLite
  corruption.
- **Key steps:**
  1. Wrap `SessionManager` in `Arc<Mutex<SessionManager>>` (coarse-grained)
  2. Or: separate `Mutex<Connection>` for SQLite, atomic state for `PlayerState`
  3. Write concurrent test: `tokio::join!(session.load(url1), session.load(url2))`
  4. Second load should return `409 Conflict` (single session)

---

## Phase 6 — Protocol Servers

### T-6.1 HTTP API: POST /api/cast
- **Crate:** `picast-protocols`
- **Depends on:** T-5.2
- **Effort:** 1 day
- **Description:** Implement the `/api/cast` endpoint using `hyper`. Parse JSON
  body, delegate to `session.load()`, return `202 Accepted`.
- **Acceptance:** `curl -X POST http://localhost:8585/api/cast -d '{"url":"..."}'`
  returns `{"sessionId":"...","status":"resolving"}`.
- **Key steps:**
  1. Parse request body: `serde_json::from_slice::<CastRequest>(&body)?`
  2. Validate `url` field is present and valid URI
  3. `session.load(url).await?` → `Uuid`
  4. Return `202 Accepted` with `{"sessionId": id, "status": "resolving"}`
  5. Handle: `400` (bad URL), `409` (session active), `422` (resolution failed), `503` (pipeline error)

### T-6.2 HTTP API: POST /api/stop
- **Crate:** `picast-protocols`
- **Depends on:** T-5.4
- **Effort:** 0.5 day
- **Description:** Implement `/api/stop`. Stop current session, release resources.
- **Acceptance:** `POST /api/stop` returns `200 OK` with `{"status":"idle"}`.
- **Key steps:**
  1. Parse optional `sessionId` from body
  2. `session.stop(session_id).await?`
  3. Return `200 OK` with previous session ID
  4. `404` if no active session

### T-6.3 HTTP API: POST /api/pause
- **Crate:** `picast-protocols`
- **Depends on:** T-5.4
- **Effort:** 0.5 day
- **Description:** Implement `/api/pause` to toggle pause state.
- **Acceptance:** `POST /api/pause` when playing returns `{"status":"paused"}`.
  When paused, returns `{"status":"playing"}`.
- **Key steps:**
  1. `session.pause(session_id).await?`
  2. Return current state + position + duration
  3. `409` if no active session

### T-6.4 HTTP API: POST /api/seek
- **Crate:** `picast-protocols`
- **Depends on:** T-5.4
- **Effort:** 0.5 day
- **Description:** Implement `/api/seek` with absolute and relative modes.
- **Acceptance:** `POST /api/seek -d '{"seconds":120}'` seeks to 2:00.
- **Key steps:**
  1. Parse `SeekRequest { seconds: f64, mode: Option<String> }`
  2. Default mode: `"absolute"`
  3. If relative: add to current position
  4. Validate bounds: 0 ≤ position ≤ duration
  5. `session.seek(id, position_ms).await?`

### T-6.5 HTTP API: GET /api/status
- **Crate:** `picast-protocols`
- **Depends on:** T-5.4
- **Effort:** 1 day
- **Description:** Implement `/api/status` returning full session state as JSON.
  Includes all fields from the SPECIFICATION.
- **Acceptance:** `GET /api/status` returns complete JSON matching the spec.
- **Key steps:**
  1. `session.status(session_id).await?`
  2. Map `MediaSession` → `StatusResponse` with all spec fields
  3. Include `bufferPercent` from `playback.buffer_health()`
  4. Include `videoCodec`, `videoResolution`, `audioCodec` from pipeline queries
  5. When idle: return `{"sessionId": null, "status": "idle"}`

### T-6.6 HTTP API: POST /api/volume
- **Crate:** `picast-protocols`
- **Depends on:** T-5.4
- **Effort:** 0.5 day
- **Description:** Implement `/api/volume` to set volume and mute state.
- **Acceptance:** `POST /api/volume -d '{"level":0.5}'` sets volume to 50%.
- **Key steps:**
  1. Parse `VolumeRequest { level: Option<f64>, muted: Option<bool> }`
  2. Validate: `0.0 ≤ level ≤ 1.0`
  3. `session.set_volume(id, (level * 100.0) as u8).await?`

### T-6.7 CORS headers for browser extension
- **Crate:** `picast-protocols`
- **Depends on:** T-6.1
- **Effort:** 0.5 day
- **Description:** Add `Access-Control-Allow-Origin: *` and other CORS headers
  to all HTTP responses. Handle OPTIONS preflight requests.
- **Acceptance:** Browser extension can make cross-origin requests to the API.
- **Key steps:**
  1. Add CORS middleware to hyper service
  2. Set headers: `Access-Control-Allow-Origin: *`, `Allow-Methods: GET, POST, OPTIONS`, `Allow-Headers: Content-Type`
  3. Handle `OPTIONS` requests with `204 No Content`
  4. Apply to all `/api/*` routes

### T-6.8 WebSocket server
- **Crate:** `picast-protocols`
- **Depends on:** T-5.5
- **Effort:** 3 days
- **Description:** Implement WebSocket server on port 8586 using `tokio-tungstenite`.
  Accept connections, parse client messages, broadcast state changes.
- **Acceptance:** WebSocket client connects, sends `CAST` message, receives
  `MEDIA_STATUS` updates.
- **Key steps:**
  1. TCP listener on `0.0.0.0:8586`
  2. WebSocket upgrade via `tokio-tungstenite::accept_async()`
  3. Maintain `Arc<Mutex<Vec<WebSocketSender>>>` for connected clients
  4. Parse incoming messages: `CAST`, `STOP`, `PAUSE`, `SEEK`, `VOLUME`, `SUBTITLE`
  5. Delegate to `SessionManager` methods
  6. Subscribe to `watch` channel: broadcast `MEDIA_STATUS` to all clients
  7. Ping/pong every 30s; disconnect unresponsive clients after 10s

### T-6.9 WebSocket: RESOLVE_PROGRESS messages
- **Crate:** `picast-protocols`
- **Depends on:** T-6.8, T-4.1
- **Effort:** 1 day
- **Description:** During yt-dlp resolution, send periodic `RESOLVE_PROGRESS`
  messages to WebSocket clients. Parse yt-dlp stderr for progress indication.
- **Acceptance:** WebSocket client receives `RESOLVE_PROGRESS` every ~5s during
  resolution.
- **Key steps:**
  1. Capture yt-dlp stderr line-by-line
  2. Parse common patterns: "Downloading webpage", "Extracting info", etc.
  3. Map to `phase` enum values
  4. Send `RESOLVE_PROGRESS` to all WebSocket clients
  5. Also send `RESOLVE_PROGRESS` via HTTP (polling) as fallback

### T-6.10 DLNA via gmediarender
- **Crate:** `picast-protocols`
- **Depends on:** T-5.4
- **Effort:** 3 days
- **Description:** Spawn `gmediarender` as a subprocess with a custom GStreamer
  pipeline string that matches PiCast's V4L2 + kmssink configuration. Monitor
  state changes and synchronize with `SessionManager`.
- **Acceptance:** VLC discovers PiCast as a renderer; casting a URL from VLC
  plays video on the Pi's HDMI output.
- **Key steps:**
  1. Spawn `gmediarender -f "PiCast" --gstout-audiosink=alsasink --gstout-videosink=kmssink`
  2. Wait for SSDP advertisement
  3. Monitor D-Bus or GStreamer bus for state changes
  4. On `SetAVTransportURI`: extract URL, call `session.load()`
  5. On `Play`/`Pause`/`Stop`: call corresponding session methods
  6. Handle gmediarender crashes: restart subprocess

### T-6.11 HTTP API integration tests
- **Crate:** `picast-protocols`
- **Depends on:** T-6.1 through T-6.7
- **Effort:** 1 day
- **Description:** Integration tests using `reqwest` against a real HTTP server
  with a mock session manager.
- **Acceptance:** All HTTP endpoints tested; error cases covered.
- **Key steps:**
  1. Spin up `HttpApiServer` on `127.0.0.1:18585` with mock session manager
  2. Test `POST /api/cast` with valid and invalid URLs
  3. Test `POST /api/stop`, `/api/pause`, `/api/seek`, `/api/volume`
  4. Test `GET /api/status` in various states
  5. Test CORS headers on all responses
  6. Test error codes: 400, 404, 409, 422, 503

---

## Phase 7 — Server Orchestration

### T-7.1 Real component initialization
- **Crate:** `picast-server`
- **Depends on:** T-1.4, T-2.4, T-3.4, T-4.2, T-5.4, T-6.1
- **Effort:** 1 day
- **Description:** Replace all `Arc::new(())` stubs in `main.rs` with real
  component construction. Handle errors with clear diagnostics.
- **Acceptance:** `picast-server` binary starts and initializes all subsystems.
- **Key steps:**
  1. `TorManager::new(&config.tor_socks)` → `ensure_running()`
  2. `DisplayManager::new("/dev/dri/card0")`
  3. `PlaybackEngine::new(pipeline_config)`
  4. `Resolver::new(tor.clone())`
  5. `SessionManager::new(&db_path, resolver, playback, display, tor)`
  6. `HttpApiServer::new(&config.http_addr, session.clone())`
  7. `WebSocketServer::new(&config.ws_addr, session.clone())`
  8. `DlnaRenderer::new(&config.dlna_name, session.clone())`

### T-7.2 Task spawning and concurrent execution
- **Crate:** `picast-server`
- **Depends on:** T-7.1
- **Effort:** 1 day
- **Description:** Spawn each protocol server as a `tokio::spawn` task. All
  tasks receive the shutdown signal via `broadcast::Receiver`.
- **Acceptance:** All three protocol servers run concurrently; none blocks the others.
- **Key steps:**
  1. `tokio::spawn(http.start(shutdown_rx.resubscribe()))`
  2. `tokio::spawn(ws.start(shutdown_rx.resubscribe()))`
  3. `tokio::spawn(dlna.start(shutdown_rx.resubscribe()))`
  4. Wait for shutdown signal
  5. Broadcast shutdown to all tasks

### T-7.3 Graceful shutdown sequence
- **Crate:** `picast-server`
- **Depends on:** T-7.2
- **Effort:** 1 day
- **Description:** On SIGINT/SIGTERM: stop playback → release display → kill
  Tor → wait for protocol tasks to finish → exit.
- **Acceptance:** Shutdown completes in <5s with no orphan processes.
- **Key steps:**
  1. Receive signal → `shutdown_tx.send(())`
  2. `session.stop()` → stop GStreamer pipeline
  3. `display.release()` → release DRM master
  4. `tor.shutdown()` → SIGTERM Tor process
  5. `tokio::join!(http_task, ws_task, dlna_task)` with 10s timeout
  6. Force kill any remaining tasks after timeout

### T-7.4 Startup ordering validation
- **Crate:** `picast-server`
- **Depends on:** T-7.1
- **Effort:** 0.5 day
- **Description:** Enforce sequential startup: Tor → Display → Playback →
  Resolver → Session → Protocols. Each step must succeed before the next.
  On failure: log clear error and exit.
- **Acceptance:** If Tor fails to start, the binary exits with error immediately
  rather than hanging.
- **Key steps:**
  1. `tor.ensure_running().await?` → if fails, log and exit
  2. `DisplayManager::new()` → if fails, log and exit
  3. Continue through chain
  4. Each failure: `tracing::error!()` with actionable message

### T-7.5 Health check endpoint
- **Crate:** `picast-server`
- **Depends on:** T-7.1
- **Effort:** 0.5 day
- **Description:** Add `GET /api/health` endpoint that returns status of all
  subsystems: Tor (connected?), Display (DRM master?), Playback (ready?),
  Resolver (yt-dlp available?).
- **Acceptance:** `GET /api/health` returns `{"tor":"ok","display":"ok","playback":"ok","resolver":"ok"}`.
- **Key steps:**
  1. Query each subsystem's health
  2. `tor.health_check()`, `display.resolution()`, playback status, `which yt-dlp`
  3. Return `200 OK` if all healthy, `503` if any degraded

### T-7.6 Configuration file support
- **Crate:** `picast-server`
- **Depends on:** T-0.1
- **Effort:** 1 day
- **Description:** Add TOML config file support alongside env vars. Search
  `/etc/picast/picast.conf`, `~/.config/picast/picast.conf`, `./picast.conf`.
- **Acceptance:** `picast.conf` settings override defaults; env vars override config file.
- **Key steps:**
  1. Add `toml` dependency
  2. Define `Config` struct with `Deserialize`
  3. Load: env vars > config file > defaults
  4. Validate on startup: ports in range, paths exist, etc.

### T-7.7 End-to-end smoke test on Pi
- **Crate:** `picast-server`
- **Depends on:** T-7.3, T-7.4
- **Effort:** 1 day
- **Description:** Full integration test on Raspberry Pi: boot → start PiCast →
  cast YouTube URL → verify HDMI output → stop → clean shutdown.
- **Acceptance:** Video appears on HDMI; API responds correctly; shutdown is clean.
- **Key steps:**
  1. Build `picast-server` for aarch64
  2. Copy to Pi, run with `./picast-server`
  3. `curl POST /api/cast -d '{"url":"https://www.youtube.com/watch?v=dQw4w9WgXcQ"}'`
  4. Verify video on HDMI monitor
  5. `curl POST /api/pause`, `/api/seek`, `/api/stop`
  6. Ctrl+C → verify clean shutdown in logs

---

## Phase 8 — Browser Extension Production

### T-8.1 Generate extension icons
- **Crate:** `src/extension/`
- **Depends on:** nothing
- **Effort:** 0.5 day
- **Description:** Create `icon16.png`, `icon48.png`, `icon128.png` for the
  browser extension. Use a simple, recognizable PiCast logo.
- **Acceptance:** Icons appear in Chrome/Firefox extension management UI.
- **Key steps:**
  1. Design simple logo (cast icon + Pi silhouette, or abstract)
  2. Export at 16×16, 48×48, 128×128
  3. Place in `src/extension/icons/`
  4. Verify `manifest.json` references match filenames

### T-8.2 Content script for page URL detection
- **Crate:** `src/extension/`
- **Depends on:** nothing
- **Effort:** 2 days
- **Description:** Inject a content script into web pages that detects `<video>`
  and `<source>` elements, extracts their `src` attributes, and reports them
  to the background service worker.
- **Acceptance:** On a page with a `<video>` element, the extension's popup
  shows the video URL as a castable item.
- **Key steps:**
  1. Create `src/extension/src/content.js`
  2. `document.querySelectorAll('video, source, iframe')` → extract URLs
  3. Also detect `video.src`, `video.currentSrc`, `source.src`
  4. Send detected URLs to background: `chrome.runtime.sendMessage({ type: 'DETECTED_MEDIA', urls })`
  5. Watch for dynamically added video elements via `MutationObserver`
  6. Register content script in `manifest.json`: `"content_scripts": [{"matches": ["<all_urls>"], "js": ["src/content.js"]}]`

### T-8.3 Firefox Manifest V2/V3 compatibility
- **Crate:** `src/extension/`
- **Depends on:** nothing
- **Effort:** 1 day
- **Description:** Ensure the extension works on both Chrome (Manifest V3) and
  Firefox (V2 or V3). Use `browser.*` namespace with `chrome.*` fallback.
  Create separate build configs if needed.
- **Acceptance:** Extension loads and works in both Chrome and Firefox.
- **Key steps:**
  1. Replace `chrome.*` calls with `browser.*` + `chrome.*` fallback wrapper
  2. Or: use `webextension-polyfill` npm package
  3. Firefox supports MV2 with `browser.webRequest.onBeforeRequest`
  4. Firefox MV3 support is still evolving — test both
  5. Create `manifest-firefox.json` if necessary (different permissions model)
  6. Build script: `cp manifest-firefox.json manifest.json` for Firefox build

### T-8.4 Popup: detected media list with cast buttons
- **Crate:** `src/extension/`
- **Depends on:** T-8.2
- **Effort:** 1 day
- **Description:** Update popup to show intercepted media URLs with a "Cast"
  button for each. Show URL type (direct, HLS, page) and confidence level.
- **Acceptance:** Popup displays list of detected media; clicking "Cast" sends
  URL to PiCast server.
- **Key steps:**
  1. Query background: `chrome.runtime.sendMessage({ type: 'GET_MEDIA_QUEUE', tabId })`
  2. Render list with type badges and cast buttons
  3. On "Cast" click: `chrome.runtime.sendMessage({ type: 'CAST', url, title })`
  4. Show "Casting..." status after successful cast

### T-8.5 Popup: playback controls
- **Crate:** `src/extension/`
- **Depends on:** T-8.4
- **Effort:** 1 day
- **Description:** Add play/pause, stop, seek bar, and volume slider to the
  popup. All controls wired to the PiCast HTTP API.
- **Acceptance:** Popup controls work: pause pauses, stop stops, seek bar seeks.
- **Key steps:**
  1. Play/pause button: `POST /api/pause`
  2. Stop button: `POST /api/stop`
  3. Seek bar: range input, `POST /api/seek` on change
  4. Volume slider: range input 0-100, `POST /api/volume`
  5. Disable controls when no session is active

### T-8.6 Popup: WebSocket status updates
- **Crate:** `src/extension/`
- **Depends on:** T-8.5
- **Effort:** 1 day
- **Description:** Connect popup to PiCast's WebSocket server for real-time
  status updates (position, buffer %, state changes). Update UI without polling.
- **Acceptance:** Popup shows live position counter and buffer percentage during
  playback without page refresh.
- **Key steps:**
  1. `new WebSocket('ws://picast.local:8586/ws')` on popup open
  2. Handle `MEDIA_STATUS` messages: update position, state, buffer
  3. Handle `RESOLVE_PROGRESS` messages: show "Resolving..." with phase
  4. Handle `ERROR` messages: show error notification
  5. Reconnect on disconnect with exponential backoff

### T-8.7 Options page: full settings
- **Crate:** `src/extension/`
- **Depends on:** nothing
- **Effort:** 0.5 day
- **Description:** Complete the options page with all configurable settings:
  Pi address, port, Tor mode, auto-detect toggle, default cast behavior.
- **Acceptance:** Settings persist across browser restarts; changing Pi address
  updates API calls.
- **Key steps:**
  1. `chrome.storage.local.set/get` for all settings
  2. Pi address: text input (default: `picast.local`)
  3. Port: number input (default: `8585`)
  4. Tor mode: dropdown (`full`, `resolution-only`, `off`)
  5. Auto-detect: toggle (automatically detect media on page load)
  6. Test connection button: `GET /api/health`

### T-8.8 Chrome and Firefox packaging
- **Crate:** `src/extension/`
- **Depends on:** T-8.3
- **Effort:** 1 day
- **Description:** Package the extension for Chrome Web Store and Firefox Add-ons.
  Create build scripts for both targets.
- **Acceptance:** Extension loads in Chrome via developer mode; loads in Firefox
  via `about:debugging`.
- **Key steps:**
  1. Chrome: `zip -r picast-chrome.zip src/extension/*` (excluding Firefox-specific files)
  2. Firefox: create `manifest-firefox.json`, adjust permissions, zip
  3. Test load in both browsers
  4. Create `scripts/build-extension.sh` for automated packaging
  5. Document loading instructions in `docs/extension/`

### T-8.9 Error handling in extension
- **Crate:** `src/extension/`
- **Depends on:** T-8.4
- **Effort:** 0.5 day
- **Description:** Handle API unreachable, timeout, and server error cases
  gracefully in the extension UI.
- **Acceptance:** When PiCast server is unreachable, popup shows "PiCast not
  found" instead of a blank or error state.
- **Key steps:**
  1. Catch `fetch()` errors → show "PiCast not found at [address]:[port]"
  2. Timeout after 5s → show "Connection timed out"
  3. Retry with exponential backoff (1s, 2s, 4s, max 30s)
  4. Show last known status when offline
  5. "Retry" button to manually re-check connection

---

## Phase 9 — Testing & Quality Assurance

### T-9.1 Unit test coverage for all crates
- **Crate:** all
- **Depends on:** T-7.1
- **Effort:** 3 days
- **Description:** Achieve ≥80% line coverage for `tor`, `resolver`, `session`
  crates. ≥60% for `playback`, `display` (harder to test without hardware).
- **Acceptance:** `cargo tarpaulin --workspace` reports ≥75% average coverage.
- **Key steps:**
  1. `picast-tor`: test all `TorError` variants, `SocksProxy` methods, stream ID generation
  2. `picast-resolver`: test `classify()` exhaustively, cache TTL, error mapping
  3. `picast-session`: test state machine transitions (valid and invalid), persistence, watch channel
  4. `picast-playback`: test `PipelineConfig` serialization, `BufferHealth` defaults
  5. `picast-display`: test mock mode, `DrmPlane`/`DrmCrtc` construction
  6. `picast-protocols`: test HTTP request/response types, WebSocket message parsing

### T-9.2 Integration test: full playback flow
- **Crate:** `tests/`
- **Depends on:** T-7.7
- **Effort:** 2 days
- **Description:** End-to-end test: load URL → resolve → play → pause → seek →
  stop. Uses real components with mock display and (optionally) real Tor.
- **Acceptance:** Test passes with mock display; all state transitions verified.
- **Key steps:**
  1. Create `tests/integration.rs`
  2. Set up: `TorManager` (or mock), `DisplayManager::new("mock")`, `PlaybackEngine` (or mock), `Resolver` (or mock with canned response)
  3. `SessionManager::new(...)` with real SQLite
  4. `session.load(test_url).await?` → verify `status == Playing`
  5. `session.pause().await?` → verify `status == Paused`
  6. `session.seek(60_000).await?` → verify position
  7. `session.stop().await?` → verify `status == Idle`

### T-9.3 Pi hardware smoke test script
- **Crate:** `scripts/`
- **Depends on:** T-7.7
- **Effort:** 1 day
- **Description:** Automated smoke test script for Pi hardware. Tests: boot,
  Tor connectivity, HDMI output, API responses, clean shutdown.
- **Acceptance:** `./scripts/smoke-test.sh` exits 0 on a Pi 4 with all hardware connected.
- **Key steps:**
  1. Start `picast-server`
  2. Wait for `/api/health` → 200 OK
  3. Verify Tor: `curl --socks5 127.0.0.1:9050 https://check.torproject.org/`
  4. Cast test URL → verify `202 Accepted`
  5. Wait for `Playing` status
  6. Test pause/seek/stop
  7. Verify HDMI: `modetest -M vc4` shows active planes

### T-9.4 Network isolation verification
- **Crate:** `config/`
- **Depends on:** T-7.1
- **Effort:** 0.5 day
- **Description:** Verify iptables rules block all outbound traffic except
  through Tor. Test DNS leak prevention.
- **Acceptance:** `tcpdump` shows no non-SOCKS outbound connections during
  playback; DNS queries only to Tor's DNSPort.
- **Key steps:**
  1. Apply `config/iptables.rules`
  2. Start PiCast and play a video
  3. `tcpdump -i eth0 not port 9050 and not port 53` → should be empty
  4. Verify DNS goes through Tor: `dig +short @127.0.0.1 -p 5353 google.com`
  5. Test that direct HTTP fails: `curl --noproxy '*' http://example.com` → timeout

### T-9.5 Memory leak test
- **Crate:** all
- **Depends on:** T-7.7
- **Effort:** 1 day
- **Description:** Run 8-hour continuous playback session. Monitor RSS growth.
  Target: <10 MB/hour leak rate.
- **Acceptance:** RSS growth < 80 MB over 8 hours; no GStreamer pipeline leaks.
- **Key steps:**
  1. Start PiCast, cast a long video
  2. Log RSS every 60s: `ps -o rss= -p $(pidof picast-server)`
  3. After 8 hours: calculate leak rate
  4. Monitor GStreamer: `GST_TRACE=1` to track buffer allocations
  5. If leak detected: use `valgrind --leak-check=full` on x86 build

### T-9.6 Soak test: 100 cast/stop cycles
- **Crate:** all
- **Depends on:** T-7.7
- **Effort:** 1 day
- **Description:** Run 100 cast/stop cycles in a loop. Verify no resource
  exhaustion: GStreamer pipelines fully cleaned up, SQLite DB stays small,
  no fd leaks.
- **Acceptance:** After 100 cycles: RSS < 2× initial, open fds < 100, SQLite < 1 MB.
- **Key steps:**
  1. Script: `for i in $(seq 1 100); do curl POST /api/cast; sleep 5; curl POST /api/stop; sleep 1; done`
  2. Before/after: `lsof -p $(pidof picast-server) | wc -l` → fd count
  3. Before/after: `du -h /var/lib/picast/sessions.db`
  4. Monitor RSS trend

### T-9.7 Security audit checklist
- **Crate:** all
- **Depends on:** T-9.4
- **Effort:** 1 day
- **Description:** Walk through security checklist: no DNS leaks, circuit
  isolation works, iptables enforced, no root beyond DRM, no unnecessary
  services running.
- **Acceptance:** All checklist items pass; document findings.
- **Key steps:**
  1. Verify: all outbound via Tor SOCKS (iptables + tcpdump)
  2. Verify: DNS queries only to Tor DNSPort
  3. Verify: stream isolation (different domains → different circuits)
  4. Verify: DRM master is only PiCast (no X11/Wayland)
  5. Verify: process runs as `picast` user, not root (DRM via group membership)
  6. Verify: no unnecessary listening ports (only 8585, 8586, 49152, 9050)
  7. Verify: systemd service has `ProtectSystem`, `NoNewPrivileges`, etc.

### T-9.8 CI pipeline finalization
- **Crate:** `.github/`
- **Depends on:** T-9.1
- **Effort:** 1 day
- **Description:** Finalize GitHub Actions CI: x86 check + test, aarch64
  cross-compile check, clippy, rustfmt, security audit (`cargo audit`).
- **Acceptance:** CI runs on every PR; all checks must pass before merge.
- **Key steps:**
  1. `cargo check --workspace` (x86, no `hw` feature)
  2. `cargo test --workspace` (x86, no `hw` feature)
  3. `cargo check --target aarch64-unknown-linux-gnu --workspace`
  4. `cargo clippy --workspace -- -D warnings`
  5. `cargo fmt --check`
  6. `cargo audit` (security vulnerabilities)
  7. Branch protection: require all checks pass

---

## Phase 10 — Distribution & Documentation

### T-10.1 Setup script overhaul
- **Crate:** `scripts/`
- **Depends on:** T-7.3
- **Effort:** 1 day
- **Description:** Rewrite `scripts/setup.sh` for one-command install on
  fresh Raspberry Pi OS. Install all system deps, build PiCast, configure
  Tor, iptables, and systemd.
- **Acceptance:** On a fresh Pi OS Lite image, `curl -sSL setup.sh | bash`
  results in a running PiCast service.
- **Key steps:**
  1. `apt install build-essential libgstreamer1.0-dev ... tor yt-dlp`
  2. `cargo build --release --target aarch64-unknown-linux-gnu`
  3. Copy binary to `/usr/local/bin/picast-server`
  4. Install `config/picast.service` → `systemctl enable picast`
  5. Install `config/torrc` → `systemctl restart tor`
  6. Install `config/iptables.rules` → apply on boot
  7. Create `picast` user, add to `video` and `render` groups

### T-10.2 Debian package
- **Crate:** `scripts/`
- **Depends on:** T-10.1
- **Effort:** 2 days
- **Description:** Build a `.deb` package containing the binary, configs,
  systemd service, and postinst scripts for auto-configuration.
- **Acceptance:** `dpkg -i picast_0.1.0_arm64.deb` installs and starts PiCast.
- **Key steps:**
  1. Create `debian/` directory structure
  2. `DEBIAN/control`: Package, Version, Architecture, Depends, Description
  3. `DEBIAN/postinst`: add user, enable service, apply iptables
  4. `DEBIAN/prerm`: stop service
  5. Build: `dpkg-deb --build picast_0.1.0`
  6. Test on fresh Pi OS

### T-10.3 Pre-built SD card image
- **Crate:** `scripts/`
- **Depends on:** T-10.2
- **Effort:** 2 days
- **Description:** Create a flash-and-boot Raspberry Pi OS image with PiCast
  pre-installed. Compatible with Raspberry Pi Imager.
- **Acceptance:** Flash image to SD card → boot Pi → PiCast is running.
- **Key steps:**
  1. Start with Raspberry Pi OS Lite (64-bit) base image
  2. Use `pi-gen` or manual `chroot` to customize
  3. Install PiCast .deb package
  4. Disable desktop: `systemctl set-default multi-user.target`
  5. Enable: `picast.service`, `tor.service`
  6. Set hostname: `picast`
  7. Compress: `xz -z picast.img`
  8. Test: flash → boot → verify API responds

### T-10.4 README.md rewrite
- **Crate:** root
- **Depends on:** T-10.1
- **Effort:** 0.5 day
- **Description:** Rewrite README as a quick start guide: what PiCast is,
  hardware requirements, one-command install, extension install, cast first
  video.
- **Acceptance:** A new user can go from "never heard of PiCast" to "watching
  a casted video" using only the README.
- **Key steps:**
  1. One-paragraph description
  2. Hardware requirements: Pi 4B+, HDMI monitor, SD card, network
  3. Quick install: `curl | bash` command
  4. Extension install: Chrome Web Store / Firefox Add-on link
  5. Cast first video: open YouTube → click extension → "Cast"
  6. Architecture diagram (from ARCHITECTURE.md)

### T-10.5 User guide
- **Crate:** `docs/`
- **Depends on:** T-10.4
- **Effort:** 1 day
- **Description:** Write `docs/USER_GUIDE.md` covering: configuration options,
  troubleshooting (Tor won't start, no video, no audio), FAQ.
- **Acceptance:** Common issues have documented solutions.
- **Key steps:**
  1. Configuration: `picast.conf` format and all options
  2. Tor: checking circuit status, bridge configuration, bandwidth tips
  3. Display: selecting resolution, multi-monitor (unsupported), EDID issues
  4. Playback: codec support, subtitle configuration, ABR behavior
  5. Extension: installing, configuring Pi address, permissions
  6. DLNA: discovering from VLC, Home Assistant, Android
  7. Troubleshooting: `journalctl -u picast`, `GST_DEBUG`, `tor-log`

### T-10.6 Security hardening guide
- **Crate:** `docs/`
- **Depends on:** T-9.7
- **Effort:** 0.5 day
- **Description:** Write `docs/SECURITY.md` documenting the security model,
  iptables rules, Tor configuration, and physical security recommendations.
- **Acceptance:** Security reviewer can verify PiCast's security properties
  using this document.
- **Key steps:**
  1. Threat model: what PiCast defends against and what it doesn't
  2. iptables rules explanation (line by line)
  3. Tor configuration: circuit isolation, DNS leak prevention
  4. Process privileges: `picast` user, group memberships, capabilities
  5. Physical security: SD card encryption, UART disabled, GPIO locked
  6. Update policy: Tor updates, yt-dlp updates, PiCast updates

### T-10.7 Release checklist and GitHub Release
- **Crate:** root
- **Depends on:** T-10.3, T-10.4, T-10.5, T-10.6
- **Effort:** 0.5 day
- **Description:** Create release checklist. Tag v1.0.0. Upload binary, .deb,
  SD image, and SHA-256 checksums to GitHub Releases.
- **Acceptance:** GitHub Release page has all artifacts with checksums and
  release notes.
- **Key steps:**
  1. Tag: `git tag v1.0.0 -m "PiCast v1.0.0 release"`
  2. Build: binary, .deb, SD image, extension zip
  3. Checksums: `sha256sum * > SHA256SUMS`
  4. Release notes: features, known issues, upgrade instructions
  5. Upload all artifacts to GitHub Release

---

## Dependency Graph (Simplified)

```
T-0.1 ──► T-0.2 (cross-compile)
  │    ──► T-0.3 (CI)
  │    ──► T-0.4 (smoke tests)
  │    ──► T-0.5 (feature flags)
  │
  ├──► T-1.1 ──► T-1.2 ──► T-1.3 (stream isolation)
  │              │         ──► T-1.5 (control port)
  │              └──► T-1.4 (lifecycle)
  │                        ──► T-1.6 (integration test)
  │
  ├──► T-2.1 ──► T-2.2 (enumerate)
  │              ──► T-2.3 (connector)
  │              ──► T-2.5 (GBM)
  │              ──► T-2.6 (mock mode)
  │         T-2.2 + T-2.3 ──► T-2.4 (atomic modeset) ──► T-2.7 (Pi test)
  │
  ├──► T-3.1 ──► T-3.2 ──► T-3.3 (Tor proxy)
  │                     ──► T-3.4 (state transitions)
  │                              ──► T-3.5 (seek)
  │                              ──► T-3.9 (error recovery)
  │                              ──► T-3.10 (position/duration)
  │         T-3.1 ──► T-3.6 (volume)
  │         T-3.2 ──► T-3.7 (buffer health)
  │         T-3.2 ──► T-3.8 (sw decode fallback)
  │
  ├──► T-4.1 ──► T-4.2 (JSON parsing)
  │              ──► T-4.3 (H.264 forcing)
  │              ──► T-4.5 (cache) ──► T-4.6 (subtitles)
  │              ──► T-4.7 (direct passthrough)
  │              ──► T-4.8 (error handling)
  │         T-4.1 + T-1.3 ──► T-4.4 (Tor routing)
  │         T-4.3 + T-4.4 + T-4.5 ──► T-4.9 (integration test)
  │
  │    T-1.3 + T-2.6 + T-3.4 + T-4.2 ──► T-5.1 (wiring)
  │                                        ──► T-5.2 (load flow)
  │                                           ──► T-5.3 (state machine)
  │                                              ──► T-5.4 (commands)
  │                                              ──► T-5.5 (watch channel)
  │                                           ──► T-5.6 (cleanup)
  │                                           ──► T-5.7 (thread safety)
  │
  │    T-5.2 ──► T-6.1 (POST /api/cast)
  │    T-5.4 ──► T-6.2 (stop) ──► T-6.3 (pause) ──► T-6.4 (seek)
  │           ──► T-6.5 (status) ──► T-6.6 (volume)
  │    T-6.1 ──► T-6.7 (CORS)
  │    T-5.5 ──► T-6.8 (WebSocket) ──► T-6.9 (progress)
  │    T-5.4 ──► T-6.10 (DLNA) ──► T-6.11 (HTTP tests)
  │
  │    All Phase 6 ──► T-7.1 (init) ──► T-7.2 (spawn) ──► T-7.3 (shutdown)
  │                                    ──► T-7.4 (ordering) ──► T-7.5 (health)
  │                                    ──► T-7.6 (config)
  │                                    ──► T-7.7 (E2E test)
  │
  │    T-8.1 (icons) — independent
  │    T-8.2 (content script) ──► T-8.4 (media list) ──► T-8.5 (controls)
  │                                                       ──► T-8.6 (WebSocket)
  │    T-8.3 (Firefox compat) ──► T-8.8 (packaging)
  │    T-8.4 ──► T-8.9 (error handling)
  │    T-8.7 (options) — independent
  │
  │    T-7.7 ──► T-9.1 (unit tests) ──► T-9.2 (integration) ──► T-9.3 (smoke)
  │           ──► T-9.4 (network isolation) ──► T-9.7 (security audit)
  │           ──► T-9.5 (memory leak) ──► T-9.6 (soak test)
  │           ──► T-9.8 (CI finalization)
  │
  │    T-7.3 ──► T-10.1 (setup script) ──► T-10.2 (deb) ──► T-10.3 (image)
  │           ──► T-10.4 (README) ──► T-10.5 (user guide) ──► T-10.6 (security)
  │           ──► T-10.7 (release)
```

---

## Effort Summary

| Phase | Tasks | Total Effort |
|-------|-------|-------------|
| 0 — Build & CI | T-0.1 to T-0.5 | 3.5 days |
| 1 — Tor | T-1.1 to T-1.6 | 6 days |
| 2 — Display | T-2.1 to T-2.7 | 8 days |
| 3 — Playback | T-3.1 to T-3.10 | 13 days |
| 4 — Resolver | T-4.1 to T-4.9 | 9 days |
| 5 — Session | T-5.1 to T-5.7 | 9 days |
| 6 — Protocols | T-6.1 to T-6.11 | 12 days |
| 7 — Server | T-7.1 to T-7.7 | 6 days |
| 8 — Extension | T-8.1 to T-8.9 | 8 days |
| 9 — Testing | T-9.1 to T-9.8 | 10.5 days |
| 10 — Distribution | T-10.1 to T-10.7 | 7.5 days |
| **Total** | **72 tasks** | **~92 days** |
