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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;
#[cfg(feature = "hw")]
use tokio::sync::mpsc;
#[cfg(feature = "hw")]
use tokio::sync::Mutex;

// ── Playback State ───────────────────────────────────────────────────

/// Current state of the playback engine (used in mock mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackState {
    Stopped,
    Playing,
    Paused,
}

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
/// When compiled without the `hw` feature, the engine simulates a
/// working playback pipeline so the HTTP → Session → Playback chain
/// can be tested on x86.
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

    // ── Mock-mode state (only compiled without `hw` feature) ──────
    /// Whether a URL is loaded in mock mode.
    #[cfg(not(feature = "hw"))]
    mock_loaded: AtomicBool,
    /// Whether playback is active in mock mode.
    #[cfg(not(feature = "hw"))]
    mock_playing: AtomicBool,
    /// Whether playback is paused in mock mode.
    #[cfg(not(feature = "hw"))]
    mock_paused: AtomicBool,
    /// Current playback position in ms (mock mode).
    #[cfg(not(feature = "hw"))]
    mock_position_ms: AtomicU64,
    /// Total duration in ms (mock mode, default 300000 = 5 min).
    #[cfg(not(feature = "hw"))]
    mock_duration_ms: AtomicU64,
    /// Current volume 0.0–1.0 (mock mode).
    #[cfg(not(feature = "hw"))]
    mock_volume: std::sync::Mutex<f64>,
    /// Buffer health (mock mode).
    #[cfg(not(feature = "hw"))]
    mock_buffer_health: std::sync::Mutex<BufferHealth>,
    /// Currently loaded URL (mock mode).
    #[cfg(not(feature = "hw"))]
    mock_url: std::sync::Mutex<Option<String>>,
}

impl PlaybackEngine {
    /// Create a new engine with the given pipeline configuration.
    ///
    /// Initialises GStreamer on first call. The engine starts in an
    /// idle state with no pipeline loaded.
    pub fn new(config: PipelineConfig) -> Result<Self, PlaybackError> {
        // GStreamer init is deferred to pipeline construction.
        #[cfg(not(feature = "hw"))]
        let initial_volume = config.volume;

        Ok(Self {
            config,
            #[cfg(feature = "hw")]
            gst_pipeline: Arc::new(Mutex::new(None)),
            #[cfg(feature = "hw")]
            event_tx: mpsc::channel(64).0,
            is_playing: Arc::new(AtomicBool::new(false)),
            #[cfg(not(feature = "hw"))]
            mock_loaded: AtomicBool::new(false),
            #[cfg(not(feature = "hw"))]
            mock_playing: AtomicBool::new(false),
            #[cfg(not(feature = "hw"))]
            mock_paused: AtomicBool::new(false),
            #[cfg(not(feature = "hw"))]
            mock_position_ms: AtomicU64::new(0),
            #[cfg(not(feature = "hw"))]
            mock_duration_ms: AtomicU64::new(300_000),
            #[cfg(not(feature = "hw"))]
            mock_volume: std::sync::Mutex::new(initial_volume),
            #[cfg(not(feature = "hw"))]
            mock_buffer_health: std::sync::Mutex::new(BufferHealth::default()),
            #[cfg(not(feature = "hw"))]
            mock_url: std::sync::Mutex::new(None),
        })
    }

    /// Create a new engine with a custom event channel size.
    pub fn with_channel_size(
        config: PipelineConfig,
        _channel_size: usize,
    ) -> Result<Self, PlaybackError> {
        #[cfg(feature = "hw")]
        let event_tx = mpsc::channel(_channel_size).0;
        #[cfg(not(feature = "hw"))]
        let initial_volume = config.volume;

        Ok(Self {
            config,
            #[cfg(feature = "hw")]
            gst_pipeline: Arc::new(Mutex::new(None)),
            #[cfg(feature = "hw")]
            event_tx,
            is_playing: Arc::new(AtomicBool::new(false)),
            #[cfg(not(feature = "hw"))]
            mock_loaded: AtomicBool::new(false),
            #[cfg(not(feature = "hw"))]
            mock_playing: AtomicBool::new(false),
            #[cfg(not(feature = "hw"))]
            mock_paused: AtomicBool::new(false),
            #[cfg(not(feature = "hw"))]
            mock_position_ms: AtomicU64::new(0),
            #[cfg(not(feature = "hw"))]
            mock_duration_ms: AtomicU64::new(300_000),
            #[cfg(not(feature = "hw"))]
            mock_volume: std::sync::Mutex::new(initial_volume),
            #[cfg(not(feature = "hw"))]
            mock_buffer_health: std::sync::Mutex::new(BufferHealth::default()),
            #[cfg(not(feature = "hw"))]
            mock_url: std::sync::Mutex::new(None),
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

    /// Load a URL and transition to the Playing state (mock mode).
    #[cfg(not(feature = "hw"))]
    pub async fn play(
        &self,
        url: &str,
        _socks_addr: &str,
        _isolation_username: &str,
    ) -> Result<(), PlaybackError> {
        // Store the URL
        {
            let mut guard = self.mock_url.lock().unwrap();
            *guard = Some(url.to_string());
        }
        self.mock_loaded.store(true, Ordering::Relaxed);
        self.mock_playing.store(true, Ordering::Relaxed);
        self.mock_paused.store(false, Ordering::Relaxed);
        self.mock_position_ms.store(0, Ordering::Relaxed);
        self.is_playing.store(true, Ordering::Relaxed);

        // Set buffer health to full (healthy)
        {
            let mut guard = self.mock_buffer_health.lock().unwrap();
            *guard = BufferHealth {
                fill_percent: 100,
                buffered_seconds: 300.0,
                estimated_fill_ms: None,
                is_buffering: false,
            };
        }

        Ok(())
    }

    /// Pause the pipeline.
    #[cfg(feature = "hw")]
    pub async fn pause(&self) -> Result<(), PlaybackError> {
        let mut guard = self.gst_pipeline.lock().await;
        let pipeline = guard.as_mut().ok_or(PlaybackError::NoPipeline)?;
        pipeline.pause()
    }

    /// Pause the pipeline (mock mode).
    #[cfg(not(feature = "hw"))]
    pub async fn pause(&self) -> Result<(), PlaybackError> {
        if !self.mock_loaded.load(Ordering::Relaxed) {
            return Err(PlaybackError::NoPipeline);
        }
        self.mock_playing.store(false, Ordering::Relaxed);
        self.mock_paused.store(true, Ordering::Relaxed);
        self.is_playing.store(false, Ordering::Relaxed);
        Ok(())
    }

    /// Resume after a pause.
    #[cfg(feature = "hw")]
    pub async fn resume(&self) -> Result<(), PlaybackError> {
        let mut guard = self.gst_pipeline.lock().await;
        let pipeline = guard.as_mut().ok_or(PlaybackError::NoPipeline)?;
        pipeline.resume()
    }

    /// Resume after a pause (mock mode).
    #[cfg(not(feature = "hw"))]
    pub async fn resume(&self) -> Result<(), PlaybackError> {
        if !self.mock_loaded.load(Ordering::Relaxed) {
            return Err(PlaybackError::NoPipeline);
        }
        if !self.mock_paused.load(Ordering::Relaxed) {
            return Err(PlaybackError::InvalidState("cannot resume — not paused".into()));
        }
        self.mock_playing.store(true, Ordering::Relaxed);
        self.mock_paused.store(false, Ordering::Relaxed);
        self.is_playing.store(true, Ordering::Relaxed);
        Ok(())
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

    /// Stop and tear down the pipeline (mock mode).
    #[cfg(not(feature = "hw"))]
    pub async fn stop(&self) -> Result<(), PlaybackError> {
        self.mock_loaded.store(false, Ordering::Relaxed);
        self.mock_playing.store(false, Ordering::Relaxed);
        self.mock_paused.store(false, Ordering::Relaxed);
        self.mock_position_ms.store(0, Ordering::Relaxed);
        self.is_playing.store(false, Ordering::Relaxed);

        // Clear the URL
        {
            let mut guard = self.mock_url.lock().unwrap();
            *guard = None;
        }

        Ok(())
    }

    /// Seek to an absolute position in milliseconds.
    #[cfg(feature = "hw")]
    pub async fn seek(&self, position_ms: u64) -> Result<(), PlaybackError> {
        let mut guard = self.gst_pipeline.lock().await;
        let pipeline = guard.as_mut().ok_or(PlaybackError::NoPipeline)?;
        pipeline.seek(position_ms)
    }

    /// Seek to an absolute position in milliseconds (mock mode).
    #[cfg(not(feature = "hw"))]
    pub async fn seek(&self, position_ms: u64) -> Result<(), PlaybackError> {
        if !self.mock_loaded.load(Ordering::Relaxed) {
            return Err(PlaybackError::NoPipeline);
        }
        let duration = self.mock_duration_ms.load(Ordering::Relaxed);
        let clamped = position_ms.min(duration);
        self.mock_position_ms.store(clamped, Ordering::Relaxed);
        Ok(())
    }

    /// Set the playback volume (0.0–1.0).
    #[cfg(feature = "hw")]
    pub async fn set_volume(&self, volume: f64) -> Result<(), PlaybackError> {
        let mut guard = self.gst_pipeline.lock().await;
        let pipeline = guard.as_mut().ok_or(PlaybackError::NoPipeline)?;
        pipeline.set_volume(volume)
    }

    /// Set the playback volume (mock mode).
    #[cfg(not(feature = "hw"))]
    pub async fn set_volume(&self, volume: f64) -> Result<(), PlaybackError> {
        let clamped = volume.clamp(0.0, 1.0);
        let mut guard = self.mock_volume.lock().unwrap();
        *guard = clamped;
        Ok(())
    }

    /// Return the current playback position in milliseconds.
    #[cfg(feature = "hw")]
    pub async fn position_ms(&self) -> Result<u64, PlaybackError> {
        let guard = self.gst_pipeline.lock().await;
        let pipeline = guard.as_ref().ok_or(PlaybackError::NoPipeline)?;
        pipeline.position_ms()
    }

    /// Return the current playback position in milliseconds (mock mode).
    #[cfg(not(feature = "hw"))]
    pub async fn position_ms(&self) -> Result<u64, PlaybackError> {
        if !self.mock_loaded.load(Ordering::Relaxed) {
            return Err(PlaybackError::NoPipeline);
        }
        Ok(self.mock_position_ms.load(Ordering::Relaxed))
    }

    /// Return the total duration in milliseconds.
    #[cfg(feature = "hw")]
    pub async fn duration_ms(&self) -> Result<Option<u64>, PlaybackError> {
        let guard = self.gst_pipeline.lock().await;
        let pipeline = guard.as_ref().ok_or(PlaybackError::NoPipeline)?;
        pipeline.duration_ms()
    }

    /// Return the total duration in milliseconds (mock mode).
    #[cfg(not(feature = "hw"))]
    pub async fn duration_ms(&self) -> Result<Option<u64>, PlaybackError> {
        if !self.mock_loaded.load(Ordering::Relaxed) {
            return Err(PlaybackError::NoPipeline);
        }
        Ok(Some(self.mock_duration_ms.load(Ordering::Relaxed)))
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

    /// Query the current buffer health (mock mode — always healthy).
    #[cfg(not(feature = "hw"))]
    pub async fn buffer_health(&self) -> BufferHealth {
        let guard = self.mock_buffer_health.lock().unwrap();
        *guard
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

    /// Return the current playback state (mock mode).
    #[cfg(not(feature = "hw"))]
    pub fn mock_state(&self) -> PlaybackState {
        if self.mock_playing.load(Ordering::Relaxed) {
            PlaybackState::Playing
        } else if self.mock_paused.load(Ordering::Relaxed) {
            PlaybackState::Paused
        } else {
            PlaybackState::Stopped
        }
    }

    /// Set the mock duration in milliseconds (for testing).
    #[cfg(not(feature = "hw"))]
    pub fn mock_set_duration(&self, duration_ms: u64) {
        self.mock_duration_ms.store(duration_ms, Ordering::Relaxed);
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

    // ── Mock-mode tests ───────────────────────────────────────────

    #[tokio::test]
    async fn mock_play_pause_resume_stop_lifecycle() {
        let engine = PlaybackEngine::new(PipelineConfig::default()).unwrap();

        // Initially stopped
        assert_eq!(engine.mock_state(), PlaybackState::Stopped);
        assert!(!engine.is_playing());

        // Play
        engine
            .play("https://example.com/video.mp4", "", "")
            .await
            .expect("mock play should succeed");
        assert_eq!(engine.mock_state(), PlaybackState::Playing);
        assert!(engine.is_playing());

        // Pause
        engine.pause().await.expect("mock pause should succeed");
        assert_eq!(engine.mock_state(), PlaybackState::Paused);
        assert!(!engine.is_playing());

        // Resume
        engine.resume().await.expect("mock resume should succeed");
        assert_eq!(engine.mock_state(), PlaybackState::Playing);
        assert!(engine.is_playing());

        // Stop
        engine.stop().await.expect("mock stop should succeed");
        assert_eq!(engine.mock_state(), PlaybackState::Stopped);
        assert!(!engine.is_playing());
    }

    #[tokio::test]
    async fn mock_seek_updates_position() {
        let engine = PlaybackEngine::new(PipelineConfig::default()).unwrap();

        // Seek without a loaded URL should fail
        let result = engine.seek(5000).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            PlaybackError::NoPipeline => {},
            other => panic!("Expected NoPipeline, got {:?}", other),
        }

        // Load a URL
        engine.play("https://example.com/video.mp4", "", "").await.unwrap();

        // Seek to 5000 ms
        engine.seek(5000).await.expect("mock seek should succeed");
        let pos = engine.position_ms().await.expect("position_ms should succeed");
        assert_eq!(pos, 5000);

        // Seek beyond duration should clamp
        engine.mock_set_duration(300_000);
        engine.seek(999_999).await.expect("seek beyond duration should succeed");
        let pos = engine.position_ms().await.expect("position_ms should succeed");
        assert_eq!(pos, 300_000, "position should be clamped to duration");
    }

    #[tokio::test]
    async fn mock_volume_setting() {
        let engine = PlaybackEngine::new(PipelineConfig::default()).unwrap();

        // Set volume (works even without a loaded URL)
        engine.set_volume(0.5).await.expect("mock set_volume should succeed");
        {
            let vol = engine.mock_volume.lock().unwrap();
            assert!((*vol - 0.5).abs() < f64::EPSILON, "volume should be 0.5");
        }

        // Clamp above 1.0
        engine.set_volume(1.5).await.expect("set_volume should succeed");
        {
            let vol = engine.mock_volume.lock().unwrap();
            assert!((*vol - 1.0).abs() < f64::EPSILON, "volume should be clamped to 1.0");
        }

        // Clamp below 0.0
        engine.set_volume(-0.5).await.expect("set_volume should succeed");
        {
            let vol = engine.mock_volume.lock().unwrap();
            assert!((*vol - 0.0).abs() < f64::EPSILON, "volume should be clamped to 0.0");
        }
    }

    #[tokio::test]
    async fn mock_position_and_duration_return_correct_values() {
        let engine = PlaybackEngine::new(PipelineConfig::default()).unwrap();

        // Without a loaded URL, should fail
        assert!(engine.position_ms().await.is_err());
        assert!(engine.duration_ms().await.is_err());

        // Load a URL
        engine.play("https://example.com/video.mp4", "", "").await.unwrap();

        // Position should be 0 right after play
        let pos = engine.position_ms().await.expect("position_ms should succeed");
        assert_eq!(pos, 0, "position should be 0 after play");

        // Default duration is 300000 ms (5 min)
        let dur = engine.duration_ms().await.expect("duration_ms should succeed");
        assert_eq!(dur, Some(300_000), "default duration should be 300000 ms");

        // Set custom duration
        engine.mock_set_duration(600_000);
        let dur = engine.duration_ms().await.expect("duration_ms should succeed");
        assert_eq!(dur, Some(600_000), "duration should be updated to 600000 ms");
    }

    #[tokio::test]
    async fn mock_buffer_health_returns_healthy() {
        let engine = PlaybackEngine::new(PipelineConfig::default()).unwrap();

        let health = engine.buffer_health().await;
        assert_eq!(health.fill_percent, 100);
        assert!(!health.is_buffering);

        // After play, buffer health should be healthy
        engine.play("https://example.com/video.mp4", "", "").await.unwrap();

        let health = engine.buffer_health().await;
        assert_eq!(health.fill_percent, 100);
        assert!(!health.is_buffering);
        assert!(health.estimated_fill_ms.is_none());
    }

    #[tokio::test]
    async fn mock_operations_fail_when_no_url_loaded() {
        let engine = PlaybackEngine::new(PipelineConfig::default()).unwrap();

        // Without a loaded URL, these should return NoPipeline
        let result = engine.pause().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            PlaybackError::NoPipeline => {},
            other => panic!("Expected NoPipeline for pause, got {:?}", other),
        }

        let result = engine.resume().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            PlaybackError::NoPipeline => {},
            other => panic!("Expected NoPipeline for resume, got {:?}", other),
        }

        let result = engine.seek(0).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            PlaybackError::NoPipeline => {},
            other => panic!("Expected NoPipeline for seek, got {:?}", other),
        }

        let result = engine.position_ms().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            PlaybackError::NoPipeline => {},
            other => panic!("Expected NoPipeline for position_ms, got {:?}", other),
        }

        let result = engine.duration_ms().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            PlaybackError::NoPipeline => {},
            other => panic!("Expected NoPipeline for duration_ms, got {:?}", other),
        }

        // Stop should succeed even without a loaded URL (idempotent)
        engine.stop().await.expect("stop should succeed without loaded URL");

        // set_volume should succeed without a loaded URL (no pipeline check needed)
        engine.set_volume(0.5).await.expect("set_volume should succeed without loaded URL");
    }

    #[tokio::test]
    async fn mock_play_resets_position() {
        let engine = PlaybackEngine::new(PipelineConfig::default()).unwrap();

        // Load and seek to some position
        engine.play("https://example.com/video.mp4", "", "").await.unwrap();
        engine.seek(120_000).await.unwrap();
        let pos = engine.position_ms().await.unwrap();
        assert_eq!(pos, 120_000);

        // Play again — position should reset to 0
        engine.play("https://example.com/other.mp4", "", "").await.unwrap();
        let pos = engine.position_ms().await.unwrap();
        assert_eq!(pos, 0, "position should reset to 0 on play");
    }

    #[tokio::test]
    async fn mock_resume_fails_when_not_paused() {
        let engine = PlaybackEngine::new(PipelineConfig::default()).unwrap();

        // Load and play
        engine.play("https://example.com/video.mp4", "", "").await.unwrap();

        // Resume while playing should fail
        let result = engine.resume().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            PlaybackError::InvalidState(msg) => {
                assert!(msg.contains("not paused"), "error message should mention 'not paused'");
            },
            other => panic!("Expected InvalidState, got {:?}", other),
        }
    }

    #[test]
    fn playback_state_serialization_roundtrip() {
        let states = vec![PlaybackState::Stopped, PlaybackState::Playing, PlaybackState::Paused];
        for state in states {
            let json = serde_json::to_string(&state).expect("serialize");
            let deserialized: PlaybackState = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(deserialized, state);
        }
    }

    #[test]
    fn playback_state_serde_rename() {
        let json = serde_json::to_string(&PlaybackState::Playing).unwrap();
        assert_eq!(json, "\"playing\"");
        let json = serde_json::to_string(&PlaybackState::Paused).unwrap();
        assert_eq!(json, "\"paused\"");
        let json = serde_json::to_string(&PlaybackState::Stopped).unwrap();
        assert_eq!(json, "\"stopped\"");
    }
}
