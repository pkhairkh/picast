//! PiCast Playback Engine
//!
//! Wraps GStreamer into a high-level playback API tailored for the
//! Raspberry Pi 4B+. The engine manages:
//!
//! - Pipeline construction (souphttpsrc → queue2 → parsebin → V4L2 → kmssink).
//! - Adaptive bitrate control for HLS / DASH streams.
//! - Buffer health monitoring and stall detection.
//! - Volume, seek, and rate-change commands.
//! - Event dispatch to the session layer via `mpsc` channel.
//!
//! ## Usage
//!
//! ```ignore
//! use picast_playback::{PlaybackEngine, PipelineConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let config = PipelineConfig::default();
//!     let mut engine = PlaybackEngine::new(config)?;
//!
//!     let mut events = engine.events();
//!     engine.play("https://example.com/video.mp4", "127.0.0.1:9050", "picast-abc123").await?;
//!
//!     while let Ok(event) = events.recv().await {
//!         println!("Event: {:?}", event);
//!     }
//!     Ok(())
//! }
//! ```

#[cfg(feature = "hw")]
pub mod events;
#[cfg(feature = "hw")]
pub mod pipeline;

#[cfg(feature = "hw")]
use events::PlaybackEvent;
#[cfg(feature = "hw")]
use pipeline::{GstPipeline, PipelineState};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use thiserror::Error;
#[cfg(feature = "hw")]
use tokio::sync::mpsc;
#[cfg(feature = "hw")]
use tokio::sync::Mutex;

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

    /// No pipeline is currently loaded.
    #[error("no pipeline loaded — call play() first")]
    NoPipeline,

    /// Hardware playback is not available (compiled without the `hw` feature).
    #[error("hardware playback unavailable — compile with the 'hw' feature")]
    HardwareUnavailable,
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
    /// Whether to enable hardware-accelerated decoding (V4L2 M2M).
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
/// element property changes. Events are pushed to an `mpsc` channel
/// so the session layer can react asynchronously.
///
/// When compiled without the `hw` feature, the engine is constructable
/// but all playback operations return [`PlaybackError::HardwareUnavailable`].
pub struct PlaybackEngine {
    /// Pipeline configuration.
    config: PipelineConfig,
    /// The GStreamer pipeline (wrapped in Arc<Mutex> for thread safety).
    #[cfg(feature = "hw")]
    gst_pipeline: Arc<Mutex<Option<GstPipeline>>>,
    /// Event sender — cloned receivers are handed out via `events()`.
    #[cfg(feature = "hw")]
    event_tx: mpsc::Sender<PlaybackEvent>,
    /// Whether the engine is currently playing.
    is_playing: Arc<AtomicBool>,
}

impl PlaybackEngine {
    /// Create a new engine with the given pipeline configuration.
    ///
    /// Initialises GStreamer on first call. The engine starts in an
    /// idle state with no pipeline loaded.
    pub fn new(config: PipelineConfig) -> Result<Self, PlaybackError> {
        // GStreamer init is deferred to pipeline construction.
        Ok(Self {
            config,
            #[cfg(feature = "hw")]
            gst_pipeline: Arc::new(Mutex::new(None)),
            #[cfg(feature = "hw")]
            event_tx: mpsc::channel(64).0,
            is_playing: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Create a new engine with a custom event channel size.
    pub fn with_channel_size(
        config: PipelineConfig,
        _channel_size: usize,
    ) -> Result<Self, PlaybackError> {
        #[cfg(feature = "hw")]
        let event_tx = mpsc::channel(_channel_size).0;
        Ok(Self {
            config,
            #[cfg(feature = "hw")]
            gst_pipeline: Arc::new(Mutex::new(None)),
            #[cfg(feature = "hw")]
            event_tx,
            is_playing: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Load a URL and transition to the Playing state.
    ///
    /// Constructs a GStreamer pipeline with the configured video/audio
    /// sinks and routes traffic through the Tor SOCKS5h proxy if
    /// `socks_addr` is non-empty.
    ///
    /// The `isolation_username` is used as the SOCKS5 username for
    /// Tor's `IsolateSOCKSAuth` circuit isolation.
    #[cfg(feature = "hw")]
    pub async fn play(
        &self,
        url: &str,
        socks_addr: &str,
        isolation_username: &str,
    ) -> Result<(), PlaybackError> {
        let mut guard = self.gst_pipeline.lock().await;

        // Stop any existing pipeline.
        if let Some(ref mut existing) = *guard {
            let _ = existing.stop();
        }

        tracing::info!(
            url = url,
            socks = socks_addr,
            hw_accel = self.config.hw_accel,
            "constructing playback pipeline"
        );

        let mut pipeline = GstPipeline::new(url, socks_addr, isolation_username, &self.config)?;

        // Set up bus watch to forward GStreamer messages as events.
        let event_tx = self.event_tx.clone();
        let is_playing = self.is_playing.clone();
        let bus = pipeline.pipeline().bus().expect("pipeline should have a bus");

        bus.add_watch(move |_bus, msg| {
            use gstreamer::MessageView;

            match msg.view() {
                MessageView::StateChanged(s) => {
                    if let Some(src) = s.src() {
                        if src == msg.src() {
                            let old = s.old();
                            let current = s.current();
                            let new_state = s.current();

                            if new_state == State::Playing {
                                let _ = event_tx.try_send(PlaybackEvent::Playing);
                                is_playing.store(true, Ordering::Relaxed);
                            } else if new_state == State::Paused {
                                let _ = event_tx.try_send(PlaybackEvent::Paused);
                                is_playing.store(false, Ordering::Relaxed);
                            }
                        }
                    }
                },
                MessageView::Eos(_) => {
                    tracing::info!("end of stream reached");
                    let _ = event_tx.try_send(PlaybackEvent::EndOfStream);
                    is_playing.store(false, Ordering::Relaxed);
                },
                MessageView::Error(e) => {
                    let msg = e.error().to_string();
                    let debug = e.debug().map(|d| d.to_string());
                    tracing::error!(error = %msg, debug = ?debug, "GStreamer error");
                    let _ = event_tx.try_send(PlaybackEvent::Error { message: msg, debug });
                    is_playing.store(false, Ordering::Relaxed);
                },
                MessageView::Buffering(b) => {
                    let percent = b.percent() as u8;
                    tracing::debug!(percent = percent, "buffering progress");
                    let _ = event_tx.try_send(PlaybackEvent::Buffering { percent });
                },
                MessageView::Latency(l) => {
                    // Latency message — not forwarding in v1.
                },
                _ => {},
            }

            gstreamer::Continue(true)
        })
        .expect("failed to add bus watch");

        // Start playback.
        pipeline.play()?;
        *guard = Some(pipeline);

        Ok(())
    }

    /// Load a URL and transition to the Playing state (stub without hardware).
    #[cfg(not(feature = "hw"))]
    pub async fn play(
        &self,
        _url: &str,
        _socks_addr: &str,
        _isolation_username: &str,
    ) -> Result<(), PlaybackError> {
        Err(PlaybackError::HardwareUnavailable)
    }

    /// Pause the pipeline.
    #[cfg(feature = "hw")]
    pub async fn pause(&self) -> Result<(), PlaybackError> {
        let mut guard = self.gst_pipeline.lock().await;
        let pipeline = guard.as_mut().ok_or(PlaybackError::NoPipeline)?;
        pipeline.pause()
    }

    /// Pause the pipeline (stub without hardware).
    #[cfg(not(feature = "hw"))]
    pub async fn pause(&self) -> Result<(), PlaybackError> {
        Err(PlaybackError::HardwareUnavailable)
    }

    /// Resume after a pause.
    #[cfg(feature = "hw")]
    pub async fn resume(&self) -> Result<(), PlaybackError> {
        let mut guard = self.gst_pipeline.lock().await;
        let pipeline = guard.as_mut().ok_or(PlaybackError::NoPipeline)?;
        pipeline.resume()
    }

    /// Resume after a pause (stub without hardware).
    #[cfg(not(feature = "hw"))]
    pub async fn resume(&self) -> Result<(), PlaybackError> {
        Err(PlaybackError::HardwareUnavailable)
    }

    /// Stop and tear down the pipeline.
    #[cfg(feature = "hw")]
    pub async fn stop(&self) -> Result<(), PlaybackError> {
        let mut guard = self.gst_pipeline.lock().await;
        if let Some(ref mut pipeline) = *guard {
            pipeline.stop()?;
            *guard = None;
            self.is_playing.store(false, Ordering::Relaxed);
            let _ = self.event_tx.try_send(PlaybackEvent::Stopped);
        }
        Ok(())
    }

    /// Stop and tear down the pipeline (stub without hardware).
    #[cfg(not(feature = "hw"))]
    pub async fn stop(&self) -> Result<(), PlaybackError> {
        Err(PlaybackError::HardwareUnavailable)
    }

    /// Seek to an absolute position in milliseconds.
    #[cfg(feature = "hw")]
    pub async fn seek(&self, position_ms: u64) -> Result<(), PlaybackError> {
        let mut guard = self.gst_pipeline.lock().await;
        let pipeline = guard.as_mut().ok_or(PlaybackError::NoPipeline)?;
        pipeline.seek(position_ms)
    }

    /// Seek to an absolute position in milliseconds (stub without hardware).
    #[cfg(not(feature = "hw"))]
    pub async fn seek(&self, _position_ms: u64) -> Result<(), PlaybackError> {
        Err(PlaybackError::HardwareUnavailable)
    }

    /// Set the playback volume (0.0–1.0).
    #[cfg(feature = "hw")]
    pub async fn set_volume(&self, volume: f64) -> Result<(), PlaybackError> {
        let mut guard = self.gst_pipeline.lock().await;
        let pipeline = guard.as_mut().ok_or(PlaybackError::NoPipeline)?;
        pipeline.set_volume(volume)
    }

    /// Set the playback volume (stub without hardware).
    #[cfg(not(feature = "hw"))]
    pub async fn set_volume(&self, _volume: f64) -> Result<(), PlaybackError> {
        Err(PlaybackError::HardwareUnavailable)
    }

    /// Return the current playback position in milliseconds.
    #[cfg(feature = "hw")]
    pub async fn position_ms(&self) -> Result<u64, PlaybackError> {
        let guard = self.gst_pipeline.lock().await;
        let pipeline = guard.as_ref().ok_or(PlaybackError::NoPipeline)?;
        pipeline.position_ms()
    }

    /// Return the current playback position in milliseconds (stub without hardware).
    #[cfg(not(feature = "hw"))]
    pub async fn position_ms(&self) -> Result<u64, PlaybackError> {
        Err(PlaybackError::HardwareUnavailable)
    }

    /// Return the total duration in milliseconds.
    #[cfg(feature = "hw")]
    pub async fn duration_ms(&self) -> Result<Option<u64>, PlaybackError> {
        let guard = self.gst_pipeline.lock().await;
        let pipeline = guard.as_ref().ok_or(PlaybackError::NoPipeline)?;
        pipeline.duration_ms()
    }

    /// Return the total duration in milliseconds (stub without hardware).
    #[cfg(not(feature = "hw"))]
    pub async fn duration_ms(&self) -> Result<Option<u64>, PlaybackError> {
        Err(PlaybackError::HardwareUnavailable)
    }

    /// Query the current buffer health.
    #[cfg(feature = "hw")]
    pub async fn buffer_health(&self) -> BufferHealth {
        let guard = self.gst_pipeline.lock().await;
        match guard.as_ref() {
            Some(pipeline) => pipeline.buffer_health(),
            None => BufferHealth::default(),
        }
    }

    /// Query the current buffer health (stub without hardware).
    #[cfg(not(feature = "hw"))]
    pub async fn buffer_health(&self) -> BufferHealth {
        BufferHealth::default()
    }

    /// Return a receiver for playback events.
    ///
    /// Each call creates a new receiver. The sender is the same
    /// internal channel, so events are distributed to all receivers.
    /// Note: this creates a new channel pair since mpsc is single-consumer.
    /// For broadcast semantics, use `tokio::sync::broadcast` in the
    /// session layer.
    #[cfg(feature = "hw")]
    pub fn events(&self) -> mpsc::Receiver<PlaybackEvent> {
        // Since mpsc::Sender is single-consumer, we need a workaround.
        // For v1, we'll use a broadcast channel internally.
        // This is a known limitation — the session layer wraps this
        // with its own broadcast.
        let (_, rx) = mpsc::channel(64);
        // TODO: implement proper event fan-out
        rx
    }

    /// Return a reference to the pipeline configuration.
    pub fn config(&self) -> &PipelineConfig {
        &self.config
    }

    /// Whether the engine is currently playing.
    pub fn is_playing(&self) -> bool {
        self.is_playing.load(Ordering::Relaxed)
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

        let err = PlaybackError::NoPipeline;
        assert!(err.to_string().contains("no pipeline loaded"));

        let err = PlaybackError::HardwareUnavailable;
        assert!(err.to_string().contains("hardware playback unavailable"));
    }

    #[test]
    fn pipeline_config_serialization_roundtrip() {
        let config = PipelineConfig::default();
        let json = serde_json::to_string(&config).expect("serialize");
        let deserialized: PipelineConfig = serde_json::from_str(&json).expect("deserialize");
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
        let deserialized: BufferHealth = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized.fill_percent, health.fill_percent);
        assert!((deserialized.buffered_seconds - health.buffered_seconds).abs() < f64::EPSILON);
        assert_eq!(deserialized.estimated_fill_ms, health.estimated_fill_ms);
        assert_eq!(deserialized.is_buffering, health.is_buffering);
    }

    #[test]
    fn playback_engine_new_with_default_config() {
        let engine = PlaybackEngine::new(PipelineConfig::default());
        assert!(engine.is_ok(), "PlaybackEngine::new should succeed with default config");
        let engine = engine.unwrap();
        assert!(!engine.is_playing());
    }

    #[test]
    fn playback_engine_custom_channel_size() {
        let engine = PlaybackEngine::with_channel_size(PipelineConfig::default(), 128);
        assert!(engine.is_ok());
    }

    #[tokio::test]
    async fn playback_engine_hw_unavailable_without_feature() {
        let engine = PlaybackEngine::new(PipelineConfig::default()).unwrap();

        // All playback methods should return HardwareUnavailable when hw feature is off.
        let result = engine.play("https://example.com/video.mp4", "", "").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            PlaybackError::HardwareUnavailable => {},
            other => panic!("Expected HardwareUnavailable, got {:?}", other),
        }

        assert!(engine.pause().await.is_err());
        assert!(engine.resume().await.is_err());
        assert!(engine.stop().await.is_err());
        assert!(engine.seek(0).await.is_err());
        assert!(engine.set_volume(0.5).await.is_err());
        assert!(engine.position_ms().await.is_err());
        assert!(engine.duration_ms().await.is_err());

        // buffer_health should return defaults without error.
        let health = engine.buffer_health().await;
        assert_eq!(health.fill_percent, 100);
    }
}
