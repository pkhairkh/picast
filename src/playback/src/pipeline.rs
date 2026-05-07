#![cfg(feature = "hw")]
//! PiCast GStreamer Pipeline Construction
//!
//! Builds and manages the GStreamer pipeline for H.264 video playback
//! with V4L2 hardware decode and direct DRM/KMS output on Raspberry Pi 4B+.
//!
//! ## Pipeline Topology
//!
//! ```text
//! ┌──────────┐    ┌────────┐    ┌──────────┐    ┌───────┐    ┌──────────┐    ┌────────────────────┐    ┌──────────────┐
//! │souphttpsrc│───►│queue2  │───►│parsebin  │──┬►│queue  │───►│h264parse │───►│v4l2h264dec(dmabuf)│───►│kmssink       │
//! │(SOCKS5h) │    │(buffer)│    │(demux)   │  │ │       │    │          │    │(zero-copy HW dec) │    │(DRM/KMS,     │
//! └──────────┘    └────────┘    └──────────┘  │ └───────┘    └──────────┘    └────────────────────┘    │ max-lateness) │
//!                                               │                                                         └──────────────┘
//!                                               │ ┌───────┐    ┌──────────────┐    ┌──────────────┐    ┌────────┐    ┌─────────────────────┐
//!                                               └►│queue  │───►│audioconvert  │───►│audioresample │───►│volume │───►│alsasink             │
//!                                                 │       │    │              │    │              │    │        │    │(device=plughw:C,D)  │
//!                                                 └───────┘    └──────────────┘    └──────────────┘    └────────┘    └─────────────────────┘
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

use crate::{BufferHealth, PipelineConfig, PlaybackError};
use gstreamer::prelude::*;
use gstreamer::{Element, ElementFactory, Pipeline, State};

/// Ensure GStreamer is initialised exactly once.
static GST_INIT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();

/// Initialise GStreamer. Safe to call multiple times.
/// Returns an error if initialisation fails (instead of panicking),
/// and subsequent calls will return the same error.
fn ensure_gst_init() -> Result<(), PlaybackError> {
    match GST_INIT.get_or_init(|| {
        match gstreamer::init() {
            Ok(()) => {
                tracing::debug!("GStreamer initialised successfully");
                Ok(())
            },
            Err(e) => {
                let message = format!("GStreamer init failed (permanent): {}", e);
                tracing::error!("{}", message);
                Err(message)
            },
        }
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
    /// The `socks_addr` parameter is retained for API compatibility but
    /// proxy routing is now handled via `config.tor_http_proxy` (Tor's
    /// HTTP CONNECT proxy) instead of SOCKS5, because souphttpsrc does
    /// not support SOCKS5 proxy URIs with libsoup2.4.
    ///
    /// The `isolation_username` is retained for API compatibility.
    pub fn new(
        url: &str,
        _socks_addr: &str,
        _isolation_username: &str,
        config: &PipelineConfig,
    ) -> Result<Self, PlaybackError> {
        ensure_gst_init()?;

        let pipeline = Pipeline::new();

        // ── Source element ──────────────────────────────────────────
        //
        // A browser-like User-Agent is critical: many video CDNs (Voe,
        // DoodStream, Cloudflare-fronted hosts) reject requests with the
        // default "GStreamer souphttpsrc" UA, returning 403 or closing the
        // connection.  The same UA string is used by the custom resolvers
        // in picast-resolver so the CDN sees a consistent identity across
        // both the resolution and playback phases.
        const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

        let src = ElementFactory::make("souphttpsrc")
            .property("location", url)
            .property("timeout", 30u32)
            .property("user-agent", BROWSER_UA)
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("souphttpsrc: {}", e)))?;

        // Set extra HTTP headers.  Many video CDNs require a Referer header
        // for hotlink protection — they check that the request originates
        // from the embedding page's domain.  We set the Referer to the
        // URL's own origin so the CDN sees a "same-origin" request.
        // Accept and Accept-Language headers make the request look more
        // browser-like, which helps with CDNs that reject GStreamer's
        // default headers.
        if src.find_property("extra-headers").is_some() {
            let mut headers = gstreamer::Structure::new_empty("extra-headers");
            headers.set("Accept", "video/webm,video/mp4,video/*;q=0.9,application/ogg,*/*;q=0.7");
            headers.set("Accept-Language", "en-US,en;q=0.5");
            // Derive Referer from the URL's origin (scheme://host).
            if let Ok(parsed) = url::Url::parse(url) {
                if let Some(host) = parsed.host_str() {
                    let referer = format!("{}://{}", parsed.scheme(), host);
                    headers.set("Referer", referer.as_str());
                    tracing::debug!(referer = %referer, "souphttpsrc: Referer header set");
                }
            }
            src.set_property("extra-headers", &headers);
            tracing::debug!("souphttpsrc: extra-headers configured");
        }

        // Configure Tor proxy for media routing. ALL non-loopback
        // traffic goes through Tor — this is a core security property
        // of PiCast. Privacy is non-negotiable: the user's ISP must
        // never see which content is being accessed.
        //
        // souphttpsrc's `proxy` property only supports HTTP proxy URIs.
        // libsoup2.4 (Debian Bookworm) does NOT support `socks5h://`
        // proxy URIs, and `socks5-proxy-ip`/`socks5-proxy-port` properties
        // do not exist on souphttpsrc (they are on `tcpclientsrc`, a
        // different element).  The only reliable way to route souphttpsrc
        // through Tor is via an HTTP CONNECT proxy provided by Tor's
        // HTTPTunnelPort (requires `HTTPTunnelPort 9080` in torrc).
        //
        // The HTTP CONNECT proxy handles DNS resolution through Tor (the
        // hostname is sent in the CONNECT request, not resolved locally),
        // preventing DNS leaks.
        let is_loopback_url = url.starts_with("http://127.0.0.1:")
            || url.starts_with("http://localhost:")
            || url.starts_with("http://[::1]:");

        if !is_loopback_url && !config.tor_http_proxy.is_empty() {
            if src.find_property("proxy").is_some() {
                src.set_property("proxy", &config.tor_http_proxy);
                tracing::info!(
                    proxy = %config.tor_http_proxy,
                    "HTTP CONNECT proxy configured on souphttpsrc (all traffic through Tor via HTTPTunnelPort)"
                );
            } else {
                tracing::warn!(
                    "souphttpsrc has no 'proxy' property; cannot route media through Tor"
                );
            }
        } else if is_loopback_url {
            tracing::debug!("loopback media URL detected; skipping playback proxy");
        } else if config.tor_http_proxy.is_empty() {
            tracing::warn!(
                "no Tor HTTP proxy configured — media will be fetched directly (NOT through Tor). \
                 Add 'HTTPTunnelPort 9080' to /etc/tor/torrc and restart Tor."
            );
        }

        // ── Buffer element ──────────────────────────────────────────
        //
        // queue2 sits between souphttpsrc and parsebin and provides a
        // download buffer for network resilience.  The key configuration
        // decision is `use-buffering`:
        //
        // With `use-buffering=true`, queue2 blocks the pipeline from
        // prerolling until the buffer reaches `high-percent`.  This
        // requires the application to handle BUFFERING messages on the
        // bus and pause/resume the pipeline accordingly.  Without proper
        // handling, preroll blocks forever (the state change from Ready
        // to Paused never completes), and video never starts playing.
        //
        // With `use-buffering=false` (our choice), queue2 acts as a
        // simple data queue — preroll completes as soon as the first
        // buffer arrives from souphttpsrc, and the pipeline starts
        // playing immediately.  If the network is slower than the
        // playback rate, the queue may run empty and cause brief stalls,
        // but this is preferable to never starting at all.
        let queue2 = ElementFactory::make("queue2")
            .property("max-size-bytes", 50_000_000u32) // 50 MB
            .property("use-buffering", false)
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
        let audio_queue = ElementFactory::make("queue")
            .property("max-size-buffers", 200u32)
            .build()
            .map_err(|e| PlaybackError::PipelineCreation(format!("audio_queue: {}", e)))?;

        let audio_decoder = ElementFactory::make("avdec_aac")
            .build()
            .or_else(|_| ElementFactory::make("fdkaacdec").build())
            .map_err(|e| {
                tracing::warn!("no AAC decoder available (avdec_aac or fdkaacdec) — audio will be disabled: {}", e);
                // Non-fatal: we'll add a fakesink instead
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

        // audio sink (alsasink) — with configurable ALSA device.
        //
        // Previous versions used async=false to prevent alsasink from
        // blocking preroll, but this caused clock/sync issues: audio
        // buffers could be scheduled incorrectly relative to the pipeline
        // clock.  Now both kmssink and alsasink use the default async=true,
        // so the pipeline waits for both sinks to preroll before starting
        // the clock, ensuring audio and video are properly synchronised.
        //
        // The `device` property routes audio to a specific ALSA output
        // (e.g. "plughw:1,0" for HDMI).  When empty, ALSA's default
        // device is used.  Use `plughw` (not `hw`) because plughw allows
        // ALSA to convert formats — HDMI devices may not accept the exact
        // F32LE format that GStreamer negotiates.
        let mut audiosink_builder = ElementFactory::make(&config.audio_sink);
        if !config.audio_device.is_empty() {
            audiosink_builder = audiosink_builder.property("device", &config.audio_device);
            tracing::info!(
                device = %config.audio_device,
                "alsasink: using explicit ALSA device"
            );
        } else {
            tracing::info!("alsasink: using ALSA default device (no device property set)");
        }
        let audiosink = audiosink_builder
            .build()
            .map_err(|e| {
                PlaybackError::PipelineCreation(format!("{}: {}", config.audio_sink, e))
            })?;

        // ── Assemble pipeline ───────────────────────────────────────
        let mut all_elements: Vec<&Element> = vec![
            &src, &queue2, &parsebin, &video_bin,
            &audio_queue, &audioconvert, &audioresample, &volume, &audiosink,
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
            Element::link_many([&audio_queue, dec, &audioconvert, &audioresample, &volume, &audiosink])
                .map_err(|e| PlaybackError::PipelineCreation(format!("link audio chain (with decoder): {}", e)))?;
            tracing::info!("audio chain: audio_queue → avdec_aac → audioconvert → audioresample → volume → alsasink");
        } else {
            Element::link_many([&audio_queue, &audioconvert, &audioresample, &volume, &audiosink])
                .map_err(|e| PlaybackError::PipelineCreation(format!("link audio chain (no decoder): {}", e)))?;
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
        //      video bin first (most common case for unknown pads) and
        //      fall back to audio if that fails.
        let video_bin_weak = video_bin.downgrade();
        let audio_queue_weak = audio_queue.downgrade();
        let pipeline_weak = pipeline.downgrade();

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
                match video_bin_weak.upgrade() {
                    Some(vbin) => {
                        let sink_pad =
                            vbin.static_pad("sink").expect("video bin should have a sink pad");
                        if sink_pad.is_linked() {
                            tracing::info!("video pad already linked, skipping");
                            return;
                        }
                        if let Err(e) = pad.link(&sink_pad) {
                            tracing::error!(
                                error = ?e,
                                caps = %caps_str,
                                "failed to link parsebin video pad"
                            );
                        } else {
                            tracing::info!(
                                caps = %caps_str,
                                "linked parsebin → video bin"
                            );
                        }
                    },
                    None => {
                        tracing::error!(
                            "video_bin_weak.upgrade() failed — video bin was dropped, video pad will be unlinked!"
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
                // available yet.  Try to link it to the video bin first
                // (the most common case for uncategorised pads), and if
                // that fails, try the audio chain.
                tracing::warn!(
                    pad = %pad_name,
                    media_type = ?media_type,
                    caps = %caps_str,
                    "parsebin pad with unrecognised media type — attempting to link as video first, then audio"
                );

                // Try video bin
                let mut linked = false;
                if let Some(vbin) = video_bin_weak.upgrade() {
                    let sink_pad =
                        vbin.static_pad("sink").expect("video bin should have a sink pad");
                    if !sink_pad.is_linked() {
                        match pad.link(&sink_pad) {
                            Ok(_) => {
                                tracing::info!("linked unknown pad → video bin (heuristic)");
                                linked = true;
                            },
                            Err(e) => {
                                tracing::warn!(
                                    error = ?e,
                                    "unknown pad failed to link as video — trying audio"
                                );
                            },
                        }
                    }
                }

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

        Ok(Self { pipeline, _video_sink: video_sink, volume, state: PipelineState::Ready, bus_watch: None })
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
    /// most important property for PiCast's performance on Pi 4 —
    /// without it, `mmap` mode forces frames through system memory,
    /// which causes high CPU usage and frequent dropped buffers.
    fn build_hw_video_bin(config: &PipelineConfig) -> Result<(Element, Element), PlaybackError> {
        let video_queue = ElementFactory::make("queue")
            .property("max-size-buffers", 60u32)
            .build()
            .map_err(|e| {
                PlaybackError::PipelineCreation(format!("video_queue: {}", e))
            })?;

        // h264parse ensures the stream is properly framed for V4L2 decode.
        let h264parse = ElementFactory::make("h264parse")
            .build()
            .map_err(|e| {
                PlaybackError::PipelineCreation(format!("h264parse: {}", e))
            })?;

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
        let v4l2dec = ElementFactory::make("v4l2h264dec")
            .build()
            .map_err(|e| {
                PlaybackError::PipelineCreation(format!(
                    "v4l2h264dec (may not be available): {}",
                    e
                ))
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

        // Build kmssink.  The fd and driver-name properties are mutually
        // exclusive in kmssink: setting both causes a warning "Can't set
        // fd... driver-name already set" and fd is silently ignored.
        // Strategy: if we have a valid DRM fd from the DisplayManager,
        // use it (and skip driver-name).  Otherwise, use driver-name to
        // let kmssink find and open the device itself.
        let mut kmssink_builder = ElementFactory::make(&config.video_sink)
            .property("can-scale", true)
            .property("max-lateness", -1i64); // unlimited — never drop late buffers on Pi
        // NOTE: kmssink uses the default async=true (preroll-aware).
        //
        // Previous versions set async=false to avoid preroll stalls, but
        // this caused a clock-start race: the pipeline clock began running
        // before parsebin had emitted pads and buffers reached kmssink.
        // By the time video frames arrived (5-6 seconds later), their
        // timestamps (near 0) were already "late" against the running
        // clock, and kmssink dropped almost every frame ("A lot of
        // buffers are being dropped").
        //
        // With async=true (the GStreamer default), kmssink waits for the
        // first video frame before completing its Paused state change.
        // The pipeline only transitions to Playing after ALL async sinks
        // have prerolled, so the clock starts at the right time and
        // buffers are never late.
        //
        // The startup sequence is: set pipeline to Paused → wait for
        // kmssink to preroll on real data → auto-transition to Playing.

        if let Some(drm_fd) = config.drm_fd {
            if drm_fd >= 0 {
                // Use the pre-opened DRM fd.  Don't set driver-name —
                // kmssink will use our fd instead of opening the device.
                kmssink_builder = kmssink_builder.property("fd", drm_fd);
                tracing::info!(fd = drm_fd, "kmssink: using provided DRM fd (not setting driver-name)");
            } else {
                kmssink_builder = kmssink_builder.property_from_str("driver-name", "vc4");
                tracing::info!("kmssink: using driver-name=vc4 (no valid fd provided)");
            }
        } else {
            kmssink_builder = kmssink_builder.property_from_str("driver-name", "vc4");
            tracing::info!("kmssink: using driver-name=vc4 (no fd provided)");
        }

        // Only set plane-id if explicitly configured (> 0).
        // When plane-id is 0 (default), kmssink auto-detects the best
        // overlay plane, which is more reliable across kernel versions.
        if config.plane_id > 0 {
            kmssink_builder = kmssink_builder.property("plane-id", config.plane_id as i32);
            tracing::info!(plane_id = config.plane_id, "kmssink: using explicit plane-id");
        } else {
            tracing::info!("kmssink: auto-detecting overlay plane (plane-id not set)");
        }

        // Set connector-id if known — this ensures kmssink renders to
        // the correct HDMI output.  Without it, kmssink auto-detects
        // the first connected connector, which is usually correct but
        // can fail on multi-monitor setups.
        if let Some(conn_id) = config.connector_id {
            if conn_id > 0 {
                kmssink_builder = kmssink_builder.property("connector-id", conn_id as i32);
                tracing::info!(connector_id = conn_id, "kmssink: using explicit connector-id");
            }
        }

        let kmssink = kmssink_builder.build().map_err(|e| {
            PlaybackError::PipelineCreation(format!("{}: {}", config.video_sink, e))
        })?;

        let bin = gstreamer::Bin::new();
        bin.add_many([&video_queue, &h264parse, &v4l2dec, &kmssink]).map_err(|e| {
            PlaybackError::PipelineCreation(format!("add video elements to bin: {}", e))
        })?;

        Element::link_many([&video_queue, &h264parse, &v4l2dec, &kmssink]).map_err(|e| {
            PlaybackError::PipelineCreation(format!("link video_queue→h264parse→v4l2h264dec→kmssink: {}", e))
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
            .property("max-size-buffers", 60u32)
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

        let mut kmssink_builder = ElementFactory::make(&config.video_sink)
            .property("can-scale", true);
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
        _socks_addr: &str,
        _isolation_username: &str,
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
        if self.state != PipelineState::Null {
            // Drop the bus watch first to prevent callbacks during shutdown.
            self.bus_watch = None;
            let _ = self.pipeline.set_state(State::Null);
        }
    }
}
