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

use anyhow::{anyhow, Result};
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
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
        let pipeline = if self.socks_addr.is_empty() {
            "souphttpsrc location=%s ! queue2 max-size-bytes=52428800 use-buffering=true ! parsebin ! v4l2h264dec capture-io-mode=dmabuf ! kmssink driver-name=vc4 plane-id=0 can-scale=true force-modesetting=true"
        } else {
            &format!(
                "souphttpsrc location=%s socks5-proxy-ip=127.0.0.1 socks5-proxy-port={} ! queue2 max-size-bytes=52428800 use-buffering=true ! parsebin ! v4l2h264dec capture-io-mode=dmabuf ! kmssink driver-name=vc4 plane-id=0 can-scale=true force-modesetting=true",
                self.socks_addr.split(':').next_back().unwrap_or("9050")
            )
        };

        tracing::info!(
            name = %self.friendly_name,
            pipeline_len = pipeline.len(),
            "starting gmediarender subprocess"
        );

        let child = Command::new(&self.binary_path)
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
}

impl Drop for DlnaRenderer {
    fn drop(&mut self) {
        if self.child.try_lock().is_ok() {
            // We have the lock and are dropping — try to clean up.
            tracing::debug!("DlnaRenderer dropped — subprocess will be orphaned");
        }
    }
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
}
