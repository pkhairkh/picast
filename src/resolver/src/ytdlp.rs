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
//!   --write-subs --sub-langs "en,es,fr,de" --sub-format vtt \
//!   <url>
//! ```
//!
//! The format string forces H.264 (avc1) at 1080p max, which
//! is required for the V4L2 hardware decoder on Pi 4B+.
//! The `--write-subs` flag extracts available subtitles in VTT format.

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
/// Prioritises hardware-decodable H.264 at 1080p or below.
const H264_FORMAT_STRING: &str = concat!(
    "bestvideo[vcodec^=avc1][height<=1080]+bestaudio/",
    "best[vcodec^=avc1][height<=1080]/",
    "best[height<=1080]"
);

/// Parsed output from `yt-dlp --dump-json`.
///
/// Only the fields we care about are deserialized; everything
/// else is silently ignored.
#[derive(Debug, Deserialize)]
struct YtdlpOutput {
    /// The webpage URL (same as input).
    #[allow(dead_code)]
    #[serde(default)]
    webpage_url: Option<String>,
    /// Direct media URL for the best format.
    url: String,
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
/// When `extract_subs` is true, adds `--write-subs --sub-langs --sub-format`
/// flags to the yt-dlp command so that available subtitle tracks are
/// included in the JSON output's `subtitles` field.
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

    let mut cmd = Command::new("yt-dlp");
    cmd.arg("--dump-json")
        .arg("--no-download")
        .arg("--no-warnings")
        .arg("--socket-timeout")
        .arg("30")
        .arg("--proxy")
        .arg(&proxy_url)
        .arg("--format")
        .arg(H264_FORMAT_STRING);

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

    let output = tokio::time::timeout(
        Duration::from_secs(YTDLP_TIMEOUT_SECS),
        cmd.output(),
    )
    .await
    .map_err(|_| ResolveError::Network(format!("yt-dlp timed out after {}s", YTDLP_TIMEOUT_SECS)))?
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
        "yt-dlp resolved media URL"
    );

    // Determine category based on what yt-dlp returned.
    let category = determine_category(&ytdlp);

    // Determine MIME type from video/audio codecs.
    let mime_type = determine_mime_type(&ytdlp);

    Ok(ResolveResult {
        source_url: url.to_owned(),
        direct_url: ytdlp.url,
        category,
        mime_type,
        content_length: None, // Not available from yt-dlp
        used_tor: true,
        title: ytdlp.title,
        duration: ytdlp.duration.map(|d| d as u64 * 1000),
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

/// Determine the MIME type from codec information.
fn determine_mime_type(ytdlp: &YtdlpOutput) -> Option<String> {
    let has_video = ytdlp.vcodec.as_ref().is_some_and(|c| c != "none");
    let has_audio = ytdlp.acodec.as_ref().is_some_and(|c| c != "none");

    match (has_video, has_audio) {
        (true, true) => Some("video/mp4".to_owned()),
        (true, false) => Some("video/mp4".to_owned()),
        (false, true) => Some("audio/mp4".to_owned()),
        (false, false) => None,
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
            url: "https://example.com/stream".into(),
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
            url: "https://example.com/stream".into(),
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
            url: "https://example.com/video.mp4".into(),
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
    fn determine_mime_type_video() {
        let ytdlp = YtdlpOutput {
            webpage_url: None,
            url: String::new(),
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
        assert_eq!(determine_mime_type(&ytdlp), Some("video/mp4".into()));
    }

    #[test]
    fn determine_mime_type_audio_only() {
        let ytdlp = YtdlpOutput {
            webpage_url: None,
            url: String::new(),
            title: None,
            duration: None,
            thumbnail: None,
            vcodec: Some("none".into()),
            acodec: Some("opus".into()),
            width: None,
            height: None,
            subtitles: Default::default(),
            format: None,
        };
        assert_eq!(determine_mime_type(&ytdlp), Some("audio/mp4".into()));
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
            url: String::new(),
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
    fn determine_mime_type_no_codecs() {
        let ytdlp = YtdlpOutput {
            webpage_url: None,
            url: String::new(),
            title: None,
            duration: None,
            thumbnail: None,
            vcodec: Some("none".into()),
            acodec: Some("none".into()),
            width: None,
            height: None,
            subtitles: Default::default(),
            format: None,
        };
        assert_eq!(determine_mime_type(&ytdlp), None);
    }

    #[test]
    fn determine_category_no_format() {
        let ytdlp = YtdlpOutput {
            webpage_url: None,
            url: "https://example.com/video.mp4".into(),
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
        assert_eq!(ytdlp.url, "https://rr.googlevideo.com/videoplayback?id=abc");
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
}
