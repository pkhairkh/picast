#![cfg(feature = "hw")]
//! boGDan GStreamer Pipeline Construction
//!
//! Builds and manages the GStreamer pipeline for H.264/HEVC video playback
//! with V4L2 hardware decode and direct DRM/KMS output on Raspberry Pi 4B+.
//!
//! ## Pipeline Topology (H.264)
//!
//! ```text
//! ┌──────────┐    ┌────────┐    ┌──────────┐    ┌───────┐    ┌────────────────────┐    ┌────────────┐    ┌──────────────┐
//! │souphttpsrc│───►│queue2  │───►│parsebin  │──┬►│queue  │───►│v4l2h264dec(dmabuf) │───►│v4l2convert │───►│kmssink       │
//! │(SOCKS5h) │    │(buffer)│    │(demux)   │  │ │       │    │(zero-copy HW dec)  │    │(ISP:       │    │(DRM/KMS,     │
//! └──────────┘    └────────┘    └──────────┘  │ └───────┘    └────────────────────┘    │SAND→NV12)  │    │ max-lateness) │
//!                                               │                                                   └────────────┘    └──────────────┘
//!                                               │ ┌───────┐    ┌──────────────┐    ┌──────────────┐    ┌────────┐    ┌─────────────────────┐
//!                                               └►│queue  │───►│audioconvert  │───►│audioresample │───►│volume │───►│alsasink             │
//!                                                 │(50buf │    │              │    │              │    │        │    │(ts-offset=+100ms,   │
//!                                                 │2s max)│    │              │    │              │    │        │    │ device=plughw:C,D) │
//!                                                 └───────┘    └──────────────┘    └──────────────┘    └────────┘    └─────────────────────┘
//! ```
//!
//! ## Pipeline Topology (HEVC/H.265 — ISP fallback)
//!
//! ```text
//! ┌──────────┐    ┌────────┐    ┌──────────┐    ┌───────┐    ┌──────────┐    ┌────────────────────┐    ┌────────────┐    ┌──────────┐
//! │souphttpsrc│───►│queue2  │───►│parsebin  │──┬►│queue  │───►│h265parse │───►│v4l2slh265dec      │───►│v4l2convert │───►│kmssink  │
//! │(SOCKS5h) │    │(buffer)│    │(demux)   │  │ │       │    │          │    │(stateless HEVC)   │    │(ISP:       │    │(DRM/KMS)│
//! └──────────┘    └────────┘    └──────────┘  │ └───────┘    └──────────┘    └────────────────────┘    │SAND→NV12)  │    └──────────┘
//!                                               │                                                                 └────────────┘
//!                                               │  (audio branch same as H.264)
//! ```
//!
//! NOTE: The `v4l2slh265dec` stateless decoder does NOT have `output-io-mode` or
//! `capture-io-mode` properties (those belong to the stateful `v4l2h264dec`/`v4l2h265dec`).
//! DMA-BUF I/O mode is auto-negotiated by the `GstV4l2Decoder` base class.
//!
//! ## Fallback
//!
//! If V4L2 decode fails to negotiate (e.g. non-H.264 input),
//! the pipeline falls back to software decode:
//!
//! ```text
//! souphttpsrc → queue2 → parsebin → avdec_h264 → videoconvert → kmssink
//! ```
//!
//! ## Colorimetry
//!
//! A `capssetter` element was previously used to force `colorimetry=bt709`
//! on the decoded output. It was removed because it caused caps negotiation
//! failures: `capssetter` with `replace=true` destroyed essential raw video
//! caps fields (format, width, height, framerate, interlace-mode, memory
//! features) that kmssink requires, resulting in "not-negotiated (-4)".
//! Most streams already report bt709 in their H.264 VUI parameters, and
//! v4l2h264dec passes this through to kmssink correctly without intervention.

use crate::stream_source::{ProgressState, StreamSource};
use crate::{BufferHealth, DownloadProgress, PipelineConfig, PlaybackError};
use gstreamer::prelude::*;
use gstreamer::{Element, ElementFactory, Pipeline, State};
#[cfg(feature = "hevc")]
use bogdan_v3d::V3dComputeEngine;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Ensure GStreamer is initialised exactly once.
static GST_INIT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();

/// Initialise GStreamer. Safe to call multiple times.
/// Returns an error if initialisation fails (instead of panicking),
/// and subsequent calls will return the same error.
fn ensure_gst_init() -> Result<(), PlaybackError> {
    match GST_INIT.get_or_init(|| match gstreamer::init() {
        Ok(()) => {
            tracing::debug!("GStreamer initialised successfully");
            Ok(())
        },
        Err(e) => {
            let message = format!("GStreamer init failed (permanent): {}", e);
            tracing::error!("{}", message);
            Err(message)
        },
    }) {
        Ok(()) => Ok(()),
        Err(message) => Err(PlaybackError::Gstreamer(message.clone())),
    }
}

/// A constructed GStreamer pipeline ready for state transitions.
pub struct GstPipeline {
    /// The GStreamer pipeline element.
    pipeline: Pipeline,
    /// The video sink element (kmssink or fallback).
    /// Prefixed with `_` to suppress dead_code warning; retained for future
    /// use (e.g. querying display resolution, DRM master status).
    _video_sink: Element,
    /// The volume element for audio control.
    volume: Element,
    /// Current pipeline state.
    state: PipelineState,
    /// Keeps the GStreamer bus watch alive for this pipeline.
    bus_watch: Option<gstreamer::bus::BusWatchGuard>,
    /// Cancel token for the appsrc push task. Set to true when the
    /// pipeline is stopped to signal the background download+push task
    /// to terminate.
    push_cancel: Option<Arc<AtomicBool>>,
    /// Shared download progress state, updated by the StreamSource
    /// download task and read via `download_progress()`.
    download_progress: Arc<ProgressState>,
    /// Whether the stream is rate-limited by the CDN (sp= parameter).
    /// When true, the bus watch uses more aggressive buffering thresholds
    /// to minimise rebuffer pauses.
    is_rate_limited: bool,
    /// V3D compute shader engine for SAND→NV12 conversion.
    /// Only present when HEVC decode is enabled and V3D is available.
    #[cfg(feature = "hevc")]
    _v3d_engine: Option<V3dComputeEngine>,
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
    /// The `url` parameter is the direct media URL (CDN URL) to stream.
    ///
    /// The `source_url` parameter is the original page URL that the user
    /// cast. It's used to set the Referer header — many CDNs (Voe,
    /// DoodStream) require the Referer to match the originating site's
    /// domain, not the CDN's domain. If empty, the Referer falls back
    /// to the media URL's origin.
    ///
    /// The `socks_addr` parameter provides the Tor SOCKS5 proxy address
    /// (e.g. "127.0.0.1:9050"). If non-empty and the URL is not loopback,
    /// a local HTTP CONNECT→SOCKS5 forwarder is started that routes
    /// souphttpsrc through Tor's SOCKS5 with the same isolation username
    /// as the resolver. Same SOCKS5 username = same Tor circuit = same
    /// exit IP, so CDN IP-bound tokens match.
    ///
    /// The `isolation_username` is the SOCKS5 username for Tor's
    /// IsolateSOCKSAuth circuit isolation. It must match the username
    /// used during URL resolution to ensure the same Tor circuit.
    pub async fn new(
        url: &str,
        source_url: &str,
        socks_addr: &str,
        isolation_username: &str,
        config: &PipelineConfig,
        cookies: &[String],
    ) -> Result<Self, PlaybackError> {
        ensure_gst_init()?;

        let pipeline = Pipeline::new();

        // CRITICAL: Enable async-handling on the pipeline so it doesn't
        // block waiting for async sinks (kmssink, alsasink) to preroll
        // before completing its own state change.
        //
        // Without async-handling, the pipeline gets stuck at Ready→Paused
        // because kmssink's sink pad is initially unconnected (the video
        // decode chain is created dynamically in parsebin's pad-added
        // callback). kmssink can't preroll without data, and it can't
        // receive data because it's not linked to the decoder yet. This
        // creates a deadlock: the pipeline waits for kmssink to preroll,
        // but kmssink can't preroll until the pipeline connects it.
        //
        // With async-handling=true, the pipeline reaches Paused
        // immediately without waiting for async children. The bus watch
        // then handles the transition to Playing based on buffer state.
        // kmssink and alsasink complete their preroll asynchronously
        // when they receive their first buffers, and the pipeline
        // transitions to Playing smoothly.
        //
        // This is safe because our bus watch explicitly controls when
        // to start playing (based on buffer fill level), so we don't
        // need the pipeline to gate the Playing transition on preroll.
        pipeline.set_property("async-handling", true);

        // ── Source element ──────────────────────────────────────────
        //
        // CDN anti-bot bypass: souphttpsrc uses HTTP/1.1 + GnuTLS, which
        // CDN anti-bot systems flag as non-browser (Chrome uses HTTP/2 +
        // BoringSSL). To bypass this, we start a local streaming HTTP
        // media proxy that:
        //   1. Accepts HTTP/1.1 connections from souphttpsrc on localhost
        //   2. Fetches from the CDN via reqwest (HTTP/2 + rustls TLS)
        //   3. Streams the response body back to souphttpsrc
        //
        // The CDN sees reqwest's HTTP/2 + rustls connection (matching
        // Chrome's fingerprint), while souphttpsrc sees a simple HTTP
        // response from localhost. This eliminates the 403 Forbidden
        // errors caused by CDN TLS/HTTP fingerprinting.
        //
        // The media proxy internally starts a SOCKS forwarder for Tor
        // circuit isolation (same exit IP as the resolver → CDN IP-
        // binding token matches).
        //
        // StreamSource supports two modes:
        //   1. SOCKS mode (socks_addr + isolation_username provided):
        //      Routes through Tor for CDN downloads. The CDN URL was
        //      resolved through Tor, so it's bound to the Tor exit IP.
        //   2. Direct mode (socks_addr or isolation_username empty):
        //      Connects directly to the CDN without Tor. The CDN URL
        //      was resolved without Tor, so it's bound to the local IP.
        //      This avoids CDN blocking of Tor exit IPs.
        let is_loopback_url = url.starts_with("http://127.0.0.1:")
            || url.starts_with("http://localhost:")
            || url.starts_with("http://[::1]:");

        // Use StreamSource for all non-loopback CDN URLs.
        // When socks_addr/isolation_username are empty, StreamSource
        // operates in direct mode (no SOCKS forwarder, no Tor).
        let use_stream_source = !is_loopback_url;

        // ── Source element ──────────────────────────────────────────
        //
        // CDN URLs go through StreamSource + appsrc (progressive download):
        //   CDN → Tor → SOCKS Forwarder → reqwest → channel → appsrc → queue2
        //
        // This replaces the old MediaProxy + souphttpsrc path:
        //   CDN → Tor → SOCKS Forwarder → reqwest → MediaProxy HTTP → souphttpsrc → queue2
        //
        // The appsrc path eliminates the MediaProxy HTTP server hop (one fewer
        // user-space relay) and decouples download from playback, allowing
        // throughput-aware buffering.
        //
        // Loopback URLs use souphttpsrc directly (no Tor/proxy needed).

        let mut stream_source = None;
        let mut push_cancel = None;
        let download_progress = Arc::new(ProgressState::new());

        let src = if use_stream_source {
            // ── CDN URL: appsrc + StreamSource (progressive download) ──
            //
            // Start the StreamSource, do a preflight CDN check, then
            // create an appsrc element that receives downloaded data
            // via a background push task.

            let mut source = StreamSource::start(
                url.to_string(),
                source_url.to_string(),
                socks_addr.to_string(),
                isolation_username.to_string(),
                cookies.to_vec(),
                download_progress.clone(),
            )
            .await
            .map_err(|e| PlaybackError::PipelineCreation(e))?;

            // Preflight CDN check — verify the CDN accepts this Tor circuit.
            // For MP4: verifies the CDN URL via Range request. For HLS:
            // fetches and parses the master/variant playlists.
            if let Err(e) = source.preflight_check().await {
                tracing::warn!(
                    error = %e,
                    "stream source: preflight CDN check failed (including fallback) — re-resolve needed"
                );
                return Err(PlaybackError::PipelineCreation(
                    format!("CDN preflight failed: {} — re-resolve needed", e),
                ));
            }

            // Log CDN rate limit and check for bitrate mismatch.
            // The sp= parameter in CDN URLs caps download speed (e.g. sp=380
            // = 380 kbps). If the rate limit is below typical video bitrates,
            // playback will stutter — the download can never keep up with
            // the decode rate regardless of buffer size.
            let cdn_rate_limit = *download_progress.cdn_rate_limit_kbps.lock().unwrap();
            if let Some(rate_limit) = cdn_rate_limit {
                // Estimate video bitrate from Content-Length / assumed duration.
                // Typical CDN videos are 20-60 minutes. A conservative estimate
                // uses 30 minutes (1800 seconds) as the denominator.
                let content_length = download_progress.total_bytes.lock().unwrap();
                if let Some(total_bytes) = *content_length {
                    let estimated_bitrate_kbps = (total_bytes * 8) / (1800 * 1000);
                    if rate_limit < estimated_bitrate_kbps {
                        tracing::warn!(
                            cdn_rate_limit_kbps = rate_limit,
                            estimated_video_bitrate_kbps = estimated_bitrate_kbps,
                            content_length_bytes = total_bytes,
                            "⚠ CDN rate limit (sp={}) is BELOW estimated video bitrate ({} kbps). \
                             Playback WILL stutter — download speed cannot sustain decode rate. \
                             Consider selecting a lower quality stream.",
                            rate_limit,
                            estimated_bitrate_kbps
                        );
                    } else {
                        tracing::info!(
                            cdn_rate_limit_kbps = rate_limit,
                            estimated_video_bitrate_kbps = estimated_bitrate_kbps,
                            "CDN rate limit is above estimated video bitrate — smooth playback expected"
                        );
                    }
                } else {
                    tracing::warn!(
                        cdn_rate_limit_kbps = rate_limit,
                        "CDN URL has rate limit (sp={}) but no Content-Length — \
                         cannot estimate bitrate mismatch. Playback may stutter if \
                         video bitrate exceeds {} kbps.",
                        rate_limit,
                        rate_limit
                    );
                }
            }

            // Start downloading from the CDN immediately. Data flows
            // into the StreamSource's internal channel.
            source.start_download(None);

            tracing::info!(
                cdn_url = url,
                cdn_rate_limit_kbps = ?download_progress.cdn_rate_limit_kbps.lock().unwrap(),
                "stream source started — appsrc will receive downloaded data via channel (no MediaProxy HTTP hop)"
            );

            // Create appsrc element for pushing downloaded data.
            let appsrc = ElementFactory::make("appsrc")
                .property_from_str("stream-type", "stream") // GST_APP_STREAM_TYPE_STREAM — sequential, no seeking
                .property_from_str("format", "bytes") // GST_FORMAT_BYTES
                .property("is-live", false)
                .property("block", true) // Block push-buffer when downstream is full — provides proper flow control
                .build()
                .map_err(|e| PlaybackError::PipelineCreation(format!("appsrc: {}", e)))?;

            stream_source = Some(source);
            appsrc
        } else {
            // ── Loopback URL: souphttpsrc directly ──
            tracing::debug!("loopback media URL detected; connecting directly");
            ElementFactory::make("souphttpsrc")
                .property("location", url)
                .property("timeout", 120u32)
                .build()
                .map_err(|e| PlaybackError::PipelineCreation(format!("souphttpsrc: {}", e)))?
        };

        // ── Buffer element ──────────────────────────────────────────
        //
        // queue2 sits between the source (appsrc or souphttpsrc) and
        // parsebin, providing a download buffer for network resilience.
        //
        // With `use-buffering=true`, queue2 emits BUFFERING messages
        // and the pipeline waits for the buffer to fill before playing.
        // This is essential for Tor-routed streams where throughput is
        // variable (typically 1-5 Mbps). Without buffering, the queue
        // empties faster than Tor can refill it, causing stalls and
        // low effective FPS.
        //
        // For the appsrc path (CDN URLs): queue2 provides the primary
        // buffering mechanism. appsrc pushes data from the StreamSource
        // channel at the CDN's download rate. queue2 accumulates this
        // data and controls when playback starts (high-percent).
        //
        // Adaptive buffering: if the stream is rate-limited by the CDN
        // (sp= parameter present and not successfully bypassed), we use
        // maximum buffering parameters to minimise rebuffer pauses.
        // Rate-limited streams have a fixed ceiling on download speed,
        // so the buffer will drain faster than it fills once playback
        // starts. A larger buffer and higher thresholds give more
        // play time before rebuffering is needed.
        let is_rate_limited = download_progress.cdn_rate_limit_kbps.lock().unwrap().is_some();

        let queue2 = if is_rate_limited {
            // Rate-limited stream: use maximum buffering
            tracing::info!(
                "queue2: using rate-limited buffering profile (500 MB buffer, 600s max time, 99% high-percent)"
            );
            ElementFactory::make("queue2")
                .property("max-size-bytes", 500_000_000u32) // 500 MB — absolute max buffer
                .property("max-size-time", 600_000_000_000u64) // 600 seconds — 10 minutes
                .property("use-buffering", true)
                .property("high-percent", 99i32) // Wait until 99% full before playing
                .property("low-percent", 5i32) // Only pause at 5% — give maximum play time
                .build()
                .map_err(|e| PlaybackError::PipelineCreation(format!("queue2: {}", e)))?
        } else {
            // Normal stream: standard buffering
            tracing::info!(
                "queue2: using standard buffering profile (400 MB buffer, 300s max time, 95% high-percent)"
            );
            ElementFactory::make("queue2")
                .property("max-size-bytes", 400_000_000u32) // 400 MB — larger buffer for CDN streams
                .property("max-size-time", 300_000_000_000u64) // 300 seconds of media data
                .property("use-buffering", true)
                .property("high-percent", 95i32) // start playing when 95% full
                .property("low-percent", 10i32) // pause when buffer drops to 10%
                .build()
                .map_err(|e| PlaybackError::PipelineCreation(format!("queue2: {}", e)))?
        };

        // ── Demuxer ─────────────────────────────────────────────────
        let parsebin = ElementFactory::make("parsebin")
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("parsebin: {}", e)))?;

        // ── Video sink ──────────────────────────────────────────────
        //
        // kmssink is created now and added to the pipeline. The video decode
        // chain (queue + decoder + optional converter) is NOT created here —
        // it is built dynamically in the parsebin pad-added callback based on
        // the actual codec detected at runtime (H.264 vs HEVC). This avoids
        // the caps mismatch that occurred when a pre-built HEVC video bin
        // (h265parse → v4l2slh265dec) received H.264 data from parsebin.
        //
        // parsebin already includes the appropriate parser (h264parse or
        // h265parse) internally, so we don't need a parser in the video
        // chain — just the decoder and optional format converter.
        let video_sink = Self::build_kmssink(config)?;

        // ── Audio elements ──────────────────────────────────────────
        //
        // Audio chain: audio_queue → avdec_aac → audioconvert → audioresample → volume → alsasink
        //
        // The audio decoder is essential: parsebin outputs *encoded* audio
        // (e.g. `audio/mpeg, mpegversion=4` for AAC), but audioconvert
        // only handles *raw* PCM.  Without a decoder, caps negotiation
        // fails with "Noformat" and the unlinked pad kills the pipeline
        // with "not-linked (-1)".
        //
        // avdec_aac handles AAC (the most common codec in MP4 containers
        // from video CDNs).  If the Pi doesn't have gst-libav installed,
        // fdkaacdec is tried as a fallback.  If neither is available,
        // audio playback is skipped (video still works).
        // Audio queue: sits between parsebin's audio output and the
        // audio decoder. Limits how much audio data can be buffered.
        //
        // max-size-buffers=50 (down from 200): Each audio buffer is
        // typically 20-25ms. 50 buffers = ~1 second, which is plenty for
        // smoothing jitter without introducing excessive latency. 200
        // buffers could buffer ~4 seconds of audio, which adds
        // unnecessary latency and can exacerbate A/V desync.
        //
        // max-size-time=2_000_000_000 (2 seconds): Time-based limit
        // prevents the queue from buffering more than 2 seconds of audio
        // data regardless of buffer count. Without this, the queue could
        // hold many seconds of audio in low-bitrate streams, causing
        // variable and unpredictable audio latency.
        let audio_queue = ElementFactory::make("queue")
            .property("max-size-buffers", 50u32)
            .property("max-size-time", 2_000_000_000u64)
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("audio_queue: {}", e)))?;

        // ── Audio decoder ──────────────────────────────────────────
        //
        // Use `avdec_aac` directly for AAC decoding. This is the lowest-latency
        // path because it avoids decodebin3's internal parsebin + multiqueue, which
        // add ~200-500ms of buffering latency and cause audio-video desync.
        //
        // Audio codec detection happens in the parsebin pad-added callback,
        // where we check the audio caps. If the stream is not AAC, the pad
        // fails to link to the AAC-only decoder, and a fakesink is added so
        // video can still play. This is acceptable because AAC is by far the
        // most common codec in MP4 containers from video CDNs.
        //
        // Future improvement: detect the codec in pad-added and dynamically
        // create the right decoder (avdec_mp3, avdec_opus, etc.).
        let audio_decoder = ElementFactory::make("avdec_aac")
            .build()
            .or_else(|_| ElementFactory::make("fdkaacdec").build())
            .map_err(|e| {
                tracing::warn!("no AAC decoder available (avdec_aac or fdkaacdec) — audio will be disabled: {}", e);
                e
            }).ok();

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

        // audio sink (alsasink/pulsesink) — with configurable device and
        // A/V sync compensation.
        //
        // ts-offset: A positive value delays audio rendering by that many
        // nanoseconds. This compensates for video decode pipeline latency
        // that GStreamer's latency query may not fully account for.
        //
        // On Pi 4 with V4L2 hardware decode (v4l2h264dec + v4l2convert ISP),
        // the video pipeline has ~100-160ms of latency from:
        //   - v4l2h264dec: 2-4 capture buffers at 25fps = 80-160ms
        //   - v4l2convert (ISP): 1 frame = ~40ms
        // GStreamer's latency query often under-reports this because the
        // V4L2 stateful decoder's actual latency depends on the codec's
        // reordering window and B-frame depth, which aren't known until
        // after the first few frames are decoded.
        //
        // Without ts-offset compensation, audio plays ahead of video
        // (lip-sync desync). The default 100ms offset (configurable via
        // PipelineConfig::audio_ts_offset_ns) shifts audio later to
        // align with the video output.
        //
        // alsasink keeps async=true (the default) so the pipeline clock
        // starts only after alsasink prerolls (receives its first audio
        // buffer). This ensures the pipeline clock is correctly synchronised
        // with the audio stream. kmssink uses async=false to avoid the
        // preroll deadlock (see kmssink comments in build_kmssink).
        //
        // The `device` property routes audio to a specific ALSA output
        // (e.g. "plughw:1,0" for HDMI).  When empty, ALSA's default
        // device is used.  Use `plughw` (not `hw`) because plughw allows
        // ALSA to convert formats — HDMI devices may not accept the exact
        // F32LE format that GStreamer negotiates.
        //
        // For Bluetooth audio, PulseAudio is typically required on Pi.
        // If `audio_sink` is "pulsesink", the `device` property sets the
        // PulseAudio sink name (not ALSA device). If empty, pulsesink
        // uses PulseAudio's default sink (which auto-routes to Bluetooth
        // if configured).
        let mut audiosink_builder = ElementFactory::make(&config.audio_sink);
        if !config.audio_device.is_empty() {
            // For alsasink: device="plughw:1,0"
            // For pulsesink: device="alsa_output.bluetooth" (PulseAudio sink name)
            if config.audio_sink == "pulsesink" {
                audiosink_builder = audiosink_builder.property("device", &config.audio_device);
                tracing::info!(
                    device = %config.audio_device,
                    "pulsesink: using PulseAudio sink device"
                );
            } else {
                audiosink_builder = audiosink_builder.property("device", &config.audio_device);
                tracing::info!(
                    device = %config.audio_device,
                    "alsasink: using explicit ALSA device"
                );
            }
        } else {
            if config.audio_sink == "pulsesink" {
                tracing::info!("pulsesink: using PulseAudio default sink (auto-routes to Bluetooth if configured)");
            } else {
                tracing::info!("alsasink: using ALSA default device (no device property set)");
            }
        }

        // Apply A/V sync compensation: ts-offset delays audio rendering
        // to compensate for V4L2 hardware decode latency.
        if config.audio_ts_offset_ns != 0 {
            audiosink_builder = audiosink_builder.property("ts-offset", config.audio_ts_offset_ns);
            tracing::info!(
                ts_offset_ms = config.audio_ts_offset_ns as f64 / 1_000_000.0,
                sink = %config.audio_sink,
                "A/V sync: audio ts-offset applied (compensating for V4L2 decode latency)"
            );
        }

        let audiosink = audiosink_builder.build().map_err(|e| {
            PlaybackError::PipelineCreation(format!("{}: {}", config.audio_sink, e))
        })?;

        // ── Assemble pipeline ───────────────────────────────────────
        let mut all_elements: Vec<&Element> = vec![
            &src,
            &queue2,
            &parsebin,
            &video_sink,
            &audio_queue,
            &audioconvert,
            &audioresample,
            &volume,
            &audiosink,
        ];
        if let Some(ref dec) = audio_decoder {
            all_elements.push(dec);
        }
        pipeline
            .add_many(&all_elements)
            .map_err(|e| PlaybackError::PipelineCreation(format!("add elements: {}", e)))?;

        // Link: src → queue2 → parsebin
        Element::link_many([&src, &queue2, &parsebin]).map_err(|e| {
            PlaybackError::PipelineCreation(format!("link src→queue2→parsebin: {}", e))
        })?;

        // Link audio chain.
        // If we have an audio decoder: audio_queue → avdec_aac → audioconvert → audioresample → volume → alsasink
        // If no decoder available:        audio_queue → audioconvert → audioresample → volume → alsasink
        //   (will fail caps negotiation for encoded audio, but the fakesink
        //    fallback in the pad-added handler prevents pipeline death)
        if let Some(ref dec) = audio_decoder {
            Element::link_many([
                &audio_queue,
                dec,
                &audioconvert,
                &audioresample,
                &volume,
                &audiosink,
            ])
            .map_err(|e| {
                PlaybackError::PipelineCreation(format!("link audio chain (with decoder): {}", e))
            })?;
            tracing::info!(
                "audio chain: audio_queue → avdec_aac → audioconvert → audioresample → volume → {}",
                config.audio_sink
            );
        } else {
            Element::link_many([&audio_queue, &audioconvert, &audioresample, &volume, &audiosink])
                .map_err(|e| {
                    PlaybackError::PipelineCreation(format!("link audio chain (no decoder): {}", e))
                })?;
            tracing::warn!("audio chain has no decoder — encoded audio streams will be dropped");
        }

        // ── Dynamic pad linking (parsebin → video/audio) ────────────
        //
        // parsebin emits "pad-added" for each stream it discovers in the
        // container.  The handler must link each pad to the appropriate
        // branch (video bin or audio chain).
        //
        // IMPORTANT: when parsebin creates a source pad, current_caps()
        // may return None if caps negotiation hasn't completed yet.  This
        // is especially common for the *first* pad (usually video) — the
        // demuxer creates the pad before the downstream decoder has
        // responded with its accepted caps.  If we only check
        // current_caps(), we miss the video pad and the screen stays
        // black forever.
        //
        // Strategy:
        //   1. Try current_caps() (already-negotiated caps).
        //   2. If None, fall back to query_caps(None) which returns the
        //      pad's template caps — these describe what media type the
        //      pad *will* produce (e.g. "video/x-h264") even before
        //      negotiation completes.
        //   3. If we still can't determine the type, try linking to the
        //      audio chain, then add a fakesink as a last resort.
        let video_sink_weak = video_sink.downgrade();
        let audio_queue_weak = audio_queue.downgrade();
        let pipeline_weak = pipeline.downgrade();
        let hw_accel = config.hw_accel;

        parsebin.connect_pad_added(move |_parsebin, pad| {
            // Step 1: Try already-negotiated caps
            let current_caps = pad.current_caps();

            // Step 2: Fallback to template/query caps
            let caps = current_caps.or_else(|| {
                let query = pad.query_caps(None);
                if query.is_fixed() || !query.is_empty() {
                    tracing::info!(
                        pad = %pad.name(),
                        query_caps = %query.to_string(),
                        "current_caps was empty — using query_caps (template caps) to determine media type"
                    );
                    Some(query)
                } else {
                    None
                }
            });

            let media_type = caps.as_ref().and_then(|c| c.structure(0).map(|s| s.name().to_string()));

            let is_video = media_type.as_ref().map(|t| t.starts_with("video/")).unwrap_or(false);
            let is_audio = media_type.as_ref().map(|t| t.starts_with("audio/")).unwrap_or(false);

            // Log every pad that parsebin creates
            let pad_name = pad.name().to_string();
            let caps_str = caps.as_ref().map(|c| c.to_string()).unwrap_or_default();
            tracing::info!(
                pad = %pad_name,
                media_type = ?media_type,
                is_video = is_video,
                is_audio = is_audio,
                caps = %caps_str,
                "parsebin pad-added"
            );

            if is_video {
                // ── Dynamic video chain creation ──────────────────────
                //
                // We don't know the video codec until parsebin discovers it.
                // Build the appropriate decoder chain on-the-fly based on
                // the detected media type:
                //
                //   H.264 + hw_accel: queue → v4l2h264dec(dmabuf) → v4l2convert(ISP) → kmssink
                //   H.265 + hw_accel: queue → v4l2slh265dec → v4l2convert(ISP) → kmssink
                //   Software decode:   queue → avdec_h264 → videoconvert → kmssink
                //
                // parsebin already includes the parser (h264parse/h265parse),
                // so we don't add one here — we go straight to the decoder.

                let is_h265 = media_type.as_ref()
                    .map(|t| t.contains("h265") || t.contains("hevc"))
                    .unwrap_or(false);

                let pipe = match pipeline_weak.upgrade() {
                    Some(p) => p,
                    None => {
                        tracing::error!("pipeline weak ref failed — cannot create video chain");
                        return;
                    }
                };

                let ksink = match video_sink_weak.upgrade() {
                    Some(k) => k,
                    None => {
                        tracing::error!("kmssink weak ref failed — cannot create video chain");
                        return;
                    }
                };

                // Check if kmssink's sink pad is already linked (from a previous
                // video chain that was set up dynamically)
                let kmssink_sink = ksink.static_pad("sink")
                    .expect("kmssink should have a sink pad");
                if kmssink_sink.is_linked() {
                    tracing::info!("video chain already linked to kmssink, skipping");
                    return;
                }

                // Create video queue (shared by all decode paths)
                // 200 buffers at 25fps = 8 seconds of video buffer. This is
                // critical for smooth playback on Tor-routed streams where
                // network throughput is variable. With only 60 buffers (2.4s),
                // the queue starves quickly during throughput dips, causing
                // the decoder to stall and produce low FPS.
                let video_queue = match ElementFactory::make("queue")
                    .property("max-size-buffers", 200u32)
                    .property("max-size-time", 5_000_000_000u64) // 5 seconds time-based limit
                    .build()
                {
                    Ok(q) => q,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to create video queue");
                        return;
                    }
                };

                // Build the appropriate decoder chain based on codec + hw_accel
                if is_h265 && hw_accel && cfg!(feature = "hevc") {
                    // ── HEVC hardware decode ────────────────────────
                    //
                    // v4l2slh265dec outputs SAND128 (NV12_64Z32) which
                    // kmssink can't scan out directly. v4l2convert uses
                    // the bcm2835-ISP hardware to convert SAND128→NV12.

                    let decoder = match ElementFactory::make("v4l2slh265dec").build() {
                        Ok(d) => d,
                        Err(e) => {
                            tracing::error!(error = %e, "v4l2slh265dec not available");
                            return;
                        }
                    };
                    let converter = match ElementFactory::make("v4l2convert").build() {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::error!(error = %e, "v4l2convert not available");
                            return;
                        }
                    };
                    // Set DMA-BUF io modes on v4l2convert for zero-copy ISP
                    // conversion (SAND128→NV12). Same as H.264 path.
                    converter.set_property_from_str("output-io-mode", "dmabuf");
                    converter.set_property_from_str("capture-io-mode", "dmabuf");

                    // Add elements to pipeline
                    if let Err(e) = pipe.add_many([&video_queue, &decoder, &converter]) {
                        tracing::error!(error = %e, "failed to add HEVC elements to pipeline");
                        return;
                    }

                    // Link: video_queue → v4l2slh265dec → v4l2convert → kmssink
                    if let Err(e) = Element::link_many([&video_queue, &decoder, &converter]) {
                        tracing::error!(error = %e, "failed to link HEVC decode chain");
                        return;
                    }
                    if let Err(e) = converter.link(&ksink) {
                        tracing::error!(error = ?e, "failed to link v4l2convert → kmssink");
                        return;
                    }

                    // Set elements to Paused so they participate in preroll
                    let _ = video_queue.set_state(State::Paused);
                    let _ = decoder.set_state(State::Paused);
                    let _ = converter.set_state(State::Paused);

                    tracing::info!("HEVC video chain: video_queue → v4l2slh265dec → v4l2convert(ISP) → kmssink");

                } else if !is_h265 && hw_accel {
                    // ── H.264 hardware decode ───────────────────────
                    //
                    // v4l2h264dec with capture-io-mode=dmabuf may output
                    // SAND128 (NV12_64Z32) tiled format on Pi 4, which
                    // kmssink cannot scan out directly. v4l2convert uses
                    // the bcm2835-ISP hardware to convert SAND→NV12 (or
                    // passthrough if already NV12). This matches the HEVC
                    // decode path which also uses v4l2convert for the
                    // same reason.

                    let decoder = match ElementFactory::make("v4l2h264dec").build() {
                        Ok(d) => d,
                        Err(e) => {
                            tracing::error!(error = %e, "v4l2h264dec not available");
                            return;
                        }
                    };
                    // Set DMA-BUF io modes for zero-copy decode
                    decoder.set_property_from_str("output-io-mode", "dmabuf");
                    decoder.set_property_from_str("capture-io-mode", "dmabuf");

                    let converter = match ElementFactory::make("v4l2convert").build() {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::error!(error = %e, "v4l2convert not available for H.264 path");
                            return;
                        }
                    };
                    // Set DMA-BUF io modes on v4l2convert for zero-copy ISP
                    // conversion (SAND128→NV12). Without dmabuf, v4l2convert
                    // copies buffers through system memory, which is slow and
                    // wastes memory bandwidth on the Pi 4's shared bus.
                    converter.set_property_from_str("output-io-mode", "dmabuf");
                    converter.set_property_from_str("capture-io-mode", "dmabuf");

                    // Add elements to pipeline
                    if let Err(e) = pipe.add_many([&video_queue, &decoder, &converter]) {
                        tracing::error!(error = %e, "failed to add H.264 elements to pipeline");
                        return;
                    }

                    // Link: video_queue → v4l2h264dec → v4l2convert(ISP) → kmssink
                    if let Err(e) = Element::link_many([&video_queue, &decoder, &converter]) {
                        tracing::error!(error = %e, "failed to link H.264 decode chain");
                        return;
                    }
                    if let Err(e) = converter.link(&ksink) {
                        tracing::error!(error = ?e, "failed to link v4l2convert → kmssink");
                        return;
                    }

                    let _ = video_queue.set_state(State::Paused);
                    let _ = decoder.set_state(State::Paused);
                    let _ = converter.set_state(State::Paused);

                    tracing::info!("H.264 video chain: video_queue → v4l2h264dec(dmabuf) → v4l2convert(ISP) → kmssink");

                } else {
                    // ── Software decode fallback ────────────────────

                    let decoder = match ElementFactory::make("avdec_h264")
                        .build()
                        .or_else(|_| ElementFactory::make("avdec_h265").build())
                    {
                        Ok(d) => d,
                        Err(e) => {
                            tracing::error!(error = %e, "no software video decoder available");
                            return;
                        }
                    };
                    let converter = match ElementFactory::make("videoconvert").build() {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::error!(error = %e, "videoconvert not available");
                            return;
                        }
                    };

                    if let Err(e) = pipe.add_many([&video_queue, &decoder, &converter]) {
                        tracing::error!(error = %e, "failed to add SW decode elements to pipeline");
                        return;
                    }

                    if let Err(e) = Element::link_many([&video_queue, &decoder, &converter]) {
                        tracing::error!(error = %e, "failed to link SW decode chain");
                        return;
                    }
                    if let Err(e) = converter.link(&ksink) {
                        tracing::error!(error = ?e, "failed to link videoconvert → kmssink");
                        return;
                    }

                    let _ = video_queue.set_state(State::Paused);
                    let _ = decoder.set_state(State::Paused);
                    let _ = converter.set_state(State::Paused);

                    tracing::info!("SW video chain: video_queue → avdec → videoconvert → kmssink");
                }

                // Link parsebin pad → video_queue
                let queue_sink = video_queue.static_pad("sink")
                    .expect("video_queue should have a sink pad");
                if queue_sink.is_linked() {
                    tracing::info!("video queue sink already linked, skipping");
                    return;
                }
                match pad.link(&queue_sink) {
                    Ok(_) => tracing::info!(
                        caps = %caps_str,
                        "linked parsebin → video decode chain"
                    ),
                    Err(e) => {
                        tracing::error!(
                            error = ?e,
                            caps = %caps_str,
                            "failed to link parsebin video pad to decode chain"
                        );
                    },
                }
            } else if is_audio {
                if let Some(aq) = audio_queue_weak.upgrade() {
                    let sink_pad =
                        aq.static_pad("sink").expect("audio_queue should have a sink pad");
                    if sink_pad.is_linked() {
                        tracing::info!("audio pad already linked, skipping");
                        return;
                    }
                    let caps_str = caps
                        .as_ref()
                        .map(|c| c.to_string())
                        .unwrap_or_default();
                    match pad.link(&sink_pad) {
                        Ok(_) => tracing::info!(
                            caps = %caps_str,
                            "linked parsebin → audio_queue"
                        ),
                        Err(e) => {
                            // Audio pad can't link to the audio chain (e.g. unsupported
                            // codec, missing decoder).  An unlinked pad causes GStreamer
                            // to stop the pipeline with "not-linked (-1)", which also
                            // kills the video stream.  To prevent this, we add a
                            // fakesink that silently discards the audio data, allowing
                            // the video to play without audio.
                            tracing::warn!(
                                error = ?e,
                                "failed to link parsebin audio pad to audio chain — \
                                 adding fakesink so video can still play"
                            );
                            if let Some(pipe) = pipeline_weak.upgrade() {
                                match ElementFactory::make("fakesink")
                                    .property("sync", false)
                                    .property("silent", true)
                                    .build()
                                {
                                    Ok(fakesink) => {
                                        let fakesink_name = fakesink.name().to_string();
                                        if let Err(add_err) = pipe.add(&fakesink) {
                                            tracing::warn!(error = %add_err, "failed to add audio fakesink to pipeline");
                                        } else if let Err(link_err) = pad.link(&fakesink.static_pad("sink").expect("fakesink should have a sink pad")) {
                                            tracing::warn!(error = ?link_err, "failed to link audio pad to fakesink");
                                        } else {
                                            // The fakesink must be set to at least READY
                                            // before data flows, otherwise it will error.
                                            let _ = fakesink.set_state(State::Ready);
                                            tracing::info!(
                                                fakesink = %fakesink_name,
                                                "audio fakesink added — video will play without audio"
                                            );
                                        }
                                    },
                                    Err(mk_err) => {
                                        tracing::warn!(error = %mk_err, "failed to create audio fakesink");
                                    },
                                }
                            }
                        },
                    }
                }
            } else {
                // Neither video nor audio — could be a subtitle, metadata,
                // or (most critically) a video/audio pad whose caps aren't
                // available yet.  Try to link it to the audio chain first
                // (video chain requires codec detection for dynamic creation),
                // and if that fails, add a fakesink.
                tracing::warn!(
                    pad = %pad_name,
                    media_type = ?media_type,
                    caps = %caps_str,
                    "parsebin pad with unrecognised media type — attempting to link as audio, then fakesink"
                );

                // Try audio chain first (video chain requires dynamic creation
                // based on codec type, so we can't link unknown pads to it)
                let mut linked = false;

                // Try audio chain
                if !linked {
                    if let Some(aq) = audio_queue_weak.upgrade() {
                        let sink_pad =
                            aq.static_pad("sink").expect("audio_queue should have a sink pad");
                        if !sink_pad.is_linked() {
                            match pad.link(&sink_pad) {
                                Ok(_) => {
                                    tracing::info!("linked unknown pad → audio_queue (heuristic)");
                                    linked = true;
                                },
                                Err(e) => {
                                    tracing::warn!(
                                        error = ?e,
                                        "unknown pad failed to link as audio — adding fakesink"
                                    );
                                },
                            }
                        }
                    }
                }

                // If nothing worked, add fakesink to prevent pipeline death
                if !linked {
                    tracing::warn!(
                        pad = %pad_name,
                        "could not link unknown pad to video or audio — adding fakesink to prevent pipeline error"
                    );
                    if let Some(pipe) = pipeline_weak.upgrade() {
                        match ElementFactory::make("fakesink")
                            .property("sync", false)
                            .property("silent", true)
                            .build()
                        {
                            Ok(fakesink) => {
                                if let Err(add_err) = pipe.add(&fakesink) {
                                    tracing::warn!(error = %add_err, "failed to add fakesink for unknown pad");
                                } else if let Err(link_err) = pad.link(&fakesink.static_pad("sink").expect("fakesink sink pad")) {
                                    tracing::warn!(error = ?link_err, "failed to link unknown pad to fakesink");
                                } else {
                                    let _ = fakesink.set_state(State::Ready);
                                    tracing::info!("fakesink added for unrecognised pad");
                                }
                            },
                            Err(mk_err) => {
                                tracing::warn!(error = %mk_err, "failed to create fakesink for unknown pad");
                            },
                        }
                    }
                }
            }
        });

        // ── AppSrc push thread ────────────────────────────────────────
        //
        // If using StreamSource + appsrc, start a background thread that
        // reads downloaded data chunks from the StreamSource channel and
        // pushes them into the appsrc element as GStreamer buffers.
        //
        // This replaces the old MediaProxy HTTP server + souphttpsrc path.
        // Data flows: CDN → Tor → SOCKS Forwarder → reqwest → channel → appsrc.
        //
        // IMPORTANT: The push loop runs on a dedicated std::thread, NOT a
        // tokio task. With appsrc `block=true`, the push-buffer call blocks
        // when the pipeline's internal queue is full (e.g., during buffering).
        // Blocking a tokio worker thread would starve the async runtime;
        // a dedicated thread avoids this entirely.
        //
        // The thread bridges the async tokio channel (recv_chunk) to the
        // synchronous GStreamer push-buffer via Handle::block_on(). This is
        // safe because the thread is NOT inside a tokio async context.
        //
        // When the download completes, an EOS event is pushed into appsrc.
        if let Some(mut source) = stream_source.take() {
            let appsrc_weak = src.downgrade();
            let cancel = Arc::new(AtomicBool::new(false));
            let cancel_clone = cancel.clone();
            let tokio_handle = tokio::runtime::Handle::current();

            std::thread::Builder::new()
                .name("appsrc-push".into())
                .spawn(move || {
                    loop {
                        // Receive the next chunk from the download channel.
                        // Handle::block_on() bridges the async recv to this
                        // synchronous thread without blocking the tokio runtime.
                        let chunk = match tokio_handle.block_on(source.recv_chunk()) {
                            Some(c) => c,
                            None => {
                                // Channel closed — download completed or source dropped.
                                break;
                            },
                        };

                        if cancel_clone.load(Ordering::Relaxed) {
                            tracing::info!(offset = chunk.offset, "appsrc push thread: cancelled");
                            return;
                        }

                        // Push the chunk into appsrc as a GStreamer buffer.
                        // With block=true, this blocks when the pipeline's
                        // internal queue is full, providing natural backpressure
                        // that throttles the CDN download to match playback rate.
                        if let Some(appsrc) = appsrc_weak.upgrade() {
                            let buffer = gstreamer::Buffer::from_slice(chunk.data.to_vec());
                            let result =
                                appsrc.emit_by_name::<gstreamer::FlowReturn>("push-buffer", &[&buffer]);
                            match result {
                                gstreamer::FlowReturn::Ok => {},
                                gstreamer::FlowReturn::Flushing => {
                                    tracing::debug!(
                                        offset = chunk.offset,
                                        "appsrc is flushing — stopping push thread"
                                    );
                                    return;
                                },
                                gstreamer::FlowReturn::Eos => {
                                    tracing::debug!(
                                        offset = chunk.offset,
                                        "appsrc at EOS — stopping push thread"
                                    );
                                    return;
                                },
                                other => {
                                    tracing::warn!(
                                        offset = chunk.offset,
                                        result = ?other,
                                        "appsrc push-buffer returned unexpected result — stopping push thread"
                                    );
                                    return;
                                },
                            }
                        } else {
                            tracing::debug!(
                                offset = chunk.offset,
                                "appsrc element dropped — stopping push thread"
                            );
                            return;
                        }
                    }

                    // Download completed — push EOS into appsrc.
                    tracing::info!("appsrc push thread: download complete, pushing EOS");
                    if let Some(appsrc) = appsrc_weak.upgrade() {
                        let _ = appsrc.emit_by_name::<gstreamer::FlowReturn>("end-of-stream", &[]);
                    }
                })
                .expect("failed to spawn appsrc push thread");

            push_cancel = Some(cancel);
        }

        Ok(Self {
            pipeline,
            _video_sink: video_sink,
            volume,
            state: PipelineState::Ready,
            bus_watch: None,
            push_cancel,
            download_progress,
            is_rate_limited,
            #[cfg(feature = "hevc")]
            _v3d_engine: None, // V3D compute disabled — EGL context creation not yet implemented
        })
    }

    /// Build a kmssink element with the appropriate configuration.
    ///
    /// Shared between H.264, HEVC, and software decode paths to avoid
    /// duplicating the complex kmssink property logic (fd vs driver-name,
    /// plane-id, connector-id, max-lateness, etc.).
    fn build_kmssink(config: &PipelineConfig) -> Result<Element, PlaybackError> {
        let mut kmssink_builder = ElementFactory::make(&config.video_sink)
            .property("can-scale", true)
            // max-lateness controls how late a buffer can be before the sink
            // drops it. Setting -1 (unlimited) means kmssink NEVER drops late
            // buffers, which causes stuttering on Tor-routed streams: when
            // throughput dips, buffers arrive late, and displaying them pushes
            // the pipeline clock behind real-time. This compounds: each late
            // buffer makes subsequent buffers even later.
            //
            // With the appsrc-based pipeline (replacing MediaProxy + souphttpsrc),
            // the timing precision is lower than with souphttpsrc's native
            // GStreamer-driven pull model. The appsrc push model introduces
            // more jitter in frame delivery timing, especially during the
            // initial startup when the video decode chain is created dynamically
            // (~5 seconds after pipeline Paused). Frames can easily be 1-3
            // seconds late during the first few seconds of playback.
            //
            // 5 seconds (5_000_000_000ns) provides enough tolerance for the
            // appsrc pipeline's timing jitter while still dropping frames that
            // are truly too late (after a multi-second network stall) to
            // prevent clock drift. The previous value of 500ms was too tight
            // for the appsrc path — combined with qos=true, it created a
            // death spiral: a few late frames → QoS events → V4L2 decoder
            // skips frames → even fewer frames displayed → more QoS events
            // → decoder throttles to ~1 fps.
            .property("max-lateness", 5_000_000_000i64)
            // skip-vsync: when enabled, kmssink does NOT wait internally for
            // vsync when using atomic DRM drivers (like vc4 on Pi 4). Without
            // this, kmssink calls drmModeAtomicCommit with DRM_MODE_ATOMIC_ALLOW_MODESET
            // and then waits for the vblank event before returning, which adds
            // an extra vsync of latency (up to 16.7ms at 60Hz, 33.3ms at 30Hz).
            // For the vc4 atomic driver, this double-vsync is unnecessary — the
            // kernel's atomic commit already handles page-flip synchronization.
            // Enabling skip-vsync reduces display latency by one full frame.
            .property("skip-vsync", true)
            // qos (Quality of Service): DISABLED for hardware decode.
            //
            // When enabled, kmssink generates QoS events upstream when frames
            // are dropped or displayed late. For software decoders (avdec_h264),
            // QoS events help by skipping decode of frames that would arrive
            // too late to display. But for V4L2 hardware decoders
            // (v4l2h264dec/v4l2slh265dec), QoS events cause a death spiral:
            //
            //   1. A few frames arrive late (normal during initial startup
            //      with the appsrc push model)
            //   2. kmssink drops them and sends QoS events upstream
            //   3. V4L2 decoder receives QoS and starts skipping decode frames
            //   4. Fewer frames reach kmssink → more are late → more QoS events
            //   5. Decoder throttles down to ~1 fps
            //
            // The hardware decoder produces frames at the correct rate driven
            // by the pipeline clock. It does NOT benefit from QoS throttling
            // because the decode is already hardware-accelerated and can't
            // meaningfully "skip" frames without corrupting the decode state
            // (B-frames reference previous frames).
            //
            // Disabling QoS prevents the feedback loop while max-lateness
            // still drops truly late frames (after multi-second stalls) to
            // prevent clock drift.
            .property("qos", false)
            // async=false: kmssink completes its Ready→Paused transition
            // immediately WITHOUT waiting for the first video buffer. This is
            // CRITICAL on Raspberry Pi 4 because the video decode chain is
            // built dynamically in parsebin's pad-added callback, which fires
            // AFTER the pipeline is already transitioning to Paused.
            //
            // With async=true (the GStreamer default), a deadlock occurs:
            //   1. Pipeline set_state(Paused) → kmssink returns ASYNC (waiting
            //      for a buffer to preroll)
            //   2. Pipeline can't reach Paused because kmssink hasn't prerolled
            //   3. Data doesn't flow through the pipeline at Ready state
            //   4. kmssink never receives a buffer → never prerolls → deadlock
            //
            // With async=false, kmssink completes Ready→Paused immediately,
            // allowing the pipeline to reach Paused. Once in Paused, data flows
            // through the pipeline, caps negotiation completes, and kmssink
            // receives and displays frames normally.
            //
            // A/V sync is NOT affected: alsasink keeps async=true, so the
            // pipeline clock only starts when alsasink prerolls (receives its
            // first audio buffer). By that time, the video chain is already
            // flowing and kmssink is ready to render. The ts-offset on alsasink
            // compensates for V4L2 decode latency as before.
            .property("async", false);

        // The fd and driver-name properties are mutually exclusive in kmssink.
        // If we have a valid DRM fd from the DisplayManager, use it;
        // otherwise, use driver-name to let kmssink find the device itself.
        if let Some(drm_fd) = config.drm_fd {
            if drm_fd >= 0 {
                kmssink_builder = kmssink_builder.property("fd", drm_fd);
                tracing::info!(fd = drm_fd, "kmssink: using provided DRM fd");
            } else {
                kmssink_builder = kmssink_builder.property_from_str("driver-name", "vc4");
                tracing::info!("kmssink: using driver-name=vc4 (no valid fd)");
            }
        } else {
            kmssink_builder = kmssink_builder.property_from_str("driver-name", "vc4");
            tracing::info!("kmssink: using driver-name=vc4 (no fd provided)");
        }

        if config.plane_id > 0 {
            kmssink_builder = kmssink_builder.property("plane-id", config.plane_id as i32);
            tracing::info!(plane_id = config.plane_id, "kmssink: using explicit plane-id");
        } else {
            tracing::info!("kmssink: auto-detecting overlay plane (plane-id not set)");
        }

        if let Some(conn_id) = config.connector_id {
            if conn_id > 0 {
                kmssink_builder = kmssink_builder.property("connector-id", conn_id as i32);
                tracing::info!(connector_id = conn_id, "kmssink: using explicit connector-id");
            }
        }

        kmssink_builder
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("{}: {}", config.video_sink, e)))
    }

    /// Build the hardware-accelerated video branch:
    /// `h264parse → v4l2h264dec(dmabuf) → kmssink`
    ///
    /// No `capssetter` is used.  Previous versions used capssetter to
    /// force `colorimetry=bt709` on the decoded output, but this caused
    /// caps negotiation failures with kmssink:
    ///
    /// - With `replace=true` (even after the decoder, on video/x-raw),
    ///   capssetter destroyed essential raw video fields (format, width,
    ///   height, framerate, interlace-mode, DRM memory features) that
    ///   kmssink needs to negotiate a concrete renderable format.
    /// - Even with `join=true, replace=false`, capssetter could still
    ///   interfere with caps renegotiation during format changes.
    ///
    /// Most H.264 streams already carry bt709 in their VUI parameters,
    /// and v4l2h264dec passes this through correctly.  If colorimetry
    /// correction is needed in the future, it should be done via
    /// kmssink properties or a GStreamer video-filter that only
    /// modifies the colorimetry field without touching other caps.
    ///
    /// We use `capture-io-mode=dmabuf` (and `output-io-mode=dmabuf`)
    /// to enable the Pi zero-copy decode path.  With dmabuf, decoded
    /// frames stay in GPU/DMA memory and are passed directly to kmssink
    /// without copying through CPU/system memory.  This is the single
    /// most important property for boGDan's performance on Pi 4 —
    /// without it, `mmap` mode forces frames through system memory,
    /// which causes high CPU usage and frequent dropped buffers.
    #[allow(dead_code)]
    fn build_hw_video_bin(config: &PipelineConfig) -> Result<(Element, Element), PlaybackError> {
        let video_queue = ElementFactory::make("queue")
            .property("max-size-buffers", 200u32)
            .property("max-size-time", 5_000_000_000u64)
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("video_queue: {}", e)))?;

        // h264parse ensures the stream is properly framed for V4L2 decode.
        let h264parse = ElementFactory::make("h264parse")
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("h264parse: {}", e)))?;

        // V4L2 hardware H.264 decoder using bcm2835-codec on Pi 4.
        // Use dmabuf mode for zero-copy decode — decoded frames stay in
        // DMA memory and are passed directly to kmssink without CPU copies.
        // Without capture-io-mode=dmabuf, the decoder allocates system
        // memory buffers and the zero-copy path is broken, causing low
        // FPS and dropped buffers ("A lot of buffers are being dropped").
        //
        // IMPORTANT: We set the io-mode properties AFTER building the element
        // because `property_from_str` on `ElementFactoryBuilder` stores the
        // value as a string `SendValue` which may not be correctly converted
        // to the GEnum type when `build()` applies the properties. Setting
        // them on the built element with `set_property_from_str()` ensures
        // GStreamer's type conversion is invoked properly.
        let v4l2dec = ElementFactory::make("v4l2h264dec").build().map_err(|e| {
            PlaybackError::PipelineCreation(format!("v4l2h264dec (may not be available): {}", e))
        })?;

        // Set dmabuf io-modes on the built element for reliable type conversion.
        // GST_V4L2_IO_DMABUF = 3 (nick: "dmabuf").
        //
        // We set these properties AFTER building the element because
        // .property_from_str() on ElementFactoryBuilder stores values as
        // string SendValues that may not convert correctly to GEnum when
        // build() applies them.  Setting on the built element invokes
        // GStreamer's full g_object_set_property() path which handles
        // string→GEnum conversion correctly.
        //
        // v4l2h264dec io-mode enum values:
        //   0 = auto, 1 = rw, 2 = mmap, 3 = dmabuf, 4 = dmabuf-import
        v4l2dec.set_property_from_str("output-io-mode", "dmabuf");
        v4l2dec.set_property_from_str("capture-io-mode", "dmabuf");

        // NOTE: We intentionally do NOT read back the io-mode properties to
        // verify them.  The GstV4l2IOMode type is a GEnum registered as its
        // own GType — GLib's type system refuses to cast it to gint (i32) or
        // gchararray (String), causing a panic:
        //   "Value type mismatch. Actual GstV4l2IOMode, requested gint"
        // set_property_from_str() succeeds (it uses g_object_set_property()
        // which handles string→GEnum conversion), so the values ARE set.
        // Verification that zero-copy is working comes from:
        //   1. Pipeline FPS (should be ~25fps, not 10-15fps)
        //   2. GStreamer debug log: GST_V4L2="3" shows "DMABUF capture"
        //   3. No "A lot of buffers are being dropped" warnings
        tracing::info!(
            "v4l2h264dec: output-io-mode=dmabuf, capture-io-mode=dmabuf (zero-copy path enabled)"
        );

        let kmssink = Self::build_kmssink(config)?;

        let bin = gstreamer::Bin::new();
        bin.add_many([&video_queue, &h264parse, &v4l2dec, &kmssink]).map_err(|e| {
            PlaybackError::PipelineCreation(format!("add video elements to bin: {}", e))
        })?;

        Element::link_many([&video_queue, &h264parse, &v4l2dec, &kmssink]).map_err(|e| {
            PlaybackError::PipelineCreation(format!(
                "link video_queue→h264parse→v4l2h264dec→kmssink: {}",
                e
            ))
        })?;

        // Create ghost pads for the bin (on the queue element, which is the entry point).
        let sink_pad = video_queue.static_pad("sink").expect("video_queue should have a sink pad");
        bin.add_pad(&gstreamer::GhostPad::with_target(&sink_pad).expect("create video ghost pad"))
            .map_err(|e| PlaybackError::PipelineCreation(format!("video ghost pad: {}", e)))?;

        let bin_element: Element =
            bin.dynamic_cast::<Element>().expect("bin to element cast should succeed");

        Ok((bin_element, kmssink))
    }

    /// Build the hardware-accelerated HEVC video branch:
    /// `h265parse → v4l2slh265dec(dmabuf) → [V3D compute: SAND→NV12] → kmssink`
    ///
    /// This pipeline branch handles HEVC/H.265 hardware decoding using the
    /// Raspberry Pi's dedicated HEVC decoder block (rpivid driver). The HEVC
    /// decoder outputs decoded frames in Broadcom's SAND128 column-tiled format
    /// (V4L2 `NV12_COL128`), which is incompatible with the HVS for direct
    /// scanout. The V3D GPU compute shader converts SAND128→NV12 in near-zero-
    /// copy fashion: the pixel data moves from the HEVC decoder's DMA-BUF to
    /// the GPU for format transformation and then to a new DMA-BUF for display,
    /// but the CPU never touches the pixel data.
    ///
    /// ## Pipeline Topology
    ///
    /// ```text
    /// ┌──────────┐    ┌───────────────┐    ┌──────────────────┐    ┌────────────┐
    /// │ h265parse│───►│v4l2slh265dec  │───►│ V3D Compute      │───►│  kmssink   │
    /// │          │    │(SAND128 dmabuf)│    │ SAND128→NV12     │    │(NV12 dmabuf│
    /// └──────────┘    └───────────────┘    └──────────────────┘    └────────────┘
    /// ```
    ///
    /// ## SAND128→NV12 Conversion
    ///
    /// The V3D compute shader is the key innovation: instead of using the CPU
    /// or the bcm2835-ISP hardware for format conversion (both of which
    /// require CPU involvement), the GPU directly reads the SAND128 data from
    /// the decoder's DMA-BUF, performs the column-to-linear address remapping
    /// in a GLSL ES 3.1 compute shader, and writes the NV12 output into a
    /// second DMA-BUF that the HVS can scan out. This is "near-zero-copy"
    /// because:
    ///
    /// - The HEVC decoder writes SAND128 pixels into CMA DMA-BUF #1
    /// - The V3D GPU reads DMA-BUF #1, converts to NV12, writes DMA-BUF #2
    /// - The HVS reads DMA-BUF #2 for HDMI scanout
    /// - The CPU is never in the pixel data path
    ///
    /// ## Integration Strategy
    ///
    /// For the initial implementation, the V3D conversion is integrated via
    /// GStreamer's `appsink`/`appsrc` elements:
    ///
    /// 1. `v4l2slh265dec` outputs SAND128 buffers via `appsink` (pull mode)
    /// 2. Each buffer's DMA-BUF fd is extracted and passed to the V3D engine
    /// 3. The V3D engine dispatches the compute shader for conversion
    /// 4. The output NV12 DMA-BUF fd is pushed into `appsrc`
    /// 5. `appsrc` feeds the NV12 data to `kmssink` for scanout
    ///
    /// This approach requires an extra buffer copy between appsink and appsrc,
    /// but the GPU conversion is the critical path, and the copy overhead is
    /// negligible compared to the SAND→NV12 conversion work. A future
    /// optimization would use a custom GStreamer element or pad probe to
    /// intercept and transform buffers in-place within the pipeline.
    #[cfg(feature = "hevc")]
    #[allow(dead_code)]
    fn build_hevc_video_bin(
        config: &PipelineConfig,
    ) -> Result<(Element, Element, Option<V3dComputeEngine>), PlaybackError> {
        let video_queue = ElementFactory::make("queue")
            .property("max-size-buffers", 200u32)
            .property("max-size-time", 5_000_000_000u64)
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("hevc video_queue: {}", e)))?;

        // h265parse ensures the stream is properly framed for V4L2 stateless
        // decode. The stateless HEVC decoder requires properly delimited NALUs
        // with SPS/PPS/VPS prepended to IDR frames.
        let h265parse = ElementFactory::make("h265parse")
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("h265parse: {}", e)))?;

        // V4L2 stateless HEVC decoder using rpivid on Pi 4.
        // This element uses the V4L2 Request API (stateless decode), where
        // the application must provide per-slice control parameters alongside
        // the compressed data. GStreamer's v4l2slh265dec handles this
        // internally.
        //
        // IMPORTANT: Unlike the stateful decoder (v4l2h264dec), the stateless
        // decoder does NOT have `output-io-mode` or `capture-io-mode` properties.
        // The stateless decoder (GstV4l2Decoder base class) manages DMA-BUF
        // allocation internally via the V4L2 Request API. It automatically
        // negotiates the best I/O mode (typically dmabuf when the downstream
        // element supports it). Setting those properties would panic at runtime
        // with "property 'output-io-mode' of type 'v4l2slh265dec' not found".
        //
        // The stateless decoder's available properties are:
        //   - `device` (string): V4L2 device path (auto-detected if not set)
        //   - `extra-controls` (GstStructure): Extra V4L2 controls per request
        //   - `min-queued` (uint): Minimum buffers to queue before decode starts
        //
        // Output format: NV12_COL128 (SAND128) — Broadcom's column-tiled
        // format that is INCOMPATIBLE with the HVS for direct scanout.
        // The V3D compute shader (or bcm2835-ISP) will convert SAND128→NV12.
        let v4l2h265dec = ElementFactory::make("v4l2slh265dec")
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("v4l2slh265dec: {}", e)))?;

        // The stateless decoder auto-negotiates DMA-BUF I/O mode internally.
        // No explicit output-io-mode / capture-io-mode setting needed
        // (and attempting to set them would panic — they don't exist on
        // GstV4l2Decoder-based elements).
        tracing::info!(
            "v4l2slh265dec: stateless decoder created (DMA-BUF auto-negotiated by GstV4l2Decoder)"
        );

        // ── V3D Compute Shader Engine ──────────────────────────────
        //
        // Initialize the V3D compute engine for SAND→NV12 conversion.
        // The engine creates an EGL context on the V3D GPU, compiles the
        // compute shader, and manages the SSBO resources.
        //
        // If V3D is not available, we fall back to using the bcm2835-ISP
        // hardware converter via the v4l2convert GStreamer element.
        let mut v3d_engine = None;

        let drm_fd = config.drm_fd.unwrap_or(-1);
        if V3dComputeEngine::is_available() {
            match V3dComputeEngine::new(drm_fd) {
                Ok(engine) => {
                    tracing::info!(
                        "V3D compute engine initialized — SAND→NV12 near-zero-copy conversion enabled"
                    );
                    v3d_engine = Some(engine);
                },
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "V3D compute engine init failed — falling back to bcm2835-ISP hardware conversion"
                    );
                },
            }
        } else {
            tracing::info!("V3D compute not available — using bcm2835-ISP for SAND→NV12");
        }

        // ── Conversion path selection ──────────────────────────────
        //
        // If V3D compute is available: use appsink/appsrc with GPU conversion
        // If V3D is not available: use v4l2convert (bcm2835-ISP hardware)
        //
        // The v4l2convert path is simpler but requires CPU involvement for
        // buffer management. The V3D path is near-zero-copy.

        let kmssink = Self::build_kmssink(config)?;

        let bin = gstreamer::Bin::new();

        if v3d_engine.is_some() {
            // ── V3D compute shader path ────────────────────────────
            //
            // Pipeline: h265parse → v4l2slh265dec → appsink
            //                                              ↓ (SAND128 DMA-BUF)
            //                                        V3dComputeEngine
            //                                              ↓ (NV12 DMA-BUF)
            //                                        appsrc → kmssink

            // appsink caps: accept SAND128 (NV12_64Z32) output from the
            // stateless HEVC decoder. The format NV12_64Z32 is the DRM/GStreamer
            // fourcc for Broadcom's SAND128 column-tiled NV12. We do NOT set
            // an explicit width/height — the decoder will negotiate these during
            // the PAUSED→PLAYING state transition.
            //
            // NOTE: Do NOT add non-standard fields like "interop=sand128" to the
            // caps — they are not recognized by GStreamer's caps negotiation and
            // will cause "not-negotiated" errors.
            let appsink = ElementFactory::make("appsink")
                .property("emit-signals", true)
                .property("max-buffers", 2u32)
                .property("drop", false) // Never drop buffers — causes visual artifacts
                .property_from_str("caps", "video/x-raw,format=NV12_64Z32")
                .build()
                .map_err(|e| PlaybackError::PipelineCreation(format!("hevc appsink: {}", e)))?;

            let appsrc = ElementFactory::make("appsrc")
                .property_from_str("format", "time") // GST_FORMAT_TIME
                .property_from_str("caps", "video/x-raw,format=NV12")
                .property_from_str("stream-type", "stream") // GST_APP_STREAM_TYPE_STREAM
                .build()
                .map_err(|e| PlaybackError::PipelineCreation(format!("hevc appsrc: {}", e)))?;

            bin.add_many([&video_queue, &h265parse, &v4l2h265dec, &appsink, &appsrc, &kmssink])
                .map_err(|e| {
                    PlaybackError::PipelineCreation(format!("add HEVC video elements: {}", e))
                })?;

            Element::link_many([&video_queue, &h265parse, &v4l2h265dec, &appsink]).map_err(
                |e| PlaybackError::PipelineCreation(format!("link HEVC decode chain: {}", e)),
            )?;

            Element::link_many([&appsrc, &kmssink]).map_err(|e| {
                PlaybackError::PipelineCreation(format!("link HEVC display chain: {}", e))
            })?;

            tracing::info!(
                "HEVC video chain: video_queue → h265parse → v4l2slh265dec(SAND128) → appsink → [V3D compute] → appsrc → kmssink"
            );
        } else {
            // ── ISP fallback path ──────────────────────────────────
            //
            // Pipeline: h265parse → v4l2slh265dec → v4l2convert(ISP) → kmssink
            //
            // The v4l2convert element uses the bcm2835-ISP hardware at
            // /dev/video12 to convert SAND128→NV12. This is NOT zero-copy
            // (the ISP reads from the decoder's DMA-BUF and writes to a
            // new DMA-BUF), but the conversion happens in dedicated hardware
            // without CPU pixel processing.

            let v4l2convert = ElementFactory::make("v4l2convert")
                .property_from_str("output-io-mode", "dmabuf")
                .property_from_str("capture-io-mode", "dmabuf")
                .build()
                .map_err(|e| {
                    PlaybackError::PipelineCreation(format!("v4l2convert (ISP): {}", e))
                })?;

            bin.add_many([&video_queue, &h265parse, &v4l2h265dec, &v4l2convert, &kmssink])
                .map_err(|e| {
                    PlaybackError::PipelineCreation(format!("add HEVC ISP video elements: {}", e))
                })?;

            Element::link_many([&video_queue, &h265parse, &v4l2h265dec, &v4l2convert, &kmssink])
                .map_err(|e| {
                    PlaybackError::PipelineCreation(format!("link HEVC ISP video chain: {}", e))
                })?;

            tracing::info!(
                "HEVC video chain (ISP fallback): video_queue → h265parse → v4l2slh265dec → v4l2convert(ISP) → kmssink"
            );
        }

        let sink_pad = video_queue.static_pad("sink").expect("video_queue should have a sink pad");
        bin.add_pad(
            &gstreamer::GhostPad::with_target(&sink_pad).expect("create HEVC video ghost pad"),
        )
        .map_err(|e| PlaybackError::PipelineCreation(format!("HEVC video ghost pad: {}", e)))?;

        let bin_element: Element =
            bin.dynamic_cast::<Element>().expect("bin to element cast should succeed");

        Ok((bin_element, kmssink, v3d_engine))
    }

    /// Build the software-decode video branch:
    /// `avdec_h264 → videoconvert → kmssink`
    #[allow(dead_code)]
    fn build_sw_video_bin(config: &PipelineConfig) -> Result<(Element, Element), PlaybackError> {
        let video_queue = ElementFactory::make("queue")
            .property("max-size-buffers", 200u32)
            .property("max-size-time", 5_000_000_000u64)
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("sw video_queue: {}", e)))?;

        let avdec = ElementFactory::make("avdec_h264")
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("avdec_h264: {}", e)))?;

        let vconv = ElementFactory::make("videoconvert")
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("videoconvert: {}", e)))?;

        let mut kmssink_builder =
            ElementFactory::make(&config.video_sink).property("can-scale", true);
        if let Some(drm_fd) = config.drm_fd {
            if drm_fd >= 0 {
                kmssink_builder = kmssink_builder.property("fd", drm_fd);
            } else {
                kmssink_builder = kmssink_builder.property_from_str("driver-name", "vc4");
            }
        } else {
            kmssink_builder = kmssink_builder.property_from_str("driver-name", "vc4");
        }

        // Only set plane-id if explicitly configured (> 0).
        if config.plane_id > 0 {
            kmssink_builder = kmssink_builder.property("plane-id", config.plane_id as i32);
        }

        let kmssink = kmssink_builder.build().map_err(|e| {
            PlaybackError::PipelineCreation(format!("{}: {}", config.video_sink, e))
        })?;

        let bin = gstreamer::Bin::new();
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

    /// Start the pipeline by transitioning to Paused (preroll).
    ///
    /// This is the first phase of the two-phase startup sequence:
    ///   1. `preroll()` — set to Paused, wait for sinks to receive first buffer
    ///   2. `start_playing()` — set to Playing once preroll completes
    ///
    /// With kmssink using async=true (the default), GStreamer waits for
    /// the first decoded video frame before completing the Paused transition.
    /// This ensures the pipeline clock doesn't start until real data is
    /// flowing, preventing the "late buffers" problem where frames are
    /// dropped because their timestamps are behind the running clock.
    ///
    /// The PlaybackEngine's bus watch automatically transitions from
    /// Paused to Playing when preroll completes.
    pub fn preroll(&mut self) -> Result<(), PlaybackError> {
        // Set to Paused (not Playing). GStreamer will:
        //   1. Start fetching data from souphttpsrc
        //   2. Parsebin will demux and emit source pads
        //   3. Pad-added handler links pads to video/audio chains
        //   4. Data flows through decoders to sinks
        //   5. kmssink (async=true) prerolls on the first video frame
        //   6. Pipeline completes the Paused transition
        //
        // Then the bus watch auto-transitions to Playing.
        let result = self.pipeline.set_state(State::Paused);
        match result {
            Ok(gstreamer::StateChangeSuccess::Success) => {
                tracing::info!("pipeline transitioned to Paused synchronously (preroll complete)");
            },
            Ok(gstreamer::StateChangeSuccess::Async) => {
                tracing::info!(
                    "pipeline state change to Paused accepted (async — \
                     GStreamer is connecting to CDN and prerolling)"
                );
            },
            Ok(gstreamer::StateChangeSuccess::NoPreroll) => {
                tracing::info!("pipeline transitioned to Paused (no-preroll / live source)");
            },
            Err(e) => {
                let msg = format!("set_state Paused failed: {}", e);
                tracing::error!(%msg);

                // Try to get a more specific error from the bus.
                if let Some(bus) = self.pipeline.bus() {
                    while let Some(bus_msg) =
                        bus.timed_pop(gstreamer::ClockTime::from_mseconds(500))
                    {
                        match bus_msg.view() {
                            gstreamer::MessageView::Error(err) => {
                                tracing::error!(
                                    error = %err.error(),
                                    debug = ?err.debug(),
                                    "GStreamer error during set_state Paused"
                                );
                                return Err(PlaybackError::Gstreamer(format!(
                                    "{} — {}",
                                    e,
                                    err.error()
                                )));
                            },
                            gstreamer::MessageView::Warning(w) => {
                                tracing::warn!(
                                    warning = %w.error(),
                                    "GStreamer warning during set_state Paused"
                                );
                            },
                            _ => {},
                        }
                    }
                }

                return Err(PlaybackError::Gstreamer(msg));
            },
        }

        self.state = PipelineState::Paused;
        Ok(())
    }

    /// Transition the pipeline from Paused to Playing.
    ///
    /// Should only be called after preroll completes (the pipeline has
    /// reached the Paused state with all async sinks having received
    /// their first buffer).  This starts the pipeline clock and begins
    /// real-time playback.
    ///
    /// Also used by `resume()` to un-pause a user-paused pipeline.
    pub fn start_playing(&mut self) -> Result<(), PlaybackError> {
        let result = self.pipeline.set_state(State::Playing);
        match result {
            Ok(gstreamer::StateChangeSuccess::Success) => {
                tracing::info!("pipeline transitioned to Playing");
            },
            Ok(gstreamer::StateChangeSuccess::Async) => {
                tracing::info!("pipeline Paused→Playing accepted (async)");
            },
            Ok(gstreamer::StateChangeSuccess::NoPreroll) => {
                tracing::info!("pipeline transitioned to Playing (no-preroll)");
            },
            Err(e) => {
                return Err(PlaybackError::Gstreamer(format!("set_state Playing: {}", e)));
            },
        }

        self.state = PipelineState::Playing;
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
        self.start_playing()
    }

    /// Stop the pipeline and release resources.
    pub fn stop(&mut self) -> Result<(), PlaybackError> {
        // Signal the appsrc push task to stop downloading.
        if let Some(cancel) = self.push_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
            tracing::debug!("signalled appsrc push task to cancel");
        }

        // Send EOS to allow elements to flush.
        let _ = self.pipeline.send_event(gstreamer::event::Eos::new());

        self.pipeline
            .set_state(State::Null)
            .map_err(|e| PlaybackError::Gstreamer(format!("set_state Null: {}", e)))?;
        self.state = PipelineState::Null;
        tracing::debug!("pipeline stopped and set to Null");
        Ok(())
    }

    /// Get the current download progress from the StreamSource.
    pub fn download_progress(&self) -> DownloadProgress {
        self.download_progress.snapshot()
    }

    /// Cancel the active CDN download and appsrc push task.
    pub fn cancel_download(&mut self) {
        if let Some(cancel) = self.push_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
            tracing::info!("download cancelled via cancel_download()");
        }
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
        let mut query = gstreamer::query::Buffering::new(gstreamer::Format::Time);
        if self.pipeline.query(&mut query) {
            let (busy, percent) = query.percent();
            let percent = percent.clamp(0, 100) as u8;
            let _stats = query.stats();
            BufferHealth {
                fill_percent: percent,
                buffered_seconds: 0.0, // Approximated from fill_percent
                estimated_fill_ms: None,
                is_buffering: busy || percent < 100,
            }
        } else {
            BufferHealth::default()
        }
    }

    /// Return a reference to the GStreamer pipeline for bus watch setup.
    pub fn pipeline(&self) -> &Pipeline {
        &self.pipeline
    }

    /// Retain the bus watch guard so GStreamer keeps dispatching messages.
    pub fn set_bus_watch(&mut self, guard: gstreamer::bus::BusWatchGuard) {
        self.bus_watch = Some(guard);
    }

    /// Return whether the stream is rate-limited by the CDN (sp= parameter).
    ///
    /// When true, the bus watch should use more aggressive buffering
    /// thresholds (start at 95%, pause at <5%, resume at >=95%) to
    /// minimise rebuffer pauses on rate-limited streams.
    pub fn is_rate_limited(&self) -> bool {
        self.is_rate_limited
    }

    /// Return the current pipeline state.
    pub fn state(&self) -> PipelineState {
        self.state
    }

    /// Attempt to rebuild the pipeline with software decode.
    ///
    /// Called when V4L2 hardware decode fails to negotiate.
    pub async fn rebuild_sw(
        &mut self,
        url: &str,
        source_url: &str,
        _socks_addr: &str,
        _isolation_username: &str,
        config: &PipelineConfig,
        cookies: &[String],
    ) -> Result<(), PlaybackError> {
        tracing::warn!("rebuilding pipeline with software decode fallback");

        // Stop the current pipeline.
        self.stop()?;

        // Build a new pipeline with HW accel disabled.
        let mut sw_config = config.clone();
        sw_config.hw_accel = false;
        // SW decode doesn't have V4L2 pipeline latency, so ts-offset
        // compensation is not needed (and would cause A/V desync in
        // the opposite direction — audio delayed behind video).
        sw_config.audio_ts_offset_ns = 0;
        let new = Self::new(url, source_url, _socks_addr, _isolation_username, &sw_config, cookies)
            .await?;

        // Replace self with the new pipeline.
        *self = new;
        self.preroll()?;

        Ok(())
    }
}

impl Drop for GstPipeline {
    fn drop(&mut self) {
        // Ensure the pipeline is set to NULL before dropping.
        // Without this, GStreamer prints "Trying to dispose element X,
        // but it is in READY/PAUSED instead of the NULL state" warnings
        // when a failed pipeline is dropped.

        // Signal the appsrc push task to stop (if running).
        if let Some(cancel) = &self.push_cancel {
            cancel.store(true, Ordering::Relaxed);
        }

        if self.state != PipelineState::Null {
            // Drop the bus watch first to prevent callbacks during shutdown.
            self.bus_watch = None;
            let _ = self.pipeline.set_state(State::Null);
        }
    }
}
