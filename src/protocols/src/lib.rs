//! PiCast Protocols
//!
//! Exposes the three network-facing servers that external controllers use
//! to interact with PiCast:
//!
//! - **HTTP API** – REST-like control surface (`/play`, `/pause`, `/status`, …).
//! - **WebSocket** – low-latency bidirectional event stream for real-time UIs.
//! - **DLNA/UPnP** – appears as a MediaRenderer on the local network so
//!   phones and PCs can "cast" to the device.

use anyhow::Result;

// ── HTTP API Server ──────────────────────────────────────────────────

/// REST API server built on `hyper`.
///
/// Endpoints mirror the standard Cast receiver protocol:
///
/// | Method | Path            | Description                     |
/// |--------|-----------------|---------------------------------|
/// | POST   | `/v1/load`      | Load a media URL                |
/// | POST   | `/v1/play`      | Resume playback                 |
/// | POST   | `/v1/pause`     | Pause playback                  |
/// | POST   | `/v1/stop`      | Stop and unload                 |
/// | POST   | `/v1/seek`      | Seek to a position (ms)         |
/// | POST   | `/v1/setVolume` | Set volume 0–100                |
/// | GET    | `/v1/status`    | Current player state & metadata |
pub struct HttpApiServer {
    /// Socket address the server binds to.
    listen_addr: String,
}

impl HttpApiServer {
    /// Create a new HTTP server that will bind to `listen_addr`.
    pub fn new(listen_addr: &str) -> Self {
        Self {
            listen_addr: listen_addr.to_owned(),
        }
    }

    /// Start accepting connections.
    ///
    /// This method runs indefinitely until the `shutdown` future resolves.
    pub async fn start(&self, shutdown: impl std::future::Future<Output = ()>) -> Result<()> {
        tracing::info!(addr = %self.listen_addr, "HTTP API server starting");
        shutdown.await;
        Ok(())
    }
}

// ── WebSocket Server ─────────────────────────────────────────────────

/// WebSocket server for real-time, bidirectional communication.
///
/// Clients subscribe to player-state events and send control commands
/// through a single long-lived socket. Message payloads are JSON.
pub struct WebSocketServer {
    /// Socket address the server binds to.
    listen_addr: String,
}

impl WebSocketServer {
    /// Create a new WebSocket server that will bind to `listen_addr`.
    pub fn new(listen_addr: &str) -> Self {
        Self {
            listen_addr: listen_addr.to_owned(),
        }
    }

    /// Start accepting connections.
    pub async fn start(&self, shutdown: impl std::future::Future<Output = ()>) -> Result<()> {
        tracing::info!(addr = %self.listen_addr, "WebSocket server starting");
        shutdown.await;
        Ok(())
    }
}

// ── DLNA / UPnP Renderer ────────────────────────────────────────────

/// UPnP MediaRenderer implementation.
///
/// Advertises itself via SSDP on the local network and responds to
/// UPnP `AVTransport` and `RenderingControl` SOAP actions so that
/// standard DLNA controllers (phones, Windows, VLC, …) can discover
/// and control the device without installing extra software.
pub struct DlnaRenderer {
    /// Friendly name broadcast via SSDP.
    friendly_name: String,
}

impl DlnaRenderer {
    /// Create a new DLNA renderer with the given `friendly_name`.
    pub fn new(friendly_name: &str) -> Self {
        Self {
            friendly_name: friendly_name.to_owned(),
        }
    }

    /// Start the SSDP advertiser and SOAP action listener.
    pub async fn start(&self, shutdown: impl std::future::Future<Output = ()>) -> Result<()> {
        tracing::info!(name = %self.friendly_name, "DLNA renderer starting");
        shutdown.await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── HTTP API Server construction ─────────────────────────────────

    #[test]
    fn http_api_server_new() {
        let server = HttpApiServer::new("0.0.0.0:8080");
        // Server should be constructed without error; the listen_addr
        // is stored internally (no public accessor yet).
        let _ = server;
    }

    #[test]
    fn http_api_server_custom_addr() {
        let server = HttpApiServer::new("192.168.1.100:9090");
        let _ = server;
    }

    // ── WebSocket Server construction ────────────────────────────────

    #[test]
    fn websocket_server_new() {
        let server = WebSocketServer::new("0.0.0.0:8081");
        let _ = server;
    }

    #[test]
    fn websocket_server_custom_addr() {
        let server = WebSocketServer::new("10.0.0.1:7070");
        let _ = server;
    }

    // ── DLNA Renderer construction ───────────────────────────────────

    #[test]
    fn dlna_renderer_new() {
        let renderer = DlnaRenderer::new("PiCast");
        let _ = renderer;
    }

    #[test]
    fn dlna_renderer_custom_name() {
        let renderer = DlnaRenderer::new("Living Room Pi");
        let _ = renderer;
    }
}
