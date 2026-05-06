//! PiCast Protocols
//!
//! Exposes the three network-facing servers that external controllers use
//! to interact with PiCast:
//!
//! - **HTTP API** – REST-like control surface (`/api/cast`, `/api/pause`, `/api/status`, etc.).
//! - **WebSocket** – low-latency bidirectional event stream for real-time UIs.
//! - **DLNA/UPnP** – appears as a MediaRenderer on the local network so
//!   phones and PCs can "cast" to the device.

pub mod dlna;
pub mod http;
pub mod ws;

pub use dlna::{run_dlna_sync, DlnaRenderer};
pub use http::HttpApiServer;
pub use ws::WebSocketServer;
