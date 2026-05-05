# PiCast Development Roadmap

> **Project:** PiCast — Tor-routed zero-copy media casting appliance for Raspberry Pi 4B+  
> **Last updated:** 2025-03-05  
> **Status:** Active development

This roadmap defines the release plan from foundation through production hardening and future capabilities. Each version is tagged, milestone-driven, and maps features to the implementing crate.

> **For concrete task-by-task execution, see [IMPLEMENTATION-PLAN.md](IMPLEMENTATION-PLAN.md)** — that document breaks each milestone into ordered, dependency-aware tasks with exact file paths and acceptance criteria.

---

## v0.1.0 — Foundation

**Theme:** DRM/KMS display, GStreamer V4L2 H.264 pipeline, Tor SOCKS5 routing, HTTP API  
**Status:** In Progress

This milestone establishes the core rendering, playback, networking, and control infrastructure. At the end of v0.1.0, a developer can `curl` a media URL to the Pi and see it play on the attached HDMI display through Tor.

### Display — `picast-display`

- [ ] Open DRM device `/dev/dri/card0` and acquire DRM master via `drmSetMaster()`
- [ ] Enumerate connectors, find the connected HDMI output, and read preferred mode
- [ ] Create DRM framebuffer and CRTC configuration via atomic modesetting (`drmModeAtomicCommit()`)
- [ ] Program HVS (Hardware Video Scaler) plane 0 for fullscreen scanout
- [ ] Implement `DisplayHandle` struct that wraps the DRM file descriptor and exposes methods for mode info, plane allocation, and CRTC commit
- [ ] Handle hotplug events via `drmHandleEvent()` (udev listener for HPD on HDMI)
- [ ] Graceful cleanup: `drmDropMaster()`, CRTC restore on process exit
- [ ] Unit tests with `vkms` (virtual KMS) kernel module for non-Pi development

### Playback — `picast-playback`

- [ ] Construct GStreamer pipeline: `souphttpsrc → queue2 → parsebin → v4l2h264dec → kmssink`
- [ ] Configure `v4l2h264dec` with `capture-io-mode=dmabuf` for zero-copy DMA-BUF output
- [ ] Configure `kmssink` with `plane-id=0` and `can-attach-static=true` for direct HVS scanout
- [ ] Configure `souphttpsrc` with `proxy-id` pointing to Tor SOCKS5 (`socks5://127.0.0.1:9050`)
- [ ] Configure `queue2` with `max-size-bytes=10485760` (10 MB) and `use-buffering=true` for ABR signal extraction
- [ ] Implement `PlaybackEngine` struct wrapping `gst::Pipeline` with methods: `play()`, `pause()`, `stop()`, `seek()`, `set_volume()`
- [ ] GStreamer bus message handler: listen for `GST_MESSAGE_ERROR`, `GST_MESSAGE_EOS`, `GST_MESSAGE_BUFFERING`, `GST_MESSAGE_STATE_CHANGED`
- [ ] Emit `PlaybackEvent` enum variants: `Playing`, `Paused`, `Stopped`, `Error`, `EndOfStream`, `Buffering { percent: u8 }`
- [ ] Handle V4L2 format negotiation failures gracefully (fall back to software decode via `avdec_h264` with caps warning)
- [ ] Validate pipeline on Pi 4 hardware with 1080p30 and 1080p60 H.264 test streams

### Tor — `picast-tor`

- [ ] Generate per-hostname SOCKS5 credentials: username = `sha256(hostname)[..16]`, password = `""`
- [ ] Implement `TorHandle` struct that manages SOCKS5 proxy URL construction with per-host isolation
- [ ] Tor control port client: connect to `127.0.0.1:9051`, authenticate via cookie (`/run/tor/control.authcookie`)
- [ ] Send `SIGNAL NEWNYM` for circuit rotation on user request or timeout
- [ ] Monitor Tor bootstrap status via `GETINFO status/bootstrap-phase`
- [ ] Health check: verify SOCKS5 connectivity by attempting a test connection through Tor
- [ ] Emit `TorEvent` enum variants: `CircuitEstablished`, `CircuitClosed`, `BootstrapProgress { percent: u8 }`, `ConnectionFailed`

### Session — `picast-session`

- [ ] Implement `SessionManager` as the central state coordinator (singleton per PiCast instance)
- [ ] Session lifecycle: `Idle → Resolving → Buffering → Playing → Paused → Stopped → Idle`
- [ ] Generate unique `sessionId` (UUID v4) per casting session
- [ ] Track session state: URL, title, position, duration, volume, mute, Tor mode, buffer percentage
- [ ] Coordinate between `picast-resolver` (URL resolution), `picast-playback` (media playback), and `picast-tor` (circuit setup)
- [ ] Emit `SessionEvent` on every state transition for downstream consumers (HTTP API, WebSocket)
- [ ] Single-session enforcement: reject new cast requests while a session is active (return 409 Conflict)
- [ ] Session timeout: auto-stop after 30 minutes of no playback activity (paused or buffering)

### Server — `picast-server`

- [ ] HTTP REST API on port 8585 using `hyper` (NOT axum/actix — per AGENT.md convention)
- [ ] `POST /api/cast` — initiate a new casting session (returns 202 Accepted with sessionId)
- [ ] `POST /api/stop` — stop current session
- [ ] `POST /api/pause` — toggle pause state
- [ ] `POST /api/seek` — seek to position (absolute or relative)
- [ ] `GET /api/status` — return current session state as JSON
- [ ] CORS headers: `Access-Control-Allow-Origin: *` for browser extension compatibility
- [ ] Error responses with structured JSON: `{ "error": "...", "details": "..." }`
- [ ] Graceful shutdown on SIGTERM/SIGINT

---

## v0.2.0 — Protocols

**Theme:** DLNA MediaRenderer, WebSocket events, SSDP discovery  
**Status:** Planned

Adds DLNA interoperability for VLC/Home Assistant, real-time WebSocket push notifications, and network discovery. After v0.2.0, PiCast is usable from any DLNA controller on the LAN.

### Protocols — `picast-protocols`

- [ ] Spawn `gmediarender` subprocess with custom `GSTREAMER_PIPELINE` environment variable pointing to PiCast's V4L2 + kmssink pipeline
- [ ] Configure gmediarender: `--friendly-name "PiCast"`, `--uuid <generated>`, `--port 49152`
- [ ] Implement `DlnaManager` struct that manages gmediarender lifecycle (start, stop, health check, restart on crash)
- [ ] SSDP announcement: verify gmediarender sends M-SEARCH responses and NOTIFY advertisements on multicast `239.255.255.250:1900`
- [ ] UPnP device description: validate XML at `http://<pi>:49152/description.xml` returns correct `MediaRenderer:1` device type
- [ ] AVTransport integration: monitor gmediarender's `SetAVTransportURI` calls and forward URIs to `picast-session`
- [ ] RenderingControl integration: map DLNA volume (0–100) to GStreamer volume (0.0–1.0)
- [ ] Session synchronization: when DLNA sets a URI, create a PiCast session; when PiCast stops, notify gmediarender

### Server — `picast-server`

- [ ] WebSocket server on port 8586 at path `/ws` using `tokio-tungstenite`
- [ ] Server→Client messages: `MEDIA_STATUS`, `RESOLVE_PROGRESS`, `ERROR`, `QUEUE_UPDATE`
- [ ] Client→Server messages: `CAST`, `STOP`, `PAUSE`, `SEEK`, `VOLUME`, `SUBTITLE`
- [ ] JSON message framing with `type` field for dispatch
- [ ] Ping/pong keepalive: server sends ping every 30s, disconnect unresponsive clients after 10s
- [ ] Broadcast state changes to all connected WebSocket clients
- [ ] Multiple simultaneous WebSocket clients supported

### Protocols — `picast-protocols`

- [ ] SSDP discovery helper: optional M-SEARCH listener for PiCast to discover other DLNA devices on the LAN (for future media browsing)
- [ ] mDNS announcement: broadcast `_picast._tcp` service on port 8585 for PiCast browser extension auto-discovery
- [ ] Validate iptables rules: SSDP/UPnP traffic restricted to LAN interface only

---

## v0.3.0 — Resolution

**Theme:** yt-dlp subprocess integration, URL classification, format selection  
**Status:** Planned

Adds intelligent URL resolution so users can paste YouTube/Vimeo/Twitch page URLs (not just direct media URLs). After v0.3.0, the full "paste URL → watch video" flow works.

### Resolver — `picast-resolver`

- [ ] Implement `UrlClassifier` that categorizes URLs into:
  - `DirectMedia` — URL ends in `.mp4`, `.mkv`, `.webm`, `.ts`, or has known media MIME type via HEAD request
  - `AdaptiveManifest` — URL is `.m3u8` (HLS) or `.mpd` (DASH manifest)
  - `PageUrl` — URL is a web page that needs yt-dlp extraction
- [ ] Implement `YtdlpResolver` that spawns `yt-dlp` as a subprocess:
  - Command: `yt-dlp --dump-json --no-download --no-warnings --socket-timeout 30 --proxy socks5://127.0.0.1:9050 --username <hosthash> --format "bv[height<=1080][vcodec^=avc1]+ba/b[height<=1080]/bv+ba" <URL>`
  - Parse JSON output with `serde_json` into `ResolvedMedia` struct
  - Extract: `url`, `format_id`, `height`, `width`, `vcodec`, `acodec`, `duration`, `title`, `subtitles`
  - 30-second timeout via `tokio::time::timeout` + `child.kill()`
- [ ] Implement `ResolvePipeline`: `UrlClassifier → YtdlpResolver → ResolvedMedia`
- [ ] Emit `ResolveEvent` variants: `Started`, `Progress { phase, message }`, `Completed(ResolvedMedia)`, `Failed(Error)`
- [ ] Error handling: yt-dlp exit code parsing, timeout detection, unsupported URL messages
- [ ] Cache resolved URLs for 10 minutes (same URL within cache window returns cached result without re-running yt-dlp)
- [ ] Tor SOCKS5 username: use `sha256(url::Host)[..16]` for IsolateSOCKSAuth circuit isolation

### Session — `picast-session`

- [ ] Integrate resolver into session lifecycle: `cast(url)` → `classify(url)` → `resolve(url)` → `play(resolved_url)`
- [ ] Expose resolution state via `GET /api/status` (`status: "resolving"`)
- [ ] Forward `RESOLVE_PROGRESS` events to WebSocket clients
- [ ] Handle resolution failures: return 422 with details, transition session to `Error` state

---

## v0.4.0 — Browser Extension

**Theme:** Chrome Manifest V3, webRequest interception, popup UI  
**Status:** Planned

Adds the PiCast Chrome extension so users can cast from their browser with one click. After v0.4.0, the end-user experience is: browse web → click PiCast icon → video plays on TV.

### Extension — `src/extension/` (TypeScript + HTML)

- [ ] Manifest V3 `manifest.json` with permissions: `activeTab`, `webRequest`, `storage`, `scripting`
- [ ] Background service worker (`background.js`):
  - Intercept media URL requests via `chrome.webRequest.onBeforeRequest` for video/audio MIME types
  - Detect `<video>` and `<source>` elements in page DOM via content script injection
  - Capture YouTube, Vimeo, Twitch page URLs from the active tab
  - Send cast request to PiCast HTTP API: `POST http://<pi-ip>:8585/api/cast`
  - Track active session state and display badge icon state (idle: gray, casting: blue, error: red)
- [ ] Popup UI (`popup.html` + `popup.js`):
  - PiCast IP address configuration (saved to `chrome.storage.local`)
  - Cast current tab button
  - Playback controls: play/pause, stop, seek slider, volume slider
  - Status display: current title, position/duration, buffer percentage
  - Tor mode selector: full / resolution-only / off
  - Error display with retry button
- [ ] Options page (`options.html`) for advanced settings:
  - Default Tor mode
  - Default quality ceiling (480p / 720p / 1080p)
  - PiCast auto-discovery via mDNS (`_picast._tcp`) or manual IP entry
  - Keyboard shortcut configuration for cast/pause/stop
- [ ] Auto-discovery: attempt to find PiCast on the LAN by checking `http://picast.local:8585/api/status`
- [ ] Handle PiCast unreachable state: display connection error in popup with troubleshooting hints

### Server — `picast-server`

- [ ] Add `GET /api/discover` endpoint that returns PiCast device info (name, IP, version) for extension auto-detection
- [ ] Rate limiting on `POST /api/cast` (max 5 requests per 10 seconds per IP)
- [ ] Request logging with `tracing` for audit trail

---

## v0.5.0 — Polish

**Theme:** ABR controller, buffer management, subtitle overlay, OSD  
**Status:** Planned

Refines the playback experience with adaptive bitrate, subtitle support, and on-screen display. After v0.5.0, PiCast handles real-world Tor bandwidth variability gracefully.

### Playback — `picast-playback`

- [ ] ABR controller: monitor `queue2` buffering messages and implement quality switching
  - Buffer < 25%: request lower-quality stream from yt-dlp (e.g., 480p → 360p)
  - Buffer > 75%: request higher-quality stream (e.g., 720p → 1080p)
  - Cooldown period: minimum 30 seconds between quality switches to avoid thrashing
- [ ] Dynamic pipeline reconfiguration: on ABR switch, tear down current pipeline and rebuild with new URL
  - Preserve playback position across pipeline rebuilds
  - Minimize gap: target < 2 seconds of black screen during quality switch
- [ ] Buffer management tuning:
  - `queue2 max-size-bytes`: scale based on estimated bitrate and Tor circuit bandwidth
  - Pre-buffer threshold: start playback when buffer reaches 40% (configurable)
  - Stall recovery: pause playback when buffer drops to 0%, resume at 25%
- [ ] Subtitle overlay via GStreamer `subtitleoverlay` element:
  - Parse SRT, VTT, and ASS subtitle formats from yt-dlp subtitle extraction
  - `textoverlay` or `subtitleoverlay` element inserted between decoder and kmssink
  - Font size, color, and position configurable via `/etc/picast/picast.conf`
  - Subtitle track selection via `POST /api/subtitle` or WebSocket `SUBTITLE` message
- [ ] OSD (On-Screen Display):
  - Render on DRM plane 1 (overlay plane) using a separate GStreamer pipeline
  - Display: title, resolution, Tor status, buffer percentage
  - Auto-hide after 5 seconds; show on volume change or seek
  - Text rendered via `textoverlay` on a transparent background

### Resolver — `picast-resolver`

- [ ] yt-dlp subtitle extraction: add `--write-subs --sub-langs en,es,fr,de --sub-format vtt` flags
- [ ] Parse subtitle URLs from yt-dlp JSON `subtitles` and `automatic_captions` fields
- [ ] Download subtitle files to `/tmp/picast/subs/` and pass paths to GStreamer `subtitleoverlay`

### Session — `picast-session`

- [ ] Track active subtitle track and available subtitles in session state
- [ ] Expose subtitle selection via HTTP API (`POST /api/subtitle`) and WebSocket (`SUBTITLE` message)

---

## v1.0.0 — Production

**Theme:** systemd hardening, iptables, security audit, documentation  
**Status:** Planned

Hardens the appliance for unattended 24/7 operation. After v1.0.0, PiCast is ready for production deployment as a headless appliance.

### Security — `config/`

- [ ] Systemd service hardening for `picast.service`:
  ```ini
  NoNewPrivileges=yes
  ProtectSystem=strict
  ProtectHome=yes
  PrivateTmp=yes
  ProtectKernelTunables=yes
  ProtectControlGroups=yes
  RestrictNamespaces=yes
  LockPersonality=yes
  MemoryDenyWriteExecute=yes
  RestrictSUIDSGID=yes
  CapabilityBoundingSet=CAP_SYS_TTY_CONFIG
  AmbientCapabilities=CAP_SYS_TTY_CONFIG
  SystemCallFilter=@system-service
  SystemCallErrorNumber=EPERM
  ```
- [ ] Systemd service hardening for `tor.service`:
  - Verify `debian-tor` user isolation and apparmor profile
  - Add `ProtectHome=yes`, `PrivateTmp=yes`, `NoNewPrivileges=yes`
- [ ] iptables rules (`config/iptables.rules`):
  - Default INPUT policy: DROP
  - Allow ESTABLISHED,RELATED connections
  - Allow SSH (port 22) from LAN only
  - Allow HTTP API (port 8585) from LAN only
  - Allow WebSocket (port 8586) from LAN only
  - Allow SSDP/UPnP (port 1900 UDP multicast + port 49152 TCP) from LAN only
  - Allow Tor SOCKS5 (port 9050) from localhost only
  - Allow Tor Control (port 9051) from localhost only
  - Allow DNS (port 53 UDP) from localhost only (Tor resolves)
  - Reject all other INPUT
  - Default FORWARD policy: DROP (no routing)
  - Default OUTPUT policy: ACCEPT (Tor handles outbound routing)
- [ ] Read-only root filesystem: `/` mounted read-only, `/tmp` as tmpfs, `/var/log` as tmpfs
- [ ] Watchdog: systemd `WatchdogSec=60` — PiCast must send `sd_notify("WATCHDOG=1")` every 30 seconds

### Server — `picast-server`

- [ ] Structured logging with `tracing-subscriber` and `env-filter`
- [ ] Log rotation: pipe logs to journald, configure `SystemMaxUse=50M` in `journald.conf`
- [ ] Health check endpoint: `GET /api/health` returns `200 OK` if all subsystems are healthy (GStreamer, Tor, DRM)
- [ ] Graceful degradation: if Tor is down, allow direct connections with warning (configurable policy)

### Documentation — `docs/`

- [ ] Complete API reference (OpenAPI/Swagger spec)
- [ ] User guide: setup, configuration, DLNA usage, browser extension installation
- [ ] Operator guide: OS image flashing, network configuration, iptables, Tor troubleshooting
- [ ] Security audit: document all network-facing surfaces, trust boundaries, and mitigation strategies
- [ ] Hardware compatibility matrix: tested Pi 4 RAM sizes, HDMI displays, USB Ethernet adapters

### Testing

- [ ] Integration test suite: Docker-based Pi 4 emulation with vkms + software decode
- [ ] End-to-end test: `curl POST /api/cast` → verify video plays → verify GStreamer pipeline state
- [ ] Tor circuit isolation test: cast from two different hostnames, verify different exit nodes via `https://check.torproject.org/api/ip`
- [ ] Stress test: 24-hour continuous playback with Tor circuit rotation
- [ ] Memory leak test: valgrind/ASAN on 100 cast/stop cycles

---

## v2.0.0 — Future

**Theme:** HEVC HW decode, arti migration, MSE segment proxy, Matter Casting  
**Status:** Planned (post-v1.0.0)

Expands PiCast's capabilities with next-generation codec support, Tor stack modernization, and emerging casting protocols.

### HEVC Hardware Decode — `picast-playback` + `picast-display`

- [ ] SAND→NV12 conversion pipeline (when one of the following is production-ready):
  - GStreamer 1.26 `sand2nv12` element
  - V3D compute shader conversion (DMA-BUF import → GPU compute → DMA-BUF export)
  - `rpi-hevc-dec` kernel-mode transparent conversion
- [ ] Update yt-dlp format string to prefer HEVC at 4K: `bv[height<=2160][vcodec^=hev1]+ba/bv[height<=1080][vcodec^=avc1]+ba`
- [ ] HEVC-specific GStreamer pipeline: `souphttpsrc → queue2 → parsebin → v4l2h265dec → sand2nv12 → kmssink`
- [ ] Validate zero-copy path: verify DMA-BUF flows from decoder through conversion to kmssink without CPU copy
- [ ] Test 4Kp30 HEVC playback through Tor (requires Tor circuit ≥ 15 Mbps sustained)
- [ ] Fallback: if SAND conversion adds > 1 frame latency, provide option to force H.264

### arti Migration — `picast-tor`

- [ ] Migrate from C Tor daemon to arti (Rust Tor client) when arti gains `IsolateSOCKSAuth` support
- [ ] In-process SOCKS5 proxy: embed arti directly in `picast-tor` crate, eliminating separate process overhead (~30 MB RAM savings)
- [ ] arti `StreamIsolation` trait: implement per-hostname circuit isolation using arti's native API
- [ ] Tor control: replace C daemon control port commands with arti's Rust API for circuit management
- [ ] Remove `tor.service` systemd dependency; `picast-tor` crate manages Tor lifecycle directly
- [ ] Maintain backward compatibility: support `torrc` config file format for operator familiarity

### MSE Segment Proxy — `picast-server` + `picast-protocols`

- [ ] Implement Media Source Extensions (MSE) segment proxy for sites that use MSE-based players (e.g., YouTube's dash.js player)
- [ ] Intercept and parse DASH/HLS segment requests from the browser extension
- [ ] Proxy segments through Tor SOCKS5, assembling them into a contiguous stream for GStreamer
- [ ] Handle encryption: if segments are encrypted (Clear Key only, not Widevine), decrypt using keys extracted from the manifest
- [ ] Buffer management: pre-fetch next N segments based on current playback position and available bandwidth

### Matter Casting — `picast-protocols`

- [ ] Implement Matter Casting protocol (CSA standard for casting over Thread/Wi-Fi)
- [ ] Matter device commissioning: PiCast appears as a Matter casting endpoint
- [ ] Support Matter Casting from Android 14+ and iOS 17+ devices
- [ ] Map Matter Casting media commands to PiCast session API: play, pause, stop, seek, volume
- [ ] Thread border router support: if Pi 4 has a Thread radio (via USB dongle), act as Thread border router for other Matter devices

### Other v2 Considerations

- [ ] Play queue: support multiple URLs in sequence (WebSocket `QUEUE_UPDATE` already defined in spec)
- [ ] Remote control: IR receiver support via GPIO for physical remote control
- [ ] Multi-display: support Pi 4's dual HDMI outputs for different content on each display
- [ ] Pi 5 support: validate on BCM2723 (RP1 I/O controller, VideoCore VII GPU)
- [ ] Web-based admin UI: simple HTML dashboard for configuration, status, and log viewing (served on port 8585)
