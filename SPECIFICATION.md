# boGDan Technical Specification v1.0

> **Companion document:** boGDan Architecture Paper  
> **Status:** ratified  
> **Last updated:** 2025-03-04  
> **Audience:** implementers, integrators, security reviewers

This specification defines the concrete API contracts, decision records, configuration formats, and operational parameters that govern boGDan v1. Every normative requirement uses RFC 2119 keywords (MUST, SHALL, SHOULD, MAY). Informative commentary appears in blockquotes.

---

## 1. Architecture Decision Records

Each ADR follows the Michael Nygard format. Status badges reflect the ratification outcome at v1 freeze.

---

### ADR-001: No Display Server

**Status:** ![ACCEPTED](https://img.shields.io/badge/Status-ACCEPTED-green)

**Context**

A display server (X11, Wayland compositor, or kiosk window manager) provides window management, input event routing, and compositing. boGDan has exactly one output: a single fullscreen video surface rendered to the attached HDMI display. There is no local keyboard or pointer input—control arrives exclusively over the network via HTTP, WebSocket, or UPnP/DLNA. Running a display server would consume memory for its own framebuffers, introduce a compositor copy for every frame, add an IPC layer between the application and the kernel's display subsystem, and widen the attack surface with a substantial privileged daemon.

**Decision**

boGDan will drive the display directly through DRM/KMS (Direct Rendering Manager / Kernel Mode Setting). The application opens `/dev/dri/card0`, calls `drmSetMaster()` to acquire mastering privileges, discovers the connected connector and preferred mode via `drmModeGetConnector()`, and programs the Hardware Video Scaler (HVS) through `drmModeAtomicCommit()`. This is the same approach used by Kodi, LibreELEC, and retro-gaming front-ends on the Pi.

No X11 server, Wayland compositor, or window manager will be started. The `autologin` systemd service will launch `bogdand` directly on the first virtual terminal (`tty1`).

**Consequences**

| Direction | Effect |
|-----------|--------|
| **Positive** | 50–100 MB RAM savings (no X/Wayland server, no compositor framebuffers, no DRI3 buffer sharing overhead). Zero compositor latency—pixel data travels V4L2 decoder → DMA-BUF → HVS → HDMI without an intermediate copy. Reduced attack surface—no X11 protocol parser, no Wayland IPC, no window manager process. |
| **Negative** | Cannot run any GUI application (no terminal emulator, no browser, no debugger UI). All debugging and log inspection must occur over SSH. Screenshot capture requires `modetest` or a custom DRM client rather than a simple window-capture tool. OTA UI updates (e.g., "Now Playing" overlays) must be rendered through GStreamer's `textoverlay` or a custom DRM plane rather than a toolkit. |

**Alternatives Rejected**

| Alternative | Why rejected |
|-------------|-------------|
| **X11 + openbox** | Xorg alone consumes ~60 MB RAM; openbox adds ~5 MB. DRI3 PRIME buffer passing introduces a copy. X11 protocol is a large attack surface (2,369 CVEs since 2000). Provides window management boGDan doesn't need. |
| **Weston (Wayland reference compositor)** | Lighter than X11 (~20 MB) but still unnecessary. Adds an IPC layer (Wayland protocol) between GStreamer and the display. `weston-simple-dmabuf` demonstrates zero-copy is possible, but the compositor process is still in the data path. |
| **matchbox-window-manager** | Minimal X11 manager (~2 MB) designed for embedded kiosks. Still requires X11 server underneath, so the fundamental overhead remains. Adds an X11 IPC layer for no benefit. |

---

### ADR-002: No Chromium / No Browser Runtime

**Status:** ![ACCEPTED](https://img.shields.io/badge/Status-ACCEPTED-green)

**Context**

A browser engine—most likely Chromium in kiosk mode—could serve as a universal player: render DRM-protected pages (Netflix, Disney+), execute JavaScript for sites that generate video URLs dynamically, and display web content. However, Chromium on a Raspberry Pi 4 consumes 300–500 MB RAM for even a single tab, performs all video decoding in software (no V4L2 M2M integration in Chromium's media pipeline on ARM), presents a massive attack surface (V8 JavaScript engine, Blink rendering engine, GPU process, network stack), and adds 30–60 seconds of startup time.

**Decision**

boGDan will not include any browser engine. Video URL resolution is handled by `yt-dlp` (as a subprocess—see ADR-008). Media playback is handled by GStreamer with hardware-accelerated V4L2 decoding (see ADR-003). DRM-protected content is explicitly out of scope (see ADR-007).

**Consequences**

| Direction | Effect |
|-----------|--------|
| **Positive** | ~400 MB RAM savings versus Chromium kiosk. ~40% CPU savings (no JavaScript compilation, no DOM rendering, no compositor). No V8 attack surface (V8 has had 1,200+ CVEs). No Blink attack surface. Simpler build—no need to cross-compile Chromium or bundle a 150 MB binary. |
| **Negative** | Cannot play DRM content (Netflix, Disney+, Amazon Prime Video, HBO Max). Approximately 5–10% of web video sites require JavaScript execution beyond what yt-dlp can handle—these will fail. No general web browsing capability. Some sites with sophisticated anti-bot JavaScript (e.g., Cloudflare Turnstile) may block yt-dlp's requests, requiring periodic yt-dlp updates. |

**Alternatives Rejected**

| Alternative | Why rejected |
|-------------|-------------|
| **Chromium kiosk mode** | Resource-heavy (300–500 MB RAM, 30–60s startup). Software-only video decode on ARM. Massive attack surface. DRM support requires Widevine L3 blob (unreliable on ARM, slow). |
| **deno_core embedded runtime** | Rust-embeddable V8 runtime at ~30 MB. Could execute site-specific JavaScript for URL extraction. However, deno_core cannot handle DRM (no EME support), doesn't render pages (no DOM), and introduces V8's attack surface. Maintenance burden of per-site JavaScript extraction scripts is high. |
| **Cog / WPE WebKit** | Lightweight WebKit port for embedded (~50 MB). Supports hardware-accelerated compositing via WPE-backend-fdo. However, DRM is not supported on ARM. WPE's media pipeline doesn't integrate with V4L2 M2M on Pi 4 without significant patching. Smaller community than Chromium. |

---

### ADR-003: GStreamer Over mpv

**Status:** ![ACCEPTED](https://img.shields.io/badge/Status-ACCEPTED-green)

**Context**

Both GStreamer and mpv can play video on the Raspberry Pi 4. mpv offers a simpler CLI and Lua scripting interface. GStreamer provides a pipeline architecture with first-class V4L2 M2M integration, a `kmssink` element for zero-copy DRM/KMS output, fine-grained buffer control via `queue2`, and built-in adaptive streaming demuxers (HLS, DASH).

**Decision**

boGDan will use GStreamer (1.22+) as its media framework. The primary playback pipeline uses `v4l2h264dec` for hardware H.264 decoding and `kmssink` for zero-copy display via DRM/KMS. Adaptive streaming uses GStreamer's `hlsdemux` and `dashdemux` elements. Buffer management uses `queue2` with buffering percentage signals for ABR control.

mpv's `--vo=drm` output driver does not support hardware decode on the Pi 4. mpv's V4L2 support requires manual `--hwdec=v4l2m2m` configuration and outputs to an OpenGL surface, not directly to a DRM plane—breaking zero-copy.

**Consequences**

| Direction | Effect |
|-----------|--------|
| **Positive** | Zero-copy pipeline: V4L2 decoder outputs DMA-BUF → `kmssink` imports DMA-BUF → HVS scans directly from the buffer. No CPU copy, no GPU copy. `queue2` provides network buffering with percentage signals—enabling ABR decisions without polling. `subtitleoverlay` element composites SRT/VTT/ASS subtitles onto the video plane. `hlsdemux` and `dashdemux` handle adaptive stream parsing, ABR ladder selection, and segment fetching. `souphttpsrc` provides HTTP source with proxy support (Tor SOCKS5). |
| **Negative** | Steeper learning curve—GStreamer's pipeline construction API is more complex than mpv's configuration file approach. GStreamer has occasional pipeline negotiation failures that require careful caps filtering. Some GStreamer elements have thread-safety issues when dynamically reconfiguring (e.g., seeking during ABR switch). Debugging requires `GST_DEBUG` environment variable rather than mpv's simpler `--msg-level`. |

**Alternatives Rejected**

| Alternative | Why rejected |
|-------------|-------------|
| **mpv `--vo=drm`** | No hardware decode on Pi 4. `--hwdec=v4l2m2m` outputs to OpenGL, not DRM plane. Requires GL→DRM copy, breaking zero-copy. No built-in adaptive streaming demuxer. |
| **FFmpeg + custom DRM sink** | Would require implementing a DRM/KMS output module from scratch (equivalent to `kmssink`). No built-in adaptive streaming. No buffer management signals. Essentially reinventing GStreamer's pipeline architecture. |
| **Kodi as a library** | Kodi can run headless with DRM/KMS and V4L2 decode. However, Kodi is a full media center application (~150 MB installed), not a library. Its Python add-on API is not suitable for real-time external control. Embedding Kodi would add unnecessary complexity. |

---

### ADR-004: C Tor Daemon Over arti

**Status:** ![ACCEPTED](https://img.shields.io/badge/Status-ACCEPTED-green)

**Context**

arti is the Tor Project's Rust-based client, intended as the eventual successor to the C Tor daemon. arti is production-ready for HTTP CONNECT proxying and basic SOCKS5. The C Tor daemon is a separate process configured via `torrc`, running as the `debian-tor` user.

boGDan requires fine-grained stream isolation: different websites must use independent Tor circuits to prevent correlation. The C Tor daemon's `IsolateSOCKSAuth` flag maps SOCKS5 username/password combinations to separate circuits. arti, as of v1.2.0, does not support `IsolateSOCKSAuth` or an equivalent per-username circuit isolation mechanism.

**Decision**

boGDan will use the C Tor daemon (`tor` package from Debian) configured with `IsolateSOCKSAuth` on SOCKS port 9050. The daemon runs as a separate `systemd` service (`tor.service`). boGDan communicates with Tor via SOCKS5, using the SOCKS5 username field to encode the destination domain hash for circuit isolation.

**Consequences**

| Direction | Effect |
|-----------|--------|
| **Positive** | Proven SOCKS5 implementation—C Tor has been production-hardened for 20+ years. Extensive configuration options via `torrc` (bandwidth bursting, exit policy, bridge support, pluggable transports). Well-documented behavior; `IsolateSOCKSAuth` semantics are precisely specified. Mature systemd integration (`tor.service`, `debian-tor` user, apparmor profile). |
| **Negative** | Separate process (~30 MB RAM overhead for the Tor daemon). Inter-process communication via SOCKS5 (no in-process API). arti is the Tor Project's stated future direction—C Tor will eventually be deprecated. arti's Rust implementation would allow in-process, tokio-native SOCKS5 without IPC overhead. |

**Alternatives Rejected**

| Alternative | Why rejected |
|-------------|-------------|
| **arti** | Lacks `IsolateSOCKSAuth` or equivalent per-username circuit isolation. arti's `StreamIsolation` trait exists but doesn't map SOCKS5 usernames to separate circuits. This is a hard blocker for boGDan's privacy model. Re-evaluate when arti adds this feature (see OD-003). |
| **No Tor** | Violates boGDan's core privacy requirement. Tor integration is a first-class feature, not optional. |

---

### ADR-005: Cast V2 Protocol Rejected

**Status:** ![REJECTED](https://img.shields.io/badge/Status-REJECTED-red)

**Context**

Google's Cast V2 protocol would enable the native Chrome cast button to appear for boGDan devices on the LAN. The protocol has been reverse-engineered and documented in open-source projects (e.g., `node-castv2`, `pychromecast`). Implementing Cast V2 would provide the most seamless user experience—users could cast from any Chrome tab without installing an extension.

However, Google enforces device authentication in the Cast SDK. Official Cast receivers must authenticate via a TLS handshake using certificates provisioned through Google's cloud service. Unofficial receivers (like boGDan would be) cannot complete this handshake. While some open-source implementations bypass the auth check, Google has progressively tightened enforcement, and the bypass technique changes with each Chrome update.

**Decision**

boGDan will not implement the Cast V2 protocol. The fragility of depending on reverse-engineered authentication bypass is unacceptable for a stable product. Users will cast via the boGDan browser extension, VLC, Home Assistant, or any DLNA-compatible controller.

**Consequences**

| Direction | Effect |
|-----------|--------|
| **Positive** | No fragile dependency on reverse-engineered protocol details that break with Chrome updates. Simpler implementation—no need for mDNS discovery with Cast-specific service types, no TLS certificate management, no protobuf message parsing. Reduced maintenance burden—Cast V2 is not a public API and changes without notice. |
| **Negative** | No native Chrome cast button—users must install the boGDan browser extension or use VLC/DLNA. Reduced discoverability for non-technical users who expect "just works" casting like a Chromecast. Some user education required ("Why doesn't the Cast button work?"). |

**Alternatives Rejected**

| Alternative | Why rejected |
|-------------|-------------|
| **Cast V2 with auth bypass** | Fragile—Google has broken bypass techniques in Chrome 90, 100, and 110 updates. Each breakage requires reverse-engineering the new auth flow. User-visible failures (cast button disappears, connection drops) are unacceptable. |
| **Shanocast** | Open-source Cast V2 receiver that maintains the auth bypass. Unreliable for media playback—frequent disconnections, limited codec support. Not actively maintained. |
| **DIAL only** | DIAL (Discovery and Launch) provides only device discovery and app launch—no media control. Would require a separate control channel. DIAL is deprecated by Google in favor of Cast V2. |

---

### ADR-006: UPnP/DLNA MediaRenderer

**Status:** ![ACCEPTED](https://img.shields.io/badge/Status-ACCEPTED-green)

**Context**

boGDan needs interoperability with existing media controllers without requiring custom software. DLNA (Digital Living Network Alliance) is supported by VLC, Home Assistant, Android apps (BubbleUPnP, Hi-Fi Cast), and Windows Media Player. A DLNA MediaRenderer implements the UPnP AVTransport and RenderingControl services.

**Decision**

boGDan will use `gmediarender` (also known as `gmrender-resurrect`) as its DLNA MediaRenderer. gmediarender is a mature, well-tested implementation that has been running on Raspberry Pi devices for over 10 years. It is available as a Debian package (`gmediarender`). boGDan will configure gmediarender with a custom GStreamer pipeline string that matches the boGDan playback engine (V4L2 decode + kmssink).

boGDan's Rust daemon will monitor gmediarender's state via D-Bus or by watching GStreamer bus messages, synchronizing DLNA playback state with the internal session manager.

**Consequences**

| Direction | Effect |
|-----------|--------|
| **Positive** | Immediate compatibility with VLC (Tools → Renderer → boGDan), Home Assistant (media_player entity), Android DLNA apps, and Windows. Well-tested on ARM—gmediarender has been the default DLNA renderer on Pi for a decade. Debian package available—no custom compilation needed. |
| **Negative** | DLNA only supports directly fetchable URLs—no site resolution (no yt-dlp integration). SSDP discovery is slow (M-SEARCH responses take 0–3 seconds, cache lifetime is 30 minutes). Limited real-time playback status—UPnP `GetPositionInfo` polling is the only way to get position, and many controllers don't poll. gmediarender's GStreamer pipeline integration requires careful configuration to avoid pipeline conflicts with boGDan's primary pipeline. |

**Alternatives Rejected**

| Alternative | Why rejected |
|-------------|-------------|
| **Custom DLNA implementation** | Reinventing gmediarender. SSDP, UPnP SOAP, device description XML, eventing (GENA) — all must be implemented from scratch. Significant effort for no functional gain. |
| **Rygel** | GNOME's DLNA server/renderer. Heavier than gmediarender (~15 MB vs ~3 MB). Pulls in GNOME dependencies (GSSO, GUPnP). Designed as a media server first, renderer second. |

---

### ADR-007: DRM Out of Scope

**Status:** ![ACCEPTED](https://img.shields.io/badge/Status-ACCEPTED-green)

**Context**

DRM (Digital Rights Management) content includes Netflix, Disney+, Amazon Prime Video, HBO Max, Hulu, and other subscription services. Widevine L3 (the software-based Widevine security level) exists for ARM platforms and can be used within Chromium to decrypt DRM streams. However, Widevine L3 on ARM is slow (adds 10–20% CPU overhead for decryption), requires Chromium as the playback engine (see ADR-002), involves proprietary binary blobs (libwidevinecdm.so) that are not auditable, and is unreliable on ARM—Google has broken Widevine L3 on ARM in the past.

**Decision**

DRM content playback is explicitly out of scope for boGDan v1. The primary use case is YouTube, Vimeo, Twitch, PeerTube, Internet Archive, and other platforms that serve clear (non-DRM) video streams. Users who need DRM content should use a dedicated streaming device (Chromecast, Fire TV, Apple TV).

**Consequences**

| Direction | Effect |
|-----------|--------|
| **Positive** | No proprietary dependencies—boGDan is 100% open-source software. Simpler build and distribution—no Widevine blob to download, license, or version-pin. Reduced attack surface—Widevine L3 runs proprietary, unauditable code in the Chromium sandbox. Clear scope—users understand boGDan is for open web video, not a Chromecast replacement. |
| **Negative** | Cannot cast DRM content from Netflix, Disney+, Amazon, HBO, etc. This excludes the most popular streaming services. Some users will need a separate device for DRM content. |

**Alternatives Rejected**

| Alternative | Why rejected |
|-------------|-------------|
| **Widevine L3 in Chromium** | Requires Chromium (see ADR-002). Slow on ARM. Unreliable—Google breaks it periodically. Proprietary blob—cannot be audited, cannot be patched. Adds 300+ MB to the image. |

---

### ADR-008: yt-dlp as Subprocess

**Status:** ![ACCEPTED](https://img.shields.io/badge/Status-ACCEPTED-green)

**Context**

yt-dlp is a Python application that resolves web page URLs to direct media stream URLs. It can be used as a library (`import yt_dlp`) or as a subprocess (`yt-dlp -J <url>`). As a library, it shares the host process's memory and event loop—a crash or hang in yt-dlp's network code could bring down the entire boGDan daemon. As a subprocess, yt-dlp runs in isolation—its crashes are contained, and it can be killed with a timeout.

**Decision**

boGDan will invoke yt-dlp as a subprocess via `tokio::process::Command`. The command produces JSON output (`-J` flag), which boGDan parses into a structured `ResolvedMedia` type. Process isolation ensures yt-dlp crashes (Python exceptions, network hangs, segfaults in native extensions) do not affect boGDan. The yt-dlp binary can be updated independently (`pip install -U yt-dlp`) without rebuilding boGDan.

**Consequences**

| Direction | Effect |
|-----------|--------|
| **Positive** | Process isolation—yt-dlp crashes are contained. The boGDan daemon remains responsive even if yt-dlp hangs (kill after timeout). Independent update cycle—yt-dlp can be updated via `pip` without recompiling boGDan. Simple error handling—subprocess exit code and stderr provide clear failure diagnostics. |
| **Negative** | 5–15 second Python startup time (CPython interpreter initialization, yt-dlp module loading, extractor registration). JSON output overhead—yt-dlp's `-J` output can be 50–200 KB for complex playlists. No progress hooks—cannot receive incremental download progress during resolution (only final result). No streaming extraction—must wait for full resolution before playback begins. |

**Alternatives Rejected**

| Alternative | Why rejected |
|-------------|-------------|
| **yt-dlp as Python library** (`import yt_dlp`) | Shared process space—a Python exception can panic the Rust runtime via PyO3. No process isolation. Complex FFI bridge (PyO3) adds maintenance burden. yt-dlp update could break the API contract at import level. |
| **yt-dlp as HTTP microservice** | Adds network dependency and another daemon. Latency overhead of HTTP round-trip on top of Python startup. No simpler than subprocess approach. |

---

### ADR-009: HEVC Deferred

**Status:** ![DEFERRED](https://img.shields.io/badge/Status-DEFERRED-yellow)

**Context**

The Raspberry Pi 4's VideoCore VI GPU includes a hardware HEVC (H.265) decoder capable of 4Kp60. This is significant because YouTube and other platforms increasingly encode 4K content in HEVC only (H.264 4K streams are rarely available). However, the HEVC hardware decoder outputs frames in SAND (Self-Describing Associative Network Data) format, which the Pi 4's HVS (Hardware Video Scaler) cannot directly display. Displaying SAND frames requires conversion to NV12 (or similar linear) format, which breaks the zero-copy pipeline because the conversion requires a CPU or GPU copy.

**Decision**

HEVC hardware decode is deferred to boGDan v2. In v1, yt-dlp's format selection string forces H.264 (`vcodec^=avc1`), which means 4K content will be limited to the highest available H.264 resolution (typically 1080p60). For HEVC-only content (rare but possible), GStreamer's `avdec_h265` software decoder will be used as a fallback, limited to 720p30 due to CPU constraints.

Re-evaluate when one of the following conditions is met:
- GStreamer 1.26 lands patches for SAND format handling in `v4l2h265dec` → `kmssink`.
- V3D compute shader SAND→NV12 conversion is proven and merged.
- A kernel DRM driver provides transparent SAND→linear conversion.

**Consequences**

| Direction | Effect |
|-----------|--------|
| **Positive** | Only proven zero-copy H.264 pipeline in v1—no risk of SAND format issues. Simpler testing and validation. H.264 1080p60 covers the vast majority of content. |
| **Negative** | Some 4K content limited to 1080p H.264 stream (YouTube 4K is HEVC-only for most videos). HEVC hardware decoder sits unused—wasted silicon. Software HEVC fallback (`avdec_h265`) is limited to ~720p30 on Pi 4's CPU. |

**Alternatives Rejected**

| Alternative | Why rejected |
|-------------|-------------|
| **CPU NEON SAND→NV12 now** | Proven at ~30fps for 4K, but introduces a copy that breaks zero-copy. Power and thermal implications for sustained conversion. Better to wait for hardware-assisted conversion. |
| **HEVC with copy** (accept the copy) | ~2–4 GB/s memory bandwidth for 4Kp60 conversion. Reduces the RAM and bandwidth savings that are boGDan's core value. Inconsistent with the zero-copy architecture principle. |

---

## 2. API Specification

### 2.1 HTTP REST API (Port 8585)

The REST API is the primary control interface for the boGDan browser extension and third-party integrations. All endpoints are served over plain HTTP (no TLS—boGDan is a LAN-only device; TLS on LAN adds complexity without meaningful security benefit in a trusted-network model).

**Base URL:** `http://<pi-ip>:8585`  
**Content-Type:** `application/json` (request and response bodies)  
**CORS:** `Access-Control-Allow-Origin: *` (required for browser extension cross-origin requests)

---

#### `POST /api/cast`

Initiates a new casting session. The server resolves the URL, constructs a GStreamer pipeline, and begins playback.

**Request Body:**

```json
{
  "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
  "title": "Optional display title — used for OSD and status",
  "resumePosition": 0,
  "torMode": "full"
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `url` | `string` | **yes** | — | URL to resolve and play. May be a direct media URL, adaptive manifest URL, or a web page URL. |
| `title` | `string` | no | `""` | Display title for the session. If empty, boGDan will derive a title from the URL or yt-dlp metadata. |
| `resumePosition` | `number` | no | `0` | Resume playback from this position, in seconds. Ignored if the source doesn't support seeking. |
| `torMode` | `string` | no | `"full"` | Tor routing mode: `"full"`, `"resolution-only"`, or `"off"`. Overrides the global default for this session. |

**Response:** `202 Accepted`

```json
{
  "sessionId": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "status": "resolving",
  "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
  "title": "Rick Astley - Never Gonna Give You Up",
  "torMode": "full"
}
```

The `202 Accepted` status indicates the URL has been accepted for resolution but playback has not yet begun. The client should poll `GET /api/status` or subscribe to the WebSocket for state transitions.

**Error Responses:**

| Status | Condition | Body |
|--------|-----------|------|
| `400 Bad Request` | Missing or invalid `url` field | `{"error": "url is required and must be a valid URI"}` |
| `409 Conflict` | A session is already active (boGDan supports one session at a time) | `{"error": "session already active", "sessionId": "..."}` |
| `422 Unprocessable Entity` | URL resolution failed (yt-dlp error, no streams found) | `{"error": "resolution failed", "details": "yt-dlp: ERROR: Unsupported URL"}` |
| `503 Service Unavailable` | GStreamer pipeline construction failed | `{"error": "playback engine unavailable"}` |

---

#### `POST /api/stop`

Stops the current playback session and releases GStreamer pipeline resources.

**Request Body:**

```json
{
  "sessionId": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `sessionId` | `string` | no | The session to stop. If omitted, stops the currently active session. Returns `404` if the session doesn't match the active one. |

**Response:** `200 OK`

```json
{
  "status": "idle",
  "previousSessionId": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
}
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| `404 Not Found` | No active session, or `sessionId` doesn't match |
| `500 Internal Server Error` | GStreamer pipeline failed to stop cleanly |

---

#### `POST /api/pause`

Toggles pause state. If playing, pauses. If paused, resumes.

**Request Body:** `{}` (empty object)

**Response:** `200 OK`

```json
{
  "sessionId": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "status": "paused",
  "position": 47.3,
  "duration": 212.0
}
```

The `status` field reflects the new state (`"paused"` or `"playing"`). The `position` and `duration` are in seconds.

---

#### `POST /api/seek`

Seeks to a position within the current media.

**Request Body:**

```json
{
  "seconds": 120.0,
  "mode": "absolute"
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `seconds` | `number` | **yes** | — | Target position in seconds. Must be ≥ 0 and ≤ duration. |
| `mode` | `string` | no | `"absolute"` | `"absolute"` — seek to `seconds` from start. `"relative"` — seek `seconds` from current position (can be negative). |

**Response:** `200 OK`

```json
{
  "sessionId": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "status": "seeking",
  "targetPosition": 120.0
}
```

The `status` field is `"seeking"` during the seek operation. The client should wait for a `MEDIA_STATUS` WebSocket message with `"playing"` to confirm seek completion.

**Error Responses:**

| Status | Condition |
|--------|-----------|
| `400 Bad Request` | `seconds` is negative (in absolute mode) or would result in position < 0 or > duration |
| `409 Conflict` | No active session |

---

#### `GET /api/status`

Returns the complete state of the current (or most recent) session.

**Response:** `200 OK`

```json
{
  "sessionId": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "status": "playing",
  "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
  "title": "Rick Astley - Never Gonna Give You Up",
  "position": 47.3,
  "duration": 212.0,
  "volume": 0.8,
  "muted": false,
  "torMode": "full",
  "bufferPercent": 72,
  "videoCodec": "H.264",
  "videoResolution": "1920x1080",
  "audioCodec": "AAC",
  "subtitleTrack": "en",
  "availableSubtitles": ["en", "es", "fr", "de"],
  "sourceType": "youtube",
  "resolvedAt": "2025-03-04T10:23:45Z"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `sessionId` | `string?` | Session ID, or `null` if idle |
| `status` | `string` | One of: `"idle"`, `"resolving"`, `"buffering"`, `"playing"`, `"paused"`, `"seeking"`, `"error"`, `"stopped"` |
| `url` | `string?` | Original URL cast by the user |
| `title` | `string` | Display title |
| `position` | `number` | Current playback position in seconds (0.0 if idle) |
| `duration` | `number` | Total duration in seconds (-1.0 if unknown/live) |
| `volume` | `number` | Volume level 0.0–1.0 |
| `muted` | `boolean` | Mute state |
| `torMode` | `string` | Active Tor mode for this session |
| `bufferPercent` | `number` | Buffer fill percentage 0–100 (from GStreamer `queue2` buffering signal) |
| `videoCodec` | `string?` | Detected video codec (e.g., `"H.264"`, `"HEVC"`) |
| `videoResolution` | `string?` | Video resolution (e.g., `"1920x1080"`) |
| `audioCodec` | `string?` | Detected audio codec (e.g., `"AAC"`, `"Opus"`) |
| `subtitleTrack` | `string?` | Active subtitle language code, or `null` |
| `availableSubtitles` | `string[]` | List of available subtitle language codes |
| `sourceType` | `string?` | Source classification: `"direct"`, `"adaptive"`, `"youtube"`, `"vimeo"`, `"twitch"`, `"peertube"`, `"other"` |
| `resolvedAt` | `string?` | ISO 8601 timestamp when URL was resolved |

When no session is active, the response returns a minimal object:

```json
{
  "sessionId": null,
  "status": "idle"
}
```

---

### 2.2 WebSocket Protocol (Port 8586)

The WebSocket provides real-time, push-based state updates to connected clients. Multiple clients may connect simultaneously—boGDan broadcasts state changes to all connected WebSocket clients.

**Connection URL:** `ws://<pi-ip>:8586/ws`  
**Protocol:** RFC 6455 (standard WebSocket)  
**Subprotocol:** None (no `Sec-WebSocket-Protocol` negotiation)  
**Ping/Pong:** Server sends ping every 30 seconds; clients that don't respond within 10 seconds are disconnected.

All messages are JSON text frames with a `type` field for dispatch.

---

#### Server → Client Messages

**`MEDIA_STATUS`**

Broadcast whenever playback state changes. Payload matches `GET /api/status` response format.

```json
{
  "type": "MEDIA_STATUS",
  "payload": {
    "sessionId": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
    "status": "playing",
    "position": 47.3,
    "duration": 212.0,
    "bufferPercent": 72
  }
}
```

Sent on: play, pause, seek, buffer underrun, resolution complete, error, stop.

**`RESOLVE_PROGRESS`**

Sent during yt-dlp resolution to provide progress indication. Emitted every 5 seconds during resolution.

```json
{
  "type": "RESOLVE_PROGRESS",
  "payload": {
    "sessionId": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
    "elapsedSeconds": 12,
    "phase": "extracting_info",
    "message": "Downloading webpage..."
  }
}
```

| `phase` value | Meaning |
|---------------|---------|
| `"initializing"` | yt-dlp subprocess started, Python loading |
| `"downloading_webpage"` | Fetching the page HTML |
| `"extracting_info"` | Parsing page to find stream URLs |
| `"downloading_formats"` | Fetching format manifest (HLS/DASH) |
| `"selecting_format"` | Choosing best format per resolution constraints |
| `"downloading_subtitles"` | Fetching subtitle files |
| `"complete"` | Resolution finished successfully |

**`ERROR`**

Sent when an error occurs that isn't tied to a specific client request.

```json
{
  "type": "ERROR",
  "payload": {
    "severity": "fatal",
    "category": "playback",
    "message": "GStreamer pipeline error: Internal data stream error",
    "sessionId": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
    "recoverable": false
  }
}
```

| `severity` | Action |
|------------|--------|
| `"warning"` | Non-critical; playback continues |
| `"error"` | Playback interrupted; may be recoverable |
| `"fatal"` | Session terminated; user action required |

| `category` | Meaning |
|------------|---------|
| `"resolution"` | URL resolution failure |
| `"playback"` | GStreamer pipeline error |
| `"network"` | Network connectivity issue |
| `"tor"` | Tor circuit or SOCKS5 failure |
| `"system"` | System resource error (OOM, disk, etc.) |

**`QUEUE_UPDATE`**

Sent when the play queue changes. (v1 supports single-item playback; this message type is defined for forward compatibility with queue support in v2.)

```json
{
  "type": "QUEUE_UPDATE",
  "payload": {
    "queueLength": 1,
    "currentIndex": 0,
    "items": [
      {
        "title": "Rick Astley - Never Gonna Give You Up",
        "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
        "duration": 212
      }
    ]
  }
}
```

---

#### Client → Server Messages

**`CAST`**

Initiates casting (equivalent to `POST /api/cast`).

```json
{
  "type": "CAST",
  "payload": {
    "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
    "title": "Rick Astley - Never Gonna Give You Up",
    "resumePosition": 0,
    "torMode": "full"
  }
}
```

**`STOP`**

Stops the current session.

```json
{
  "type": "STOP",
  "payload": {}
}
```

**`PAUSE`**

Toggles pause state.

```json
{
  "type": "PAUSE",
  "payload": {}
}
```

**`SEEK`**

Seeks to a position.

```json
{
  "type": "SEEK",
  "payload": {
    "seconds": 120.0,
    "mode": "absolute"
  }
}
```

**`VOLUME`**

Sets volume level.

```json
{
  "type": "VOLUME",
  "payload": {
    "level": 0.8,
    "muted": false
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `level` | `number` | no | Volume 0.0–1.0. If omitted, volume unchanged. |
| `muted` | `boolean` | no | Mute state. If omitted, mute state unchanged. |

**`SUBTITLE`**

Selects or disables subtitle track.

```json
{
  "type": "SUBTITLE",
  "payload": {
    "track": "en",
    "enabled": true
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `track` | `string` | yes | Language code from `availableSubtitles`, or `"none"` to disable. |
| `enabled` | `boolean` | no | `true` to enable, `false` to disable. Default: `true`. |

---

### 2.3 UPnP/DLNA Interface (Port 49152)

boGDan exposes a DLNA MediaRenderer device via gmediarender on port 49152. The device is discoverable via SSDP on the LAN.

**Device Description URL:** `http://<pi-ip>:49152/description.xml`

**Device Type:** `urn:schemas-upnp-org:device:MediaRenderer:1`

**Friendly Name:** `boGDan` (configurable via `/etc/bogdan/bogdan.conf`)

**UDN:** `uuid:bogdan-<machine-id>` (derived from `/etc/machine-id`)

---

#### AVTransport Service (`urn:schemas-upnp-org:service:AVTransport:1`)

**Service URL:** `http://<pi-ip>:49152/upnp/control/AVTransport`

| Action | Arguments | Description |
|--------|-----------|-------------|
| `SetAVTransportURI` | `InstanceID=0`, `CurrentURI=<url>`, `CurrentURIMetaData=<didl>` | Sets the media URL to play. boGDan will attempt to resolve the URL through the same classification pipeline (direct media → adaptive manifest → page URL → yt-dlp). |
| `Play` | `InstanceID=0`, `Speed="1"` | Starts or resumes playback. |
| `Pause` | `InstanceID=0` | Pauses playback. |
| `Stop` | `InstanceID=0` | Stops playback and clears the transport. |
| `Seek` | `InstanceID=0`, `Unit="REL_TIME"`, `Target="00:02:00"` | Seeks to the specified position. `Unit` must be `"REL_TIME"` (HH:MM:SS format). |
| `GetTransportInfo` | `InstanceID=0` | Returns `CurrentTransportState` (`"NO_MEDIA_PRESENT"`, `"PLAYING"`, `"PAUSED_PLAYBACK"`, `"STOPPED"`), `CurrentTransportStatus` (`"OK"` or `"ERROR_OCCURRED"`), `CurrentSpeed` (`"1"`). |
| `GetPositionInfo` | `InstanceID=0` | Returns `RelTime` (current position as HH:MM:SS), `AbsTime`, `TrackDuration` (total duration as HH:MM:SS). |

---

#### RenderingControl Service (`urn:schemas-upnp-org:service:RenderingControl:1`)

**Service URL:** `http://<pi-ip>:49152/upnp/control/RenderingControl`

| Action | Arguments | Description |
|--------|-----------|-------------|
| `SetVolume` | `InstanceID=0`, `Channel="Master"`, `DesiredVolume=<0-100>` | Sets volume as integer 0–100. boGDan maps this to GStreamer's `volume` element (0.0–1.0). |
| `GetVolume` | `InstanceID=0`, `Channel="Master"` | Returns `CurrentVolume` as integer 0–100. |
| `SetMute` | `InstanceID=0`, `Channel="Master"`, `DesiredMute=<0|1>` | Sets mute state. |
| `GetMute` | `InstanceID=0`, `Channel="Master"` | Returns `CurrentMute` as integer 0 or 1. |

---

## 3. Format Support Matrix

### 3.1 Video Codecs

| Codec | HW Decode | Zero-Copy | Max Resolution | Support Level | Notes |
|-------|-----------|-----------|----------------|---------------|-------|
| **H.264** (AVC) | ✅ `v4l2h264dec` | ✅ DMA-BUF → kmssink | 1080p60 | **Full** | Primary codec. All GStreamer elements proven on Pi 4. Forced via yt-dlp format selection. |
| **HEVC** (H.265) | ✅ `v4l2h265dec` | ❌ SAND format output | 4Kp60 (decode only) | **Fallback** | HW decode works but SAND output is incompatible with HVS. yt-dlp forces H.264. Software fallback via `avdec_h265` limited to 720p30. Deferred to v2 (see ADR-009). |
| **VP9** | ❌ No HW on Pi 4 | N/A | 720p30 (software) | **Limited** | Software decode via `avdec_vp9` or `libvpx`. CPU-intensive; 720p30 is the practical limit. YouTube VP9 streams avoided by forcing H.264 in yt-dlp. |
| **AV1** | ❌ No HW on Pi 4 | N/A | 480p30 (software) | **Minimal** | Software decode via `av1dec` (dav1d). Extremely CPU-intensive on Pi 4. Only used as last-resort fallback for AV1-only content. |
| **MPEG-2** | ❌ No HW on Pi 4 | N/A | 1080i (software) | **Limited** | Software decode via `avdec_mpeg2video`. No MPEG-2 HW decode license on Pi 4 (unlike Pi 3 which had it as a paid option). |
| **VC-1** | ❌ No HW on Pi 4 | N/A | 720p30 (software) | **Limited** | Software decode via `avdec_vc1`. Rarely encountered on the open web. |

### 3.2 Container Formats

| Container | GStreamer Element | Support Level | Notes |
|-----------|-------------------|---------------|-------|
| **MP4 / MOV** | `qtdemux` | **Full** | Most common container. Supports moov atom parsing, fragmented MP4, and fast-start. |
| **WebM** | `matroskademux` | **Full** | WebM is a subset of Matroska. Full support for Vorbis/Opus audio, VP8/VP9 video. |
| **MKV** | `matroskademux` | **Full** | General Matroska container. Supports embedded subtitles (SRT, ASS, VobSub). Chapter markers parsed but not exposed in v1. |
| **AVI** | `avidemux` | **Full** | Legacy container. Well-supported for backward compatibility. |
| **MPEG-TS** | `tsdemux` | **Full** | Transport stream. Used by HLS segments and broadcast captures. Supports H.264 + AAC common combo. |
| **FLV** | `flvdemux` | **Partial** | Flash Video container. Supports H.264 + AAC (most FLV files). Does not support VP6 or Sorenson Spark (legacy Flash codecs). |

### 3.3 Streaming Protocols

| Protocol | GStreamer Element | ABR | Tor-Compatible | Notes |
|----------|-------------------|-----|----------------|-------|
| **HLS** | `hlsdemux` | ✅ | ✅ | HTTP Live Streaming. Primary adaptive protocol for YouTube, Twitch, and most CDNs. `hlsdemux` handles master playlist parsing, variant selection, and segment fetching. Fully compatible with Tor SOCKS5 proxy. |
| **DASH** | `dashdemux` | ✅ | ✅ | Dynamic Adaptive Streaming over HTTP. Used by YouTube (alongside HLS). `dashdemux` handles MPD parsing, period/ adaptation set selection, and segment fetching. Tor-compatible. |
| **Progressive HTTP** | `souphttpsrc` | ❌ | ✅ | Direct media file download over HTTP. No adaptive bitrate—single quality. Used for direct media URLs (`.mp4`, `.webm`). Tor-compatible via `souphttpsrc` proxy property. |
| **RTMP** | `rtmpsrc` | ❌ | Partial | Real-Time Messaging Protocol. Used by some live streams. RTMP runs over TCP, so it can theoretically go through Tor SOCKS5, but many RTMP servers reject connections from Tor exit-like latency patterns. Unreliable through Tor. |
| **RTSP** | `rtspsrc` | ❌ | ❌ | Real-Time Streaming Protocol. Uses UDP for media transport, which cannot traverse Tor's SOCKS5 proxy. RTSP-over-TCP (`protocols=tcp`) could work but is not implemented in v1. Not Tor-compatible. |

### 3.4 Subtitle Formats

| Format | Extraction Method | Rendering Approach | Notes |
|--------|-------------------|-------------------|-------|
| **SRT** (SubRip) | yt-dlp `--write-subs` / embedded in container | `subtitleoverlay` element | Most common subtitle format. Simple timestamp + text. Fully supported. |
| **VTT** (WebVTT) | yt-dlp `--write-subs --sub-format vtt` / embedded in HLS | `subtitleoverlay` element | Web standard subtitle format. Supports basic styling (bold, italic, color). `subtitleoverlay` handles VTT natively. |
| **SSA/ASS** (SubStation Alpha) | yt-dlp `--write-subs` / embedded in MKV | `subtitleoverlay` element | Advanced subtitle format with positioning, styling, and karaoke effects. `subtitleoverlay` renders basic SSA; complex ASS effects (animations, drawing) are simplified. |
| **EIA-608** (Closed Captions) | Extracted from H.264 SEI NAL units by `closedcaption` element | `cccombiner` → `subtitleoverlay` | North American closed caption standard embedded in video stream. Extracted automatically by GStreamer's `closedcaption` infrastructure. |
| **Auto-generated** (YouTube) | yt-dlp `--write-auto-subs --sub-format vtt` | `subtitleoverlay` element | YouTube's machine-generated subtitles. Downloaded as VTT format. Quality varies; presented with "auto-generated" label in subtitle selector. |

---

## 4. Content Resolution Specification

### 4.1 URL Classification

When a URL arrives (via any interface), boGDan classifies it into one of three categories before proceeding with resolution:

| Category | Detection Method | Resolution Path | Typical Latency | Example |
|----------|-----------------|-----------------|-----------------|---------|
| **Direct Media** | URL path extension matches known media extensions: `.mp4`, `.webm`, `.mkv`, `.avi`, `.mov`, `.ts`, `.flv`, `.m4v`, `.ogv`, `.wmv` | Pass URL directly to GStreamer `souphttpsrc` | < 1 second | `https://example.com/video.mp4` |
| **Adaptive Manifest** | URL path ends with `.m3u8` (HLS) or `.mpd` (DASH), or URL contains known manifest path patterns (`/manifest.m3u8`, `/dash.mpd`) | Pass URL to GStreamer `hlsdemux` or `dashdemux` via `uridecodebin` | < 1 second | `https://cdn.example.com/stream.m3u8` |
| **Page URL** | Does not match Direct Media or Adaptive Manifest patterns | Invoke yt-dlp subprocess for URL resolution | 5–30 seconds | `https://www.youtube.com/watch?v=dQw4w9WgXcQ` |

**Classification algorithm (pseudocode):**

```
function classify(url):
    path = urlparse(url).path.lower()

    if path matches /\.(mp4|webm|mkv|avi|mov|ts|flv|m4v|ogv|wmv)(\?.*)?$/:
        return DIRECT_MEDIA

    if path matches /\.(m3u8|mpd)(\?.*)?$/:
        return ADAPTIVE_MANIFEST

    # Check known CDN manifest patterns
    if url contains "googlevideo.com/videoplayback":
        # Could be direct segment or manifest; try adaptive
        return ADAPTIVE_MANIFEST

    return PAGE_URL
```

If Direct Media or Adaptive Manifest classification fails at playback time (e.g., the URL returns an HTML page instead of media), boGDan falls back to Page URL classification and invokes yt-dlp.

### 4.2 yt-dlp Invocation Specification

When a URL is classified as Page URL, boGDan invokes yt-dlp as a subprocess. The complete invocation is:

```bash
yt-dlp \
  --proxy "socks5h://bogdan-2d5b0a1c:x@127.0.0.1:9050" \
  --no-check-certificates \
  -J \
  --no-playlist \
  --write-subs \
  --write-auto-subs \
  --sub-langs 'en,.*,en-US' \
  --sub-format 'vtt/srt' \
  -o '/tmp/bogdan/subs/<session-id>/%(title)s' \
  -f 'bestvideo[vcodec^=avc1][height<=720]+bestaudio/bestvideo[vcodec^=avc1]+bestaudio/best[height<=720]/best' \
  "<URL>"
```

**Parameter explanations:**

| Parameter | Purpose |
|-----------|---------|
| `--proxy socks5h://bogdan-<hash>:x@127.0.0.1:9050` | Route request through Tor SOCKS5 proxy. The SOCKS5 username encodes the domain hash for circuit isolation (see §6.2). The `h` in `socks5h` ensures DNS resolution occurs on the Tor exit (not locally). The password `x` is a dummy value required by the SOCKS5 auth protocol. |
| `--no-check-certificates` | Skip TLS certificate verification. Some Tor exit nodes present certificates that don't match the hostname. Also handles sites with self-signed or expired certificates. |
| `-J` | Dump video metadata as JSON to stdout. Single JSON object containing all available formats, thumbnails, subtitles, and video metadata. boGDan parses this to select the optimal stream URL. |
| `--no-playlist` | If the URL is a playlist, extract only the first video. boGDan v1 does not support playlist playback. |
| `--write-subs` | Download available subtitle files for the video. Only downloads subtitles that have been manually uploaded by the content creator. |
| `--write-auto-subs` | Also download auto-generated subtitles (e.g., YouTube's speech-to-text). These are lower quality but provide coverage for videos without manual subtitles. |
| `--sub-langs 'en,.*,en-US'` | Download subtitles in English (`en`), all available languages (`.*`), and specifically US English (`en-US`). The `.*` wildcard ensures all languages are available for user selection. |
| `--sub-format 'vtt/srt'` | Prefer VTT format, fall back to SRT. VTT supports basic styling and is the web standard. |
| `-o '/tmp/bogdan/subs/<session-id>/%(title)s'` | Write subtitle files to a per-session temporary directory. The `%(title)s` template ensures filenames are human-readable. The session ID directory enables cleanup on session end. |
| `-f 'bestvideo[vcodec^=avc1][height<=720]+bestaudio/...'` | Format selection string (see below). |

**Format selection string breakdown:**

```
bestvideo[vcodec^=avc1][height<=720]+bestaudio   ← Prefer: H.264, ≤720p, best audio
/bestvideo[vcodec^=avc1]+bestaudio                ← Fallback: H.264, any resolution, best audio
/best[height<=720]                                 ← Fallback: any codec, ≤720p
/best                                              ← Last resort: any codec, any resolution
```

The format selection prioritizes H.264 (`vcodec^=avc1`) for zero-copy hardware decode, caps resolution at 720p by default (configurable via `maxResolution` setting) to match Tor bandwidth constraints, and progressively relaxes constraints if earlier selections are unavailable.

When `torMode` is `"off"` and `maxResolution` is set higher, the format string is regenerated:

```
# maxResolution=1080, torMode=off:
bestvideo[vcodec^=avc1][height<=1080]+bestaudio/bestvideo[vcodec^=avc1]+bestaudio/best[height<=1080]/best
```

### 4.3 Timeout and Error Handling

| Phase | Timeout | On Timeout | On Error |
|-------|---------|------------|----------|
| **yt-dlp invocation** | 30 seconds | Kill subprocess (`SIGKILL`), report `RESOLUTION_TIMEOUT` error to client. Retry once with `--no-cache` flag. | Parse stderr for known error patterns (age-gate, geo-block, deleted). Report `RESOLUTION_FAILED` with human-readable message. |
| **Stream URL validation** | 10 seconds | HTTP HEAD request to the resolved URL times out. Report `STREAM_UNREACHABLE` error. Attempt alternate format from yt-dlp's format list (if available). | HTTP status code ≥ 400. Report `STREAM_ERROR` with HTTP status code. Attempt alternate format. |
| **Initial buffering** | 60 seconds | GStreamer `queue2` has not reached `low-percent` (15%) within 60 seconds of pipeline start. Report `BUFFER_TIMEOUT` error. May indicate insufficient bandwidth for selected quality. | Pipeline state change to `PLAYING` fails. Report `PLAYBACK_FAILED`. |
| **Playback stall** | 15 seconds | GStreamer reports 0 bytes/second throughput for 15 consecutive seconds during playback. Report `PLAYBACK_STALLED`. Attempt re-resolution of the URL (same session). | Pipeline error on the GStreamer bus. Classify severity. For `stream-error` with code `ResourceNotFound`, attempt re-resolution. For `library-error` or `codec-error`, report `PLAYBACK_FAILED` (non-recoverable). |

**Retry policy:**

- Maximum 2 re-resolution attempts per session.
- After 2 failures, report `PLAYBACK_FAILED` and transition to `error` state.
- Re-resolution creates a new yt-dlp subprocess with a fresh Tor circuit (new SOCKS5 username suffix with timestamp).
- Between retries, a 3-second delay allows Tor circuit establishment.

---

## 5. Browser Extension Specification

### 5.1 Manifest Requirements

The boGDan browser extension is built on Manifest V3 (the only accepted format for Chrome Web Store submissions since June 2024).

**`manifest.json` key fields:**

```json
{
  "manifest_version": 3,
  "name": "boGDan",
  "version": "1.0.0",
  "description": "Cast web video to your boGDan device",
  "permissions": [
    "webRequest",
    "activeTab"
  ],
  "host_permissions": [
    "<all_urls>"
  ],
  "background": {
    "service_worker": "background.js",
    "type": "module"
  },
  "action": {
    "default_popup": "popup.html",
    "default_icon": {
      "16": "icons/icon16.png",
      "32": "icons/icon32.png",
      "48": "icons/icon48.png",
      "128": "icons/icon128.png"
    }
  },
  "options_page": "options.html",
  "icons": {
    "16": "icons/icon16.png",
    "32": "icons/icon32.png",
    "48": "icons/icon48.png",
    "128": "icons/icon128.png"
  }
}
```

**Permissions rationale:**

| Permission | Justification |
|------------|---------------|
| `webRequest` | Required to intercept network requests and identify media URLs (HLS manifests, DASH manifests, direct video files). Without this, the extension cannot detect playable media on the page. |
| `activeTab` | Required to access the URL of the currently active tab when the user clicks the boGDan action button. Unlike the `tabs` permission, `activeTab` is granted only on user interaction (clicking the extension icon), which is less intrusive. |
| `<all_urls>` (host permission) | Required for `webRequest` interception across all sites. Without this, `webRequest` events only fire for explicitly listed domains. Since media can be served from any domain (CDNs, embeds), all URLs must be monitored. |

**Explicitly NOT requested:**

| Permission | Why omitted |
|------------|-------------|
| `tabs` | Would grant access to all tab URLs at all times. `activeTab` achieves the same result on user interaction with less privilege. |
| `scripting` | Not needed. The extension does not inject content scripts. Media detection relies on `webRequest` interception, not DOM inspection. |
| `downloads` | Not needed. The extension does not download files. |

### 5.2 Network Interception

The extension registers a `chrome.webRequest.onBeforeRequest` listener in the background service worker. This listener examines every network request to identify media URLs.

**Listener configuration:**

```javascript
chrome.webRequest.onBeforeRequest.addListener(
  onBeforeRequestHandler,
  { urls: ["<all_urls>"] },
  ["requestBody"]  // needed for POST-based DASH manifest requests
);
```

**Signature detection table:**

| Signature | URL Pattern | Confidence | Classification | Notes |
|-----------|-------------|------------|----------------|-------|
| HLS Manifest | URL ends with `.m3u8` or contains `m3u8` in query params | **High** | `hls_manifest` | May be master playlist (contains `#EXT-X-STREAM-INF`) or media playlist. Both are captured. |
| DASH Manifest | URL ends with `.mpd` or contains `mpd` in query params | **High** | `dash_manifest` | MPD (Media Presentation Description) is an XML document describing DASH streams. |
| Direct Video | URL ends with `.mp4`, `.webm`, `.mkv`, `.avi`, `.mov`, `.ts`, `.flv` | **High** | `direct_video` | Direct media file URL. Content-Type header is checked on response to confirm (optional validation). |
| HLS Segment | URL ends with `.ts` or `.m4s` and referrer is an HLS manifest URL | **Medium** | `hls_segment` | Individual HLS segment. Not useful for casting directly (need the manifest), but indicates HLS stream is active. |
| CDN Pattern | URL hostname matches `googlevideo.com`, `cdninstagram.com`, `twimg.com`, `akamaized.net`, `cloudfront.net`, `fbcdn.net` | **High** | `cdn_media` | Known media CDN domains. The URL is likely a media segment or manifest. Checked in combination with path patterns for higher confidence. |

**Interception state management:**

The service worker maintains an in-memory map of intercepted URLs, keyed by tab ID:

```javascript
// Tab ID → Array of intercepted URLs
const interceptedUrls = new Map();

// Each entry:
{
  url: "https://manifest.googlevideo.com/...",
  type: "hls_manifest",        // classification
  confidence: "high",           // confidence level
  timestamp: 1709545625000,     // when intercepted (ms since epoch)
  tabUrl: "https://youtube.com/...",  // the page that triggered it
  contentType: null             // filled in by onHeadersReceived if available
}
```

Entries are garbage-collected after 10 minutes or when the tab is closed (whichever comes first). A maximum of 50 entries per tab prevents memory exhaustion on pages with many network requests.

**`onHeadersReceived` enrichment:**

A secondary listener on `chrome.webRequest.onHeadersReceived` enriches intercepted URL entries with `Content-Type` header data, improving classification accuracy:

```javascript
chrome.webRequest.onHeadersReceived.addListener(
  onHeadersReceivedHandler,
  { urls: ["<all_urls>"] },
  ["responseHeaders"]
);
```

If `Content-Type` starts with `video/` or `application/vnd.apple.mpegurl` (HLS) or `application/dash+xml` (DASH), the entry's classification is confirmed or upgraded.

### 5.3 Cast Button Behavior

When the user clicks the boGDan extension action button, the following logic executes:

**Case 1: Intercepted URLs are available**

1. Retrieve the intercepted URL list for the active tab.
2. Filter to entries with confidence `"high"`.
3. Prefer HLS/DASH manifests over direct video URLs (manifests enable ABR).
4. Select the most recent matching entry (latest timestamp).
5. Send a `CAST` message to the boGDan device with the intercepted URL.
6. Display "Casting..." badge on the extension icon (green dot).

**Case 2: No intercepted URLs available**

1. Retrieve the active tab's page URL via `chrome.tabs.query({ active: true, currentWindow: true })`.
2. Send a `CAST` message to the boGDan device with the page URL (boGDan will use yt-dlp to resolve it).
3. Display "Resolving..." badge on the extension icon (yellow dot).

**Case 3: Cannot connect to boGDan device**

1. Attempt WebSocket connection to `ws://<pi-address>:8586/ws` with a 5-second timeout.
2. On failure, attempt HTTP `GET /api/status` with a 5-second timeout.
3. On both failures, display red error badge on the extension icon.
4. Show notification: "Cannot connect to boGDan at \<pi-address\>. Check that the device is powered on and on the same network."
5. Offer "Retry" button in the popup.

**Badge states:**

| State | Badge | Color |
|-------|-------|-------|
| Idle | None | Default |
| Connecting | • (dot) | Yellow (`#F9AB00`) |
| Resolving | ⟳ (spinner) | Yellow (`#F9AB00`) |
| Buffering | ▮ (bar) | Blue (`#4285F4`) |
| Playing | ▶ (triangle) | Green (`#34A853`) |
| Paused | ❚❚ (bars) | Blue (`#4285F4`) |
| Error | ✕ (cross) | Red (`#EA4335`) |

### 5.4 Extension Configuration

The extension provides an options page (`options.html`) with the following configurable settings, stored in `chrome.storage.local`:

| Setting | Key | Type | Default | Description |
|---------|-----|------|---------|-------------|
| Pi Address | `piAddress` | `string` | `"bogdan.local"` | Hostname or IP address of the boGDan device. Supports mDNS hostnames (`.local`) and raw IPs (`192.168.1.100`). |
| Tor Mode | `torMode` | `string` | `"full"` | Default Tor routing mode for new cast sessions: `"full"`, `"resolution-only"`, or `"off"`. Can be overridden per-session in the popup. |
| Prefer Intercepted URLs | `preferIntercepted` | `boolean` | `true` | When `true`, the extension sends intercepted media URLs (HLS/DASH manifests) directly to boGDan. When `false`, always sends the page URL for yt-dlp resolution (slower but more reliable for some sites). |
| Auto-Cast | `autoCast` | `boolean` | `false` | When `true`, the extension automatically casts the first detected media URL without requiring the user to click the action button. Use with caution on sites with auto-playing videos. |
| Max Resolution | `maxResolution` | `number` | `720` | Maximum video resolution in vertical pixels. Valid values: `360`, `480`, `720`, `1080`. Lower values reduce bandwidth requirements, especially important when using Tor. |

**Configuration validation:**

- `piAddress`: Must be non-empty. No schema prefix (no `http://`). Validated by attempting a WebSocket connection on save.
- `maxResolution`: Must be one of the valid values. Invalid values revert to `720`.
- `torMode`: Must be one of the valid values. Invalid values revert to `"full"`.

---

## 6. Tor Configuration Specification

### 6.1 torrc

The complete `/etc/tor/torrc` for boGDan:

```torrc
# boGDan Tor Configuration
# ========================

# SOCKS port 9050: IsolateSOCKSAuth mode
# Each unique SOCKS5 username creates a separate circuit.
# boGDan uses "bogdan-<domain-hash>" as the username to isolate
# different streaming sites onto independent circuits.
SocksPort 9050 IsolateSOCKSAuth

# SOCKS port 9051: IsolateDestAddr mode
# Each unique destination address gets its own circuit.
# Used as a fallback for connections where per-domain isolation
# is not explicitly specified.
SocksPort 9051 IsolateDestAddr

# Circuit management
# ==================

# Maximum time a circuit is kept open before being rotated.
# 600 seconds (10 minutes) balances privacy (frequent rotation)
# against performance (avoiding circuit creation overhead during
# long playback sessions).
MaxCircuitDirtiness 600

# Maximum time to wait for a circuit to be established before
# giving up. 30 seconds allows for slow relays but prevents
# indefinite hangs.
CircuitBuildTimeout 30

# How often to build a new "clean" circuit even if the old one
# hasn't expired. 120 seconds ensures fresh circuits are always
# available.
NewCircuitPeriod 120

# Control port
# ============

# Disabled. boGDan does not need Tor control port access.
# No need for NEWNYM signals — circuit isolation is handled
# via IsolateSOCKSAuth usernames.
ControlPort 0

# Logging
# =======

# Notice-level logging to syslog. Errors and warnings are captured
# by journald via the tor.service unit.
Log notice syslog

# Bandwidth
# =========

# No bandwidth rate limiting. boGDan relies on the Tor network's
# natural throughput. Rate limiting would degrade streaming quality.
# RelayMode is disabled (no Relay or Exit configuration).

# Security
# ========

# Refuse entry guards from local network ranges.
# Prevents a malicious LAN device from becoming an entry node.
EntryNodes {not} PrivateAddresses

# Disable .onion service hosting. boGDan is a client only.
HiddenServiceDir 0

# Strict nodes: never use nodes that are flagged as unstable
# or unverified unless absolutely necessary.
StrictNodes 1
```

### 6.2 Stream Isolation Mapping

boGDan isolates streams from different websites onto independent Tor circuits using SOCKS5 username-based isolation. The SOCKS5 username is derived from the destination domain:

```
username = "bogdan-" + first_8_chars(hex(MD5(domain)))
```

**Isolation examples:**

| Domain | MD5 (first 8 hex chars) | SOCKS5 Username | Circuit |
|--------|------------------------|-----------------|---------|
| `youtube.com` | `2d5b0a1c...` | `bogdan-2d5b0a1c` | Circuit A |
| `vimeo.com` | `7f3e9b2d...` | `bogdan-7f3e9b2d` | Circuit B |
| `twitch.tv` | `a1c4e8f0...` | `bogdan-a1c4e8f0` | Circuit C |
| `peertube.example.com` | `b5d2f6a9...` | `bogdan-b5d2f6a9` | Circuit D |
| `archive.org` | `e3c7a1b4...` | `bogdan-e3c7a1b4` | Circuit E |

**Isolation semantics:**

- Two requests with the same SOCKS5 username share a circuit (if the existing circuit is still valid per `MaxCircuitDirtiness`).
- Two requests with different SOCKS5 usernames always use different circuits.
- This means YouTube and Vimeo traffic never shares a Tor circuit, preventing a relay operator from correlating the two streams.
- Multiple YouTube requests (e.g., initial page fetch + subtitle download) share a circuit because they use the same username.

**yt-dlp proxy string construction:**

```
--proxy "socks5h://bogdan-<domain-hash>:x@127.0.0.1:9050"
```

The password field is always `x` (required by SOCKS5 auth but not used for isolation—only the username matters for `IsolateSOCKSAuth`).

**GStreamer proxy configuration:**

GStreamer's `souphttpsrc` element uses the same SOCKS5 proxy:

```python
souphttpsrc.set_property("proxy", "socks5h://127.0.0.1:9050")
souphttpsrc.set_property("proxy-id", "bogdan-<domain-hash>")
souphttpsrc.set_property("proxy-pw", "x")
```

Note: `souphttpsrc` in GStreamer 1.22+ supports SOCKS5 proxy with authentication via the `proxy-id` and `proxy-pw` properties.

### 6.3 Bandwidth Expectations

Tor's bandwidth varies significantly based on relay selection, network congestion, and the entry-middle-exit path quality. boGDan's quality expectations must account for this variability.

| Metric | Value | Notes |
|--------|-------|-------|
| **Minimum sustained throughput** | 500 Kbps | Sufficient for 360p H.264 at acceptable quality. Below this, playback will experience frequent buffering. |
| **Median sustained throughput** | 1–2 Mbps | Supports 480p–720p H.264. This is the typical Tor experience for well-connected exits. |
| **Best-case sustained throughput** | 3–5 Mbps | Supports 720p–1080p H.264. Achievable with high-bandwidth exits (Unlimited, Fast flags) and favorable path. Rare but possible. |
| **Circuit creation latency** | 5–15 seconds | Time from SOCKS5 connect to first byte. Three relay handshakes (TLS + ntor) plus congestion. |
| **Circuit lifetime** | 10 minutes | After `MaxCircuitDirtiness`, the circuit is closed and a new one is built. Playback may briefly pause (1–3 seconds) during circuit rotation. |

**ABR implications:**

The default `maxResolution` of 720p in `full` Tor mode is conservative. HLS/DASH ABR will automatically downgrade to lower quality representations if buffer fill rate drops below playback rate. The `queue2` buffer provides a 50 MB shock absorber (~30 seconds at 720p, ~120 seconds at 360p).

**Tor mode comparison:**

| Mode | yt-dlp Resolution | Media Streaming | Privacy Level | Expected Quality |
|------|-------------------|-----------------|---------------|------------------|
| `full` | Via Tor | Via Tor | Maximum—no LAN observer can see destination | 480p–720p (limited by Tor bandwidth) |
| `resolution-only` | Via Tor | Direct (LAN → CDN) | Medium—LAN observer sees CDN connection but not which page was visited | Up to 1080p (limited by ISP bandwidth) |
| `off` | Direct | Direct | None—all traffic visible on LAN | Up to 1080p (limited by ISP bandwidth) |

---

## 7. Network Security Specification

### 7.1 iptables Rules

boGDan uses `iptables` to enforce network traffic restrictions. The default policy for OUTPUT is DROP—only explicitly allowed traffic may leave the device.

```bash
#!/bin/bash
# /etc/iptables/bogdan-rules.sh
# boGDan Firewall Rules v1.0

# ============================================================
# INPUT CHAIN — Control which inbound connections are accepted
# ============================================================

# Set default policy
iptables -P INPUT DROP

# Allow established connections and related traffic
iptables -A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT

# Allow loopback
iptables -A INPUT -i lo -j ACCEPT

# boGDan HTTP REST API (port 8585) — LAN only
iptables -A INPUT -p tcp --dport 8585 -s 192.168.0.0/16 -j ACCEPT
iptables -A INPUT -p tcp --dport 8585 -s 10.0.0.0/8 -j ACCEPT
iptables -A INPUT -p tcp --dport 8585 -s 172.16.0.0/12 -j ACCEPT

# boGDan WebSocket (port 8586) — LAN only
iptables -A INPUT -p tcp --dport 8586 -s 192.168.0.0/16 -j ACCEPT
iptables -A INPUT -p tcp --dport 8586 -s 10.0.0.0/8 -j ACCEPT
iptables -A INPUT -p tcp --dport 8586 -s 172.16.0.0/12 -j ACCEPT

# DLNA MediaRenderer (port 49152) — LAN only
iptables -A INPUT -p tcp --dport 49152 -s 192.168.0.0/16 -j ACCEPT
iptables -A INPUT -p tcp --dport 49152 -s 10.0.0.0/8 -j ACCEPT
iptables -A INPUT -p tcp --dport 49152 -s 172.16.0.0/12 -j ACCEPT

# SSDP discovery (port 1900) — multicast UDP, LAN only
iptables -A INPUT -p udp --dport 1900 -d 239.255.255.250 -s 192.168.0.0/16 -j ACCEPT
iptables -A INPUT -p udp --dport 1900 -d 239.255.255.250 -s 10.0.0.0/8 -j ACCEPT
iptables -A INPUT -p udp --dport 1900 -d 239.255.255.250 -s 172.16.0.0/12 -j ACCEPT

# mDNS (port 5353) — multicast UDP, LAN only
iptables -A INPUT -p udp --dport 5353 -d 224.0.0.251 -s 192.168.0.0/16 -j ACCEPT
iptables -A INPUT -p udp --dport 5353 -d 224.0.0.251 -s 10.0.0.0/8 -j ACCEPT
iptables -A INPUT -p udp --dport 5353 -d 224.0.0.251 -s 172.16.0.0/12 -j ACCEPT

# SSH (port 22) — LAN only (for administration)
iptables -A INPUT -p tcp --dport 22 -s 192.168.0.0/16 -j ACCEPT
iptables -A INPUT -p tcp --dport 22 -s 10.0.0.0/8 -j ACCEPT
iptables -A INPUT -p tcp --dport 22 -s 172.16.0.0/12 -j ACCEPT

# Log and reject everything else
iptables -A INPUT -j LOG --log-prefix "BOGDAN-INPUT-DROP: " --log-level 4
iptables -A INPUT -j REJECT --reject-with icmp-port-unreachable


# ============================================================
# OUTPUT CHAIN — Control which outbound connections are allowed
# ============================================================

# Set default policy — DROP everything not explicitly allowed
iptables -P OUTPUT DROP

# Allow established connections
iptables -A OUTPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT

# Allow loopback
iptables -A OUTPUT -o lo -j ACCEPT

# Allow Tor daemon (debian-tor user) to make outbound connections
# This is the ONLY path to the internet. All WAN traffic must go through Tor.
iptables -A OUTPUT -m owner --uid-owner debian-tor -j ACCEPT

# Allow LAN connections for boGDan services (HTTP, WS, DLNA responses, SSDP, mDNS)
iptables -A OUTPUT -p tcp --sport 8585 -d 192.168.0.0/16 -j ACCEPT
iptables -A OUTPUT -p tcp --sport 8585 -d 10.0.0.0/8 -j ACCEPT
iptables -A OUTPUT -p tcp --sport 8585 -d 172.16.0.0/12 -j ACCEPT

iptables -A OUTPUT -p tcp --sport 8586 -d 192.168.0.0/16 -j ACCEPT
iptables -A OUTPUT -p tcp --sport 8586 -d 10.0.0.0/8 -j ACCEPT
iptables -A OUTPUT -p tcp --sport 8586 -d 172.16.0.0/12 -j ACCEPT

iptables -A OUTPUT -p tcp --sport 49152 -d 192.168.0.0/16 -j ACCEPT
iptables -A OUTPUT -p tcp --sport 49152 -d 10.0.0.0/8 -j ACCEPT
iptables -A OUTPUT -p tcp --sport 49152 -d 172.16.0.0/12 -j ACCEPT

iptables -A OUTPUT -p udp --sport 1900 -d 239.255.255.250 -j ACCEPT
iptables -A OUTPUT -p udp --sport 5353 -d 224.0.0.251 -j ACCEPT

# Allow DNS queries to local dnsmasq (127.0.0.1:53)
iptables -A OUTPUT -p udp --dport 53 -d 127.0.0.1 -j ACCEPT
iptables -A OUTPUT -p tcp --dport 53 -d 127.0.0.1 -j ACCEPT

# Allow DHCP client (for initial network configuration)
iptables -A OUTPUT -p udp --sport 68 --dport 67 -j ACCEPT

# Allow NTP (time synchronization — critical for Tor consensus)
iptables -A OUTPUT -p udp --dport 123 -j ACCEPT

# Allow SSH responses
iptables -A OUTPUT -p tcp --sport 22 -j ACCEPT

# In resolution-only Tor mode, allow bogdan user direct HTTP/HTTPS
# (These rules are added/removed dynamically by bogdand based on torMode)
# iptables -A OUTPUT -m owner --uid-owner bogdan -p tcp --dport 80 -j ACCEPT
# iptables -A OUTPUT -m owner --uid-owner bogdan -p tcp --dport 443 -j ACCEPT

# Log and drop everything else
iptables -A OUTPUT -j LOG --log-prefix "BOGDAN-OUTPUT-DROP: " --log-level 4
iptables -A OUTPUT -j DROP


# ============================================================
# FORWARD CHAIN — boGDan is not a router
# ============================================================
iptables -P FORWARD DROP
```

### 7.2 DNS Leak Prevention

DNS leaks are a critical privacy concern when using Tor. If DNS queries bypass the Tor SOCKS5 proxy and go directly to the LAN's DNS resolver, the resolver (and any network observer) can see which domains the user is visiting, defeating the purpose of Tor.

boGDan implements a three-layer DNS leak prevention strategy:

**Layer 1: `socks5h://` protocol prefix**

All Tor-routed connections use `socks5h://` (not `socks5://`). The `h` suffix instructs the SOCKS5 client to perform DNS resolution on the proxy server (the Tor exit node), not locally. This is the primary defense:

- yt-dlp: `--proxy "socks5h://bogdan-<hash>:x@127.0.0.1:9050"`
- GStreamer: `souphttpsrc` proxy property set to `socks5h://127.0.0.1:9050`

With `socks5h`, the domain name is sent through the Tor circuit to the exit node, which resolves it via the exit's DNS resolver. The local system never makes a DNS query for the destination domain.

**Layer 2: `/etc/resolv.conf` lockdown**

```
# /etc/resolv.conf
nameserver 127.0.0.1
```

The system's resolver is configured to point only at the local dnsmasq instance (`127.0.0.1`). No external DNS servers are configured.

**Layer 3: dnsmasq refuse-all configuration**

```
# /etc/dnsmasq.conf
# boGDan DNS lockdown — dnsmasq refuses all queries
# This ensures that even if an application bypasses socks5h and
# attempts a local DNS resolution, it fails rather than leaking.

# Listen only on loopback
listen-address=127.0.0.1

# Refuse all DNS queries
# Only the Tor daemon resolves domains (via exit nodes)
# Local resolution is deliberately broken to prevent leaks
address=/#/0.0.0.0

# Log refused queries for debugging
log-queries
log-facility=/var/log/dnsmasq.log

# Exception: allow mDNS (.local) resolution for boGDan discovery
server=/local/127.0.0.1#5353
```

With all three layers active, DNS leaks are impossible:

1. `socks5h` ensures DNS is done on the Tor exit (Layer 1).
2. If an application ignores `socks5h` and tries local resolution, it queries `127.0.0.1` (Layer 2).
3. dnsmasq at `127.0.0.1` returns `0.0.0.0` for all queries (Layer 3), causing the application to fail rather than leak.

> **Note:** When `torMode` is `"off"` or `"resolution-only"`, Layers 2 and 3 remain active. For `"resolution-only"`, the media stream bypasses Tor but DNS resolution still occurs via the CDN's IP address (received from yt-dlp's resolved URL, which was obtained through Tor). For `"off"`, dnsmasq is reconfigured to forward queries to the LAN's DNS resolver.

### 7.3 Tor Mode Configuration

boGDan supports three Tor modes, configurable globally and per-session:

| Mode | yt-dlp Resolution | Media Streaming | DNS Resolution | iptables Rules | Privacy Level | Expected Quality |
|------|-------------------|-----------------|----------------|----------------|---------------|------------------|
| **`full`** | Via Tor SOCKS5 (port 9050) | Via Tor SOCKS5 (port 9050) | Remote (Tor exit) | Only `debian-tor` user has WAN access | **Maximum** — No LAN observer can determine which sites are being visited or what content is being streamed | 480p–720p (limited by Tor bandwidth) |
| **`resolution-only`** | Via Tor SOCKS5 (port 9050) | Direct (LAN → CDN) | Remote for yt-dlp, local for media | `debian-tor` + `bogdan` user on ports 80/443 | **Medium** — LAN observer sees CDN connection (can infer content type from CDN hostname and traffic volume) but not which page was visited | Up to 1080p (limited by ISP bandwidth) |
| **`off`** | Direct (no proxy) | Direct (no proxy) | Local (dnsmasq forwarding) | `bogdan` user has full WAN access | **None** — All traffic is visible on the LAN. DNS queries, URLs, and content are observable | Up to 1080p (limited by ISP bandwidth) |

**Dynamic iptables switching:**

When the Tor mode changes, `bogdand` dynamically modifies iptables rules:

- **`full` → `resolution-only`**: Add `iptables -A OUTPUT -m owner --uid-owner bogdan -p tcp --dport 80 -j ACCEPT` and `--dport 443`. Restart the active GStreamer pipeline with the proxy removed.
- **`resolution-only` → `full`**: Remove the `bogdan` user OUTPUT rules. Restart the active GStreamer pipeline with the proxy added.
- **`off`**: Remove all proxy-related iptables rules. Add full WAN access for `bogdan` user. Reconfigure dnsmasq to forward queries.

These rules are persisted in `/etc/iptables/bogdan-dynamic-rules.v4` and applied on boot by a `pre-up` directive in `/etc/network/interfaces`.

---

## 8. Playback Engine Specification

### 8.1 GStreamer Pipeline Configurations

boGDan uses three primary pipeline configurations. All pipelines use V4L2 hardware H.264 decoding and DRM/KMS direct output.

**Pipeline 1: H.264 Progressive HTTP (Direct Media)**

```
souphttpsrc location=<URL> proxy=socks5h://127.0.0.1:9050 proxy-id=bogdan-<hash> proxy-pw=x \
  ! queue2 max-size-bytes=52428800 use-buffering=true low-percent=15 high-percent=70 \
  ! parsebin \
  ! v4l2h264dec io-mode=dmabuf \
  ! kmssink bus-id=vc4hdmi force-modesetting=true can-scale=false
```

Element-by-element explanation:

| Element | Purpose |
|---------|---------|
| `souphttpsrc` | HTTP source with SOCKS5 proxy support. Handles HTTP/1.1, chunked transfer encoding, and range requests (for seeking). Proxy properties route through Tor when `torMode` is `"full"`. |
| `queue2` | Network buffer. `max-size-bytes=52428800` (50 MB) provides ~30 seconds of 720p H.264 buffering. `use-buffering=true` emits `buffering` messages for ABR control. `low-percent=15` / `high-percent=70` set the buffering thresholds. |
| `parsebin` | Auto-detects container format and demuxes. Equivalent to `qtdemux` for MP4, `matroskademux` for MKV, etc. Chooses the correct parser based on stream data. |
| `v4l2h264dec` | V4L2 memory-to-memory H.264 hardware decoder on the Pi 4's VideoCore VI GPU. `io-mode=dmabuf` enables zero-copy output—decoded frames are DMA-BUF file descriptors that `kmssink` can import directly. |
| `kmssink` | DRM/KMS sink. Writes decoded frames directly to the HDMI output via the HVS (Hardware Video Scaler). `bus-id=vc4hdmi` selects the HDMI output. `force-modesetting=true` ensures boGDan sets the display mode. `can-scale=false` disables GStreamer-side scaling (HVS handles scaling in hardware). |

**Pipeline 2: HLS Adaptive Stream**

```
souphttpsrc location=<manifest-url> proxy=socks5h://127.0.0.1:9050 proxy-id=bogdan-<hash> proxy-pw=x \
  ! hlsdemux \
  ! queue2 max-size-bytes=52428800 use-buffering=true low-percent=15 high-percent=70 \
  ! v4l2h264dec io-mode=dmabuf \
  ! kmssink bus-id=vc4hdmi force-modesetting=true can-scale=false
```

Additional elements:

| Element | Purpose |
|---------|---------|
| `hlsdemux` | HLS demuxer. Parses the master playlist, selects a variant stream based on bandwidth, and fetches media segments. Automatically handles segment sequencing, discontinuities, and live playlist updates. For DASH streams, replace with `dashdemux`. |

**Pipeline 3: With Subtitle Overlay**

```
souphttpsrc location=<URL> proxy=socks5h://127.0.0.1:9050 proxy-id=bogdan-<hash> proxy-pw=x \
  ! queue2 max-size-bytes=52428800 use-buffering=true low-percent=15 high-percent=70 \
  ! parsebin name=demux \
  demux. \
  ! v4l2h264dec io-mode=dmabuf \
  ! queue name=videoqueue \
  ! subtitleoverlay name=overlay \
  ! kmssink bus-id=vc4hdmi force-modesetting=true can-scale=false \
  demux. \
  ! subparse \
  ! text/x-raw,format=pango \
  ! overlay.
```

Additional elements:

| Element | Purpose |
|---------|---------|
| `parsebin name=demux` | Named parsebin for linking multiple output pads (video + subtitle). |
| `queue name=videoqueue` | Decouples video decoding from subtitle overlay rendering. Prevents subtitle parsing latency from stalling the video pipeline. |
| `subtitleoverlay` | Composites subtitle text onto the video frame. Accepts `text/x-raw` input from `subparse`. Renders using Pango for text layout and Cairo for compositing. |
| `subparse` | Parses subtitle files (SRT, VTT, SSA/ASS) into `text/x-raw` buffers with timestamps. Auto-detects subtitle format. |

For external subtitle files (downloaded by yt-dlp), a separate `filesrc` branch feeds into `subparse`:

```
filesrc location=/tmp/bogdan/subs/<session-id>/<title>.en.vtt \
  ! subparse \
  ! text/x-raw,format=pango \
  ! overlay.
```

### 8.2 Buffer Configuration

The `queue2` element is the primary buffer management component. Its configuration directly impacts playback stability and ABR behavior.

| Parameter | Value | Rationale |
|-----------|-------|-----------|
| `max-size-bytes` | `52428800` (50 MB) | 50 MB stores ~30 seconds of 720p H.264 at 2 Mbps, or ~120 seconds of 360p at 0.5 Mbps. Large enough to absorb Tor bandwidth variability. Must not exceed available RAM—Pi 4 with 1 GB has ~600 MB available after OS + boGDan. |
| `max-size-time` | `0` (disabled) | Time-based limit disabled. Byte-based limit is sufficient and more predictable for memory usage. |
| `max-size-buffers` | `0` (disabled) | Buffer-count limit disabled. Not meaningful for compressed data where buffer sizes vary. |
| `use-buffering` | `true` | Enables GStreamer buffering messages. The pipeline automatically pauses when buffer drops below `low-percent` and resumes when it reaches `high-percent`. |
| `low-percent` | `15` | When buffer fill drops below 15%, pipeline pauses and waits for rebuffering. 15% of 50 MB = 7.5 MB ≈ 4.5 seconds at 720p. Provides minimum playable buffer. |
| `high-percent` | `70` | When buffer fill reaches 70% during rebuffering, pipeline resumes playback. 70% of 50 MB = 35 MB ≈ 21 seconds at 720p. Provides comfortable buffer headroom. |
| `temp-template` | `/tmp/bogdan/buffer-` | Enables disk-backed buffering for very large files. If `max-size-bytes` is exceeded, `queue2` spills to disk. Prevents OOM on unexpectedly large streams. |

**souphttpsrc configuration:**

| Parameter | Value | Rationale |
|-----------|-------|-----------|
| `timeout` | `30` (seconds) | HTTP connection timeout. If no data is received for 30 seconds, `souphttpsrc` reports an error. Matches Tor circuit build timeout. |
| `retry-count` | `3` | Number of retry attempts on connection failure. Retries use the same proxy (Tor circuit). |
| `compress` | `false` | Disable HTTP compression. Video data is already compressed; HTTP compression wastes CPU. |
| `iradio-mode` | `false` | Disable internet radio mode. Not applicable to boGDan. |

**v4l2h264dec configuration:**

| Parameter | Value | Rationale |
|-----------|-------|-----------|
| `io-mode` | `dmabuf` | Zero-copy mode. Decoder outputs DMA-BUF file descriptors instead of mapped memory. `kmssink` imports these directly without a CPU copy. |
| `capture-io-mode` | `dmabuf` | Capture (output) side also uses DMA-BUF. Both sides of the V4L2 M2M operation use zero-copy. |
| `extra-controls` | `video,horiz-speed=1` | Hint to the decoder for real-time playback (not transcoding). Optional; may improve latency on some firmware versions. |

### 8.3 ABR Controller Thresholds

The ABR (Adaptive Bitrate) controller monitors the `queue2` buffer fill percentage and makes quality decisions based on the following thresholds:

| Buffer Range | State | Action | Rationale |
|-------------|-------|--------|-----------|
| **< 15%** | **Emergency** | Immediately switch to the lowest available bitrate representation. If already at lowest, pause and wait for rebuffering to 70%. | Buffer is critically low. Playback will stall within seconds if quality isn't reduced. Low-quality playback is preferable to stalling. |
| **15% – 70%** | **Stable** | Maintain current quality. No switches. | Buffer is in a healthy operating range. Switching quality during this range would cause unnecessary visual artifacts (resolution changes) without a clear need. |
| **> 70% for ≥ 30 seconds** | **Opportunity** | Switch to the next higher bitrate representation. If already at highest, no action. | Buffer has been consistently full for 30 seconds, indicating sufficient bandwidth for higher quality. The 30-second threshold prevents oscillation due to temporary bandwidth spikes. |
| **0 data received for 15 seconds** | **Critical** | Kill the current GStreamer pipeline. Attempt re-resolution of the URL via yt-dlp (new Tor circuit). If re-resolution fails twice, report `PLAYBACK_STALLED` error. | No data is flowing. This indicates a network failure (Tor circuit died, CDN rejected the connection, etc.), not a simple bandwidth issue. Re-establishing the connection is the only recovery path. |

**ABR ladder (HLS/DASH representations):**

boGDan does not control the ABR ladder—streaming providers define available representations. However, the controller's target quality is influenced by `maxResolution`:

| `maxResolution` | Target Bitrate | Preferred Representation |
|-----------------|---------------|------------------------|
| `360` | 500–800 Kbps | 360p H.264 |
| `480` | 800–1500 Kbps | 480p H.264 |
| `720` | 1500–3000 Kbps | 720p H.264 |
| `1080` | 3000–6000 Kbps | 1080p H.264 |

**HLS representation selection:**

`hlsdemux` in GStreamer automatically selects representations based on the `connection-speed` property. boGDan sets this property based on `maxResolution` and current buffer state:

```python
# Target speed in bits per second
target_speed = {
    360: 800000,
    480: 1500000,
    720: 3000000,
    1080: 6000000,
}[max_resolution]

hlsdemux.set_property("connection-speed", target_speed)
```

During emergency state, `connection-speed` is reduced to the minimum (500 Kbps) to force a downgrade. During opportunity state, it's increased to the next tier.

---

## 9. Deployment Specification

### 9.1 OS Requirements

| Component | Requirement | Version | Rationale |
|-----------|-------------|---------|-----------|
| **Operating System** | Raspberry Pi OS Lite 64-bit | Bookworm (Debian 12) | Minimal headless OS. 64-bit required for >2 GB address space and modern Rust toolchain. Bookworm provides recent GStreamer (1.22) and Linux kernel (6.6 LTS). |
| **Linux Kernel** | LTS | 6.6+ | Required for V4L2 M2M H.264 decode, DRM/KMS atomic commits, and DMA-BUF heap support. Kernel 6.6 is the current LTS branch with long-term maintenance. |
| **GStreamer** | Core + Plugins | 1.22+ | Provides `v4l2h264dec`, `kmssink`, `hlsdemux`, `dashdemux`, `souphttpsrc` with SOCKS5 proxy, `subtitleoverlay`. GStreamer 1.22 is the minimum for stable V4L2 M2M on Pi 4. |
| **yt-dlp** | Python package | Latest (pinned to commit hash in deployment) | URL resolution for web pages. Updated frequently (weekly releases) to handle site changes. Must be updated regularly via `pip install -U yt-dlp`. |
| **Tor** | C daemon | 0.4.8+ | Required for `IsolateSOCKSAuth` support. Tor 0.4.8+ has improved circuit isolation and congestion control. |
| **gmediarender** | DLNA renderer | 0.0.7+ | DLNA MediaRenderer. Version 0.0.7+ supports custom GStreamer pipeline strings via command-line arguments. |
| **Rust** | Compiler | 1.75+ | boGDan daemon is written in Rust. 1.75+ provides async closures, `impl Trait` in return position, and stable `tokio` 1.x compatibility. |
| **Python** | Runtime | 3.11+ | Required by yt-dlp. Python 3.11+ provides significant performance improvements (10–60% faster than 3.10). |
| **dnsmasq** | DNS forwarder | 2.90+ | DNS leak prevention (see §7.2). Version 2.90+ supports `address=/#/` catch-all syntax. |

**Installation method:** boGDan is distributed as a Debian package (`bogdan_1.0.0_arm64.deb`) with dependencies on `gstreamer1.0-plugins-base`, `gstreamer1.0-plugins-good`, `gstreamer1.0-plugins-bad`, `gstreamer1.0-libav`, `tor`, `gmediarender`, `dnsmasq`, `python3-pip`, and `yt-dlp`.

### 9.2 systemd Service Unit

```ini
# /etc/systemd/system/bogdan.service
[Unit]
Description=boGDan — Privacy-First Media Casting Daemon
Documentation=https://github.com/bogdan/bogdan/blob/main/SPECIFICATION.md
After=network-online.target tor.service gmediarender.service dnsmasq.service
Wants=network-online.target tor.service
Requires=gmediarender.service

# Wait for Tor to be ready (SOCKS port listening)
# bogdand will retry Tor connection internally, but this
# ordering prevents early startup failures.
StartLimitIntervalSec=60
StartLimitBurst=3

[Service]
Type=notify
NotifyAccess=all

# Run as dedicated user
User=bogdan
SupplementaryGroups=video render audio debian-tor

# Working directory
WorkingDirectory=/var/lib/bogdan
RuntimeDirectory=bogdan
StateDirectory=bogdan
LogsDirectory=bogdan

# Executable
ExecStart=/usr/bin/bogdand --config /etc/bogdan/bogdan.conf
ExecStartPre=/usr/bin/bogdan-prepare   # Ensure /dev/dri permissions, create temp dirs
ExecStopPost=/usr/bin/bogdan-cleanup   # Kill orphaned yt-dlp processes, clear temp files

# Restart policy
Restart=on-failure
RestartSec=5

# Watchdog — bogdand must send NOTIFY_WATCHDOG every 30 seconds
# If it fails, systemd restarts the service
WatchdogSec=30

# Resource limits
LimitNOFILE=4096
MemoryMax=512M
CPUWeight=80

# Security hardening
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
NoNewPrivileges=true
ReadWritePaths=/var/lib/bogdan /tmp/bogdan /var/log/bogdan
BindReadOnlyPaths=/etc/bogdan /etc/tor/torrc

# Capability restrictions
CapabilityBoundingSet=CAP_SYS_PTRACE
AmbientCapabilities=

# Device access — required for DRM/KMS and V4L2
DeviceAllow=/dev/dri/card0 rw
DeviceAllow=/dev/dri/renderD128 rw
DeviceAllow=/dev/vchiq rw
DevicePolicy=closed

[Install]
WantedBy=multi-user.target
```

**User and group setup:**

```bash
# Create bogdan user with no login shell
useradd --system --home-dir /var/lib/bogdan --shell /usr/sbin/nologin bogdan

# Group memberships:
# video   — /dev/dri/card0, /dev/dri/renderD128 (DRM/KMS)
# render  — GPU rendering (V3D)
# audio   — ALSA/PulseAudio for volume control
# debian-tor — Read access to Tor SOCKS port
usermod -aG video,render,audio,debian-tor bogdan
```

**Udev rules for DRM/KMS access:**

```
# /etc/udev/rules.d/99-bogdan-drm.rules
# Allow bogdan group access to DRI devices
SUBSYSTEM=="drm", GROUP="video", MODE="0660"
SUBSYSTEM=="drm", KERNEL=="renderD*", GROUP="render", MODE="0660"
```

### 9.3 Resource Requirements

| Resource | Minimum | Recommended | Notes |
|----------|---------|-------------|-------|
| **RAM** | 1 GB | 4 GB | 1 GB sufficient for 720p playback (GStreamer ~50 MB, Tor ~30 MB, yt-dlp ~80 MB during resolution, OS ~300 MB, buffer ~50 MB). 4 GB recommended for headroom and larger buffers. |
| **Storage** | 8 GB SD | 16 GB SD (Class 10 / A2) | boGDan binary ~15 MB, dependencies ~500 MB, OS ~1.5 GB, yt-dlp cache ~200 MB, buffer spill ~500 MB. 8 GB is tight; 16 GB provides comfortable headroom. |
| **Network** | Ethernet (100 Mbps) | Ethernet (1 Gbps) | Ethernet strongly recommended over Wi-Fi. Wi-Fi latency and jitter are incompatible with Tor's already-variable latency. 100 Mbps is sufficient (Tor maxes out at ~5 Mbps). |
| **Power** | 5V / 3A (USB-C) | 5V / 3A (official Pi 4 PSU) | Undervoltage causes CPU throttling and HDMI signal drops. Official PSU recommended. No PoE HAT required. |
| **Display** | Any HDMI display | 1080p HDMI display | boGDan sets the display to the video's native resolution (or the display's preferred mode). Any HDMI display is compatible. CEC is not supported in v1. |
| **CPU** | Raspberry Pi 4 (any RAM variant) | Raspberry Pi 4 (4 GB or 8 GB) | The VideoCore VI GPU handles H.264 decode. CPU is used for GStreamer pipeline management, HTTP/WebSocket server, and yt-dlp subprocess management. All Pi 4 variants have the same CPU and GPU. |
| **Thermal** | Passive cooling (stock) | Heatsink + fan | Sustained 1080p60 decode + Tor encryption can push CPU to 70°C with passive cooling. Active cooling recommended for long sessions. |

---

## 10. Open Decisions (v2)

These decisions are not resolved in v1 and are tracked for v2 consideration. Each has an OPEN status and a set of known options under evaluation.

---

### OD-001: HEVC SAND-to-NV12 Conversion Path

**Status:** ![OPEN](https://img.shields.io/badge/Status-OPEN-blue)

**Problem**

The Pi 4's hardware HEVC decoder outputs frames in SAND (Self-Describing Associative Network Data) format, which the HVS (Hardware Video Scaler) cannot directly display. A conversion from SAND to NV12 (or another linear format) is required before `kmssink` can import the frame. This conversion breaks zero-copy if it involves a CPU or GPU copy.

**Options under evaluation:**

| # | Approach | Description | Pros | Cons | Maturity |
|---|----------|-------------|------|------|----------|
| 1 | **CPU NEON conversion** | ARM NEON SIMD instructions perform SAND→NV12 conversion on the CPU. Proven at ~30 fps for 4K on Pi 4. | Proven. No GPU involvement. Pure software—predictable behavior. | Breaks zero-copy (CPU reads SAND, writes NV12). ~2 GB/s memory bandwidth for 4Kp60. CPU load ~15-20%. Not power-efficient. | **Proven** — implemented in Raspberry Pi firmware tools and upstream FFmpeg patches. |
| 2 | **V3D GPU compute shader** | The Pi 4's V3D GPU has a compute shader unit (QPU). A compute shader could perform SAND→NV12 conversion on the GPU, writing NV12 output to a DMA-BUF that `kmssink` imports. | Potentially zero-copy from GPU perspective. Offloads CPU. More power-efficient. | V3D compute is undocumented and unproven for this workload. No public implementation. May not have sufficient QPU throughput for 4Kp60. | **Unproven** — no known implementation. Requires significant R&D. |
| 3 | **Kernel driver conversion** | A kernel DRM driver could transparently perform SAND→NV12 conversion when a client imports a SAND DMA-BUF. The conversion would be invisible to userspace. | Transparent to GStreamer. No application changes needed. Clean abstraction. | Requires kernel module development. Must handle synchronization, memory management, and DMA-BUF lifetime. Complex kernel code with security implications. | **Conceptual** — no implementation. Would require upstream kernel review. |
| 4 | **GStreamer 1.26 element** | GStreamer 1.26 may include a `sandconvert` or similar element that handles SAND→NV12 within the pipeline, potentially using V4L2 M2M for conversion. | Within GStreamer pipeline—no kernel changes. Uses existing infrastructure. | GStreamer 1.26 release timeline uncertain. May not support Pi 4's specific SAND variant. Performance unknown. | **Pending** — GStreamer 1.26 is in development. |

**Recommendation:** Wait for GStreamer 1.26 (Option 4). If it doesn't provide a solution, implement CPU NEON conversion (Option 1) as a proven fallback. Explore V3D compute (Option 2) as a research project.

---

### OD-002: Matter Casting Protocol

**Status:** ![OPEN](https://img.shields.io/badge/Status-OPEN-blue)

**Problem**

Matter is an emerging open standard for IoT device interoperability, backed by Amazon, Apple, Google, and the CSA (Connectivity Standards Alliance). Matter includes a media casting protocol that could provide a standardized, royalty-free alternative to Google's Cast V2 protocol. If Matter casting gains adoption, boGDan could support it natively without relying on reverse-engineered protocols.

**Current state:**

- Matter 1.0 (2022) focused on lighting, thermostats, locks, and sensors. No media casting.
- Matter 1.2 (2023) added refrigerators, washing machines, robot vacuums. Still no media casting.
- Matter 1.3 (2024) added energy management and water leak detectors. No media casting.
- Amazon has implemented Matter casting on Fire TV and Echo Show devices (2024), suggesting the specification exists but is not yet public.
- No open-source Matter casting implementation exists for Linux.

**Options:**

| # | Approach | Description | Risk |
|---|----------|-------------|------|
| 1 | **Wait and adopt** | Monitor Matter specification development. When media casting is publicly specified, implement a Matter casting receiver for boGDan. | Medium — Matter casting may be limited to certified devices (like Cast V2 auth), reducing boGDan's ability to implement it. |
| 2 | **Early implementation via Amazon's approach** | Reverse-engineer Amazon's Matter casting implementation on Fire TV. Build an open-source Matter casting receiver. | High — Amazon's implementation may change before the specification is finalized. Maintenance burden similar to Cast V2. |
| 3 | **Skip Matter** | Continue with UPnP/DLNA + custom extension. Matter casting doesn't add functionality that DLNA doesn't already provide. | Low — if Matter casting becomes the dominant protocol, boGDan may lose interoperability. |

**Recommendation:** Wait (Option 1). Re-evaluate when the Matter media casting specification is publicly available and at least one open-source implementation exists.

---

### OD-003: arti Migration

**Status:** ![OPEN](https://img.shields.io/badge/Status-OPEN-blue)

**Problem**

arti is the Tor Project's Rust-based client, intended as the successor to the C Tor daemon. arti can run in-process (as a Rust crate), eliminating the IPC overhead of the C Tor daemon. arti is tokio-native, meaning it integrates directly with boGDan's async runtime without blocking threads on SOCKS5 I/O. However, arti currently lacks `IsolateSOCKSAuth` or an equivalent per-username circuit isolation mechanism, which is a hard requirement for boGDan's privacy model.

**Current state:**

- arti 1.2.0 (2024) supports HTTP CONNECT proxying and basic SOCKS5.
- arti's `StreamIsolation` trait exists but does not map SOCKS5 usernames to separate circuits.
- The Tor Project has acknowledged the need for fine-grained isolation but has not committed to a timeline.
- arti's SOCKS5 implementation does not support username-based isolation in any released version.

**Migration path (when arti adds isolation):**

```rust
// Conceptual: arti in-process with per-domain isolation
let tor_client = TorClient::builder()
    .stream_isolation(StreamIsolation::from_socks_username)
    .build()
    .await?;

// Per-domain isolated stream
let isolator = DomainIsolator::new("youtube.com");
let stream = tor_client.connect(isolator, target_addr).await?;
```

**Benefits of migration:**

| Benefit | Impact |
|---------|--------|
| In-process (no separate `tor` daemon) | -30 MB RAM. No IPC. No process management. |
| tokio-native | Direct integration with boGDan's async runtime. No blocking SOCKS5 I/O. |
| Single binary | No dependency on `tor.service`. Simpler deployment. |
| Rust memory safety | No C memory corruption bugs in the Tor client. |

**Recommendation:** Track arti releases for `IsolateSOCKSAuth` equivalent support. When available, create a feature flag (`arti-tor`) in boGDan for A/B testing. Migrate when arti's isolation is production-proven (6+ months of stable releases with isolation).

---

### OD-004: LAN Authentication

**Status:** ![OPEN](https://img.shields.io/badge/Status-OPEN-blue)

**Problem**

v1 assumes a trusted LAN—any device on the same network can control boGDan via HTTP, WebSocket, or DLNA. This is acceptable for home networks but insufficient for shared networks (dormitories, co-working spaces, conferences) where an attacker could inject cast commands, change volume, or stop playback.

**Proposed v2 solution: Pre-shared key via QR code pairing**

1. **Pairing flow:**
   - boGDan displays a QR code on the HDMI output containing: `bogdan://<pi-ip>:8585?key=<base64-psk>#<timestamp>`.
   - User scans the QR code with the boGDan browser extension (or a companion mobile app).
   - The extension extracts the pre-shared key (PSK) and stores it in `chrome.storage.local`.
   - All subsequent API requests include an `Authorization` header: `Authorization: boGDan <HMAC>`.

2. **HMAC construction:**

   ```
   HMAC-SHA256(
     key = <PSK>,
     message = <HTTP method> + "\n" +
               <URL path> + "\n" +
               <timestamp> + "\n" +
               SHA-256(<request body>)
   )
   ```

   The `Authorization` header format:

   ```
   Authorization: boGDan <timestamp>:<HMAC-hex>
   ```

3. **Validation:**
   - boGDan validates the timestamp (must be within ±60 seconds of server time to prevent replay attacks).
   - boGDan recomputes the HMAC and compares it with the provided value (constant-time comparison to prevent timing attacks).
   - Requests with invalid or missing `Authorization` headers receive `401 Unauthorized`.

4. **Key rotation:**
   - PSK is valid for 30 days. After expiration, a new QR code is displayed.
   - User can manually revoke a key via SSH: `bogdanctl revoke-key <key-id>`.
   - Maximum 5 paired devices per boGDan instance.

**DLNA compatibility:** UPnP/DLNA does not support authentication. In authenticated mode, DLNA control is disabled (gmediarender is stopped). Users must use the boGDan extension or API.

**Recommendation:** Implement in v2. The QR code pairing flow is user-friendly and doesn't require a centralized account system. HMAC-based auth is simple, stateless, and doesn't require TLS.

---

### OD-005: MSE Segment Proxy

**Status:** ![OPEN](https://img.shields.io/badge/Status-OPEN-blue)

**Problem**

Some websites (notably YouTube's web player) use Media Source Extensions (MSE) to construct video streams in the browser. Instead of providing a direct HLS/DASH manifest URL, the website's JavaScript fetches individual segments (`.m4s` files) and appends them to a `SourceBuffer` via `appendBuffer()`. The boGDan browser extension's `webRequest` interceptor can see these segments, but they're useless individually—they must be reassembled into a coherent stream.

**Proposed approach: Local HTTP proxy on the sender device**

1. **Content script injection:** The boGDan extension injects a content script into the media page. This script hooks `SourceBuffer.prototype.appendBuffer()` to capture segment data and metadata (init segments, codec info, timestamp offsets).

2. **Local HTTP server:** The extension starts a lightweight HTTP server on the sender device (e.g., `http://localhost:8587`) that re-serves captured segments in sequence.

3. **Casting flow:**
   - Extension sends a `CAST` message to boGDan with a special URL: `http://<sender-ip>:8587/stream`.
   - boGDan's `souphttpsrc` fetches from this local HTTP server.
   - The local server serves an HLS-like manifest + segments synthesized from the captured MSE data.
   - boGDan plays the synthesized stream like any other HLS source.

4. **Segment timing:** The content script tracks `updateend` events on the `SourceBuffer` to know when each segment is complete. It signals the local server to make the next segment available for fetching by boGDan.

**Challenges:**

| Challenge | Severity | Description |
|-----------|----------|-------------|
| Content script complexity | High | Hooking MSE APIs reliably across sites requires per-site content scripts or a general MSE interceptor. YouTube's player code changes frequently. |
| Local HTTP server | Medium | Running an HTTP server in a browser extension requires `chrome.sockets.tcp` API (available in Chrome apps, not standard extensions). May require a native messaging host (companion native app). |
| Segment timing | High | Segments must be served in order with correct timing. If the sender's browser buffers ahead, boGDan could receive segments faster than real-time, causing buffer overflow. |
| DRM blocking | Critical | Most MSE-based players use Encrypted Media Extensions (EME) for DRM. Encrypted segments are useless to boGDan without the decryption keys. This approach is fundamentally incompatible with DRM content. |
| Network topology | Medium | boGDan must be able to reach the sender device's HTTP server. On some networks, client isolation prevents device-to-device communication. |

**Recommendation:** Defer. The MSE segment proxy is complex, fragile, and blocked by DRM. It provides value only for non-DRM sites that use MSE without EME—a small and shrinking set. Re-evaluate if a simpler approach emerges (e.g., `chrome.debugger` API to extract media URLs from the page's network stack, or a WebRTC-based approach for peer-to-peer media transfer).

---

*End of boGDan Technical Specification v1.0*
