//! HTTP API integration tests.
//!
//! These tests start an in-process HTTP server with a bare session
//! manager and verify the REST API endpoints work correctly.

use picast_session::SessionManager;
use std::sync::Arc;

/// Verify that the health endpoint returns HTTP 200.
/// Uses a simple direct server test rather than spawning a background task.
#[tokio::test]
async fn test_health_endpoint_returns_200() {
    let session = Arc::new(SessionManager::new(":memory:").unwrap());
    let addr = "127.0.0.1:18885";

    let server = picast_protocols::HttpApiServer::new(addr, session);

    // Run the server in a background task.
    let handle = tokio::spawn(async move {
        let _ = server.start(async {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }).await;
    });

    // Give the server time to bind.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let url = format!("http://{}/api/health", addr);
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await;
    assert!(resp.is_ok(), "health request should succeed");
    let resp = resp.unwrap();
    assert_eq!(resp.status(), 200, "health endpoint should return 200");

    handle.abort();
}

/// Verify that the status endpoint returns idle when no session is active.
#[tokio::test]
async fn test_status_endpoint_returns_idle() {
    let session = Arc::new(SessionManager::new(":memory:").unwrap());
    let addr = "127.0.0.1:18886";

    let server = picast_protocols::HttpApiServer::new(addr, session);
    let handle = tokio::spawn(async move {
        let _ = server.start(async {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }).await;
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let url = format!("http://{}/api/status", addr);
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await;
    assert!(resp.is_ok(), "status request should succeed");
    let resp = resp.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["state"], "idle");

    handle.abort();
}

/// Verify that CORS headers are set on responses.
#[tokio::test]
async fn test_cors_headers_present() {
    let session = Arc::new(SessionManager::new(":memory:").unwrap());
    let addr = "127.0.0.1:18887";

    let server = picast_protocols::HttpApiServer::new(addr, session);
    let handle = tokio::spawn(async move {
        let _ = server.start(async {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }).await;
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let url = format!("http://{}/api/health", addr);
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await.unwrap();
    let cors = resp.headers().get("access-control-allow-origin");
    assert!(cors.is_some(), "CORS header should be present");
    assert_eq!(cors.unwrap(), "*");

    handle.abort();
}

/// Verify that unknown endpoints return 404.
#[tokio::test]
async fn test_unknown_endpoint_returns_404() {
    let session = Arc::new(SessionManager::new(":memory:").unwrap());
    let addr = "127.0.0.1:18888";

    let server = picast_protocols::HttpApiServer::new(addr, session);
    let handle = tokio::spawn(async move {
        let _ = server.start(async {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }).await;
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let url = format!("http://{}/api/nonexistent", addr);
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await;
    assert!(resp.is_ok());
    let resp = resp.unwrap();
    assert_eq!(resp.status(), 404);

    handle.abort();
}

/// Verify that OPTIONS preflight works.
#[tokio::test]
async fn test_options_preflight() {
    let session = Arc::new(SessionManager::new(":memory:").unwrap());
    let addr = "127.0.0.1:18889";

    let server = picast_protocols::HttpApiServer::new(addr, session);
    let handle = tokio::spawn(async move {
        let _ = server.start(async {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }).await;
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let url = format!("http://{}/api/cast", addr);
    let client = reqwest::Client::new();
    let resp = client.request(reqwest::Method::OPTIONS, &url).send().await;
    assert!(resp.is_ok(), "OPTIONS request should succeed");
    let resp = resp.unwrap();
    assert!(resp.status().is_success(), "OPTIONS should return success");

    handle.abort();
}

/// Verify that the cast endpoint accepts a URL.
#[tokio::test]
async fn test_cast_endpoint_accepts_url() {
    let session = Arc::new(SessionManager::new(":memory:").unwrap());
    let addr = "127.0.0.1:18890";

    let server = picast_protocols::HttpApiServer::new(addr, session);
    let handle = tokio::spawn(async move {
        let _ = server.start(async {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }).await;
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let url = format!("http://{}/api/cast", addr);
    let client = reqwest::Client::new();
    let body = serde_json::json!({ "url": "https://example.com/video.mp4" });
    let resp = client.post(&url).json(&body).send().await;
    assert!(resp.is_ok(), "cast request should succeed");

    handle.abort();
}
