//! HTTP API integration tests.
//!
//! These tests start an in-process HTTP server with a real session
//! manager (in-memory SQLite) and mock subsystems, verifying the
//! REST API endpoints work correctly end-to-end.

use picast_session::interfaces::{DisplayTrait, PlaybackTrait, ResolverTrait, TorTrait};
use picast_session::SessionManager;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

// ── Mock subsystems ──────────────────────────────────────────────────

struct MockResolver;

#[async_trait::async_trait]
impl ResolverTrait for MockResolver {
    async fn resolve(&self, url: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        Ok(format!("{}?resolved=1", url))
    }
}

struct MockPlayback {
    is_playing: AtomicBool,
    volume: StdMutex<f64>,
    last_seek_ms: StdMutex<u64>,
}

impl MockPlayback {
    fn new() -> Self {
        Self {
            is_playing: AtomicBool::new(false),
            volume: StdMutex::new(1.0),
            last_seek_ms: StdMutex::new(0),
        }
    }
}

#[async_trait::async_trait]
impl PlaybackTrait for MockPlayback {
    async fn play(
        &self,
        _url: &str,
        _socks_addr: &str,
        _isolation_username: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.is_playing.store(true, Ordering::Relaxed);
        Ok(())
    }
    async fn pause(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.is_playing.store(false, Ordering::Relaxed);
        Ok(())
    }
    async fn resume(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.is_playing.store(true, Ordering::Relaxed);
        Ok(())
    }
    async fn stop(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.is_playing.store(false, Ordering::Relaxed);
        Ok(())
    }
    async fn seek(&self, position_ms: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        *self.last_seek_ms.lock().unwrap() = position_ms;
        Ok(())
    }
    async fn set_volume(
        &self,
        volume: f64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        *self.volume.lock().unwrap() = volume;
        Ok(())
    }
    async fn position_ms(&self) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        Ok(0)
    }
    async fn duration_ms(&self) -> Result<Option<u64>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Some(300000))
    }
}

struct MockDisplay;

#[async_trait::async_trait]
impl DisplayTrait for MockDisplay {
    async fn acquire(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
    async fn release(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
    async fn resolution(&self) -> Result<(u32, u32), Box<dyn std::error::Error + Send + Sync>> {
        Ok((1920, 1080))
    }
}

struct MockTor {
    socks_addr: String,
}

#[async_trait::async_trait]
impl TorTrait for MockTor {
    async fn ensure_running(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
    fn socks_addr(&self) -> String {
        self.socks_addr.clone()
    }
    async fn health_check(&self) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        Ok(true)
    }
    fn isolation_username(&self, hostname: &str) -> String {
        picast_tor::stream_isolation_id(hostname)
    }
}

// ── Helper ───────────────────────────────────────────────────────────

fn make_session() -> Arc<SessionManager> {
    let resolver: Arc<dyn ResolverTrait> = Arc::new(MockResolver);
    let playback: Arc<dyn PlaybackTrait> = Arc::new(MockPlayback::new());
    let display: Arc<dyn DisplayTrait> = Arc::new(MockDisplay);
    let tor: Arc<dyn TorTrait> = Arc::new(MockTor { socks_addr: "127.0.0.1:9050".into() });

    Arc::new(SessionManager::with_subsystems(":memory:", resolver, playback, display, tor).unwrap())
}

/// Start the server and return (addr, handle).
async fn start_server(
    session: Arc<SessionManager>,
    port: u16,
) -> (String, tokio::task::JoinHandle<()>) {
    let addr = format!("127.0.0.1:{}", port);
    let addr_clone = addr.clone();
    let server = picast_protocols::HttpApiServer::new(&addr, session);
    let handle = tokio::spawn(async move {
        let _ = server
            .start(async {
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
            })
            .await;
    });
    // Give the server time to bind.
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    (addr_clone, handle)
}

// ── Tests ────────────────────────────────────────────────────────────

/// Verify that the health endpoint returns HTTP 200.
#[tokio::test]
async fn test_health_endpoint_returns_200() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18885).await;

    let url = format!("http://{}/api/health", addr);
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");

    handle.abort();
}

/// Verify that the status endpoint returns idle when no session is active.
#[tokio::test]
async fn test_status_endpoint_returns_idle() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18886).await;

    let url = format!("http://{}/api/status", addr);
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["state"], "idle");
    assert!(body["session_id"].is_null());

    handle.abort();
}

/// Verify that CORS headers are set on responses.
#[tokio::test]
async fn test_cors_headers_present() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18887).await;

    let url = format!("http://{}/api/health", addr);
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await.unwrap();
    let cors = resp.headers().get("access-control-allow-origin");
    assert!(cors.is_some(), "CORS header should be present");
    assert_eq!(cors.unwrap(), "*");

    let methods = resp.headers().get("access-control-allow-methods");
    assert!(methods.is_some(), "CORS methods header should be present");

    handle.abort();
}

/// Verify that unknown endpoints return 404.
#[tokio::test]
async fn test_unknown_endpoint_returns_404() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18888).await;

    let url = format!("http://{}/api/nonexistent", addr);
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 404);

    handle.abort();
}

/// Verify that OPTIONS preflight works.
#[tokio::test]
async fn test_options_preflight() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18889).await;

    let url = format!("http://{}/api/cast", addr);
    let client = reqwest::Client::new();
    let resp = client.request(reqwest::Method::OPTIONS, &url).send().await.unwrap();
    assert!(resp.status().is_success());

    // Check CORS headers on preflight.
    let cors = resp.headers().get("access-control-allow-origin");
    assert!(cors.is_some());

    handle.abort();
}

/// Verify that the cast endpoint accepts a URL and returns a session ID.
#[tokio::test]
async fn test_cast_endpoint_creates_session() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18890).await;

    let url = format!("http://{}/api/cast", addr);
    let client = reqwest::Client::new();
    let body = serde_json::json!({ "url": "https://example.com/video.mp4" });
    let resp = client.post(&url).json(&body).send().await.unwrap();

    // Should return 202 Accepted with session_id.
    assert_eq!(resp.status(), 202);

    let result: serde_json::Value = resp.json().await.unwrap();
    assert!(result["session_id"].is_string(), "should have session_id");
    assert!(!result["session_id"].as_str().unwrap().is_empty());
    assert_eq!(result["status"], "resolving");

    handle.abort();
}

/// Verify the full lifecycle: cast → status → pause → status → stop → status.
#[tokio::test]
async fn test_full_lifecycle_via_http() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18891).await;

    let client = reqwest::Client::new();
    let base = format!("http://{}", addr);

    // 1. Cast
    let resp = client
        .post(format!("{}/api/cast", base))
        .json(&serde_json::json!({ "url": "https://example.com/video.mp4" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let cast_result: serde_json::Value = resp.json().await.unwrap();
    let session_id = cast_result["session_id"].as_str().unwrap().to_string();

    // 2. Status — should show playing
    let resp = client.get(format!("{}/api/status", base)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let status: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(status["state"], "playing");
    assert_eq!(status["session_id"], session_id);

    // 3. Pause
    let resp = client.post(format!("{}/api/pause", base)).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    // 4. Status — should show paused
    let resp = client.get(format!("{}/api/status", base)).send().await.unwrap();
    let status: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(status["state"], "paused");

    // 5. Resume
    let resp = client.post(format!("{}/api/resume", base)).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    // 6. Status — should show playing again
    let resp = client.get(format!("{}/api/status", base)).send().await.unwrap();
    let status: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(status["state"], "playing");

    // 7. Volume
    let resp = client
        .post(format!("{}/api/volume", base))
        .json(&serde_json::json!({ "volume": 50 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // 8. Seek
    let resp = client
        .post(format!("{}/api/seek", base))
        .json(&serde_json::json!({ "position_ms": 30000 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // 9. Stop
    let resp = client.post(format!("{}/api/stop", base)).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    // 10. Status — should be idle
    let resp = client.get(format!("{}/api/status", base)).send().await.unwrap();
    let status: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(status["state"], "idle");

    handle.abort();
}

/// Verify that casting while already active returns 409 Conflict.
#[tokio::test]
async fn test_cast_while_active_returns_conflict() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18892).await;

    let client = reqwest::Client::new();
    let base = format!("http://{}", addr);

    // First cast should succeed.
    let resp = client
        .post(format!("{}/api/cast", base))
        .json(&serde_json::json!({ "url": "https://example.com/video.mp4" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    // Second cast should fail with 409.
    let resp = client
        .post(format!("{}/api/cast", base))
        .json(&serde_json::json!({ "url": "https://example.com/video2.mp4" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);

    handle.abort();
}

/// Verify that pause without active session returns error.
#[tokio::test]
async fn test_pause_without_session_returns_error() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18893).await;

    let client = reqwest::Client::new();
    let resp = client.post(format!("http://{}/api/pause", addr)).send().await.unwrap();
    assert!(resp.status().is_client_error());

    handle.abort();
}

/// Verify that stop without active session returns error.
#[tokio::test]
async fn test_stop_without_session_returns_error() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18894).await;

    let client = reqwest::Client::new();
    let resp = client.post(format!("http://{}/api/stop", addr)).send().await.unwrap();
    assert!(resp.status().is_client_error());

    handle.abort();
}

/// Verify volume endpoint with valid range.
#[tokio::test]
async fn test_volume_endpoint() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18895).await;

    let client = reqwest::Client::new();

    // First cast so we have an active session.
    let _ = client
        .post(format!("http://{}/api/cast", addr))
        .json(&serde_json::json!({ "url": "https://example.com/video.mp4" }))
        .send()
        .await
        .unwrap();

    // Set volume.
    let resp = client
        .post(format!("http://{}/api/volume", addr))
        .json(&serde_json::json!({ "volume": 75 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["volume"], 75);

    handle.abort();
}

/// Verify seek endpoint with both ms and seconds.
#[tokio::test]
async fn test_seek_endpoint() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18896).await;

    let client = reqwest::Client::new();

    // First cast.
    let _ = client
        .post(format!("http://{}/api/cast", addr))
        .json(&serde_json::json!({ "url": "https://example.com/video.mp4" }))
        .send()
        .await
        .unwrap();

    // Seek with position_ms.
    let resp = client
        .post(format!("http://{}/api/seek", addr))
        .json(&serde_json::json!({ "position_ms": 60000 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Seek with position_seconds.
    let resp = client
        .post(format!("http://{}/api/seek", addr))
        .json(&serde_json::json!({ "position_seconds": 30.5 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    handle.abort();
}
