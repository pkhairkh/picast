#![cfg(feature = "hw")]
//! PiCast GStreamer Pipeline Construction
//!
//! Builds and manages the GStreamer pipeline for H.264 video playback
//! with V4L2 hardware decode and direct DRM/KMS output on Raspberry Pi 4B+.
//!
//! ## Pipeline Topology
//!
//! ```text
//! ┌──────────┐    ┌────────┐    ┌──────────┐    ┌───────┐    ┌──────────┐    ┌───────────┐    ┌──────────┐    ┌─────────┐
//! │souphttpsrc│───►│queue2  │───►│parsebin  │──┬►│queue  │───►│h264parse │───►│capssetter │───►│v4l2h264dec│──►│kmssink  │
//! │(SOCKS5h) │    │(buffer)│    │(demux)   │  │ │       │    │          │    │(bt709)    │    │(HW decode)│   │(DRM/KMS)│
//! └──────────┘    └────────┘    └──────────┘  │ └───────┘    └──────────┘    └───────────┘    └──────────┘    └─────────┘
//!                                               │
//!                                               │ ┌───────┐    ┌──────────────┐    ┌──────────────┐    ┌────────┐    ┌─────────┐
//!                                               └►│queue  │───►│audioconvert  │───►│audioresample │───►│volume │───►│alsasink │
//!                                                 │       │    │              │    │              │    │        │    │(HDMI)   │
//!                                                 └───────┘    └──────────────┘    └──────────────┘    └────────┘    └─────────┘
//! ```
//!
//! The `capssetter` element overrides the H.264 stream colorimetry to bt709,
//! which prevents "not-negotiated" errors between v4l2h264dec and kmssink
//! caused by unusual VUI colorimetry values in some streams.
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

        // Configure SOCKS5h proxy if provided. The proxy is only used when:
        //   1. A proxy address is configured (Tor is available)
        //   2. The URL is NOT a loopback address
        //   3. The URL is an .onion address (requires Tor to reach)
        //
        // Clearnet CDN URLs are fetched DIRECTLY — routing multi-megabyte
        // video downloads through Tor is extremely slow (often < 100 KB/s)
        // and causes souphttpsrc to time out during preroll. The Tor proxy
        // is needed for URL *resolution* (bypassing Cloudflare, accessing
        // onion sites), but the resolved CDN media URL should be fetched
        // directly for acceptable throughput.
        let is_loopback_url = url.starts_with("http://127.0.0.1:")
            || url.starts_with("http://localhost:")
            || url.starts_with("http://[::1]:");
        let is_onion_url = url.contains(".onion/")
            || url.contains(".onion:")
            || url.ends_with(".onion");

        let use_proxy = !socks_addr.is_empty() && !is_loopback_url && is_onion_url;

        if use_proxy {
            let parts: Vec<&str> = socks_addr.split(':').collect();
            let (host, port) = if parts.len() == 2 {
                (parts[0], parts[1].parse::<u32>().unwrap_or(9050))
            } else {
                ("127.0.0.1", 9050u32)
            };
            if src.find_property("socks5-proxy-ip").is_some()
                && src.find_property("socks5-proxy-port").is_some()
            {
                src.set_property("socks5-proxy-ip", host);
                src.set_property("socks5-proxy-port", port);
                tracing::info!(
                    host = host,
                    port = port,
                    user = isolation_username,
                    "SOCKS5h proxy configured on souphttpsrc (onion URL)"
                );
            } else if src.find_property("proxy").is_some() {
                let proxy = format!("socks5h://{}@{}:{}", isolation_username, host, port);
                src.set_property("proxy", proxy);
                tracing::info!(
                    host = host,
                    port = port,
                    user = isolation_username,
                    "SOCKS proxy URI configured on souphttpsrc (onion URL)"
                );
            } else {
                tracing::warn!(
                    "souphttpsrc has no supported SOCKS proxy property; playback proxy not set"
                );
            }
        } else if is_loopback_url {
            tracing::debug!("loopback media URL detected; skipping playback proxy");
        } else if !is_onion_url && !socks_addr.is_empty() {
            tracing::info!(
                "clearnet media URL detected; fetching directly (not through Tor proxy) for performance"
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
            .property("max-size-bytes", 10_485_760u32) // 10 MB
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
            .property("max-size-buffers", 3u32)
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

        // audio sink (alsasink) — set async=false so it does NOT block the
        // pipeline's state change waiting for the first audio buffer to preroll.
        //
        // Without this, the pre-linked audio chain (audio_queue → decoder →
        // audioconvert → audioresample → volume → alsasink) blocks the entire
        // pipeline at Paused when parsebin hasn't linked an audio pad yet (no
        // audio stream, codec mismatch → fakesink fallback, or CDN 403).  GStreamer
        // requires ALL sinks to preroll before transitioning to Playing; alsasink
        // waiting for a buffer that never arrives keeps the whole pipeline stuck.
        //
        // With async=false, alsasink completes its Ready→Paused transition
        // immediately without waiting for data.  When audio buffers eventually
        // arrive, they play normally.  If no audio data ever arrives, alsasink
        // sits idle without blocking the video path.
        let audiosink = ElementFactory::make(&config.audio_sink)
            .property("async", false)
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
    /// `h264parse → capssetter(colorimetry=bt709) → v4l2h264dec(mmap) → kmssink`
    ///
    /// The colorimetry override via capssetter is critical on Raspberry Pi 4:
    /// the bcm2835-codec V4L2 decoder reports colorimetry from the H.264
    /// bitstream VUI parameters, and some streams (especially those from
    /// Apple encoders) use colorimetry values (e.g. 1:3:5:1) that
    /// GStreamer's caps system does not recognise, causing "not-negotiated"
    /// between v4l2h264dec and kmssink.  Forcing bt709 resolves this.
    ///
    /// We also use `capture-io-mode=mmap` instead of `dmabuf` because
    /// dmabuf adds `memory:DMABuf` caps features that kmssink may not
    /// properly negotiate on older GStreamer versions (< 1.22).
    fn build_hw_video_bin(config: &PipelineConfig) -> Result<(Element, Element), PlaybackError> {
        let video_queue = ElementFactory::make("queue")
            .property("max-size-buffers", 3u32)
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

        // capssetter overrides the colorimetry to bt709, which is the most
        // common colour space for H.264 content.  Without this, streams
        // with unusual VUI colorimetry (e.g. Apple's 1:3:5:1) cause
        // "not-negotiated" between v4l2h264dec and kmssink.
        let capssetter = ElementFactory::make("capssetter")
            .property_from_str("caps", "video/x-h264,colorimetry=bt709")
            .property("join", false)
            .property("replace", true)
            .build()
            .map_err(|e| {
                PlaybackError::PipelineCreation(format!("capssetter: {}", e))
            })?;

        // V4L2 hardware H.264 decoder using bcm2835-codec on Pi 4.
        // Use mmap mode — dmabuf mode adds memory:DMABuf caps features
        // that kmssink may not negotiate correctly.
        let v4l2dec = ElementFactory::make("v4l2h264dec")
            .property_from_str("capture-io-mode", "mmap")
            .build()
            .map_err(|e| {
                PlaybackError::PipelineCreation(format!(
                    "v4l2h264dec (may not be available): {}",
                    e
                ))
            })?;

        // Build kmssink.  The fd and driver-name properties are mutually
        // exclusive in kmssink: setting both causes a warning "Can't set
        // fd... driver-name already set" and fd is silently ignored.
        // Strategy: if we have a valid DRM fd from the DisplayManager,
        // use it (and skip driver-name).  Otherwise, use driver-name to
        // let kmssink find and open the device itself.
        let mut kmssink_builder = ElementFactory::make(&config.video_sink)
            .property("can-scale", true)
            // Set async=false so kmssink does NOT block the pipeline's
            // READY→PAUSED transition waiting for the first video frame
            // to be decoded and rendered.  Without this, kmssink stays
            // ASYNC during preroll and the entire pipeline gets stuck at
            // READY with pending=PLAYING — video never appears.
            //
            // With async=false, kmssink completes its state change
            // immediately, and when video frames eventually arrive from
            // v4l2h264dec, they render normally.  This is the same
            // approach used for alsasink (see audio sink construction
            // for the detailed rationale).
            .property("async", false);

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
        bin.add_many([&video_queue, &h264parse, &capssetter, &v4l2dec, &kmssink]).map_err(|e| {
            PlaybackError::PipelineCreation(format!("add video elements to bin: {}", e))
        })?;

        Element::link_many([&video_queue, &h264parse, &capssetter, &v4l2dec, &kmssink]).map_err(|e| {
            PlaybackError::PipelineCreation(format!("link video_queue→h264parse→capssetter→v4l2h264dec→kmssink: {}", e))
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

        let mut kmssink_builder = ElementFactory::make(&config.video_sink)
            .property("can-scale", true)
            .property("async", false);
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

    /// Transition the pipeline to Playing.
    ///
    /// Sets the pipeline directly to Playing. GStreamer internally goes
    /// through the Paused (preroll) state first, but we don't block
    /// waiting for preroll — the state change happens asynchronously.
    /// When the CDN is slow, preroll can take many seconds, and blocking
    /// would starve the tokio runtime (watchdog heartbeat, API handlers).
    ///
    /// Errors from caps negotiation or DRM failures surface on the GStreamer
    /// bus and are handled by the bus watch in PlaybackEngine.
    pub fn play(&mut self) -> Result<(), PlaybackError> {
        // Go directly to Playing. GStreamer will internally transition
        // through Paused (preroll) first. When set_state returns:
        //   Ok(Success) — pipeline is already Playing (e.g. local file)
        //   Ok(Async)   — pipeline is transitioning asynchronously
        //   Ok(NoPreroll) — live source, no preroll needed
        //   Err(...)    — immediate failure (rare; most errors are async)
        let result = self.pipeline.set_state(State::Playing);
        match result {
            Ok(gstreamer::StateChangeSuccess::Success) => {
                tracing::info!("pipeline transitioned to Playing synchronously");
            },
            Ok(gstreamer::StateChangeSuccess::Async) => {
                tracing::info!(
                    "pipeline state change to Playing accepted (async — \
                     GStreamer is connecting to CDN and prerolling in the background)"
                );
            },
            Ok(gstreamer::StateChangeSuccess::NoPreroll) => {
                tracing::info!("pipeline transitioned to Playing (no-preroll / live source)");
            },
            Err(e) => {
                let msg = format!("set_state Playing failed: {}", e);
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
                                    "GStreamer error during set_state Playing"
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
                                    "GStreamer warning during set_state Playing"
                                );
                            },
                            _ => {},
                        }
                    }
                }

                return Err(PlaybackError::Gstreamer(msg));
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
