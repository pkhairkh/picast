# GStreamer Pipeline Definitions

This document describes the GStreamer pipelines used by boGDan for media playback on the Raspberry Pi 4. The pipeline is constructed dynamically at runtime based on the detected codec — the primary path uses progressive download via `appsrc` + `StreamSource` (not `souphttpsrc` for CDN URLs).

## Architecture: Progressive Download via appsrc

CDN URLs are fetched through boGDan's progressive download architecture, not through GStreamer's `souphttpsrc`:

```
CDN → Tor → SOCKS Forwarder → reqwest (HTTP/2 + rustls) → bounded channel → appsrc → queue2 → parsebin
```

**Why not souphttpsrc?** `souphttpsrc` uses HTTP/1.1 + GnuTLS, which CDN anti-bot systems flag as non-browser. The `reqwest` client uses HTTP/2 + rustls TLS, matching Chrome's fingerprint and eliminating 403 Forbidden errors from CDN TLS/HTTP fingerprinting.

**SOCKS Forwarder:** A local SOCKS5→SOCKS5h forwarder routes reqwest through Tor with the same isolation username as the resolver. Same SOCKS5 username = same Tor circuit = same exit IP, so CDN IP-bound tokens match.

**CDN Preflight Check:** Before starting the full download, boGDan performs a GET request with `Range: bytes=0-0` (not HEAD — many CDNs return 404 for HEAD). This verifies the CDN accepts the URL and returns the file size via `Content-Range`. If the CDN URL has a speed-limit parameter (`sp=380`), bypass URLs are tried first (sp=99999, sp= stripped). If all bypasses fail with 403, the original rate-limited URL is used as fallback.

---

## Pipeline 1: H.264 Progressive Download (Primary Path)

Used for CDN URLs resolved by the resolver. `StreamSource` downloads data through Tor via reqwest, feeds it into `appsrc`, which pushes into the GStreamer pipeline.

### Complete Pipeline Topology

```
┌──────────┐    ┌────────┐    ┌──────────┐    ┌───────┐    ┌────────────────────┐    ┌────────────┐    ┌──────────────┐
│appsrc    │───►│queue2  │───►│parsebin  │──┬►│queue  │───►│v4l2h264dec(dmabuf) │───►│v4l2convert │───►│kmssink       │
│(Stream   │    │(buffer)│    │(demux)   │  │ │(200buf│    │(zero-copy HW dec)  │    │(ISP:       │    │(DRM/KMS,     │
│ Source)  │    │        │    │          │  │ │ 5s)   │    │                    │    │SAND→NV12)  │    │ max-lateness) │
└──────────┘    └────────┘    └──────────┘  │ └───────┘    └────────────────────┘    └────────────┘    └──────────────┘
                                               │
                                               │ ┌───────┐    ┌──────────────┐    ┌──────────────┐    ┌────────┐    ┌─────────────────────┐
                                               └►│queue  │───►│avdec_aac     │───►│audioresample │───►│volume │───►│alsasink / pulsesink │
                                                 │(50buf │    │(or fdkaacdec)│    │              │    │        │    │(ts-offset=+100ms,   │
                                                 │ 2s max)│    │              │    │              │    │        │    │ device=plughw:C,D) │
                                                 └───────┘    └──────────────┘    └──────────────┘    └────────┘    └─────────────────────┘
```

### Source Element Selection

The source element depends on the URL type and proxy configuration:

| Condition | Source Element | Reason |
|-----------|---------------|--------|
| CDN URL + Tor proxy + isolation username | `appsrc` + `StreamSource` | Progressive download through Tor with preflight check |
| Loopback URL (`127.0.0.1` / `localhost`) | `souphttpsrc` | No Tor needed, direct connection |
| No Tor proxy configured | `souphttpsrc` | Direct CDN access, no Tor routing |
| No isolation username | `souphttpsrc` | Direct CDN access (warning logged) |

### Element Explanations

| Element | Purpose | Key Properties |
|---------|---------|----------------|
| `appsrc` | Pushes downloaded data into the GStreamer pipeline | `stream-type=stream`, `format=bytes`, `is-live=false`, `block=true` (flow control) |
| `queue2` | Burst-absorption buffer with buffering signals | Standard: 400 MB / 300s / 95% high. Rate-limited: 500 MB / 600s / 99% high |
| `parsebin` | Auto-detect container format and demux | Emits dynamic pads for each stream (video/audio). Includes internal parsers (h264parse, h265parse) |
| `v4l2h264dec` | Hardware H.264 decoder via V4L2 M2M | `io-mode=dmabuf`, `capture-io-mode=dmabuf` (zero-copy DMA-BUF output) |
| `v4l2convert` | Format conversion via bcm2835-ISP hardware | `output-io-mode=dmabuf`, `capture-io-mode=dmabuf`. Converts SAND128→NV12 for HEVC, or acts as passthrough for H.264 NV12 |
| `kmssink` | DRM/KMS direct scanout sink | `driver-name=vc4`, `can-scale=true`, `force-modesetting=true`, `async=false` (avoids preroll deadlock) |
| `avdec_aac` | AAC audio decoder (libav) | Fallback: `fdkaacdec`. If neither available, audio is disabled (video still works) |
| `audioconvert` | Audio format conversion | Ensures sink receives correct sample format |
| `audioresample` | Audio sample rate conversion | Handles mismatched sample rates |
| `volume` | Software volume control | `volume` property 0.0–1.0; used for user volume and mute |
| `alsasink` / `pulsesink` | Audio output | `ts-offset=+100ms` (A/V sync compensation for V4L2 decode latency). `device` configurable |

### Buffering Strategy

queue2 uses adaptive buffering based on whether the CDN stream is rate-limited:

| Profile | max-size-bytes | max-size-time | high-percent | low-percent | When |
|---------|---------------|---------------|-------------|------------|------|
| **Standard** | 400 MB | 300s | 95% | 10% | CDN has no speed limit |
| **Rate-limited** | 500 MB | 600s | 99% | 5% | CDN has `sp=` parameter (speed cap) |

Rate-limited streams have a fixed ceiling on download speed, so the buffer drains faster than it fills once playback starts. The rate-limited profile maximizes play time before rebuffering.

---

## Pipeline 2: HEVC/H.265 (Behind `hevc` Feature Flag)

When HEVC content is detected by parsebin (media type contains `h265` or `hevc`), the video decode chain is built dynamically:

```
parsebin video pad → queue → v4l2slh265dec → v4l2convert(ISP: SAND→NV12) → kmssink
```

**Key differences from H.264 path:**
- `v4l2slh265dec` is a stateless V4L2 decoder (not stateful like v4l2h264dec)
- Does NOT have `output-io-mode` / `capture-io-mode` properties — DMA-BUF I/O is auto-negotiated
- `v4l2convert` (bcm2835-ISP) is **required** to convert SAND128 (NC12) output to NV12 for kmssink
- H.264 path with v4l2convert also works — it acts as a passthrough when input is already NV12

---

## Pipeline 3: Software Decode Fallback

If V4L2 hardware decode fails to negotiate (e.g., non-H.264/HEVC codec, or running on x86_64 without bcm2835-codec), the pipeline falls back to software decode:

```
appsrc → queue2 → parsebin → queue → avdec_h264 → videoconvert → kmssink
```

This fallback is essential for testing without Pi hardware. Software decode uses `videoconvert` instead of `v4l2convert`, and `ximagesink` or `autovideosink` instead of `kmssink` on development machines.

---

## Dynamic Video Chain Construction

The video decode chain is NOT built at pipeline construction time. Instead, `parsebin`'s `pad-added` signal triggers dynamic chain creation based on the detected codec:

1. parsebin discovers a video stream and creates a source pad
2. The pad-added callback inspects the pad's caps (or template caps if negotiation isn't complete)
3. Based on the media type:
   - `video/x-h264` → creates: `queue → v4l2h264dec → v4l2convert → kmssink`
   - `video/x-h265` (with `hevc` feature) → creates: `queue → v4l2slh265dec → v4l2convert → kmssink`
   - Other → falls back to software decode: `queue → avdec_* → videoconvert → kmssink`
4. Elements are added to the pipeline and linked dynamically

This approach avoids caps mismatch errors that occurred when pre-built HEVC bins received H.264 data.

**Important:** When checking pad caps, `current_caps()` may return `None` for the first pad (before negotiation completes). The callback falls back to `query_caps(None)` (template caps) to determine the media type.

---

## Audio Pipeline

The audio chain is pre-built at pipeline construction time (unlike video, which is dynamic):

```
audio_queue(max 50 buffers, 2s) → avdec_aac → audioconvert → audioresample → volume → alsasink/pulsesink
```

**AAC decoder selection:** `avdec_aac` (from gst-libav) is preferred. If unavailable, `fdkaacdec` is tried. If neither exists, the audio decoder is omitted and a fakesink is attached to the audio pad in the parsebin callback (video still works).

**Audio/Video sync compensation:** The audio sink has `ts-offset=+100ms` (configurable) to delay audio rendering, compensating for V4L2 hardware decode latency (v4l2h264dec: 2-4 capture buffers at 25fps = 80-160ms, v4l2convert ISP: ~40ms). Without this, audio plays ahead of video (lip-sync desync).

**Bluetooth audio:** When `audio_sink` is set to `pulsesink`, the `device` property sets the PulseAudio sink name. If empty, pulsesink auto-routes to the default sink (which can be a Bluetooth device if configured in PulseAudio).

---

## CDN Rate Limit Handling

Many video CDNs (e.g. Voe) embed a speed-limit token as `&sp=NNN` in the URL, where NNN is the maximum download speed in kbps. When `sp=380`, throughput is capped at ~380 kbps.

### Bypass Strategy (in `stream_source.rs`)

1. **sp=99999**: Replace the speed limit with a very high value. The CDN may accept it, effectively removing the cap while keeping the required parameter present.
2. **sp= stripped**: Remove the parameter entirely. Usually results in 403 from the CDN (the `sp=` value is part of the signed URL token), but tried as a last resort.
3. **Original URL fallback**: If all bypass URLs return 403, fall back to the original rate-limited URL. The CDN generated this URL and should accept it for the correct exit IP.
4. **Re-resolve on 403 from original URL**: If even the original URL returns 403, this indicates an IP block. The session layer re-resolves through a different Tor circuit.

### Preflight Check

Before starting playback, boGDan performs a CDN preflight check:

- **Method**: GET with `Range: bytes=0-0` (NOT HEAD — many CDNs return 404 for HEAD on download URLs)
- **Expected response**: 206 Partial Content (CDN supports Range) or 200 OK (CDN ignored Range)
- **File size extraction**: From `Content-Range: bytes 0-0/<total>` (206) or `Content-Length` (200)
- **Bitrate mismatch warning**: If CDN rate limit < estimated video bitrate, logs a warning that playback will stutter

---

## GStreamer Debugging

```bash
# Show pipeline graph as image (requires graphviz)
GST_DEBUG_DUMP_DOT_DIR=/tmp GST_DEBUG=GST_TRACER:7 bogdan-server
dot -Tpng /tmp/bogdan.*.dot > pipeline.png

# Verbose element-level logging
GST_DEBUG=2,appsrc:5,parsebin:5,v4l2h264dec:5,kmssink:5 bogdan-server

# Check if V4L2 M2M elements are available
gst-inspect-1.0 v4l2h264dec
gst-inspect-1.0 kmssink

# Test pipeline manually (H.264 progressive via appsrc — not possible with gst-launch)
# Use the boGDan HTTP API instead:
curl -X POST http://pi:8585/api/cast -H 'Content-Type: application/json' -d '{"url":"https://example.com/video.mp4"}'

# Monitor buffer fill percentage
GST_DEBUG=queue2:5 bogdan-server 2>&1 | grep buffering
```
