# ADR-003: GStreamer Over mpv

| Field        | Value          |
|--------------|----------------|
| **ID**       | ADR-003        |
| **Status**   | ACCEPTED       |
| **Date**     | 2025-01-16     |
| **Supersedes** | —            |
| **Superseded by** | —         |

## Context

PiCast needs a media playback framework that can:

1. **Hardware-decode H.264** on Pi 4 via V4L2 Memory-to-Memory (M2M) API
2. **Zero-copy display** via DRM/KMS atomic modesetting with DMA-BUF passing
3. **Adaptive bitrate (ABR)** over Tor SOCKS5 — Tor circuits have unpredictable bandwidth (0.5–5 Mbps), requiring runtime quality switching
4. **Pipeline control** — PiCast needs programmatic control over buffer sizes, queue thresholds, and sink configuration for Tor-specific optimization

The two primary candidates for Linux media playback are **GStreamer** and **mpv** (libmpv). Both are mature, well-maintained, and support a wide range of codecs.

### mpv Assessment

mpv is an excellent media player with a clean CLI and powerful Lua scripting. However:

- **No HW decode on Pi 4 DRM**: mpv's `--vo=drm` output driver renders via software blitting to a dumb DRM buffer. It does not use V4L2 M2M for decoding on Pi 4. The `--hwdec=drm` and `--hwdec=v4l2m2m` options exist but are unreliable on Pi 4 — they rely on FFmpeg's V4L2 M2M wrapper, which has known issues with DMA-BUF export and format negotiation on BCM2711.
- **No kmssink equivalent**: mpv has no equivalent to GStreamer's `kmssink` element, which directly programs a DRM plane via atomic modesetting. mpv's DRM output goes through a shadow buffer copy.
- **Limited ABR control**: mpv supports `--ytdl-format` for initial format selection but does not provide runtime adaptive bitrate switching. Switching quality mid-stream requires stopping and restarting playback.
- **Pipeline opacity**: mpv's internal pipeline is not programmatically composable. You cannot insert custom queue elements, tee branches, or pad probes without modifying mpv source code.

### GStreamer Assessment

GStreamer is a pipeline-based framework where every element is composable:

- **V4L2 M2M integration**: The `v4l2h264dec` element in GStreamer has been tested and proven on Pi 4. It correctly negotiates DMA-BUF output and handles format conversion.
- **kmssink**: The `kmssink` element performs DRM atomic modesetting, importing DMA-BUFs directly into DRM planes. This is the zero-copy path from decoder to display.
- **queue2 ABR**: GStreamer's `queue2` element supports byte/time limits and buffering messages. PiCast can use these signals to implement adaptive bitrate: when `queue2` reports low buffering, request a lower-quality stream from yt-dlp.
- **Pad probes**: GStreamer allows attaching pad probes for buffer inspection, timing analysis, and dynamic pipeline reconfiguration — all without modifying GStreamer source.
- **Rust bindings**: The `gstreamer` crate provides idiomatic Rust bindings with full pipeline construction and bus message handling.

## Decision

PiCast uses GStreamer as its media playback framework. The `picast-playback` crate constructs the following pipeline:

```
souphttpsrc location=<url> proxy-id=<tor-socks5> !
  queue2 max-size-buffers=0 max-size-time=0 max-size-bytes=10485760 use-buffering=true !
  parsebin !
  v4l2h264dec capture-io-mode=dmabuf !
  kmssink plane-id=0 can-attach-static=true
```

- `souphttpsrc` with `proxy-id` routes through Tor SOCKS5
- `queue2` provides ABR buffering signals (10 MB buffer, buffering messages)
- `v4l2h264dec` with `capture-io-mode=dmabuf` enables zero-copy DMA-BUF output
- `kmssink` programs DRM plane 0 directly

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ Proven V4L2 M2M on Pi 4 | `v4l2h264dec` is the stable, tested path for H.264 hardware decode on BCM2711 |
| ✅ Zero-copy display | DMA-BUF from decoder → kmssink → HVS scanout; no CPU copy |
| ✅ ABR control | `queue2` buffering messages enable runtime quality switching over Tor |
| ✅ Programmatic pipeline | Rust crate allows full pipeline construction, pad probes, and dynamic reconfiguration |
| ✅ Broad format support | GStreamer parsebin auto-detects container formats (MP4, WebM, MKV, TS) |
| ❌ GStreamer dependency size | GStreamer + plugins ≈ 30–50 MB on disk; mpv/FFmpeg is slightly smaller |
| ❌ GStreamer version sensitivity | V4L2 M2M improvements require GStreamer ≥ 1.22; older Raspbian may ship 1.20 |
| ❌ Pipeline complexity | Constructing correct GStreamer pipelines in Rust requires understanding element properties, caps negotiation, and bus message handling |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **mpv** | No V4L2 M2M hardware decode on Pi 4; `--vo=drm` is software blit only; no kmssink equivalent; no runtime ABR; pipeline is not programmatically composable |
| **FFmpeg + custom DRM sink** | Would require writing a custom DRM/KMS sink from scratch; FFmpeg's V4L2 M2M wrapper has DMA-BUF export issues on Pi 4; no built-in ABR; reinventing GStreamer's pipeline management |
| **Kodi as media library** | Kodi is a full media center application, not a library; it includes GUI, skin engine, and addon system — massive overhead for PiCast's use case; no programmatic Rust API; cannot be embedded as a crate |
