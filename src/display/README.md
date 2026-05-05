# picast-display

Manages the DRM/KMS display pipeline on the Raspberry Pi 4: atomic modesetting, plane management, GBM buffer allocation for OSD overlay, and V3D GPU-accelerated text rendering. This crate owns the DRM master role and provides the display infrastructure that `picast-playback`'s `kmssink` element uses for zero-copy video output.

## Purpose

The display crate provides the low-level display control layer that configures the BCM2711's Hardware Video Scaler (HVS) to composite video (from GStreamer/kmssink via DMA-BUF) and OSD overlays (rendered by V3D GPU) onto the HDMI output. This crate opens `/dev/dri/card0`, acquires DRM master privileges, discovers the connected HDMI connector and preferred mode, and programs the HVS planes via atomic modesetting. It coordinates with `picast-playback` to ensure kmssink can use the correct CRTC and plane configuration, and it manages the separate OSD overlay plane (Plane 1) for subtitle text and status indicators.

## Public API

| Item | Kind | Description |
|------|------|-------------|
| `DisplayManager` | struct | Implements `DisplayTrait`; opens DRM device, manages modes and planes |
| `DisplayManager::open(device_path)` | async constructor | Open DRM device, become master, enumerate resources, set preferred mode |
| `GbmBuffer` | struct | GBM-allocated scanout buffer with DMA-BUF export capability |
| `PlaneInfo` | struct | DRM plane metadata (ID, type, ZPOS, supported formats) |
| `PlaneType` | enum | `Primary`, `Overlay`, `Cursor` |
| `DisplayError` | enum | DRM/GBM error variants: `NotMaster`, `NoConnector`, `NoMode`, `PlaneConfig`, `GbmAlloc` |

Implements `picast_session::interfaces::DisplayTrait`:

| Method | Description |
|--------|-------------|
| `acquire()` | Open DRM device, set master, configure CRTC and planes |
| `release()` | Drop DRM master, close device, release all framebuffers |
| `resolution()` | Return current display resolution as `(width, height)` |

Additional methods:

| Method | Description |
|--------|-------------|
| `set_mode(w, h, hz)` | Atomic modeset to specified resolution and refresh rate |
| `blank()` | Show black (clear both planes) |
| `show_osd(text, x, y)` | Render text overlay on Plane 1 at specified position |
| `clear_osd()` | Remove overlay by setting Plane 1 FB_ID to 0 |
| `connector_name()` | Active connector name (e.g., `"HDMI-A-1"`) |
| `current_mode()` | Current mode as `(width, height, refresh_hz)` |
| `drm_fd()` | Raw DRM file descriptor (for sharing with kmssink) |
| `plane_info(plane_type)` | Get `PlaneInfo` for a specific plane type |

## Dependencies

| Dependency | Why |
|------------|-----|
| `picast-session` | Provides `DisplayTrait` trait definition that this crate implements |
| `drm` / `drm-ffi` | DRM ioctl wrappers for modesetting, plane configuration, framebuffer management |
| `gbm` | GBM buffer allocation for OSD overlay (scanout + render capable) |
| `nix` | Low-level Unix APIs: `ioctl`, `mmap`, `fcntl` for DRM FD management |
| `tokio` | Async runtime (DRM operations are synchronous but wrapped in async for trait compliance) |
| `tracing` | Debug logging for DRM resource enumeration and plane configuration |

## DRM/KMS Resource Model

On the BCM2711 with the `vc4-kms-v3d` device tree overlay, the DRM subsystem exposes the following resources. Understanding this model is essential for configuring the display pipeline correctly.

```
DRM Device /dev/dri/card0  (driver: vc4)
│
├── Planes (displayed in Z-order, lower ZPOS = further back)
│   ├── Plane 31  (Primary,  ZPOS 0)  ← Video framebuffer (NV12 DMA-BUF from kmssink)
│   │   Supported formats: NV12, XRGB8888, ARGB8888
│   │   Used for: decoded video from V4L2 H.264 decoder
│   │
│   ├── Plane 32  (Overlay, ZPOS 1)  ← OSD overlay (ARGB8888 from V3D EGL)
│   │   Supported formats: ARGB8888, XRGB8888
│   │   Used for: subtitles, buffering indicator, error messages
│   │
│   └── Plane 33  (Cursor,  ZPOS 2)  ← Unused in v1 (future: mouse cursor for UI)
│       Supported formats: ARGB8888
│
├── CRTC 0 (HVS Channel 0)
│   ├── Drives: Connector HDMI-A-1
│   ├── Current mode: 1920×1080@60Hz (preferred)
│   ├── Assigned planes: Plane 31 (video) + Plane 32 (OSD)
│   └── Vblank events: used for page-flip synchronization
│
├── Encoder 31 (TMDS — HDMI signal encoding)
│   └── Links CRTC 0 to Connector HDMI-A-1
│
└── Connector 32 (HDMI-A-1, connected)
    ├── Status: Connected
    ├── EDID: read from attached monitor
    ├── Preferred mode: 1920×1080@60Hz
    └── Available modes: [list from EDID]
```

### Buffer Allocation Paths

| Plane | Buffer Source | Allocation Method | Format |
|-------|--------------|-------------------|--------|
| Plane 0 (video) | V4L2 decoder CAPTURE queue | `VIDIOC_EXPBUF` → DMA-BUF FD | NV12 (two-plane 4:2:0) |
| Plane 1 (OSD) | GBM + V3D GPU rendering | `gbm_bo_create()` with `SCANOUT\|RENDERING` | ARGB8888 (32-bit with alpha) |

## Atomic Commit Workflow

PiCast uses the DRM atomic modesetting API (`drmModeAtomicCommit`) for all display updates. Atomic commits allow multiple properties (plane framebuffers, source/destination rectangles, CRTC mode) to be changed in a single, tear-free operation that takes effect at the next vblank.

```
Step 1: Create atomic request
    req = drmModeAtomicAlloc()

Step 2: Set CRTC properties
    req.add(CRTC_0, MODE_ID,   <mode_blob>)     ← Display mode (1920×1080@60Hz)
    req.add(CRTC_0, ACTIVE,    1)                ← Enable the CRTC

Step 3: Set connector properties
    req.add(HDMI-A-1, CRTC_ID, CRTC_0)           ← Link connector to CRTC

Step 4: Set Plane 0 (video) properties
    req.add(Plane_31, FB_ID,    <video_fb>)       ← DRM framebuffer from DMA-BUF import
    req.add(Plane_31, CRTC_ID,  CRTC_0)           ← Assign to CRTC 0
    req.add(Plane_31, SRC_X,    0)                ← Source rect X (16.16 fixed-point)
    req.add(Plane_31, SRC_Y,    0)
    req.add(Plane_31, SRC_W,    1920 << 16)       ← 125829120 in 16.16 fixed-point
    req.add(Plane_31, SRC_H,    1080 << 16)       ← 70778880 in 16.16 fixed-point
    req.add(Plane_31, CRTC_X,   0)                ← Destination rect (pixels)
    req.add(Plane_31, CRTC_Y,   0)
    req.add(Plane_31, CRTC_W,   1920)
    req.add(Plane_31, CRTC_H,   1080)

Step 5: Set Plane 1 (OSD) properties
    req.add(Plane_32, FB_ID,    <osd_fb>)         ← DRM framebuffer from GBM buffer
    req.add(Plane_32, CRTC_ID,  CRTC_0)
    req.add(Plane_32, SRC_X,    0)
    req.add(Plane_32, SRC_Y,    0)
    req.add(Plane_32, SRC_W,    osd_width << 16)
    req.add(Plane_32, SRC_H,    osd_height << 16)
    req.add(Plane_32, CRTC_X,   osd_x)            ← Position on screen
    req.add(Plane_32, CRTC_Y,   osd_y)
    req.add(Plane_32, CRTC_W,   osd_width)
    req.add(Plane_32, CRTC_H,   osd_height)
    req.add(Plane_32, ALPHA,    255)              ← Fully opaque (0–255)
    req.add(Plane_32, ZPOS,     1)                ← Above video (Plane 0)
    req.add(Plane_32, PIXEL_BLEND_MODE, 1)        ← Pre-multiplied alpha

Step 6: Test the request
    drmModeAtomicCommit(fd, req, DRM_MODE_ATOMIC_TEST_ONLY)
    → If error: check plane formats, mode compatibility, bandwidth limits

Step 7: Commit for real
    drmModeAtomicCommit(fd, req,
        DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_PAGE_FLIP_EVENT)

Step 8: Wait for page-flip event
    drmHandleEvent(fd, ...)
    → Previous framebuffers can now be safely reused or freed
```

## Implementation Guide for AI Agents

1. **DRM device opening and master** — use `drm::Device` trait from the `drm` crate. Call `set_master()` after opening. If `set_master()` fails with EPERM, another compositor (X11, Wayland) is running; fail with a clear error message telling the user to disable the desktop. The systemd unit should enforce this by not starting a desktop session.

2. **Resource enumeration** — call `resource_handles()` and `plane_handles()` to discover CRTCs, connectors, and planes. Match the first connected HDMI connector to its CRTC. Find the preferred mode (the one with `PREFERRED` flag). Enumerate plane types (Primary, Overlay, Cursor) and ZPOS values.

3. **Atomic modesetting** — the `drm-ffi` crate provides raw ioctl wrappers. Build a `drm_mode_atomic_req` and commit it. Start with just setting the mode and a solid-color framebuffer on Plane 0; add Plane 1 overlay configuration once the basic mode is working.

4. **GBM buffer allocation** — use the `gbm` crate. Create a GBM device from the DRM FD, then allocate buffers with `GBM_BO_USE_SCANOUT | GBM_BO_USE_RENDERING` flags. Verify the buffer is scanout-capable by checking the GBM format support.

5. **OSD rendering** — defer EGL rendering to a later phase. Start with a simple solid-color rectangle overlay (fill a dumb buffer with a color) to prove the plane composition works. Then upgrade to Pango/Cairo text rendering, and finally V3D GPU-accelerated rendering.

6. **Double buffering** — the OSD must use double buffering to avoid tearing. Maintain two GBM buffers: one on-screen (front), one being rendered (back). When rendering is complete, swap via atomic commit. Wait for the page-flip event before reusing the old front buffer.

7. **Testing** — unit tests can use a mock DRM device (the `drm` crate supports render nodes that don't require master). Integration tests must run on actual Pi hardware with an HDMI monitor connected.

## Key Constraints

- **DRM master is exclusive**: if X11 or Wayland is running, PiCast cannot become DRM master. The setup script must disable the desktop autologin and ensure `picast.service` starts on `tty1` instead.

- **Plane Z-ordering**: the HVS composites planes from lowest ZPOS to highest. Video MUST be on ZPOS 0 and OSD on ZPOS 1. If the Z-order is reversed, the OSD will be hidden behind the video and invisible.

- **Buffer format matching**: Plane 0 (video) must use NV12 format (from V4L2 decoder DMA-BUF). Plane 1 (OSD) must use ARGB8888 format (from GBM buffer). The DRM driver validates format compatibility during atomic commit — mismatched formats cause the commit to fail silently.

- **16.16 fixed-point SRC coordinates**: the `SRC_X`, `SRC_Y`, `SRC_W`, `SRC_H` properties use 16.16 fixed-point format. For a 1920×1080 source: `SRC_W = 1920 << 16 = 125829120`. Forgetting the shift results in a 0-pixel source rectangle (invisible plane) or a distorted image.

- **V3D firmware dependencies**: the V3D GPU requires the `v3d` kernel module loaded and firmware present. Verify with `lsmod | grep v3d` and `dmesg | grep v3d`. The `vc4-kms-v3d` device tree overlay must be enabled in `/boot/config.txt`.

- **HVS bandwidth limit**: the BCM2711 HVS has limited bandwidth. Two full-resolution planes at 4K60 may exceed the limit, causing display artifacts. Stick to 1080p60 or lower for reliable operation.

- **DMA-BUF lifetime**: a GBM buffer must not be freed while its DRM framebuffer is still on a plane. The HVS is reading from that memory. Use double-buffering: prepare the next OSD frame before releasing the previous one. Premature free causes visible corruption or a kernel panic.

- **DRM FD sharing with kmssink**: `kmssink` needs to use the same DRM device as `DisplayManager`. Either share the FD directly or ensure both open `/dev/dri/card0`. The `kmssink` element's `bus-id=vc4` property selects the correct driver.

## Reference

| Resource | Location |
|----------|----------|
| ADR-001: No Display Server | `DECISIONS.md` / `SPECIFICATION.md` §1.1 |
| DRM/KMS programming guide | `docs/playback/drm-kms.md` |
| BCM2711 HVS details | `docs/hardware/bcm2711.md` |
| Zero-copy pipeline architecture | `ARCHITECTURE.md` §4–5 |
| V4L2 DMA-BUF export | `docs/hardware/v4l2-pipeline.md` |
| DRM plane properties reference | `docs/playback/drm-kms.md` (Plane Properties table) |
