//! PiCast Display Manager
//!
//! Direct rendering manager for the Raspberry Pi's DRM/KMS subsystem.
//! Bypasses X11/Wayland and speaks directly to the kernel mode-setting
//! API so that GStreamer's `kmssink` (or a custom video sink) can
//! render frames with minimal latency and zero compositor overhead.
//!
//! ## Architecture
//!
//! ```text
//! ┌───────────────┐      ┌──────────────┐      ┌─────────────┐
//! │ GStreamer     │─────►│ GBM surface  │─────►│ DRM CRTC    │
//! │ video output  │      │ (scanout buf)│      │ (scan-out)  │
//! └───────────────┘      └──────────────┘      └─────────────┘
//!         ▲                                            │
//!         │              ┌──────────────┐              │
//!         └──────────────│ DRM plane    │◄─────────────┘
//!                        │ (z-order)    │
//!                        └──────────────┘
//! ```

use thiserror::Error;

// ── Errors ───────────────────────────────────────────────────────────

/// Errors originating from DRM/GBM operations.
#[derive(Error, Debug)]
pub enum DisplayError {
    /// Failed to open the DRM device node.
    #[error("failed to open DRM device: {0}")]
    DeviceOpen(String),

    /// A DRM mode-setting ioctl failed.
    #[error("DRM mode-setting failed: {0}")]
    Modeset(String),

    /// No suitable CRTC was found.
    #[error("no available CRTC")]
    NoCrtc,

    /// No suitable plane was found.
    #[error("no available plane")]
    NoPlane,

    /// GBM buffer allocation failed.
    #[error("GBM allocation failed: {0}")]
    GbmAlloc(String),
}

// ── DRM Plane ────────────────────────────────────────────────────────

/// Represents a DRM hardware overlay plane.
///
/// Planes are layered in Z-order; the video plane typically sits
/// above the primary (UI) plane so that video frames are composited
/// on top.
#[derive(Debug, Clone)]
pub struct DrmPlane {
    /// Kernel-assigned plane ID.
    pub plane_id: u32,
    /// Index in the Z-order stack (0 = bottom).
    pub zpos: u32,
    /// Supported pixel formats (fourcc codes).
    pub formats: Vec<u32>,
    /// Whether this plane can be used for video scan-out.
    pub is_primary: bool,
}

// ── DRM CRTC ─────────────────────────────────────────────────────────

/// Represents a DRM CRTC (CRT Controller) – the hardware scan-out
/// engine that reads a framebuffer and sends it to a connector.
#[derive(Debug, Clone)]
pub struct DrmCrtc {
    /// Kernel-assigned CRTC ID.
    pub crtc_id: u32,
    /// Currently active display mode width in pixels.
    pub width: u32,
    /// Currently active display mode height in pixels.
    pub height: u32,
    /// Refresh rate in Hz.
    pub refresh_rate: u32,
    /// ID of the framebuffer currently attached to this CRTC.
    pub fb_id: u32,
}

// ── Display Manager ──────────────────────────────────────────────────

/// High-level manager that owns the DRM device, GBM device, and
/// provides methods to acquire/release display resources.
///
/// Typically created once at startup and held for the lifetime of the
/// application.
pub struct DisplayManager {
    /// Path to the DRM device node (e.g. `/dev/dri/card0`).
    device_path: String,
    // TODO: add drm::Device and gbm::Device handles:
    // drm_device: drm::Device,
    // gbm_device: gbm::Device<drm::Device>,
}

impl DisplayManager {
    /// Open the DRM device at `device_path` and initialise GBM.
    ///
    /// Falls back to `/dev/dri/card0` if `device_path` is empty.
    pub fn new(device_path: &str) -> Result<Self, DisplayError> {
        let path = if device_path.is_empty() {
            "/dev/dri/card0"
        } else {
            device_path
        };
        // TODO: open drm::Device, create gbm::Device
        Ok(Self {
            device_path: path.to_owned(),
        })
    }

    /// Return a list of available overlay planes.
    pub fn planes(&self) -> Result<Vec<DrmPlane>, DisplayError> {
        // TODO: enumerate DRM planes via resources
        Ok(vec![])
    }

    /// Return a list of available CRTCs.
    pub fn crtcs(&self) -> Result<Vec<DrmCrtc>, DisplayError> {
        // TODO: enumerate DRM CRTCs via resources
        Ok(vec![])
    }

    /// Acquire the primary CRTC and configure it for video output.
    pub fn acquire(&mut self) -> Result<(), DisplayError> {
        // TODO: mode-set the CRTC
        Ok(())
    }

    /// Release the CRTC and restore the previous framebuffer.
    pub fn release(&mut self) -> Result<(), DisplayError> {
        // TODO: drop the mode-set
        Ok(())
    }

    /// Return the current display resolution as `(width, height)`.
    pub fn resolution(&self) -> Result<(u32, u32), DisplayError> {
        // TODO: query current mode from CRTC
        Ok((1920, 1080))
    }

    /// Return the DRM device path.
    pub fn device_path(&self) -> &str {
        &self.device_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_manager_default_path() {
        let mgr = DisplayManager::new("").expect("new with empty path should succeed");
        assert_eq!(mgr.device_path(), "/dev/dri/card0", "empty path should fall back to /dev/dri/card0");

        let mgr = DisplayManager::new("/dev/dri/card1").expect("new with explicit path should succeed");
        assert_eq!(mgr.device_path(), "/dev/dri/card1");
    }

    #[test]
    fn display_error_variants() {
        let err = DisplayError::DeviceOpen("/dev/dri/card0".into());
        assert!(err.to_string().contains("failed to open DRM device"));

        let err = DisplayError::Modeset("mode rejected".into());
        assert!(err.to_string().contains("DRM mode-setting failed"));

        let err = DisplayError::NoCrtc;
        assert!(err.to_string().contains("no available CRTC"));

        let err = DisplayError::NoPlane;
        assert!(err.to_string().contains("no available plane"));

        let err = DisplayError::GbmAlloc("out of memory".into());
        assert!(err.to_string().contains("GBM allocation failed"));
    }

    #[test]
    fn display_manager_planes_and_crtcs_empty() {
        let mgr = DisplayManager::new("").expect("new should succeed");
        assert!(mgr.planes().unwrap().is_empty(), "stub should return empty planes");
        assert!(mgr.crtcs().unwrap().is_empty(), "stub should return empty CRTCs");
    }

    #[test]
    fn display_manager_resolution_default() {
        let mgr = DisplayManager::new("").expect("new should succeed");
        let (w, h) = mgr.resolution().expect("resolution should succeed");
        assert_eq!((w, h), (1920, 1080), "stub should return 1920x1080");
    }
}
