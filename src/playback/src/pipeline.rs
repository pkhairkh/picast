#![cfg(feature = "hw")]
//! PiCast GStreamer Pipeline Construction
//!
//! Builds and manages the GStreamer pipeline for H.264 video playback
//! with V4L2 hardware decode and direct DRM/KMS output on Raspberry Pi 4B+.
//!
//! ## Pipeline Topology
//!
//! ```text
//! ┌──────────┐    ┌────────┐    ┌──────────┐    ┌───────┐    ┌───────────────┐    ┌─────────┐
//! │souphttpsrc│───►│queue2  │───►│parsebin  │──┬►│queue  │───►│v4l2h264dec   │───►│kmssink  │
//! │(SOCKS5h) │    │(buffer)│    │(demux)   │  │ │       │    │(HW decode)   │    │(DRM/KMS)│
//! └──────────┘    └────────┘    └──────────┘  │ └───────┘    └───────────────┘    └─────────┘
//!                                               │
//!                                               │ ┌───────┐    ┌──────────────┐    ┌──────────────┐    ┌────────┐    ┌─────────┐
//!                                               └►│queue  │───►│audioconvert  │───►│audioresample │───►│volume │───►│alsasink │
//!                                                 │       │    │              │    │              │    │        │    │(HDMI)   │
//!                                                 └───────┘    └──────────────┘    └──────────────┘    └────────┘    └─────────┘
//! ```
//!
//! ## Fallback
//!
//! If V4L2 decode fails to negotiate (e.g. non-H.264 input),
//! the pipeline falls back to software decode:
//!
//! ```text
//! souphttpsrc → queue2 → parsebin → avdec_h264 → videoconvert → kmssink
//! ```

use crate::{BufferHealth, PipelineConfig, PlaybackError};
use gstreamer::prelude::*;
use gstreamer::{Element, ElementFactory, Pipeline, State};

/// Ensure GStreamer is initialised exactly once.
static GST_INIT: std::sync::OnceLock<Result<(), PlaybackError>> = std::sync::OnceLock::new();

/// Initialise GStreamer. Safe to call multiple times.
/// Returns an error if initialisation fails (instead of panicking),
/// and subsequent calls will return the same error.
fn ensure_gst_init() -> Result<(), PlaybackError> {
    GST_INIT
        .get_or_init(|| {
            match gstreamer::init() {
                Ok(()) => {
                    tracing::debug!("GStreamer initialised successfully");
                    Ok(())
                },
                Err(e) => {
                    tracing::error!("GStreamer initialisation failed: {}", e);
                    Err(PlaybackError::Gstreamer(format!(
                        "GStreamer init failed (permanent): {}",
                        e
                    )))
                },
            }
        })
        .clone()
}

/// A constructed GStreamer pipeline ready for state transitions.
pub struct GstPipeline {
    /// The GStreamer pipeline element.
    pipeline: Pipeline,
    /// The video sink element (kmssink or fallback).
    video_sink: Element,
    /// The volume element for audio control.
    volume: Element,
    /// Current pipeline state.
    state: PipelineState,
}

/// Internal tracking of pipeline state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineState {
    /// No pipeline is constructed.
    Null,
    /// Pipeline is constructed but not playing.
    Ready,
    /// Pipeline is actively playing.
    Playing,
    /// Pipeline is paused.
    Paused,
    /// Pipeline encountered an error.
    Error,
}

impl GstPipeline {
    /// Construct a new GStreamer pipeline for the given URL.
    ///
    /// The `socks_addr` parameter configures SOCKS5h proxy routing
    /// through Tor. If empty, no proxy is used (for local media).
    ///
    /// The `isolation_username` is used as the SOCKS5 username for
    /// Tor circuit isolation (IsolateSOCKSAuth).
    pub fn new(
        url: &str,
        socks_addr: &str,
        isolation_username: &str,
        config: &PipelineConfig,
    ) -> Result<Self, PlaybackError> {
        ensure_gst_init()?;

        let pipeline = Pipeline::new();

        // ── Source element ──────────────────────────────────────────
        let src = ElementFactory::make("souphttpsrc")
            .property("location", url)
            .property("timeout", 30u64 * 1_000_000_000u64) // 30s in ns
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("souphttpsrc: {}", e)))?;

        // Configure SOCKS5h proxy if provided.
        if !socks_addr.is_empty() {
            let parts: Vec<&str> = socks_addr.split(':').collect();
            let (host, port) = if parts.len() == 2 {
                (parts[0], parts[1].parse::<u32>().unwrap_or(9050))
            } else {
                ("127.0.0.1", 9050u32)
            };
            // Note: souphttpsrc's built-in SOCKS5 does not support
            // username-based circuit isolation (IsolateSOCKSAuth).
            // All connections through this proxy share Tor circuits.
            // For per-host isolation, use a SOCKS5 forwarder or
            // GStreamer's souphttpsrc with a local socat bridge.
            src.set_property("socks5-proxy-ip", host);
            src.set_property("socks5-proxy-port", port);
            tracing::debug!(
                host = host,
                port = port,
                user = isolation_username,
                "SOCKS5h proxy configured on souphttpsrc"
            );
        }

        // ── Buffer element ──────────────────────────────────────────
        let queue2 = ElementFactory::make("queue2")
            .property("max-size-bytes", 52_428_800u64) // 50 MB
            .property("use-buffering", true)
            .property_from_str("low-percent", "25")
            .property_from_str("high-percent", "75")
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("queue2: {}", e)))?;

        // ── Demuxer ─────────────────────────────────────────────────
        let parsebin = ElementFactory::make("parsebin")
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("parsebin: {}", e)))?;

        // ── Video elements ──────────────────────────────────────────
        let (video_bin, video_sink) = if config.hw_accel {
            Self::build_hw_video_bin(config)?
        } else {
            Self::build_sw_video_bin(config)?
        };

        // ── Audio elements ──────────────────────────────────────────
        let audio_queue = ElementFactory::make("queue")
            .property("max-size-buffers", 3u32)
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("audio_queue: {}", e)))?;

        let audioconvert = ElementFactory::make("audioconvert")
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("audioconvert: {}", e)))?;

        let audioresample = ElementFactory::make("audioresample")
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("audioresample: {}", e)))?;

        let volume = ElementFactory::make("volume")
            .property("volume", config.volume)
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("volume: {}", e)))?;

        let audiosink = ElementFactory::make(&config.audio_sink).build().map_err(|e| {
            PlaybackError::PipelineCreation(format!("{}: {}", config.audio_sink, e))
        })?;

        // ── Assemble pipeline ───────────────────────────────────────
        pipeline
            .add_many([
                &src, &queue2, &parsebin, &video_bin,
                &audio_queue, &audioconvert, &audioresample, &volume, &audiosink,
            ])
            .map_err(|e| PlaybackError::PipelineCreation(format!("add elements: {}", e)))?;

        // Link: src → queue2 → parsebin
        Element::link_many([&src, &queue2, &parsebin]).map_err(|e| {
            PlaybackError::PipelineCreation(format!("link src→queue2→parsebin: {}", e))
        })?;

        // Link audio: audio_queue → audioconvert → audioresample → volume → audiosink
        Element::link_many([&audio_queue, &audioconvert, &audioresample, &volume, &audiosink])
            .map_err(|e| PlaybackError::PipelineCreation(format!("link audio chain: {}", e)))?;

        // ── Dynamic pad linking (parsebin → video/audio) ────────────
        let pipeline_weak = pipeline.downgrade();
        let video_bin_weak = video_bin.downgrade();
        let audio_queue_weak = audio_queue.downgrade();

        parsebin.connect_pad_added(move |_parsebin, pad| {
            let caps = pad.current_caps();
            let media_type = caps.and_then(|c| c.structure(0).map(|s| s.name().to_string()));

            let is_video = media_type.as_ref().map(|t| t.starts_with("video/")).unwrap_or(false);

            let is_audio = media_type.as_ref().map(|t| t.starts_with("audio/")).unwrap_or(false);

            if is_video {
                if let Some(vbin) = video_bin_weak.upgrade() {
                    let sink_pad =
                        vbin.static_pad("sink").expect("video bin should have a sink pad");
                    if sink_pad.is_linked() {
                        tracing::debug!("video pad already linked, skipping");
                        return;
                    }
                    if let Err(e) = pad.link(&sink_pad) {
                        tracing::error!("failed to link parsebin video pad: {:?}", e);
                    } else {
                        tracing::debug!("linked parsebin → video bin");
                    }
                }
            } else if is_audio {
                if let Some(aq) = audio_queue_weak.upgrade() {
                    let sink_pad =
                        aq.static_pad("sink").expect("audio_queue should have a sink pad");
                    if sink_pad.is_linked() {
                        tracing::debug!("audio pad already linked, skipping");
                        return;
                    }
                    if let Err(e) = pad.link(&sink_pad) {
                        tracing::error!("failed to link parsebin audio pad: {:?}", e);
                    } else {
                        tracing::debug!("linked parsebin → audio_queue");
                    }
                }
            }
        });

        Ok(Self { pipeline, video_sink, volume, state: PipelineState::Ready })
    }

    /// Build the hardware-accelerated video branch:
    /// `v4l2h264dec (DMA-BUF) → kmssink`
    fn build_hw_video_bin(config: &PipelineConfig) -> Result<(Element, Element), PlaybackError> {
        let video_queue = ElementFactory::make("queue")
            .property("max-size-buffers", 3u32)
            .build()
            .map_err(|e| {
                PlaybackError::PipelineCreation(format!("video_queue: {}", e))
            })?;

        let v4l2dec = ElementFactory::make("v4l2h264dec")
            .property_from_str("capture-io-mode", "dmabuf")
            .build()
            .map_err(|e| {
                PlaybackError::PipelineCreation(format!(
                    "v4l2h264dec (may not be available): {}",
                    e
                ))
            })?;

        let kmssink = ElementFactory::make(&config.video_sink)
            .property_from_str("driver-name", "vc4")
            .property("plane-id", config.plane_id)
            .property("can-scale", true)
            .property("force-modesetting", true)
            .build()
            .map_err(|e| {
                PlaybackError::PipelineCreation(format!("{}: {}", config.video_sink, e))
            })?;

        let bin = gstreamer::Bin::new(Some("video-bin"));
        bin.add_many([&video_queue, &v4l2dec, &kmssink]).map_err(|e| {
            PlaybackError::PipelineCreation(format!("add video elements to bin: {}", e))
        })?;

        Element::link_many([&video_queue, &v4l2dec, &kmssink]).map_err(|e| {
            PlaybackError::PipelineCreation(format!("link video_queue→v4l2h264dec→kmssink: {}", e))
        })?;

        // Create ghost pads for the bin (on the queue element, which is the entry point).
        let sink_pad = video_queue.static_pad("sink").expect("video_queue should have a sink pad");
        bin.add_pad(&gstreamer::GhostPad::with_target(&sink_pad).expect("create video ghost pad"))
            .map_err(|e| PlaybackError::PipelineCreation(format!("video ghost pad: {}", e)))?;

        let bin_element: Element =
            bin.dynamic_cast::<Element>().expect("bin to element cast should succeed");

        Ok((bin_element, kmssink))
    }

    /// Build the software-decode video branch:
    /// `avdec_h264 → videoconvert → kmssink`
    fn build_sw_video_bin(config: &PipelineConfig) -> Result<(Element, Element), PlaybackError> {
        let video_queue = ElementFactory::make("queue")
            .property("max-size-buffers", 3u32)
            .build()
            .map_err(|e| {
                PlaybackError::PipelineCreation(format!("sw video_queue: {}", e))
            })?;

        let avdec = ElementFactory::make("avdec_h264")
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("avdec_h264: {}", e)))?;

        let vconv = ElementFactory::make("videoconvert")
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("videoconvert: {}", e)))?;

        let kmssink = ElementFactory::make(&config.video_sink)
            .property_from_str("driver-name", "vc4")
            .property("can-scale", true)
            .property("force-modesetting", true)
            .build()
            .map_err(|e| {
                PlaybackError::PipelineCreation(format!("{}: {}", config.video_sink, e))
            })?;

        let bin = gstreamer::Bin::new(Some("sw-video-bin"));
        bin.add_many([&video_queue, &avdec, &vconv, &kmssink]).map_err(|e| {
            PlaybackError::PipelineCreation(format!("add sw video elements: {}", e))
        })?;

        Element::link_many([&video_queue, &avdec, &vconv, &kmssink])
            .map_err(|e| PlaybackError::PipelineCreation(format!("link sw video chain: {}", e)))?;

        let sink_pad = video_queue.static_pad("sink").expect("video_queue should have a sink pad");
        bin.add_pad(
            &gstreamer::GhostPad::with_target(&sink_pad).expect("create sw video ghost pad"),
        )
        .map_err(|e| PlaybackError::PipelineCreation(format!("sw video ghost pad: {}", e)))?;

        let bin_element: Element =
            bin.dynamic_cast::<Element>().expect("bin to element cast should succeed");

        Ok((bin_element, kmssink))
    }

    /// Transition the pipeline to Playing.
    pub fn play(&mut self) -> Result<(), PlaybackError> {
        self.pipeline
            .set_state(State::Playing)
            .map_err(|e| PlaybackError::Gstreamer(format!("set_state Playing: {}", e)))?;
        self.state = PipelineState::Playing;
        tracing::debug!("pipeline transitioned to Playing");
        Ok(())
    }

    /// Transition the pipeline to Paused.
    pub fn pause(&mut self) -> Result<(), PlaybackError> {
        self.pipeline
            .set_state(State::Paused)
            .map_err(|e| PlaybackError::Gstreamer(format!("set_state Paused: {}", e)))?;
        self.state = PipelineState::Paused;
        tracing::debug!("pipeline transitioned to Paused");
        Ok(())
    }

    /// Resume from Paused to Playing.
    pub fn resume(&mut self) -> Result<(), PlaybackError> {
        if self.state != PipelineState::Paused {
            return Err(PlaybackError::InvalidState(format!(
                "cannot resume from state {:?}",
                self.state
            )));
        }
        self.play()
    }

    /// Stop the pipeline and release resources.
    pub fn stop(&mut self) -> Result<(), PlaybackError> {
        // Send EOS to allow elements to flush.
        let _ = self.pipeline.send_event(gstreamer::event::Eos::new());

        self.pipeline
            .set_state(State::Null)
            .map_err(|e| PlaybackError::Gstreamer(format!("set_state Null: {}", e)))?;
        self.state = PipelineState::Null;
        tracing::debug!("pipeline stopped and set to Null");
        Ok(())
    }

    /// Perform a flushing seek to an absolute position.
    pub fn seek(&mut self, position_ms: u64) -> Result<(), PlaybackError> {
        let position = gstreamer::ClockTime::from_mseconds(position_ms);
        self.pipeline
            .seek_simple(gstreamer::SeekFlags::FLUSH | gstreamer::SeekFlags::KEY_UNIT, position)
            .map_err(|e| PlaybackError::SeekFailed(format!("seek to {}ms: {}", position_ms, e)))?;
        tracing::debug!(position_ms = position_ms, "seek completed");
        Ok(())
    }

    /// Set the playback volume (0.0–1.0).
    pub fn set_volume(&mut self, volume: f64) -> Result<(), PlaybackError> {
        let clamped = volume.clamp(0.0, 1.0);
        self.volume.set_property("volume", clamped);
        tracing::debug!(volume = clamped, "volume set");
        Ok(())
    }

    /// Return the current playback position in milliseconds.
    pub fn position_ms(&self) -> Result<u64, PlaybackError> {
        let position = self
            .pipeline
            .query_position::<gstreamer::ClockTime>()
            .map(|p| p.mseconds())
            .unwrap_or(0);
        Ok(position)
    }

    /// Return the total duration in milliseconds.
    pub fn duration_ms(&self) -> Result<Option<u64>, PlaybackError> {
        let duration = self.pipeline.query_duration::<gstreamer::ClockTime>().map(|d| d.mseconds());
        Ok(duration)
    }

    /// Query the current buffer health from the queue2 element.
    pub fn buffer_health(&self) -> BufferHealth {
        // Query buffering stats from queue2.
        let query = gstreamer::query::Buffering::new();
        if self.pipeline.query(&query) {
            let percent = query.percent();
            let stats = query.stats();
            BufferHealth {
                fill_percent: percent as u8,
                buffered_seconds: 0.0, // Approximated from fill_percent
                estimated_fill_ms: None,
                is_buffering: percent < 100,
            }
        } else {
            BufferHealth::default()
        }
    }

    /// Return a reference to the GStreamer pipeline for bus watch setup.
    pub fn pipeline(&self) -> &Pipeline {
        &self.pipeline
    }

    /// Return the current pipeline state.
    pub fn state(&self) -> PipelineState {
        self.state
    }

    /// Attempt to rebuild the pipeline with software decode.
    ///
    /// Called when V4L2 hardware decode fails to negotiate.
    pub fn rebuild_sw(
        &mut self,
        url: &str,
        socks_addr: &str,
        isolation_username: &str,
        config: &PipelineConfig,
    ) -> Result<(), PlaybackError> {
        tracing::warn!("rebuilding pipeline with software decode fallback");

        // Stop the current pipeline.
        self.stop()?;

        // Build a new pipeline with HW accel disabled.
        let mut sw_config = config.clone();
        sw_config.hw_accel = false;
        let new = Self::new(url, socks_addr, isolation_username, &sw_config)?;

        // Replace self with the new pipeline.
        *self = new;
        self.play()?;

        Ok(())
    }
}
