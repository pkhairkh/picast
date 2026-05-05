//! PiCast Playback Engine
//!
//! Wraps GStreamer into a high-level playback API tailored for the
//! Raspberry Pi. The engine manages:
//!
//! - Pipeline construction (uridecodebin → videoconvert → kmssink / audiosink).
//! - Adaptive bitrate control for HLS / DASH streams.
//! - Buffer health monitoring and stall detection.
//! - Volume, seek, and rate-change commands.

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── Errors ───────────────────────────────────────────────────────────

/// Errors that can occur during playback operations.
#[derive(Error, Debug)]
pub enum PlaybackError {
    /// The GStreamer pipeline could not be constructed.
    #[error("pipeline creation failed: {0}")]
    PipelineCreation(String),

    /// A GStreamer element returned an error.
    #[error("gstreamer error: {0}")]
    Gstreamer(String),

    /// The pipeline is in a state that forbids the requested operation.
    #[error("invalid state for operation: {0}")]
    InvalidState(String),

    /// A seek operation failed.
    #[error("seek failed: {0}")]
    SeekFailed(String),
}

// ── Pipeline Config ──────────────────────────────────────────────────

/// Configuration for a GStreamer pipeline instance.
///
/// Controls which video and audio sinks are used, buffer sizes, and
/// hardware-acceleration preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    /// GStreamer video sink element (e.g. `"kmssink"`, `"glimagesink"`).
    pub video_sink: String,
    /// GStreamer audio sink element (e.g. `"alsasink"`, `"pulseaudio"`).
    pub audio_sink: String,
    /// Buffer duration in milliseconds for the stream buffer.
    pub buffer_duration_ms: u64,
    /// Whether to enable hardware-accelerated decoding (v4l2, etc.).
    pub hw_accel: bool,
    /// Initial volume (0.0 – 1.0).
    pub volume: f64,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            video_sink: "kmssink".into(),
            audio_sink: "alsasink".into(),
            buffer_duration_ms: 3000,
            hw_accel: true,
            volume: 1.0,
        }
    }
}

// ── Buffer Health ────────────────────────────────────────────────────

/// Real-time buffer health metrics reported by the pipeline.
///
/// The session layer polls these metrics to detect stalls and inform
/// the UI about buffering progress.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BufferHealth {
    /// Percentage of the buffer currently filled (0–100).
    pub fill_percent: u8,
    /// Number of seconds of media data buffered ahead of the current
    /// playback position.
    pub buffered_seconds: f64,
    /// Estimated time (in ms) until the buffer will be full at the
    /// current download rate. `None` if already full.
    pub estimated_fill_ms: Option<u64>,
    /// Whether the pipeline is currently buffering (stalled).
    pub is_buffering: bool,
}

impl Default for BufferHealth {
    fn default() -> Self {
        Self {
            fill_percent: 100,
            buffered_seconds: 0.0,
            estimated_fill_ms: None,
            is_buffering: false,
        }
    }
}

// ── Playback Engine ──────────────────────────────────────────────────

/// The main playback engine backed by GStreamer.
///
/// Owns the pipeline lifecycle and translates high-level commands
/// (play, pause, seek, volume) into GStreamer bus messages and
/// element property changes.
pub struct PlaybackEngine {
    /// Pipeline configuration.
    config: PipelineConfig,
    // TODO: add GStreamer pipeline handle:
    // pipeline: gstreamer::Pipeline,
}

impl PlaybackEngine {
    /// Create a new engine with the given pipeline configuration.
    pub fn new(config: PipelineConfig) -> Result<Self, PlaybackError> {
        // TODO: gstreamer::init() and pipeline construction
        Ok(Self { config })
    }

    /// Load a URL and transition to the Playing state.
    pub async fn play(&mut self, _url: &str) -> Result<(), PlaybackError> {
        // TODO: set uri on uridecodebin, set pipeline to Playing
        Err(PlaybackError::InvalidState("not implemented".into()))
    }

    /// Pause the pipeline.
    pub async fn pause(&mut self) -> Result<(), PlaybackError> {
        Err(PlaybackError::InvalidState("not implemented".into()))
    }

    /// Resume after a pause.
    pub async fn resume(&mut self) -> Result<(), PlaybackError> {
        Err(PlaybackError::InvalidState("not implemented".into()))
    }

    /// Stop and tear down the pipeline.
    pub async fn stop(&mut self) -> Result<(), PlaybackError> {
        Err(PlaybackError::InvalidState("not implemented".into()))
    }

    /// Seek to an absolute position in milliseconds.
    pub async fn seek(&mut self, _position_ms: u64) -> Result<(), PlaybackError> {
        Err(PlaybackError::SeekFailed("not implemented".into()))
    }

    /// Set the playback volume.
    pub async fn set_volume(&mut self, _volume: f64) -> Result<(), PlaybackError> {
        Err(PlaybackError::InvalidState("not implemented".into()))
    }

    /// Return the current playback position in milliseconds.
    pub async fn position_ms(&self) -> Result<u64, PlaybackError> {
        Err(PlaybackError::InvalidState("not implemented".into()))
    }

    /// Query the current buffer health.
    pub fn buffer_health(&self) -> BufferHealth {
        BufferHealth::default()
    }

    /// Return a reference to the pipeline configuration.
    pub fn config(&self) -> &PipelineConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_config_defaults() {
        let config = PipelineConfig::default();
        assert_eq!(config.video_sink, "kmssink");
        assert_eq!(config.audio_sink, "alsasink");
        assert_eq!(config.buffer_duration_ms, 3000);
        assert!(config.hw_accel);
        assert!((config.volume - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn buffer_health_defaults() {
        let health = BufferHealth::default();
        assert_eq!(health.fill_percent, 100);
        assert!((health.buffered_seconds - 0.0).abs() < f64::EPSILON);
        assert!(health.estimated_fill_ms.is_none());
        assert!(!health.is_buffering);
    }

    #[test]
    fn playback_error_display() {
        let err = PlaybackError::PipelineCreation("bad pipeline".into());
        assert!(err.to_string().contains("pipeline creation failed"));

        let err = PlaybackError::Gstreamer("gst error".into());
        assert!(err.to_string().contains("gstreamer error"));

        let err = PlaybackError::InvalidState("wrong state".into());
        assert!(err.to_string().contains("invalid state for operation"));

        let err = PlaybackError::SeekFailed("cannot seek".into());
        assert!(err.to_string().contains("seek failed"));
    }

    #[test]
    fn pipeline_config_serialization_roundtrip() {
        let config = PipelineConfig::default();
        let json = serde_json::to_string(&config).expect("serialize");
        let deserialized: PipelineConfig =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized.video_sink, config.video_sink);
        assert_eq!(deserialized.audio_sink, config.audio_sink);
        assert_eq!(deserialized.buffer_duration_ms, config.buffer_duration_ms);
        assert_eq!(deserialized.hw_accel, config.hw_accel);
        assert!((deserialized.volume - config.volume).abs() < f64::EPSILON);
    }

    #[test]
    fn buffer_health_serialization_roundtrip() {
        let health = BufferHealth::default();
        let json = serde_json::to_string(&health).expect("serialize");
        let deserialized: BufferHealth =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized.fill_percent, health.fill_percent);
        assert!((deserialized.buffered_seconds - health.buffered_seconds).abs() < f64::EPSILON);
        assert_eq!(deserialized.estimated_fill_ms, health.estimated_fill_ms);
        assert_eq!(deserialized.is_buffering, health.is_buffering);
    }

    #[test]
    fn playback_engine_new_with_default_config() {
        let engine = PlaybackEngine::new(PipelineConfig::default());
        assert!(engine.is_ok(), "PlaybackEngine::new should succeed with default config");
    }
}
