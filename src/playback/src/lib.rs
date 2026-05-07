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
//!     engine.play("https://example.com/video.mp4", "https://example.com/page", "127.0.0.1:9050", "picast-abc123").await?;
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
mod socks_forwarder;

#[cfg(feature = "hw")]
use events::PlaybackEvent;
#[cfg(feature = "hw")]
use gstreamer::prelude::*;
#[cfg(feature = "hw")]
use gstreamer::State;
#[cfg(feature = "hw")]
use pipeline::GstPipeline;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(feature = "hw"))]
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use thiserror::Error;
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

/// Default plane ID — 0 means auto-detect (let kmssink pick the best plane).
///
/// On Raspberry Pi 4 with vc4, kmssink can auto-select the video overlay
/// plane. Hardcoding plane-id=1 may fail on different kernel versions or
/// display configurations where the plane numbering differs.
fn default_plane_id() -> u32 {
    0
}

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
    /// ALSA device string for the audio sink (e.g. `"plughw:1,0"` for HDMI).
    /// When empty, alsasink uses the ALSA default device.
    /// Use `plughw` (not `hw`) because plughw allows ALSA format
    /// conversion — HDMI devices may not accept the exact F32LE
    /// format that GStreamer negotiates.
    #[serde(default)]
    pub audio_device: String,
    /// Buffer duration in milliseconds for the stream buffer.
    pub buffer_duration_ms: u64,
    /// Whether to enable hardware-accelerated decoding (V4L2 M2M).
    pub hw_accel: bool,
    /// Initial volume (0.0 – 1.0).
    pub volume: f64,
    /// DRM plane ID for video overlay (kmssink plane-id property).
    /// Set to 0 to let kmssink auto-detect the best overlay plane.
    /// On Pi 4, the video overlay is typically on plane 1+ (not 0, which is the primary plane),
    /// but plane numbering varies by kernel version and vc4 configuration.
    #[serde(default = "default_plane_id")]
    pub plane_id: u32,
    /// DRM connector ID for kmssink (e.g. 33 for HDMI-A-1 on Pi 4).
    /// When `None`, kmssink auto-detects the first connected connector.
    /// Setting this explicitly avoids misdetection on multi-output setups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector_id: Option<u32>,
    /// Pre-opened DRM device file descriptor for kmssink.
    /// When `Some`, kmssink uses this fd instead of opening the device
    /// itself, which guarantees it shares our DRM master and can modeset.
    /// When `None`, kmssink opens the device itself.
    ///
    /// This field is not serialized — it's set programmatically at
    /// runtime from the DisplayManager.
    #[serde(skip)]
    pub drm_fd: Option<i32>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            video_sink: "kmssink".into(),
            audio_sink: "alsasink".into(),
            // Auto-detect HDMI audio device for Raspberry Pi 4.
            // The default ALSA device (empty string) typically routes to the
            // 3.5mm headphone jack, not HDMI.  On Pi 4, HDMI audio is usually
            // card 1 (vc4hdmi0).  If the detection fails, we fall back to
            // "plughw:1,0" which is the most common HDMI device on Pi 4.
            audio_device: detect_hdmi_audio_device(),
            buffer_duration_ms: 3000,
            hw_accel: true,
            volume: 1.0,
            plane_id: 0,
            connector_id: None,
            drm_fd: None,
        }
    }
}

/// Auto-detect the HDMI audio device on Raspberry Pi by parsing
/// `/proc/asound/cards` and `/proc/asound/pcm`.  Returns the ALSA
/// device string (e.g. `"plughw:1,0"`) for the first HDMI card found,
/// or `"plughw:1,0"` as a fallback if detection fails.
///
/// On Pi 4, the typical ALSA card layout is:
/// - Card 0: Headphones (bcm2835, 3.5mm jack)
/// - Card 1: vc4hdmi0 (HDMI 0 audio)
/// - Card 2: vc4hdmi1 (HDMI 1 audio, if dual HDMI)
fn detect_hdmi_audio_device() -> String {
    // Parse /proc/asound/cards for HDMI cards
    let cards_content = match std::fs::read_to_string("/proc/asound/cards") {
        Ok(c) => c,
        Err(_) => {
            tracing::info!(
                "audio auto-detect: /proc/asound/cards not available — \
                 defaulting to plughw:1,0 (HDMI on Pi 4)"
            );
            return "plughw:1,0".into();
        },
    };

    // Parse card entries: "index [shortname]: ... - longname"
    // Look for cards with "hdmi" in the short or long name
    let mut cards: Vec<(u32, String)> = Vec::new(); // (card_index, short_name)
    for line in cards_content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(bracket_end) = line.find(']') {
            let before_bracket = &line[..bracket_end];
            if let Some(idx_str) = before_bracket.split_whitespace().next() {
                if let Ok(card_idx) = idx_str.parse::<u32>() {
                    let short_name = before_bracket
                        .find('[')
                        .map(|pos| before_bracket[pos + 1..].trim().to_lowercase())
                        .unwrap_or_default();
                    let long_name = line
                        .split(" - ")
                        .nth(1)
                        .map(|s| s.trim().to_lowercase())
                        .unwrap_or_default();
                    if short_name.contains("hdmi") || long_name.contains("hdmi") {
                        cards.push((card_idx, short_name));
                    }
                }
            }
        }
    }

    if let Some((card_idx, name)) = cards.first() {
        tracing::info!(
            card_index = card_idx,
            card_name = %name,
            "audio auto-detect: found HDMI audio card"
        );
        format!("plughw:{},0", card_idx)
    } else {
        tracing::info!(
            "audio auto-detect: no HDMI card found — defaulting to plughw:1,0 (HDMI on Pi 4)"
        );
        "plughw:1,0".into()
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

// ── Decode Mode ──────────────────────────────────────────────────

/// Decode mode — tracks whether hardware or software decode is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecodeMode {
    /// Hardware-accelerated V4L2 M2M decode.
    Hardware,
    /// Software decode fallback (avdec_h264).
    Software,
}

impl std::fmt::Display for DecodeMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeMode::Hardware => write!(f, "hardware"),
            DecodeMode::Software => write!(f, "software"),
        }
    }
}

// ── Negotiation error detection ──────────────────────────────────────

/// Check if a GStreamer error message indicates a caps negotiation failure.
///
/// Negotiation errors typically contain phrases like "not negotiated",
/// "negotiation", or reference V4L2 elements. When these occur with
/// HW decode, we should automatically fall back to software decode.
#[cfg(feature = "hw")]
fn is_negotiation_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("not negotiated")
        || lower.contains("negotiation")
        || lower.contains("not-negotiated")
        || lower.contains("v4l2h264dec")
        || lower.contains("no common format")
        || lower.contains("could not link")
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
///
/// ## Decode Modes
///
/// The engine supports two decode modes:
/// - **Hardware** ([`DecodeMode::Hardware`]): Uses V4L2 M2M hardware-accelerated
///   H.264 decoding via `v4l2h264dec`. This is the default on Raspberry Pi 4B+.
/// - **Software** ([`DecodeMode::Software`]): Falls back to `avdec_h264` software
///   decoding when V4L2 decode is unavailable or fails to negotiate. Higher CPU
///   usage but works on any platform.
///
/// The engine automatically falls back from hardware to software decode when
/// GStreamer reports a negotiation error from `v4l2h264dec`. The current
/// decode mode can be queried via [`PlaybackEngine::decode_mode()`].
pub struct PlaybackEngine {
    /// Pipeline configuration.
    config: PipelineConfig,
    /// The GStreamer pipeline (wrapped in Arc<Mutex> for thread safety).
    #[cfg(feature = "hw")]
    gst_pipeline: Arc<Mutex<Option<GstPipeline>>>,
    /// Event sender — cloned receivers are handed out via `events()`.
    #[cfg(feature = "hw")]
    event_tx: tokio::sync::broadcast::Sender<PlaybackEvent>,
    /// Whether the engine is currently playing.
    is_playing: Arc<AtomicBool>,
    /// Whether the active pipeline fell back from HW to SW decode.
    /// Set to `true` after a successful SW fallback, reset on each new play().
    #[cfg(feature = "hw")]
    sw_fallback_active: Arc<AtomicBool>,

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
    /// Current decode mode (mock mode).
    #[cfg(not(feature = "hw"))]
    mock_decode_mode: std::sync::Mutex<DecodeMode>,
    /// Mock event broadcast channel (simulates GStreamer bus events).
    #[cfg(not(feature = "hw"))]
    mock_event_tx: tokio::sync::broadcast::Sender<PlaybackState>,
}

impl PlaybackEngine {
    /// Create a new engine with the given pipeline configuration.
    ///
    /// Initialises GStreamer on first call. The engine starts in an
    /// idle state with no pipeline loaded.
    pub fn new(config: PipelineConfig) -> Result<Self, PlaybackError> {
        // GStreamer init is deferred to pipeline construction.

        // Start a GLib main loop in a background thread so that
        // GStreamer bus watch callbacks are dispatched.  Without a
        // running main loop, `Bus::add_watch()` attaches a source to
        // the default main context that is never iterated, so bus
        // messages (errors, state changes, buffering) are silently
        // dropped — leaving us completely blind to pipeline failures.
        #[cfg(feature = "hw")]
        {
            static GLIB_MAIN_LOOP: std::sync::OnceLock<()> = std::sync::OnceLock::new();
            GLIB_MAIN_LOOP.get_or_init(|| {
                std::thread::Builder::new()
                    .name("glib-main-loop".into())
                    .spawn(|| {
                        let context = gstreamer::glib::MainContext::default();
                        // Bind the acquire guard to a named variable declared AFTER context,
                        // so Rust's reverse-order drop drops the guard before context.
                        // This avoids E0597: the temporary from context.acquire() in a
                        // match tail-expression would outlive context.
                        let guard = match context.acquire() {
                            Ok(g) => g,
                            Err(_) => {
                                tracing::error!(
                                    "Failed to acquire GLib main context — \
                                     bus watch callbacks will NOT be dispatched. \
                                     Pipeline errors and state changes will go unreported."
                                );
                                return;
                            }
                        };
                        let main_loop = gstreamer::glib::MainLoop::new(Some(&context), false);
                        tracing::info!(
                            "GLib main loop started — bus watch callbacks will be dispatched"
                        );
                        main_loop.run();
                        // guard is dropped first (reverse declaration order), then context.
                        // In practice main_loop.run() never returns, so neither is dropped.
                        let _ = guard;
                    })
                    .expect("Failed to spawn GLib main loop thread");
            });
        }

        #[cfg(not(feature = "hw"))]
        let initial_volume = config.volume;
        #[cfg(not(feature = "hw"))]
        let decode_mode = if config.hw_accel { DecodeMode::Hardware } else { DecodeMode::Software };
        #[cfg(not(feature = "hw"))]
        let (mock_event_tx, _) = tokio::sync::broadcast::channel(64);

        Ok(Self {
            config,
            #[cfg(feature = "hw")]
            gst_pipeline: Arc::new(Mutex::new(None)),
            #[cfg(feature = "hw")]
            event_tx: tokio::sync::broadcast::channel(64).0,
            is_playing: Arc::new(AtomicBool::new(false)),
            #[cfg(feature = "hw")]
            sw_fallback_active: Arc::new(AtomicBool::new(false)),
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
            #[cfg(not(feature = "hw"))]
            mock_decode_mode: std::sync::Mutex::new(decode_mode),
            #[cfg(not(feature = "hw"))]
            mock_event_tx,
        })
    }

    /// Create a new engine with a custom event channel size.
    pub fn with_channel_size(
        config: PipelineConfig,
        _channel_size: usize,
    ) -> Result<Self, PlaybackError> {
        #[cfg(feature = "hw")]
        let event_tx = tokio::sync::broadcast::channel(_channel_size).0;
        #[cfg(not(feature = "hw"))]
        let initial_volume = config.volume;
        #[cfg(not(feature = "hw"))]
        let decode_mode = if config.hw_accel { DecodeMode::Hardware } else { DecodeMode::Software };
        #[cfg(not(feature = "hw"))]
        let (mock_event_tx, _) = tokio::sync::broadcast::channel(_channel_size);

        Ok(Self {
            config,
            #[cfg(feature = "hw")]
            gst_pipeline: Arc::new(Mutex::new(None)),
            #[cfg(feature = "hw")]
            event_tx,
            is_playing: Arc::new(AtomicBool::new(false)),
            #[cfg(feature = "hw")]
            sw_fallback_active: Arc::new(AtomicBool::new(false)),
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
            #[cfg(not(feature = "hw"))]
            mock_decode_mode: std::sync::Mutex::new(decode_mode),
            #[cfg(not(feature = "hw"))]
            mock_event_tx,
        })
    }

    /// Load a URL and transition to the Playing state.
    ///
    /// Constructs a GStreamer pipeline with the configured video/audio
    /// sinks and routes traffic through the Tor SOCKS5h proxy if
    /// `socks_addr` is non-empty.
    ///
    /// The `url` is the direct media URL (CDN URL) to stream.
    ///
    /// The `source_url` is the original page URL that the user cast.
    /// It's used to set the Referer header — many CDNs (Voe, DoodStream)
    /// require the Referer to match the originating site's domain, not
    /// the CDN's domain. Sending the CDN's origin as Referer causes 403.
    ///
    /// The `isolation_username` is used as the SOCKS5 username for
    /// Tor's `IsolateSOCKSAuth` circuit isolation.
    #[cfg(feature = "hw")]
    pub async fn play(
        &self,
        url: &str,
        source_url: &str,
        socks_addr: &str,
        isolation_username: &str,
    ) -> Result<(), PlaybackError> {
        // Stop any existing pipeline while holding the lock.
        {
            let mut guard = self.gst_pipeline.lock().await;
            if let Some(ref mut existing) = *guard {
                let _ = existing.stop();
            }
            *guard = None;
        }

        // Reset SW fallback flag for the new playback attempt.
        self.sw_fallback_active.store(false, Ordering::Relaxed);

        tracing::info!(
            url = url,
            socks = socks_addr,
            hw_accel = self.config.hw_accel,
            "constructing playback pipeline"
        );

        let mut pipeline = GstPipeline::new(url, source_url, socks_addr, isolation_username, &self.config).await?;

        // Set up bus watch to forward GStreamer messages as events.
        let event_tx = self.event_tx.clone();
        let is_playing = self.is_playing.clone();
        let bus = pipeline.pipeline().bus().expect("pipeline should have a bus");

        // Flag to indicate that the pipeline should auto-transition from
        // Paused to Playing once preroll completes.  Set during initial
        // startup, cleared once Playing is reached or on error.
        let pending_auto_play = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let pending_auto_play_bus = pending_auto_play.clone();

        // Weak reference to the pipeline element for the bus watch to
        // trigger the Paused→Playing auto-transition.
        let pipeline_weak_bus = pipeline.pipeline().downgrade();

        let bus_watch = bus.add_watch(move |_bus, msg| {
            use gstreamer::MessageView;

            match msg.view() {
                MessageView::StateChanged(s) => {
                    let old_state = s.old();
                    let new_state = s.current();
                    let pending = s.pending();

                    // Log all state transitions at info level for
                    // diagnostics — the async Ready→Paused→Playing
                    // transition is the critical path and failures
                    // here explain why video never appears.
                    if new_state != old_state {
                        tracing::info!(
                            old = ?old_state,
                            new = ?new_state,
                            pending = ?pending,
                            source = %msg.src().map(|s| s.path_string()).unwrap_or_default(),
                            "pipeline state change"
                        );
                    }

                    // Only emit application-level events for the pipeline
                    // element itself, not for sub-elements.
                    let src_name = msg.src().map(|s| s.path_string()).unwrap_or_default();
                    let is_pipeline = src_name.starts_with("/GstPipeline:") && !src_name[1..].contains('/');

                    if new_state == State::Playing && is_pipeline {
                        // Pipeline reached Playing — clear the auto-play flag.
                        pending_auto_play_bus.store(false, Ordering::Relaxed);
                        let _ = event_tx.send(PlaybackEvent::Playing);
                        is_playing.store(true, Ordering::Relaxed);
                    } else if new_state == State::Paused && pending == State::VoidPending && is_pipeline {
                        // Pipeline reached Paused with no pending state change.
                        //
                        // During initial startup, we set the pipeline to Paused
                        // and wait for preroll.  When preroll completes (kmssink
                        // has received its first video frame), the pipeline reaches
                        // Paused with no pending state change.  At this point, we
                        // auto-transition to Playing — this starts the clock at
                        // the correct time so that buffer timestamps are not "late".
                        //
                        // For user-initiated pauses, pending_auto_play is false,
                        // so we don't auto-play.
                        if pending_auto_play_bus.load(Ordering::Relaxed) {
                            tracing::info!(
                                "pipeline prerolled (Paused with no pending) — \
                                 auto-transitioning to Playing"
                            );
                            if let Some(pipe) = pipeline_weak_bus.upgrade() {
                                let _ = pipe.set_state(State::Playing);
                            }
                            // Don't clear pending_auto_play here — it will be
                            // cleared when the pipeline reaches Playing.
                        } else {
                            tracing::info!(
                                "pipeline reached Paused — genuine pause (no auto-play)"
                            );
                            let _ = event_tx.send(PlaybackEvent::Paused);
                            is_playing.store(false, Ordering::Relaxed);
                        }
                    }
                },
                MessageView::Eos(_) => {
                    tracing::info!("end of stream reached");
                    let _ = event_tx.send(PlaybackEvent::EndOfStream);
                    is_playing.store(false, Ordering::Relaxed);
                },
                MessageView::Error(e) => {
                    let msg = e.error().to_string();
                    let debug_info = e.debug().map(|d| d.to_string());
                    let source_element = e.src().map(|s| s.path_string());
                    tracing::error!(
                        error = %msg,
                        debug = ?debug_info,
                        source = ?source_element,
                        "GStreamer error"
                    );
                    // Clear auto-play flag on error to prevent spurious transitions.
                    pending_auto_play_bus.store(false, Ordering::Relaxed);
                    let _ =
                        event_tx.send(PlaybackEvent::Error { message: msg, debug: debug_info });
                    is_playing.store(false, Ordering::Relaxed);
                },
                MessageView::Buffering(b) => {
                    let percent = b.percent() as u8;
                    tracing::info!(percent = percent, "buffering progress");
                    let _ = event_tx.send(PlaybackEvent::Buffering { percent });

                    // IMPORTANT: Do NOT pause/resume the pipeline from this
                    // handler.  Even though our queue2 has use-buffering=false,
                    // other elements (e.g. souphttpsrc) can still post
                    // BUFFERING messages.  Calling set_state(Paused) here
                    // cancels the pending Paused→Playing async transition,
                    // leaving the pipeline stuck at Paused with no video.
                    // This was the root cause of the "no video" bug: the
                    // buffering handler was killing the state transition.
                    //
                    // If we switch to use-buffering=true in the future,
                    // buffering control must be done by a dedicated
                    // component that tracks whether the pipeline is still
                    // prerolling and doesn't interrupt the initial
                    // Ready→Paused→Playing sequence.
                },
                MessageView::Latency(_l) => {
                    // Latency message — not forwarding in v1.
                },
                MessageView::Warning(w) => {
                    let warn_msg = w.error().to_string();
                    tracing::warn!(
                        warning = %warn_msg,
                        source = %msg.src().map(|s: &gstreamer::Object| s.path_string()).unwrap_or_default(),
                        "GStreamer warning"
                    );
                },
                _ => {},
            }

            gstreamer::glib::ControlFlow::Continue
        })
        .expect("failed to add bus watch");
        pipeline.set_bus_watch(bus_watch);

        // pipeline.preroll() is non-blocking — it calls set_state(Paused) which
        // starts the async state transition and returns immediately. GStreamer
        // begins fetching from souphttpsrc and demuxing via parsebin.
        //
        // The bus watch detects when preroll completes (pipeline reaches Paused
        // with pending_auto_play=true) and automatically transitions to Playing.
        // This ensures the pipeline clock starts at the correct time — after
        // buffers have reached the sinks — preventing the "late buffers" problem.
        let play_result = pipeline.preroll();

        // Store pipeline and handle result.
        let mut guard = self.gst_pipeline.lock().await;

        // Start playback — try HW decode first, fall back to SW on failure.
        match play_result {
            Ok(()) => {
                tracing::info!("pipeline prerolled successfully (hw_accel={})", self.config.hw_accel);
                *guard = Some(pipeline);

                // Spawn a diagnostic task that checks the pipeline state
                // after a few seconds.  The bus watch handles the automatic
                // Paused→Playing transition after preroll, so this task only
                // needs to check if things went wrong (e.g. stuck in a state,
                // pads not linked, errors).
                let pipeline_weak_diag = guard.as_ref().unwrap().pipeline().downgrade();
                let pending_auto_play_diag = pending_auto_play.clone();
                tokio::spawn(async move {
                    // Check at 10s — verify the pipeline reached Playing and
                    // that video/audio pads are linked.
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    if let Some(pipe) = pipeline_weak_diag.upgrade() {
                        let (result, current, pending) = pipe.state(gstreamer::ClockTime::from_mseconds(0));
                        tracing::info!(
                            result = ?result,
                            current = ?current,
                            pending = ?pending,
                            "pipeline state check after 10s"
                        );

                        // If still not Playing and auto-play hasn't triggered yet,
                        // force a transition.  This handles edge cases where the
                        // bus watch didn't fire (e.g. GStreamer bug, timing issue).
                        if current != State::Playing && pending_auto_play_diag.load(Ordering::Relaxed) {
                            tracing::warn!(
                                current = ?current,
                                "pipeline NOT playing after 10s — forcing transition to Playing"
                            );
                            let _ = pipe.set_state(State::Playing);
                        }

                        // Check whether parsebin's video/audio pads are linked.
                        // We use parsebin.iterate_src_pads() as the source of truth —
                        // walking bins by factory name is fragile and previously produced
                        // false "NO video pad linked" errors that contradicted the
                        // pad-added handler's own logs.
                        if let Ok(bin) = pipe.dynamic_cast::<gstreamer::Bin>() {
                            let mut video_linked = false;
                            let mut audio_linked = false;

                            // Check parsebin's source pads directly — this is the
                            // authoritative way to determine linkage.
                            if let Some(parsebin_elem) = bin.by_name("parsebin0") {
                                let mut pad_iter = parsebin_elem.iterate_src_pads();
                                let mut pad_count = 0;
                                loop {
                                    match pad_iter.next() {
                                        Ok(Some(pad)) => {
                                            let caps = pad.current_caps()
                                                .map(|c: gstreamer::Caps| c.to_string())
                                                .unwrap_or_default();
                                            let is_linked = pad.is_linked();

                                            // Determine media type from caps
                                            let media_type = pad.current_caps()
                                                .and_then(|c| c.structure(0).map(|s| s.name().to_string()))
                                                .unwrap_or_default();
                                            let is_video = media_type.starts_with("video/");
                                            let is_audio = media_type.starts_with("audio/");

                                            tracing::info!(
                                                pad = %pad.name(),
                                                caps = %caps,
                                                is_linked = is_linked,
                                                media_type = %media_type,
                                                "parsebin source pad"
                                            );

                                            if is_video && is_linked {
                                                video_linked = true;
                                            }
                                            if is_audio && is_linked {
                                                audio_linked = true;
                                            }
                                            pad_count += 1;
                                        },
                                        Ok(None) | Err(_) => break,
                                    }
                                }
                                if pad_count == 0 {
                                    tracing::error!(
                                        "parsebin has ZERO source pads after 8s — it hasn't demuxed any streams yet. \
                                         This typically means souphttpsrc is not providing data (network error, CDN 403, etc.)."
                                    );
                                }
                            }

                            if video_linked {
                                tracing::info!("video pad linked ✓ — parsebin video pad is connected");
                            } else {
                                tracing::error!(
                                    "NO video pad linked after 8s — parsebin did not connect video to the video bin. \
                                     Possible causes: (1) the stream has no video track, (2) parsebin's pad-added signal \
                                     fired before the callback was connected (race condition), (3) the video bin's sink \
                                     pad ghost pad is misconfigured."
                                );
                            }
                            if audio_linked {
                                tracing::info!("audio pad linked ✓ — parsebin audio pad is connected");
                            } else {
                                tracing::warn!("no audio pad linked after 8s — audio track may be missing");
                            }
                        }
                    }

                    // Second check at 20s — detailed per-element state dump
                    tokio::time::sleep(std::time::Duration::from_secs(12)).await;
                    if let Some(pipe) = pipeline_weak_diag.upgrade() {
                        let (result, current, pending) = pipe.state(gstreamer::ClockTime::from_mseconds(0));
                        tracing::info!(
                            result = ?result,
                            current = ?current,
                            pending = ?pending,
                            "pipeline state check after 20s"
                        );

                        // Walk all elements and log their states to find
                        // which one is stuck and blocking preroll.
                        if current != State::Playing {
                            tracing::warn!("pipeline NOT playing after 20s — dumping per-element state:");
                            if let Ok(bin) = pipe.dynamic_cast::<gstreamer::Bin>() {
                                let mut elem_iter = bin.iterate_elements();
                                loop {
                                    match elem_iter.next() {
                                        Ok(Some(e)) => {
                                            let (res, st, pend) = e.state(gstreamer::ClockTime::from_mseconds(0));
                                            let factory_name = e.factory()
                                                .map(|f: gstreamer::ElementFactory| f.name().to_string())
                                                .unwrap_or_default();
                                            tracing::warn!(
                                                element = %e.name(),
                                                type_ = %factory_name,
                                                state = ?st,
                                                pending = ?pend,
                                                result = ?res,
                                                "element state"
                                            );
                                            // Also check elements inside bins (e.g. video bin)
                                            if let Ok(sub_bin) = e.dynamic_cast::<gstreamer::Bin>() {
                                                let mut sub_iter = sub_bin.iterate_elements();
                                                loop {
                                                    match sub_iter.next() {
                                                        Ok(Some(se)) => {
                                                            let (sr, sst, sp) = se.state(gstreamer::ClockTime::from_mseconds(0));
                                                            let sub_factory = se.factory()
                                                                .map(|f: gstreamer::ElementFactory| f.name().to_string())
                                                                .unwrap_or_default();
                                                            tracing::warn!(
                                                                element = %se.name(),
                                                                type_ = %sub_factory,
                                                                state = ?sst,
                                                                pending = ?sp,
                                                                result = ?sr,
                                                                "  sub-element state"
                                                            );
                                                        },
                                                        Ok(None) | Err(_) => break,
                                                    }
                                                }
                                            }
                                        },
                                        Ok(None) | Err(_) => break,
                                    }
                                }
                            }

                            // Point to GStreamer debug log for details
                            tracing::warn!(
                                "check GST_DEBUG_FILE, usually /run/picast/gst-debug.log under systemd, for detailed kmssink/v4l2h264dec debug output. \
                                 For caps negotiation issues, set GST_DEBUG=kmssink:6,v4l2h264dec:6,h264parse:5,GST_CAPS:6,GST_PADS:5"
                            );
                        }
                    }
                });

                Ok(())
            },
            Err(PlaybackError::Gstreamer(ref msg)) if self.config.hw_accel && is_negotiation_error(msg) => {
                // V4L2 caps negotiation failed — fall back to software decode.
                tracing::warn!(
                    error = %msg,
                    "HW decode negotiation failed — falling back to software decode"
                );
                // Stop the failed pipeline before trying SW fallback.
                let mut failed_pipeline = pipeline;
                let _ = failed_pipeline.stop();
                drop(failed_pipeline);
                drop(guard); // release lock before fallback call
                self.play_software_fallback(url, socks_addr, isolation_username).await
            },
            Err(e) => {
                tracing::error!(error = %e, "pipeline play() failed");
                // Stop the failed pipeline.
                let mut failed_pipeline = pipeline;
                let _ = failed_pipeline.stop();
                drop(failed_pipeline);
                // Try SW fallback if HW accel was enabled.
                if self.config.hw_accel {
                    tracing::warn!("attempting software decode fallback after play failure");
                    drop(guard);
                    match self.play_software_fallback(url, socks_addr, isolation_username).await {
                        Ok(()) => Ok(()),
                        Err(fallback_err) => {
                            tracing::error!(error = %fallback_err, "software decode fallback also failed");
                            Err(e) // return original error
                        },
                    }
                } else {
                    Err(e)
                }
            },
        }
    }

    /// Attempt software-decode fallback after HW decode failure.
    ///
    /// Constructs a new pipeline with `hw_accel = false` and starts
    /// playback. This uses `avdec_h264` instead of `v4l2h264dec`,
    /// which avoids V4L2 caps negotiation issues at the cost of
    /// higher CPU usage.
    #[cfg(feature = "hw")]
    async fn play_software_fallback(
        &self,
        url: &str,
        socks_addr: &str,
        isolation_username: &str,
    ) -> Result<(), PlaybackError> {
        // Stop any existing pipeline while holding the lock.
        {
            let mut guard = self.gst_pipeline.lock().await;
            if let Some(ref mut existing) = *guard {
                let _ = existing.stop();
            }
            *guard = None;
        }

        let mut sw_config = self.config.clone();
        sw_config.hw_accel = false;

        tracing::info!(
            url = url,
            "constructing SOFTWARE DECODE fallback pipeline (avdec_h264 → videoconvert → kmssink)"
        );

        let mut pipeline = GstPipeline::new(url, socks_addr, isolation_username, &sw_config).await?;

        // Set up bus watch for the fallback pipeline.
        let event_tx = self.event_tx.clone();
        let is_playing = self.is_playing.clone();
        let bus = pipeline.pipeline().bus().expect("pipeline should have a bus");

        // Auto-play flag for the SW fallback pipeline (same as primary).
        let pending_auto_play_sw = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let pending_auto_play_sw_bus = pending_auto_play_sw.clone();
        let pipeline_weak_sw = pipeline.pipeline().downgrade();

        let bus_watch = bus.add_watch(move |_bus, msg| {
            use gstreamer::MessageView;

            match msg.view() {
                MessageView::StateChanged(s) => {
                    let old_state = s.old();
                    let new_state = s.current();
                    let pending = s.pending();

                    if new_state != old_state {
                        tracing::info!(
                            old = ?old_state,
                            new = ?new_state,
                            pending = ?pending,
                            source = %msg.src().map(|s| s.path_string()).unwrap_or_default(),
                            "pipeline state change (SW decode)"
                        );
                    }

                    let src_name = msg.src().map(|s| s.path_string()).unwrap_or_default();
                    let is_pipeline = src_name.starts_with("/GstPipeline:") && !src_name[1..].contains('/');

                    if new_state == State::Playing && is_pipeline {
                        pending_auto_play_sw_bus.store(false, Ordering::Relaxed);
                        let _ = event_tx.send(PlaybackEvent::Playing);
                        is_playing.store(true, Ordering::Relaxed);
                    } else if new_state == State::Paused && pending == State::VoidPending && is_pipeline {
                        if pending_auto_play_sw_bus.load(Ordering::Relaxed) {
                            tracing::info!(
                                "SW fallback pipeline prerolled — auto-transitioning to Playing"
                            );
                            if let Some(pipe) = pipeline_weak_sw.upgrade() {
                                let _ = pipe.set_state(State::Playing);
                            }
                        } else {
                            tracing::info!(
                                "pipeline reached Paused (SW) — genuine pause"
                            );
                            let _ = event_tx.send(PlaybackEvent::Paused);
                            is_playing.store(false, Ordering::Relaxed);
                        }
                    }
                },
                MessageView::Eos(_) => {
                    tracing::info!("end of stream reached (SW decode)");
                    let _ = event_tx.send(PlaybackEvent::EndOfStream);
                    is_playing.store(false, Ordering::Relaxed);
                },
                MessageView::Error(e) => {
                    let err_msg = e.error().to_string();
                    let debug_info = e.debug().map(|d| d.to_string());
                    tracing::error!(
                        error = %err_msg,
                        debug = ?debug_info,
                        source = %msg.src().map(|s: &gstreamer::Object| s.path_string()).unwrap_or_default(),
                        "GStreamer error (SW decode fallback)"
                    );
                    pending_auto_play_sw_bus.store(false, Ordering::Relaxed);
                    let _ = event_tx.send(PlaybackEvent::Error { message: err_msg, debug: debug_info });
                    is_playing.store(false, Ordering::Relaxed);
                },
                MessageView::Warning(w) => {
                    let warn_msg = w.error().to_string();
                    tracing::warn!(
                        warning = %warn_msg,
                        source = %msg.src().map(|s: &gstreamer::Object| s.path_string()).unwrap_or_default(),
                        "GStreamer warning (SW decode)"
                    );
                },
                MessageView::Buffering(b) => {
                    let percent = b.percent() as u8;
                    tracing::info!(percent = percent, "buffering progress (SW decode)");
                    let _ = event_tx.send(PlaybackEvent::Buffering { percent });

                    // Do NOT pause/resume the pipeline — see the comment
                    // in the primary bus watch above for the full explanation.
                },
                _ => {},
            }

            gstreamer::glib::ControlFlow::Continue
        })
        .expect("failed to add bus watch for SW fallback");
        pipeline.set_bus_watch(bus_watch);

        // pipeline.preroll() starts the SW fallback pipeline (Paused → auto-Playing).
        pipeline.preroll()?;
        self.sw_fallback_active.store(true, Ordering::Relaxed);
        tracing::info!("software decode fallback pipeline started successfully");

        let mut guard = self.gst_pipeline.lock().await;
        *guard = Some(pipeline);
        Ok(())
    }

    /// Load a URL and transition to the Playing state (mock mode).
    #[cfg(not(feature = "hw"))]
    pub async fn play(
        &self,
        url: &str,
        _source_url: &str,
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

        // Emit mock Playing event.
        let _ = self.mock_event_tx.send(PlaybackState::Playing);

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
        let _ = self.mock_event_tx.send(PlaybackState::Paused);
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
        let _ = self.mock_event_tx.send(PlaybackState::Playing);
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
            let _ = self.event_tx.send(PlaybackEvent::Stopped);
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

        let _ = self.mock_event_tx.send(PlaybackState::Stopped);

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
    /// Each call creates a new receiver via `broadcast::Sender::subscribe()`.
    /// All subscribers receive a copy of each event.
    #[cfg(feature = "hw")]
    pub fn events(&self) -> tokio::sync::broadcast::Receiver<PlaybackEvent> {
        self.event_tx.subscribe()
    }

    /// Return a receiver for playback state changes (mock mode).
    ///
    /// In mock mode, events are simple `PlaybackState` changes rather
    /// than full `PlaybackEvent` messages. This lets the session layer
    /// detect transitions without requiring GStreamer.
    #[cfg(not(feature = "hw"))]
    pub fn events(&self) -> tokio::sync::broadcast::Receiver<PlaybackState> {
        self.mock_event_tx.subscribe()
    }

    /// Return a reference to the pipeline configuration.
    pub fn config(&self) -> &PipelineConfig {
        &self.config
    }

    /// Whether the engine is currently playing.
    pub fn is_playing(&self) -> bool {
        self.is_playing.load(Ordering::Relaxed)
    }

    /// Get the current ALSA audio device string.
    ///
    /// Returns the device string (e.g. `"plughw:1,0"`) that will be used
    /// for the next pipeline's `alsasink device=` property.
    pub fn audio_device(&self) -> String {
        self.config.audio_device.clone()
    }

    /// Set the ALSA audio device for the next playback pipeline.
    ///
    /// This updates the runtime config so that the *next* `play()` call
    /// creates an `alsasink` with `device=<new_device>`.  It does NOT
    /// affect a currently-running pipeline — the change takes effect on
    /// the next playback session.
    ///
    /// Pass an empty string to use the ALSA default device.
    pub fn set_audio_device(&self, device: String) {
        tracing::info!(old = %self.config.audio_device, new = %device, "audio device updated");
        // PipelineConfig is behind &self (not &mut self) because it's
        // shared via Arc<Mutex<…>> for the pipeline, but the config itself
        // is only read during play() — we need interior mutability here.
        // Since PlaybackEngine is always used through Arc, we use
        // unsafe to mutate the config field.  This is safe because:
        // 1. config is only read in play(), which holds the gst_pipeline lock
        // 2. set_audio_device and play() are never called concurrently
        //    (both go through the SessionManager which serialises access)
        // 3. audio_device is a String — no Drop impl that could cause issues
        let config_ptr = &self.config as *const PipelineConfig as *mut PipelineConfig;
        unsafe {
            (*config_ptr).audio_device = device;
        }
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

    /// Return the current decode mode.
    ///
    /// In hardware mode, returns `Hardware` if V4L2 decode is active,
    /// `Software` if the engine fell back to avdec_h264 (tracked by
    /// `sw_fallback_active`), or `Software` if hw_accel was disabled
    /// in the config.
    /// In mock mode, returns the configured decode mode.
    #[cfg(feature = "hw")]
    pub fn decode_mode(&self) -> DecodeMode {
        if self.sw_fallback_active.load(Ordering::Relaxed) {
            DecodeMode::Software
        } else if self.config.hw_accel {
            DecodeMode::Hardware
        } else {
            DecodeMode::Software
        }
    }

    /// Return the current decode mode (mock mode).
    #[cfg(not(feature = "hw"))]
    pub fn decode_mode(&self) -> DecodeMode {
        let guard = self.mock_decode_mode.lock().unwrap();
        *guard
    }

    /// Simulate a software decode fallback (for testing).
    ///
    /// In mock mode, switches the decode mode from Hardware to Software.
    /// In hardware mode, this is a no-op (use `PlaybackEngine::play()`
    /// with `hw_accel = false` to force software decode).
    #[cfg(not(feature = "hw"))]
    pub fn mock_fallback_to_software(&self) {
        let mut guard = self.mock_decode_mode.lock().unwrap();
        tracing::info!(from = %guard, to = "software", "mock decode fallback");
        *guard = DecodeMode::Software;
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
            .play("https://example.com/video.mp4", "https://example.com/video.mp4", "", "")
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
        engine.play("https://example.com/video.mp4", "https://example.com/video.mp4", "", "").await.unwrap();

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
        engine.play("https://example.com/video.mp4", "https://example.com/video.mp4", "", "").await.unwrap();

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
        engine.play("https://example.com/video.mp4", "https://example.com/video.mp4", "", "").await.unwrap();

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
        engine.play("https://example.com/video.mp4", "https://example.com/video.mp4", "", "").await.unwrap();
        engine.seek(120_000).await.unwrap();
        let pos = engine.position_ms().await.unwrap();
        assert_eq!(pos, 120_000);

        // Play again — position should reset to 0
        engine.play("https://example.com/other.mp4", "https://example.com/other.mp4", "", "").await.unwrap();
        let pos = engine.position_ms().await.unwrap();
        assert_eq!(pos, 0, "position should reset to 0 on play");
    }

    #[tokio::test]
    async fn mock_resume_fails_when_not_paused() {
        let engine = PlaybackEngine::new(PipelineConfig::default()).unwrap();

        // Load and play
        engine.play("https://example.com/video.mp4", "https://example.com/video.mp4", "", "").await.unwrap();

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

    #[test]
    fn decode_mode_default_is_hardware() {
        let engine = PlaybackEngine::new(PipelineConfig::default()).unwrap();
        assert_eq!(engine.decode_mode(), DecodeMode::Hardware);
    }

    #[test]
    fn decode_mode_software_when_hw_accel_disabled() {
        let mut config = PipelineConfig::default();
        config.hw_accel = false;
        let engine = PlaybackEngine::new(config).unwrap();
        assert_eq!(engine.decode_mode(), DecodeMode::Software);
    }

    #[test]
    fn decode_mode_fallback_from_hardware_to_software() {
        let engine = PlaybackEngine::new(PipelineConfig::default()).unwrap();
        assert_eq!(engine.decode_mode(), DecodeMode::Hardware);

        // Simulate a fallback
        engine.mock_fallback_to_software();
        assert_eq!(engine.decode_mode(), DecodeMode::Software);
    }

    #[test]
    fn decode_mode_display() {
        assert_eq!(DecodeMode::Hardware.to_string(), "hardware");
        assert_eq!(DecodeMode::Software.to_string(), "software");
    }

    #[test]
    fn decode_mode_serialization_roundtrip() {
        let modes = vec![DecodeMode::Hardware, DecodeMode::Software];
        for mode in modes {
            let json = serde_json::to_string(&mode).expect("serialize");
            let deserialized: DecodeMode = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(deserialized, mode);
        }
    }
}
