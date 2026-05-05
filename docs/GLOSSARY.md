# PiCast Technical Glossary

> Reference definitions for terms used throughout PiCast documentation, ADRs, and source code.

---

## A

### ABR (Adaptive Bitrate)

A streaming technique where the client dynamically switches between different quality levels (bitrate ladders) based on available network bandwidth and buffer state. In PiCast, ABR is implemented via GStreamer's `queue2` element, which emits buffering percentage signals. When buffer fill drops below a threshold, PiCast requests a lower-quality stream; when buffer is healthy, it requests higher quality. This is critical for Tor-routed playback because Tor circuit bandwidth is unpredictable (0.5–5 Mbps) and can vary within a single session.

---

## B

### BCM2711

The System-on-Chip (SoC) at the heart of the Raspberry Pi 4 Model B. Manufactured by Broadcom, it features:

- **CPU**: 4× Cortex-A72 (ARM v8-A) at 1.5 GHz
- **GPU**: VideoCore VI (V3D 4.2) with OpenGL ES 3.2, Vulkan 1.0 support
- **Video decode**: Dedicated H.264 (1080p60) and HEVC (4Kp60) hardware decoder blocks exposed via V4L2 M2M
- **Display**: HVS (Hardware Video Scaler) with dual HDMI output
- **Memory**: 1, 2, 4, or 8 GB LPDDR4-3200

BCM2711's video pipeline is the foundation of PiCast's zero-copy architecture: the V4L2 decoder outputs DMA-BUFs that the HVS scans directly, avoiding all CPU copies.

---

## C

### CRTC (Cathode Ray Tube Controller)

In DRM/KMS terminology, the CRTC is the hardware block that reads pixel data from a framebuffer and sends it to a connector (HDMI, DSI, etc.) for display. Despite the anachronistic name, CRTCs are used in all modern display controllers. On BCM2711, each HDMI output has its own CRTC. PiCast programs CRTC 0 via atomic modesetting to scan out from the GStreamer `kmssink` framebuffer on HDMI-1.

---

## D

### DLNA (Digital Living Network Alliance)

An industry standard for sharing media content between devices on a home network. DLNA is built on top of UPnP and defines device profiles (MediaServer, MediaRenderer, MediaController). PiCast implements the **MediaRenderer** profile via gmediarender, allowing DLNA controllers (VLC, BubbleUPnP, Home Assistant) to discover PiCast and send media URLs for playback. DLNA only supports direct URL playback — it cannot execute JavaScript or render web pages.

### DMA-BUF

A Linux kernel framework for sharing buffers between subsystems without copying. A DMA-BUF is a file descriptor that represents a buffer in device memory (GPU, V4L2, etc.) that can be passed between processes and kernel subsystems. In PiCast, the V4L2 hardware decoder produces DMA-BUFs containing decoded video frames; GStreamer's `kmssink` imports these DMA-BUFs into DRM planes for display. This is the mechanism that enables zero-copy playback — the decoded frame is never copied to CPU-accessible system memory.

### DRM/KMS (Direct Rendering Manager / Kernel Mode Setting)

The Linux kernel subsystem for managing GPUs and displays. DRM provides the low-level interface to graphics hardware; KMS is the DRM sub-API for configuring display outputs (connectors, CRTCs, planes, framebuffers). PiCast uses DRM/KMS directly (no display server) via `libdrm` Rust bindings:

1. Open `/dev/dri/card0` and acquire DRM master
2. Discover connected HDMI connector and preferred video mode
3. Create DRM framebuffer backed by a DMA-BUF
4. Program CRTC via atomic modesetting (`drmModeAtomicCommit()`) to scan out the framebuffer

DRM/KMS atomic modesetting allows PiCast to guarantee tear-free, vsync-aligned frame presentation on the HDMI output.

---

## G

### GBM (Generic Buffer Manager)

A library (libgbm) that allocates buffers for DRM/KMS rendering. GBM provides a platform-agnostic API for creating buffer objects that can be used as DRM framebuffers and shared as DMA-BUFs. While PiCast primarily uses GStreamer's `kmssink` (which handles buffer allocation internally), GBM is used by Mesa's Vulkan and OpenGL drivers and may be needed for future GPU-accelerated OSD or SAND→NV12 conversion.

---

## H

### H.264 / AVC (Advanced Video Coding)

The most widely deployed video compression standard. Defined in ITU-T H.264 / MPEG-4 Part 10. H.264 is the primary codec for PiCast v1 because:

- BCM2711 has a dedicated H.264 hardware decoder (`v4l2h264dec`) supporting 1080p60
- H.264 output is in NV12 format, which the HVS can scan directly (no format conversion needed)
- Most web video is available in H.264, including YouTube, Vimeo, and most CDNs

PiCast forces H.264 selection in yt-dlp via the format string `vcodec^=avc1`.

### HEVC / H.265 (High Efficiency Video Coding)

A video compression standard offering ~40% bitrate savings over H.264 at equivalent quality. BCM2711 has an HEVC hardware decoder (`v4l2h265dec`) supporting 4Kp60, but the decoder outputs frames in SAND format (NC12/NC30), which the HVS cannot display directly. HEVC is deferred to PiCast v2 pending a production-ready SAND→NV12 conversion path. See ADR-009.

### HLS (HTTP Live Streaming)

An adaptive streaming protocol developed by Apple. Media is encoded at multiple quality levels, segmented into small files (typically 6–10 seconds each), and described by a playlist file (`.m3u8`). GStreamer's `hlsdemux` element handles master playlist parsing, variant selection, and segment fetching. HLS is fully compatible with Tor SOCKS5 routing via `souphttpsrc` proxy configuration.

### HVS (Hardware Video Scaler)

The display engine on BCM2711 responsible for compositing and scanning out pixels to HDMI outputs. The HVS supports:

- Multiple planes (layers) per CRTC for compositing
- Scaling, color space conversion, and alpha blending
- Direct scanout from DMA-BUFs (zero-copy path)

In PiCast, the HVS is programmed via DRM atomic modesetting to scan out decoded video frames from plane 0. The HVS supports NV12, NV21, YUYV, UYVY, and RGB pixel formats — but **not** SAND format (NC12/NC30), which is the output format of the HEVC decoder.

---

## I

### IsolateSOCKSAuth

A Tor SOCKS port flag that maps different SOCKS5 username/password combinations to separate Tor circuits. When `IsolateSOCKSAuth` is enabled on the SOCKS port, connections with different SOCKS5 usernames use different circuits, preventing cross-site correlation. PiCast uses this feature by setting the SOCKS5 username to a hash of the destination hostname, ensuring that traffic to `youtube.com` and `example.com` uses independent circuits with different exit nodes. This feature is available in the C Tor daemon but **not** in arti (the Rust Tor client), which is why PiCast uses the C daemon (see ADR-004).

---

## K

### kmssink

A GStreamer sink element that renders video frames directly to a DRM/KMS plane via atomic modesetting. `kmssink` imports DMA-BUFs from upstream elements (like `v4l2h264dec`) into DRM framebuffers and programs the CRTC for display. It is the final element in PiCast's zero-copy pipeline: `v4l2h264dec → kmssink`. Key properties used by PiCast:

- `plane-id=0`: Use DRM plane 0 for fullscreen video
- `can-attach-static=true`: Allow static plane attachment for zero-copy DMA-BUF import
- `connector-id`: Specify which HDMI output to use

---

## N

### NV12

A bi-planar YUV 4:2:0 pixel format used for video frames. NV12 stores the Y (luma) plane contiguously, followed by an interleaved UV (chroma) plane. NV12 is the output format of BCM2711's H.264 hardware decoder and is directly supported by the HVS for scanout. In contrast, the HEVC decoder outputs SAND format (NC12), which requires conversion to NV12 before the HVS can display it. NV12 is sometimes called "linear NV12" or "raster NV12" to distinguish it from tiled formats like SAND.

---

## S

### SAND Format

A proprietary Broadcom pixel tiling format used by BCM2711's HEVC hardware decoder. SAND (also called "band format" or "column format") arranges pixels in column-based bands rather than standard raster scan order. Specific SAND variants on Pi 4:

- **NC12**: 8-bit SAND format (NV12 equivalent in SAND layout)
- **NC30**: 10-bit SAND format (P010 equivalent in SAND layout)

The HVS cannot scan out SAND format directly — it requires conversion to a linear format (NV12). This is the fundamental blocker for HEVC hardware decoding on Pi 4, as the conversion adds CPU overhead or requires a GPU compute shader that is not yet production-ready. See ADR-009.

### SOCKS5

An Internet protocol for routing network traffic through a proxy server. SOCKS5 supports TCP connections, UDP association, and authentication (username/password). In PiCast, GStreamer's `souphttpsrc` and yt-dlp connect through a Tor SOCKS5 proxy at `127.0.0.1:9050`. The SOCKS5 username field is used for Tor's `IsolateSOCKSAuth` circuit isolation — each hostname gets a unique username, ensuring separate Tor circuits.

### SSDP (Simple Service Discovery Protocol)

The discovery protocol used by UPnP/DLNA. Devices announce their presence on the local network by sending UDP multicast messages to `239.255.255.250:1900`. Controllers discover devices by sending M-SEARCH queries. PiCast's gmediarender process responds to SSDP queries, making PiCast discoverable by VLC, Home Assistant, and DLNA controller apps.

---

## U

### UPnP (Universal Plug and Play)

A set of networking protocols for device discovery and control on local networks. UPnP provides:

- **SSDP**: Device discovery via UDP multicast
- **SOAP**: Control protocol for invoking actions on devices
- **GENA**: Event notification for state changes

PiCast uses UPnP via gmediarender to implement the DLNA MediaRenderer profile. The AVTransport service handles play/pause/stop/seek, and the RenderingControl service handles volume/mute.

---

## V

### V3D

The GPU (Graphics Processing Unit) on BCM2711, also known as VideoCore VI. V3D 4.2 supports:

- OpenGL ES 3.2
- Vulkan 1.0
- Compute shaders (potentially useful for SAND→NV12 conversion in PiCast v2)

V3D is distinct from the V4L2 video decode blocks — V3D handles 3D rendering and compute, while the dedicated decode blocks handle H.264 and HEVC decompression.

### V4L2 M2M (Video4Linux2 Memory-to-Memory)

A Linux kernel API for hardware-accelerated video codec operations. V4L2 M2M provides a memory-to-memory model: compressed data goes in (OUTPUT queue), decoded frames come out (CAPTURE queue). On BCM2711:

- `/dev/video11`: H.264 decoder (`v4l2h264dec` in GStreamer)
- `/dev/video19`: HEVC decoder (`v4l2h265dec` in GStreamer)

The CAPTURE queue can output decoded frames as DMA-BUFs (when `capture-io-mode=dmabuf` is set), enabling zero-copy handoff to `kmssink` for display. V4L2 M2M is the standard Linux interface for hardware video codecs and is supported by GStreamer, FFmpeg, and VLC.

---

## Y

### yt-dlp

A command-line tool for extracting direct media stream URLs from web pages. A community-maintained fork of youtube-dl, yt-dlp supports 1800+ sites and is actively updated to handle site changes. PiCast uses yt-dlp as a subprocess (see ADR-008) to resolve page URLs (e.g., `https://youtube.com/watch?v=...`) into direct media URLs that GStreamer can play. Key features used:

- `--dump-json`: Output stream metadata as JSON
- `--proxy socks5://...`: Route through Tor SOCKS5
- `--username <hash>`: SOCKS5 username for IsolateSOCKSAuth circuit isolation
- `--format "bv[height<=1080][vcodec^=avc1]+ba"`: Force H.264 ≤1080p

---

## Z

### Zero-Copy

A data path optimization where pixel data is never copied between memory regions or across process boundaries. In PiCast's zero-copy pipeline:

1. V4L2 hardware decoder writes decoded H.264 frames into device memory, exposed as DMA-BUF file descriptors
2. GStreamer's `kmssink` imports the DMA-BUFs into DRM planes without copying
3. HVS scans out directly from the DMA-BUF-backed framebuffer to HDMI

The CPU never touches decoded video frames. This eliminates the most expensive operation in software video playback (memory copies of multi-megabyte frames at 30–60 fps) and keeps the ARM cores free for Tor encryption, yt-dlp resolution, and system tasks. The zero-copy path requires:
- V4L2 decoder `capture-io-mode=dmabuf`
- Output format compatible with HVS (NV12, not SAND)
- `kmssink` with DMA-BUF import support

---

### atomic modesetting

A DRM/KMS API that allows multiple display properties (CRTC mode, plane framebuffer, connector assignment) to be committed atomically — either all changes apply simultaneously or none do. Atomic modesetting prevents visual artifacts (tearing, partial updates) that can occur with the legacy `drmModeSetCrtc()` API when properties are set individually. PiCast uses atomic modesetting exclusively via `drmModeAtomicCommit()` for all display configuration, including initial setup, resolution changes, and plane updates.

---

*For ADR-specific terminology, refer to the individual ADR files in `docs/decisions/`.*
