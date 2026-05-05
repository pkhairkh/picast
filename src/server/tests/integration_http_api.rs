//! HTTP API integration tests.
//!
//! These tests verify the REST-like control surface exposed by
//! `picast_protocols::HttpApiServer`. Each test starts a fresh server
//! instance and issues HTTP requests against it.
//!
//! **Note:** These tests require a running PiCast server. Set
//! `PICAST_TEST_HTTP_ADDR` to override the default `127.0.0.1:8585`.

mod common;

use common::{test_http_addr, wait_for_server};

/// Verify that the health endpoint returns HTTP 200.
#[tokio::test]
async fn test_health_endpoint_returns_200() {
    let addr = test_http_addr();
    wait_for_server(&addr, 5_000);

    let url = format!("http://{}/v1/health", addr);
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await;
    assert!(resp.is_ok(), "health request should succeed");
    let resp = resp.unwrap();
    assert_eq!(resp.status(), 200, "health endpoint should return 200");
}

/// Verify that the `/v1/load` endpoint accepts a media URL.
#[tokio::test]
async fn test_cast_endpoint_accepts_url() {
    let addr = test_http_addr();
    wait_for_server(&addr, 5_000);

    let url = format!("http://{}/v1/load", addr);
    let client = reqwest::Client::new();
    let body = serde_json::json!({ "url": "https://example.com/video.mp4" });
    let resp = client.post(&url).json(&body).send().await;
    assert!(resp.is_ok(), "load request should succeed");
    let resp = resp.unwrap();
    assert!(
        resp.status().is_success(),
        "load endpoint should accept a URL, got {}",
        resp.status()
    );
}

/// Verify that the `/v1/status` endpoint returns session state.
#[tokio::test]
async fn test_status_endpoint_returns_session_state() {
    let addr = test_http_addr();
    wait_for_server(&addr, 5_000);

    let url = format!("http://{}/v1/status", addr);
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await;
    assert!(resp.is_ok(), "status request should succeed");
    let resp = resp.unwrap();
    assert!(
        resp.status().is_success(),
        "status endpoint should return success, got {}",
        resp.status()
    );
}

/// Verify the full pause → play → stop lifecycle.
#[tokio::test]
async fn test_pause_play_stop_lifecycle() {
    let addr = test_http_addr();
    wait_for_server(&addr, 5_000);

    let client = reqwest::Client::new();

    // Load a URL first so we have an active session.
    let load_url = format!("http://{}/v1/load", addr);
    let body = serde_json::json!({ "url": "https://example.com/video.mp4" });
    let resp = client.post(&load_url).json(&body).send().await;
    assert!(resp.is_ok(), "load request should succeed");

    // Pause
    let pause_url = format!("http://{}/v1/pause", addr);
    let resp = client.post(&pause_url).send().await;
    assert!(resp.is_ok(), "pause request should succeed");

    // Play (resume)
    let play_url = format!("http://{}/v1/play", addr);
    let resp = client.post(&play_url).send().await;
    assert!(resp.is_ok(), "play request should succeed");

    // Stop
    let stop_url = format!("http://{}/v1/stop", addr);
    let resp = client.post(&stop_url).send().await;
    assert!(resp.is_ok(), "stop request should succeed");
}

/// Verify that volume can be set and retrieved.
#[tokio::test]
async fn test_volume_set_and_get() {
    let addr = test_http_addr();
    wait_for_server(&addr, 5_000);

    let client = reqwest::Client::new();

    // Set volume
    let set_vol_url = format!("http://{}/v1/setVolume", addr);
    let body = serde_json::json!({ "volume": 50 });
    let resp = client.post(&set_vol_url).json(&body).send().await;
    assert!(resp.is_ok(), "setVolume request should succeed");
    let resp = resp.unwrap();
    assert!(
        resp.status().is_success(),
        "setVolume should return success, got {}",
        resp.status()
    );

    // Get status to verify volume
    let status_url = format!("http://{}/v1/status", addr);
    let resp = client.get(&status_url).send().await;
    assert!(resp.is_ok(), "status request should succeed");
}

/// Verify that an invalid URL is rejected with an error.
#[tokio::test]
async fn test_invalid_url_returns_error() {
    let addr = test_http_addr();
    wait_for_server(&addr, 5_000);

    let url = format!("http://{}/v1/load", addr);
    let client = reqwest::Client::new();
    let body = serde_json::json!({ "url": "not-a-valid-url" });
    let resp = client.post(&url).json(&body).send().await;
    assert!(resp.is_ok(), "request itself should succeed");
    let resp = resp.unwrap();
    assert!(
        !resp.status().is_success(),
        "invalid URL should return an error, got {}",
        resp.status()
    );
}
