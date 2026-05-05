# V4L2 M2M Pipeline Details

The BCM2711 exposes its hardware video decoders through the V4L2 Memory-to-Memory (M2M) API. This document covers the stateful vs stateless API models, device nodes, buffer queue workflow, DMA-BUF export, format negotiation, resolution change handling, and performance benchmarks. Understanding this pipeline is essential for implementing the `PlaybackEngine` in `picast-playback`.

## Stateful vs Stateless API

The V4L2 specification defines two API models for codec hardware. The BCM2711 uses both — the stateful model for H.264 (the primary PiCast decode path) and the stateless model for HEVC (deferred to v2). Understanding the difference is critical because they require fundamentally different userspace code.

| Aspect | Stateful (M2M) | Stateless (Request API) |
|--------|----------------|------------------------|
| Firmware holds | All decode state (SPS, PPS, DPB, reference lists) | Nothing — host must manage all state |
| Host sends | Raw bytestream chunks (NALUs) | Individual slice headers + per-frame control parameters |
| Latency | Higher (firmware buffers internally for reordering) | Lower (host controls decode timing) |
| Complexity for host | Simple — just feed bytes | Complex — must parse bitstream, manage DPB, provide per-slice params |
| BCM2711 support | **H.264 (bcm2835-codec)** | HEVC (rpivid, experimental) |
| GStreamer element | `v4l2h264dec` | `v4l2slh265dec` |
| PiCast v1 status | **Primary decode path** | Not used (deferred to v2) |

### Why Stateful is Preferred for PiCast

The stateful API is dramatically simpler to implement. The application merely feeds compressed bytes into the OUTPUT queue and retrieves decoded frames from the CAPTURE queue. The driver handles all internal state management, reference frame tracking, and B-frame reordering. For a streaming media player like PiCast — where latency tolerance is high (100ms+ is acceptable) and implementation simplicity is valued — the stateful model is the clear choice. The stateless model would require PiCast to implement a full HEVC bitstream parser in Rust, which is a substantial development effort for no functional benefit in v1.

## Device Nodes

```
/dev/video10  ← bcm2835-codec: H.264 decode (OUTPUT + CAPTURE, M2M single device)
/dev/video11  ← bcm2835-codec: H.264 encode (not used by PiCast)
/dev/video12  ← bcm2835-codec: ISP (color space conversion, SAND→NV12 for future HEVC)
/dev/video20  ← bcm2835-codec: Deinterlace (not used by PiCast)
/dev/media0   ← Media controller (pipeline topology enumeration)
```

Verify with:
```bash
v4l2-ctl --list-devices
# Expected output:
# bcm2835-codec (platform:bcm2835-codec):
#     /dev/video10
#     /dev/video11
#     /dev/video12
#     /dev/video20
```

Check decoder capabilities:
```bash
v4l2-ctl -d /dev/video10 --list-formats-ext
# Expected:
# Pixel Format: H264 (compressed)
#   Size: Discrete 1920x1080
#   Size: Discrete 1280x720
```

## Buffer Queue / Dequeue Workflow

The V4L2 M2M decode model uses two queues on the same device node: an OUTPUT queue for compressed data input and a CAPTURE queue for decoded frame output. The application feeds NALUs into OUTPUT and retrieves NV12 frames from CAPTURE in a producer-consumer pattern.

### Step 1: Open Device and Set Format

```c
int fd = open("/dev/video10", O_RDWR);

// Set OUTPUT format (compressed H.264 stream input)
struct v4l2_format fmt = {
    .type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
    .fmt.pix_mp.pixelformat = V4L2_PIX_FMT_H264;
    .fmt.pix_mp.width = 1920;
    .fmt.pix_mp.height = 1080;
    .fmt.pix_mp.num_planes = 1;
    .fmt.pix_mp.plane_fmt[0].sizeimage = 2 * 1024 * 1024; // 2 MB per buffer
};
ioctl(fd, VIDIOC_S_FMT, &fmt);

// Set CAPTURE format (decoded NV12 frames output)
struct v4l2_format fmt = {
    .type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    .fmt.pix_mp.pixelformat = V4L2_PIX_FMT_NV12;
    .fmt.pix_mp.width = 1920;
    .fmt.pix_mp.height = 1080;
};
ioctl(fd, VIDIOC_S_FMT, &fmt);
```

### Step 2: Request Buffers (DMA-BUF Mode)

```c
// Request OUTPUT buffers using DMA-BUF for zero-copy from GStreamer
struct v4l2_requestbuffers req = {
    .count = 4,                                     // 4 compressed input buffers
    .type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
    .memory = V4L2_MEMORY_DMABUF,                   // DMA-BUF for zero-copy
};
ioctl(fd, VIDIOC_REQBUFS, &req);

// Request CAPTURE buffers using DMA-BUF for zero-copy to kmssink
struct v4l2_requestbuffers req = {
    .count = 4,                                     // 4 decoded frame buffers
    .type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
    .memory = V4L2_MEMORY_DMABUF,                   // DMA-BUF for zero-copy
};
ioctl(fd, VIDIOC_REQBUFS, &req);
```

### Step 3: Start Streaming and Decode Loop

```
┌──────────────────────────────────────────────────────────────┐
│                    DECODE LOOP                                │
│                                                              │
│  ┌─────────────┐    QBUF     ┌─────────────┐                │
│  │ H.264 NAL   │───────────▶│  OUTPUT     │                │
│  │ units       │             │  queue      │                │
│  │ (from       │             └──────┬──────┘                │
│  │  GStreamer  │                    │                        │
│  │  h264parse) │             Hardware decode                 │
│  └─────────────┘             (bcm2835-codec)                │
│                                     │                        │
│                              ┌──────▼──────┐                │
│                              │  CAPTURE    │  DQBUF          │
│                              │  queue      │──────────▶│ kmssink
│                              └─────────────┘            │ (DMA-BUF
│                                                         │  import)
│  GStreamer handles this automatically via v4l2h264dec   │
└──────────────────────────────────────────────────────────────┘
```

The decode loop operates as follows:

1. **QBUF (OUTPUT)**: Queue a buffer containing compressed H.264 NALUs into the OUTPUT queue. The decoder begins processing immediately.
2. **DQBUF (OUTPUT)**: Dequeue the buffer when the decoder has consumed the data. The buffer can be reused for the next NALUs.
3. **DQBUF (CAPTURE)**: Dequeue a decompressed NV12 frame when the decoder has finished producing it. The frame is available as a DMA-BUF fd.
4. **QBUF (CAPTURE)**: Re-queue the frame buffer after the display (kmssink via HVS) has finished scanning it out. The decoder reuses it for the next frame.

### Step 4: Resolution Change Handling

When the H.264 stream changes resolution (e.g., ABR quality switch from 720p to 1080p), the decoder emits a `V4L2_EVENT_SOURCE_CHANGE` event:

```c
// Subscribe to source change events
struct v4l2_event_subscription sub = {
    .type = V4L2_EVENT_SOURCE_CHANGE,
};
ioctl(fd, VIDIOC_SUBSCRIBE_EVENT, &sub);

// When event fires:
// 1. Stop CAPTURE queue: VIDIOC_STREAMOFF
// 2. Get new resolution: VIDIOC_G_FMT on CAPTURE
// 3. Reallocate CAPTURE buffers: VIDIOC_REQBUFS with new count
// 4. Restart CAPTURE queue: VIDIOC_STREAMON
```

GStreamer's `v4l2h264dec` element handles this automatically, but the resolution change causes a brief pipeline reconfiguration (~50ms) during which no frames are produced.

## DMA-BUF Export

### From V4L2 (Decoder Output to kmssink)

```c
// Export a decoded frame as a DMA-BUF file descriptor
struct v4l2_exportbuffer exp = {
    .type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
    .index = buffer_index,
    .flags = O_RDONLY,
    .plane = 0,
};
ioctl(fd, VIDIOC_EXPBUF, &exp);
// exp.fd is now a DMA-BUF file descriptor that can be passed to kmssink
```

### Import into DRM (For Display via kmssink)

```c
// Import the DMA-BUF as a DRM framebuffer
struct drm_mode_fb_cmd2 fb = {
    .width = 1920,
    .height = 1080,
    .pixel_format = DRM_FORMAT_NV12,
    .flags = 0,
    .handles = { dma_buf_fd, 0, 0, 0 },      // NV12: Y plane handle
    .pitches = { 1920, 0, 0, 0 },              // Y pitch
    .offsets = { 0, 1920*1080, 0, 0 },         // UV plane offset
};
ioctl(drm_fd, DRM_IOCTL_MODE_ADDFB2, &fb);
// fb.fb_id is the DRM framebuffer ID, assigned to a plane via atomic commit
```

### Complete Zero-Copy Flow in PiCast

```
yt-dlp resolution ──▶ GStreamer souphttpsrc (Tor SOCKS5)
                              │
                              ▼
                     h264parse (SPS/PPS injection)
                              │
                              ▼
                     v4l2h264dec (V4L2 M2M via bcm2835-codec)
                              │
                      VIDIOC_EXPBUF → DMA-BUF FD
                              │
                         (zero-copy: no CPU reads or writes)
                              │
                              ▼
                     kmssink (DRM ADDFB2 + drmModeAtomicCommit)
                              │
                              ▼
                     HVS → HDMI Output
```

No pixel data is copied through CPU memory at any point after decode. The entire path from decoder output to HDMI scanout is zero-copy, which is critical for 1080p60 playback at ~3% CPU on the Pi 4.

## Format Negotiation

### Querying Supported Formats

```bash
# List supported compressed input formats
v4l2-ctl -d /dev/video10 --list-formats-ext
# Expected:
# Pixel Format: H264 (compressed)
#   Size: Discrete 1920x1080   Size: Discrete 1280x720

# List supported decompressed output formats
v4l2-ctl -d /dev/video10 --list-formats-ext --set-fmt-video=pixelformat=NV12
# Expected:
# Pixel Format: NV12 (Y/CbCr 4:2:0)
```

### Format Selection Priority

PiCast's format selection follows a strict priority order, enforced by the yt-dlp format string:

1. **H.264** → Hardware decode via V4L2 M2M (`v4l2h264dec`), zero-copy to kmssink
2. **VP9** → Software decode via GStreamer `avdec_vp9` or `libvpx` (limited to ~720p30, ~70% CPU)
3. **HEVC** → Deferred to v2 (SAND format output incompatibility)
4. **AV1** → Software decode via `av1dec` (dav1d), extremely CPU-intensive (~90% CPU at 480p30)

The yt-dlp format string `bv[height<=720][vcodec^=avc1]` ensures H.264 is always selected first.

## Multi-Plane Format Details (NV12)

NV12 is a **multi-plane** format in both V4L2 and DRM terminology:

| Plane | Content | Size (1920×1080) | Pitch |
|-------|---------|-------------------|-------|
| Plane 0 | Y (luma) | 1920 × 1080 = 2,073,600 bytes | 1920 bytes per row |
| Plane 1 | UV (chroma, interleaved CbCr) | 1920 × 540 = 1,036,800 bytes | 1920 bytes per row |

Total frame size: 3,110,400 bytes (2.97 MB)

In DRM, NV12 is represented as a single framebuffer object with two planes referenced by offset. The `DRM_IOCTL_MODE_ADDFB2` ioctl is used (not the older `ADDFB`) because it supports multi-plane formats.

## Performance Benchmarks

| Resolution | Codec | Decode Method | Framerate | CPU Usage | Power | Notes |
|-----------|-------|--------------|-----------|-----------|-------|-------|
| 1080p | H.264 | Hardware (V4L2 M2M) | 30 fps | ~5% | 3.5 W | PiCast primary path |
| 1080p | H.264 | Hardware (V4L2 M2M) | 60 fps | ~8% | 3.8 W | Highest H.264 rate |
| 1080p | H.264 | Software (avdec_h264) | 30 fps | ~90% | 6.5 W | Fallback only |
| 720p | VP9 | Software (avdec_vp9) | 30 fps | ~70% | 5.5 W | yt-dlp fallback format |
| 720p | H.264 | Hardware (V4L2 M2M) | 60 fps | ~8% | 3.8 W | Ideal for Tor streaming |
| 480p | H.264 | Hardware (V4L2 M2M) | 30 fps | ~3% | 3.3 W | ABR fallback tier |
| 4K | HEVC | Hardware (rpivid) | 30 fps | ~5%* | 4.0 W | *Requires SAND→NV12 conversion, breaks zero-copy |

**Conclusion**: Always prefer H.264 hardware decode via V4L2 M2M. The yt-dlp format selection string in `picast-resolver` prioritizes H.264 (`avc1`) over VP9 for this reason. The difference between 5% CPU (hardware) and 90% CPU (software) is the difference between a usable appliance and an overheating, stuttering device.

## Troubleshooting

### Common Issues

| Symptom | Cause | Fix |
|---------|-------|-----|
| `v4l2h264dec` element not found | GStreamer bad plugins not installed | `sudo apt install gstreamer1.0-plugins-bad` |
| `/dev/video10` does not exist | bcm2835-codec driver not loaded | Add `dtoverlay=vc4-kms-v3d` to `/boot/config.txt` |
| Decoder outputs garbled frames | Wrong pixel format on CAPTURE queue | Ensure CAPTURE is set to NV12, not YUV420 |
| Pipeline fails with "not negotiated" | Missing `h264parse` with `config-interval=-1` | Add `h264parse config-interval=-1` before decoder |
| DMA-BUF import fails in kmssink | CMA pool exhausted | Increase `cma=384M` in `/boot/cmdline.txt` |
| High CPU despite V4L2 decode | `io-mode` not set to `dmabuf` | Set both `io-mode=dmabuf` and `capture-io-mode=dmabuf` on v4l2h264dec |
