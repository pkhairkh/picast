# BP-ADR-003: V4L2 stateful H.264 decoder in zero-copy DMA-BUF pipeline

| Field        | Value          |
|--------------|----------------|
| **ID**       | BP-ADR-003        |
| **Status**   | PROPOSED       |
| **Date**     | 2026-07-30     |


| **Related** | ADR-003 (GStreamer over mpv), ADR-009 (HEVC deferred) |

## Context

Problem [[P-003]] requires 1080p H.264 playback at 30 fps+ with < 50% CPU usage and a verified zero-copy pipeline. The BCM2711 SoC has a hardware H.264 decoder exposed via the V4L2 stateful M2M API; software decode tops out at ~30 fps and overheats the Pi. The challenge is wiring the decoder into a zero-copy DMA-BUF pipeline so the CPU never touches decoded pixels. ADR-003 already chose GStreamer as the media engine, and ADR-009 deferred HEVC because its SAND128 output format is incompatible with the HVS — this ADR elaborates the H.264 path that does work.

## Decision

boGDan uses the V4L2 stateful M2M decoder (`v4l2h264dec`) in DMA-BUF export mode, fed by GStreamer `parsebin` which auto-detects container and codec and builds the decode chain in a pad-added callback. For pixel-format conversion the bcm2835-ISP (`v4l2convert`) is used so the HVS can scan out the buffer. The decode chain is: `appsrc → queue2 → parsebin → (pad-added) → queue → v4l2h264dec → v4l2convert → kmssink`. Decoded frames are DMA-BUF file descriptors imported directly by `kmssink` into DRM plane 0 — the CPU is entirely uninvolved in the display data path. Target: 1080p60 at < 50% CPU.

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ 1080p60 at < 50% CPU | Hardware decoder plus zero-copy DMA-BUF pipeline meets P-003 success metric |
| ✅ Verified zero-copy | `v4l2-ctl` shows buffer passthrough; no memcpy in the decode→display path |
| ✅ Container / codec auto-detection | `parsebin` handles MP4, WebM, MKV, and TS without per-format branching in boGDan code |
| ✅ Audio decode in parallel | `parsebin` also routes audio pads to `avdec_aac → audioconvert → alsasink`, so a/v sync is handled by GStreamer |
| ❌ HEVC still unsupported in v1 | V4L2 stateless HEVC outputs SAND128 the HVS cannot display; the V3D compute shader fix (SAND→NV12) is unfinished — see ADR-009 |
| ❌ V4L2 stateful API fragility | Stateful decoders are sensitive to stream anomalies (missing SEI, broken GOPs); recovery requires `flush` events which can stall the pipeline |
| ❌ Format selection in resolver | Resolver must force H.264 via yt-dlp format string, since hardware decode is H.264-only in v1; user cannot request HEVC even on supported sources |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **V4L2 stateless decoder for H.264 (v4l2slh264dec)** | Requires per-slice header parsing in userspace; less mature than stateful on BCM2711; offers no throughput advantage at 1080p60 because the stateful path already meets the target |
| **Software decode (avdec_h264)** | Tops out at ~30 fps at 1080p on Pi 4 and overheats the SoC; kept only as a fallback for codecs the hardware cannot handle (e.g. VP9 if it ever needs supporting) |
| **Broadcom MMAL decoder (legacy)** | Deprecated on BCM2711; no DMA-BUF export; would break the zero-copy guarantee and lock the project to an unmaintained stack |
