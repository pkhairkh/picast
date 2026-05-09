# boGDan — Architecture Decision Records

> This file consolidates all key architectural decisions. Each ADR follows the
> Context → Decision → Consequences → Alternatives format with a status badge.

---

## ADR-001: No Display Server

**Status: `ACCEPTED`** — Use DRM/KMS directly, no X11/Wayland/openbox

### Context

A display server (X11, Wayland) provides window management, input handling, and compositor services. These are useful for desktop environments but add overhead, complexity, and attack surface. boGDan has a single fullscreen video output and no local input devices.

### Decision

boGDan opens `/dev/dri/card0` directly, calls `drmSetMaster()` to become the DRM master, and programs the HVS planes via `drmModeAtomicCommit()`. No display server process runs on the system. This is the same approach used by Kodi on LibreELEC.

### Consequences

| Effect | Detail |
|--------|--------|
| ✅ RAM savings | ~50-100 MB freed (no X11/Wayland process) |
| ✅ Latency | Zero compositor scheduling overhead |
| ✅ Security | Reduced attack surface (no X11 IPC, no Wayland protocol) |
| ✅ Power | Lower CPU/GPU utilization |
| ❌ No GUI apps | Cannot run terminal, file manager alongside boGDan |
| ❌ Debugging | Requires SSH; no on-screen terminal |
| ❌ Crash recovery | Display left in undefined state until service restarts |

### Alternatives Rejected

| Alternative | Why Rejected |
|-------------|-------------|
| X11 + openbox | 50MB+ RAM, compositor adds scheduling latency |
| Weston (Wayland) | Lighter than X11 but still unnecessary process |
| X11 + matchbox-window-manager | Minimalist but still adds IPC layer |

---

## ADR-002: No Chromium / No Browser Runtime

**Status: `ACCEPTED`** — No browser engine for content rendering or DRM playback

### Context

Chromium can render web pages with DRM (Widevine CDM), execute JavaScript for content resolution, and act as a universal media player. However, on Pi 4, Chromium consumes 300-500 MB RAM at idle, uses software video decode paths (hardware decode integration is unreliable), and adds a massive attack surface (multi-process sandbox, V8 JIT, GPU process, network service).

### Decision

boGDan does not use any browser engine. Content resolution is performed by yt-dlp (Python subprocess). Media playback uses GStreamer with V4L2 hardware decode. DRM content is out of scope for v1.

### Consequences

| Effect | Detail |
|--------|--------|
| ✅ RAM savings | ~400 MB freed vs Chromium kiosk |
| ✅ CPU savings | ~40% freed during playback |
| ✅ Security | No V8 JIT, no GPU process, no sandbox escape surface |
| ✅ Simplicity | No proprietary CDM blob dependency |
| ❌ No DRM | Cannot play Netflix, Disney+, Amazon Prime |
| ❌ JS-heavy sites | ~5-10% of sites needing JS beyond yt-dlp extractors |

### Alternatives Rejected

| Alternative | Why Rejected |
|-------------|-------------|
| Chromium kiosk | 300-500MB RAM, Widevine L3 on ARM slow/unreliable |
| deno_core embedded V8 | 30MB lighter but adds complexity, still can't handle DRM |
| Cog/WPE WebKit | Lighter than Chromium (~100MB) but no DRM on ARM without CDM |

---

## ADR-003: GStreamer Over mpv for Playback

**Status: `ACCEPTED`** — GStreamer pipeline with V4L2 M2M + kmssink

### Context

Both GStreamer and mpv can play video on Pi 4. mpv is simpler to configure and has a convenient CLI. GStreamer offers a pipeline-based architecture with fine-grained control over each element, native V4L2 M2M integration, and a DRM/KMS sink (kmssink) that enables zero-copy display.

### Decision

boGDan uses GStreamer as the playback engine with `v4l2h264dec` for hardware decode and `kmssink` for DRM/KMS direct display. mpv's `--vo=drm` backend does not support hardware-accelerated decode on Pi 4 (only software-decoded frames), and its `--hwdec=v4l2m2m` with `--vo=gpu` produces blue screen issues on Wayland and X11 alike.

### Consequences

| Effect | Detail |
|--------|--------|
| ✅ Zero-copy | DMA-BUF from decoder to HVS, no memory copy |
| ✅ Buffer control | `queue2` for Tor ABR with buffer-level callbacks |
| ✅ Subtitle overlay | `subtitleoverlay` element with timing sync |
| ✅ Adaptive streaming | `adaptivedemux2` for HLS/DASH |
| ❌ Learning curve | Pipeline construction more complex than mpv CLI |
| ❌ GStreamer bugs | `v4l2slh265dec` broken on Pi 4 for HEVC |

### Alternatives Rejected

| Alternative | Why Rejected |
|-------------|-------------|
| mpv `--vo=drm` | No HW decode support on Pi 4 |
| FFmpeg + custom DRM sink | Requires writing DRM display code, no buffer management |
| Kodi as library | Too heavy, includes entire media center UI |

---

## ADR-004: C Tor Daemon Over arti

**Status: `ACCEPTED`** — Use C Tor daemon (v0.4.8+) with IsolateSOCKSAuth

### Context

arti is the Tor Project's Rust-based Tor client, production-ready for HTTP CONNECT use cases. It offers in-process embedding, native async (tokio) integration, and memory safety. The C Tor daemon is a separate process, configured via torrc, and communicated with via SOCKS5.

### Decision

boGDan uses the C Tor daemon. The decisive factor is `IsolateSOCKSAuth` support: the C daemon's `SocksPort` directive with `IsolateSOCKSAuth` enables per-site circuit isolation based on SOCKS5 username, which is critical for boGDan's stream isolation strategy. arti does not yet support equivalent fine-grained isolation control.

### Consequences

| Effect | Detail |
|--------|--------|
| ✅ Stream isolation | `IsolateSOCKSAuth` for per-site circuits |
| ✅ Configuration | Extensive torrc options for streaming tuning |
| ✅ Proven | Same daemon used by Tor Browser |
| ❌ Separate process | Must manage daemon lifecycle |
| ❌ Memory | ~30 MB overhead for daemon process |
| ❌ Future direction | arti is the Tor Project's long-term direction |

### Alternatives Rejected

| Alternative | Why Rejected |
|-------------|-------------|
| arti | Lacks `IsolateSOCKSAuth` equivalent; streaming insufficiently tested |
| No Tor | Violates core security requirement |

---

## ADR-005: Cast V2 Protocol Rejected

**Status: `REJECTED`** — Do not implement Google Cast V2 receiver

### Context

The Google Cast V2 protocol would enable the Pi to appear in Chrome's native cast menu and Android's cast dialog, providing the most seamless user experience. The protocol has been reverse-engineered and implemented in open-source projects (node-castv2, pycast).

### Decision

boGDan does not implement the Cast V2 receiver. Google enforces device authentication in the Cast SDK: sender devices verify receiver certificates during the TLS handshake, and only Google-approved (certified) receivers pass authentication. No Pi-based project has reliably bypassed this across Chrome updates.

### Consequences

| Effect | Detail |
|--------|--------|
| ✅ Stability | No dependency on reverse-engineered protocol |
| ✅ Simplicity | No protobuf/TLS/auth implementation needed |
| ❌ UX | Pi does not appear in Chrome's native cast menu |
| ❌ Adoption | Users must install extension or use VLC/DLNA |

### Alternatives Rejected

| Alternative | Why Rejected |
|-------------|-------------|
| Cast V2 with auth bypass | Fragile, breaks on Chrome updates |
| Shanocast (Openscreen) | Works for tab mirroring, unreliable for media casting |
| DIAL-only implementation | Only handles discovery, not media control |

---

## ADR-006: UPnP/DLNA MediaRenderer for Interop

**Status: `ACCEPTED`** — Implement DLNA MediaRenderer via gmediarender

### Context

boGDan needs to interoperate with existing media ecosystems without requiring users to install custom software. The UPnP/DLNA MediaRenderer standard is the most widely supported protocol for network media playback.

### Decision

boGDan bundles gmediarender (gmrender-resurrect), a mature, lightweight DLNA MediaRenderer implementation that uses GStreamer as its playback backend. gmediarender is modified to use boGDan's custom GStreamer pipeline (V4L2 + kmssink) instead of the default playbin.

### Consequences

| Effect | Detail |
|--------|--------|
| ✅ VLC/HA/DLNA | Instant compatibility without custom sender software |
| ✅ Proven | 10+ years in production on Raspberry Pi hardware |
| ❌ URL limitation | Only directly fetchable URLs (no JS, no cookies) |
| ❌ SSDP | Discovery can be slow on some networks |
| ❌ Status | Limited real-time playback status via DLNA |

### Alternatives Rejected

| Alternative | Why Rejected |
|-------------|-------------|
| Custom DLNA implementation | Reinventing gmediarender |
| Rygel | Heavier, GNOME dependency |

---

## ADR-007: DRM Out of Scope

**Status: `ACCEPTED`** — No DRM/Widevine support in v1

### Context

DRM-protected content (Netflix, Disney+, Amazon Prime, Hulu) accounts for a significant portion of consumer media consumption. Widevine L3 CDM exists for ARM Linux but requires proprietary binary blobs and integration with a Chromium-based browser.

### Decision

DRM content is explicitly out of scope for boGDan v1. The primary use case is casting from open video platforms (YouTube, Vimeo, Twitch, PeerTube, Odysee, direct media URLs), none of which use DRM. Widevine L3 on ARM is software decryption, which is slow and unreliable on the Pi 4's CPU.

### Consequences

| Effect | Detail |
|--------|--------|
| ✅ No proprietary deps | No CDM blobs, cleaner build and distribution |
| ✅ Simpler | No browser engine integration needed |
| ✅ Security | Reduced attack surface |
| ❌ DRM content | Cannot cast from Netflix, Disney+, Amazon Prime |

---

## ADR-008: yt-dlp as Subprocess, Not Library

**Status: `ACCEPTED`** — Invoke yt-dlp as subprocess, not embedded Python

### Context

yt-dlp is a Python application. It can be used as a Python library (`import yt_dlp`) or invoked as a subprocess. Using it as a library would allow tighter integration, but at the cost of coupling the boGDan process to Python's runtime.

### Decision

boGDan invokes yt-dlp as a subprocess (`tokio::process::Command`) and parses its JSON output. This isolation ensures that yt-dlp's Python runtime cannot affect the boGDan main process. If yt-dlp hangs, it can be killed with a timeout without affecting the playback engine.

### Consequences

| Effect | Detail |
|--------|--------|
| ✅ Process isolation | yt-dlp crashes don't affect boGDan |
| ✅ Independent updates | Update yt-dlp without rebuilding boGDan |
| ✅ Simple errors | Exit code + stderr for error handling |
| ❌ Startup overhead | 5-15 second Python interpreter startup |
| ❌ JSON overhead | Serialization for large format lists |
| ❌ No progress hooks | Cannot get real-time resolution progress |

---

## ADR-009: HEVC Deferred to v2

**Status: `DEFERRED`** — HEVC hardware decode deferred; force H.264 format selection

### Context

The Pi 4 has a dedicated HEVC hardware decoder capable of 4Kp60 decode. However, the decoder outputs in Broadcom's proprietary SAND column format (NC12/NC30), which the vc4 HVS cannot display directly. A SAND-to-NV12 format conversion step is required, breaking the zero-copy pipeline.

### Decision

HEVC hardware decode is deferred to v2. In v1, yt-dlp's format selection string explicitly prefers H.264 (`vcodec^=avc1`), ensuring the zero-copy H.264 pipeline is always used. For HEVC-only content, a software decode fallback via GStreamer's `avdec_h265` provides playback at reduced resolution.

### Consequences

| Effect | Detail |
|--------|--------|
| ✅ Proven pipeline | Only the zero-copy H.264 path in v1 |
| ✅ Simpler testing | One decode pipeline to validate |
| ❌ 4K limitation | Some 4K content limited to 1080p H.264 |
| ❌ Unused HW | HEVC decoder sits idle in v1 |

### Re-evaluation Triggers

- GStreamer 1.26 ships with SAND format support
- V3D GPU compute shader SAND→NV12 conversion is proven
- `rpi-hevc-dec` driver adds kernel-level format conversion
