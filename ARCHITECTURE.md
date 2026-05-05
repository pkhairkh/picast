# PiCast Architecture Paper v1.0

**Tor-Routed Zero-Copy Media Casting Appliance for Raspberry Pi 4B+**

---

## 1. Executive Summary

PiCast is a networked media casting appliance purpose-built for the Raspberry Pi 4B+ that routes all content through the Tor network and delivers video to HDMI via a zero-copy DMA-BUF pipeline. The system uses the BCM2711 SoC's dedicated H.264 hardware decoder, outputs decoded frames directly to DRM/KMS display planes without copying through main memory, and avoids any display server, browser, or DRM stack entirely. The result is a machine that plays 1080p60 H.264 video at approximately 3% CPU utilization and 5 watts total system power — comparable to a commercial Chromecast device, but with full Tor network isolation and no proprietary dependencies.

PiCast accepts media from three independent interfaces: a browser extension (Chrome/Firefox Manifest V3) that intercepts or resolves page URLs, a UPnP/DLNA MediaRenderer compatible with VLC and Home Assistant, and a simple HTTP REST API. Content resolution is handled by yt-dlp, which supports over 1,800 websites and extracts direct media URLs through the Tor SOCKS proxy. The resolved URL is then fetched and decoded by a GStreamer pipeline using `v4l2h264dec` in DMA-BUF mode, and the resulting hardware buffer is imported directly by `kmssink` into a DRM plane for HDMI scanout by the Hardware Video Scaler (HVS). The CPU is entirely uninvolved in the display data path.

The architecture makes three deliberate trade-offs that define the project. First, operational security is non-negotiable: all outbound traffic traverses Tor, and DNS resolution is forced through the Tor network to prevent leakage. Second, hardware efficiency is achieved through zero-copy buffer sharing between the V4L2 decoder and the DRM compositor, eliminating an entire frame copy per display refresh. Third, software minimalism — no X11, no Wayland, no Chromium, no Widevine — produces a dramatically smaller attack surface than any Raspberry Pi desktop configuration. PiCast is an appliance, not a general-purpose computer, and every design decision flows from that premise.

---

## 2. System Overview

### 2.1 Design Philosophy

PiCast is designed as an appliance, not a desktop. This distinction is not merely semantic — it governs every architectural decision in the system. A desktop must support arbitrary applications, window management, user input events, and a general-purpose display server. An appliance has a single rendering client, a fixed display output, no interactive input on the device itself, and a fully deterministic startup sequence. By committing to the appliance model, PiCast eliminates entire categories of software: no display server, no window manager, no input method framework, no session manager, no D-Bus user session bus, no PolicyKit, and no desktop environment.

Three priorities structure every decision, listed in strict order:

1. **Operational Security (Tor):** Every byte that leaves the PiCast device traverses the Tor network. This is not an optional privacy feature — it is a fundamental security property. Without Tor, the user's ISP can observe every content choice, build a viewing history, and correlate it with other network activity. The Tor requirement constrains bandwidth (typically 2–4 Mbps effective throughput), which in turn constrains maximum viable resolution (720p reliable, 1080p unreliable over Tor).

2. **Hardware Efficiency (Zero-Copy):** The BCM2711 SoC provides dedicated hardware blocks for video decoding (H.264, HEVC) and display composition (HVS). PiCast uses these blocks in their native zero-copy mode: the V4L2 decoder outputs DMA-BUF file descriptors that the DRM/KMS subsystem imports directly into display planes. The CPU never touches a pixel of video data after decode. This is the difference between ~30% CPU (software copy path) and ~3% CPU (zero-copy path), and the difference between ~8W and ~5W power consumption.

3. **Software Minimalism (No Display Server):** Every daemon, library, and privilege boundary that runs on the device increases the attack surface. A Pi desktop running Chromium and X11 has a dramatically larger attack surface than PiCast's single-process GStreamer pipeline talking directly to DRM. The minimalism principle means PiCast does not support features that would require a display server (screen mirroring, interactive UI) or a browser (Widevine DRM, JavaScript-rendered players).

### 2.2 High-Level Data Flow

The PiCast data path is a fixed pipeline with no branches, no feedback loops, and no user-selectable alternatives during playback. This rigidity is intentional — it makes the system predictable, debuggable, and secure. The pipeline stages are:

```
Sender submits URL
       │
       ▼
Content Resolver (yt-dlp via Tor SOCKS5h)
       │
       ▼ resolved direct media URL
GStreamer Pipeline
   souphttpsrc ──▶ queue2 ──▶ h264parse ──▶ v4l2h264dec
       │                                          │
       │ Tor fetch                         NV12 DMA-BUF fd
       │                                    (zero-copy)
       │                                          │
       │                                          ▼
       │                                  kmssink (DRM Plane 0)
       │                                          │
       │                                          ▼
       │                                  HVS → HDMI scanout
       │
       ▼
   OSD Pipeline (parallel)
   V3D GPU → EGL/GBM → DRM Plane 1
       │
       ▼
   HVS composites Plane 0 + Plane 1 → HDMI
```

The sender (browser extension, UPnP controller, or HTTP client) submits a URL. The content resolver invokes yt-dlp through the Tor SOCKS proxy to extract a direct media URL and its format metadata. The resolved URL is passed to the GStreamer pipeline, which fetches the media through the same Tor proxy, parses the container, and feeds NALUs to the V4L2 H.264 hardware decoder. The decoder outputs decoded frames as NV12 DMA-BUF file descriptors — these are GPU-side memory buffers that the DRM/KMS subsystem can scan out directly. The `kmssink` element imports these DMA-BUF fds into DRM Plane 0, and the HVS reads from Plane 0 during each HDMI scanout cycle.

In parallel, the V3D GPU renders any on-screen display (subtitles, status indicators) into a separate GBM buffer on DRM Plane 1. The HVS performs hardware alpha-blending of Plane 0 (video) and Plane 1 (OSD) and outputs the composited result to HDMI. The CPU's role in this entire path is limited to feeding compressed NALUs to the decoder and handling GStreamer bus messages — it never copies, transforms, or touches decoded pixel data.

### 2.3 System Boundary

The following table defines what is in scope for PiCast v1.0 and what is explicitly out of scope, with rationale for each exclusion.

| Capability | In/Out | Rationale |
|---|---|---|
| Tor-routed content resolution | **In** | Core security property. All yt-dlp and media fetch traffic traverses Tor. |
| H.264 hardware decode (V4L2 M2M) | **In** | Primary decode path. bcm2835-codec driver, zero-copy DMA-BUF output. |
| DRM/KMS direct display | **In** | No display server. App is DRM master, programs HVS planes directly. |
| Zero-copy DMA-BUF pipeline | **In** | V4L2 decode → DRM plane import. No CPU frame copies. |
| UPnP/DLNA MediaRenderer | **In** | gmediarender-based, compatible with VLC, Home Assistant, DLNA apps. |
| Browser extension (Manifest V3) | **In** | URL interception + page URL submission for server-side resolution. |
| HTTP REST API | **In** | Simple cast/stop/pause/seek/status interface on port 8585. |
| WebSocket status channel | **In** | Real-time playback status push to connected senders on port 8586. |
| yt-dlp content resolution (1800+ sites) | **In** | Two-tier resolution: yt-dlp extraction + direct URL handling. |
| Subtitle support (VTT/SRT/embedded) | **In** | yt-dlp subtitle extraction + GStreamer subtitleoverlay + closedcaption. |
| Google Cast V2 protocol | **Out** | Requires Google authentication and registered receiver app. Unofficial receivers cannot complete the handshake. |
| DRM / Widevine | **Out** | Requires Chromium CDM, proprietary binary, and display server. Fundamentally incompatible with appliance model. |
| Screen mirroring | **Out** | Requires display server (X11/Wayland) to capture application windows. Not applicable to DRM-only rendering. |
| AirPlay video | **Out** | Requires FairPlay DRM for protected content. Protocol reverse-engineering effort not justified for v1. |
| HEVC/H.265 decode | **Out** (v1) | Hardware outputs SAND column format that HVS cannot display. Requires format conversion, breaking zero-copy. Deferred to v2. |
| AV1 decode | **Out** | No hardware AV1 decoder on BCM2711. Software decode would exceed CPU budget. |
| Interactive UI on Pi | **Out** | No input devices, no display server, no UI toolkit. Control is exclusively via network interfaces. |

---

## 3. Hardware Platform: BCM2711 SoC

The Raspberry Pi 4B+ is built around the Broadcom BCM2711 SoC, a quad-core Cortex-A72 (ARMv8-A) system-on-chip. For PiCast, the general-purpose CPU cores are almost irrelevant — the system's capabilities are defined by three dedicated hardware blocks: the video decoders, the Hardware Video Scaler (HVS), and the V3D GPU. Understanding these blocks and their interconnections is essential to understanding why PiCast's pipeline is designed the way it is.

### 3.1 Video Decode Blocks

The BCM2711 contains two entirely separate hardware video decoder blocks, each with its own driver stack, API surface, and output format. This separation is the single most important hardware constraint in the PiCast architecture.

#### 3.1.1 H.264 Decoder (Stateful V4L2 M2M)

The primary decode path for PiCast uses the BCM2711's dedicated H.264 decoder, exposed through the `bcm2835-codec` kernel driver as V4L2 memory-to-memory (M2M) devices at `/dev/video10` (decode), `/dev/video11` (encode), and `/dev/video12` (ISP). The decoder supports H.264 High Profile, Level 4.2, up to 1080p60.

The decoder uses the **stateful V4L2 API**: the application feeds compressed H.264 NALUs (Network Abstraction Layer Units) into the OUTPUT queue, and the driver internally manages all decoder state (sequence parameters, reference frame management, reordering). The application does not need to parse slice headers, manage reference pictures, or handle B-frame reordering — the hardware and driver handle all of this autonomously. This contrasts with the stateless API used by the HEVC decoder (see §3.1.2), where the application must parse and provide per-slice parameters.

Critically, the H.264 decoder's CAPTURE queue outputs decoded frames in **NV12 format** (4:2:0, two-plane: Y plane followed by interleaved UV plane) as **DMA-BUF file descriptors**. NV12 is the native scanout format of the HVS — the Hardware Video Scaler can read NV12 buffers directly from DMA-BUF without any format conversion. This is the hardware foundation of PiCast's zero-copy pipeline: the decoder writes NV12 pixels into a DMA-BUF, and the HVS reads those same pixels from the same DMA-BUF. No copy, no conversion, no CPU involvement.

The GStreamer element `v4l2h264dec` with `io-mode=dmabuf` configures this path automatically. When the decoder produces a frame, GStreamer receives a `GstBuffer` containing a `GstMemory` backed by the DMA-BUF fd. This buffer flows downstream to `kmssink`, which calls `drmPrimeFDToHandle()` to import the DMA-BUF into a DRM framebuffer, then calls `drmModeSetPlane()` to assign it to a display plane. The entire operation occurs without mapping the buffer into the application's virtual address space.

| Parameter | Value |
|---|---|
| Driver | bcm2835-codec |
| Device | /dev/video10 |
| API | V4L2 stateful M2M |
| Codec | H.264 High Profile, Level 4.2 |
| Max resolution | 1920×1080 @ 60fps |
| Output format | NV12 (two-plane 4:2:0) |
| Buffer export | DMA-BUF fd (zero-copy compatible) |
| GStreamer element | v4l2h264dec (io-mode=dmabuf) |

#### 3.1.2 HEVC/H.265 Decoder (Stateless V4L2 Request)

The BCM2711 also contains a separate HEVC/H.265 decoder hardware block, which is architecturally and electrically distinct from the H.264 decoder. This decoder supports HEVC Main Profile, Level 5.0, up to 4Kp60 at 8-bit depth (10-bit support is hardware-limited and driver-dependent). The HEVC decoder uses the **stateless V4L2 Request API**, a fundamentally different programming model from the H.264 decoder's stateful M2M interface.

In the stateless model, the application is responsible for parsing the HEVC bitstream and providing per-slice control parameters (slice header, prediction weights, reference frame lists, etc.) alongside the compressed data. The driver does not maintain internal decoder state — each request is self-describing. This requires a HEVC bitstream parser in userspace (GStreamer's `v4l2slh265dec` element), which adds complexity but provides finer control over the decode process.

The upstream driver status is a critical concern. The Raspberry Pi fork maintains the `rpivid` downstream driver, which works on Raspberry Pi OS kernels but is not merged into the mainline Linux kernel. The upstream effort, `rpi-hevc-dec`, was at patch version 5 as of February 2026 and has not been merged. PiCast targets Raspberry Pi OS with its patched kernel, so the downstream driver is available, but the long-term upstream status remains uncertain.

**The critical problem with the HEVC decoder is its output format.** The HEVC hardware block outputs decoded frames in **SAND column format** (also called "handed" or "tiled" format, with V4L2 pixel format codes NC12 for 8-bit and NC30 for 10-bit). SAND format stores pixel data in columns of 128-byte width, which is efficient for the decoder's internal memory access patterns but is completely incompatible with the HVS. The Hardware Video Scaler can only scan out linear NV12/P030 formats — it has no SAND deinterleaver. This means that HEVC decode output **cannot be displayed without a format conversion step**, which fundamentally breaks the zero-copy pipeline.

The conversion options are all problematic:

| Conversion Method | Performance | Zero-Copy | Status |
|---|---|---|---|
| NEON SIMD (ARM) | ~30fps at 4K, ~120fps at 1080p | No (CPU copies) | Available now |
| bcm2835-ISP (hardware) | ~60fps at 4K | No (separate DMA-BUF) | Available now |
| V3D compute shader | Unknown (unproven) | Potentially near-zero-copy | Research phase |
| Kernel driver conversion | Depends on implementation | No (extra buffer) | Not implemented |

Any conversion step requires writing from one DMA-BUF (SAND) into another DMA-BUF (NV12), consuming memory bandwidth and CPU/GPU time. For v1, PiCast avoids this entirely by forcing H.264 format selection through yt-dlp. HEVC support is deferred to v2, pending either a kernel-level SAND→NV12 conversion path or a proven V3D compute shader approach.

| Parameter | H.264 Decoder | HEVC Decoder |
|---|---|---|
| Hardware block | Separate dedicated block | Separate dedicated block |
| Driver | bcm2835-codec (mainline) | rpivid (downstream), rpi-hevc-dec (upstream WIP) |
| V4L2 API | Stateful M2M | Stateless Request |
| Max resolution | 1080p60 | 4Kp60 (8-bit) |
| Output format | NV12 (linear) | NC12/NC30 (SAND column) |
| HVS-compatible | Yes (direct scanout) | No (requires conversion) |
| Zero-copy to display | Yes | No |
| PiCast v1 support | Primary path | Not supported |

### 3.2 Hardware Video Scaler (HVS)

The Hardware Video Scaler (HVS) is the BCM2711's dedicated display compositor, operating entirely independently of the V3D GPU. It is not a software component — it is a fixed-function hardware block that reads pixel data from DMA-BUF-backed framebuffers, performs scaling, colorspace conversion, and alpha blending across multiple input planes, and outputs the composited result to the HDMI transmitter. The HVS runs continuously, scanning out a full frame to HDMI every 16.67ms at 60Hz (or every 33.33ms at 30Hz), regardless of whether the application is actively rendering.

PiCast uses the HVS in its most efficient configuration: two input planes. **Plane 0** carries the decoded video frame (NV12 DMA-BUF from the V4L2 decoder), and **Plane 1** carries the OSD overlay (AR24/XRGB8888 GBM buffer from the V3D GPU). The HVS performs hardware alpha-blending of these two planes during each scanout cycle, producing the final composited frame that appears on the HDMI output. This compositing operation consumes zero CPU time — it is entirely a hardware function.

The HVS has specific capabilities and constraints that influence PiCast's design. It supports up to 16 planes (though the vc4 DRM driver exposes fewer for practical use), each with independent source and destination rectangles, per-plane alpha, and per-pixel alpha from the framebuffer. It can perform high-quality upscaling and downscaling using built-in polyphase filters. Input formats include NV12, P030, XRGB8888, ARGB8888, and RGB565, but notably not SAND column formats.

The info-beamer blog provides the definitive public documentation of HVS behavior, including its plane priority ordering (lower-numbered planes are composited on top), its handling of partial plane updates, and the interaction between atomic modesetting commits and vblank synchronization. PiCast follows the patterns described there: Plane 0 (video) is the bottom layer, Plane 1 (OSD) is composited on top, and all plane updates are committed atomically to prevent tearing.

### 3.3 V3D GPU (VideoCore VI)

The V3D GPU in the BCM2711 is a VideoCore VI part supporting OpenGL ES 3.1, Vulkan 1.0, and delivering approximately 24 GFLOPS of compute performance. It is critical to understand that the V3D is a **3D graphics processor**, not a video decoder. It cannot decode H.264 or HEVC bitstreams — that is the job of the dedicated decoder blocks described above.

In PiCast, the V3D GPU serves a single purpose: **rendering the OSD overlay** on DRM Plane 1. This includes subtitle text rendered by Pango/Cairo via an EGL/GBM surface, and any transient status indicators (buffering animation, error messages, volume level). The GPU renders these elements into an ARGB8888 GBM buffer, which the HVS alpha-blends on top of the video plane during scanout.

The V3D's compute capabilities (OpenGL ES 3.1 compute shaders, Vulkan compute) represent a potential future optimization path for the SAND→NV12 conversion problem described in §3.1.2. A compute shader could potentially transform SAND-column data into linear NV12 directly on the GPU, writing the output to a DMA-BUF that the HVS could then scan out. This would be "near-zero-copy" — the data would move from the HEVC decoder's DMA-BUF to the GPU for transformation and then to a new DMA-BUF for display, but the CPU would not be involved and the data would never enter main memory. However, this approach is unproven: the compute shader's performance characteristics, the DMA-BUF sharing semantics between the HEVC decoder and the V3D, and the latency of the conversion are all open research questions. PiCast v1 does not attempt this optimization.

### 3.4 Memory Architecture

The BCM2711 uses LPDDR4-3200 SDRAM on a 32-bit bus, providing a theoretical peak bandwidth of approximately 12.8 GB/s (3.2 GT/s × 4 bytes per transfer). Practical sustained bandwidth is typically 4–8 GB/s, depending on access patterns, bank conflicts, and the overhead of refresh cycles. The Raspberry Pi 4B+ is available with 2GB, 4GB, or 8GB configurations; PiCast recommends 4GB as the sweet spot, providing ample space for GStreamer buffer pools, the decode pipeline's reference frame storage, and the OS while keeping cost reasonable.

The memory bandwidth budget is directly relevant to PiCast's zero-copy design. A single 1080p60 NV12 frame occupies 3,110,400 bytes (1920 × 1080 × 1.5 bytes for 4:2:0). At 60fps, the HVS reads 186.6 MB/s just for video plane scanout. If the system performed a CPU-side frame copy (reading from the decoder's output buffer and writing to a display buffer), the total memory bandwidth for video would double to ~373 MB/s — still well within the 4–8 GB/s budget, but the copy operation consumes CPU cache capacity, pollutes L1/L2 caches, and adds scheduling latency. More importantly, the copy requires the CPU to touch every pixel, which at 1080p60 means processing 186 MB/s through the CPU's load/store units — this is the difference between ~3% CPU (zero-copy) and ~30% CPU (copy path), and the corresponding power difference of approximately 3 watts.

Zero-copy is therefore not merely an optimization — it is the architectural enabler that allows a 5W appliance to deliver 1080p60 playback. Without zero-copy, the CPU would spend a significant fraction of its time copying frames, raising power consumption, reducing thermal headroom, and potentially causing frame drops under load (e.g., when Tor circuit congestion causes bursty network delivery).

| Memory Parameter | Value |
|---|---|
| SDRAM type | LPDDR4-3200 |
| Bus width | 32-bit |
| Theoretical bandwidth | ~12.8 GB/s |
| Practical sustained bandwidth | 4–8 GB/s |
| 1080p60 NV12 frame size | 3.1 MB |
| Display scanout bandwidth | 186 MB/s |
| Zero-copy total video bandwidth | 186 MB/s (scanout only) |
| Copy-path total video bandwidth | 373 MB/s (read + write + scanout) |
| CPU overhead (zero-copy) | ~3% |
| CPU overhead (copy path) | ~30% |

---

## 4. Zero-Copy Video Pipeline

### 4.1 Pipeline Architecture

The PiCast video pipeline consists of four stages, connected by zero-copy buffer transfers. Each stage operates on DMA-BUF file descriptors — kernel-managed handles to physically contiguous (or IOMMU-mapped) memory that can be shared between hardware devices without copying through main memory.

**Stage 1: Network Fetch.** The `souphttpsrc` GStreamer element fetches the media stream over HTTPS, routing through the Tor SOCKS5 proxy. This stage operates on compressed data (H.264 NALUs within a container) and writes into GStreamer's `queue2` buffer, which provides burst absorption for Tor's variable-bandwidth delivery. The queue2 element can buffer up to 50MB of compressed data, providing 30–60 seconds of playback buffer at typical 720p bitrates over Tor. This stage does involve CPU-side memory copies — compressed data is received from the network stack and written into GStreamer buffers — but the data is small (compressed H.264 at 2–4 Mbps) compared to decoded video (186 MB/s for 1080p60 NV12).

**Stage 2: Hardware Decode.** The `v4l2h264dec` element feeds compressed NALUs to the BCM2711's H.264 hardware decoder via the V4L2 M2M OUTPUT queue. The decoder operates autonomously: it parses NALUs, manages reference frames, performs motion compensation and inverse transform, and writes decoded NV12 frames into CAPTURE queue buffers. When `io-mode=dmabuf` is set, the CAPTURE queue's buffers are DMA-BUF file descriptors. The decoder writes pixel data directly into these DMA-BUFs, which are allocated by the V4L2 framework from the CMA (Contiguous Memory Allocator) pool. The application never maps these buffers into its address space — it merely passes the DMA-BUF fds downstream.

**Stage 3: DRM Plane Assignment.** The `kmssink` element receives `GstBuffer` objects containing DMA-BUF-backed `GstMemory`. For each buffer, kmssink calls `drmPrimeFDToHandle()` to import the DMA-BUF into a DRM framebuffer object (GEM handle), then calls `drmModeSetPlane()` to assign that framebuffer to DRM Plane 0. The import operation does not copy pixel data — it creates a DRM-side reference to the same physical memory that the V4L2 decoder wrote into. The HVS now has a pointer to the decoded frame.

**Stage 4: HDMI Scanout.** The HVS reads from Plane 0's framebuffer during each display refresh cycle (60Hz at 1080p) and outputs the pixel data to the HDMI transmitter. This is a pure DMA operation — the HVS reads from physical memory addresses and writes to the HDMI FIFO. The CPU is not involved in this stage at all. The decoded frame appears on the HDMI output with zero CPU-side pixel processing.

The data flow can be summarized as: compressed bytes (network → GStreamer buffers) → decoded NV12 pixels (V4L2 decoder writes to DMA-BUF) → HVS reads from same DMA-BUF → HDMI. After the initial decode, the pixel data is never read by the CPU, never copied to a second buffer, and never mapped into userspace.

### 4.2 GStreamer Pipeline Definition

The complete GStreamer pipeline for H.264 playback with Tor routing is defined as follows:

```bash
gst-launch-1.0 \
  souphttpsrc location="<resolved-url>" \
    proxy-id="" \
    socks5-proxy-ip=127.0.0.1 \
    socks5-proxy-port=9050 \
    socks5-proxy-username="picast-<site-hash>" \
  ! queue2 max-size-bytes=52428800 \
    use-buffering=true \
    buffering-threshold-high=80 \
    buffering-threshold-low=10 \
  ! h264parse \
    config-interval=-1 \
  ! v4l2h264dec \
    io-mode=dmabuf \
    capture-io-mode=dmabuf \
  ! kmssink \
    driver-name=vc4 \
    plane-id=0 \
    can-scale=true \
    force-modesetting=true
```

Element-by-element explanation:

- **`souphttpsrc`**: HTTPS fetch via libsoup. Configured with `socks5-proxy-ip` and `socks5-proxy-port` to route through the local Tor SOCKS proxy at `127.0.0.1:9050`. The `socks5-proxy-username` field carries the stream isolation identifier (e.g., `picast-youtube-abc123`), which Tor's `IsolateSOCKSAuth` uses to assign this connection to a dedicated circuit. The `proxy-id=""` disables HTTP proxy (we use SOCKS5, not HTTP proxy).

- **`queue2`**: Burst-absorption buffer with 50MB capacity (`max-size-bytes=52428800`). Tor's bandwidth is variable — a circuit might deliver 4 Mbps for several seconds and then stall for 500ms while a new relay is selected. The queue2 buffer absorbs these bursts, providing smooth decode input. `use-buffering=true` enables buffering messages on the GStreamer bus, which PiCast uses to display buffering state via the WebSocket channel. The high/low thresholds control when buffering starts and stops: below 10% full, the pipeline pauses; above 80% full, it resumes.

- **`h264parse`**: Parses the H.264 bitstream, extracting SPS/PPS NALUs and ensuring they are prepended to each keyframe (`config-interval=-1`). This is necessary because some streaming formats (notably raw H.264 over HTTP) may send SPS/PPS only once at the start, but the stateful V4L2 decoder requires them before every IDR frame to initialize correctly.

- **`v4l2h264dec`**: The V4L2 H.264 hardware decoder. `io-mode=dmabuf` configures the OUTPUT queue (compressed input) to use DMA-BUF, and `capture-io-mode=dmabuf` configures the CAPTURE queue (decoded output) to export DMA-BUF file descriptors. This is the critical setting that enables zero-copy — without it, the decoder would allocate system memory buffers and require the application to copy data.

- **`kmssink`**: Direct DRM/KMS output sink. `driver-name=vc4` selects the vc4 DRM driver. `plane-id=0` assigns the video to the first display plane. `can-scale=true` allows the HVS to scale the video to fit the display resolution (e.g., upscaling 720p to 1080p). `force-modesetting=true` ensures the CRTC is configured even if no prior mode has been set.

### 4.3 OSD Overlay Pipeline

The OSD overlay runs as a parallel pipeline that shares the DRM device but uses a separate display plane and a different rendering backend. Where the video pipeline is V4L2 → DMA-BUF → DRM, the OSD pipeline is CPU (Pango/Cairo) → V3D GPU → GBM buffer → DRM.

The OSD rendering process works as follows. A dedicated rendering thread maintains an EGL context backed by a GBM surface allocated on DRM Plane 1. When subtitles or status indicators need to be displayed, the thread renders text using Pango for layout and Cairo for rasterization, writing into the EGL surface. The V3D GPU accelerates the compositing and format conversion, producing an ARGB8888 buffer suitable for HVS alpha-blending.

The GBM buffer for the OSD is allocated via `gbm_bo_create()` with `GBM_BO_USE_RENDERING | GBM_BO_USE_SCANOUT` flags, ensuring it can be both rendered to by the V3D and scanned out by the HVS. When a new OSD frame is ready, the application calls `drmModeAtomicCommit()` to update Plane 1's framebuffer to the new GBM buffer's DRM handle, simultaneously with any Plane 0 updates. Atomic commit ensures that the HVS updates both planes in the same vblank period, preventing visual tearing or misaligned overlay/video frames.

For subtitles specifically, GStreamer's `subtitleoverlay` element can render SRT/VTT subtitles directly into the video pipeline. However, this approach requires copying the video frame (to composite the subtitle text on top of it), which breaks zero-copy. PiCast instead extracts subtitle data from the GStreamer pipeline and renders it on the separate OSD plane, preserving the zero-copy video path. The subtitle text, timing, and styling are parsed by a custom GStreamer pad probe that intercepts subtitle buffers before `subtitleoverlay` and forwards them to the OSD renderer.

### 4.4 HEVC Pipeline (Deferred)

As described in §3.1.2, the HEVC decoder's SAND column output format is incompatible with direct HVS scanout. The HEVC pipeline would require an additional conversion stage between decode and display:

```
souphttpsrc → queue2 → h265parse → v4l2slh265dec → [SAND→NV12 conversion] → kmssink
```

The conversion stage is the blocker. Three approaches have been evaluated:

1. **NEON SIMD conversion**: An ARM NEON-optimized SAND→NV12 routine can process approximately 30 frames per second at 4K resolution, or ~120fps at 1080p. This is sufficient for 1080p60 HEVC playback, but it involves CPU-side memory copies (reading from the SAND DMA-BUF, converting in registers, writing to an NV12 DMA-BUF), which violates the zero-copy principle and adds ~15–20% CPU utilization.

2. **bcm2835-ISP conversion**: The ISP hardware block can perform format conversions, but it requires a separate DMA-BUF allocation for the output and an additional kernel IOCTL to submit the conversion job. This avoids CPU involvement but still requires an extra buffer copy at the hardware level. Latency and power impact are not well characterized.

3. **V3D compute shader**: A Vulkan compute shader could theoretically perform the SAND→NV12 reorganization directly on the GPU, reading from the HEVC decoder's DMA-BUF and writing to a new NV12 DMA-BUF. This would be near-zero-copy (data stays in GPU-accessible memory) and zero-CPU, but the approach is unproven — no working implementation has been demonstrated, and the DMA-BUF import semantics between the rpivid driver and the V3D GPU are not guaranteed to work.

GStreamer 1.26 includes work-in-progress patches for SAND format handling in the V4L2 elements, which would simplify pipeline construction but do not solve the fundamental conversion problem. For PiCast v1, HEVC is entirely deferred: yt-dlp's format selection is configured to prefer H.264 streams (`bestvideo[vcodec^=avc1]`), and the system does not attempt to play HEVC content. This limits the maximum available resolution on some platforms (YouTube, for example, offers 4K only in VP9/AV1 on many videos, with H.264 capped at 1080p), but 1080p over Tor is already at the edge of practical bandwidth.

---

## 5. Display Stack: DRM/KMS Direct

### 5.1 Why No Display Server

PiCast does not run X11, Wayland, or any other display server. This decision is sometimes misunderstood as mere minimalism, but it is in fact a technical necessity for the zero-copy pipeline and a security advantage for the appliance model.

A display server sits between rendering clients and the kernel's DRM/KMS subsystem. It owns the DRM master privilege, manages CRTC and plane assignments, and composites client buffers into the final display output. This indirection introduces several problems for PiCast:

1. **Buffer copy overhead**: A display server typically composites client surfaces into its own framebuffer before presenting to the CRTC. Even Wayland compositors that support direct scanout (weston, mutter, kwin) require explicit per-surface opt-in and have strict format/modifier constraints. The zero-copy DMA-BUF path from V4L2 decoder to DRM plane is fragile under a compositor — the compositor may decide to copy the buffer for compositing, defeating the zero-copy optimization.

2. **Resource consumption**: X11 with a typical desktop environment consumes 50–100MB of RAM. Wayland compositors are lighter but still require 20–50MB. On a 2GB Pi, this is 2–5% of total system memory consumed by a component that provides no value for a single-rendering-client appliance.

3. **Scheduling latency**: A display server introduces an additional scheduling entity between the media pipeline and the display hardware. The compositor must be scheduled by the Linux kernel to process buffer submissions, which adds variable latency (typically 1–5ms) to the display path. Under memory pressure or CPU contention, this latency can spike, causing frame drops.

4. **Attack surface**: A display server is a complex, privileged process that handles IPC from untrusted clients (X11), parses protocol messages, manages GPU memory, and holds DRM master privileges. CVE databases show consistent vulnerability reports for X11 servers and Wayland compositors. For an appliance with a single rendering client, the display server's entire attack surface is unnecessary.

PiCast instead opens `/dev/dri/card0` directly, calls `drmSetMaster()` to acquire DRM master privileges, and programs the HVS planes directly via DRM IOCTLs. This is the same approach used by Kodi, LibreELEC, and other embedded media players. The application is the only rendering client, there are no window management decisions, and there is no input event routing (the Pi has no keyboard or mouse). The display server provides no functionality that PiCast needs.

### 5.2 DRM/KMS Resource Model

The vc4 DRM driver exposes the BCM2711's display hardware through the standard Linux DRM/KMS API. PiCast uses the following resources:

| DRM Resource | Description | PiCast Usage |
|---|---|---|
| CRTC 0 | Display controller, generates vblank events, drives HDMI | Single CRTC for HDMI output |
| Plane 0 | Primary video plane, NV12 compatible | Decoded video from V4L2 decoder |
| Plane 1 | Overlay plane, ARGB8888 compatible | OSD (subtitles, status) |
| Connector (HDMI) | Physical output, reports EDID, sets display mode | HDMI-A-1, 1080p60 preferred |

Buffer allocation uses two different paths for the two planes:

- **Plane 0 (video)**: Buffers come from the V4L2 decoder's CAPTURE queue as DMA-BUF file descriptors. These are allocated by the V4L2 framework from the CMA (Contiguous Memory Allocator) pool, which provides physically contiguous memory suitable for DMA by both the decoder and the HVS. When `v4l2h264dec` produces a frame with `capture-io-mode=dmabuf`, the resulting `GstBuffer` contains a `GstDmaBufAllocator`-backed `GstMemory`. The `kmssink` element imports this DMA-BUF fd into a DRM GEM handle via `drmPrimeFDToHandle()`.

- **Plane 1 (OSD)**: Buffers are allocated via GBM (`Generic Buffer Manager`). The application calls `gbm_bo_create()` with the desired width, height, format (ARGB8888), and usage flags (`GBM_BO_USE_RENDERING | GBM_BO_USE_SCANOUT`). GBM allocates from the same CMA pool as V4L2, ensuring the HVS can scan out the result. The EGL context renders into this GBM buffer via a `gbm_surface`, and the resulting `gbm_bo` is imported into DRM via `gbm_bo_get_handle()`.

The DRM master privilege is held by the PiCast process throughout its lifetime. Since there is no display server, no other process competes for DRM master. If the PiCast process crashes, `systemd` restarts it, and it re-acquires DRM master on the next device open.

### 5.3 Atomic Modesetting

PiCast uses the DRM **atomic modesetting** API (`drmModeAtomicCommit`) for all display updates. Atomic modesetting is the modern DRM API that allows multiple properties (plane source/destination rectangles, framebuffer assignments, CRTC mode) to be changed in a single, atomic commit that takes effect at the next vblank. This is essential for tear-free display updates — without atomic commits, updating Plane 0 and Plane 1 in separate IOCTL calls could result in one frame of desynchronized display (new video frame with old OSD, or vice versa).

The atomic commit workflow is:

1. Create an atomic request: `drmModeAtomicReqPtr req = drmModeAtomicAlloc()`
2. Set plane properties for Plane 0 (video): source rect from decoder output, destination rect to fill display, framebuffer from imported DMA-BUF
3. Set plane properties for Plane 1 (OSD): source/destination rects, framebuffer from GBM buffer, zpos to ensure overlay on top
4. Commit: `drmModeAtomicCommit(fd, req, DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_PAGE_FLIP_EVENT, userdata)`
5. Wait for vblank event via `drmHandleEvent()` to confirm the update has been applied

The `DRM_MODE_PAGE_FLIP_EVENT` flag requests a vblank event when the commit takes effect. PiCast uses this event for vblank synchronization — the OSD renderer only updates when the previous frame has been scanned out, preventing rendering ahead of the display and wasting memory bandwidth on unseen frames.

For the video pipeline specifically, `kmssink` handles atomic commits internally. The element implements a `GstPadProbe` that intercepts outgoing buffers and submits them to DRM via atomic commit. The `plane-properties` setting on kmssink can be used to set the zpos and alpha values for each plane.

---

## 6. Protocol Layer

### 6.1 Protocol Selection Rationale

PiCast does not implement Google's Cast V2 protocol, despite it being the most widely recognized casting protocol. This is a deliberate decision based on technical constraints, not a preference for custom protocols.

Cast V2 requires the receiver device to authenticate with Google's cloud infrastructure using a registered device certificate. The receiver must present a valid OAuth2 token during the handshake, which is provisioned only to devices that have completed Google's manufacturing certification process. Unofficial receivers — including PiCast — cannot obtain these credentials. Without successful authentication, the sender (Chrome browser, Android phone) will not discover or connect to the device, even if it advertises `_googlecast._tcp.local` via mDNS.

Some open-source projects (notably go-chromecast) have attempted to bypass the authentication requirement by reverse-engineering the protocol, but these efforts are fragile and break frequently as Google updates the protocol. The Cast V2 specification itself is proprietary and subject to change without notice. Implementing a fragile reverse-engineered protocol would violate PiCast's reliability requirement.

Instead, PiCast provides three independent interfaces that cover the same use cases without any dependency on Google infrastructure:

1. **HTTP REST API** — for programmatic control, home automation, and the browser extension
2. **UPnP/DLNA MediaRenderer** — for compatibility with existing media apps (VLC, BubbleUPnP, Home Assistant)
3. **WebSocket status channel** — for real-time bidirectional communication

These three interfaces can be used simultaneously by different senders. The HTTP API and DLNA interface both accept media URLs and control playback; the WebSocket channel provides real-time status updates to all connected senders.

### 6.2 HTTP REST API (Port 8585)

The HTTP REST API is the primary control interface for PiCast, used by the browser extension and any programmatic sender. It runs on port 8585 and accepts JSON request/response bodies.

| Method | Endpoint | Request Body | Response | Description |
|---|---|---|---|---|
| POST | /api/cast | `{url, format?, resumePosition?}` | `{sessionId, state, resolvedUrl}` | Submit URL for casting. Triggers resolution if not a direct media URL. Returns session ID for tracking. |
| POST | /api/stop | `{sessionId?}` | `{state}` | Stop playback. Clears queue if no sessionId. |
| POST | /api/pause | `{sessionId?}` | `{state, position}` | Toggle pause/resume. |
| POST | /api/seek | `{sessionId?, position}` | `{state, position}` | Seek to position (seconds from start). |
| GET | /api/status | — | `{state, position, duration, bufferLevel, url, sessionId, queue}` | Current playback status including buffer level and queue. |

The `/api/cast` endpoint accepts two types of URLs:

- **Page URLs** (e.g., `https://www.youtube.com/watch?v=dQw4w9WgXcQ`): These trigger the content resolution pipeline (§7), which invokes yt-dlp to extract a direct media URL through the Tor proxy. Resolution typically takes 3–15 seconds depending on the site and Tor circuit latency. The response includes the resolved URL for sender-side caching.

- **Direct media URLs** (e.g., `https://cdn.example.com/video.mp4`): These bypass yt-dlp resolution and are passed directly to the GStreamer pipeline. The browser extension often provides pre-resolved URLs from its interception logic.

The `format` field in the cast request allows the sender to suggest a preferred format (e.g., `720p`, `1080p`, `audio-only`). If omitted, PiCast uses its default format selection strategy (720p for Tor mode, 1080p for resolution-only/off mode). The `resumePosition` field supports resuming playback from a specific timestamp, used for queue entries that were previously interrupted.

All responses include the current session state, allowing the sender to confirm the command took effect without polling. For real-time status updates, the WebSocket channel (§6.4) is preferred over polling `/api/status`.

### 6.3 UPnP/DLNA MediaRenderer (Port 49152)

PiCast implements a UPnP/DLNA MediaRenderer device, based on the `gmediarender` open-source project with modifications to use PiCast's GStreamer pipeline and Tor proxy. The MediaRenderer implementation supports two UPnP service templates:

- **AVTransport** (urn:schemas-upnp-org:service:AVTransport:1): Controls playback (SetAVTransportURI, Play, Pause, Stop, Seek). The `SetAVTransportURI` action accepts a media URL, which PiCast passes to the GStreamer pipeline. The `CurrentTransportState` variable reports IDLE/PLAYING/PAUSED_PLAYBACK/STOPPED/TRANSITIONING.

- **RenderingControl** (urn:schemas-upnp-org:service:RenderingControl:1): Controls audio volume and mute state. PiCast maps these to GStreamer's `volume` element and ALSA mixer controls.

The DLNA interface is compatible with a wide range of sender applications, including VLC, BubbleUPnP, Hi-Fi Cast, Home Assistant's DLNA integration, and any application that implements the UPnP AVTransport control point specification. This provides a zero-install casting experience: users who already have a DLNA-compatible app on their phone can cast to PiCast without installing anything new.

However, the DLNA interface has a significant limitation compared to the HTTP API: the URL provided via `SetAVTransportURI` must be directly fetchable by the PiCast device. DLNA control points typically send the direct streaming URL, not a page URL. This means DLNA senders cannot benefit from yt-dlp resolution — if a user wants to cast a YouTube video via DLNA, the control point must resolve the direct media URL itself, or PiCast's DLNA handler must detect that the URL is a page URL and invoke yt-dlp internally. PiCast implements the latter behavior: when `SetAVTransportURI` receives a URL that does not have a recognized media file extension (`.mp4`, `.mkv`, `.m3u8`, `.mpd`, etc.), it passes the URL through the content resolution pipeline before starting playback.

### 6.4 WebSocket Status Channel (Port 8586)

The WebSocket channel provides real-time bidirectional communication between PiCast and connected senders. It runs on port 8586 and uses JSON-encoded messages. The channel serves two purposes: pushing status updates from PiCast to senders (eliminating the need for polling), and receiving low-latency control commands from senders.

**Messages from PiCast to senders:**

| Message Type | Fields | Description |
|---|---|---|
| `MEDIA_STATUS` | `{state, position, duration, bufferLevel, url, sessionId}` | Pushed on any state change, position update (every 1s during playback), or buffer level change. |
| `RESOLVE_PROGRESS` | `{url, stage, progress?, error?}` | Pushed during content resolution. Stages: `started`, `fetching_info`, `extracting_url`, `completed`, `failed`. Provides feedback during yt-dlp's 3–15s resolution time. |
| `ERROR` | `{code, message, url?, recoverable}` | Pushed on errors. Recoverable errors allow retry; non-recoverable errors transition to ERROR state. |

**Messages from senders to PiCast:**

| Message Type | Fields | Description |
|---|---|---|
| `CAST` | `{url, format?}` | Same as POST /api/cast. |
| `STOP` | `{sessionId?}` | Same as POST /api/stop. |
| `PAUSE` | `{sessionId?}` | Same as POST /api/pause. |
| `SEEK` | `{position}` | Same as POST /api/seek. |
| `VOLUME` | `{level}` | Set volume (0.0–1.0). |

Multiple senders can connect to the WebSocket channel simultaneously. PiCast broadcasts `MEDIA_STATUS` and `ERROR` messages to all connected senders, ensuring that a status change initiated by one sender (e.g., pressing pause in the browser extension) is immediately reflected on all other connected senders (e.g., the Home Assistant dashboard). Control commands from any sender are processed on a first-come-first-served basis — there is no sender priority or ownership model.

### 6.5 Device Discovery

PiCast advertises itself on the local network using two discovery protocols:

**mDNS (Multicast DNS):** PiCast registers two mDNS service types:
- `_picast._tcp.local` on port 8585 — PiCast's custom service type, used by the browser extension for auto-discovery. The TXT record includes `version=1.0`, `tor=enabled`, and `maxResolution=1080p`.
- `_http._tcp.local` on port 8585 — Standard HTTP service, used by generic mDNS browsers and home automation platforms.

**SSDP (Simple Service Discovery Protocol):** PiCast sends periodic SSDP NOTIFY messages and responds to M-SEARCH queries for the UPnP MediaRenderer device type (`urn:schemas-upnp-org:device:MediaRenderer:1`) on the standard SSDP multicast address (239.255.255.250:1900). This enables discovery by UPnP control points without mDNS support.

PiCast explicitly does **not** register `_googlecast._tcp.local`. Advertising as a Google Cast device would cause Chromecast-compatible senders (Android, Chrome) to discover PiCast and attempt the Cast V2 authentication handshake, which would fail. This would create a confusing user experience — the device would appear in cast menus but fail to connect. It is better to not appear as a Cast target at all than to appear and fail.

---

## 7. Content Resolution Pipeline

### 7.1 Resolution Strategy

PiCast uses a two-tier resolution strategy to convert user-submitted URLs into directly playable media URLs. The distinction between the tiers is based on whether the submitted URL points to a web page (which requires extraction logic to find the embedded media) or directly to a media resource (which can be played immediately).

**Tier 1: yt-dlp extraction.** For page URLs (YouTube, Vimeo, Twitter, and 1,800+ other sites), PiCast invokes yt-dlp to extract the direct media URL, format metadata, and subtitle URLs. This tier handles the vast majority of user-facing casting scenarios — most users submit a page URL from their browser, not a direct media URL.

**Tier 2: Direct URL handling.** For URLs that point directly to media files (identified by file extension or Content-Type header), PiCast bypasses yt-dlp and passes the URL directly to the GStreamer pipeline. This tier handles CDN-hosted files, HLS/DASH manifests, and pre-resolved URLs from the browser extension.

The tier selection is automatic. When a URL is received, PiCast checks it against a list of known media extensions (`.mp4`, `.mkv`, `.avi`, `.m3u8`, `.mpd`, `.ts`, `.webm`, `.flac`, `.mp3`, `.ogg`, `.opus`, `.wav`) and known media content types. If the URL matches, it goes to Tier 2. If not, it goes to Tier 1. If Tier 1 fails (yt-dlp cannot extract the URL), PiCast falls back to Tier 2, passing the original URL to GStreamer's `parsebin` element for auto-detection.

### 7.2 Tier 1: yt-dlp Extraction

yt-dlp is a Python-based command-line tool that extracts direct media URLs from over 1,800 websites. It is the backbone of PiCast's content resolution pipeline. PiCast invokes yt-dlp as a subprocess, routing all its network traffic through the Tor SOCKS proxy.

The full invocation for URL extraction is:

```bash
yt-dlp \
  --proxy socks5h://picast-<site-hash>@127.0.0.1:9050 \
  -J \
  --no-playlist \
  --no-warnings \
  --no-check-certificates \
  -f "bestvideo[vcodec^=avc1][height<=720]+bestaudio/best[vcodec^=avc1][height<=720]/best[height<=720]/best" \
  --write-subs \
  --write-auto-subs \
  --sub-langs "en.*,.*" \
  --sub-format vtt \
  --paths /tmp/picast/subs/ \
  "<url>"
```

Parameter explanation:

- **`--proxy socks5h://picast-<site-hash>@127.0.0.1:9050`**: Routes all yt-dlp traffic through the Tor SOCKS proxy. The `socks5h://` scheme (note the `h`) forces DNS resolution through the Tor network, preventing DNS leaks. The username `picast-<site-hash>` enables Tor stream isolation via `IsolateSOCKSAuth` — connections with different SOCKS usernames are assigned to different Tor circuits. The `<site-hash>` is a truncated SHA-256 of the URL's domain, so videos from the same site share a circuit while videos from different sites use separate circuits (see §8.3).

- **`-J`**: Outputs a single JSON object containing all extracted information (media URLs, format metadata, subtitles, duration, title). PiCast parses this JSON to select the optimal format and configure the GStreamer pipeline.

- **`--no-playlist`**: Prevents yt-dlp from extracting an entire playlist when given a video URL that is part of a playlist. PiCast handles playlist queuing separately.

- **`-f "bestvideo[vcodec^=avc1][height<=720]+bestaudio/..."`**: Format selection string that prioritizes H.264 (avc1) video at 720p or below. This is critical for three reasons: (1) the BCM2711's H.264 hardware decoder is the only zero-copy-compatible decode path; (2) 720p is the practical maximum resolution for reliable playback over Tor (see §8.2); and (3) forcing H.264 avoids the HEVC/VP9/AV1 formats that require software decode or format conversion.

- **`--write-subs --write-auto-subs`**: Extracts both manual and auto-generated subtitles (see §7.4).

The format selection string is the most critical parameter. It encodes the following preference order:

1. Best H.264 video ≤720p + best audio (separate streams)
2. Best H.264 combined video+audio ≤720p (pre-merged)
3. Best video ≤720p of any codec (fallback — will fail to play if non-H.264)
4. Best available video of any resolution/codec (last resort)

If the only available format is VP9 or AV1 (common for YouTube 1080p+), yt-dlp will still extract the URL, but PiCast's GStreamer pipeline will fail to decode it (no hardware decoder available). In this case, the error is reported to the sender, and the user may need to cast from a source that provides H.264 streams. Future versions may implement software VP9 decode as a fallback for lower resolutions.

### 7.3 Tier 2: Direct URL Handling

When the submitted URL is recognized as a direct media resource, PiCast bypasses yt-dlp and passes the URL directly to the GStreamer pipeline. This avoids the 3–15 second overhead of yt-dlp extraction and is the preferred path when the browser extension has already identified the media URL.

The direct URL path uses GStreamer's `parsebin` element for container auto-detection. `parsebin` probes the incoming data, identifies the container format (MP4, MKV, WebM, AVI, etc.), and instantiates the appropriate demuxer and parser elements automatically. For adaptive streaming formats (HLS `.m3u8`, DASH `.mpd`), the `adaptivedemux2` element handles segment fetching, manifest parsing, and bitrate adaptation.

The browser extension can provide pre-resolved direct URLs via the `url` field in the `/api/cast` request, along with optional `mimeType` and `headers` fields. When these are provided, PiCast can skip even the `parsebin` auto-detection and directly instantiate the appropriate GStreamer elements, reducing startup latency to under 1 second.

For HLS and DASH manifests, the `adaptivedemux2` element fetches segment lists and individual segments through the Tor proxy. Adaptive bitrate switching is disabled in PiCast v1 (the format is fixed by yt-dlp's resolution), but the demuxer's segment fetching and buffering logic is still used for reliable playback of chunked streams.

### 7.4 Subtitle Extraction

PiCast supports three subtitle sources:

1. **yt-dlp extracted subtitles**: When yt-dlp is invoked with `--write-subs` and `--write-auto-subs`, it downloads subtitle files in VTT or SRT format. These files are stored in `/tmp/picast/subs/` and loaded by GStreamer's `subtitleoverlay` element. However, as noted in §4.3, PiCast does not use `subtitleoverlay` directly (it would break zero-copy by compositing subtitles into the video frame). Instead, the subtitle file is parsed by a custom module that extracts text, timing, and styling, and renders it on the OSD plane.

2. **Embedded captions (EIA-608/CEA-608)**: Some video streams, particularly North American broadcast content, carry closed captions embedded in the H.264 SEI NAL units. GStreamer's `closedcaption` element extracts these captions, which are then rendered on the OSD plane alongside or instead of external subtitles.

3. **External subtitle files**: Users can submit subtitle URLs or file paths alongside the media URL via the HTTP API's `subtitleUrl` parameter. This is useful for sites where yt-dlp cannot extract subtitles, or for custom subtitle tracks.

Subtitle rendering on the OSD plane preserves the zero-copy video pipeline while providing full subtitle functionality. The subtitle renderer supports text styling (font, size, color, outline), positioning (bottom-center by default, with SSA/ASS positioning support), and timing (VTT/SRT timestamp parsing with millisecond accuracy).

---

## 8. Tor Integration

### 8.1 Tor as Security Substrate

Tor is not an optional privacy feature in PiCast — it is a fundamental security property. The distinction matters. A privacy feature can be disabled for convenience; a security property cannot be compromised without undermining the system's purpose.

Without Tor, the user's ISP (and any passive network observer) can see every URL that PiCast resolves and every media server it connects to. This creates a complete viewing history: which videos were watched, when, and from which sources. For users in surveillance-heavy environments, this metadata can be identifying or incriminating. Even in benign environments, the principle of least information applies: the ISP does not need to know what media the user consumes.

Tor provides three layers of encryption and routes traffic through three relays (guard, middle, exit) in a circuit. The guard relay knows the user's IP address but not the destination. The exit relay knows the destination but not the user's IP address. The middle relay knows neither. No single relay can link the user to the content they are accessing.

PiCast uses the C Tor daemon (`tor` package), not the Rust-based `arti` client. This decision is driven by a single critical feature: `IsolateSOCKSAuth` support. The `IsolateSOCKSAuth` option on Tor's `SocksPort` configuration ensures that SOCKS connections with different username/password credentials are assigned to different Tor circuits. PiCast uses this feature to implement per-site stream isolation (see §8.3). The `arti` client, as of its current implementation, does not support `IsolateSOCKSAuth`, making it unsuitable for PiCast's stream isolation requirements.

The Tor daemon runs as a system service under the `debian-tor` user, with its `SocksPort` configured on `127.0.0.1:9050` and its `ControlPort` on `127.0.0.1:9051`. PiCast does not need the control port for circuit management — stream isolation is handled entirely through SOCKS username parameters. The Tor daemon's configuration includes:

```
SocksPort 127.0.0.1:9050 IsolateSOCKSAuth
SocksPort 127.0.0.1:9050 IsolateSOCKSAuth
ControlPort 127.0.0.1:9051
CookieAuthentication 1
SafeLogging 1
```

`SafeLogging 1` ensures that Tor's log files do not contain sensitive information (URLs, IP addresses), which is important for a device that may be accessed or inspected by others on the LAN.

### 8.2 Bandwidth Reality

Tor's bandwidth is the primary constraint on PiCast's video quality. The Tor network consists of volunteer-operated relays with widely varying capacity. Effective throughput depends on the selected circuit's relay capacities, network congestion, and the number of simultaneous Tor users. PiCast cannot control which relays are selected, and throughput can vary significantly between circuits and over time.

The following table summarizes realistic bandwidth expectations for video playback over Tor, based on empirical measurements across multiple circuits and time periods:

| Resolution | Bitrate Range | Tor Reliability | Typical Buffer Time | Notes |
|---|---|---|---|---|
| 240p | 0.3–0.5 Mbps | Always reliable | <2s | Lowest quality, suitable for audio-centric content |
| 360p | 0.5–1.0 Mbps | Reliable | 2–5s | Acceptable for most content, slight softness |
| 480p | 1.0–2.0 Mbps | Usually reliable | 5–15s | Good quality on small screens, occasional buffering |
| 720p | 2.0–4.0 Mbps | Possible with buffering | 15–60s | PiCast's default target. 50MB queue2 buffer provides 30–60s of playback. |
| 1080p | 4.0–8.0 Mbps | Unreliable | 30–120s+ | Frequent buffering stalls. Only viable on fast circuits. |

The 720p default is a practical compromise. At 2–4 Mbps, most Tor circuits can sustain playback with the queue2 buffer absorbing periodic bandwidth dips. The 50MB queue2 buffer (`max-size-bytes=52428800`) holds approximately 30–60 seconds of 720p H.264 video, providing sufficient cushion for typical Tor circuit fluctuations (500ms–2s stalls). When the buffer drops below the low threshold (10%), PiCast enters BUFFERING state and displays a buffering indicator on the OSD. When it recovers above the high threshold (80%), playback resumes.

1080p playback is possible on favorable Tor circuits but unreliable. Users who attempt 1080p will experience frequent buffering stalls, sometimes lasting 10–30 seconds while the queue2 buffer refills. PiCast's adaptive bitrate logic (§10.3) can automatically downscale from 1080p to 720p when buffer levels are persistently low, but the re-resolution process takes 5–30 seconds (yt-dlp must re-extract the URL for a lower-quality format), during which playback continues from the remaining buffer.

### 8.3 Stream Isolation Strategy

Stream isolation is the practice of routing different network connections through different Tor circuits, preventing correlation of traffic from different sources. PiCast implements per-site stream isolation using Tor's `IsolateSOCKSAuth` feature.

When a SOCKS connection is made to Tor with a username/password pair, Tor's `IsolateSOCKSAuth` option ensures that connections with different username values use different circuits. Connections with the same username share a circuit. PiCast constructs the SOCKS username as `picast-<site-hash>`, where `<site-hash>` is the first 8 characters of the SHA-256 hash of the URL's domain. For example:

- YouTube URL → `picast-2cdc4e7a` (hash of "youtube.com")
- Vimeo URL → `picast-8f3b1a09` (hash of "vimeo.com")
- Direct CDN URL → `picast-direct` (shared circuit for all direct URLs)

This strategy means that all videos from the same site share a Tor circuit, while videos from different sites use separate circuits. The rationale for per-site isolation (rather than per-video isolation) is:

1. **Circuit creation overhead**: Establishing a new Tor circuit takes 5–15 seconds (three TLS handshakes through the guard, middle, and exit relays). If every video used a new circuit, users would wait 5–15 seconds before each resolution and fetch — an unacceptable delay for a media appliance.

2. **Bandwidth consistency**: A single circuit's bandwidth is relatively stable over short periods. By reusing a circuit for the same site, consecutive videos benefit from the circuit's existing bandwidth characteristics. Per-video isolation would assign each video to a random circuit, resulting in unpredictable bandwidth.

3. **Practical threat model**: The primary threat is correlation of the user's viewing across different sites. Per-site isolation prevents the exit relay from seeing that the same user visits both site A and site B. Per-video isolation within a site provides marginal additional benefit — the site itself already knows which videos the user requested, and the exit relay can already see that the user is connecting to that site's CDN.

The `IsolateSOCKSAuth` configuration in Tor ensures that connections with different SOCKS usernames cannot share a circuit, even if they are made simultaneously. This is a stronger guarantee than what could be achieved with `SocksPort` isolation (which would require multiple SocksPort instances).

### 8.4 DNS Leak Prevention

DNS leakage is a critical concern for any Tor-routed system. If DNS queries bypass Tor and are sent to the ISP's recursive resolver, the ISP can observe the domains the user is accessing, even though the actual media traffic is Tor-encrypted. PiCast implements defense-in-depth DNS leak prevention at three layers:

**Layer 1: SOCKS5h protocol enforcement.** All connections to the Tor proxy use the `socks5h://` scheme (SOCKS5 with remote DNS resolution). The `h` suffix tells the SOCKS client library to send the hostname to the Tor proxy for resolution, rather than resolving it locally. The Tor proxy resolves the hostname through the Tor network (via the exit relay's DNS resolver), ensuring that DNS queries never reach the ISP's resolver. PiCast configures this in every component: GStreamer's `souphttpsrc` element (`socks5-proxy-ip` parameter), yt-dlp's `--proxy` flag, and any custom HTTP clients.

**Layer 2: System resolver override.** PiCast sets `/etc/resolv.conf` to point to `127.0.0.1`, where a local `dnsmasq` instance listens. This dnsmasq is configured to **refuse all queries** — it does not forward any DNS requests upstream. This ensures that even if an application or library ignores the SOCKS5h configuration and attempts to resolve a hostname locally, the DNS query will fail immediately rather than leaking to the ISP's resolver. The failed resolution will cause the application to fall back to the SOCKS5h proxy (or fail with a clear error), rather than silently leaking.

**Layer 3: iptables OUTPUT chain filtering.** PiCast's firewall rules (see §11.2) block all outbound traffic except connections to `127.0.0.1:9050` (Tor SOCKS), `127.0.0.1:9051` (Tor control), and LAN addresses. UDP port 53 (DNS) outbound to any non-local address is explicitly DROPped and LOGged. This provides a kernel-level guarantee that DNS queries cannot reach an external resolver, even if the application and system resolver configurations are both misconfigured.

The combination of these three layers ensures that DNS leakage is effectively impossible under normal operation. The only theoretical bypass would be a compromised Tor exit relay that injects DNS resolution into the Tor protocol itself, but this is outside PiCast's threat model — it would require compromise of the Tor protocol, which is a network-level rather than application-level concern.

---

## 9. Browser Extension Architecture

### 9.1 Extension Design

The PiCast browser extension is the primary user-facing interface for casting content from a desktop browser. It is implemented as a Manifest V3 extension for Chrome and Firefox, using the modern declarative and event-driven APIs that Manifest V3 requires.

The extension operates in two modes, selected automatically based on the content being viewed:

**Intercept mode** uses the `declarativeNetRequest` API (Manifest V3's replacement for `webRequest` blocking) to observe network requests and identify media URLs. The extension registers dynamic rules that match common media URL patterns: `.m3u8` (HLS manifests), `.mpd` (DASH manifests), `.mp4` (MPEG-4 containers), `.ts` (MPEG-TS segments), `.webm` (WebM containers), and `.m4s` (DASH/CMAF segments). When a matching URL is observed, the extension captures it and presents a "Cast to PiCast" action to the user. If the user accepts, the intercepted URL is sent directly to PiCast's HTTP API, bypassing yt-dlp resolution entirely.

**Fallback mode** is activated when intercept mode does not capture a usable media URL (e.g., the site uses encrypted media segments, non-standard URL patterns, or dynamically generated URLs). In fallback mode, the extension sends the page URL to PiCast's HTTP API, which invokes yt-dlp for server-side resolution. This mode is slower (3–15 seconds for yt-dlp extraction) but works with any site that yt-dlp supports.

The extension automatically selects the appropriate mode: if intercept mode has captured a media URL, it uses that; if not, it falls back to page URL submission. The user can override this behavior via the extension's settings (see §9.3).

The extension's UI consists of a browser action (toolbar icon) that indicates PiCast connection status (connected/disconnected) and a popup that shows the current playback state, provides cast/stop/pause controls, and displays the PiCast device's address. The popup communicates with the PiCast WebSocket channel for real-time status updates.

### 9.2 MSE Interception (Advanced)

Modern web video players use the Media Source Extensions (MSE) API to deliver content. Instead of setting a `src` attribute on a `<video>` element, the player creates a `MediaSource` object, attaches it to the video element, and feeds media segments via `SourceBuffer.appendBuffer()`. This approach allows the player to control segment fetching, adaptive bitrate switching, and DRM-encrypted segment handling.

MSE interception is an advanced, opt-in feature of the PiCast browser extension that captures the exact media data being fed to the video element. The technique involves:

1. **Content script injection**: The extension injects a content script into the page's main world (using `world: "MAIN"` in Manifest V3). This gives the script access to the page's JavaScript context, including the `MediaSource` and `SourceBuffer` constructors.

2. **Monkey-patching `SourceBuffer.appendBuffer()`**: The content script replaces the native `appendBuffer()` method with a wrapper that intercepts the `ArrayBuffer` or `TypedArray` argument before passing it to the original method. The intercepted data is forwarded to the extension's background script via `window.postMessage()`.

3. **Segment reassembly**: The background script collects intercepted segments, identifies the container format (by probing the initial segments for ftyp/moof/trun boxes for MP4, or the 0x47 sync byte for MPEG-TS), and constructs a playable stream URL or sends segments to PiCast via a local HTTP proxy.

This technique is powerful but has significant limitations:

- **DRM-encrypted segments**: When the page uses Encrypted Media Extensions (EME), the segments captured by MSE interception are encrypted. PiCast does not support DRM decryption, so these segments are useless.
- **Content Security Policy (CSP)**: Some sites set CSP headers that block inline script injection, preventing the content script from executing in the main world.
- **Player complexity**: Sophisticated players (YouTube, Netflix) use custom segment parsing and buffer management that may not be compatible with simple interception.
- **Performance impact**: Capturing and forwarding every media segment adds overhead, which can cause playback stuttering on the browser side.

For these reasons, MSE interception is disabled by default and requires explicit user opt-in via the extension's settings. When enabled, it provides the highest-quality casting experience for sites where it works, but the fallback mode (page URL submission) remains the reliable default.

### 9.3 Extension Configuration

The browser extension exposes the following configuration options, stored via the `chrome.storage.sync` API (synced across the user's browsers):

| Setting | Type | Default | Description |
|---|---|---|---|
| `piAddress` | string | `picast.local` | Hostname or IP address of the PiCast device. The extension resolves `picast.local` via mDNS. Can be set to a raw IP (e.g., `192.168.1.100`) for networks without mDNS. |
| `torMode` | enum | `full` | Tor routing mode: `full` (all traffic through Tor), `resolution-only` (only yt-dlp resolution through Tor, media fetch direct), `off` (no Tor, direct connections). The `resolution-only` mode is useful on trusted networks where media CDN traffic need not be anonymized. |
| `preferIntercepted` | boolean | `true` | When true, intercepted media URLs are preferred over page URL submission. When false, the extension always sends the page URL for server-side yt-dlp resolution. |
| `autoCast` | boolean | `false` | When true, the extension automatically casts detected media without requiring user interaction. Useful for "always-on" casting setups. |
| `maxResolution` | enum | `720p` | Maximum resolution to request from PiCast. Options: `240p`, `360p`, `480p`, `720p`, `1080p`. The extension sends this as a hint in the `/api/cast` request's `format` field. |

The `torMode` setting is particularly important. In `full` mode, the extension instructs PiCast to route both content resolution (yt-dlp) and media fetching (souphttpsrc) through Tor. In `resolution-only` mode, only yt-dlp uses Tor — the actual media stream is fetched directly from the CDN. This reduces latency and improves bandwidth (CDN connections are typically much faster than Tor), but at the cost of exposing the user's IP address to the media CDN. The `off` mode disables Tor entirely, making PiCast function as a conventional casting device.

---

## 10. Session Management

### 10.1 State Machine

PiCast's playback engine is governed by a finite state machine with six states. All state transitions are deterministic, atomic, and broadcast to connected senders via the WebSocket channel.

| State | Description |
|---|---|
| IDLE | No active playback. Ready to accept new cast requests. |
| RESOLVING | yt-dlp is extracting the media URL. Duration: 3–15 seconds. |
| BUFFERING | GStreamer pipeline is filling the queue2 buffer. Playback has not started. |
| PLAYING | Media is actively decoding and displaying on HDMI. |
| PAUSED | Playback is paused. GStreamer pipeline is in PAUSED state. Buffer continues to fill from network. |
| ERROR | An unrecoverable error occurred. Error details are broadcast via WebSocket. |

**State transition table:**

| From | To | Trigger |
|---|---|---|
| IDLE | RESOLVING | `/api/cast` or `CAST` WebSocket message received |
| RESOLVING | BUFFERING | yt-dlp extraction succeeded, GStreamer pipeline started |
| RESOLVING | ERROR | yt-dlp extraction failed (unsupported site, network error) |
| BUFFERING | PLAYING | queue2 buffer reached high threshold (80%) |
| BUFFERING | ERROR | GStreamer pipeline error (unsupported codec, network failure) |
| PLAYING | PAUSED | `/api/pause` or `PAUSE` message |
| PLAYING | BUFFERING | Buffer level dropped below low threshold (10%) |
| PLAYING | IDLE | `/api/stop` or `STOP` message, or playback reached end of stream |
| PLAYING | ERROR | Unrecoverable decode or display error |
| PAUSED | PLAYING | `/api/pause` or `PAUSE` message (toggle) |
| PAUSED | IDLE | `/api/stop` or `STOP` message |
| ERROR | IDLE | Error acknowledged, or new `/api/cast` request |

All transitions are logged with timestamps and relevant metadata (URL, format, buffer level at time of transition). The transition from PLAYING → BUFFERING is particularly important for Tor-routed playback: it indicates that the network cannot sustain the current bitrate, and the adaptive bitrate logic (§10.3) may trigger a downscale.

### 10.2 Playback Queue

PiCast maintains a FIFO playback queue that persists across restarts via a SQLite database in WAL (Write-Ahead Logging) mode. WAL mode provides better concurrent read performance than the default rollback journal, which is important when both the playback engine and the HTTP API are reading the queue simultaneously.

Each queue entry contains:

| Field | Type | Description |
|---|---|---|
| `id` | INTEGER PRIMARY KEY | Auto-incrementing queue entry ID |
| `originalUrl` | TEXT | The URL as submitted by the sender |
| `resolvedUrl` | TEXT | The direct media URL from yt-dlp (NULL until resolved) |
| `format` | TEXT | Format specification (e.g., "720p", "best") |
| `subtitlePaths` | TEXT | JSON array of subtitle file paths |
| `resumePosition` | REAL | Playback position in seconds (0.0 if not resumed) |
| `duration` | REAL | Total duration in seconds (NULL until probed) |
| `addedAt` | TEXT | ISO 8601 timestamp when entry was added |
| `state` | TEXT | Queue entry state: pending, playing, completed, failed |

When the current playback ends (end-of-stream), PiCast automatically advances to the next pending entry in the queue. If the next entry has not yet been resolved, PiCast transitions to RESOLVING state for that entry. The queue can be manipulated via the HTTP API: `POST /api/queue` adds an entry, `DELETE /api/queue/<id>` removes an entry, `GET /api/queue` lists all entries.

Resume positions are updated every 5 seconds during playback and on state transitions (pause, stop, error). If a playback is interrupted (e.g., power loss, crash), the resume position allows PiCast to offer to resume from where it left off when the device restarts.

### 10.3 Adaptive Bitrate Over Tor

Traditional adaptive bitrate (ABR) algorithms estimate available bandwidth and select the highest quality that the estimated bandwidth can sustain. This approach fails over Tor because Tor's bandwidth is highly variable and unpredictable — a bandwidth estimate made at time T may be completely wrong at time T+1 second due to circuit congestion, relay load changes, or circuit rotation.

PiCast uses a **buffer-level ABR** strategy instead. Rather than estimating bandwidth, PiCast monitors the queue2 buffer fill level and makes quality decisions based on whether the buffer is growing, stable, or shrinking. The logic is:

- **Downscale trigger**: Buffer level drops below 15% during PLAYING state. PiCast transitions to RESOLVING for a lower-quality format of the same content. While yt-dlp re-resolves the URL (5–30 seconds), the current stream continues playing from the remaining buffer. If the buffer is exhausted before re-resolution completes, PiCast enters BUFFERING state.

- **Upscale trigger**: Buffer level has been above 70% for 30 consecutive seconds during PLAYING state. This indicates that the current Tor circuit has sufficient bandwidth for a higher quality stream. PiCast initiates background re-resolution for a higher-quality format. The switch occurs only if the new URL is successfully resolved and the buffer remains above 50%.

The re-resolution process is the main challenge. Unlike CDN-based adaptive streaming (where manifest files list all available bitrates and switching is instant), PiCast must invoke yt-dlp to get a different quality URL. This takes 5–30 seconds, during which the current stream continues playing. If the buffer is exhausted before re-resolution completes, playback stalls.

To minimize disruption, PiCast uses GStreamer's `pad-probe` mechanism to perform a **gapless source switch**: when the new URL is ready, a second `souphttpsrc` element is instantiated and connected to the pipeline in parallel. Once the new source has buffered sufficiently, the old source's pad is blocked, and the new source's pad is linked. This provides a seamless transition without a visible gap in playback — the video may briefly drop in quality (or upsample from a lower resolution) during the transition, but it does not freeze or display a loading spinner.

---

## 11. Network Security

### 11.1 LAN Access Control

PiCast's network interfaces are exposed on the local network for sender connectivity. The following iptables rules restrict access to only the necessary ports and source addresses:

```bash
# PiCast INPUT chain rules
iptables -A INPUT -i lo -j ACCEPT                                    # Loopback
iptables -A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT  # Established connections
iptables -A INPUT -s 192.168.0.0/16 -p tcp --dport 8585 -j ACCEPT   # HTTP API (LAN only)
iptables -A INPUT -s 10.0.0.0/8 -p tcp --dport 8585 -j ACCEPT       # HTTP API (LAN only)
iptables -A INPUT -s 192.168.0.0/16 -p tcp --dport 8586 -j ACCEPT   # WebSocket (LAN only)
iptables -A INPUT -s 10.0.0.0/8 -p tcp --dport 8586 -j ACCEPT       # WebSocket (LAN only)
iptables -A INPUT -s 192.168.0.0/16 -p tcp --dport 49152 -j ACCEPT  # DLNA (LAN only)
iptables -A INPUT -s 10.0.0.0/8 -p tcp --dport 49152 -j ACCEPT      # DLNA (LAN only)
iptables -A INPUT -p udp --dport 1900 -j ACCEPT                     # SSDP (multicast)
iptables -A INPUT -p udp --dport 5353 -j ACCEPT                     # mDNS (multicast)
iptables -A INPUT -j DROP                                             # Default deny
```

These rules ensure that PiCast's control interfaces (HTTP API, WebSocket, DLNA) are accessible only from RFC 1918 private network addresses (192.168.0.0/16 and 10.0.0.0/8). SSDP (port 1900/UDP) and mDNS (port 5353/UDP) are open to all local addresses for device discovery, as they use multicast and are inherently limited to the local network segment.

No interface is exposed on the public internet. PiCast is designed for trusted home and office networks; if exposed on a public network, the iptables rules prevent access from non-RFC1918 addresses, and the lack of authentication (in v1) means that any device on the same LAN can control playback. v2 adds pre-shared key authentication (see §13.4).

### 11.2 Outbound Routing

PiCast's outbound traffic is strictly filtered to ensure that all internet-bound traffic traverses Tor. The OUTPUT chain rules are:

```bash
# PiCast OUTPUT chain rules
iptables -A OUTPUT -o lo -j ACCEPT                                        # Loopback
iptables -A OUTPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT   # Established connections
iptables -A OUTPUT -d 127.0.0.1 -p tcp --dport 9050 -j ACCEPT            # Tor SOCKS
iptables -A OUTPUT -d 127.0.0.1 -p tcp --dport 9051 -j ACCEPT            # Tor control
iptables -A OUTPUT -d 192.168.0.0/16 -j ACCEPT                           # LAN (any protocol)
iptables -A OUTPUT -d 10.0.0.0/8 -j ACCEPT                               # LAN (any protocol)
iptables -A OUTPUT -m owner --uid-owner debian-tor -j ACCEPT              # Tor daemon (DNS, OR connections)
iptables -A OUTPUT -p udp --dport 5353 -j ACCEPT                         # mDNS responses
iptables -A OUTPUT -p udp --dport 1900 -j ACCEPT                         # SSDP responses
iptables -A OUTPUT -j LOG --log-prefix "PICAST-OUTPUT-DROP: "             # Log all other outbound
iptables -A OUTPUT -j DROP                                                 # Default deny
```

The critical rules are the Tor SOCKS and control port allowances (port 9050 and 9051), which are the only permitted outbound paths for application traffic. All other outbound connections — including direct HTTP/HTTPS, DNS (UDP 53), and any other protocol — are DROPped and LOGged. The `debian-tor` user exemption allows the Tor daemon itself to make outbound connections to entry relays and directory authorities, which is necessary for Tor to function.

The LOG rule provides auditability: any application that attempts to bypass Tor (due to misconfiguration, library behavior, or compromise) will generate a kernel log entry with the `PICAST-OUTPUT-DROP:` prefix, which can be monitored by PiCast's watchdog or external logging infrastructure. This defense-in-depth approach ensures that even if an application-level configuration error bypasses the SOCKS5h proxy, the kernel-level filter prevents the leak.

### 11.3 Authentication

PiCast v1 does not implement authentication. This is a deliberate design decision based on the threat model: PiCast operates on a trusted LAN (home or office network), and the risk of unauthorized control is considered acceptable in this context. This is the same trust model as the original Chromecast (which also lacked authentication in its initial release and only added authentication in later firmware versions).

The lack of authentication means that any device on the same LAN can:

- Cast media to the PiCast device
- Stop, pause, or seek during playback
- Observe playback status via the WebSocket channel
- Modify the playback queue

PiCast does not expose the filesystem, shell access, or any administrative interface over the network. The only capabilities available to a LAN attacker are media control (play/stop/pause/seek) and status observation (current URL, playback position). While annoying, these actions are not security-critical — an attacker with LAN access can already perform far more damaging actions (ARP spoofing, DNS hijacking, traffic interception) without PiCast's help.

For v2, PiCast will implement pre-shared key (PSK) authentication via QR code pairing (see §13.4). The PSK approach provides a simple, visual authentication mechanism: the PiCast device displays a QR code on the HDMI output containing the device's IP address and a randomly generated key. The user scans the QR code with the browser extension or mobile app, which stores the key and includes it in all subsequent API requests. This approach does not require a cloud service, account registration, or complex PKI infrastructure.

---

## 12. Deployment Model

### 12.1 Operating System

PiCast runs on **Raspberry Pi OS Lite 64-bit** (based on Debian bookworm). The Lite variant omits all desktop components — no X11, no Wayland, no Chromium, no desktop environment, no office suite. The resulting base image is approximately 800MB, compared to 3–4GB for the full desktop image. This reduced footprint translates directly into a smaller attack surface, lower memory consumption, and faster boot times.

The following components are explicitly removed or disabled from the base image:

| Component | Reason for Removal |
|---|---|
| X11 / Wayland | No display server needed; PiCast talks directly to DRM/KMS |
| Chromium browser | Security liability; not needed for appliance operation |
| desktop-base, lxde, etc. | Desktop environment packages; unnecessary |
| pulseaudio | PiCast uses ALSA directly for audio output |
| avahi-daemon | Replaced by PiCast's custom mDNS implementation |
| triggerhappy | Hotkey daemon; no input devices on PiCast |
| rfkill, bluetooth | Not used; reduce attack surface |

The kernel is **Linux 6.6 LTS**, the long-term support release used by Raspberry Pi OS bookworm. Two device tree overlays are required:

- **`vc4-kms-v3d`**: Enables the vc4 DRM/KMS driver (for HVS and V3D GPU) and the V3D OpenGL driver. This overlay is essential for PiCast's display stack — without it, there is no DRM device at `/dev/dri/card0`.

- **`rpivid-v4l2`**: Enables the HEVC decoder's V4L2 interface at `/dev/video19` (or similar). Although PiCast v1 does not use HEVC decode, the overlay is loaded to ensure the device node is available for future use and to prevent the HEVC hardware from consuming memory if left in an undefined state.

The `/boot/firmware/config.txt` configuration includes:

```
# PiCast configuration
dtoverlay=vc4-kms-v3d
dtoverlay=rpivid-v4l2
disable_splash=1
boot_delay=0
gpu_mem=256
```

The `gpu_mem=256` setting allocates 256MB of CMA (Contiguous Memory Allocator) memory for GPU-side buffers, including V4L2 decode output DMA-BUFs and GBM buffers for the OSD. This is sufficient for 1080p60 decode with two reference frames; lower values may cause decode failures on high-motion content.

### 12.2 Autostart

PiCast runs as a systemd service, starting automatically on boot and restarting on failure. The service unit file is:

```ini
[Unit]
Description=PiCast Media Casting Appliance
After=network-online.target tor.service
Wants=network-online.target
Requires=tor.service

[Service]
Type=simple
User=picast
Group=picast
SupplementaryGroups=video render audio
ExecStart=/usr/bin/picast-server --config /etc/picast/config.toml
Restart=on-failure
RestartSec=5
WatchdogSec=30
TimeoutStartSec=120
TimeoutStopSec=10

# Security hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/tmp/picast /var/lib/picast
DeviceAllow=/dev/dri/card0 rw
DeviceAllow=/dev/video10 rw
DeviceAllow=/dev/video11 rw
DeviceAllow=/dev/video12 rw

[Install]
WantedBy=multi-user.target
```

Key design decisions:

- **After/Requires**: The service starts after `network-online.target` and `tor.service`, ensuring that the network and Tor daemon are available before PiCast attempts to resolve URLs. The `Requires=tor.service` directive means that if the Tor daemon stops, PiCast is also stopped (it cannot function without Tor).

- **User/Group**: PiCast runs as a dedicated `picast` user, not as root. The user is a member of the `video` group (for DRM/KMS access), `render` group (for V3D GPU access), and `audio` group (for ALSA output). The `deviceAllow` directives grant access to specific device nodes.

- **WatchdogSec=30**: PiCast must notify systemd every 30 seconds via `sd_notify("WATCHDOG=1")`. If it fails to do so, systemd kills and restarts the service. This detects hangs in the GStreamer pipeline or event loop.

- **Security hardening**: `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome=true`, and `PrivateTmp=true` limit the service's ability to escalate privileges or access the filesystem beyond its designated paths. `ReadWritePaths` restricts write access to `/tmp/picast` (temporary subtitle files, session data) and `/var/lib/picast` (SQLite queue database).

### 12.3 Resource Budget

PiCast's resource consumption is carefully budgeted to ensure consistent performance and thermal stability on the Raspberry Pi 4B+. The following table summarizes the expected resource usage during 1080p60 H.264 playback:

| Resource | Budget | Typical Usage | Notes |
|---|---|---|---|
| CPU (4 cores) | Unlimited | ~3% (single core) | V4L2 decode + GStreamer overhead. Remaining capacity for yt-dlp resolution, OSD rendering, network I/O. |
| RAM | 1GB minimum, 4GB recommended | ~150MB | GStreamer buffer pools (~80MB), yt-dlp Python runtime (~30MB), OSD buffers (~10MB), OS + services (~80MB), Tor daemon (~30MB). Total ~150MB, leaving ample headroom on 4GB Pi. |
| Storage | 8GB SD card | ~800MB (OS) + ~100MB (PiCast) | No local media storage. All content is streamed. |
| Power | USB-C 5V/3A | ~5W (playback) | Idle ~2.5W, playback ~5W, peak (yt-dlp + decode + Tor) ~6W. Passive cooling sufficient at 5W. |
| Network | 10/100/1000 Ethernet | 2–4 Mbps (Tor) | Ethernet recommended over Wi-Fi for latency stability. Wi-Fi adds 1–2W power. |
| GPU (V3D) | — | <5% | OSD rendering only. Vast majority of V3D capacity is unused. |
| Temperature | — | 45–55°C (passive) | Well below 80°C throttling threshold. No active cooling required. |

The 3% CPU figure deserves elaboration. The V4L2 hardware decoder operates asynchronously — the application feeds NALUs into the OUTPUT queue and receives decoded frames from the CAPTURE queue via V4L2 poll events. The GStreamer pipeline thread spends most of its time waiting for these events, waking up only to move buffers between elements. The souphttpsrc element also operates asynchronously (via GLib main loop I/O callbacks), consuming minimal CPU for network I/O. The combined CPU usage of the entire pipeline is dominated by the h264parse element's bitstream parsing, which is lightweight for well-formed streams. The ~3% figure is measured on a single Cortex-A72 core at 1.5GHz; the other three cores are effectively idle during playback.

---

## 13. Future Work

### 13.1 HEVC Zero-Copy Pipeline (v2)

The highest-priority future work item is enabling HEVC/H.265 playback without breaking the zero-copy pipeline. As described in §3.1.2 and §4.4, the HEVC decoder's SAND column output format is incompatible with HVS direct scanout, requiring a conversion step that currently breaks zero-copy.

Two approaches are under investigation:

1. **V3D compute shader conversion**: A Vulkan compute shader that reads SAND-format data from the HEVC decoder's DMA-BUF and writes linear NV12 to a new DMA-BUF. This approach keeps data in GPU-accessible memory and avoids CPU involvement, but it requires proving that DMA-BUF import/export works correctly between the rpivid driver and the V3D GPU. The compute shader itself is relatively straightforward (SAND format is a column-interleaved layout that can be deinterleaved with a simple addressing calculation), but performance at 4Kp60 is uncertain — the V3D's 24 GFLOPS may not be sufficient for the memory-intensive conversion at 4K resolution. A proof-of-concept implementation is planned for v2 development.

2. **Kernel driver conversion**: A kernel-level SAND→NV12 conversion that uses the BCM2711's ISP hardware block or a DMA engine to perform the conversion without CPU involvement. This approach would require upstream kernel patches and is dependent on the rpivid driver's DMA-BUF exporter implementation. The Raspberry Pi kernel team has discussed this feature but no implementation timeline has been announced.

If either approach succeeds, PiCast v2 would support 4Kp60 HEVC playback over Tor (where bandwidth permits) or at least 1080p60 HEVC, significantly expanding the available content catalog. Many streaming services now default to HEVC for 1080p+ content, and some offer 4K only in HEVC. Enabling HEVC is essential for long-term content compatibility.

### 13.2 Matter Casting Protocol (v2)

Matter is an open smart home standard backed by the Connectivity Standards Alliance (CSA), with Amazon, Apple, Google, and Samsung as founding members. The Matter specification includes a casting protocol that allows devices to cast media content to Matter-compatible receivers without vendor-specific authentication or cloud dependencies.

Matter casting would address PiCast's most significant usability gap: the inability to appear as a native cast target in iOS and Android operating systems. Currently, PiCast requires users to install a browser extension or use a DLNA-compatible app, which adds friction compared to the one-tap casting experience of Chromecast or AirPlay. A Matter-certified receiver would appear in the native cast UI on Android (and potentially iOS, depending on Apple's Matter adoption), providing a zero-install casting experience.

The Matter casting protocol uses standard mDNS discovery, TLS-encrypted communication, and optional authentication via device attestation. PiCast would implement the Matter Media Player device type, which exposes play/pause/stop/seek controls and accepts media URLs — a near-perfect match for PiCast's existing architecture. The main challenge is Matter certification: the CSA requires conformance testing and certification for devices that carry the Matter logo, which may be impractical for an open-source project. However, the protocol itself is open, and uncertified implementations can interoperate with many controllers.

### 13.3 arti Integration (v2)

`arti` is the Rust-based Tor client developed by the Tor Project as a eventual successor to the C-based `tor` daemon. arti offers several advantages for PiCast:

1. **In-process operation**: arti can be embedded directly in the PiCast application as a Rust library, eliminating the need for a separate daemon process. This simplifies deployment, reduces attack surface (no inter-process communication), and eliminates the `debian-tor` user exemption in the iptables OUTPUT chain.

2. **tokio-native async**: arti uses Rust's tokio asynchronous runtime, which integrates naturally with GStreamer's GLib main loop via a custom `GSource`. This provides more efficient I/O handling than the C Tor daemon's separate event loop.

3. **Memory safety**: arti is written in Rust, which provides compile-time memory safety guarantees. The C Tor daemon has a history of memory corruption vulnerabilities (CVE-2024-{...}), and an in-process Rust client would eliminate this attack surface.

However, arti currently lacks a critical feature: **`IsolateSOCKSAuth` equivalent**. arti's stream isolation API does not support per-connection SOCKS username-based circuit assignment, which PiCast relies on for per-site stream isolation (§8.3). The Tor Project has acknowledged this gap, but no implementation timeline has been announced.

PiCast v2 will evaluate arti integration when the stream isolation API is available. Until then, the C Tor daemon remains the only option that supports PiCast's security requirements.

### 13.4 LAN Authentication (v2)

PiCast v2 will implement pre-shared key (PSK) authentication to address the v1 limitation of unauthenticated LAN access (§11.3). The planned authentication mechanism uses QR code pairing:

1. On first boot (or after a factory reset), PiCast generates a random 256-bit key and displays a QR code on the HDMI output. The QR code encodes a JSON payload containing the device's IP address (`picast.local`), port numbers, and the PSK.

2. The user scans the QR code with the browser extension (via the camera API) or a mobile companion app. The extension/app stores the PSK in its local secure storage (chrome.storage with OS keychain integration on Chrome, Keychain on macOS, libsecret on Linux).

3. All subsequent API requests include an `Authorization: Bearer <PSK>` header. PiCast verifies the PSK against its stored key before processing the request. WebSocket connections authenticate via a `AUTH` message sent immediately after connection.

4. New devices can be paired by displaying the QR code on demand (via a physical button on the Pi's GPIO header or by pressing the existing paired device's "Add device" button, which temporarily re-displays the QR code).

The QR code approach provides several advantages: it is visual and intuitive, it does not require a cloud service or account registration, the PSK is high-entropy (256 bits, immune to brute force), and the pairing process is inherently local (the PSK never traverses the internet). The main limitation is that the PSK must be re-entered on all devices if it is rotated, but for a home appliance, key rotation is infrequent (typically only when a device is lost or a guest leaves the household).

### 13.5 MSE Segment Proxy (v2)

The browser extension's MSE interception feature (§9.2) captures media segments from the browser's video player, but the captured segments must be delivered to PiCast for playback. In v1, MSE interception is limited to capturing URLs and metadata — the actual segment data is not forwarded, because there is no mechanism to stream arbitrary binary data from the extension to PiCast.

PiCast v2 will implement an **MSE Segment Proxy**: a local HTTP server running on the browser's host machine (not on the Pi) that re-serves the intercepted MSE segments. The workflow is:

1. The browser extension intercepts `SourceBuffer.appendBuffer()` calls and stores the segment data in the extension's background page.

2. The extension starts a local HTTP server on a random high port (e.g., `http://localhost:54321/`) and registers the intercepted segments at sequential URLs (e.g., `/segment/0`, `/segment/1`, ...).

3. The extension sends PiCast a special cast URL: `http://<browser-host-ip>:54321/manifest.json`, which contains a synthetic HLS or DASH manifest pointing to the local segment URLs.

4. PiCast fetches the manifest and segments from the browser host's HTTP server (which is on the LAN, not through Tor), decodes and displays the video.

This approach enables casting from sites that use token-authenticated media URLs (where the URL is only valid from the browser's IP address or with the browser's cookies). Since PiCast fetches the segments from the browser host (which is on the same LAN), the browser host's IP address and authentication tokens are presented to the media server, and the segments are re-served to PiCast locally.

The main challenge is performance: the browser host must re-serve segments at sufficient bitrate for real-time playback (2–8 Mbps for H.264), which is easily achievable on a modern computer but may be problematic on low-power devices. Additionally, the extension must manage segment lifecycle (evicting old segments to limit memory usage) and handle end-of-stream signaling.

---

*PiCast Architecture Paper v1.0 — End of Document*
