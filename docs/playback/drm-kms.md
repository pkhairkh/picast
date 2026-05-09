# DRM/KMS Programming Guide

This document describes how boGDan programs the Direct Rendering Manager (DRM) and Kernel Mode Setting (KMS) subsystem on the Raspberry Pi 4 for video display and OSD overlay. It covers device opening, master acquisition, resource enumeration, atomic modesetting, framebuffer management, double buffering, and the complete atomic commit workflow with all property values.

## Opening the DRM Device

### Device Nodes

```
/dev/dri/card0      ← Primary DRM device (vc4 driver)
/dev/dri/card1      ← (if V3D is exposed as a separate device)
/dev/dri/renderD128 ← V3D render node (no auth needed, for off-screen GPU rendering)
```

### Opening and Becoming Master

```rust
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;

let drm_file = OpenOptions::new()
    .read(true)
    .write(true)
    .open("/dev/dri/card0")?;

// Acquire DRM master privilege (required for modesetting)
// Only one process can be master at a time.
drm::control::Device::set_master(&drm_file)?;
```

**Important**: If X11 or Wayland is running, they already hold DRM master and `set_master()` will fail with EPERM. boGDan requires exclusive access to the display. The systemd unit file must not start a desktop session — the `autologin` service should launch `bogdand` directly on `tty1`.

### DRM Master Lifetime

boGDan holds the DRM master privilege for its entire process lifetime. If the process crashes, systemd restarts it, and it re-acquires DRM master on the next `open()` + `set_master()`. There is no other process competing for DRM master in the boGDan appliance configuration.

## Resource Enumeration

### Getting Resource Handles

```rust
use drm::control::Device;

let res = drm_file.resource_handles()?;

// CRTCs (display controllers)
let crtcs: Vec<Handle> = res.crtcs();

// Connectors (HDMI, DSI, DPI physical outputs)
let connectors: Vec<Handle> = res.connectors();

// Encoders (link between CRTC and connector)
let encoders: Vec<Handle> = res.encoders();

// Framebuffers (currently attached)
let fbs: Vec<Handle> = res.framebuffers();
```

### Enumerating Planes (Atomic API)

```rust
let plane_res = drm_file.plane_handles()?;

for plane_handle in plane_res.planes() {
    let info = drm_file.get_plane(plane_handle)?;
    // info.possible_crtcs – bitmask of which CRTCs this plane can use
    // info.fb_id – currently attached framebuffer (0 if unused)
    // info.crtc_id – currently attached CRTC

    // Get plane properties (type, zpos, formats, etc.)
    let props = drm_file.get_properties(plane_handle)?;
    for prop in props {
        let info = drm_file.get_property(prop.prop_handle)?;
        // info.name() – "type", "zpos", "FB_ID", "CRTC_ID", etc.
    }
}
```

### Finding the Active Connector

```rust
for conn_handle in res.connectors() {
    let conn = drm_file.get_connector(conn_handle)?;

    if conn.state() == drm::control::connector::State::Connected {
        println!("Connected: {:?}", conn.interface());

        // Find the preferred mode (typically the monitor's native resolution)
        let preferred = conn.modes().iter().find(|m| {
            m.type_flags().contains(drm::control::ModeTypeFlags::PREFERRED)
        });
    }
}
```

## Atomic Modesetting

Atomic modesetting is the modern DRM API that allows setting multiple properties (CRTC mode, plane framebuffer, plane position, alpha blending) in a single, flicker-free commit that takes effect at the next vertical blank (vblank). This is essential for boGDan because it prevents tearing between video and OSD plane updates.

### Complete Atomic Commit Workflow

```
Step 1: Create atomic request
    req = AtomicReq::new()

Step 2: Set CRTC properties
    req.add(crtc, CRTC_MODE_ID, mode_blob)     ← Mode blob from create_mode_blob()
    req.add(crtc, CRTC_ACTIVE, 1)               ← Enable the CRTC

Step 3: Set connector properties
    req.add(connector, CRTC_CRTC_ID, crtc)       ← Link HDMI connector to CRTC 0

Step 4: Set Plane 0 (video) properties
    req.add(plane_0, PLANE_FB_ID, video_fb)       ← DRM framebuffer from DMA-BUF import
    req.add(plane_0, PLANE_CRTC_ID, crtc)         ← Assign to CRTC 0
    req.add(plane_0, PLANE_SRC_X, 0)              ← Source X (16.16 fixed-point)
    req.add(plane_0, PLANE_SRC_Y, 0)              ← Source Y (16.16 fixed-point)
    req.add(plane_0, PLANE_SRC_W, 1920 << 16)     ← Source W = 125829120
    req.add(plane_0, PLANE_SRC_H, 1080 << 16)     ← Source H = 70778880
    req.add(plane_0, PLANE_CRTC_X, 0)             ← Destination X (pixels)
    req.add(plane_0, PLANE_CRTC_Y, 0)             ← Destination Y (pixels)
    req.add(plane_0, PLANE_CRTC_W, 1920)          ← Destination W (pixels)
    req.add(plane_0, PLANE_CRTC_H, 1080)          ← Destination H (pixels)

Step 5: Set Plane 1 (OSD) properties
    req.add(plane_1, PLANE_FB_ID, osd_fb)         ← GBM buffer imported as DRM FB
    req.add(plane_1, PLANE_CRTC_ID, crtc)         ← Assign to CRTC 0
    req.add(plane_1, PLANE_SRC_X, 0)
    req.add(plane_1, PLANE_SRC_Y, 0)
    req.add(plane_1, PLANE_SRC_W, osd_w << 16)
    req.add(plane_1, PLANE_SRC_H, osd_h << 16)
    req.add(plane_1, PLANE_CRTC_X, osd_x)         ← Position on screen
    req.add(plane_1, PLANE_CRTC_Y, osd_y)
    req.add(plane_1, PLANE_CRTC_W, osd_w)
    req.add(plane_1, PLANE_CRTC_H, osd_h)
    req.add(plane_1, PLANE_ALPHA, 255)            ← Fully opaque (0–255 range)
    req.add(plane_1, PLANE_ZPOS, 1)              ← Above video (Plane 0 is ZPOS 0)
    req.add(plane_1, PLANE_PIXEL_BLEND_MODE, 1)  ← Pre-multiplied alpha

Step 6: Test the request (validates without applying)
    drm_file.atomic_commit(&req, AtomicCommitFlags::TEST_ONLY)

Step 7: Apply for real (takes effect at next vblank)
    drm_file.atomic_commit(&req,
        AtomicCommitFlags::ALLOW_MODESET | AtomicCommitFlags::PAGE_FLIP_EVENT)

Step 8: Wait for page-flip event
    drmHandleEvent(fd, ...)
    → Previous framebuffers can now be safely freed or reused
```

### Commit Flags

| Flag | Value | Purpose |
|------|-------|---------|
| `DRM_MODE_ATOMIC_ALLOW_MODESET` | 0x01 | Allow modesetting changes (CRTC mode, connector assignment). Required for the first commit or when changing resolution. |
| `DRM_MODE_ATOMIC_NONBLOCK` | 0x02 | Don't block until vblank. The commit returns immediately and the change takes effect at the next vblank. |
| `DRM_MODE_ATOMIC_TEST_ONLY` | 0x04 | Validate the request without applying it. Use this before the real commit to catch configuration errors. |
| `DRM_MODE_PAGE_FLIP_EVENT` | — | Request a page-flip event when the commit takes effect. Used for vblank synchronization. |

## Framebuffer Management

### Creating a Framebuffer from a DMA-BUF (Video Path)

```rust
use drm::control::framebuffer;

// Import DMA-BUF fd as a DRM GEM object
let gem_handle = drm_file.gem_import_dma_buf(dma_buf_fd)?;

// Create DRM framebuffer for NV12 (two-plane)
let fb_info = framebuffer::Info::new(
    1920,                       // width
    1080,                       // height
    drm::buffer::Format::NV12,  // pixel format
    &[
        framebuffer::PlaneInfo {
            handle: gem_handle,  // Y plane
            pitch: 1920,         // bytes per row
            offset: 0,           // Y starts at offset 0
        },
        framebuffer::PlaneInfo {
            handle: gem_handle,  // UV plane (same buffer)
            pitch: 1920,         // bytes per row
            offset: 1920 * 1080, // UV starts after Y plane
        },
    ],
)?;
let fb = drm_file.add_framebuffer(&fb_info, 32)?;
```

### Creating a Dumb Buffer (Simple OSD Path)

For simple OSD overlays that don't need GPU rendering, a "dumb buffer" is the simplest approach. Dumb buffers are CPU-accessible memory that can be written to directly:

```rust
let dumb = drm_file.create_dumb_buffer(
    1920, 1080,
    drm::buffer::Format::XRGB8888,
    32,     // bits per pixel
)?;

// Map the dumb buffer for CPU rendering
let mut mapping = drm_file.map_dumb_buffer(&dumb)?;
// mapping.buffer_mut() → &mut [u8]

// Write pixels directly (e.g., fill with solid color for a blank OSD)
for pixel in mapping.buffer_mut().chunks_exact_mut(4) {
    pixel[0] = 0;    // Blue
    pixel[1] = 0;    // Green
    pixel[2] = 0;    // Red
    pixel[3] = 0xFF; // Alpha (unused for XRGB)
}

// Create framebuffer from dumb buffer
let fb = drm_file.add_framebuffer(&dumb, 32)?;
```

### Creating a GBM Buffer (GPU-Accelerated OSD Path)

For high-quality OSD rendering with text antialiasing, use GBM + V3D GPU:

```rust
// 1. Create GBM device from DRM FD
let gbm_device = gbm::Device::new(drm_fd)?;

// 2. Allocate scanout + render buffer
let buffer = gbm_device.create_buffer_object(
    width, height,
    drm::buffer::Format::ARGB8888,
    gbm::BufferObjectFlags::SCANOUT | gbm::BufferObjectFlags::RENDERING,
)?;

// 3. Render OSD text via V3D EGL (see bcm2711.md V3D section)
// ...

// 4. Import as DRM framebuffer
let fb = drm_file.add_framebuffer(&buffer, 32)?;
```

## Plane Properties Reference

| Property | Type | Description | Notes |
|----------|------|-------------|-------|
| `FB_ID` | object | Framebuffer to display on this plane | Set to 0 to disable the plane |
| `CRTC_ID` | object | Which CRTC this plane is associated with | Must match the CRTC that drives the connector |
| `SRC_X` | range | Source rectangle X position | 16.16 fixed-point format |
| `SRC_Y` | range | Source rectangle Y position | 16.16 fixed-point format |
| `SRC_W` | range | Source rectangle width | 16.16 fixed-point format (e.g., 1920 << 16) |
| `SRC_H` | range | Source rectangle height | 16.16 fixed-point format (e.g., 1080 << 16) |
| `CRTC_X` | signed range | Destination rectangle X (pixels) | Can be negative (off-screen) |
| `CRTC_Y` | signed range | Destination rectangle Y (pixels) | Can be negative (off-screen) |
| `CRTC_W` | range | Destination rectangle width (pixels) | Can be larger than source for HVS upscaling |
| `CRTC_H` | range | Destination rectangle height (pixels) | HVS handles scaling automatically |
| `type` | enum | Plane type: Primary(0), Overlay(1), Cursor(2) | Read-only; set by driver |
| `zpos` | range | Z-ordering | Lower = behind. boGDan: video=0, OSD=1 |
| `alpha` | range | Per-plane alpha (0–255) | 0=transparent, 255=opaque |
| `pixel_blend_mode` | enum | 0=None, 1=Pre-multiplied, 2=Coverage | Use Pre-multiplied (1) for proper alpha blending |
| `rotation` | bitmask | 1=Rotate-0, 2=Rotate-90, 4=Rotate-180, 8=Rotate-270 | Not used by boGDan (always Rotate-0) |

## Coordinate System

Source coordinates (`SRC_X/Y/W/H`) use **16.16 fixed-point** format. This means the pixel value is shifted left by 16 bits:

```
SRC_X = (pixel_x << 16)
SRC_W = (pixel_width << 16)
```

For a 1920×1080 framebuffer displayed at full screen on a 1920×1080 display:

```
SRC_X = 0
SRC_Y = 0
SRC_W = 1920 << 16 = 125,829,120
SRC_H = 1080 << 16 = 70,778,880
CRTC_X = 0
CRTC_Y = 0
CRTC_W = 1920
CRTC_H = 1080
```

For a 720p video (1280×720) upscaled to fill a 1080p display:

```
SRC_X = 0
SRC_Y = 0
SRC_W = 1280 << 16 = 83,886,080
SRC_H = 720 << 16 = 47,185,920
CRTC_X = 0
CRTC_Y = 0
CRTC_W = 1920       ← HVS scales from 1280→1920
CRTC_H = 1080       ← HVS scales from 720→1080
```

The HVS performs high-quality polyphase scaling when the source and destination rectangles differ in size. This is how 720p video fills a 1080p display without CPU involvement.

## Double Buffering

To avoid visual tearing, the OSD plane uses double buffering. Two GBM buffers are maintained: one is being displayed (front), the other is being rendered into (back). When rendering is complete, the buffers are swapped via atomic commit.

```rust
struct OsdDoubleBuffer {
    front: Option<framebuffer::Handle>,
    back: Option<framebuffer::Handle>,
}

impl OsdDoubleBuffer {
    async fn swap(&mut self, new_back: framebuffer::Handle, drm: &DrmDevice) {
        // 1. Commit new_back to overlay plane
        let mut req = AtomicReq::new();
        req.add(overlay_plane, PLANE_FB_ID, new_back);
        drm.atomic_commit(&req, AtomicCommitFlags::PAGE_FLIP_EVENT)?;

        // 2. Wait for page-flip event (vblank)
        drm.wait_for_page_flip_event().await;

        // 3. Old front is now off-screen and can be reused
        self.front = self.back.take();
        self.back = Some(new_back);
    }
}
```

The video plane (Plane 0) does not need explicit double buffering because `kmssink` handles it internally — it maintains a pool of DMA-BUF buffers from the V4L2 decoder and swaps them automatically via GStreamer's buffer flow.

## Common Pitfalls

1. **SRC coordinates must be 16.16 fixed-point**. Forgetting the `<< 16` shift results in a 0-pixel source rectangle (invisible plane) or a distorted, tiny image.

2. **Plane must be attached to a CRTC**. Setting `FB_ID` without `CRTC_ID` has no effect — the plane needs to know which display controller is driving it.

3. **CRTC must be active**. A modeset commit (with `ALLOW_MODESET`) is required to activate a CRTC before planes can be displayed. Subsequent updates only need `PAGE_FLIP_EVENT`.

4. **Framebuffer format must match**. The DRM driver validates that the framebuffer's pixel format is supported by the plane. NV12 is supported on the primary plane but NOT on the overlay plane (use ARGB8888 for overlay).

5. **Alpha blending requires the correct blend mode**. Set `pixel_blend_mode = Pre-multiplied` (value 1) for proper transparency. Without this, semi-transparent OSD pixels will appear with incorrect colors.

6. **Dumb buffers are slow**. They use CPU rendering and are not suitable for high-frame-rate content or complex OSD. Use GBM + V3D for the OSD rendering path.

7. **DRM master must not be dropped**. If the DRM file descriptor is closed, you lose master status and all modesetting state is reset. Keep the FD open for the entire application lifetime.

8. **DMA-BUF lifetime**. A DMA-BUF must not be freed while its DRM framebuffer is still on a plane. The HVS is reading from that memory asynchronously. Use double buffering to ensure old buffers are only freed after the page-flip event confirms they are no longer in use.

9. **Atomic test before commit**. Always use `TEST_ONLY` before the real commit to catch configuration errors. A failed atomic commit can leave the display in an inconsistent state.
