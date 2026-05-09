# ADR-009: HEVC Deferred to v2

| Field        | Value          |
|--------------|----------------|
| **ID**       | ADR-009        |
| **Status**   | DEFERRED       |
| **Date**     | 2025-01-22     |
| **Supersedes** | —            |
| **Superseded by** | —         |

## Context

The BCM2711 SoC in the Raspberry Pi 4B+ includes a hardware HEVC (H.265) decoder in the VideoCore VI (V3D) subsystem. This decoder is exposed via the V4L2 M2M interface as `/dev/video19` (HEVC decode) alongside `/dev/video11` (H.264 decode). On paper, this means Pi 4 can hardware-decode HEVC, which would provide:

- **~40% bitrate savings** over H.264 at equivalent quality (per MPEG testing)
- **4K playback support** — HEVC at 4K is ~15–25 Mbps; H.264 at 4K is ~30–50 Mbps, which exceeds Tor circuit bandwidth
- **Broader content availability** — Many streaming services encode in HEVC only at higher resolutions

However, there is a critical hardware limitation that prevents HEVC from working end-to-end on Pi 4 today.

### The SAND Format Problem

The BCM2711's HEVC hardware decoder outputs decoded frames in **SAND format** (also called "SAND column format" or "band format"), specifically NC12 (8-bit) and NC30 (10-bit) pixel formats. SAND is a Broadcom-proprietary tiling format that arranges pixels in column-based bands rather than the standard raster scan order.

The problem:

- **HVS cannot display SAND**: The Hardware Video Scaler (HVS) on BCM2711 — the display engine that scans out pixels to the HDMI output — only supports standard raster formats: NV12, NV21, YUYV, UYVY, and RGB formats. It cannot scan out SAND (NC12/NC30) format directly.
- **No hardware SAND→NV12 conversion**: There is no dedicated hardware block on BCM2711 that converts SAND to NV12. The ISP (Image Sensor Pipeline) can do some format conversions but does not support SAND input.
- **Software conversion is expensive**: Converting SAND→NV12 on the CPU requires touching every pixel. For 1080p30 HEVC, this adds ~40% CPU utilization on the Pi 4's Cortex-A72 cluster. For 4K30, it exceeds 100% CPU — not feasible.

### Current Workarounds and Future Prospects

Several approaches are being explored in the Pi community:

1. **GStreamer 1.26 SAND support**: Upstream GStreamer is working on SAND format awareness in `v4l2h265dec`. If the element can negotiate a SAND-compatible downstream pipeline, it might be possible to insert a software converter element. Expected in GStreamer 1.26 (late 2025).

2. **V3D compute shader conversion**: The V3D GPU on BCM2711 supports compute shaders. A compute shader could perform SAND→NV12 conversion on the GPU, freeing the CPU. This has been demonstrated experimentally but is not production-ready. Challenges include DMA-BUF import into V3D compute context and synchronization with the V4L2 decode pipeline.

3. **rpi-hevc-dec kernel conversion**: The `rpi-hevc-dec` project (Broadcom's reference HEVC decoder) includes a kernel-mode SAND→NV12 conversion option. This would perform the conversion at the kernel level, making it transparent to userspace. However, this code is not upstream and its maintenance status is uncertain.

4. **Force H.264 in yt-dlp**: The simplest immediate solution — tell yt-dlp to only request H.264 formats. This is what boGDan v1 does.

## Decision

HEVC hardware decoding is deferred to boGDan v2. For v1, the `bogdan-resolver` crate forces H.264 format selection in yt-dlp:

```
--format "bv[height<=1080][vcodec^=avc1]+ba/b[height<=1080]/bv+ba"
```

The `vcodec^=avc1` filter ensures only H.264 video streams are selected. If no H.264 stream is available at the requested resolution, yt-dlp falls back to the best available format (which might be HEVC — in that case, GStreamer will attempt software decode, which will likely fail at high resolutions, and boGDan will display an error).

This decision will be re-evaluated when **any one** of the following conditions is met:

1. **GStreamer 1.26** adds SAND format support with a `sand2nv12` conversion element, enabling a pipeline like:
   ```
   v4l2h265dec capture-io-mode=dmabuf ! sand2nv12 ! kmssink
   ```

2. **V3D compute shader** SAND→NV12 conversion is proven in production with:
   - Latency < 5ms per 1080p frame
   - No visible artifacts
   - DMA-BUF synchronization with V4L2 decoder

3. **rpi-hevc-dec kernel conversion** is upstreamed into the Raspberry Pi kernel, making SAND→NV12 transparent to userspace.

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ Reliable H.264 playback | All H.264 content plays via V4L2 M2M with zero-copy DMA-BUF display; no format conversion issues |
| ✅ No CPU overhead | No SAND→NV12 conversion required; CPU is free for Tor and yt-dlp |
| ✅ Simple pipeline | `v4l2h264dec → kmssink` is a proven, tested pipeline with no format conversion steps |
| ✅ Clear upgrade path | When SAND conversion is available, boGDan v2 can switch to HEVC by changing the yt-dlp format string and adding a conversion element to the GStreamer pipeline |
| ❌ No 4K playback | H.264 4K requires ~30–50 Mbps, exceeding typical Tor circuit bandwidth (~5 Mbps); HEVC 4K at ~15 Mbps would fit but is not available |
| ❌ Higher bandwidth for 1080p | H.264 1080p requires ~8–12 Mbps vs. HEVC 1080p at ~4–7 Mbps; on slow Tor circuits, H.264 may buffer more frequently |
| ❌ Some content unavailable | A small but growing number of streaming services only provide HEVC at 1080p+; boGDan v1 cannot play these |
| ❌ Incomplete hardware utilization | The Pi 4's HEVC decoder sits idle; hardware capability is wasted |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **Software SAND→NV12 in boGDan** | Adds ~40% CPU utilization at 1080p30; exceeds 100% CPU at 4K; CPU is already needed for Tor encryption and yt-dlp; not viable for sustained playback |
| **Force HEVC and accept software decode** | Pi 4 cannot software-decode HEVC 1080p in real-time (requires ~4x Cortex-A72 at 1.5 GHz); playback would be slideshow at best |
| **Dual-output pipeline (HEVC decode → CPU convert → GPU display)** | SAND frames in main memory → CPU convert → re-import into DRM; defeats zero-copy; adds 2+ frame latency; CPU overhead makes it impractical |
