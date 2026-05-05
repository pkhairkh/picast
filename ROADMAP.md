# PiCast Production Roadmap

**From scaffolding to a shippable Raspberry Pi 4B+ media casting appliance.**

> **Current state (as of 2026-05):** 7 Rust crates contain well-structured type
> definitions, error enums, and trait interfaces, but every method body is a stub
> returning errors or defaults. The browser extension has functional JS but no
> icons or content scripts. There are zero tests, zero CI/CD pipelines, and no
> cross-compilation setup. The documentation (7,000+ lines) is the most mature
> artifact in the project.
>
> **Target state:** A PiCast binary that boots on a Raspberry Pi 4B+, connects
> to Tor, accepts a URL from the browser extension, resolves it via yt-dlp,
> plays 1080p60 H.264 video through the zero-copy V4L2→DRM/KMS pipeline, and
> can be controlled via HTTP, WebSocket, and DLNA — all surviving a `cargo test`
> suite and an automated CI pipeline.

---

## Phase 0 — Build & CI Foundation

**Goal:** `cargo build` and `cargo test` run green in CI before any feature work begins.

**Why first:** Every subsequent phase assumes a working build. Right now, the
crates may not even compile against each other — the workspace `Cargo.toml`
dependencies are declared but the actual crate APIs they reference (`picast_tor`,
`picast_display`, etc.) are `Arc<()>` stubs in `server/main.rs`. A CI pipeline
catches regressions from day one.

| Milestone | Deliverable | Exit Criteria |
|-----------|-------------|---------------|
| 0.1 | Workspace compiles on `x86_64` (dev host) | `cargo check --workspace` exits 0 |
| 0.2 | Cross-compilation to `aarch64-unknown-linux-gnu` | `cargo check --target aarch64-unknown-linux-gnu --workspace` exits 0 |
| 0.3 | GitHub Actions CI workflow | On every push: `cargo check`, `cargo clippy`, `cargo test` for workspace + target |
| 0.4 | `cargo test` infrastructure | Every crate has a `tests/` module with at least one smoke test (even if it just tests `Default` impls) |
| 0.5 | Conditional compilation for Pi-specific deps | `gstreamer`, `drm`, `gbm`, `nix` behind `#[cfg(target_arch = "aarch64")]` gates so x86 dev works |

**Estimated effort:** 3–4 days

**Risks:**
- GStreamer Rust bindings may require system libs even for `cargo check` — solve
  with feature flags or stub-only x86 builds.
- Cross-compilation linker configuration (`linker = "aarch64-linux-gnu-gcc"`) may
  need `.cargo/config.toml` and sysroot setup.

---

## Phase 1 — Tor Daemon Integration

**Goal:** `picast-tor` can start/stop a real Tor daemon, verify the SOCKS5 proxy
is reachable, and provide the proxy address to other crates.

**Why second:** The resolver and playback engine both need the SOCKS proxy
address. Tor is the leaf dependency in the crate graph — it has no downstream
crate dependencies, making it the natural first implementation target.

| Milestone | Deliverable | Exit Criteria |
|-----------|-------------|---------------|
| 1.1 | `TorManager::ensure_running()` spawns `tor` process | Tor process visible in `ps`, SOCKS port listening |
| 1.2 | Startup health check | `ensure_running()` waits up to `startup_timeout_ms` for SOCKS port to accept a TCP connection |
| 1.3 | Stream isolation via SOCKS5 username | Each resolved domain gets a SHA-256 hash as SOCKS5 username; Tor's `IsolateSOCKSAuth` isolates circuits |
| 1.4 | Process lifecycle management | `shutdown()` sends SIGTERM, waits for exit; `Drop` impl kills on panic; auto-restart on unexpected exit |
| 1.5 | `CircuitHealth` monitoring | Background task queries Tor control port (`GETINFO circuit-status`) every 30s, populates `CircuitHealth` |
| 1.6 | Integration test | Test binary spawns Tor, verifies SOCKS5 connectivity, shuts down — all within 60s |

**Estimated effort:** 5–7 days

**Key API surface after Phase 1:**
```rust
let mut tor = TorManager::new("127.0.0.1:9050");
tor.ensure_running(30_000).await?;    // real Tor spawn + health check
let proxy = tor.socks();               // SocksProxy { host, port }
let health = tor.health_check().await?; // CircuitHealth with real metrics
tor.shutdown().await?;                 // SIGTERM + wait
```

**Risks:**
- Tor startup time is 10–30s on first run (directory fetch); subsequent starts
  are 3–10s. Tests need generous timeouts.
- Raspberry Pi's SD card I/O makes Tor directory cache slow — consider tmpfs
  for `DataDirectory`.

---

## Phase 2 — DRM/KMS Display Manager

**Goal:** `picast-display` opens `/dev/dri/card0`, becomes DRM master, enumerates
planes and CRTCs, and can set a mode on the HDMI connector.

**Why third:** The playback engine needs a working display to render video frames.
The `kmssink` GStreamer element handles plane assignment internally, but PiCast
needs the display manager to: (a) verify hardware at startup, (b) provide the
plane/CRTC configuration to the playback engine, and (c) render OSD on Plane 1.

| Milestone | Deliverable | Exit Criteria |
|-----------|-------------|---------------|
| 2.1 | Open DRM device + acquire master | `drmSetMaster()` succeeds on `/dev/dri/card0` |
| 2.2 | Enumerate resources | `planes()` returns real `DrmPlane` structs with IDs, formats, zpos; `crtcs()` returns real `DrmCrtc` structs |
| 2.3 | Auto-detect vc4 driver | Validate `driver-name == "vc4"`, fail with clear error if not Pi 4 |
| 2.4 | Connector detection | Find connected HDMI connector, read EDID, select preferred mode (1080p60) |
| 2.5 | Atomic modesetting | `acquire()` performs `drmModeAtomicCommit` to set CRTC mode and enable Plane 0 |
| 2.6 | GBM device initialization | Allocate GBM surface for Plane 1 (OSD overlay), verify `GBM_BO_USE_SCANOUT` |
| 2.7 | `release()` cleanup | Disable planes, release CRTC, drop DRM master |
| 2.8 | Off-screen test mode | `DisplayManager::new("mock")` skips real DRM for x86 unit testing |

**Estimated effort:** 7–10 days

**Key API surface after Phase 2:**
```rust
let mut display = DisplayManager::new("/dev/dri/card0")?;
let planes = display.planes()?;    // [DrmPlane { plane_id: 31, zpos: 0, .. }, ...]
let crtcs = display.crtcs()?;      // [DrmCrtc { crtc_id: 28, width: 1920, .. }]
display.acquire()?;                // atomic modeset on HDMI
let (w, h) = display.resolution()?; // (1920, 1080)
display.release()?;
```

**Risks:**
- Requires actual Pi hardware for integration tests; x86 dev uses mock mode.
- DRM master conflicts: if X11/Wayland is running, `drmSetMaster()` fails.
  The `picast.service` systemd unit must ensure no display server starts.
- GBM buffer allocation from CMA pool can fail under memory pressure — need
  fallback strategy or early CMA reservation via kernel cmdline.

---

## Phase 3 — GStreamer Playback Engine

**Goal:** `picast-playback` constructs and controls a real GStreamer pipeline
that plays H.264 video through V4L2 hardware decode and DRM/KMS direct display.

**Why fourth:** The playback engine is the core value proposition — it must work
before any protocol or session logic can be meaningful. It depends on `picast-tor`
(for SOCKS proxy address) and `picast-display` (for plane/CRTC configuration).

| Milestone | Deliverable | Exit Criteria |
|-----------|-------------|---------------|
| 3.1 | `gstreamer::init()` and pipeline construction | `PlaybackEngine::new()` initializes GStreamer and creates a `gst::Pipeline` |
| 3.2 | Full pipeline: `souphttpsrc → queue2 → h264parse → v4l2h264dec → kmssink` | Pipeline transitions to `Playing` state with a test H.264 URL |
| 3.3 | Tor proxy integration | `souphttpsrc` configured with SOCKS5 proxy from `TorManager`, stream isolation username set |
| 3.4 | Play/Pause/Resume/Stop | State transitions via `gst_element_set_state()`, confirmed by bus message |
| 3.5 | Seek | `gst_element_seek_simple()` with FLUSH_KEY_UNITS, position query after seek |
| 3.6 | Volume control | `volume` element inserted before `alsasink`, property set 0.0–1.0 |
| 3.7 | Buffer health monitoring | `queue2` buffering messages parsed, `BufferHealth` struct populated |
| 3.8 | Software decode fallback | If `v4l2h264dec` fails to negotiate, fall back to `avdec_h264 → videoconvert → kmssink` |
| 3.9 | Pipeline error recovery | `GST_MESSAGE_ERROR` on bus → clean pipeline teardown, return `PlaybackError` |
| 3.10 | Position/duration queries | `position_ms()` returns current position; duration available from `GST_QUERY_DURATION` |

**Estimated effort:** 12–15 days (largest single phase)

**Key GStreamer pipeline string:**
```
souphttpsrc location={url} socks5-proxy-ip={proxy_ip} socks5-proxy-port={proxy_port}
  socks5-proxy-username={stream_id}
! queue2 max-size-bytes=52428800 use-buffering=true
    buffering-threshold-high=80 buffering-threshold-low=10
! h264parse config-interval=-1
! v4l2h264dec io-mode=dmabuf capture-io-mode=dmabuf
! kmssink driver-name=vc4 plane-id=0 can-scale=true force-modesetting=true
```

**Risks:**
- `v4l2h264dec` DMA-BUF negotiation with `kmssink` may fail on some kernel
  versions — need fallback to `capture-io-mode=mmap` (copy path).
- GStreamer element linking is fragile — caps negotiation between `v4l2h264dec`
  (NV12 output) and `kmssink` must match HVS supported formats.
- Audio pipeline (`alsasink`) needs separate testing — Pi 4 HDMI audio vs.
  3.5mm jack routing differs.

---

## Phase 4 — Content Resolver (yt-dlp)

**Goal:** `picast-resolver` invokes yt-dlp as a subprocess, parses its JSON
output, and returns a `ResolveResult` with the direct media URL, format metadata,
and subtitle availability.

**Why fifth:** The resolver depends on Tor (for proxying yt-dlp requests) and
feeds the playback engine. It is the bridge between "user gives a YouTube URL"
and "playback engine gets an H.264 stream URL."

| Milestone | Deliverable | Exit Criteria |
|-----------|-------------|---------------|
| 4.1 | `yt-dlp -J <url>` subprocess invocation | `tokio::process::Command` runs yt-dlp with timeout (60s), captures stdout + stderr |
| 4.2 | JSON output parsing | Parse yt-dlp's `-J` output into structured `ResolveResult` with `direct_url`, `category`, `mime_type` |
| 4.3 | Format selection: force H.264 | `--format "bestvideo[vcodec^=avc1]+bestaudio/best[vcodec^=avc1]/best"` ensures H.264 |
| 4.4 | Tor SOCKS5h proxy routing | `--proxy socks5h://{username}@127.0.0.1:9050/` routes all yt-dlp traffic through Tor |
| 4.5 | Stream isolation hash | SHA-256 hash of the domain name used as SOCKS5 username for circuit isolation |
| 4.6 | Resolution cache | SQLite-backed cache: `(source_url, resolved_url, timestamp)` with 10-minute TTL |
| 4.7 | Subtitle extraction | `--write-subs --sub-langs en,es,fr,de --sub-format vtt` extracts available subtitles |
| 4.8 | Direct media passthrough | URLs classified as `DirectMedia` skip yt-dlp, return immediately |
| 4.9 | Error handling | yt-dlp exit codes mapped to `ResolveError` variants; timeout kills subprocess |
| 4.10 | Integration test | Resolve a real YouTube URL through Tor, verify `ResolveResult` fields |

**Estimated effort:** 8–10 days

**Key command template:**
```bash
yt-dlp -J --no-warnings \
  --proxy "socks5h://picast-<sha256-domain>@127.0.0.1:9050/" \
  --format "bestvideo[vcodec^=avc1]+bestaudio/best[vcodec^=avc1]/best" \
  --write-subs --sub-langs "en,es,fr,de" --sub-format vtt \
  --no-playlist \
  "https://www.youtube.com/watch?v=..."
```

**Risks:**
- yt-dlp Python startup is 5–15s on Pi's ARM CPU — may need warm process pool.
- yt-dlp extractors break frequently as websites change; need `pip install -U yt-dlp`.
- YouTube throttling of non-browser requests may require `--extractor-args "youtube:player_client=android"`.

---

## Phase 5 — Session Manager

**Goal:** `picast-session` implements the full state machine, wires resolver →
playback → display → Tor through trait objects, and persists state in SQLite.

**Why sixth:** The session manager is the central coordinator. It depends on all
four subsystems being implemented. Only after Phases 1–4 are complete can the
session manager do real work.

| Milestone | Deliverable | Exit Criteria |
|-----------|-------------|---------------|
| 5.1 | Trait-object wiring | `SessionManager::new(resolver, playback, display, tor)` stores `Arc<dyn Trait>` for each subsystem |
| 5.2 | `load()` full flow | `load(url)` → resolver.resolve() → create session → playback.play() → return session ID |
| 5.3 | State machine: 7-state transitions | `Idle → Resolving → Buffering → Playing → Paused → Seeking → Error` with valid transitions only |
| 5.4 | SQLite persistence | All state transitions written to `sessions` table; sessions survive process restart |
| 5.5 | Play/Pause/Stop/Seek/SetVolume | Each command validates current state, delegates to subsystem, updates SQLite |
| 5.6 | Watch channel for state push | `tokio::sync::watch` channel broadcasts state changes to protocol handlers |
| 5.7 | Session cleanup | Expired sessions (>24h) cleaned on startup; stop command deletes session |
| 5.8 | Concurrent access safety | `SessionManager` wrapped in `Arc<Mutex<>>` or uses internal `Mutex`; no data races under load |

**Estimated effort:** 7–9 days

**State machine diagram:**
```
          ┌──────────┐
          │   Idle   │◄────────────────── stop
          └────┬─────┘                       │
               │ load                        │
               ▼                             │
          ┌──────────┐                       │
          │Resolving │──── error ──► Error ──┘
          └────┬─────┘                       ▲
               │ resolved                    │
               ▼                             │
          ┌──────────┐                       │
          │Buffering │──── error ────────────┘
          └────┬─────┘
               │ buffer full
               ▼
          ┌──────────┐◄─── pause ───┌──────────┐
          │ Playing  │──────────────►│  Paused  │
          └────┬─────┘   resume     └──────────┘
               │ seek                        ▲
               ▼                             │
          ┌──────────┐──── done ─────────────┘
          │ Seeking  │
          └──────────┘
```

**Risks:**
- State machine edge cases: what happens if `stop()` arrives during `Resolving`?
  Need timeout-based cleanup for yt-dlp subprocess.
- SQLite write contention: multiple protocol handlers may update state
  concurrently. Use `Mutex<Connection>` or WAL mode.

---

## Phase 6 — Protocol Servers

**Goal:** `picast-protocols` implements the HTTP REST API, WebSocket server, and
DLNA MediaRenderer — the three interfaces external controllers use.

**Why seventh:** Protocol servers are the outermost layer. They depend on the
session manager being fully functional. They are also the most testable layer
because they can be tested with HTTP/WebSocket clients without Pi hardware.

| Milestone | Deliverable | Exit Criteria |
|-----------|-------------|---------------|
| 6.1 | HTTP API: `POST /api/cast` | Accepts `{"url":"..."}`, delegates to `session.load()`, returns `202 Accepted` with session ID |
| 6.2 | HTTP API: `POST /api/stop` | Stops current session, returns `200 OK` |
| 6.3 | HTTP API: `POST /api/pause` | Toggles pause, returns current state |
| 6.4 | HTTP API: `POST /api/seek` | Seeks, returns `{"status":"seeking"}` |
| 6.5 | HTTP API: `GET /api/status` | Returns full session state as JSON |
| 6.6 | HTTP API: `POST /api/volume` | Sets volume 0.0–1.0 |
| 6.7 | CORS headers | `Access-Control-Allow-Origin: *` on all responses (required for browser extension) |
| 6.8 | WebSocket server | `ws://pi:8586/ws` accepts connections, broadcasts `MEDIA_STATUS` on state change |
| 6.9 | WebSocket: client commands | `CAST`, `STOP`, `PAUSE`, `SEEK`, `VOLUME`, `SUBTITLE` message types |
| 6.10 | WebSocket: `RESOLVE_PROGRESS` | Periodic progress messages during yt-dlp resolution |
| 6.11 | DLNA via gmediarender | Spawn `gmediarender` subprocess with custom GStreamer pipeline; monitor state via D-Bus |
| 6.12 | SSDP advertisement | gmediarender broadcasts `ST: urn:schemas-upnp-org:device:MediaRenderer:1` on LAN |
| 6.13 | Integration tests | HTTP API tested with `reqwest`; WebSocket tested with `tokio-tungstenite`; DLNA tested with VLC manual |

**Estimated effort:** 12–15 days

**Risks:**
- `hyper` routing is manual (no framework) — need clean request dispatch
  or consider `axum` as a lighter alternative.
- WebSocket subprotocol is custom — no existing client libraries, so the
  browser extension is the primary test consumer.
- gmediarender subprocess management adds complexity: process monitoring,
  restart on crash, GStreamer pipeline string synchronization.

---

## Phase 7 — Server Orchestration

**Goal:** `picast-server` wires all subsystems together, spawns tasks, and
handles graceful shutdown.

| Milestone | Deliverable | Exit Criteria |
|-----------|-------------|---------------|
| 7.1 | Real component initialization | Replace `Arc::new(())` stubs with actual `TorManager`, `DisplayManager`, `PlaybackEngine`, `Resolver`, `SessionManager` |
| 7.2 | Task spawning | Each protocol server runs as a `tokio::spawn` task with shutdown signal |
| 7.3 | Graceful shutdown | SIGINT/SIGTERM → broadcast shutdown → wait for tasks → stop playback → release display → kill Tor |
| 7.4 | Startup ordering | Tor → Display → Playback → Resolver → Session → Protocols (sequential, each must succeed) |
| 7.5 | Health check endpoint | `GET /api/health` returns status of all subsystems |
| 7.6 | Configuration from file | `picast.conf` (TOML) in addition to env vars |
| 7.7 | End-to-end test on Pi | Boot → cast YouTube URL → verify video on HDMI → stop → clean shutdown |

**Estimated effort:** 5–7 days

---

## Phase 8 — Browser Extension Production

**Goal:** The Firefox/Chrome extension is fully functional, packaged, and
distributable.

| Milestone | Deliverable | Exit Criteria |
|-----------|-------------|---------------|
| 8.1 | Extension icons | Generate `icon16.png`, `icon48.png`, `icon128.png` |
| 8.2 | Content script for page URL detection | Inject script into web pages to detect video elements and report URLs to background worker |
| 8.3 | Firefox compatibility | Dual manifest: Manifest V3 for Chrome, V2/V3 compatibility for Firefox (`browser.*` namespace) |
| 8.4 | Popup: detected media list | Show intercepted media URLs with "Cast" button per item |
| 8.5 | Popup: playback controls | Play, pause, stop, seek bar, volume slider — all wired to real API |
| 8.6 | Popup: real-time status | WebSocket connection for live status updates (position, buffer %, state) |
| 8.7 | Options page: full settings | Pi address, port, Tor mode, auto-detect toggle |
| 8.8 | Chrome Web Store package | `zip` of extension directory, passes `chrome://extensions` developer mode load |
| 8.9 | Firefox Add-on package | `xpi` or unsigned `zip`, passes `about:debugging` load |
| 8.10 | Error handling | API unreachable → "PiCast not found" message; timeout → retry with backoff |

**Estimated effort:** 6–8 days

---

## Phase 9 — Testing & Quality Assurance

**Goal:** Comprehensive test coverage, automated CI, and documented test procedures.

| Milestone | Deliverable | Exit Criteria |
|-----------|-------------|---------------|
| 9.1 | Unit tests: every crate | `cargo test` per crate; ≥80% line coverage for `tor`, `resolver`, `session` |
| 9.2 | Integration tests: `tests/` | `tests/integration.rs` exercises full `load → resolve → play → pause → stop` flow |
| 9.3 | Pi hardware smoke test | Script: `scripts/smoke-test.sh` runs on Pi, verifies HDMI output, API responses |
| 9.4 | Network isolation test | `iptables -L` confirms all outbound traffic goes through Tor SOCKS |
| 9.5 | Memory leak test | Run 8-hour playback session, monitor RSS growth; target <10 MB/hour leak rate |
| 9.6 | Soak test: 100 cast/stop cycles | No resource exhaustion, no GStreamer pipeline leaks, SQLite DB stays under 1 MB |
| 9.7 | Security audit checklist | Verify: no DNS leaks, Tor circuit isolation confirmed, iptables rules enforced, no root beyond DRM |
| 9.8 | CI: cross-compilation + test | GitHub Actions builds for `aarch64`, runs x86 unit tests, clippy, rustfmt |

**Estimated effort:** 8–10 days

---

## Phase 10 — Distribution & Documentation

**Goal:** PiCast can be installed on a fresh Raspberry Pi OS image with a single
command and minimal configuration.

| Milestone | Deliverable | Exit Criteria |
|-----------|-------------|---------------|
| 10.1 | `scripts/setup.sh` overhaul | One-command install: `curl -sSL https://.../setup.sh | bash` installs all deps, builds, configures |
| 10.2 | Debian package (`picast_0.1.0_arm64.deb`) | `dpkg -i picast.deb` installs binary, config, systemd service, torrc |
| 10.3 | Pre-built SD card image | Flash-and-boot Pi OS image with PiCast pre-installed (Raspberry Pi Imager compatible) |
| 10.4 | README.md rewrite | Quick start guide: flash → boot → install extension → cast |
| 10.5 | User guide | `docs/USER_GUIDE.md`: configuration, troubleshooting, FAQ |
| 10.6 | Security hardening guide | `docs/SECURITY.md`: iptables rules explanation, Tor verification, physical security |
| 10.7 | Release checklist | GitHub Release with binary, deb, SD image, SHA-256 checksums, and release notes |

**Estimated effort:** 6–8 days

---

## Timeline Summary

| Phase | Description | Effort | Cumulative |
|-------|-------------|--------|------------|
| 0 | Build & CI Foundation | 3–4 days | 4 days |
| 1 | Tor Daemon Integration | 5–7 days | 11 days |
| 2 | DRM/KMS Display Manager | 7–10 days | 21 days |
| 3 | GStreamer Playback Engine | 12–15 days | 36 days |
| 4 | Content Resolver (yt-dlp) | 8–10 days | 46 days |
| 5 | Session Manager | 7–9 days | 55 days |
| 6 | Protocol Servers | 12–15 days | 70 days |
| 7 | Server Orchestration | 5–7 days | 77 days |
| 8 | Browser Extension | 6–8 days | 85 days |
| 9 | Testing & QA | 8–10 days | 95 days |
| 10 | Distribution & Docs | 6–8 days | 103 days |

**Total: ~100 working days (5 months solo, 2–3 months with 2 developers)**

Phases 0–3 can proceed linearly (each depends on the prior). Phases 4 and 5 can
partially overlap (resolver and session manager can be developed in parallel once
playback works). Phase 6 depends on Phase 5. Phase 7 depends on Phase 6. Phases
8, 9, and 10 can overlap with Phase 7.

### Critical Path

```
Phase 0 → Phase 1 → Phase 2 → Phase 3 → Phase 5 → Phase 6 → Phase 7 → Phase 9
                                    ↘ Phase 4 ↗                  ↘ Phase 8
                                                                  ↘ Phase 10
```

### First Demo Milestone

**After Phase 3 (day 36):** A PiCast binary that boots on Pi, starts Tor, and
plays a direct H.264 URL through the zero-copy pipeline to HDMI. This is the
"it works" moment — everything after this is wiring and polish.

### First Usable Release (v0.1.0-alpha)

**After Phase 7 (day 77):** Full `cast → resolve → play → control → stop` flow
through HTTP API. Browser extension works. DLNA works. This is the point where
the system is usable by a technical early adopter.

### Production Release (v1.0.0)

**After Phase 10 (day 103):** Tested, documented, packaged, distributable. The
system can be installed by a non-developer user on a Pi 4 and used daily.
