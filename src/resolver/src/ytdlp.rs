//! PiCast yt-dlp Subprocess Integration
//!
//! Invokes `yt-dlp` as a subprocess to resolve web page URLs
//! (YouTube, Vimeo, etc.) into direct media stream URLs.
//! The subprocess is run through the Tor SOCKS5 proxy for
//! anonymity and circuit isolation.
//!
//! ## Subprocess Command
//!
//! ```sh
//! yt-dlp --dump-json --no-download --no-warnings \
//!   --socket-timeout 30 \
//!   --proxy socks5h://picast-<isoid>@127.0.0.1:9050 \
//!   --format "bestvideo[vcodec^=avc1][height<=1080]+bestaudio/best[vcodec^=avc1][height<=1080]/best[height<=1080]" \
//!   --write-subs --write-auto-subs --sub-langs "en,es,fr,de" --sub-format vtt \
//!   --paths /tmp/picast-subs-XXXX \
//!   <url>
//! ```
//!
//! The format string forces H.264 (avc1) at 1080p max, which
//! is required for the V4L2 hardware decoder on Pi 4B+.
//! The `--write-subs` flag extracts available subtitles in VTT format.
//! Subtitle files are written to a temporary directory that is
//! automatically cleaned up when the `TempDir` is dropped.

use crate::{ResolveError, ResolveResult, UrlCategory};
use serde::Deserialize;
use std::time::Duration;
use tokio::process::Command;

/// Default timeout for yt-dlp subprocess execution (30 seconds).
const YTDLP_TIMEOUT_SECS: u64 = 30;

/// Default subtitle languages to request from yt-dlp.
const DEFAULT_SUB_LANGS: &str = "en,es,fr,de";

/// Subtitle format to request from yt-dlp.
const SUB_FORMAT: &str = "vtt";

/// The H.264 format string passed to yt-dlp's `--format` flag.
/// Prioritises pre-merged H.264 at 1080p or below (single URL with audio),
/// then falls back to separate video+audio streams (requires `requested_formats`
/// parsing), and finally any best stream at 1080p or below.
///
/// Pre-merged formats are preferred because they always provide a top-level
/// `url` field in `--dump-json` output, whereas `bestvideo+bestaudio` produces
/// separate entries in `requested_formats` with no top-level URL.
const H264_FORMAT_STRING: &str = concat!(
    "best[vcodec^=avc1][height<=1080]/",
    "best[height<=1080]/",
    "bestvideo[vcodec^=avc1][height<=1080]+bestaudio"
);

/// Parsed output from `yt-dlp --dump-json`.
///
/// Only the fields we care about are deserialized; everything
/// else is silently ignored.
///
/// When yt-dlp selects a pre-merged format (e.g. `best[vcodec^=avc1]`),
/// the `url` field is present at the top level. When it selects separate
/// video+audio streams (e.g. `bestvideo+bestaudio`), the top-level `url`
/// is empty and the individual stream URLs are in `requested_formats`.
#[derive(Debug, Deserialize)]
struct YtdlpOutput {
    /// The webpage URL (same as input).
    #[allow(dead_code)]
    #[serde(default)]
    webpage_url: Option<String>,
    /// Direct media URL for the best format.
    ///
    /// Present for pre-merged formats, empty/missing for `bestvideo+bestaudio`
    /// selections that use `requested_formats` instead.
    #[serde(default)]
    url: Option<String>,
    /// Individual stream formats when yt-dlp selects separate video+audio.
    ///
    /// Each entry has its own `url`, `vcodec`, `acodec`, etc.
    /// Only populated when the top-level `url` is empty.
    #[serde(default)]
    requested_formats: Option<Vec<RequestedFormat>>,
    /// Video title.
    title: Option<String>,
    /// Duration in seconds.
    duration: Option<f64>,
    /// Thumbnail URL.
    thumbnail: Option<String>,
    /// Video codec of the selected format.
    vcodec: Option<String>,
    /// Audio codec of the selected format.
    acodec: Option<String>,
    /// Video width.
    width: Option<i64>,
    /// Video height.
    height: Option<i64>,
    /// Available subtitle formats.
    #[serde(default)]
    subtitles: std::collections::HashMap<String, serde_json::Value>,
    /// The format identifier string.
    #[serde(default)]
    format: Option<String>,
}

/// A single format entry from yt-dlp's `requested_formats` array.
///
/// When yt-dlp selects `bestvideo+bestaudio`, each stream has its own
/// URL and codec information. We only deserialize the fields we need.
#[derive(Debug, PartialEq, Deserialize)]
struct RequestedFormat {
    /// Direct stream URL for this format.
    url: Option<String>,
    /// Video codec (e.g. "avc1.64001F", "none" for audio-only).
    #[serde(default)]
    vcodec: Option<String>,
    /// Audio codec (e.g. "mp4a.40.2", "none" for video-only).
    #[serde(default)]
    acodec: Option<String>,
    /// Video width.
    #[serde(default)]
    width: Option<i64>,
    /// Video height.
    #[serde(default)]
    height: Option<i64>,
}

impl YtdlpOutput {
    /// Resolve the effective media URL(s) from this yt-dlp output.
    ///
    /// Returns `(video_url, audio_url)` where:
    /// - `video_url` is always `Some` (the primary media URL)
    /// - `audio_url` is `Some` only when separate audio stream is available
    ///
    /// Resolution order:
    /// 1. Top-level `url` (pre-merged format) → `(url, None)`
    /// 2. `requested_formats` → video URL + optional audio URL
    /// 3. Error if no URL can be found
    fn resolve_urls(&self) -> Result<(String, Option<String>), ResolveError> {
        // Try top-level URL first (pre-merged format).
        if let Some(ref url) = self.url {
            if !url.is_empty() {
                return Ok((url.clone(), None));
            }
        }

        // Fall back to requested_formats (separate video+audio).
        if let Some(ref formats) = self.requested_formats {
            let mut video_url: Option<String> = None;
            let mut audio_url: Option<String> = None;

            for fmt in formats {
                let has_video = fmt
                    .vcodec
                    .as_ref()
                    .is_some_and(|c| c != "none" && !c.is_empty());
                let has_audio = fmt
                    .acodec
                    .as_ref()
                    .is_some_and(|c| c != "none" && !c.is_empty());

                if let Some(ref url) = fmt.url {
                    if !url.is_empty() {
                        if has_video && video_url.is_none() {
                            video_url = Some(url.clone());
                        }
                        if has_audio && audio_url.is_none() {
                            audio_url = Some(url.clone());
                        }
                    }
                }
            }

            if let Some(vurl) = video_url {
                if audio_url.is_some() {
                    tracing::warn!(
                        "separate video+audio streams detected; current pipeline plays \
                         video only — audio URL stored for future multi-stream support"
                    );
                }
                return Ok((vurl, audio_url));
            }
        }

        Err(ResolveError::NoMediaFound(
            "yt-dlp returned no playable URL (neither top-level `url` nor `requested_formats`)"
                .into(),
        ))
    }
}

/// Resolve a web page URL using yt-dlp.
///
/// Spawns `yt-dlp --dump-json` as a subprocess with the Tor
/// SOCKS5h proxy configured. Parses the JSON output to extract
/// the direct media URL, metadata, and available subtitles.
///
/// ## Timeouts
///
/// The subprocess is killed after 30 seconds. yt-dlp's own
/// socket timeout is also set to 30 seconds.
///
/// ## Error Handling
///
/// - `BinaryNotFound`: `yt-dlp` is not installed.
/// - `NoMediaFound`: yt-dlp couldn't extract a stream.
/// - `Network`: Subprocess failed (DNS, proxy, etc.).
pub async fn resolve_with_ytdlp(
    url: &str,
    socks_addr: &str,
    isolation_username: &str,
) -> Result<ResolveResult, ResolveError> {
    resolve_with_ytdlp_and_subs(url, socks_addr, isolation_username, true).await
}

/// Resolve a web page URL using yt-dlp with optional subtitle extraction.
///
/// When `extract_subs` is true, adds `--write-subs --write-auto-subs --sub-langs --sub-format`
/// flags to the yt-dlp command so that available subtitle tracks are
/// included in the JSON output's `subtitles` field.
///
/// Subtitle files are written to a temporary directory that is automatically
/// cleaned up when this function returns (both on success and error paths).
pub async fn resolve_with_ytdlp_and_subs(
    url: &str,
    socks_addr: &str,
    isolation_username: &str,
    extract_subs: bool,
) -> Result<ResolveResult, ResolveError> {
    let proxy_url = format!("socks5h://{}@{}", isolation_username, socks_addr);

    tracing::info!(
        url = url,
        proxy = %proxy_url,
        extract_subs = extract_subs,
        "spawning yt-dlp subprocess"
    );

    // Create a temp directory for yt-dlp to write subtitle files into.
    // The TempDir is cleaned up when dropped, which happens at the end
    // of this function on both success and error paths.
    let temp_dir = tempfile::tempdir().map_err(|e| {
        ResolveError::Network(format!("failed to create temp directory for subtitles: {}", e))
    })?;

    let mut cmd = Command::new("yt-dlp");
    cmd.kill_on_drop(true)
        .arg("--dump-json")
        .arg("--no-download")
        .arg("--no-warnings")
        .arg("--socket-timeout")
        .arg("30")
        .arg("--proxy")
        .arg(&proxy_url)
        .arg("--format")
        .arg(H264_FORMAT_STRING)
        .arg("--paths")
        .arg(temp_dir.path());

    // Add subtitle extraction flags when requested.
    if extract_subs {
        cmd.arg("--write-subs")
            .arg("--write-auto-subs")
            .arg("--sub-langs")
            .arg(DEFAULT_SUB_LANGS)
            .arg("--sub-format")
            .arg(SUB_FORMAT);
    }

    cmd.arg(url);

    let output = tokio::time::timeout(Duration::from_secs(YTDLP_TIMEOUT_SECS), cmd.output())
        .await
        .map_err(|_| {
            ResolveError::Network(format!("yt-dlp timed out after {}s", YTDLP_TIMEOUT_SECS))
        })?
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ResolveError::TorUnavailable(
                    "yt-dlp binary not found — install with: pip install yt-dlp".into(),
                )
            } else {
                ResolveError::Network(format!("failed to spawn yt-dlp: {}", e))
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let error_msg = stderr.lines().next().unwrap_or("unknown error");
        return Err(ResolveError::NoMediaFound(format!(
            "yt-dlp exited with {}: {}",
            output.status.code().unwrap_or(-1),
            error_msg
        )));
    }

    // Parse the JSON output.
    let ytdlp: YtdlpOutput = serde_json::from_slice(&output.stdout)
        .map_err(|e| ResolveError::NoMediaFound(format!("failed to parse yt-dlp JSON: {}", e)))?;

    tracing::info!(
        title = ?ytdlp.title,
        vcodec = ?ytdlp.vcodec,
        width = ?ytdlp.width,
        height = ?ytdlp.height,
        has_top_level_url = ytdlp.url.as_ref().is_some_and(|u| !u.is_empty()),
        has_requested_formats = ytdlp.requested_formats.is_some(),
        "yt-dlp resolved media info"
    );

    // Resolve the effective URL(s) — handles both pre-merged and separate
    // video+audio format outputs from yt-dlp.
    let (direct_url, audio_url) = ytdlp.resolve_urls()?;

    // Determine category based on what yt-dlp returned.
    let category = determine_category(&ytdlp);

    // Determine MIME type from video/audio codecs and container format.
    let mime_type = determine_mime_type(&category, &ytdlp.vcodec, &ytdlp.acodec, &ytdlp.format);

    // Handle negative, NaN, and infinite durations from yt-dlp (e.g. live streams return -1.0).
    let duration_ms = ytdlp.duration.and_then(|d| {
        if d < 0.0 || d.is_nan() || d.is_infinite() {
            None
        } else {
            Some((d * 1000.0) as u64)
        }
    });

    // temp_dir is dropped here, cleaning up any subtitle files yt-dlp wrote.
    Ok(ResolveResult {
        source_url: url.to_owned(),
        direct_url,
        audio_url,
        category,
        mime_type,
        content_length: None, // Not available from yt-dlp
        used_tor: true,
        title: ytdlp.title,
        duration: duration_ms,
        thumbnail: ytdlp.thumbnail,
        vcodec: ytdlp.vcodec,
        acodec: ytdlp.acodec,
        width: ytdlp.width.map(|w| w as u32),
        height: ytdlp.height.map(|h| h as u32),
        subtitle_tracks: ytdlp.subtitles.keys().cloned().collect(),
    })
}

/// Determine the URL category from yt-dlp output.
fn determine_category(ytdlp: &YtdlpOutput) -> UrlCategory {
    // Check the format string for HLS/DASH indicators.
    if let Some(ref fmt) = ytdlp.format {
        if fmt.contains("hls") {
            return UrlCategory::HlsManifest;
        }
        if fmt.contains("dash") {
            return UrlCategory::DashManifest;
        }
    }
    UrlCategory::DirectMedia
}

/// Determine the MIME type from codec and container information.
///
/// Unlike the previous implementation that always returned `"video/mp4"` or
/// `"audio/mp4"`, this function inspects the actual codecs and container
/// format to return the correct MIME type:
///
/// - VP9/AV1 video or WebM container → `video/webm`
/// - H.264 video (default) → `video/mp4`
/// - Opus audio → `audio/ogg`
/// - Other audio (default) → `audio/mp4`
/// - Magnet links → `application/x-magnet`
fn determine_mime_type(
    category: &UrlCategory,
    vcodec: &Option<String>,
    acodec: &Option<String>,
    format: &Option<String>,
) -> Option<String> {
    let has_vp9 = vcodec
        .as_ref()
        .map(|c| c.contains("vp9") || c.contains("vp09"))
        .unwrap_or(false);
    let has_av1 = vcodec.as_ref().map(|c| c.contains("av1")).unwrap_or(false);
    let has_opus = acodec.as_ref().map(|c| c.contains("opus")).unwrap_or(false);
    let is_webm_container = format.as_ref().map(|f| f.contains("webm")).unwrap_or(false);

    let has_video = vcodec.as_ref().is_some_and(|c| c != "none");
    let has_audio = acodec.as_ref().is_some_and(|c| c != "none");

    match category {
        UrlCategory::WebPage | UrlCategory::Onion => {
            if has_video {
                if has_vp9 || has_av1 || is_webm_container {
                    Some("video/webm".to_string())
                } else {
                    Some("video/mp4".to_string())
                }
            } else if has_audio {
                if has_opus || is_webm_container {
                    Some("audio/ogg".to_string())
                } else {
                    Some("audio/mp4".to_string())
                }
            } else {
                None
            }
        },
        UrlCategory::DirectMedia | UrlCategory::HlsManifest | UrlCategory::DashManifest => {
            // These are already classified correctly by the classifier.
            if has_video {
                if has_vp9 || has_av1 || is_webm_container {
                    Some("video/webm".to_string())
                } else {
                    Some("video/mp4".to_string())
                }
            } else if has_audio {
                if has_opus || is_webm_container {
                    Some("audio/ogg".to_string())
                } else {
                    Some("audio/mp4".to_string())
                }
            } else {
                None
            }
        },
        UrlCategory::Magnet => Some("application/x-magnet".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_format_string_is_valid() {
        // Ensure the format string compiles and looks right.
        assert!(H264_FORMAT_STRING.contains("avc1"));
        assert!(H264_FORMAT_STRING.contains("height<=1080"));
    }

    #[test]
    fn determine_category_hls() {
        let ytdlp = YtdlpOutput {
            webpage_url: None,
            url: Some("https://example.com/stream".into()),
            requested_formats: None,
            title: None,
            duration: None,
            thumbnail: None,
            vcodec: Some("avc1".into()),
            acodec: Some("mp4a".into()),
            width: None,
            height: None,
            subtitles: Default::default(),
            format: Some("hls-1234".into()),
        };
        assert_eq!(determine_category(&ytdlp), UrlCategory::HlsManifest);
    }

    #[test]
    fn determine_category_dash() {
        let ytdlp = YtdlpOutput {
            webpage_url: None,
            url: Some("https://example.com/stream".into()),
            requested_formats: None,
            title: None,
            duration: None,
            thumbnail: None,
            vcodec: Some("avc1".into()),
            acodec: Some("mp4a".into()),
            width: None,
            height: None,
            subtitles: Default::default(),
            format: Some("dash-5678".into()),
        };
        assert_eq!(determine_category(&ytdlp), UrlCategory::DashManifest);
    }

    #[test]
    fn determine_category_direct() {
        let ytdlp = YtdlpOutput {
            webpage_url: None,
            url: Some("https://example.com/video.mp4".into()),
            requested_formats: None,
            title: None,
            duration: None,
            thumbnail: None,
            vcodec: Some("avc1".into()),
            acodec: Some("mp4a".into()),
            width: None,
            height: None,
            subtitles: Default::default(),
            format: Some("137+251".into()),
        };
        assert_eq!(determine_category(&ytdlp), UrlCategory::DirectMedia);
    }

    #[test]
    fn determine_mime_type_h264_video() {
        let mime = determine_mime_type(
            &UrlCategory::WebPage,
            &Some("avc1".into()),
            &Some("mp4a".into()),
            &None,
        );
        assert_eq!(mime, Some("video/mp4".into()));
    }

    #[test]
    fn determine_mime_type_vp9_video() {
        let mime = determine_mime_type(
            &UrlCategory::WebPage,
            &Some("vp9".into()),
            &Some("opus".into()),
            &None,
        );
        assert_eq!(mime, Some("video/webm".into()));
    }

    #[test]
    fn determine_mime_type_vp09_video() {
        let mime = determine_mime_type(
            &UrlCategory::Onion,
            &Some("vp09.00.10.08".into()),
            &Some("opus".into()),
            &None,
        );
        assert_eq!(mime, Some("video/webm".into()));
    }

    #[test]
    fn determine_mime_type_av1_video() {
        let mime = determine_mime_type(
            &UrlCategory::WebPage,
            &Some("av1".into()),
            &Some("opus".into()),
            &None,
        );
        assert_eq!(mime, Some("video/webm".into()));
    }

    #[test]
    fn determine_mime_type_webm_container() {
        let mime = determine_mime_type(
            &UrlCategory::WebPage,
            &Some("avc1".into()),
            &Some("mp4a".into()),
            &Some("webm-1234".into()),
        );
        assert_eq!(mime, Some("video/webm".into()));
    }

    #[test]
    fn determine_mime_type_audio_only_mp4a() {
        let mime = determine_mime_type(
            &UrlCategory::WebPage,
            &Some("none".into()),
            &Some("mp4a".into()),
            &None,
        );
        assert_eq!(mime, Some("audio/mp4".into()));
    }

    #[test]
    fn determine_mime_type_audio_only_opus() {
        let mime = determine_mime_type(
            &UrlCategory::WebPage,
            &Some("none".into()),
            &Some("opus".into()),
            &None,
        );
        assert_eq!(mime, Some("audio/ogg".into()));
    }

    #[test]
    fn determine_mime_type_audio_opus_webm_container() {
        let mime = determine_mime_type(
            &UrlCategory::Onion,
            &Some("none".into()),
            &Some("opus".into()),
            &Some("webm".into()),
        );
        assert_eq!(mime, Some("audio/ogg".into()));
    }

    #[test]
    fn determine_mime_type_no_codecs() {
        let mime = determine_mime_type(
            &UrlCategory::WebPage,
            &Some("none".into()),
            &Some("none".into()),
            &None,
        );
        assert_eq!(mime, None);
    }

    #[test]
    fn determine_mime_type_magnet() {
        let mime = determine_mime_type(&UrlCategory::Magnet, &None, &None, &None);
        assert_eq!(mime, Some("application/x-magnet".into()));
    }

    #[test]
    fn determine_mime_type_direct_media_vp9() {
        let mime = determine_mime_type(
            &UrlCategory::DirectMedia,
            &Some("vp9".into()),
            &Some("opus".into()),
            &None,
        );
        assert_eq!(mime, Some("video/webm".into()));
    }

    #[test]
    fn determine_mime_type_hls_h264() {
        let mime = determine_mime_type(
            &UrlCategory::HlsManifest,
            &Some("avc1".into()),
            &Some("mp4a".into()),
            &None,
        );
        assert_eq!(mime, Some("video/mp4".into()));
    }

    #[test]
    fn determine_mime_type_dash_vp9() {
        let mime = determine_mime_type(
            &UrlCategory::DashManifest,
            &Some("vp9".into()),
            &Some("opus".into()),
            &None,
        );
        assert_eq!(mime, Some("video/webm".into()));
    }

    #[test]
    fn determine_mime_type_audio_webm_container() {
        // Audio-only with webm container should be audio/ogg
        let mime = determine_mime_type(
            &UrlCategory::WebPage,
            &Some("none".into()),
            &Some("mp4a".into()),
            &Some("webm".into()),
        );
        assert_eq!(mime, Some("audio/ogg".into()));
    }

    #[test]
    fn determine_mime_type_none_vcodec_none_acodec() {
        let mime = determine_mime_type(&UrlCategory::WebPage, &None, &None, &None);
        assert_eq!(mime, None);
    }

    #[test]
    fn determine_mime_type_video_only_no_audio() {
        // Video codec present, no audio codec at all.
        let mime = determine_mime_type(
            &UrlCategory::WebPage,
            &Some("avc1".into()),
            &None,
            &None,
        );
        assert_eq!(mime, Some("video/mp4".into()));
    }

    #[test]
    fn determine_mime_type_audio_only_no_video() {
        // No video codec at all, audio codec present.
        let mime = determine_mime_type(
            &UrlCategory::WebPage,
            &None,
            &Some("opus".into()),
            &None,
        );
        assert_eq!(mime, Some("audio/ogg".into()));
    }

    #[test]
    fn subtitle_extraction_from_ytdlp_json() {
        // Simulate yt-dlp JSON output with subtitles.
        let json = r#"{
            "url": "https://example.com/video.mp4",
            "title": "Test Video with Subtitles",
            "duration": 120.5,
            "thumbnail": "https://example.com/thumb.jpg",
            "vcodec": "avc1",
            "acodec": "mp4a",
            "width": 1920,
            "height": 1080,
            "subtitles": {
                "en": [{"url": "https://example.com/subs/en.vtt", "ext": "vtt"}],
                "es": [{"url": "https://example.com/subs/es.vtt", "ext": "vtt"}],
                "fr": [{"url": "https://example.com/subs/fr.vtt", "ext": "vtt"}]
            },
            "format": "137+251"
        }"#;

        let ytdlp: YtdlpOutput = serde_json::from_str(json).unwrap();
        let mut subtitle_tracks: Vec<String> = ytdlp.subtitles.keys().cloned().collect();
        subtitle_tracks.sort();

        assert_eq!(subtitle_tracks, vec!["en", "es", "fr"]);
        assert_eq!(ytdlp.title, Some("Test Video with Subtitles".into()));
        assert_eq!(ytdlp.duration, Some(120.5));
    }

    #[test]
    fn subtitle_tracks_empty_when_no_subs() {
        let ytdlp = YtdlpOutput {
            webpage_url: None,
            url: None,
            requested_formats: None,
            title: None,
            duration: None,
            thumbnail: None,
            vcodec: Some("avc1".into()),
            acodec: Some("mp4a".into()),
            width: None,
            height: None,
            subtitles: Default::default(),
            format: None,
        };
        assert!(ytdlp.subtitles.is_empty());
    }

    #[test]
    fn default_sub_langs_constant() {
        assert!(DEFAULT_SUB_LANGS.contains("en"));
        assert!(DEFAULT_SUB_LANGS.contains("es"));
        assert!(DEFAULT_SUB_LANGS.contains("fr"));
        assert!(DEFAULT_SUB_LANGS.contains("de"));
    }

    #[test]
    fn sub_format_is_vtt() {
        assert_eq!(SUB_FORMAT, "vtt");
    }

    #[test]
    fn determine_category_no_format() {
        let ytdlp = YtdlpOutput {
            webpage_url: None,
            url: Some("https://example.com/video.mp4".into()),
            requested_formats: None,
            title: None,
            duration: None,
            thumbnail: None,
            vcodec: Some("avc1".into()),
            acodec: Some("mp4a".into()),
            width: None,
            height: None,
            subtitles: Default::default(),
            format: None,
        };
        assert_eq!(determine_category(&ytdlp), UrlCategory::DirectMedia);
    }

    #[test]
    fn ytdlp_output_deserialize_full() {
        // Full end-to-end deserialization test with all fields populated.
        let json = r#"{
            "webpage_url": "https://youtube.com/watch?v=abc",
            "url": "https://rr.googlevideo.com/videoplayback?id=abc",
            "title": "Full Test Video",
            "duration": 300.0,
            "thumbnail": "https://i.ytimg.com/vi/abc/maxresdefault.jpg",
            "vcodec": "avc1.64001F",
            "acodec": "mp4a.40.2",
            "width": 1920,
            "height": 1080,
            "subtitles": {
                "en": [{"url": "https://youtube.com/api/timedtext?lang=en", "ext": "vtt"}],
                "de": [{"url": "https://youtube.com/api/timedtext?lang=de", "ext": "vtt"}]
            },
            "format": "137+251"
        }"#;

        let ytdlp: YtdlpOutput = serde_json::from_str(json).unwrap();
        assert_eq!(ytdlp.url, Some("https://rr.googlevideo.com/videoplayback?id=abc".into()));
        assert_eq!(ytdlp.title, Some("Full Test Video".into()));
        assert_eq!(ytdlp.duration, Some(300.0));
        assert_eq!(ytdlp.width, Some(1920));
        assert_eq!(ytdlp.height, Some(1080));
        assert_eq!(ytdlp.vcodec, Some("avc1.64001F".into()));
        assert_eq!(ytdlp.acodec, Some("mp4a.40.2".into()));
        assert_eq!(ytdlp.subtitles.len(), 2);
        assert!(ytdlp.subtitles.contains_key("en"));
        assert!(ytdlp.subtitles.contains_key("de"));
    }

    // ── Comprehensive ytdlp tests ──────────────────────────────────────

    #[test]
    fn determine_category_hls_priority_over_dash() {
        // If the format string contains both "hls" and "dash",
        // HLS should be returned since it's checked first.
        let ytdlp = YtdlpOutput {
            webpage_url: None,
            url: Some("https://example.com/stream".into()),
            requested_formats: None,
            title: None,
            duration: None,
            thumbnail: None,
            vcodec: Some("avc1".into()),
            acodec: Some("mp4a".into()),
            width: None,
            height: None,
            subtitles: Default::default(),
            format: Some("hls-dash-1234".into()),
        };
        assert_eq!(determine_category(&ytdlp), UrlCategory::HlsManifest);
    }

    #[test]
    fn determine_category_case_sensitive_format() {
        // The format check is case-sensitive: "HLS" should NOT match.
        let ytdlp = YtdlpOutput {
            webpage_url: None,
            url: Some("https://example.com/stream".into()),
            requested_formats: None,
            title: None,
            duration: None,
            thumbnail: None,
            vcodec: Some("avc1".into()),
            acodec: Some("mp4a".into()),
            width: None,
            height: None,
            subtitles: Default::default(),
            format: Some("HLS-1234".into()),
        };
        // "HLS" doesn't match "hls" since contains() is case-sensitive.
        assert_eq!(determine_category(&ytdlp), UrlCategory::DirectMedia);
    }

    #[test]
    fn determine_category_dash_case_sensitive() {
        // "DASH" should NOT match since contains() is case-sensitive.
        let ytdlp = YtdlpOutput {
            webpage_url: None,
            url: Some("https://example.com/stream".into()),
            requested_formats: None,
            title: None,
            duration: None,
            thumbnail: None,
            vcodec: Some("avc1".into()),
            acodec: Some("mp4a".into()),
            width: None,
            height: None,
            subtitles: Default::default(),
            format: Some("DASH-5678".into()),
        };
        assert_eq!(determine_category(&ytdlp), UrlCategory::DirectMedia);
    }

    #[test]
    fn duration_negative_treated_as_none() {
        // Live streams return -1.0 duration; it should map to None.
        let duration: Option<f64> = Some(-1.0);
        let result = duration.and_then(|d| {
            if d < 0.0 || d.is_nan() || d.is_infinite() {
                None
            } else {
                Some((d * 1000.0) as u64)
            }
        });
        assert_eq!(result, None);
    }

    #[test]
    fn duration_nan_treated_as_none() {
        let duration: Option<f64> = Some(f64::NAN);
        let result = duration.and_then(|d| {
            if d < 0.0 || d.is_nan() || d.is_infinite() {
                None
            } else {
                Some((d * 1000.0) as u64)
            }
        });
        assert_eq!(result, None);
    }

    #[test]
    fn duration_infinite_treated_as_none() {
        let duration: Option<f64> = Some(f64::INFINITY);
        let result = duration.and_then(|d| {
            if d < 0.0 || d.is_nan() || d.is_infinite() {
                None
            } else {
                Some((d * 1000.0) as u64)
            }
        });
        assert_eq!(result, None);
    }

    #[test]
    fn duration_positive_converted_to_ms() {
        let duration: Option<f64> = Some(120.5);
        let result = duration.and_then(|d| {
            if d < 0.0 || d.is_nan() || d.is_infinite() {
                None
            } else {
                Some((d * 1000.0) as u64)
            }
        });
        assert_eq!(result, Some(120500));
    }

    #[test]
    fn duration_zero_converted_to_zero_ms() {
        let duration: Option<f64> = Some(0.0);
        let result = duration.and_then(|d| {
            if d < 0.0 || d.is_nan() || d.is_infinite() {
                None
            } else {
                Some((d * 1000.0) as u64)
            }
        });
        assert_eq!(result, Some(0));
    }

    #[test]
    fn ytdlp_output_deserialize_minimal() {
        // Only the `url` field is present; everything else should default.
        let json = r#"{"url": "https://example.com/video.mp4"}"#;
        let ytdlp: YtdlpOutput = serde_json::from_str(json).unwrap();
        assert_eq!(ytdlp.url, Some("https://example.com/video.mp4".into()));
        assert_eq!(ytdlp.webpage_url, None);
        assert_eq!(ytdlp.title, None);
        assert_eq!(ytdlp.duration, None);
        assert_eq!(ytdlp.thumbnail, None);
        assert_eq!(ytdlp.vcodec, None);
        assert_eq!(ytdlp.acodec, None);
        assert_eq!(ytdlp.width, None);
        assert_eq!(ytdlp.height, None);
        assert!(ytdlp.subtitles.is_empty());
        assert_eq!(ytdlp.format, None);
        assert_eq!(ytdlp.requested_formats, None);
    }

    #[test]
    fn ytdlp_output_deserialize_missing_url_no_requested_formats_fails() {
        // The `url` field is now optional, but if neither `url` nor
        // `requested_formats` provides a playable URL, `resolve_urls()`
        // should fail.
        let json = r#"{"title": "No URL"}"#;
        let ytdlp: YtdlpOutput = serde_json::from_str(json).unwrap();
        assert!(ytdlp.url.is_none());
        assert!(ytdlp.requested_formats.is_none());
        let result = ytdlp.resolve_urls();
        assert!(result.is_err(), "resolve_urls should fail with no URL available");
    }

    #[test]
    fn ytdlp_output_deserialize_with_extra_fields() {
        // Extra fields should be silently ignored.
        let json = r#"{
            "url": "https://example.com/video.mp4",
            "title": "Test",
            "some_unknown_field": "ignored",
            "another_extra": 42
        }"#;
        let ytdlp: YtdlpOutput = serde_json::from_str(json).unwrap();
        assert_eq!(ytdlp.url, Some("https://example.com/video.mp4".into()));
        assert_eq!(ytdlp.title, Some("Test".into()));
    }

    #[test]
    fn ytdlp_output_deserialize_negative_duration() {
        // yt-dlp can return -1 for live streams or unknown duration.
        let json = r#"{"url": "https://example.com/live", "duration": -1.0}"#;
        let ytdlp: YtdlpOutput = serde_json::from_str(json).unwrap();
        assert_eq!(ytdlp.duration, Some(-1.0));
    }

    #[test]
    fn ytdlp_output_deserialize_zero_dimensions() {
        let json = r#"{"url": "https://example.com/audio.mp3", "width": 0, "height": 0}"#;
        let ytdlp: YtdlpOutput = serde_json::from_str(json).unwrap();
        assert_eq!(ytdlp.width, Some(0));
        assert_eq!(ytdlp.height, Some(0));
    }

    #[test]
    fn ytdlp_timeout_constant() {
        assert_eq!(YTDLP_TIMEOUT_SECS, 30, "default yt-dlp timeout should be 30 seconds");
    }

    #[test]
    fn h264_format_string_components() {
        // Verify the format string has the expected components.
        assert!(H264_FORMAT_STRING.contains("bestvideo"));
        assert!(H264_FORMAT_STRING.contains("bestaudio"));
        assert!(H264_FORMAT_STRING.contains("avc1"));
        assert!(H264_FORMAT_STRING.contains("height<=1080"));
        assert!(H264_FORMAT_STRING.contains("best[height<=1080]"));
    }

    #[test]
    fn subtitle_keys_extraction() {
        // Verify subtitle language codes are correctly extracted as keys.
        let json = r#"{
            "url": "https://example.com/video.mp4",
            "subtitles": {
                "en": [{"url": "https://example.com/en.vtt"}],
                "es": [{"url": "https://example.com/es.vtt"}],
                "fr": [{"url": "https://example.com/fr.vtt"}],
                "de": [{"url": "https://example.com/de.vtt"}],
                "ja": [{"url": "https://example.com/ja.vtt"}]
            }
        }"#;
        let ytdlp: YtdlpOutput = serde_json::from_str(json).unwrap();
        let mut keys: Vec<String> = ytdlp.subtitles.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, vec!["de", "en", "es", "fr", "ja"]);
    }

    #[test]
    fn subtitle_empty_hashmap() {
        let json = r#"{"url": "https://example.com/video.mp4", "subtitles": {}}"#;
        let ytdlp: YtdlpOutput = serde_json::from_str(json).unwrap();
        assert!(ytdlp.subtitles.is_empty());
    }

    #[test]
    fn determine_category_format_with_hls_substring() {
        // Any format string containing "hls" should match.
        let ytdlp = YtdlpOutput {
            webpage_url: None,
            url: Some("https://example.com/stream".into()),
            requested_formats: None,
            title: None,
            duration: None,
            thumbnail: None,
            vcodec: None,
            acodec: None,
            width: None,
            height: None,
            subtitles: Default::default(),
            format: Some("hls-4320".into()),
        };
        assert_eq!(determine_category(&ytdlp), UrlCategory::HlsManifest);
    }

    #[test]
    fn determine_category_format_with_dash_substring() {
        let ytdlp = YtdlpOutput {
            webpage_url: None,
            url: Some("https://example.com/stream".into()),
            requested_formats: None,
            title: None,
            duration: None,
            thumbnail: None,
            vcodec: None,
            acodec: None,
            width: None,
            height: None,
            subtitles: Default::default(),
            format: Some("dash-9876".into()),
        };
        assert_eq!(determine_category(&ytdlp), UrlCategory::DashManifest);
    }

    // ── requested_formats / resolve_urls tests ──────────────────────────

    #[test]
    fn resolve_urls_top_level_url_preferred() {
        // When top-level `url` is present, it should be used directly.
        let json = r#"{
            "url": "https://example.com/merged.mp4",
            "title": "Pre-merged"
        }"#;
        let ytdlp: YtdlpOutput = serde_json::from_str(json).unwrap();
        let (video_url, audio_url) = ytdlp.resolve_urls().unwrap();
        assert_eq!(video_url, "https://example.com/merged.mp4");
        assert_eq!(audio_url, None);
    }

    #[test]
    fn resolve_urls_from_requested_formats_video_plus_audio() {
        // When top-level `url` is missing but `requested_formats` has
        // separate video and audio entries, we should extract both URLs.
        let json = r#"{
            "title": "Separate Streams",
            "requested_formats": [
                {
                    "url": "https://cdn.example.com/video_only.mp4",
                    "vcodec": "avc1.64001F",
                    "acodec": "none",
                    "width": 1920,
                    "height": 1080
                },
                {
                    "url": "https://cdn.example.com/audio_only.mp4",
                    "vcodec": "none",
                    "acodec": "mp4a.40.2"
                }
            ]
        }"#;
        let ytdlp: YtdlpOutput = serde_json::from_str(json).unwrap();
        assert!(ytdlp.url.is_none());
        let (video_url, audio_url) = ytdlp.resolve_urls().unwrap();
        assert_eq!(video_url, "https://cdn.example.com/video_only.mp4");
        assert_eq!(audio_url, Some("https://cdn.example.com/audio_only.mp4".into()));
    }

    #[test]
    fn resolve_urls_from_requested_formats_video_only() {
        // When `requested_formats` only has a video entry.
        let json = r#"{
            "requested_formats": [
                {
                    "url": "https://cdn.example.com/video.mp4",
                    "vcodec": "avc1",
                    "acodec": "none"
                }
            ]
        }"#;
        let ytdlp: YtdlpOutput = serde_json::from_str(json).unwrap();
        let (video_url, audio_url) = ytdlp.resolve_urls().unwrap();
        assert_eq!(video_url, "https://cdn.example.com/video.mp4");
        assert_eq!(audio_url, None);
    }

    #[test]
    fn resolve_urls_empty_top_level_url_falls_back_to_requested_formats() {
        // When top-level `url` is an empty string, fall back to requested_formats.
        let json = r#"{
            "url": "",
            "requested_formats": [
                {
                    "url": "https://cdn.example.com/video.mp4",
                    "vcodec": "avc1",
                    "acodec": "none"
                },
                {
                    "url": "https://cdn.example.com/audio.mp4",
                    "vcodec": "none",
                    "acodec": "opus"
                }
            ]
        }"#;
        let ytdlp: YtdlpOutput = serde_json::from_str(json).unwrap();
        let (video_url, audio_url) = ytdlp.resolve_urls().unwrap();
        assert_eq!(video_url, "https://cdn.example.com/video.mp4");
        assert_eq!(audio_url, Some("https://cdn.example.com/audio.mp4".into()));
    }

    #[test]
    fn resolve_urls_no_url_at_all_errors() {
        // Neither top-level `url` nor `requested_formats` — should error.
        let json = r#"{"title": "Nothing"}"#;
        let ytdlp: YtdlpOutput = serde_json::from_str(json).unwrap();
        let result = ytdlp.resolve_urls();
        assert!(result.is_err());
        match result.unwrap_err() {
            ResolveError::NoMediaFound(msg) => {
                assert!(msg.contains("no playable URL"));
            },
            other => panic!("expected NoMediaFound, got {:?}", other),
        }
    }

    #[test]
    fn resolve_urls_requested_formats_with_empty_urls_skipped() {
        // `requested_formats` entries with empty URLs should be skipped.
        let json = r#"{
            "requested_formats": [
                {
                    "url": "",
                    "vcodec": "avc1",
                    "acodec": "none"
                }
            ]
        }"#;
        let ytdlp: YtdlpOutput = serde_json::from_str(json).unwrap();
        let result = ytdlp.resolve_urls();
        assert!(result.is_err());
    }

    #[test]
    fn format_string_prefers_pre_merged() {
        // The format string should prefer pre-merged formats first,
        // falling back to separate video+audio only as last resort.
        assert!(H264_FORMAT_STRING.starts_with("best[vcodec^=avc1]"));
        assert!(H264_FORMAT_STRING.contains("best[height<=1080]"));
        // The + format (bestvideo+bestaudio) should be the LAST option.
        let parts: Vec<&str> = H264_FORMAT_STRING.split('/').collect();
        assert_eq!(parts.last().unwrap(), &"bestvideo[vcodec^=avc1][height<=1080]+bestaudio");
    }
}
