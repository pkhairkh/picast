#![cfg(feature = "hw")]
//! PiCast Playback Events
//!
//! Defines the event types emitted by the playback engine during
//! pipeline operation. Events are delivered through an `mpsc` channel
//! so the session layer can react to state changes asynchronously.

use serde::{Deserialize, Serialize};

/// Events emitted by the playback engine.
///
/// The session layer subscribes to these events to update its state
/// machine and push notifications to connected clients (WebSocket,
/// HTTP API).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlaybackEvent {
    /// Pipeline transitioned to Playing state.
    Playing,

    /// Pipeline transitioned to Paused state.
    Paused,

    /// Pipeline was stopped and torn down.
    Stopped,

    /// End of stream reached — playback finished naturally.
    EndOfStream,

    /// An error occurred in the pipeline.
    Error {
        /// Human-readable error description.
        message: String,
        /// GStreamer debug string (may contain internal details).
        debug: Option<String>,
    },

    /// CDN returned 403 Forbidden — the Tor circuit has likely rotated
    /// since URL resolution, or the CDN token has expired. The session
    /// layer should re-resolve the URL and retry playback.
    CdnForbidden,

    /// Buffering progress update.
    Buffering {
        /// Buffer fill percentage (0–100).
        percent: u8,
    },

    /// Duration and position update.
    PositionUpdate {
        /// Current position in milliseconds.
        position_ms: u64,
        /// Total duration in milliseconds, if known.
        duration_ms: Option<u64>,
    },

    /// Pipeline latency information.
    Latency {
        /// Pipeline latency in milliseconds.
        latency_ms: u64,
    },

    /// Download progress update from StreamSource.
    DownloadProgress {
        /// Total bytes downloaded so far.
        downloaded_bytes: u64,
        /// Total bytes in the file (from Content-Length header).
        total_bytes: Option<u64>,
        /// Measured throughput in kbps over the last measurement window.
        throughput_kbps: u64,
        /// Elapsed time since download started.
        elapsed_secs: f64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_event_serde_roundtrip() {
        let events = vec![
            PlaybackEvent::Playing,
            PlaybackEvent::Paused,
            PlaybackEvent::Stopped,
            PlaybackEvent::EndOfStream,
            PlaybackEvent::Error {
                message: "decode failed".into(),
                debug: Some("v4l2h264dec: negotiation failed".into()),
            },
            PlaybackEvent::CdnForbidden,
            PlaybackEvent::Buffering { percent: 50 },
            PlaybackEvent::PositionUpdate { position_ms: 5000, duration_ms: Some(300000) },
            PlaybackEvent::Latency { latency_ms: 42 },
            PlaybackEvent::DownloadProgress {
                downloaded_bytes: 1024,
                total_bytes: Some(1048576),
                throughput_kbps: 5000,
                elapsed_secs: 1.5,
            },
        ];

        for event in events {
            let json = serde_json::to_string(&event).expect("serialize");
            let decoded: PlaybackEvent = serde_json::from_str(&json).expect("deserialize");
            let re_encoded = serde_json::to_string(&decoded).expect("re-serialize");
            assert_eq!(json, re_encoded, "round-trip failed for {:?}", event);
        }
    }

    #[test]
    fn playback_event_tagged_json() {
        let event = PlaybackEvent::Buffering { percent: 75 };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"buffering\""), "JSON should have tagged type");
        assert!(json.contains("\"percent\":75"), "JSON should have percent field");
    }
}
