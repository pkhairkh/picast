# bogdan-playback

Constructs and controls GStreamer pipelines for media playback on the Raspberry Pi 4, including buffer monitoring, gapless source switching for ABR quality changes, and subtitle overlay. This crate translates high-level commands (play, pause, seek, stop) into GStreamer pipeline state changes and provides buffer fill telemetry to the ABR controller.

## Purpose

The playback crate is the media engine of boGDan. It builds GStreamer pipelines with hardware-accelerated V4L2 H.264 decoding and zero-copy DRM/KMS display output via kmssink. It handles three distinct pipeline topologies (progressive, HLS, and with-subtitles), manages the V4L2 decoder's DMA-BUF output mode, monitors the `queue2` buffering element for ABR decisions, and coordinates with `bogdan-display` to ensure kmssink uses the correct DRM plane and CRTC. All GStreamer threading concerns are encapsulated within this crate — external callers never touch GStreamer objects directly.

## Public API

| Item | Kind | Description |
|------|------|-------------|
| `PlaybackEngine` | struct | Implements `PlaybackTrait`; owns the GStreamer pipeline and display handle |
| `PlaybackEngine::new(display, buffer_duration_ms)` | constructor | Initialize GStreamer (`gst_init`), set buffer target duration |
| `PlaybackError` | enum | Pipeline construction / state change / seek / buffer errors |
| `MediaType` | enum | `Progressive`, `Hls`, `Dash` — determines pipeline topology |
| `PipelineHandle` | struct | Wrapper around `gst::Pipeline` with `RwLock` for thread safety |

The `PlaybackEngine` struct implements `bogdan_session::interfaces::PlaybackTrait`:

| Method | Description |
|--------|-------------|
| `play(url)` | Build pipeline (if not built), set to PLAYING |
| `pause()` | Set pipeline to PAUSED |
| `resume()` | Set pipeline back to PLAYING |
| `stop()` | Set pipeline to NULL, release all GStreamer resources |
| `seek(position_ms)` | Seek to absolute position in milliseconds |
| `set_volume(volume)` | Set volume 0.0–1.0 on the `volume` GStreamer element |
| `position_ms()` | Return current position in milliseconds via `gst::Pipeline::query_position()` |

Additional methods for ABR integration:

| Method | Description |
|--------|-------------|
| `buffer_fill()` | Return buffer fill ratio 0.0–1.0 from `queue2` buffering query |
| `current_media_type()` | Return the `MediaType` of the active pipeline |
| `switch_source(url, position_ms)` | Gapless source switch: stop old pipeline, build new, seek to position |

## Dependencies

| Dependency | Why |
|------------|-----|
| `bogdan-session` | Provides `PlaybackTrait` trait definition and `MediaType` enum |
| `bogdan-display` | Provides DRM FD and plane info for kmssink configuration |
| `gstreamer` | Core GStreamer bindings (pipeline construction, state management, bus messages) |
| `gstreamer-video` | Video-specific utilities (caps, format negotiation) |
| `gstreamer-app` | `appsrc`/`appsink` for custom data flow (future: subtitle injection) |
| `gstreamer-base` | Base classes for element configuration |
| `tokio` | Async runtime (pipeline state changes are fast enough to be sync, but trait requires async) |
| `tracing` | Debug logging for pipeline construction and state transitions |

## Pipeline Templates

### 1. Progressive Download (MP4 / MKV / WebM Direct URL)

Used for direct media URLs (classified as `Direct` by the resolver). `parsebin` auto-detects the container format and codec, creates separate pads for video and audio streams, and the pipeline dynamically links them to the appropriate decoder branches.

```
souphttpsrc location=<URL> proxy=socks5://127.0.0.1:9050 timeout=30
    │
    ▼
queue2 max-size-time=3000000000 max-size-buffers=0 max-size-bytes=0
       use-buffering=true min-threshold-time=500000000
    │
    ▼
parsebin  (auto-detects container + codec, emits dynamic pads)
    │
    ├─ video/ pad ──▶ queue ──▶ v4l2h264dec io-mode=dmabuf capture-io-mode=dmabuf
    │                                         │
    │                                         ▼
    │                              kmssink driver-name=vc4 plane-id=0 can-scale=true
    │
    └─ audio/ pad ──▶ queue ──▶ avdec_aac ──▶ audioconvert ──▶ audioresample
                                                                           │
                                                                           ▼
                                                                    alsasink device=hw:CARD=vc4hdmi sync=true
```

### 2. HLS Adaptive Streaming (.m3u8 Manifest)

Used for HLS manifests (classified as `Manifest` by the resolver). `hlsdemux` handles master playlist parsing, variant selection based on the `bandwidth` property, and segment fetching. It emits separate pads for each stream (video, audio) as they are parsed from the manifest.

```
souphttpsrc location=<M3U8_URL> proxy=socks5://127.0.0.1:9050 timeout=30
    │
    ▼
hlsdemux bandwidth=<target_bps>    ← ABR controller sets this property
    │
    ├─ video/ pad ──▶ queue2 max-size-time=3000000000 use-buffering=true
    │                          │
    │                          ▼
    │                  h264parse config-interval=-1
    │                          │
    │                          ▼
    │                  v4l2h264dec io-mode=dmabuf capture-io-mode=dmabuf
    │                          │
    │                          ▼
    │                  kmssink driver-name=vc4 plane-id=0 can-scale=true
    │
    └─ audio/ pad ──▶ queue ──▶ avdec_aac ──▶ audioconvert ──▶ audioresample
                                                                           │
                                                                           ▼
                                                                    alsasink device=hw:CARD=vc4hdmi sync=true
```

### 3. With Subtitles

Used when the resolver provides a subtitle URL alongside the media URL. `subtitleoverlay` composites subtitle text onto the video frame. Note: this approach requires a video frame copy to composite the subtitle, which partially breaks zero-copy. For v1, this is acceptable because subtitles are low-frequency updates (1–2 per second). Future versions will render subtitles on the separate OSD plane via DRM Plane 1.

```
souphttpsrc location=<MEDIA_URL> proxy=socks5://127.0.0.1:9050     souphttpsrc location=<SUB_URL>
    │                                                                   │
    ▼                                                                   ▼
parsebin                                                            subparse encoding=utf8
    │                                                                   │
    ├─ video/ ──▶ h264parse ──▶ v4l2h264dec ──▶ subtitleoverlay ──▶ kmssink
    │                                               ▲
    └─ audio/ ──▶ avdec_aac ──▶ audioconvert ──▶ alsasink    │
                                                                │
                                                    subparse text-sink pad ◀┘
```

## Buffer Configuration Table

| Parameter | Value | Rationale |
|-----------|-------|-----------|
| `max-size-time` | 3,000,000,000 ns (3 s) default; configurable up to 30 s | 3 s for local files; 30 s for Tor-routed streams where bandwidth is variable (2–5 Mbps). GStreamer uses time as the primary limiting factor. |
| `max-size-buffers` | 0 (unlimited) | Let time be the limiting factor, not buffer count |
| `max-size-bytes` | 52,428,800 (50 MB) for Tor streams | 50 MB provides 30–60 seconds of buffer at 720p over Tor (2–4 Mbps). This is the critical parameter for smooth Tor playback. |
| `min-threshold-time` | 500,000,000 ns (0.5 s) | Start playing as soon as 0.5 s is buffered for responsive start |
| `buffering-threshold-low` | 10 (%) | Below 10%, pause pipeline and buffer |
| `buffering-threshold-high` | 80 (%) | Above 80%, resume playback from buffering |
| `use-buffering` | true | Emits buffering messages on the GStreamer bus for ABR monitoring |

## ABR Integration with SessionManager

1. `PlaybackEngine::buffer_fill()` queries `GstQueryBuffering` on the `queue2` element and returns the fill percentage as a 0.0–1.0 ratio.

2. `SessionManager` calls `abr_update(fill, time)` every 1 second via a background task.

3. When ABR decides to switch tier:
   - `PlaybackEngine::switch_source(new_url, position)` is called.
   - The old pipeline is set to `State::Null` and dropped (releasing V4L2 decoder buffers and DRM framebuffers).
   - A new pipeline is constructed with the new URL at the new quality tier.
   - `seek(position)` is called to resume from the saved position.
   - Total switch time: ~400 ms (cached URL) or ~10 s (requires yt-dlp re-resolution).

4. For HLS streams, a faster path exists: `hlsdemux`'s `bandwidth` property can be changed without rebuilding the pipeline, causing it to switch to a different variant in the master playlist. This achieves near-gapless switching in ~100 ms.

**Future improvement**: Use GStreamer's `uridecodebin3` with `about-to-finish` signal for true gapless switching without a visible glitch. This requires HLS master playlist support in yt-dlp output and is planned for v2.

## Implementation Guide for AI Agents

1. **Pipeline construction** — the `build_pipeline()` method is the most complex part. Start with the Progressive template and verify it works with a local MP4 file on the Pi before tackling HLS. Use `gst::Pipeline::new()` and `gst::ElementFactory::make()` to construct each element. Link elements with `gst::Element::link()`.

2. **Dynamic pad linking** — `parsebin` and `hlsdemux` create pads at runtime (you don't know the stream layout until the manifest or container header is parsed). Connect to the `pad-added` signal, inspect the new pad's caps, and link to the correct branch (video → V4L2 decoder, audio → AAC decoder). Test with both video-only and video+audio files.

3. **V4L2 M2M decoder** — `v4l2h264dec` only works on Linux with the BCM2711 V4L2 stateful decoder (`bcm2835-codec` driver at `/dev/video10`). On a dev machine (x86_64), the fallback to `avdec_h264` (software decoder) is essential. Always include the fallback in pipeline construction: try `v4l2h264dec` first, fall back to `avdec_h264` if the element is not available.

4. **DMA-BUF mode** — set `io-mode=dmabuf` and `capture-io-mode=dmabuf` on `v4l2h264dec`. Without `capture-io-mode=dmabuf`, the decoder allocates system memory buffers and the zero-copy path is broken. This is the single most important property for boGDan's performance.

5. **Buffer query** — use `GstQueryBuffering` to read the current buffer fill percentage. Convert to 0.0–1.0 ratio. Test with network-throttled streams (`--limit-rate` on a local HTTP server) to verify the buffering logic triggers correctly.

6. **Error handling** — listen to the GStreamer bus (`gst::Bus::add_watch()`) for error messages, buffering messages, and EOS (end-of-stream). Forward errors to `SessionManager` via a channel so the protocol servers can notify the sender. Handle `GST_MESSAGE_BUFFERING` to pause/resume the pipeline.

7. **Pipeline cleanup** — always set the pipeline to `State::Null` before dropping it. GStreamer doesn't free V4L2 decoder buffers or DRM framebuffers on drop — they must be explicitly released via the state change. Use a `Drop` guard that calls `set_state(State::Null)`.

## Key Constraints

- **kmssink requires DRM master**: only one process can be DRM master. If the display manager has already opened `/dev/dri/card0`, kmssink will fail to acquire the plane. Coordinate with `bogdan-display` to share the DRM FD or ensure no other process is DRM master.

- **GStreamer threading**: GStreamer creates its own threads internally (streaming threads, the bus thread, pad task threads). Do not call GStreamer methods from multiple tokio tasks without a `Mutex` or `RwLock`. The `RwLock<PipelineHandle>` protects against concurrent access.

- **V4L2 decoder limitations**: the BCM2711 H.264 decoder supports profiles Baseline, Main, and High up to Level 4.2 (1080p60). It does NOT support 10-bit depth (High 10 Profile), 4:2:2 chroma subsampling, or interlaced content (MBAFF). If yt-dlp returns a 10-bit stream, the decoder will fail — the format selection string must exclude 10-bit formats.

- **ALSA device**: the Pi's HDMI audio output is `hw:CARD=vc4hdmi` (not `default`). The `alsasink` device property must be set correctly for the user's output. If the user has connected the Pi to a monitor via HDMI, audio goes to `hw:CARD=vc4hdmi`. If using the 3.5mm jack, it's `hw:CARD=Headphones`. This should be configurable.

- **Pipeline cleanup is mandatory**: always set the pipeline to `State::Null` before dropping it. GStreamer doesn't free resources on drop — V4L2 decoder buffers, DRM framebuffers, and ALSA device handles will leak if the pipeline is dropped without proper cleanup.

- **h264parse config-interval**: set `config-interval=-1` on `h264parse` to ensure SPS/PPS NALUs are prepended to every keyframe. Some streaming formats (notably raw H.264 over HTTP) send SPS/PPS only once, but the stateful V4L2 decoder requires them before every IDR frame.

## Reference

| Resource | Location |
|----------|----------|
| ADR-003: GStreamer over mpv | `DECISIONS.md` / `SPECIFICATION.md` §1.3 |
| GStreamer pipeline templates | `docs/playback/gstreamer-pipeline.md` |
| ABR controller algorithm | `docs/playback/abr-controller.md` |
| V4L2 M2M pipeline details | `docs/hardware/v4l2-pipeline.md` |
| Zero-copy pipeline architecture | `ARCHITECTURE.md` §4 |
| Format support matrix | `SPECIFICATION.md` §3 |
