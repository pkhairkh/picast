# boGDan Implementation Plan

> **Purpose:** Concrete, ordered task list for building boGDan from scaffold to v1.0.0
> **Audience:** AI agents and human developers implementing the system
> **Convention:** Each task is scoped to a single agent session. Tasks list exact files to create/modify, dependencies, and acceptance criteria.

---

> **Note (2026-07-30):** This plan was the original task breakdown for building boGDan from scaffold to v1.0.0. All tasks herein have been implemented — see [TASKS.md](../TASKS.md) for the sprint-based breakdown showing all 7 sprints complete. This document is retained for historical context and design rationale.

---



## How to Read This Plan

Tasks are grouped by **milestone** (v0.1.0 → v1.0.0). Within each milestone, tasks are ordered by **dependency** — a task cannot start until all its dependencies are complete. Tasks that share no dependency can run in parallel (marked with `∥`).

**Task format:**
```
T<id> [∥] <title>
  Crate: <which crate>
  Files: <what to create/modify>
  Depends: <T-ids>
  Accept: <how to verify completion>
```

---

## Milestone v0.1.0 — Foundation

**Goal:** `curl -X POST http://pi:8585/api/cast -d '{"url":"..."}'` → video plays on HDMI through Tor.

### Phase 1: Leaf Crates (no internal dependencies — can run in parallel)

---

**T01** ∥ Tor SOCKS5 manager
  Crate: `bogdan-tor`
  Files: `src/tor/src/lib.rs`, `src/tor/Cargo.toml`, `src/tor/README.md`
  Depends: none
  Accept:
  - `TorHandle::new("127.0.0.1:9050")` creates a handle
  - `tor_handle.socks_addr()` returns `"127.0.0.1:9050"`
  - `tor_handle.proxied_reqwest_client("youtube.com")` returns a `reqwest::Client` configured with SOCKS5h proxy and per-hostname isolation username (`sha256(hostname)[..16]`)
  - `tor_handle.health_check()` attempts a TCP connect to SOCKS port, returns `CircuitHealth`
  - `tor_handle.ensure_running()` checks if Tor daemon process is alive, starts it if not (via `tokio::process::Command`)
  - `tor_handle.new_circuit()` sends `SIGNAL NEWNYM` to Tor control port (9051) via cookie auth
  - All unit tests pass: `cargo test -p bogdan-tor`
  - `cargo clippy -p bogdan-tor` zero warnings
  Implementation notes:
  - Add dependencies: `reqwest` (with `socks` feature), `sha2`, `tokio` (process), `nix` (signal)
  - SOCKS5 username for IsolateSOCKSAuth: `format!("bogdan-{}", &sha256(hostname)[..16])`
  - Control port auth: read `/run/tor/control.authcookie`, send `AUTHENTICATE <hex>\r\nSIGNAL NEWNYM\r\n`
  - Health check: TCP connect to SOCKS port + measure RTT with a small HTTP request through Tor to `https://check.tor-project.org/api/ip`

---

**T02** ∥ DRM/KMS display manager
  Crate: `bogdan-display`
  Files: `src/display/src/lib.rs`, `src/display/Cargo.toml`, `src/display/README.md`
  Depends: none
  Accept:
  - `DisplayManager::open("/dev/dri/card0")` opens DRM device and acquires master
  - `dm.connectors()` returns list of connected HDMI connectors with preferred mode
  - `dm.create_dumb_framebuffer(1920, 1080, XR24)` allocates a DRM framebuffer for OSD
  - `dm.set_plane(plane_id, fb_id, src_rect, dst_rect, zpos)` updates a plane via atomic commit
  - `dm.clear_screen()` fills plane 0 with black
  - `dm.resolution()` returns `(u32, u32)` for active mode
  - `dm.release()` drops master and restores previous CRTC state
  - All unit tests pass (using `vkms` module on x86_64 or mocked DRM calls)
  - `cargo clippy -p bogdan-display` zero warnings
  Implementation notes:
  - Add dependencies: `drm` crate (drm-rs), `nix` (ioctl wrappers), `gbm` (GBM buffer allocation for OSD plane)
  - Use `drm::control::atomic::AtomicCommitRequest` for all plane updates
  - `unsafe` blocks require `// SAFETY:` comments
  - For testing on non-Pi: gate V4L2/DRM code behind `#[cfg(target_arch = "aarch64")]` or feature flag
  - Fall back to `vkms` (virtual KMS) for CI testing: `modprobe vkms`

---

**T03** ∥ GStreamer playback engine
  Crate: `bogdan-playback`
  Files: `src/playback/src/lib.rs`, `src/playback/src/pipeline.rs`, `src/playback/src/events.rs`, `src/playback/Cargo.toml`, `src/playback/README.md`
  Depends: none (uses GStreamer directly, not bogdan-display — kmssink handles DRM internally)
  Accept:
  - `PlaybackEngine::new(PipelineConfig)` initializes GStreamer and creates an idle engine
  - `engine.play(url, source_url, socks_proxy, isolation_username, cookies)` constructs and starts the GStreamer pipeline: `appsrc → queue2 → parsebin → [dynamic: v4l2h264dec → v4l2convert → kmssink] + [avdec_aac → audioconvert → audioresample → volume → alsasink]` with DMA-BUF io-mode
  - `engine.pause()` transitions pipeline to Paused state
  - `engine.resume()` transitions back to Playing
  - `engine.stop()` sends EOS and destroys pipeline
  - `engine.seek(position_ms)` performs flushing seek
  - `engine.set_volume(0.0..1.0)` adjusts volume element
  - `engine.position_ms()` and `engine.duration_ms()` return current time info
  - `engine.buffer_health()` returns `BufferHealth` from queue2 buffering stats
  - `engine.events()` returns a `tokio::sync::mpsc::Receiver<PlaybackEvent>` with `Playing`, `Paused`, `Stopped`, `Error(String)`, `EndOfStream`, `Buffering { percent: u8 }`
  - All unit tests pass
  - `cargo clippy -p bogdan-playback` zero warnings
  Implementation notes:
  - Add dependencies: `gstreamer`, `gstreamer-app`, `gstreamer-video`, `gstreamer-audio` (gstreamer-rs crates)
  - `gstreamer::init()` must be called exactly once — use `std::sync::Once`
  - Pipeline construction: programmatic (not gst-launch string). Uses `appsrc` + `StreamSource` for CDN URLs, with `parsebin` for auto-detection and dynamic video chain creation in pad-added callback
  - CDN URLs: `StreamSource` (reqwest HTTP/2 + rustls TLS) → SOCKS Forwarder → Tor → CDN. Preflight check with GET+Range. sp= bypass strategy for CDN speed limits
  - Loopback URLs: `souphttpsrc` directly (no Tor needed)
  - Audio branch: `audio_queue → avdec_aac → audioconvert → audioresample → volume → alsasink`
  - Bus watch: `pipeline.bus().add_watch()` → map GStreamer messages to `PlaybackEvent` → send through mpsc channel
  - On `GST_MESSAGE_BUFFERING`: extract percent, emit `Buffering` event, pause/resume pipeline based on threshold
  - On `GST_MESSAGE_ERROR`: extract debug string, emit `Error` event, stop pipeline
  - Fallback: if `v4l2h264dec` fails to negotiate, rebuild pipeline with `avdec_h264` (software decode) and log warning
  - Software decode pipeline: `appsrc → queue2 → parsebin → queue → avdec_h264 → videoconvert → kmssink`

---

### Phase 2: Mid-Layer Crates (depend on Phase 1)

---

**T04** URL resolver with yt-dlp subprocess
  Crate: `bogdan-resolver`
  Files: `src/resolver/src/lib.rs`, `src/resolver/src/classifier.rs`, `src/resolver/src/ytdlp.rs`, `src/resolver/src/cache.rs`, `src/resolver/Cargo.toml`, `src/resolver/README.md`
  Depends: T01 (bogdan-tor for SOCKS5 client)
  Accept:
  - `Resolver::new(tor_handle)` creates a resolver with Tor integration
  - `resolver.classify(url)` returns `UrlCategory` without network access (pure URL parsing)
  - `resolver.resolve(url).await` classifies, then:
    - For `DirectMedia` / `HlsManifest` / `DashManifest`: return URL as-is with metadata from HEAD request
    - For `WebPage`: invoke `yt-dlp --dump-json` subprocess through Tor, parse JSON into `ResolvedMedia`
    - For `Onion`: same as WebPage but always through Tor
  - yt-dlp command: `yt-dlp --dump-json --no-download --no-warnings --socket-timeout 30 --proxy socks5h://<user>@127.0.0.1:9050 --format "bestvideo[vcodec^=avc1][height<=1080]+bestaudio/best[vcodec^=avc1][height<=1080]/best[height<=1080]" <url>`
  - 30-second timeout with `tokio::time::timeout`; kill subprocess on timeout
  - `ResolvedMedia` struct: `source_url`, `direct_url`, `category`, `title`, `duration`, `thumbnail`, `vcodec`, `acodec`, `width`, `height`, `subtitles`, `used_tor`
  - Cache: `HashMap<String, (ResolvedMedia, Instant)>` with 10-minute TTL
  - All unit tests pass (classifier tests are pure, ytdlp tests mock subprocess)
  - `cargo clippy -p bogdan-resolver` zero warnings
  Implementation notes:
  - Add dependencies: `tokio` (process), `serde_json`, `sha2`, `lru` (cache)
  - `classifier.rs`: URL parsing only, no network. Match on host (`.onion`), path extension (`.m3u8`, `.mpd`, `.mp4`, `.mkv`, `.webm`, `.ts`), and known site patterns (YouTube, Vimeo, Twitch domains → `WebPage`)
  - `ytdlp.rs`: subprocess execution + JSON parsing. Parse yt-dlp's JSON `formats` array to find best H.264 stream
  - `cache.rs`: simple in-memory cache, no external dependency. `insert()`, `get()`, `evict_expired()`
  - Mock yt-dlp in tests: create a shell script that outputs canned JSON, set `PATH` to include it

---

**T05** Session manager (state machine)
  Crate: `bogdan-session`
  Files: `src/session/src/lib.rs`, `src/session/src/manager.rs`, `src/session/src/interfaces.rs`, `src/session/Cargo.toml`, `src/session/README.md`
  Depends: T01, T02, T03, T04 (all leaf + mid-layer crates)
  Accept:
  - `SessionManager::new(resolver, playback, display, tor)` creates the central coordinator
  - `session.load(url).await` → creates `MediaSession`, classifies URL, resolves via `resolver`, starts `playback.play(resolved_url, socks)`, transitions state: `Idle → Resolving → Buffering → Playing`
  - `session.pause()` / `session.resume()` / `session.stop()` / `session.seek(pos)` / `session.set_volume(vol)` delegate to playback engine
  - `session.status()` returns current `MediaSession` snapshot
  - `session.events()` returns `tokio::sync::broadcast::Receiver<SessionEvent>` for state changes
  - Single-session enforcement: `load()` returns `SessionError::AlreadyActive` if session exists
  - Auto-stop: tokio task monitors session, stops after 30 min of `Paused` or `Buffering`
  - SQLite persistence: session state written on every transition, recovered on restart
  - All unit tests pass (mock all subsystems with trait implementations)
  - `cargo clippy -p bogdan-session` zero warnings
  Implementation notes:
  - The existing `interfaces.rs` defines the 4 traits (`ResolverTrait`, `PlaybackTrait`, `DisplayTrait`, `TorTrait`). All methods are `async` via `async_trait`.
  - `SessionManager` depends on `Arc<dyn ResolverTrait>`, `Arc<dyn PlaybackTrait>`, etc. — not concrete types.
  - State machine: use an `enum SessionState { Idle, Resolving { id: Uuid }, Buffering { id: Uuid }, Playing { id: Uuid }, Paused { id: Uuid }, Error { id: Uuid, msg: String } }` and match on transitions.
  - Playback event listener: spawn a tokio task that reads from `PlaybackEngine::events()` and updates session state accordingly
  - SQLite: use `rusqlite` (already in dependencies). Table schema already defined in existing `lib.rs`.
  - `SessionEvent` enum: `Created { id: Uuid, url: String }`, `Resolving { id: Uuid }`, `Playing { id: Uuid }`, `Paused { id: Uuid }`, `Stopped { id: Uuid }`, `Error { id: Uuid, message: String }`, `Buffering { id: Uuid, percent: u8 }`, `PositionUpdate { id: Uuid, position_ms: u64, duration_ms: Option<u64> }`

---

### Phase 3: Network Layer + Integration

---

**T06** HTTP REST API server
  Crate: `bogdan-protocols`
  Files: `src/protocols/src/lib.rs`, `src/protocols/src/http.rs`, `src/protocols/src/ws.rs`, `src/protocols/src/dlna.rs`, `src/protocols/Cargo.toml`, `src/protocols/README.md`
  Depends: T05 (session manager)
  Accept:
  - `HttpApiServer::new(addr, session_manager)` creates server
  - `POST /api/cast` with `{"url": "..."}` → returns `202 Accepted` with `{"sessionId": "...", "status": "resolving"}`
  - `POST /api/stop` → returns `200 OK` with `{"status": "idle"}`
  - `POST /api/pause` → toggles pause, returns new state
  - `POST /api/seek` with `{"seconds": 120.0, "mode": "absolute"}` → returns `200 OK`
  - `GET /api/status` → returns full session state JSON per SPECIFICATION.md §2.1
  - `GET /api/health` → returns `200 OK` with `{"status": "ok"}` (health check)
  - CORS headers: `Access-Control-Allow-Origin: *`
  - Error responses: `400`, `404`, `409`, `422`, `503` per spec
  - All integration tests pass (use `reqwest` test client against live server)
  - `cargo clippy -p bogdan-protocols` zero warnings
  Implementation notes:
  - Add dependencies: `hyper` (with `server` and `http1` features), `http-body-util`, `tokio` (net, signal), `serde_json`
  - NOT axum/actix/rocket — use hyper directly per AGENT.md convention
  - Route matching: manual `match` on `method` + `path` (hyper doesn't include a router)
  - Request body parsing: `hyper::body::to_bytes()` → `serde_json::from_slice()`
  - CORS: add headers manually in response
  - `POST /api/cast` spawns a tokio task for resolution (don't block the request thread)
  - Port: 8585 (configurable via `BOGDAN_HTTP_ADDR`)

---

**T07** WebSocket server
  Crate: `bogdan-protocols` (same crate, different file)
  Files: `src/protocols/src/ws.rs`
  Depends: T05, T06
  Accept:
  - `WebSocketServer::new(addr, session_manager)` creates server
  - Clients connect to `ws://<pi>:8586/ws`
  - Server broadcasts `MEDIA_STATUS`, `RESOLVE_PROGRESS`, `ERROR` to all connected clients
  - Clients send `CAST`, `STOP`, `PAUSE`, `SEEK`, `VOLUME`, `SUBTITLE`
  - Ping/pong: server sends ping every 30s, disconnects unresponsive clients after 10s
  - JSON message framing with `type` field per SPECIFICATION.md §2.2
  - Port: 8586

---

**T08** Main binary integration
  Crate: `bogdan-server`
  Files: `src/server/src/main.rs`, `src/server/Cargo.toml`
  Depends: T01–T07
  Accept:
  - `bogdan` binary starts, initializes all subsystems in order: `Tor → Display → Playback → Resolver → Session → HTTP → WebSocket`
  - Reads config from env vars: `BOGDAN_HTTP_ADDR`, `BOGDAN_WS_ADDR`, `BOGDAN_TOR_SOCKS`, `BOGDAN_DLNA_NAME`
  - Graceful shutdown on SIGINT/SIGTERM: stop playback → drop display → stop HTTP/WS servers
  - `RUST_LOG=debug cargo run` shows structured logs from all subsystems
  - `cargo build --release` produces a static binary
  - `cargo clippy` zero warnings
  Implementation notes:
  - Wire up concrete types to trait objects: `Arc<Resolver>` → `Arc<dyn ResolverTrait>`
  - Use `tokio::select!` for shutdown signal handling
  - Broadcast shutdown via `tokio::sync::broadcast` channel
  - This is the FIRST time all crates are linked together — expect compilation errors from trait mismatches. Fix them here.

---

**T09** End-to-end smoke test on Pi 4
  Crate: n/a (hardware test)
  Files: `scripts/smoke-test.sh`
  Depends: T08
  Accept:
  - Flash Pi OS Lite 64-bit bookworm, run `scripts/setup.sh`
  - `cargo build --release --target aarch64-unknown-linux-gnu` (or on-device)
  - Start boGDan: `sudo -u bogdan ./target/release/bogdan`
  - `curl -X POST http://pi:8585/api/cast -H 'Content-Type: application/json' -d '{"url":"https://www.youtube.com/watch?v=dQw4w9WgXcQ"}'`
  - Video plays on HDMI display with audio
  - `curl http://pi:8585/api/status` returns playing state
  - Pause/resume/stop all work via curl
  - Tor circuit isolation verified: two different hostnames get different exit IPs

---

## Milestone v0.2.0 — Protocols

**Goal:** boGDan appears as "boGDan" in VLC's renderer list. WebSocket pushes real-time status to connected clients.

---

**T10** DLNA MediaRenderer via gmediarender
  Crate: `bogdan-protocols`
  Files: `src/protocols/src/dlna.rs`
  Depends: T08
  Accept:
  - gmediarender starts with boGDan's GStreamer pipeline as GSTREAMER_PIPELINE env var
  - VLC → Playback → Renderer → "boGDan" appears
  - Setting URI via VLC starts playback on boGDan
  - Volume control via VLC maps to boGDan volume
  - boGDan stops gmediarender when its own HTTP API starts a session (and vice versa)
  Implementation notes:
  - gmediarender is a subprocess, not a Rust library. Spawn it with `tokio::process::Command`
  - GSTREAMER_PIPELINE env: `appsrc name=src ! queue2 use-buffering=true ! parsebin ! v4l2h264dec capture-io-mode=dmabuf ! v4l2convert output-io-mode=dmabuf capture-io-mode=dmabuf ! kmssink driver-name=vc4`
  - Session sync: monitor gmediarender's state (D-Bus or GStreamer bus), sync with boGDan session manager
  - Race condition: if both DLNA and HTTP API try to cast simultaneously, first one wins

---

**T11** mDNS announcement
  Crate: `bogdan-protocols`
  Files: `src/protocols/src/mdns.rs`
  Depends: T06
  Accept:
  - boGDan advertises `_bogcast._tcp.local` on port 8585
  - Browser extension can discover boGDan via `bogdan.local:8585`
  - avahi-daemon is already installed on Pi OS Lite

---

## Milestone v0.3.0 — Resolution

**Goal:** Paste a YouTube URL → yt-dlp resolves it through Tor → video plays.

---

**T12** Full yt-dlp integration with progress reporting
  Crate: `bogdan-resolver`
  Files: `src/resolver/src/ytdlp.rs`
  Depends: T08
  Accept:
  - yt-dlp resolution works for YouTube, Vimeo, Twitch URLs through Tor
  - Progress events are forwarded to WebSocket clients
  - Failed resolutions return structured errors (422 with details)
  - Cache prevents duplicate resolution of the same URL within 10 minutes

---

**T13** Browser extension v1 (basic cast)
  Files: `src/extension/background.js`, `src/extension/popup/popup.html`, `src/extension/popup/popup.js`
  Depends: T06, T11
  Accept:
  - Click boGDan icon → sends current tab URL to `POST /api/cast`
  - Popup shows: play/pause button, stop button, volume slider, status text
  - boGDan IP configurable in options, auto-discovered via `bogdan.local`
  - Manifest V3, loads in Chrome

---

## Milestone v0.4.0 — Polish

**Goal:** Subtitles render on screen. ABR adapts to Tor bandwidth. OSD shows status.

---

**T14** Subtitle support
  Crate: `bogdan-playback`
  Files: `src/playback/src/subtitles.rs`
  Depends: T08
  Accept:
  - yt-dlp downloads VTT/SRT subtitles
  - Subtitles render via `textoverlay` element (note: this breaks zero-copy by compositing on the video plane — acceptable for v1, move to OSD plane in v2)
  - `POST /api/subtitle` or WebSocket `SUBTITLE` selects track
  - Available tracks listed in `/api/status`

---

**T15** ABR controller
  Crate: `bogdan-playback`
  Files: `src/playback/src/abr.rs`
  Depends: T08
  Accept:
  - Monitors `queue2` buffering percentage
  - Buffer < 25% for 10 seconds → triggers re-resolution at lower quality
  - Buffer > 75% for 30 seconds → triggers re-resolution at higher quality
  - Cooldown: minimum 60 seconds between switches
  - Re-resolution preserves playback position

---

**T16** OSD overlay
  Crate: `bogdan-display`
  Files: `src/display/src/osd.rs`
  Depends: T02
  Accept:
  - Renders title, resolution, buffer %, Tor status on DRM Plane 1
  - Auto-hides after 5 seconds
  - Shows on volume change, seek, or new session start
  - Uses `textoverlay` on a separate pipeline → GBM buffer → DRM Plane 1

---

## Milestone v1.0.0 — Production

**Goal:** boGDan runs 24/7 unattended. Security hardened. Documented.

---

**T17** Systemd hardening
  Files: `config/bogdan.service`
  Depends: T08
  Accept:
  - All systemd hardening directives from ROADMAP.md applied
  - `WatchdogSec=60` with `sd_notify("WATCHDOG=1")` every 30s
  - `bogdan` user with minimal capabilities

---

**T18** Firewall hardening
  Files: `config/iptables.rules`
  Depends: T08
  Accept:
  - Default INPUT DROP
  - Only LAN-source traffic on 8585, 8586, 49152, 1900
  - Only localhost on 9050, 9051
  - No FORWARD

---

**T19** Integration test suite
  Files: `src/server/tests/` (expand existing stubs)
  Depends: T08
  Accept:
  - Full lifecycle test: cast → play → pause → seek → stop
  - Resolution test: YouTube URL resolves through Tor
  - Error test: invalid URL returns 422
  - Tor isolation test: two hostnames get different circuits

---

**T20** Documentation completion
  Files: `docs/`
  Depends: T17, T18, T19
  Accept:
  - User guide: setup, configuration, extension installation
  - Operator guide: OS image, network, Tor troubleshooting
  - API reference (expand SPECIFICATION.md with examples)
  - Security audit document

---

## Dependency Graph

```
T01 (tor) ──────┐
T02 (display) ──┤
T03 (playback) ─┤
                 ├── T04 (resolver) ──┐
                 │                     ├── T05 (session) ── T06 (http) ── T07 (ws) ── T08 (main)
                 │                     │                                                       │
                 └─────────────────────┘                                                       ├── T09 (smoke test)
                                                                                               ├── T10 (DLNA)
                                                                                               ├── T11 (mDNS)
                                                                                               │
                                                                         T12 (ytdlp full) ────┤
                                                                         T13 (extension v1) ──┤
                                                                                               │
                                                                         T14 (subtitles) ─────┤
                                                                         T15 (ABR) ───────────┤
                                                                         T16 (OSD) ───────────┘
                                                                                               │
                                                                         T17 (systemd) ───────┤
                                                                         T18 (firewall) ──────┤
                                                                         T19 (test suite) ────┤
                                                                         T20 (docs) ──────────┘
```

---

## Suggested Execution Order (serial)

| Order | Task | Duration Estimate | Cumulative |
|-------|------|-------------------|------------|
| 1 | T01 Tor + T02 Display + T03 Playback | 3 sessions (parallel) | 1 session |
| 2 | T04 Resolver | 1 session | 2 sessions |
| 3 | T05 Session | 1 session | 3 sessions |
| 4 | T06 HTTP API + T07 WebSocket | 1 session | 4 sessions |
| 5 | T08 Main binary + T09 Smoke test | 1 session | 5 sessions |
| 6 | T10 DLNA + T11 mDNS | 1 session | 6 sessions |
| 7 | T12 Full resolver + T13 Extension | 1 session | 7 sessions |
| 8 | T14 Subtitles + T15 ABR + T16 OSD | 1-2 sessions | 8-9 sessions |
| 9 | T17-T20 Production hardening | 2 sessions | 10-11 sessions |

**Total: ~10-11 focused agent sessions from scaffold to v1.0.0**

---

## Risks and Mitigations

| Risk | Probability | Impact | Mitigation |
|------|------------|--------|------------|
| GStreamer V4L2 pipeline fails on Pi 4 | Medium | High | Test early (T09 smoke test after T08); have software decode fallback path ready |
| Tor bandwidth too low for 720p | Low | Medium | ABR controller (T15) switches to 480p; increase queue2 buffer |
| kmssink doesn't support DMA-BUF import | Low | Critical | Test T03 with `gst-launch-1.0` on Pi 4 before writing Rust code; fallback to `glimagesink` |
| yt-dlp subprocess too slow (15s startup) | Medium | Low | Accept as v1 limitation; cache results; show progress in extension |
| DRM atomic commit fails on vc4 driver | Low | High | Test with `modetest` first; use legacy `drmModeSetPlane()` as fallback |
| Session manager deadlocks | Medium | High | All state mutations behind `tokio::sync::Mutex`; timeout on every subsystem call |
