# GStreamer Pipeline Definitions

This document provides detailed, element-by-element definitions of the three GStreamer pipelines used by boGDan for media playback on the Raspberry Pi 4. Each pipeline template corresponds to a different media source type: progressive download (direct MP4/MKV/WebM), HLS adaptive streaming, and media with subtitles. All pipelines use hardware-accelerated V4L2 H.264 decoding with DMA-BUF zero-copy output to the DRM/KMS display via kmssink.

## Pipeline 1: Progressive Download (Direct MP4/MKV/WebM)

Used when the resolver classifies a URL as `Direct` (the URL points directly to a media file). `parsebin` auto-detects the container format and codec, creates separate pads for video and audio streams, and the pipeline dynamically links them to the appropriate decoder branches.

### Video Branch

```
souphttpsrc location=<URL> timeout=30 proxy=socks5://127.0.0.1:9050 \
  ! queue2 max-size-time=3000000000 max-size-buffers=0 max-size-bytes=52428800 \
          use-buffering=true min-threshold-time=500000000 \
          buffering-threshold-low=10 buffering-threshold-high=80 \
  ! parsebin \
  ! video/x-h264 \
  ! h264parse config-interval=-1 \
  ! v4l2h264dec io-mode=dmabuf capture-io-mode=dmabuf \
  ! queue max-size-time=1000000000 \
  ! kmssink driver-name=vc4 plane-id=0 can-scale=true sync=true force-modesetting=true
```

### Audio Branch

```
souphttpsrc location=<URL> timeout=30 proxy=socks5://127.0.0.1:9050 \
  ! queue2 max-size-time=3000000000 use-buffering=true \
  ! parsebin \
  ! audio/mpeg \
  ! avdec_aac \
  ! audioconvert \
  ! audioresample \
  ! volume volume=0.75 \
  ! alsasink device=hw:CARD=vc4hdmi sync=true
```

### Element Explanations

| Element | Purpose | Key Properties |
|---------|---------|----------------|
| `souphttpsrc` | HTTP/HTTPS source using libsoup | `location` (URL), `timeout` (30s), `proxy` (SOCKS5 via Tor at 127.0.0.1:9050) |
| `queue2` | Burst-absorption buffer with buffering signals | `max-size-time` (3s of data), `max-size-bytes` (50MB for Tor), `use-buffering=true`, `min-threshold-time` (0.5s before play), `buffering-threshold-low` (10%, pause below), `buffering-threshold-high` (80%, resume above) |
| `parsebin` | Auto-detect container format and demux | Autoplugs: detects MP4, MKV, WebM, AVI, FLV. Emits dynamic pads for each stream. |
| `h264parse` | H.264 bitstream parser | `config-interval=-1` (prepend SPS/PPS to every IDR frame — required for V4L2 stateful decoder) |
| `v4l2h264dec` | Hardware H.264 decoder via V4L2 M2M | `io-mode=dmabuf` (compressed input via DMA-BUF), `capture-io-mode=dmabuf` (decoded output as DMA-BUF fds — **critical for zero-copy**) |
| `kmssink` | DRM/KMS direct scanout sink | `driver-name=vc4` (select vc4 DRM driver), `plane-id=0` (primary video plane), `can-scale=true` (HVS scaling), `sync=true` (vsync), `force-modesetting=true` |
| `avdec_aac` | AAC audio decoder (libav) | Handles most MP4/M4A audio tracks |
| `audioconvert` | Audio format conversion | Ensures alsasink receives the correct sample format |
| `audioresample` | Audio sample rate conversion | Handles mismatched sample rates between source and ALSA |
| `volume` | Software volume control | `volume` property 0.0–1.0; used for both user volume control and mute |
| `alsasink` | ALSA audio output | `device=hw:CARD=vc4hdmi` (HDMI audio), `sync=true` (A/V sync against GStreamer clock) |

### V4L2 M2M Decoder Fallback Strategy

```python
# Pseudocode for decoder selection in build_pipeline()
if element_exists("v4l2h264dec"):
    if supports_format("video/x-h264"):
        use("v4l2h264dec", io_mode="dmabuf", capture_io_mode="dmabuf")  # Hardware
    else:
        use("avdec_h264")  # Software fallback
else:
    use("avdec_h264")  # No V4L2 at all (dev machine, CI environment)
```

On a development machine (x86_64 without bcm2835-codec), the pipeline automatically falls back to `avdec_h264` software decoding. This fallback is essential for testing without Pi hardware. The software decoder uses `videoconvert` instead of DMA-BUF, and `ximagesink` or `autovideosink` instead of `kmssink`.

---

## Pipeline 2: HLS Adaptive Streaming (.m3u8 Manifest)

Used when the resolver classifies a URL as `Manifest` with an HLS `.m3u8` extension. `hlsdemux` handles master playlist parsing, variant selection based on the `bandwidth` property, and segment fetching with automatic retry on network errors.

### Video Branch

```
souphttpsrc location=<M3U8_URL> timeout=30 proxy=socks5://127.0.0.1:9050 \
  ! hlsdemux bandwidth=<target_bps> \
  ! queue2 max-size-time=3000000000 use-buffering=true min-threshold-time=500000000 \
          buffering-threshold-low=10 buffering-threshold-high=80 \
  ! h264parse config-interval=-1 \
  ! v4l2h264dec io-mode=dmabuf capture-io-mode=dmabuf \
  ! queue max-size-time=1000000000 \
  ! kmssink driver-name=vc4 plane-id=0 can-scale=true sync=true
```

### Audio Branch

```
souphttpsrc location=<M3U8_URL> timeout=30 proxy=socks5://127.0.0.1:9050 \
  ! hlsdemux bandwidth=<target_bps> \
  ! queue2 max-size-time=3000000000 use-buffering=true \
  ! audio/mpeg \
  ! avdec_aac \
  ! audioconvert \
  ! audioresample \
  ! volume volume=0.75 \
  ! alsasink device=hw:CARD=vc4hdmi sync=true
```

### HLS-Specific Elements

| Element | Purpose | Notes |
|---------|---------|-------|
| `hlsdemux` | HLS manifest parser and segment fetcher | Downloads `.m3u8`, fetches segments, handles variant switching. The `bandwidth` property selects the variant from the master playlist. |

### HLS Variant Selection via Bandwidth Property

When `hlsdemux` encounters a master playlist with multiple variants (different resolutions/bitrates), it selects the variant whose bandwidth is closest to but not exceeding the `bandwidth` property value. This is the primary mechanism for ABR quality control in HLS streams.

| ABR Tier | Target Bandwidth (bps) | hlsdemux bandwidth Property |
|----------|----------------------|----------------------------|
| 360p | 800,000 | `bandwidth=800000` |
| 480p | 1,500,000 | `bandwidth=1500000` |
| 720p | 3,000,000 | `bandwidth=3000000` |
| 1080p | 5,000,000 | `bandwidth=5000000` |

The ABR controller sets the `bandwidth` property dynamically when it decides to switch tiers. For HLS streams, this is much faster (~100ms) than rebuilding the entire pipeline because `hlsdemux` simply starts fetching from a different variant URL in the same master playlist.

---

## Pipeline 3: With Subtitles

Used when the resolver provides a subtitle URL alongside the media URL. `subtitleoverlay` composites subtitle text onto the video frame before display. Note: this approach requires the video decoder to output frames to a software-accessible buffer (not DMA-BUF) so that subtitleoverlay can composite text onto it. This partially breaks the zero-copy pipeline for subtitled content. Future versions will render subtitles on the separate OSD plane via DRM Plane 1, preserving zero-copy for the video path.

### Video + Subtitle Branch

```
souphttpsrc location=<MEDIA_URL> timeout=30 proxy=socks5://127.0.0.1:9050 \
  ! queue2 max-size-time=3000000000 use-buffering=true \
  ! parsebin name=demux \
  demux. \
  ! video/x-h264 \
  ! h264parse config-interval=-1 \
  ! v4l2h264dec \
  ! queue max-size-time=1000000000 \
  ! subtitleoverlay name=overlay \
  ! videoconvert \
  ! kmssink driver-name=vc4 plane-id=0 can-scale=true sync=true
```

### Subtitle Input Branch

```
souphttpsrc location=<SUBTITLE_URL> \
  ! subparse encoding=utf8 \
  ! overlay.
```

### Audio Branch

```
demux. \
  ! audio/mpeg \
  ! avdec_aac \
  ! audioconvert \
  ! audioresample \
  ! volume volume=0.75 \
  ! alsasink device=hw:CARD=vc4hdmi sync=true
```

### Subtitle-Specific Elements

| Element | Purpose | Key Properties |
|---------|---------|----------------|
| `subtitleoverlay` | Composites subtitles onto video frames | Accepts video on sink pad and subtitles on `text-sink` pad. Renders text using Pango/Cairo. |
| `subparse` | Parses SRT/SubViewer/MicroDVD subtitle formats | `encoding=utf8` ensures correct text decoding |

### Subtitle Format Support

| Format | Extension | GStreamer Parser | Notes |
|--------|-----------|------------------|-------|
| SubRip | .srt | `subparse` | Most common; yt-dlp default output |
| WebVTT | .vtt | `subparse` | Used by HLS streams |
| SSA/ASS | .ssa, .ass | `assrender` (requires gst-plugins-bad) | Advanced styling support |
| MicroDVD | .sub | `subparse` | Legacy format |

For YouTube auto-captions (which come as JSON3 format), yt-dlp converts them to SRT before providing the subtitle URL.

### Future: OSD-Plane Subtitles

The current subtitleoverlay approach breaks zero-copy because it requires reading decoded video frames into CPU memory for text compositing. A future implementation will extract subtitle data from the GStreamer pipeline via a pad probe, pass the text to the `bogdan-display` OSD renderer, and composite it on DRM Plane 1. This preserves zero-copy for the video path (Plane 0) while rendering subtitles on a separate overlay plane (Plane 1) via the V3D GPU.

---

## Gapless Source Switching (ABR Quality Change)

When the ABR controller decides to switch quality tiers, the pipeline must switch to a new source URL without a visible glitch. There are two approaches: pipeline teardown/rebuild (v1) and uridecodebin3 gapless switch (future).

### Approach 1: Pipeline Teardown and Rebuild (v1)

1. Record current position: `position_secs = pipeline.query_position()`.
2. Stop old pipeline: `pipeline.set_state(Null)`.
3. Build new pipeline with new URL at new quality tier.
4. Seek to recorded position: `pipeline.seek(position_secs)`.
5. Set to Playing.

**Drawback**: there is a brief black frame during the switch (~200–400ms for cached URLs, ~10s for URLs requiring yt-dlp re-resolution).

### Approach 2: Gapless with uridecodebin3 (Future)

```
uridecodebin3 uri=<URL> name=src \
  src. ! video/x-h264 ! v4l2h264dec ! queue ! kmssink driver-name=vc4 \
  src. ! audio/mpeg ! avdec_aac ! audioconvert ! alsasink

# On "about-to-finish" signal:
src.emit("setup-source", new_uri)
```

`uridecodebin3` supports the `about-to-finish` signal which fires before the current source runs out of data, allowing a seamless switch to the next URL. This provides truly gapless transitions with no visible glitch. Requires HLS master playlist support in yt-dlp output.

---

## Audio/Video Synchronization

Synchronization is handled by GStreamer's clock system. Key settings:

- `sync=true` on both `kmssink` and `alsasink` — ensures both sinks render at the correct clock time, maintaining lip sync.
- GStreamer's default clock is the system monotonic clock (`CLOCK_MONOTONIC`).
- The `queue2` `use-buffering=true` property causes the pipeline to pause when the buffer runs low and resume when it fills up, which naturally keeps A/V in sync.

### Latency Tuning

For low-latency live streams (e.g., Twitch), reduce buffering:

```
souphttpsrc latency=0 \
  ! hlsdemux \
  ! queue max-size-time=0 max-size-buffers=1 \
  ! v4l2h264dec \
  ! kmssink sync=true latency=0
```

This reduces latency at the cost of increased buffering risk. Not recommended for on-demand content where latency is irrelevant.

---

## GStreamer Debugging

```bash
# Show pipeline graph as image (requires graphviz)
GST_DEBUG_DUMP_DOT_DIR=/tmp GST_DEBUG=GST_TRACER:7 bogdan
dot -Tpng /tmp/bogdan.*.dot > pipeline.png

# Verbose element-level logging
GST_DEBUG=2,souphttpsrc:5,hlsdemux:5,v4l2h264dec:5,kmssink:5 bogdan

# Check if V4L2 M2M elements are available
gst-inspect-1.0 v4l2h264dec
gst-inspect-1.0 kmssink

# Test pipeline manually (H.264 progressive)
gst-launch-1.0 souphttpsrc location=<URL> ! parsebin ! v4l2h264dec io-mode=dmabuf capture-io-mode=dmabuf ! kmssink driver-name=vc4

# Test pipeline manually (HLS)
gst-launch-1.0 souphttpsrc location=<M3U8_URL> ! hlsdemux ! queue2 use-buffering=true ! v4l2h264dec ! kmssink driver-name=vc4

# Monitor buffer fill percentage
GST_DEBUG=queue2:5 bogdan 2>&1 | grep buffering
```
