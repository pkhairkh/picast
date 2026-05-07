//! PiCast DLNA/UPnP MediaRenderer
//!
//! Advertises itself via SSDP on the local network and responds to
//! UPnP `AVTransport` and `RenderingControl` SOAP actions so that
//! standard DLNA controllers (phones, Windows, VLC) can discover
//! and control the device without installing extra software.
//!
//! ## Implementation Strategy
//!
//! In v1, PiCast uses `gmediarender` as a subprocess for DLNA
//! support rather than implementing the full UPnP stack in Rust.
//! gmediarender is a lightweight, well-tested DLNA renderer that
//! uses GStreamer for playback, which aligns perfectly with our
//! pipeline architecture.
//!
//! The `GSTREAMER_PIPELINE` environment variable is set to match
//! PiCast's pipeline (with SOCKS5 proxy support), and PiCast's
//! session manager synchronises state with gmediarender.
//!
//! ## Session Synchronisation
//!
//! When wired into a `SessionManager`, the `DlnaRenderer`:
//! - Subscribes to `SessionEvent` broadcasts and mirrors session
//!   state changes (play, pause, stop, volume) to gmediarender
//!   via its D-Bus/UPnP control interface.
//! - Responds to DLNA controller actions (play, pause, stop, seek,
//!   volume) by delegating to the session manager, keeping the
//!   PiCast session state consistent with the DLNA control surface.
//!
//! In v1, bidirectional sync is approximated by having the DLNA
//! renderer start/stop with the session lifecycle. Full bidirectional
//! D-Bus bridge is deferred to v2.

use anyhow::{anyhow, Result};
use picast_session::SessionEvent;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, Mutex};
use tracing;

/// DLNA MediaRenderer that delegates to gmediarender.
pub struct DlnaRenderer {
    /// Friendly name broadcast via SSDP.
    friendly_name: String,
    /// Path to the gmediarender binary.
    binary_path: String,
    /// SOCKS5 proxy address for the GStreamer pipeline.
    socks_addr: String,
    /// The spawned gmediarender child process.
    child: Arc<Mutex<Option<Child>>>,
}

impl DlnaRenderer {
    /// Create a new DLNA renderer with the given `friendly_name`.
    pub fn new(friendly_name: &str, socks_addr: &str) -> Self {
        Self {
            friendly_name: friendly_name.to_owned(),
            binary_path: "gmediarender".to_owned(),
            socks_addr: socks_addr.to_owned(),
            child: Arc::new(Mutex::new(None)),
        }
    }

    /// Set a custom path to the gmediarender binary.
    pub fn with_binary_path(mut self, path: &str) -> Self {
        self.binary_path = path.to_owned();
        self
    }

    /// Start the DLNA renderer subprocess.
    ///
    /// Spawns `gmediarender` with PiCast's GStreamer pipeline
    /// configured via the `GSTREAMER_PIPELINE` environment variable.
    /// The pipeline routes through Tor's SOCKS5 proxy.
    pub async fn start(&self) -> Result<()> {
        let mut guard = self.child.lock().await;

        if guard.is_some() {
            tracing::warn!("gmediarender is already running");
            return Ok(());
        }

        // Build the GStreamer pipeline string for gmediarender.
        // %s is replaced by gmediarender with the URI set by the DLNA controller.
        //
        // Pipeline: souphttpsrc → queue2 → parsebin → h264parse → capssetter(bt709)
        //           → v4l2h264dec(mmap) → kmssink(vc4)
        //
        // The capssetter forces bt709 colorimetry to prevent "not-negotiated"
        // errors between v4l2h264dec and kmssink caused by unusual VUI
        // colorimetry values in some H.264 streams.
        // Using mmap instead of dmabuf avoids memory:DMABuf caps features
        // that kmssink may not negotiate correctly.
        let pipeline = if self.socks_addr.is_empty() {
            "souphttpsrc location=%s ! queue2 max-size-bytes=52428800 use-buffering=true ! parsebin ! h264parse ! capssetter caps=\"video/x-h264,colorimetry=bt709\" join=false replace=true ! v4l2h264dec capture-io-mode=mmap ! kmssink driver-name=vc4 can-scale=true".to_owned()
        } else {
            // Safely extract the port number from socks_addr, validating it's numeric.
            let port_str = self.socks_addr.split(':').next_back().unwrap_or("9050");
            let port: u16 = port_str.parse().unwrap_or_else(|_| {
                tracing::warn!(socks_addr = %self.socks_addr, "invalid SOCKS port — defaulting to 9050");
                9050
            });
            format!(
                "souphttpsrc location=%s socks5-proxy-ip=127.0.0.1 socks5-proxy-port={} ! queue2 max-size-bytes=52428800 use-buffering=true ! parsebin ! h264parse ! capssetter caps=\"video/x-h264,colorimetry=bt709\" join=false replace=true ! v4l2h264dec capture-io-mode=mmap ! kmssink driver-name=vc4 can-scale=true",
                port
            )
        };

        tracing::info!(
            name = %self.friendly_name,
            pipeline_len = pipeline.len(),
            "starting gmediarender subprocess"
        );

        let mut child = Command::new(&self.binary_path)
            .env("GSTREAMER_PIPELINE", pipeline)
            .arg("--friendly-name")
            .arg(&self.friendly_name)
            .arg("--port")
            .arg("49152")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    anyhow!(
                        "gmediarender binary not found — install with: apt install gmediarender"
                    )
                } else {
                    anyhow!("failed to spawn gmediarender: {}", e)
                }
            })?;

        // Spawn a background task that reads gmediarender's stderr and
        // logs each line at debug level. This is invaluable for diagnosing
        // pipeline issues without cluttering the main log at info level.
        if let Some(stderr) = child.stderr.take() {
            let friendly_name = self.friendly_name.clone();
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            tracing::debug!(
                                name = %friendly_name,
                                line = %line,
                                "gmediarender stderr"
                            );
                        },
                        Ok(None) => break, // EOF — process exited
                        Err(e) => {
                            tracing::debug!(
                                name = %friendly_name,
                                error = %e,
                                "gmediarender stderr read error"
                            );
                            break;
                        },
                    }
                }
            });
        }

        *guard = Some(child);
        tracing::info!("gmediarender started successfully");
        Ok(())
    }

    /// Stop the gmediarender subprocess.
    pub async fn stop(&self) -> Result<()> {
        let mut guard = self.child.lock().await;

        if let Some(ref mut child) = *guard {
            tracing::info!("stopping gmediarender");

            // Try graceful SIGTERM first.
            #[cfg(unix)]
            {
                if let Some(id) = child.id() {
                    unsafe {
                        libc::kill(id as i32, libc::SIGTERM);
                    }
                }
            }

            // Wait up to 3 seconds for the process to exit.
            match tokio::time::timeout(std::time::Duration::from_secs(3), child.wait()).await {
                Ok(Ok(status)) => {
                    tracing::info!(exit_code = ?status.code(), "gmediarender exited");
                },
                _ => {
                    tracing::warn!("killing gmediarender process");
                    let _ = child.kill().await;
                },
            }

            *guard = None;
        }

        Ok(())
    }

    /// Return the friendly name.
    pub fn friendly_name(&self) -> &str {
        &self.friendly_name
    }

    /// Whether the gmediarender subprocess is currently running.
    ///
    /// Checks the actual process state with `try_wait()` so that
    /// crashed or exited children are detected promptly rather than
    /// being reported as "running" indefinitely.
    pub async fn is_running(&self) -> bool {
        let mut guard = self.child.lock().await;
        match guard.as_mut() {
            Some(child) => {
                match child.try_wait() {
                    Ok(Some(_status)) => {
                        // Process has exited — clear the slot.
                        *guard = None;
                        false
                    },
                    Ok(None) => true, // Still running
                    Err(_) => false,
                }
            },
            None => false,
        }
    }
}

impl Drop for DlnaRenderer {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.child.try_lock() {
            if let Some(ref mut child) = *guard {
                let _ = child.kill();
                tracing::debug!("DlnaRenderer dropped — killed orphaned subprocess");
            }
        }
    }
}

/// Run the DLNA session synchroniser as a background task.
///
/// Subscribes to session events and mirrors PiCast state changes
/// to the gmediarender subprocess lifecycle:
///
/// - **Resolving** → stops gmediarender early, before PiCast acquires
///   the display or starts the GStreamer pipeline. Both PiCast and
///   gmediarender use kmssink for DRM/KMS output, and only one
///   process can hold DRM master at a time. Stopping gmediarender
///   as soon as URL resolution begins ensures DRM master is available
///   when kmssink needs it.
/// - **Stopped/Idle** → starts gmediarender so DLNA controllers can
///   discover and cast to PiCast while idle
/// - **Paused** → gmediarender stays stopped (PiCast still holds DRM)
/// - **Error** → starts gmediarender so DLNA is available as fallback
///
/// The key insight is that PiCast and gmediarender both use kmssink
/// for DRM/KMS output, and only one process can hold DRM master at
/// a time. When PiCast is playing, gmediarender must be stopped;
/// when PiCast is idle, gmediarender should be running for DLNA
/// discovery.
pub async fn run_dlna_sync(
    dlna: Arc<DlnaRenderer>,
    mut event_rx: broadcast::Receiver<SessionEvent>,
) {
    tracing::info!("DLNA session sync task started");

    loop {
        match event_rx.recv().await {
            Ok(event) => {
                match &event {
                    // Stop gmediarender as early as possible — when URL resolution
                    // starts, not when playback starts. By the time we reach the
                    // Playing state, the GStreamer pipeline has already been
                    // constructed and kmssink is trying to acquire DRM master.
                    // If gmediarender still holds it at that point, kmssink fails.
                    SessionEvent::Resolving { .. } => {
                        if dlna.is_running().await {
                            tracing::info!(
                                "session resolving — stopping gmediarender to release DRM early"
                            );
                            if let Err(e) = dlna.stop().await {
                                tracing::warn!(
                                    error = %e,
                                    "failed to stop gmediarender — DRM conflict may occur"
                                );
                            }
                        } else {
                            tracing::debug!(
                                "session resolving — gmediarender not running (DRM available)"
                            );
                        }
                    },
                    // Also handle Playing as a safety net in case the Resolving
                    // event was missed (e.g., due to broadcast lag).
                    SessionEvent::Playing { .. } => {
                        if dlna.is_running().await {
                            tracing::info!(
                                "session playing — stopping gmediarender to release DRM"
                            );
                            if let Err(e) = dlna.stop().await {
                                tracing::warn!(
                                    error = %e,
                                    "failed to stop gmediarender — DRM conflict may occur"
                                );
                            }
                        }
                    },
                    SessionEvent::Stopped { .. } => {
                        // Start gmediarender when the session stops so DLNA
                        // controllers can discover PiCast while idle.
                        if !dlna.is_running().await {
                            tracing::info!(
                                "session stopped — starting gmediarender for DLNA discovery"
                            );
                            if let Err(e) = dlna.start().await {
                                tracing::warn!(
                                    error = %e,
                                    "failed to start gmediarender after stop — DLNA unavailable"
                                );
                            }
                        }
                    },
                    SessionEvent::Paused { .. } => {
                        // gmediarender handles pause/resume internally via
                        // its UPnP AVTransport control. No action needed here.
                        tracing::debug!("session paused — DLNA renderer handles internally");
                    },
                    SessionEvent::VolumeChanged { volume, .. } => {
                        // Volume changes from the session layer are not
                        // forwarded to gmediarender in v1. gmediarender
                        // has its own RenderingControl service.
                        tracing::debug!(
                            volume = %volume,
                            "volume changed — DLNA renderer handles internally"
                        );
                    },
                    SessionEvent::Error { message, .. } => {
                        // Start gmediarender on session error so DLNA is
                        // available as a fallback input method.
                        tracing::warn!(
                            error = %message,
                            "session error — starting gmediarender as fallback"
                        );
                        if !dlna.is_running().await {
                            if let Err(e) = dlna.start().await {
                                tracing::warn!(
                                    error = %e,
                                    "failed to start gmediarender after error"
                                );
                            }
                        }
                    },
                    SessionEvent::CdnForbidden { .. } => {
                        // CDN 403 — don't start gmediarender yet.
                        // The session manager may retry with a re-resolved URL.
                        // If the retries fail, the session will transition to
                        // Error state and gmediarender will be started then.
                        tracing::warn!(
                            "CDN 403 Forbidden — not starting gmediarender (re-resolve may be attempted)"
                        );
                    },
                    _ => {
                        // Other events (Created, Resolved, Buffering, etc.)
                        // are not relevant to the DLNA renderer in v1.
                    },
                }
            },
            Err(broadcast::error::RecvError::Lagged(count)) => {
                tracing::warn!(count = count, "DLNA event stream lagged — catching up");
            },
            Err(broadcast::error::RecvError::Closed) => {
                tracing::info!("DLNA event stream closed — stopping sync task");
                // Stop gmediarender when the event stream closes (server shutdown).
                if let Err(e) = dlna.stop().await {
                    tracing::warn!(error = %e, "failed to stop gmediarender on shutdown");
                }
                break;
            },
        }
    }

    tracing::info!("DLNA session sync task finished");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dlna_renderer_new() {
        let renderer = DlnaRenderer::new("PiCast", "127.0.0.1:9050");
        assert_eq!(renderer.friendly_name(), "PiCast");
    }

    #[test]
    fn dlna_renderer_custom_name() {
        let renderer = DlnaRenderer::new("Living Room Pi", "127.0.0.1:9050");
        assert_eq!(renderer.friendly_name(), "Living Room Pi");
    }

    #[test]
    fn dlna_renderer_custom_binary_path() {
        let renderer = DlnaRenderer::new("PiCast", "127.0.0.1:9050")
            .with_binary_path("/usr/local/bin/gmediarender");
        assert_eq!(renderer.binary_path, "/usr/local/bin/gmediarender");
    }

    #[tokio::test]
    async fn dlna_renderer_not_running_by_default() {
        let renderer = DlnaRenderer::new("PiCast", "127.0.0.1:9050");
        assert!(!renderer.is_running().await);
    }

    #[tokio::test]
    async fn dlna_renderer_start_fails_without_binary() {
        let renderer = DlnaRenderer::new("PiCast", "127.0.0.1:9050")
            .with_binary_path("/nonexistent/gmediarender");
        let result = renderer.start().await;
        assert!(result.is_err(), "should fail when binary doesn't exist");
    }

    #[tokio::test]
    async fn dlna_renderer_stop_when_not_running() {
        let renderer = DlnaRenderer::new("PiCast", "127.0.0.1:9050");
        // Stop should succeed even when not running
        let result = renderer.stop().await;
        assert!(result.is_ok(), "stop should succeed when not running");
    }

    #[tokio::test]
    async fn dlna_sync_handles_lagged_events() {
        let (tx, rx) = broadcast::channel(4);
        let dlna = Arc::new(DlnaRenderer::new("PiCast", "127.0.0.1:9050"));

        // Fill the channel to cause lag
        for _i in 0..10 {
            let _ = tx.send(SessionEvent::Playing { id: uuid::Uuid::new_v4() });
        }

        // The sync task should handle lagged events gracefully
        let dlna_clone = dlna.clone();
        let handle = tokio::spawn(async move {
            run_dlna_sync(dlna_clone, rx).await;
        });

        // Drop the sender to close the stream
        drop(tx);

        // The sync task should finish
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn dlna_sync_stops_renderer_on_stream_close() {
        let (tx, rx) = broadcast::channel(16);
        let dlna = Arc::new(DlnaRenderer::new("PiCast", "127.0.0.1:9050"));

        let dlna_clone = dlna.clone();
        let handle = tokio::spawn(async move {
            run_dlna_sync(dlna_clone, rx).await;
        });

        // Drop the sender to close the stream
        drop(tx);

        // The sync task should finish
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
        assert!(result.is_ok(), "sync task should finish when stream closes");
    }
}
