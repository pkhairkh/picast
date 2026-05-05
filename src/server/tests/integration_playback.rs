//! Playback integration tests.
//!
//! These tests verify default configurations and error types exposed
//! by the `picast_playback` crate.

mod common;

use picast_playback::{BufferHealth, PlaybackError, PipelineConfig};

/// Verify that [`PipelineConfig::default`] returns the expected
/// Raspberry Pi–oriented defaults.
#[test]
fn test_pipeline_config_default() {
    let config = PipelineConfig::default();
    assert_eq!(config.video_sink, "kmssink");
    assert_eq!(config.audio_sink, "alsasink");
    assert_eq!(config.buffer_duration_ms, 3000);
    assert!(config.hw_accel);
    assert!((config.volume - 1.0).abs() < f64::EPSILON);
}

/// Verify that [`BufferHealth::default`] returns sensible initial
/// values (full buffer, not buffering).
#[test]
fn test_buffer_health_default_values() {
    let health = BufferHealth::default();
    assert_eq!(health.fill_percent, 100);
    assert!((health.buffered_seconds - 0.0).abs() < f64::EPSILON);
    assert!(health.estimated_fill_ms.is_none());
    assert!(!health.is_buffering);
}

/// Verify that [`PlaybackError`] variants produce the expected
/// display strings.
#[test]
fn test_playback_error_types() {
    let err = PlaybackError::PipelineCreation("test".into());
    assert!(err.to_string().contains("pipeline creation failed"));

    let err = PlaybackError::Gstreamer("test".into());
    assert!(err.to_string().contains("gstreamer error"));

    let err = PlaybackError::InvalidState("test".into());
    assert!(err.to_string().contains("invalid state for operation"));

    let err = PlaybackError::SeekFailed("test".into());
    assert!(err.to_string().contains("seek failed"));
}
