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
    /// Whether this plane can be used for video scan-out.
    pub is_primary: bool,
    /// Whether this plane is usable (not claimed by another client).
    pub possible_crtcs: u32,
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

// ── Connector Info ───────────────────────────────────────────────────

/// Information about a connected display output (HDMI, DSI, etc.).
#[derive(Debug, Clone)]
pub struct DisplayConnector {
    /// Kernel-assigned connector ID.
    pub connector_id: u32,
    /// Connector type (HDMI-A, DSI, etc.).
    pub connector_type: String,
    /// Connection state.
    pub connected: bool,
    /// Preferred display mode (highest resolution at highest refresh).
    pub preferred_mode: Option<(u32, u32, u32)>, // (width, height, refresh_mhz)
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
    /// Saved CRTC state for restoration on release.
    #[cfg(feature = "hw")]
    saved_crtc: Option<SavedCrtcState>,
}

/// Saved CRTC state for restoration on release.
#[cfg(feature = "hw")]
struct SavedCrtcState {
    crtc_id: u32,
    fb_id: Option<u32>,
    mode: Option<Mode>,
    x: u32,
    y: u32,
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

// ── HW implementation ────────────────────────────────────────────────

#[cfg(feature = "hw")]
impl DisplayManager {
    /// Open the DRM device at `device_path` and acquire master.
    ///
    /// Falls back to `/dev/dri/card0` if `device_path` is empty.
    /// On Raspberry Pi 4B+ with vc4, the device is typically
    /// `/dev/dri/card1` (card0 is the firmware framebuffer).
    pub fn new(device_path: &str) -> Result<Self, DisplayError> {
        let path =
            if device_path.is_empty() { Self::find_dri_device()? } else { device_path.to_owned() };

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| DisplayError::DeviceOpen(format!("{}: {}", path, e)))?;

        tracing::info!(path = %path, fd = file.as_raw_fd(), "opened DRM device");

        Ok(Self {
            device_path: path,
            drm_fd: Some(Card(file)),
            connectors: Vec::new(),
            planes: Vec::new(),
            crtcs: Vec::new(),
            active_crtc: None,
            saved_crtc: None,
        })
    }

    /// Enumerate available DRM planes.
    ///
    /// Returns information about each plane including its Z-position,
    /// supported formats, and which CRTCs it can be used with.
    pub fn planes(&self) -> Result<&[DrmPlane], DisplayError> {
        Ok(&self.planes)
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
    /// 3. Selects the best CRTC for that connector.
    /// 4. Saves the current CRTC state for restoration (only if master).
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
        //      the vc4 device.  The ExecStartPre in picast.service unbinds
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

        // Find connected connectors.
        let mut found_connectors = Vec::new();
        for &conn_handle in resources.connectors() {
            let info = fd
                .get_connector(conn_handle, true)
                .map_err(|e| DisplayError::Modeset(format!("connector info failed: {}", e)))?;

            let connected = info.state() == ConnectorState::Connected;
            let conn_type = format!("{}-{}", info.interface().as_str(), info.interface_id());

            let preferred_mode = info
                .modes()
                .iter()
                .max_by(|a, b| {
                    // Prefer highest resolution, then highest refresh.
                    let a_area = a.size().0 as u64 * a.size().1 as u64;
                    let b_area = b.size().0 as u64 * b.size().1 as u64;
                    a_area.cmp(&b_area).then_with(|| a.vrefresh().cmp(&b.vrefresh()))
                })
                .map(|m| (m.size().0 as u32, m.size().1 as u32, m.vrefresh()));

            found_connectors.push(DisplayConnector {
                connector_id: conn_handle.into(),
                connector_type: conn_type,
                connected,
                preferred_mode,
            });
        }
        self.connectors = found_connectors;

        // Find a connected connector.
        let connector =
            self.connectors.iter().find(|c| c.connected).ok_or(DisplayError::NoConnector)?;

        tracing::info!(
            connector_id = connector.connector_id,
            connector_type = %connector.connector_type,
            "found connected display"
        );

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

        // Enumerate planes.
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
            let possible_handles = resources.filter_crtcs(info.possible_crtcs());
            let possible_crtcs = resources
                .crtcs()
                .iter()
                .enumerate()
                .filter(|(_, handle)| possible_handles.contains(handle))
                .fold(0u32, |mask, (idx, _)| mask | (1u32 << idx));

            found_planes.push(DrmPlane {
                plane_id: plane_handle.into(),
                zpos: if is_primary { 0 } else { 1 },
                formats: info.formats().iter().map(|f| *f).collect(),
                is_primary,
                possible_crtcs,
            });
        }
        self.planes = found_planes;

        // Select the best CRTC for our connector.
        let crtc = self.crtcs.first().ok_or(DisplayError::NoCrtc)?.clone();

        // Save current CRTC state for restoration on release().
        // Only possible if we have DRM master — get_crtc() for saving works
        // without master, but set_crtc() for restoration requires master.
        if has_master {
            let crtc_handle = control::from_u32::<control::crtc::Handle>(crtc.crtc_id)
                .ok_or_else(|| DisplayError::Modeset(format!("invalid CRTC id {}", crtc.crtc_id)))?;
            let crtc_info = fd.get_crtc(crtc_handle).ok();
            self.saved_crtc = Some(SavedCrtcState {
                crtc_id: crtc.crtc_id,
                fb_id: crtc_info.as_ref().and_then(|i| i.framebuffer().map(|fb| fb.into())),
                mode: crtc_info.as_ref().and_then(|i| i.mode()),
                x: 0,
                y: 0,
            });
        } else {
            tracing::info!("skipping CRTC state save — no DRM master");
        }

        self.active_crtc = Some(crtc.clone());

        tracing::info!(
            crtc_id = crtc.crtc_id,
            mode = ?connector.preferred_mode,
            has_master = has_master,
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
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&self.device_path)
                    .map_err(|e| {
                        DisplayError::DeviceOpen(format!(
                            "re-open {} for CRTC restore: {}",
                            self.device_path, e
                        ))
                    })?;
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
                                self.saved_crtc = None;
                                return Ok(());
                            },
                        };
                    if let Some(mode) = saved.mode {
                        let framebuffer = saved
                            .fb_id
                            .and_then(control::from_u32::<control::framebuffer::Handle>);
                        let restore_result = fd.set_crtc(
                            crtc_handle,
                            framebuffer,
                            (saved.x, saved.y),
                            &[],
                            Some(mode),
                        );
                        match restore_result {
                            Ok(()) => tracing::info!(crtc_id = saved.crtc_id, "CRTC state restored"),
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
        self.saved_crtc = None;
        Ok(())
    }

    /// Return the current display resolution as `(width, height)`.
    ///
    /// If the display has been acquired, returns the active mode.
    /// Otherwise returns a default (1920x1080) as a hint.
    pub fn resolution(&self) -> Result<(u32, u32), DisplayError> {
        if let Some(ref crtc) = self.active_crtc {
            if crtc.width > 0 && crtc.height > 0 {
                return Ok((crtc.width, crtc.height));
            }
        }

        // Check connectors for preferred mode.
        if let Some(conn) = self.connectors.iter().find(|c| c.connected) {
            if let Some((w, h, _)) = conn.preferred_mode {
                return Ok((w, h));
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

    /// Return the connector ID of the first connected display, if known.
    ///
    /// Only available after `acquire()` has been called.
    pub fn active_connector_id(&self) -> Option<u32> {
        self.connectors
            .iter()
            .find(|c| c.connected)
            .map(|c| c.connector_id)
    }

    /// Return the raw DRM device file descriptor, if the device is open.
    ///
    /// Returns `None` after `acquire()` closes the fd to let kmssink
    /// open the device fresh (see acquire() documentation).
    #[cfg(feature = "hw")]
    pub fn drm_fd(&self) -> Option<i32> {
        self.drm_fd.as_ref().map(|card| card.as_raw_fd())
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
            preferred_mode: Some((1920, 1080, 60000)),
        }];

        let planes = vec![
            DrmPlane {
                plane_id: 31,
                zpos: 0,
                formats: vec![0x34325258, 0x34325241], // XR24, AR24
                is_primary: true,
                possible_crtcs: 0x1,
            },
            DrmPlane {
                plane_id: 32,
                zpos: 1,
                formats: vec![0x3231564E, 0x3132564E], // NV12, NV21
                is_primary: false,
                possible_crtcs: 0x1,
            },
        ];

        let crtcs = vec![DrmCrtc {
            crtc_id: 56,
            width: 1920,
            height: 1080,
            refresh_mhz: 60000,
            fb_id: None,
        }];

        Ok(Self { device_path: path, connectors, planes, crtcs, active_crtc: None })
    }

    /// Enumerate available DRM planes (mock data simulating Pi 4B+).
    pub fn planes(&self) -> Result<&[DrmPlane], DisplayError> {
        Ok(&self.planes)
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
        Ok(())
    }

    /// Release the CRTC (no-op in mock mode).
    pub fn release(&mut self) -> Result<(), DisplayError> {
        tracing::debug!("release called in mock mode — no-op");
        self.active_crtc = None;
        Ok(())
    }

    /// Return the current display resolution (default 1920×1080 in mock mode).
    pub fn resolution(&self) -> Result<(u32, u32), DisplayError> {
        Ok((1920, 1080))
    }

    /// Return the DRM device path.
    pub fn device_path(&self) -> &str {
        &self.device_path
    }

    /// Return the active CRTC (always None in mock mode).
    pub fn active_crtc(&self) -> Option<&DrmCrtc> {
        self.active_crtc.as_ref()
    }

    /// Clear the screen (no-op in mock mode).
    pub fn clear_screen(&mut self) -> Result<(), DisplayError> {
        tracing::debug!("clear_screen called in mock mode — no-op");
        Ok(())
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

    #[test]
    fn display_error_variants() {
        let err = DisplayError::DeviceOpen("/dev/dri/card0".into());
        assert!(err.to_string().contains("failed to open DRM device"));

        let err = DisplayError::MasterAcquire("denied".into());
        assert!(err.to_string().contains("failed to acquire DRM master"));

        let err = DisplayError::Modeset("mode rejected".into());
        assert!(err.to_string().contains("DRM mode-setting failed"));

        let err = DisplayError::NoCrtc;
        assert!(err.to_string().contains("no available CRTC"));

        let err = DisplayError::NoConnector;
        assert!(err.to_string().contains("no connected connector"));

        let err = DisplayError::NoPlane;
        assert!(err.to_string().contains("no available plane"));

        let err = DisplayError::NoMode;
        assert!(err.to_string().contains("no display mode"));

        let err = DisplayError::GbmAlloc("out of memory".into());
        assert!(err.to_string().contains("GBM allocation failed"));

        let err = DisplayError::HardwareUnavailable;
        assert!(err.to_string().contains("hardware display unavailable"));
    }

    #[test]
    fn drm_plane_fields() {
        let plane = DrmPlane {
            plane_id: 42,
            zpos: 1,
            formats: vec![0x34325258], // XR24
            is_primary: false,
            possible_crtcs: 0x1,
        };
        assert_eq!(plane.plane_id, 42);
        assert_eq!(plane.zpos, 1);
        assert!(!plane.is_primary);
    }

    #[test]
    fn drm_crtc_fields() {
        let crtc =
            DrmCrtc { crtc_id: 55, width: 1920, height: 1080, refresh_mhz: 60000, fb_id: Some(99) };
        assert_eq!(crtc.crtc_id, 55);
        assert_eq!(crtc.width, 1920);
        assert_eq!(crtc.height, 1080);
        assert_eq!(crtc.fb_id, Some(99));
    }

    #[test]
    fn display_connector_fields() {
        let conn = DisplayConnector {
            connector_id: 77,
            connector_type: "HDMI-A-1".into(),
            connected: true,
            preferred_mode: Some((3840, 2160, 30000)),
        };
        assert!(conn.connected);
        assert_eq!(conn.connector_type, "HDMI-A-1");
        assert_eq!(conn.preferred_mode, Some((3840, 2160, 30000)));
    }

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
        let primary = dm.planes().unwrap().iter().find(|p| p.is_primary).unwrap();
        assert_eq!(primary.zpos, 0);
        assert!(primary.formats.len() >= 1);

        let overlay = dm.planes().unwrap().iter().find(|p| !p.is_primary).unwrap();
        assert_eq!(overlay.zpos, 1);

        // Verify mock connector.
        let conn = dm.connectors().unwrap().first().unwrap();
        assert!(conn.connected);
        assert_eq!(conn.connector_type, "HDMI-A-1");
        assert_eq!(conn.preferred_mode, Some((1920, 1080, 60000)));

        // Verify mock CRTC.
        let crtc = dm.crtcs().unwrap().first().unwrap();
        assert_eq!(crtc.width, 1920);
        assert_eq!(crtc.height, 1080);
    }

    #[test]
    fn display_manager_acquire_sets_active_crtc() {
        let mut dm = DisplayManager::new("").unwrap();
        assert!(dm.active_crtc().is_none(), "no active CRTC before acquire");
        dm.acquire().unwrap();
        let crtc = dm.active_crtc().expect("should have active CRTC after acquire");
        assert_eq!(crtc.width, 1920);
        assert_eq!(crtc.height, 1080);
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
}
