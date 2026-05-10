//! boGDan Display Manager
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
//!
//! ## Platform Notes
//!
//! On Raspberry Pi 4B+ the vc4 DRM driver exposes:
//! - **Plane 0**: Primary plane (UI / OSD)
//! - **Plane 1+**: Overlay planes (video)
//!
//! The HVS (Hardware Video Scaler) can scale planes independently,
//! so video can be rendered at native resolution and upscaled to
//! the display mode by hardware.
//!
//! For testing on x86_64, load `vkms`:
//! ```sh
//! modprobe vkms enable_writeback=1
//! ```
//!
//! When compiled without the `hw` feature, the display manager
//! operates in mock mode — all types are available but DRM operations
//! are no-ops.

#[cfg(feature = "hw")]
use drm::control::connector::State as ConnectorState;
#[cfg(feature = "hw")]
use drm::control::{self, Device as ControlDevice, Mode, PlaneType};
#[cfg(feature = "hw")]
use drm::Device as DrmDevice;
#[cfg(feature = "hw")]
use std::fs::OpenOptions;
#[cfg(feature = "hw")]
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd};
#[cfg(feature = "hw")]
use std::path::Path;
use thiserror::Error;

// ── Fourcc helpers ───────────────────────────────────────────────────

/// Well-known DRM fourcc pixel format codes.
///
/// These are used for plane format validation and capability checks.
/// The kernel returns them as `u32` values in little-endian byte order,
/// so `XR24` (XRGB8888) is `0x34325258`.
pub mod fourcc {
    /// XRGB8888 (32-bit, no alpha) — typical primary plane format.
    pub const XR24: u32 = 0x34325258;
    /// ARGB8888 (32-bit with alpha) — OSD overlay format.
    pub const AR24: u32 = 0x34325241;
    /// NV12 (YUV 4:2:0, two-plane) — typical video decode output.
    pub const NV12: u32 = 0x3231564E;
    /// NV21 (YUV 4:2:0, V/U swapped) — alternative video format.
    pub const NV21: u32 = 0x3132564E;
    /// P030 (10-bit YUV 4:2:0) — HDR video format on Pi 4.
    pub const P030: u32 = 0x30335030;
    /// RGB565 (16-bit) — low-bpp fallback.
    pub const RG16: u32 = 0x36314752;
    /// YUYV (YUV 4:2:2 packed) — some cameras output this.
    pub const YUYV: u32 = 0x56595559;

    /// Convert a fourcc u32 to a 4-character string for display.
    ///
    /// ```ignore
    /// assert_eq!(fourcc::to_str(0x34325258), "XR24");
    /// ```
    pub fn to_str(code: u32) -> String {
        let bytes = code.to_le_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Check if a fourcc code represents a video-friendly format
    /// (NV12, NV21, P030, YUYV).
    pub fn is_video_format(code: u32) -> bool {
        matches!(code, NV12 | NV21 | P030 | YUYV)
    }

    /// Check if a fourcc code represents a graphics/UI format
    /// (XRGB8888, ARGB8888, RGB565).
    pub fn is_graphics_format(code: u32) -> bool {
        matches!(code, XR24 | AR24 | RG16)
    }
}

// ── Errors ───────────────────────────────────────────────────────────

/// Errors originating from DRM/GBM operations.
#[derive(Error, Debug)]
pub enum DisplayError {
    /// Failed to open the DRM device node.
    #[error("failed to open DRM device: {0}")]
    DeviceOpen(String),

    /// Failed to acquire DRM master.
    #[error("failed to acquire DRM master: {0}")]
    MasterAcquire(String),

    /// The DRM driver is not the expected one (e.g. expected vc4).
    #[error("unexpected DRM driver: expected {expected}, got {actual}")]
    WrongDriver {
        /// The driver name that was expected.
        expected: String,
        /// The actual driver name reported by the kernel.
        actual: String,
    },

    /// A DRM mode-setting ioctl failed.
    #[error("DRM mode-setting failed: {0}")]
    Modeset(String),

    /// No suitable CRTC was found.
    #[error("no available CRTC")]
    NoCrtc,

    /// No suitable connector was found.
    #[error("no connected connector")]
    NoConnector,

    /// No suitable plane was found.
    #[error("no available plane")]
    NoPlane,

    /// No plane supports a required video format (e.g. NV12).
    #[error("no plane supports required video format: {0}")]
    NoVideoPlane(String),

    /// No plane supports a required graphics format (e.g. ARGB8888).
    #[error("no plane supports required graphics format: {0}")]
    NoGraphicsPlane(String),

    /// No suitable display mode was found.
    #[error("no display mode available")]
    NoMode,

    /// GBM buffer allocation failed.
    #[error("GBM allocation failed: {0}")]
    GbmAlloc(String),

    /// Hardware display is not available (compiled without the `hw` feature).
    #[error("hardware display unavailable — compile with the 'hw' feature")]
    HardwareUnavailable,

    /// An I/O error occurred.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
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
    /// Supported pixel formats (fourcc codes as u32).
    pub formats: Vec<u32>,
    /// Whether this is the primary plane (UI/OSD).
    pub is_primary: bool,
    /// Bitmask of CRTC indices this plane can be used with.
    pub possible_crtcs: u32,
    /// Human-readable plane type name ("Primary", "Overlay", "Cursor").
    pub type_name: String,
}

impl DrmPlane {
    /// Check if this plane supports a given fourcc format.
    pub fn supports_format(&self, fourcc: u32) -> bool {
        self.formats.contains(&fourcc)
    }

    /// Check if this plane supports any video format (NV12, NV21, P030, YUYV).
    pub fn supports_video(&self) -> bool {
        self.formats.iter().any(|&f| fourcc::is_video_format(f))
    }

    /// Check if this plane supports any graphics format (XRGB8888, ARGB8888, RGB565).
    pub fn supports_graphics(&self) -> bool {
        self.formats.iter().any(|&f| fourcc::is_graphics_format(f))
    }

    /// Return a human-readable list of supported format names.
    pub fn format_names(&self) -> Vec<String> {
        self.formats.iter().map(|&f| fourcc::to_str(f)).collect()
    }
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
    /// Refresh rate in millihertz (divide by 1000 for Hz).
    pub refresh_mhz: u32,
    /// ID of the framebuffer currently attached to this CRTC.
    pub fb_id: Option<u32>,
}

// ── Display Mode ─────────────────────────────────────────────────────

/// A display mode with resolution and refresh rate.
///
/// Used to represent available modes for a connector, with
/// comparison helpers for mode selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayMode {
    /// Horizontal resolution in pixels.
    pub width: u32,
    /// Vertical resolution in pixels.
    pub height: u32,
    /// Refresh rate in millihertz (divide by 1000 for Hz).
    pub refresh_mhz: u32,
}

impl DisplayMode {
    /// Create a new display mode.
    pub fn new(width: u32, height: u32, refresh_mhz: u32) -> Self {
        Self { width, height, refresh_mhz }
    }

    /// Standard 1080p60 mode (1920x1080 at 60 Hz).
    pub fn mode_1080p60() -> Self {
        Self::new(1920, 1080, 60000)
    }

    /// Standard 720p60 mode (1280x720 at 60 Hz).
    pub fn mode_720p60() -> Self {
        Self::new(1280, 720, 60000)
    }

    /// Standard 4K30 mode (3840x2160 at 30 Hz).
    pub fn mode_4k30() -> Self {
        Self::new(3840, 2160, 30000)
    }

    /// Return the refresh rate in Hz (rounded).
    pub fn refresh_hz(&self) -> u32 {
        self.refresh_mhz / 1000
    }

    /// Return the pixel count (width * height).
    pub fn pixels(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// Check if this mode matches the standard 1080p60 specification.
    pub fn is_1080p60(&self) -> bool {
        self.width == 1920 && self.height == 1080 && self.refresh_hz() >= 60
    }

    /// Compare modes for preference ordering.
    ///
    /// Returns `Ordering::Greater` if `self` is preferred over `other`.
    /// Preference is: 1080p60 first, then highest resolution, then
    /// highest refresh rate.
    pub fn preference_cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Strongly prefer 1080p60.
        let self_is_1080p60 = self.is_1080p60();
        let other_is_1080p60 = other.is_1080p60();
        match (self_is_1080p60, other_is_1080p60) {
            (true, false) => return std::cmp::Ordering::Greater,
            (false, true) => return std::cmp::Ordering::Less,
            _ => {},
        }
        // Then prefer higher resolution.
        self.pixels().cmp(&other.pixels()).then_with(|| self.refresh_mhz.cmp(&other.refresh_mhz))
    }
}

impl std::fmt::Display for DisplayMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}@{}Hz", self.width, self.height, self.refresh_hz())
    }
}

// ── Connector Info ───────────────────────────────────────────────────

/// Information about a connected display output (HDMI, DSI, etc.).
#[derive(Debug, Clone)]
pub struct DisplayConnector {
    /// Kernel-assigned connector ID.
    pub connector_id: u32,
    /// Connector type string (e.g. "HDMI-A-1", "DSI-1").
    pub connector_type: String,
    /// Connection state.
    pub connected: bool,
    /// All available display modes for this connector.
    pub modes: Vec<DisplayMode>,
    /// The preferred (best) display mode.
    pub preferred_mode: Option<DisplayMode>,
}

impl DisplayConnector {
    /// Check if this connector is an HDMI output.
    pub fn is_hdmi(&self) -> bool {
        self.connector_type.starts_with("HDMI")
    }

    /// Select the best display mode for boGDan playback.
    ///
    /// Preference order:
    /// 1. 1080p60 (1920x1080 at 60 Hz) — ideal for video playback.
    /// 2. Highest available resolution at highest refresh rate.
    /// 3. Fall back to `preferred_mode` from the connector.
    pub fn best_mode(&self) -> Option<&DisplayMode> {
        if self.modes.is_empty() {
            return self.preferred_mode.as_ref();
        }
        self.modes.iter().max_by(|a, b| a.preference_cmp(b))
    }

    /// Select the best mode that does not exceed a given resolution.
    ///
    /// Useful for software decode fallback where we want 720p max.
    pub fn best_mode_within(&self, max_width: u32, max_height: u32) -> Option<&DisplayMode> {
        self.modes
            .iter()
            .filter(|m| m.width <= max_width && m.height <= max_height)
            .max_by(|a, b| a.preference_cmp(b))
            .or_else(|| self.best_mode())
    }
}

// ── Display Manager ──────────────────────────────────────────────────

/// High-level manager that owns the DRM device, GBM device, and
/// provides methods to acquire/release display resources.
///
/// Typically created once at startup and held for the lifetime of the
/// application. On creation, the manager opens the DRM device node,
/// acquires DRM master, and enumerates available resources.
///
/// When compiled without the `hw` feature, the manager operates in
/// mock mode — `new()` always succeeds, and `acquire()`/`release()`
/// are no-ops.
pub struct DisplayManager {
    /// Path to the DRM device node (e.g. `/dev/dri/card0`).
    device_path: String,
    /// Name of the DRM driver (e.g. "vc4", "vkms").
    driver_name: String,
    /// Raw file descriptor for the DRM device.
    #[cfg(feature = "hw")]
    drm_fd: Option<Card>,
    /// Cached list of connectors (populated on acquire).
    connectors: Vec<DisplayConnector>,
    /// Cached list of planes.
    planes: Vec<DrmPlane>,
    /// Cached list of CRTCs.
    crtcs: Vec<DrmCrtc>,
    /// Currently active CRTC (set after acquire).
    active_crtc: Option<DrmCrtc>,
    /// The selected display mode for the active output.
    active_mode: Option<DisplayMode>,
    /// Saved CRTC state for restoration on release.
    #[cfg(feature = "hw")]
    saved_crtc: Option<SavedCrtcState>,
    /// GBM device handle (for OSD overlay surface allocation).
    #[cfg(feature = "hw")]
    gbm_device: Option<GbmDevice>,
}

/// Saved CRTC state for restoration on release.
#[cfg(feature = "hw")]
struct SavedCrtcState {
    crtc_id: u32,
    fb_id: Option<u32>,
    mode: Option<Mode>,
    x: u32,
    y: u32,
    /// Saved connector ID for atomic restore.
    #[allow(dead_code)]
    connector_id: u32,
}

#[cfg(feature = "hw")]
#[derive(Debug)]
struct Card(std::fs::File);

#[cfg(feature = "hw")]
impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[cfg(feature = "hw")]
impl AsRawFd for Card {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.0.as_raw_fd()
    }
}

#[cfg(feature = "hw")]
impl DrmDevice for Card {}

#[cfg(feature = "hw")]
impl ControlDevice for Card {}

// ── GBM wrappers ─────────────────────────────────────────────────────

#[cfg(feature = "hw")]
struct GbmDevice {
    _device: gbm::Device<Card>,
}

#[cfg(feature = "hw")]
struct GbmSurface {
    _surface: gbm::Surface<()>,
    #[allow(dead_code)]
    width: u32,
    #[allow(dead_code)]
    height: u32,
    #[allow(dead_code)]
    format: u32,
}

// ── HW implementation ────────────────────────────────────────────────

#[cfg(feature = "hw")]
impl DisplayManager {
    /// Open the DRM device at `device_path` and prepare for display management.
    ///
    /// Falls back to auto-detection if `device_path` is empty.
    /// On Raspberry Pi 4B+ with vc4, the device is typically
    /// `/dev/dri/card1` (card0 is the firmware framebuffer).
    ///
    /// The driver name is queried from the kernel and logged.
    /// A warning is emitted if the driver is not "vc4" (the expected
    /// Pi 4 driver), but the manager is still created — it may work
    /// with other drivers (e.g. vkms for testing).
    pub fn new(device_path: &str) -> Result<Self, DisplayError> {
        let path =
            if device_path.is_empty() { Self::find_dri_device()? } else { device_path.to_owned() };

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| DisplayError::DeviceOpen(format!("{}: {}", path, e)))?;

        let card = Card(file);
        let fd = card.as_raw_fd();

        // Query the driver name from the kernel.
        let driver_name = match card.get_driver() {
            Ok(driver) => {
                let name = driver.name().to_string_lossy().into_owned();
                tracing::info!(path = %path, fd, driver = %name, "opened DRM device");
                name
            },
            Err(e) => {
                tracing::warn!(path = %path, fd, error = %e, "could not query DRM driver name");
                String::from("unknown")
            },
        };

        // Warn if not vc4 — the expected driver on Pi 4.
        if driver_name != "vc4" && driver_name != "vkms" {
            tracing::warn!(
                driver = %driver_name,
                "unexpected DRM driver — expected 'vc4' (Pi 4) or 'vkms' (testing). \
                 Display operations may not work correctly."
            );
        }

        Ok(Self {
            device_path: path,
            driver_name,
            drm_fd: Some(card),
            connectors: Vec::new(),
            planes: Vec::new(),
            crtcs: Vec::new(),
            active_crtc: None,
            active_mode: None,
            saved_crtc: None,
            gbm_device: None,
        })
    }

    /// Return the DRM driver name (e.g. "vc4", "vkms", "unknown").
    pub fn driver_name(&self) -> &str {
        &self.driver_name
    }

    /// Verify that the DRM driver matches the expected name.
    ///
    /// Returns `Ok(())` if the driver matches, or `Err(DisplayError::WrongDriver)`
    /// if it does not. This is a hard check — use `driver_name()` for a soft check.
    pub fn verify_driver(&self, expected: &str) -> Result<(), DisplayError> {
        if self.driver_name == expected {
            Ok(())
        } else {
            Err(DisplayError::WrongDriver {
                expected: expected.to_string(),
                actual: self.driver_name.clone(),
            })
        }
    }

    /// Enumerate available DRM planes.
    ///
    /// Returns information about each plane including its Z-position,
    /// supported formats, and which CRTCs it can be used with.
    pub fn planes(&self) -> Result<&[DrmPlane], DisplayError> {
        Ok(&self.planes)
    }

    /// Find the primary (UI/OSD) plane.
    ///
    /// Returns the first plane with `is_primary == true`, which is
    /// typically Plane 0 on Pi 4 with the vc4 driver.
    pub fn primary_plane(&self) -> Option<&DrmPlane> {
        self.planes.iter().find(|p| p.is_primary)
    }

    /// Find the best video (overlay) plane.
    ///
    /// Returns the first non-primary plane that supports NV12 or
    /// another video format. On Pi 4, this is typically Plane 1.
    pub fn video_plane(&self) -> Option<&DrmPlane> {
        self.planes.iter().find(|p| !p.is_primary && p.supports_video())
    }

    /// Enumerate available CRTCs.
    pub fn crtcs(&self) -> Result<&[DrmCrtc], DisplayError> {
        Ok(&self.crtcs)
    }

    /// Enumerate connected display connectors.
    pub fn connectors(&self) -> Result<&[DisplayConnector], DisplayError> {
        Ok(&self.connectors)
    }

    /// Acquire the primary CRTC and configure it for video output.
    ///
    /// This method:
    /// 1. Tries to acquire DRM master (best-effort — continues without it).
    /// 2. Finds the first connected HDMI connector with a preferred mode.
    /// 3. Selects 1080p60 if available, otherwise the best available mode.
    /// 4. Enumerates planes and CRTCs, validates video format support.
    /// 5. Saves the current CRTC state for restoration (only if master).
    /// 6. Drops DRM master and closes the fd so kmssink can acquire master.
    ///
    /// Must be called before any video output can occur.
    ///
    /// DRM master is **not required** for resource enumeration — the
    /// `get_connector`, `get_crtc`, and `get_plane` ioctls are read-only
    /// and work for any process that has the device open. Master is only
    /// needed for modesetting (`set_crtc`, `atomic_commit`), which kmssink
    /// handles independently when it starts playback.
    ///
    /// If we cannot get DRM master (e.g. the console framebuffer holds it
    /// and we lack CAP_SYS_ADMIN), we log a warning and proceed. kmssink
    /// will attempt to acquire master itself when it transitions to Playing;
    /// if it also fails, the GStreamer pipeline will fail, and our HW→SW
    /// fallback will kick in.
    pub fn acquire(&mut self) -> Result<(), DisplayError> {
        // Idempotent: if we already acquired and cached connector/CRTC info,
        // return Ok immediately.  After the first acquire(), the DRM fd is
        // closed so kmssink can open it fresh.  A second call from the
        // session manager must not fail just because the fd is gone.
        if self.active_crtc.is_some() && !self.connectors.is_empty() {
            tracing::debug!("display already acquired — skipping re-acquire");
            return Ok(());
        }

        // If the fd was closed by a previous acquire() (to let kmssink
        // become the first DRM opener), re-open the device now.
        if self.drm_fd.is_none() {
            tracing::info!("re-opening DRM device for re-acquire");
            let file =
                OpenOptions::new().read(true).write(true).open(&self.device_path).map_err(|e| {
                    DisplayError::DeviceOpen(format!(
                        "re-open {} for re-acquire: {}",
                        self.device_path, e
                    ))
                })?;
            self.drm_fd = Some(Card(file));
        }

        let fd = self
            .drm_fd
            .as_ref()
            .ok_or_else(|| DisplayError::DeviceOpen("DRM device not open".into()))?;

        // Try to acquire DRM master with retry — another process (console
        // framebuffer via fbcon, gmediarender) may still hold it.  We try
        // up to 10 times with exponential backoff (200 ms → 400 → 800 → …
        // capped at 2 s) to handle two distinct scenarios:
        //
        //   A) gmediarender just exited but the kernel hasn't released the
        //      master lock yet (typically < 200 ms).
        //   B) The kernel fbcon (framebuffer console) holds DRM master on
        //      the vc4 device.  The ExecStartPre in bogdan.service unbinds
        //      fbcon, but that may take a moment to propagate.
        //
        // If we cannot get master, we continue anyway — resource enumeration
        // works without it, and kmssink will try to get master itself during
        // playback. This is important because on Linux, drmSetMaster returns
        // EBUSY when another process is master, even with CAP_SYS_ADMIN.
        //
        // NOTE: CAP_SYS_ADMIN does NOT allow stealing DRM master from
        // another process in current Linux kernels (6.x). The only way to
        // release master held by the console is to unbind fbcon from the
        // DRM device, which the systemd unit's ExecStartPre handles.
        let mut has_master = false;
        let max_attempts: u32 = 10;
        let mut backoff_ms: u64 = 200;
        let mut total_waited_ms: u64 = 0;
        for attempt in 1..=max_attempts {
            match fd.acquire_master_lock() {
                Ok(()) => {
                    tracing::info!(
                        fd = fd.as_raw_fd(),
                        attempt,
                        total_waited_ms,
                        "acquired DRM master (temporary)"
                    );
                    has_master = true;
                    break;
                },
                Err(e) => {
                    if attempt < max_attempts {
                        tracing::warn!(
                            attempt = attempt,
                            backoff_ms,
                            error = %e,
                            "DRM master busy — retrying"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                        total_waited_ms += backoff_ms;
                        backoff_ms = (backoff_ms * 2).min(2000);
                    } else {
                        tracing::warn!(
                            attempt = max_attempts,
                            total_waited_ms,
                            error = %e,
                            "DRM master acquisition failed after 10 attempts — \
                             proceeding without master (kmssink will try during playback). \
                             Hint: ensure the systemd unit's ExecStartPre unbinds fbcon \
                             from the vc4 DRM device so the console releases DRM master."
                        );
                    }
                },
            }
        }

        // Set client capabilities — these don't require DRM master.
        for cap in [drm::ClientCapability::UniversalPlanes, drm::ClientCapability::Atomic] {
            if let Err(e) = fd.set_client_capability(cap, true) {
                tracing::debug!(capability = ?cap, error = %e, "DRM client capability not enabled");
            }
        }

        // Enumerate DRM resources.
        let resources = fd
            .resource_handles()
            .map_err(|e| DisplayError::Modeset(format!("failed to get resource handles: {}", e)))?;

        // Find connected connectors, preferring HDMI.
        let mut found_connectors = Vec::new();
        for &conn_handle in resources.connectors() {
            let info = fd
                .get_connector(conn_handle, true)
                .map_err(|e| DisplayError::Modeset(format!("connector info failed: {}", e)))?;

            let connected = info.state() == ConnectorState::Connected;
            let conn_type = format!("{}-{}", info.interface().as_str(), info.interface_id());

            // Build list of all available modes for this connector.
            let mut modes: Vec<DisplayMode> = info
                .modes()
                .iter()
                .map(|m| DisplayMode::new(m.size().0 as u32, m.size().1 as u32, m.vrefresh()))
                .collect();

            // Sort modes by preference (1080p60 first, then by resolution/refresh).
            modes.sort_by(|a, b| b.preference_cmp(a));

            let preferred_mode = modes.first().cloned();

            found_connectors.push(DisplayConnector {
                connector_id: conn_handle.into(),
                connector_type: conn_type,
                connected,
                modes,
                preferred_mode,
            });
        }
        self.connectors = found_connectors;

        // Find the best connected connector, preferring HDMI.
        let connector = self
            .connectors
            .iter()
            .filter(|c| c.connected)
            .max_by(|a, b| {
                // Prefer HDMI connectors over DSI or other types.
                let a_hdmi = a.is_hdmi() as u8;
                let b_hdmi = b.is_hdmi() as u8;
                b_hdmi.cmp(&a_hdmi)
            })
            .ok_or(DisplayError::NoConnector)?;

        tracing::info!(
            connector_id = connector.connector_id,
            connector_type = %connector.connector_type,
            mode_count = connector.modes.len(),
            "found connected display"
        );

        // Select the best display mode (1080p60 preferred).
        let selected_mode = connector.best_mode().ok_or(DisplayError::NoMode)?;

        tracing::info!(
            selected_mode = %selected_mode,
            is_1080p60 = selected_mode.is_1080p60(),
            "selected display mode"
        );

        self.active_mode = Some(selected_mode.clone());

        // Enumerate CRTCs.
        let mut found_crtcs = Vec::new();
        for &crtc_handle in resources.crtcs() {
            let info = fd
                .get_crtc(crtc_handle)
                .map_err(|e| DisplayError::Modeset(format!("crtc info failed: {}", e)))?;

            found_crtcs.push(DrmCrtc {
                crtc_id: crtc_handle.into(),
                width: info.mode().map(|m| m.size().0 as u32).unwrap_or(0),
                height: info.mode().map(|m| m.size().1 as u32).unwrap_or(0),
                refresh_mhz: info.mode().map(|m| m.vrefresh()).unwrap_or(0),
                fb_id: info.framebuffer().map(|fb| fb.into()),
            });
        }
        self.crtcs = found_crtcs;

        // Enumerate planes with real zpos values.
        let plane_handles = fd
            .plane_handles()
            .map_err(|e| DisplayError::Modeset(format!("plane resources failed: {}", e)))?;

        let mut found_planes = Vec::new();
        for plane_handle in plane_handles {
            let info = fd
                .get_plane(plane_handle)
                .map_err(|e| DisplayError::Modeset(format!("plane info failed: {}", e)))?;

            let plane_type = Self::plane_type(fd, plane_handle).unwrap_or(PlaneType::Overlay);
            let is_primary = plane_type == PlaneType::Primary;
            let type_name = match plane_type {
                PlaneType::Primary => "Primary",
                PlaneType::Overlay => "Overlay",
                PlaneType::Cursor => "Cursor",
            };
            let possible_handles = resources.filter_crtcs(info.possible_crtcs());
            let possible_crtcs = resources
                .crtcs()
                .iter()
                .enumerate()
                .filter(|(_, handle)| possible_handles.contains(handle))
                .fold(0u32, |mask, (idx, _)| mask | (1u32 << idx));

            // Read the real zpos property from the kernel.
            let zpos = Self::plane_zpos(fd, plane_handle).unwrap_or(if is_primary { 0 } else { 1 });

            let formats: Vec<u32> = info.formats().to_vec();

            found_planes.push(DrmPlane {
                plane_id: plane_handle.into(),
                zpos,
                formats,
                is_primary,
                possible_crtcs,
                type_name: type_name.to_string(),
            });
        }
        self.planes = found_planes;

        // Validate that we have the required planes.
        if self.primary_plane().is_none() {
            tracing::warn!("no primary plane found — OSD overlay may not work");
        }
        if self.video_plane().is_none() {
            // Check if any non-primary plane supports video formats.
            let overlay_planes: Vec<_> =
                self.planes.iter().filter(|p| !p.is_primary).collect();
            if overlay_planes.is_empty() {
                tracing::warn!("no overlay/video plane found — video may render on primary plane");
            } else {
                let format_names: Vec<String> = overlay_planes
                    .iter()
                    .flat_map(|p| p.format_names())
                    .collect();
                tracing::warn!(
                    formats = ?format_names,
                    "no overlay plane supports video formats (NV12/NV21) — \
                     V4L2 hardware decode may not work; software decode will be used"
                );
            }
        }

        // Log discovered plane information for debugging.
        for plane in &self.planes {
            tracing::info!(
                plane_id = plane.plane_id,
                type = %plane.type_name,
                zpos = plane.zpos,
                formats = ?plane.format_names(),
                supports_video = plane.supports_video(),
                supports_graphics = plane.supports_graphics(),
                "discovered DRM plane"
            );
        }

        // Select the best CRTC for our connector.
        let crtc = self.crtcs.first().ok_or(DisplayError::NoCrtc)?.clone();

        // Save current CRTC state for restoration on release().
        // Only possible if we have DRM master — get_crtc() for saving works
        // without master, but set_crtc() for restoration requires master.
        if has_master {
            let crtc_handle =
                control::from_u32::<control::crtc::Handle>(crtc.crtc_id).ok_or_else(|| {
                    DisplayError::Modeset(format!("invalid CRTC id {}", crtc.crtc_id))
                })?;
            let crtc_info = fd.get_crtc(crtc_handle).ok();
            let saved_mode = crtc_info.as_ref().and_then(|i| i.mode());
            // If get_crtc() didn't return a mode (e.g. fbcon never set
            // one, or the CRTC is inactive), fall back to the connector's
            // current mode. This ensures release() can restore a valid
            // mode instead of logging "no saved mode — cannot restore CRTC".
            let mode = saved_mode.unwrap_or_else(|| {
                tracing::info!(
                    crtc_id = crtc.crtc_id,
                    "CRTC has no active mode from get_crtc() — using connector's current mode for restore"
                );
                selected_mode.clone()
            });
            self.saved_crtc = Some(SavedCrtcState {
                crtc_id: crtc.crtc_id,
                fb_id: crtc_info.as_ref().and_then(|i| i.framebuffer().map(|fb| fb.into())),
                mode: Some(mode),
                x: 0,
                y: 0,
                connector_id: connector.connector_id,
            });
        } else {
            tracing::info!("skipping CRTC state save — no DRM master");
        }

        self.active_crtc = Some(crtc.clone());

        tracing::info!(
            crtc_id = crtc.crtc_id,
            mode = %selected_mode,
            has_master = has_master,
            planes = self.planes.len(),
            "acquired CRTC for display"
        );

        // Drop DRM master so kmssink (GStreamer) can acquire it for playback.
        // If we hold master, kmssink's set_state(Playing) will fail because
        // only one FD can be DRM master at a time.
        //
        // We also close our DRM device fd entirely.  When our process
        // still has the device open (even without master), kmssink opens
        // the device as a *subsequent* opener and does NOT automatically
        // get DRM master.  On Linux ≥ 4.17, drmSetMaster() requires
        // either the caller to be the current master or have CAP_SYS_ADMIN.
        // Even with CAP_SYS_ADMIN, drmSetMaster returns EBUSY if another
        // process already holds master — which is the case when fbcon
        // (framebuffer console) is still bound to the vc4 device.
        //
        // By closing our fd completely, kmssink becomes the *first* (and
        // only) opener of the DRM device and is automatically granted
        // DRM master by the kernel — no drmSetMaster() call needed.
        // The saved CRTC state is preserved in self.saved_crtc for
        // restoration in release(), which re-opens the device.
        if has_master {
            if let Err(e) = fd.release_master_lock() {
                tracing::warn!(error = %e, "failed to drop DRM master after saving CRTC state");
            }
        }
        // Close our fd so kmssink opens the device fresh and gets DRM
        // master automatically as the first opener.
        self.drm_fd = None;
        tracing::info!("closed DRM device fd — kmssink will open it fresh and acquire DRM master automatically");

        // Initialize GBM device for OSD overlay surface allocation.
        // GBM now uses the DRM render node (/dev/dri/renderD128) which
        // does NOT acquire DRM master, so it's safe to call here —
        // kmssink will still be the first (and only) card node opener
        // and will get DRM master automatically.
        if let Err(e) = self.init_gbm() {
            tracing::warn!(error = %e, "GBM initialization failed — OSD overlay will not be available");
        }

        Ok(())
    }

    /// Release the CRTC and restore the previous framebuffer.
    ///
    /// Should be called on shutdown to avoid leaving the display in
    /// an inconsistent state.
    pub fn release(&mut self) -> Result<(), DisplayError> {
        if let Some(ref saved) = self.saved_crtc {
            tracing::info!(
                crtc_id = saved.crtc_id,
                fb_id = ?saved.fb_id,
                has_mode = saved.mode.is_some(),
                "restoring saved CRTC state"
            );
            // Restore CRTC state via set_crtc.
            // This requires the DRM fd to be valid.  If we closed the fd
            // in acquire() (to let kmssink become the first opener and get
            // DRM master automatically), re-open the device now.  The
            // pipeline should have been stopped (NULL state) by this point,
            // so kmssink has released its fd and the kernel has dropped
            // DRM master — we can re-open and acquire master ourselves.
            if self.drm_fd.is_none() {
                tracing::info!("re-opening DRM device for CRTC restoration");
                let file =
                    OpenOptions::new().read(true).write(true).open(&self.device_path).map_err(
                        |e| {
                            DisplayError::DeviceOpen(format!(
                                "re-open {} for CRTC restore: {}",
                                self.device_path, e
                            ))
                        },
                    )?;
                self.drm_fd = Some(Card(file));
            }
            if let Some(ref fd) = self.drm_fd {
                // Re-acquire DRM master temporarily to restore the CRTC.
                // kmssink should have released it when the pipeline went to
                // NULL, but we handle the case where it still holds master.
                let has_master = fd.acquire_master_lock().is_ok();
                if !has_master {
                    tracing::warn!(
                        "could not re-acquire DRM master for CRTC restore — \
                         kmssink may still hold it; skipping CRTC restore"
                    );
                } else {
                    let crtc_handle =
                        match control::from_u32::<control::crtc::Handle>(saved.crtc_id) {
                            Some(handle) => handle,
                            None => {
                                tracing::warn!(crtc_id = saved.crtc_id, "invalid saved CRTC id");
                                let _ = fd.release_master_lock();
                                self.active_crtc = None;
                                self.active_mode = None;
                                self.saved_crtc = None;
                                self.gbm_device = None;
                                return Ok(());
                            },
                        };
                    if let Some(mode) = saved.mode {
                        let framebuffer =
                            saved.fb_id.and_then(control::from_u32::<control::framebuffer::Handle>);
                        let restore_result = fd.set_crtc(
                            crtc_handle,
                            framebuffer,
                            (saved.x, saved.y),
                            &[],
                            Some(mode),
                        );
                        match restore_result {
                            Ok(()) => {
                                tracing::info!(crtc_id = saved.crtc_id, "CRTC state restored")
                            },
                            Err(e) => tracing::warn!(
                                crtc_id = saved.crtc_id,
                                error = %e,
                                "failed to restore CRTC state"
                            ),
                        }
                    } else {
                        tracing::warn!(
                            crtc_id = saved.crtc_id,
                            "no saved mode — cannot restore CRTC"
                        );
                    }
                    // Drop DRM master after restoration.
                    let _ = fd.release_master_lock();
                }
            } else {
                tracing::warn!("DRM fd no longer available — cannot restore CRTC state");
            }
        }
        self.active_crtc = None;
        self.active_mode = None;
        self.saved_crtc = None;
        self.gbm_device = None;
        Ok(())
    }

    /// Return the current display resolution as `(width, height)`.
    ///
    /// If the display has been acquired, returns the active mode.
    /// Otherwise returns a default (1920x1080) as a hint.
    pub fn resolution(&self) -> Result<(u32, u32), DisplayError> {
        if let Some(ref mode) = self.active_mode {
            return Ok((mode.width, mode.height));
        }

        if let Some(ref crtc) = self.active_crtc {
            if crtc.width > 0 && crtc.height > 0 {
                return Ok((crtc.width, crtc.height));
            }
        }

        // Check connectors for preferred mode.
        if let Some(conn) = self.connectors.iter().find(|c| c.connected) {
            if let Some(ref mode) = conn.preferred_mode {
                return Ok((mode.width, mode.height));
            }
        }

        Ok((1920, 1080))
    }

    /// Return the DRM device path.
    pub fn device_path(&self) -> &str {
        &self.device_path
    }

    /// Return the active CRTC, if any.
    pub fn active_crtc(&self) -> Option<&DrmCrtc> {
        self.active_crtc.as_ref()
    }

    /// Return the active display mode, if set.
    pub fn active_mode(&self) -> Option<&DisplayMode> {
        self.active_mode.as_ref()
    }

    /// Return the connector ID of the first connected display, if known.
    ///
    /// Only available after `acquire()` has been called.
    pub fn active_connector_id(&self) -> Option<u32> {
        self.connectors.iter().find(|c| c.connected).map(|c| c.connector_id)
    }

    /// Return the raw DRM device file descriptor, if the device is open.
    ///
    /// Returns `None` after `acquire()` closes the fd to let kmssink
    /// open the device fresh (see acquire() documentation).
    pub fn drm_fd(&self) -> Option<i32> {
        self.drm_fd.as_ref().map(|card| card.as_raw_fd())
    }

    /// Check if GBM is available for OSD overlay surface allocation.
    pub fn has_gbm(&self) -> bool {
        self.gbm_device.is_some()
    }

    /// Clear the screen by filling the primary plane with black.
    ///
    /// Uses a dumb buffer filled with zeros and sets it as the
    /// primary plane's framebuffer.
    pub fn clear_screen(&mut self) -> Result<(), DisplayError> {
        // Dumb buffer creation and plane set would go here.
        // For v1, kmssink handles the video plane directly.
        tracing::debug!("clear_screen called — handled by kmssink in v1");
        Ok(())
    }

    // ── Private helpers ──────────────────────────────────────────────

    /// Auto-detect the DRM device node.
    ///
    /// On Raspberry Pi 4B+ with the vc4 driver:
    /// - `/dev/dri/card0` is usually the firmware framebuffer (simplefb)
    /// - `/dev/dri/card1` is the vc4 KMS device
    ///
    /// This function prefers card1 if it exists, falling back to card0.
    fn find_dri_device() -> Result<String, DisplayError> {
        let candidates = [
            "/dev/dri/card1", // vc4 on Pi 4
            "/dev/dri/card0", // fallback / vkms
        ];

        for candidate in &candidates {
            if Path::new(candidate).exists() {
                tracing::info!(path = candidate, "auto-detected DRM device");
                return Ok(candidate.to_string());
            }
        }

        Err(DisplayError::DeviceOpen(
            "no /dev/dri/card* device found — is the vc4 driver loaded?".into(),
        ))
    }

    /// Read the zpos property for a plane from the kernel.
    ///
    /// Returns `None` if the zpos property is not available (some
    /// drivers don't expose it). Falls back to a default based on
    /// plane type.
    fn plane_zpos(card: &Card, handle: control::plane::Handle) -> Option<u32> {
        let props = card.get_properties(handle).ok()?;
        for (&prop_handle, &raw_value) in props.iter() {
            let prop = card.get_property(prop_handle).ok()?;
            if prop.name().to_str().ok()? != "zpos" {
                continue;
            }
            // zpos is a range property; the raw value is the current zpos.
            if let control::property::Value::UnsignedRange(zpos_value) =
                prop.value_type().convert_value(raw_value)
            {
                return Some(zpos_value as u32);
            }
        }
        None
    }

    fn plane_type(card: &Card, handle: control::plane::Handle) -> Option<PlaneType> {
        let props = card.get_properties(handle).ok()?;
        for (&prop_handle, &raw_value) in props.iter() {
            let prop = card.get_property(prop_handle).ok()?;
            if prop.name().to_str().ok()? != "type" {
                continue;
            }

            let value_type = prop.value_type();
            if let control::property::Value::Enum(Some(enum_value)) =
                value_type.convert_value(raw_value)
            {
                return match enum_value.name().to_str().ok()? {
                    "Primary" => Some(PlaneType::Primary),
                    "Overlay" => Some(PlaneType::Overlay),
                    "Cursor" => Some(PlaneType::Cursor),
                    _ => None,
                };
            }
        }

        None
    }

    /// Initialize GBM on the DRM device for OSD overlay surface allocation.
    ///
    /// **Critical**: Opens the DRM **render node** (e.g. `/dev/dri/renderD128`)
    /// instead of the card node (e.g. `/dev/dri/card1`). Render nodes are
    /// designed for GPU compute/rendering without modesetting and do NOT
    /// acquire DRM master. Using the card node would steal DRM master from
    /// kmssink, causing `drmModeSetPlane` to fail with EPERM.
    ///
    /// On Raspberry Pi 4 with vc4, the render node is `/dev/dri/renderD128`
    /// (associated with the v3d GPU driver). If the render node is not
    /// available (e.g. on vkms), falls back to the card node with a warning.
    fn init_gbm(&mut self) -> Result<(), DisplayError> {
        // Prefer the render node — it does NOT acquire DRM master.
        let render_path = self.find_render_node();
        let gbm_path = render_path.as_deref().unwrap_or(&self.device_path);

        if render_path.is_some() {
            tracing::info!(
                path = %gbm_path,
                "GBM: using DRM render node (no DRM master acquisition)"
            );
        } else {
            tracing::warn!(
                path = %gbm_path,
                "GBM: no render node found — falling back to card node. \
                 This may steal DRM master from kmssink and cause \
                 drmModeSetPlane EPERM errors."
            );
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(gbm_path)
            .map_err(|e| {
                DisplayError::GbmAlloc(format!("cannot open DRM for GBM: {}: {}", gbm_path, e))
            })?;

        let card = Card(file);
        let gbm_dev = gbm::Device::new(card).map_err(|e| {
            DisplayError::GbmAlloc(format!("gbm::Device::new failed: {}", e))
        })?;

        tracing::info!("GBM device initialized for OSD overlay surfaces");
        self.gbm_device = Some(GbmDevice { _device: gbm_dev });
        Ok(())
    }

    /// Find the DRM render node associated with the current device.
    ///
    /// On Raspberry Pi 4 with vc4, the render node is typically
    /// `/dev/dri/renderD128` (v3d GPU). Render nodes do NOT acquire
    /// DRM master and are safe to open alongside kmssink.
    ///
    /// Returns `None` if no render node is found.
    fn find_render_node(&self) -> Option<String> {
        let dri_dir = Path::new("/dev/dri");
        if !dri_dir.exists() {
            return None;
        }

        // Scan /dev/dri for render node entries (renderD*).
        let entries = std::fs::read_dir(dri_dir).ok()?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("renderD") {
                let path = format!("/dev/dri/{}", name_str);
                if Path::new(&path).exists() {
                    tracing::debug!(path = %path, "found DRM render node");
                    return Some(path);
                }
            }
        }

        None
    }

    /// Allocate a GBM surface for the OSD overlay plane.
    ///
    /// The surface is created with ARGB8888 format and scanout capability,
    /// sized to the current display mode. Returns the surface dimensions
    /// on success.
    pub fn allocate_osd_surface(&mut self) -> Result<(u32, u32), DisplayError> {
        let gbm_dev = self
            .gbm_device
            .as_ref()
            .ok_or_else(|| DisplayError::GbmAlloc("GBM device not initialized".into()))?;

        let mode = self.active_mode.as_ref().ok_or_else(|| {
            DisplayError::GbmAlloc("no active display mode — call acquire() first".into())
        })?;

        let width = mode.width;
        let height = mode.height;

        // ARGB8888 with scanout + rendering flags.
        let surface = gbm_dev
            ._device
            .create_surface::<()>(
                width,
                height,
                gbm::Format::Argb8888,
                gbm::BufferObjectFlags::SCANOUT | gbm::BufferObjectFlags::RENDERING,
            )
            .map_err(|e| {
                DisplayError::GbmAlloc(format!(
                    "GBM surface allocation failed ({}x{} ARGB8888): {}",
                    width, height, e
                ))
            })?;

        tracing::info!(
            width,
            height,
            format = "ARGB8888",
            "allocated GBM OSD overlay surface"
        );

        // Store the surface (we could cache it for later use).
        let _osd_surface = GbmSurface { _surface: surface, width, height, format: fourcc::AR24 };

        Ok((width, height))
    }
}

// ── Mock implementation (no hw feature) ──────────────────────────────

#[cfg(not(feature = "hw"))]
impl DisplayManager {
    /// Create a mock display manager.
    ///
    /// In mock mode, the manager does not open any DRM device.
    /// All operations return success with mock data that simulates
    /// a Pi 4B+ with vc4 driver connected to a 1080p60 HDMI monitor.
    pub fn new(device_path: &str) -> Result<Self, DisplayError> {
        let path = if device_path.is_empty() {
            "/dev/dri/card0".to_owned()
        } else {
            device_path.to_owned()
        };

        tracing::info!(path = %path, "created mock display manager (hw feature disabled)");

        // Pre-populate with mock data that simulates Pi 4B+ hardware.
        let connectors = vec![DisplayConnector {
            connector_id: 89,
            connector_type: "HDMI-A-1".into(),
            connected: true,
            modes: vec![
                DisplayMode::mode_1080p60(),
                DisplayMode::mode_720p60(),
                DisplayMode::new(1920, 1080, 50000),
                DisplayMode::new(1280, 720, 50000),
            ],
            preferred_mode: Some(DisplayMode::mode_1080p60()),
        }];

        let planes = vec![
            DrmPlane {
                plane_id: 31,
                zpos: 0,
                formats: vec![fourcc::XR24, fourcc::AR24],
                is_primary: true,
                possible_crtcs: 0x1,
                type_name: "Primary".to_string(),
            },
            DrmPlane {
                plane_id: 32,
                zpos: 1,
                formats: vec![fourcc::NV12, fourcc::NV21],
                is_primary: false,
                possible_crtcs: 0x1,
                type_name: "Overlay".to_string(),
            },
        ];

        let crtcs = vec![DrmCrtc {
            crtc_id: 56,
            width: 1920,
            height: 1080,
            refresh_mhz: 60000,
            fb_id: None,
        }];

        Ok(Self {
            device_path: path,
            driver_name: "vc4".to_string(),
            connectors,
            planes,
            crtcs,
            active_crtc: None,
            active_mode: None,
        })
    }

    /// Return the DRM driver name ("vc4" in mock mode).
    pub fn driver_name(&self) -> &str {
        &self.driver_name
    }

    /// Verify that the DRM driver matches the expected name.
    pub fn verify_driver(&self, expected: &str) -> Result<(), DisplayError> {
        if self.driver_name == expected {
            Ok(())
        } else {
            Err(DisplayError::WrongDriver {
                expected: expected.to_string(),
                actual: self.driver_name.clone(),
            })
        }
    }

    /// Enumerate available DRM planes (mock data simulating Pi 4B+).
    pub fn planes(&self) -> Result<&[DrmPlane], DisplayError> {
        Ok(&self.planes)
    }

    /// Find the primary (UI/OSD) plane (mock).
    pub fn primary_plane(&self) -> Option<&DrmPlane> {
        self.planes.iter().find(|p| p.is_primary)
    }

    /// Find the best video (overlay) plane (mock).
    pub fn video_plane(&self) -> Option<&DrmPlane> {
        self.planes.iter().find(|p| !p.is_primary && p.supports_video())
    }

    /// Enumerate available CRTCs (mock data simulating Pi 4B+).
    pub fn crtcs(&self) -> Result<&[DrmCrtc], DisplayError> {
        Ok(&self.crtcs)
    }

    /// Enumerate connected display connectors (mock data simulating Pi 4B+).
    pub fn connectors(&self) -> Result<&[DisplayConnector], DisplayError> {
        Ok(&self.connectors)
    }

    /// Acquire the primary CRTC (simulated in mock mode).
    ///
    /// Sets the first CRTC as active, simulating a successful
    /// atomic modeset to 1080p60.
    pub fn acquire(&mut self) -> Result<(), DisplayError> {
        tracing::debug!("acquire called in mock mode — simulating modeset");
        if let Some(crtc) = self.crtcs.first() {
            self.active_crtc = Some(crtc.clone());
        }
        // Select 1080p60 as the active mode.
        self.active_mode = Some(DisplayMode::mode_1080p60());
        Ok(())
    }

    /// Release the CRTC (no-op in mock mode).
    pub fn release(&mut self) -> Result<(), DisplayError> {
        tracing::debug!("release called in mock mode — no-op");
        self.active_crtc = None;
        self.active_mode = None;
        Ok(())
    }

    /// Return the current display resolution (default 1920x1080 in mock mode).
    pub fn resolution(&self) -> Result<(u32, u32), DisplayError> {
        if let Some(ref mode) = self.active_mode {
            return Ok((mode.width, mode.height));
        }
        Ok((1920, 1080))
    }

    /// Return the DRM device path.
    pub fn device_path(&self) -> &str {
        &self.device_path
    }

    /// Return the active CRTC.
    pub fn active_crtc(&self) -> Option<&DrmCrtc> {
        self.active_crtc.as_ref()
    }

    /// Return the active display mode.
    pub fn active_mode(&self) -> Option<&DisplayMode> {
        self.active_mode.as_ref()
    }

    /// Clear the screen (no-op in mock mode).
    pub fn clear_screen(&mut self) -> Result<(), DisplayError> {
        tracing::debug!("clear_screen called in mock mode — no-op");
        Ok(())
    }

    /// Return the connector ID of the first connected display (mock data).
    ///
    /// Returns the mock connector ID (89) to simulate the hw implementation.
    pub fn active_connector_id(&self) -> Option<u32> {
        self.connectors.iter().find(|c| c.connected).map(|c| c.connector_id)
    }

    /// Check if GBM is available (always false in mock mode).
    pub fn has_gbm(&self) -> bool {
        false
    }

    /// Allocate a GBM OSD surface (not available in mock mode).
    pub fn allocate_osd_surface(&mut self) -> Result<(u32, u32), DisplayError> {
        Err(DisplayError::GbmAlloc("GBM not available in mock mode".into()))
    }
}

impl Drop for DisplayManager {
    fn drop(&mut self) {
        if self.active_crtc.is_some() {
            tracing::warn!(
                "DisplayManager dropped while CRTC is still active — attempting release"
            );
            let _ = self.release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Error variant tests ──────────────────────────────────────────

    #[test]
    fn display_error_variants() {
        let err = DisplayError::DeviceOpen("/dev/dri/card0".into());
        assert!(err.to_string().contains("failed to open DRM device"));

        let err = DisplayError::MasterAcquire("denied".into());
        assert!(err.to_string().contains("failed to acquire DRM master"));

        let err = DisplayError::WrongDriver {
            expected: "vc4".into(),
            actual: "i915".into(),
        };
        assert!(err.to_string().contains("unexpected DRM driver"));
        assert!(err.to_string().contains("vc4"));
        assert!(err.to_string().contains("i915"));

        let err = DisplayError::Modeset("mode rejected".into());
        assert!(err.to_string().contains("DRM mode-setting failed"));

        let err = DisplayError::NoCrtc;
        assert!(err.to_string().contains("no available CRTC"));

        let err = DisplayError::NoConnector;
        assert!(err.to_string().contains("no connected connector"));

        let err = DisplayError::NoPlane;
        assert!(err.to_string().contains("no available plane"));

        let err = DisplayError::NoVideoPlane("NV12".into());
        assert!(err.to_string().contains("no plane supports required video format"));

        let err = DisplayError::NoGraphicsPlane("ARGB8888".into());
        assert!(err.to_string().contains("no plane supports required graphics format"));

        let err = DisplayError::NoMode;
        assert!(err.to_string().contains("no display mode"));

        let err = DisplayError::GbmAlloc("out of memory".into());
        assert!(err.to_string().contains("GBM allocation failed"));

        let err = DisplayError::HardwareUnavailable;
        assert!(err.to_string().contains("hardware display unavailable"));
    }

    // ── DrmPlane tests ───────────────────────────────────────────────

    #[test]
    fn drm_plane_fields() {
        let plane = DrmPlane {
            plane_id: 42,
            zpos: 1,
            formats: vec![fourcc::XR24],
            is_primary: false,
            possible_crtcs: 0x1,
            type_name: "Overlay".to_string(),
        };
        assert_eq!(plane.plane_id, 42);
        assert_eq!(plane.zpos, 1);
        assert!(!plane.is_primary);
        assert_eq!(plane.type_name, "Overlay");
    }

    #[test]
    fn drm_plane_supports_format() {
        let plane = DrmPlane {
            plane_id: 32,
            zpos: 1,
            formats: vec![fourcc::NV12, fourcc::NV21],
            is_primary: false,
            possible_crtcs: 0x1,
            type_name: "Overlay".to_string(),
        };
        assert!(plane.supports_format(fourcc::NV12));
        assert!(plane.supports_format(fourcc::NV21));
        assert!(!plane.supports_format(fourcc::XR24));
    }

    #[test]
    fn drm_plane_supports_video() {
        let video_plane = DrmPlane {
            plane_id: 32,
            zpos: 1,
            formats: vec![fourcc::NV12, fourcc::NV21],
            is_primary: false,
            possible_crtcs: 0x1,
            type_name: "Overlay".to_string(),
        };
        assert!(video_plane.supports_video());
        assert!(!video_plane.supports_graphics());

        let primary_plane = DrmPlane {
            plane_id: 31,
            zpos: 0,
            formats: vec![fourcc::XR24, fourcc::AR24],
            is_primary: true,
            possible_crtcs: 0x1,
            type_name: "Primary".to_string(),
        };
        assert!(!primary_plane.supports_video());
        assert!(primary_plane.supports_graphics());
    }

    #[test]
    fn drm_plane_format_names() {
        let plane = DrmPlane {
            plane_id: 31,
            zpos: 0,
            formats: vec![fourcc::XR24, fourcc::AR24],
            is_primary: true,
            possible_crtcs: 0x1,
            type_name: "Primary".to_string(),
        };
        let names = plane.format_names();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"XR24".to_string()));
        assert!(names.contains(&"AR24".to_string()));
    }

    // ── DrmCrtc tests ────────────────────────────────────────────────

    #[test]
    fn drm_crtc_fields() {
        let crtc =
            DrmCrtc { crtc_id: 55, width: 1920, height: 1080, refresh_mhz: 60000, fb_id: Some(99) };
        assert_eq!(crtc.crtc_id, 55);
        assert_eq!(crtc.width, 1920);
        assert_eq!(crtc.height, 1080);
        assert_eq!(crtc.fb_id, Some(99));
    }

    // ── DisplayMode tests ────────────────────────────────────────────

    #[test]
    fn display_mode_standard_modes() {
        let m = DisplayMode::mode_1080p60();
        assert_eq!(m.width, 1920);
        assert_eq!(m.height, 1080);
        assert!(m.is_1080p60());

        let m = DisplayMode::mode_720p60();
        assert_eq!(m.width, 1280);
        assert_eq!(m.height, 720);
        assert!(!m.is_1080p60());

        let m = DisplayMode::mode_4k30();
        assert_eq!(m.width, 3840);
        assert_eq!(m.height, 2160);
        assert!(!m.is_1080p60());
    }

    #[test]
    fn display_mode_refresh_hz() {
        let m = DisplayMode::new(1920, 1080, 60000);
        assert_eq!(m.refresh_hz(), 60);

        let m = DisplayMode::new(1920, 1080, 59940);
        assert_eq!(m.refresh_hz(), 59); // truncated, not rounded
    }

    #[test]
    fn display_mode_pixels() {
        let m = DisplayMode::mode_1080p60();
        assert_eq!(m.pixels(), 2_073_600);
    }

    #[test]
    fn display_mode_display_trait() {
        let m = DisplayMode::mode_1080p60();
        assert_eq!(format!("{}", m), "1920x1080@60Hz");
    }

    #[test]
    fn display_mode_preference_cmp_prefers_1080p60() {
        let mode_1080p60 = DisplayMode::mode_1080p60();
        let mode_4k30 = DisplayMode::mode_4k30();
        let mode_720p60 = DisplayMode::mode_720p60();

        // 1080p60 should be preferred over 4K30 and 720p60.
        assert_eq!(mode_1080p60.preference_cmp(&mode_4k30), std::cmp::Ordering::Greater);
        assert_eq!(mode_1080p60.preference_cmp(&mode_720p60), std::cmp::Ordering::Greater);
        // 4K30 should be preferred over 720p60 (more pixels).
        assert_eq!(mode_4k30.preference_cmp(&mode_720p60), std::cmp::Ordering::Greater);
    }

    #[test]
    fn display_mode_equality() {
        let m1 = DisplayMode::mode_1080p60();
        let m2 = DisplayMode::new(1920, 1080, 60000);
        assert_eq!(m1, m2);
    }

    // ── DisplayConnector tests ───────────────────────────────────────

    #[test]
    fn display_connector_is_hdmi() {
        let conn = DisplayConnector {
            connector_id: 89,
            connector_type: "HDMI-A-1".into(),
            connected: true,
            modes: vec![DisplayMode::mode_1080p60()],
            preferred_mode: Some(DisplayMode::mode_1080p60()),
        };
        assert!(conn.is_hdmi());

        let dsi_conn = DisplayConnector {
            connector_id: 90,
            connector_type: "DSI-1".into(),
            connected: true,
            modes: vec![DisplayMode::mode_1080p60()],
            preferred_mode: Some(DisplayMode::mode_1080p60()),
        };
        assert!(!dsi_conn.is_hdmi());
    }

    #[test]
    fn display_connector_best_mode() {
        let conn = DisplayConnector {
            connector_id: 89,
            connector_type: "HDMI-A-1".into(),
            connected: true,
            modes: vec![
                DisplayMode::mode_720p60(),
                DisplayMode::mode_1080p60(),
                DisplayMode::new(1920, 1080, 50000),
            ],
            preferred_mode: Some(DisplayMode::mode_1080p60()),
        };
        let best = conn.best_mode().expect("should find best mode");
        assert!(best.is_1080p60());
    }

    #[test]
    fn display_connector_best_mode_within() {
        let conn = DisplayConnector {
            connector_id: 89,
            connector_type: "HDMI-A-1".into(),
            connected: true,
            modes: vec![
                DisplayMode::mode_1080p60(),
                DisplayMode::mode_720p60(),
                DisplayMode::new(1920, 1080, 50000),
            ],
            preferred_mode: Some(DisplayMode::mode_1080p60()),
        };
        // Within 1280x720, should pick 720p60.
        let best = conn.best_mode_within(1280, 720).expect("should find a mode");
        assert_eq!(best.width, 1280);
        assert_eq!(best.height, 720);
    }

    // ── Fourcc helper tests ──────────────────────────────────────────

    #[test]
    fn fourcc_to_str() {
        assert_eq!(fourcc::to_str(fourcc::XR24), "XR24");
        assert_eq!(fourcc::to_str(fourcc::AR24), "AR24");
        assert_eq!(fourcc::to_str(fourcc::NV12), "NV12");
        assert_eq!(fourcc::to_str(fourcc::NV21), "NV21");
    }

    #[test]
    fn fourcc_classification() {
        assert!(fourcc::is_video_format(fourcc::NV12));
        assert!(fourcc::is_video_format(fourcc::NV21));
        assert!(fourcc::is_video_format(fourcc::P030));
        assert!(!fourcc::is_video_format(fourcc::XR24));
        assert!(!fourcc::is_video_format(fourcc::AR24));

        assert!(fourcc::is_graphics_format(fourcc::XR24));
        assert!(fourcc::is_graphics_format(fourcc::AR24));
        assert!(!fourcc::is_graphics_format(fourcc::NV12));
    }

    // ── DisplayManager mock mode tests ───────────────────────────────

    #[test]
    fn display_manager_new_succeeds_in_mock_mode() {
        // Without the hw feature, DisplayManager::new always succeeds
        // (mock mode — doesn't actually open a DRM device).
        let result = DisplayManager::new("/dev/dri/nonexistent");
        assert!(result.is_ok(), "mock DisplayManager::new should succeed");
    }

    #[test]
    fn display_manager_default_resolution_without_crtc() {
        let dm = DisplayManager::new("").unwrap();
        let res = dm.resolution().unwrap();
        assert_eq!(res, (1920, 1080));
    }

    #[test]
    fn display_manager_acquire_release_mock() {
        let mut dm = DisplayManager::new("").unwrap();
        assert!(dm.acquire().is_ok());
        assert!(dm.release().is_ok());
    }

    #[test]
    fn display_manager_clear_screen_mock() {
        let mut dm = DisplayManager::new("").unwrap();
        assert!(dm.clear_screen().is_ok());
    }

    #[test]
    fn display_manager_planes_crtcs_connectors_mock_data() {
        let dm = DisplayManager::new("").unwrap();
        // Mock mode now provides realistic Pi 4B+ mock data.
        assert_eq!(dm.planes().unwrap().len(), 2, "should have 2 mock planes");
        assert_eq!(dm.crtcs().unwrap().len(), 1, "should have 1 mock CRTC");
        assert_eq!(dm.connectors().unwrap().len(), 1, "should have 1 mock connector");

        // Verify mock plane properties.
        let primary = dm.primary_plane().expect("should have primary plane");
        assert_eq!(primary.zpos, 0);
        assert!(primary.supports_graphics());
        assert!(!primary.supports_video());
        assert_eq!(primary.type_name, "Primary");

        let video = dm.video_plane().expect("should have video plane");
        assert_eq!(video.zpos, 1);
        assert!(video.supports_video());
        assert!(!video.supports_graphics());
        assert_eq!(video.type_name, "Overlay");

        // Verify mock connector.
        let conn = dm.connectors().unwrap().first().unwrap();
        assert!(conn.connected);
        assert!(conn.is_hdmi());
        assert_eq!(conn.connector_type, "HDMI-A-1");
        assert_eq!(conn.modes.len(), 4, "should have 4 mock modes");
        assert!(conn.preferred_mode.is_some());

        // Verify mock CRTC.
        let crtc = dm.crtcs().unwrap().first().unwrap();
        assert_eq!(crtc.width, 1920);
        assert_eq!(crtc.height, 1080);
    }

    #[test]
    fn display_manager_acquire_sets_active_crtc_and_mode() {
        let mut dm = DisplayManager::new("").unwrap();
        assert!(dm.active_crtc().is_none(), "no active CRTC before acquire");
        assert!(dm.active_mode().is_none(), "no active mode before acquire");
        dm.acquire().unwrap();
        let crtc = dm.active_crtc().expect("should have active CRTC after acquire");
        assert_eq!(crtc.width, 1920);
        assert_eq!(crtc.height, 1080);
        let mode = dm.active_mode().expect("should have active mode after acquire");
        assert!(mode.is_1080p60());
    }

    #[test]
    fn display_manager_active_crtc_none() {
        let dm = DisplayManager::new("").unwrap();
        assert!(dm.active_crtc().is_none());
    }

    #[test]
    fn display_manager_device_path() {
        let dm = DisplayManager::new("/dev/dri/card1").unwrap();
        assert_eq!(dm.device_path(), "/dev/dri/card1");
    }

    #[test]
    fn display_manager_driver_name_mock() {
        let dm = DisplayManager::new("").unwrap();
        assert_eq!(dm.driver_name(), "vc4");
    }

    #[test]
    fn display_manager_verify_driver_success() {
        let dm = DisplayManager::new("").unwrap();
        assert!(dm.verify_driver("vc4").is_ok());
    }

    #[test]
    fn display_manager_verify_driver_failure() {
        let dm = DisplayManager::new("").unwrap();
        let err = dm.verify_driver("i915").unwrap_err();
        match err {
            DisplayError::WrongDriver { expected, actual } => {
                assert_eq!(expected, "i915");
                assert_eq!(actual, "vc4");
            },
            _ => panic!("expected WrongDriver error, got {:?}", err),
        }
    }

    #[test]
    fn display_manager_resolution_from_active_mode() {
        let mut dm = DisplayManager::new("").unwrap();
        dm.acquire().unwrap();
        // After acquire, resolution comes from active_mode.
        let res = dm.resolution().unwrap();
        assert_eq!(res, (1920, 1080));
    }

    #[test]
    fn display_manager_has_gbm_mock() {
        let dm = DisplayManager::new("").unwrap();
        assert!(!dm.has_gbm());
    }

    #[test]
    fn display_manager_allocate_osd_surface_mock_fails() {
        let mut dm = DisplayManager::new("").unwrap();
        dm.acquire().unwrap();
        let result = dm.allocate_osd_surface();
        assert!(result.is_err());
        match result.unwrap_err() {
            DisplayError::GbmAlloc(msg) => assert!(msg.contains("mock mode")),
            other => panic!("expected GbmAlloc error, got {:?}", other),
        }
    }

    #[test]
    fn display_manager_release_clears_active_mode() {
        let mut dm = DisplayManager::new("").unwrap();
        dm.acquire().unwrap();
        assert!(dm.active_mode().is_some());
        dm.release().unwrap();
        assert!(dm.active_mode().is_none());
    }

    #[test]
    fn display_manager_connector_best_mode_selects_1080p60() {
        let dm = DisplayManager::new("").unwrap();
        let conn = dm.connectors().unwrap().first().unwrap();
        let best = conn.best_mode().expect("should have a best mode");
        assert!(best.is_1080p60());
    }

    #[test]
    fn display_manager_connector_modes_sorted_by_preference() {
        let dm = DisplayManager::new("").unwrap();
        let conn = dm.connectors().unwrap().first().unwrap();
        // First mode should be 1080p60 (highest preference).
        let first = conn.modes.first().expect("should have modes");
        assert!(first.is_1080p60());
    }

    // ── Lifecycle tests ──────────────────────────────────────────────

    #[test]
    fn display_manager_full_lifecycle() {
        let mut dm = DisplayManager::new("").unwrap();

        // Before acquire: no active CRTC or mode.
        assert!(dm.active_crtc().is_none());
        assert!(dm.active_mode().is_none());
        assert_eq!(dm.resolution().unwrap(), (1920, 1080)); // default

        // Acquire: sets active CRTC and mode.
        dm.acquire().unwrap();
        assert!(dm.active_crtc().is_some());
        assert!(dm.active_mode().is_some());
        assert!(dm.active_mode().unwrap().is_1080p60());

        // Resolution from active mode.
        assert_eq!(dm.resolution().unwrap(), (1920, 1080));

        // Can query planes/connectors/CRTCs.
        assert_eq!(dm.planes().unwrap().len(), 2);
        assert_eq!(dm.crtcs().unwrap().len(), 1);
        assert_eq!(dm.connectors().unwrap().len(), 1);

        // Primary and video planes are found.
        assert!(dm.primary_plane().is_some());
        assert!(dm.video_plane().is_some());

        // Active connector ID is set.
        assert_eq!(dm.active_connector_id(), Some(89));

        // Release: clears active state.
        dm.release().unwrap();
        assert!(dm.active_crtc().is_none());
        assert!(dm.active_mode().is_none());
    }

    #[test]
    fn display_manager_acquire_is_idempotent() {
        let mut dm = DisplayManager::new("").unwrap();
        dm.acquire().unwrap();
        // Second acquire should be idempotent (mock mode doesn't have the
        // same idempotency check as hw, but should still succeed).
        assert!(dm.acquire().is_ok());
    }

    #[test]
    fn display_manager_drop_with_active_crtc_logs_warning() {
        // This test verifies that Drop doesn't panic.
        let mut dm = DisplayManager::new("").unwrap();
        dm.acquire().unwrap();
        // Dropping without release should not panic.
        drop(dm);
    }

    // ── Edge case tests ──────────────────────────────────────────────

    #[test]
    fn display_connector_empty_modes_falls_back_to_preferred() {
        let conn = DisplayConnector {
            connector_id: 99,
            connector_type: "HDMI-A-2".into(),
            connected: true,
            modes: vec![],
            preferred_mode: Some(DisplayMode::mode_720p60()),
        };
        let best = conn.best_mode().expect("should fall back to preferred_mode");
        assert_eq!(best.width, 1280);
        assert_eq!(best.height, 720);
    }

    #[test]
    fn display_connector_no_modes_no_preferred() {
        let conn = DisplayConnector {
            connector_id: 99,
            connector_type: "DSI-1".into(),
            connected: true,
            modes: vec![],
            preferred_mode: None,
        };
        assert!(conn.best_mode().is_none());
    }

    #[test]
    fn display_mode_1080p60_refresh_boundary() {
        // 59940 mHz (59.94 Hz) is NOT 1080p60 by our strict check.
        let m = DisplayMode::new(1920, 1080, 59940);
        assert!(!m.is_1080p60());
        // 60000 mHz (60 Hz) is 1080p60.
        let m = DisplayMode::new(1920, 1080, 60000);
        assert!(m.is_1080p60());
    }

    #[test]
    fn display_connector_best_mode_within_falls_back() {
        // If no mode fits within the max, fall back to best overall.
        let conn = DisplayConnector {
            connector_id: 89,
            connector_type: "HDMI-A-1".into(),
            connected: true,
            modes: vec![DisplayMode::mode_1080p60()],
            preferred_mode: Some(DisplayMode::mode_1080p60()),
        };
        // Max 640x480 — no mode fits, should fall back to 1080p60.
        let best = conn.best_mode_within(640, 480).expect("should fall back");
        assert_eq!(best.width, 1920);
    }
}
