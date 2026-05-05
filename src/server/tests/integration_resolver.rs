//! Resolver integration tests.
//!
//! These tests exercise the URL classification and resolution logic
//! exposed by `picast_resolver::Resolver`.
//!
//! Note: WebPage and Onion URLs now route through yt-dlp, which
//! requires `yt-dlp` to be installed and Tor to be running. These
//! tests gracefully handle the case where yt-dlp is not available.

mod common;

use picast_resolver::{ResolveError, Resolver, UrlCategory};
use std::sync::Arc;

fn test_resolver() -> Resolver {
    let tor = Arc::new(picast_tor::TorManager::new("127.0.0.1:9050"));
    Resolver::new(tor)
}

/// YouTube URLs should be classified as [`UrlCategory::WebPage`].
/// Without yt-dlp/Tor, resolution fails gracefully.
#[tokio::test]
async fn test_classify_youtube_url_as_webpage() {
    let resolver = test_resolver();

    // Classification is pure URL parsing — no network needed.
    let cat = resolver.classify("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap();
    assert_eq!(cat, UrlCategory::WebPage);

    // Resolution now invokes yt-dlp through Tor. Without yt-dlp,
    // this should fail with TorUnavailable or Network error.
    let result = resolver.resolve("https://www.youtube.com/watch?v=dQw4w9WgXcQ").await;
    match result {
        Ok(resolved) => {
            // If yt-dlp is installed and works, verify category.
            assert_eq!(resolved.category, UrlCategory::WebPage);
        }
        Err(ResolveError::TorUnavailable(msg)) => {
            assert!(msg.contains("yt-dlp"), "error should mention yt-dlp: {}", msg);
        }
        Err(ResolveError::Network(_)) | Err(ResolveError::NoMediaFound(_)) => {
            // Acceptable without a running Tor daemon or for geo-blocked content.
        }
        Err(e) => panic!("unexpected error: {:?}", e),
    }
}

/// `.m3u8` URLs should be classified as [`UrlCategory::HlsManifest`].
#[tokio::test]
async fn test_classify_m3u8_as_hls() {
    let resolver = test_resolver();
    let result = resolver.resolve("https://example.com/stream.m3u8").await;
    assert!(result.is_ok(), "m3u8 URL should resolve successfully");
    let resolved = result.unwrap();
    assert_eq!(resolved.category, UrlCategory::HlsManifest);
}

/// `.mpd` URLs should be classified as [`UrlCategory::DashManifest`].
#[tokio::test]
async fn test_classify_mpd_as_dash() {
    let resolver = test_resolver();
    let result = resolver.resolve("https://example.com/stream.mpd").await;
    assert!(result.is_ok(), "mpd URL should resolve successfully");
    let resolved = result.unwrap();
    assert_eq!(resolved.category, UrlCategory::DashManifest);
}

/// Direct media file URLs (`.mp4`) should be classified as
/// [`UrlCategory::DirectMedia`].
#[tokio::test]
async fn test_classify_direct_mp4() {
    let resolver = test_resolver();
    let result = resolver.resolve("https://example.com/video.mp4").await;
    assert!(result.is_ok(), "mp4 URL should resolve successfully");
    let resolved = result.unwrap();
    assert_eq!(resolved.category, UrlCategory::DirectMedia);
}

/// `.onion` URLs should be classified as [`UrlCategory::Onion`].
/// Without yt-dlp/Tor, resolution fails gracefully.
#[tokio::test]
async fn test_classify_onion_url() {
    let resolver = test_resolver();

    // Classification is pure URL parsing — no network needed.
    let cat = resolver.classify("http://example.onion/video.mp4").unwrap();
    assert_eq!(cat, UrlCategory::Onion);

    // Resolution now invokes yt-dlp through Tor. Without yt-dlp,
    // this should fail with TorUnavailable or Network error.
    let result = resolver.resolve("http://example.onion/video.mp4").await;
    match result {
        Ok(resolved) => {
            assert_eq!(resolved.category, UrlCategory::Onion);
            assert!(resolved.used_tor);
        }
        Err(ResolveError::TorUnavailable(msg)) => {
            assert!(msg.contains("yt-dlp"), "error should mention yt-dlp: {}", msg);
        }
        Err(ResolveError::Network(_)) | Err(ResolveError::NoMediaFound(_)) => {
            // Acceptable without a running Tor daemon.
        }
        Err(e) => panic!("unexpected error: {:?}", e),
    }
}
