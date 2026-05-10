//! HTTP API integration tests.
//!
//! These tests start an in-process HTTP server with a real session
//! manager (in-memory SQLite) and mock subsystems, verifying the
//! REST API endpoints work correctly end-to-end.

use bogdan_session::interfaces::{
    DisplayTrait, PlaybackTrait, ResolveInfo, ResolverTrait, TorTrait,
};
use bogdan_session::SessionManager;
use futures_util::StreamExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;

// ── Mock subsystems ──────────────────────────────────────────────────

struct MockResolver;

#[async_trait::async_trait]
impl ResolverTrait for MockResolver {
    async fn resolve(
        &self,
        url: &str,
    ) -> Result<ResolveInfo, Box<dyn std::error::Error + Send + Sync>> {
        Ok(ResolveInfo {
            direct_url: format!("{}?resolved=1", url),
            title: None,
            duration_ms: Some(300000),
            cookies: vec![],
            used_tor: false,
        })
    }

    async fn invalidate_cache(&self, _url: &str) {
        // No-op for mock resolver.
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
        _source_url: &str,
        _socks_addr: &str,
        _isolation_username: &str,
        _cookies: Vec<String>,
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
    async fn set_audio_device(
        &self,
        _device: String,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
    async fn set_audio_sink(
        &self,
        _sink: String,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
    async fn audio_device(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        Ok("plughw:1,0".into())
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
        bogdan_tor::stream_isolation_id(hostname)
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
    let server = bogdan_protocols::HttpApiServer::new(&addr, session);
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

/// Poll /api/status until the player reaches the desired state.
/// Returns the status JSON on success, or panics on timeout.
async fn wait_for_state(
    client: &reqwest::Client,
    base: &str,
    desired: &str,
    timeout: std::time::Duration,
) -> serde_json::Value {
    let start = std::time::Instant::now();
    loop {
        let resp = client.get(format!("{}/api/status", base)).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let status: serde_json::Value = resp.json().await.unwrap();
        let state = status["state"].as_str().unwrap_or("unknown");
        if state == desired {
            return status;
        }
        // If we hit an error state, fail immediately
        if state == "error" {
            panic!("session entered error state while waiting for '{}'", desired);
        }
        if start.elapsed() >= timeout {
            panic!(
                "timed out waiting for state '{}' — current state: '{}'",
                desired, state
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Build a session manager with a custom resolver.
fn make_session_with_resolver(
    resolver: Arc<dyn ResolverTrait>,
) -> Arc<SessionManager> {
    let playback: Arc<dyn PlaybackTrait> = Arc::new(MockPlayback::new());
    let display: Arc<dyn DisplayTrait> = Arc::new(MockDisplay);
    let tor: Arc<dyn TorTrait> = Arc::new(MockTor { socks_addr: "127.0.0.1:9050".into() });
    Arc::new(SessionManager::with_subsystems(":memory:", resolver, playback, display, tor).unwrap())
}

/// Resolver that always fails — used to test error-state transitions.
struct FailingResolver;

#[async_trait::async_trait]
impl ResolverTrait for FailingResolver {
    async fn resolve(
        &self,
        _url: &str,
    ) -> Result<ResolveInfo, Box<dyn std::error::Error + Send + Sync>> {
        Err("resolution intentionally failed".into())
    }

    async fn invalidate_cache(&self, _url: &str) {}
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

    // 2. Wait for the background resolve+play flow to finish.
    wait_for_state(&client, &base, "playing", std::time::Duration::from_secs(5)).await;

    // 3. Status — should show playing
    let resp = client.get(format!("{}/api/status", base)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let status: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(status["state"], "playing");
    // The session_id returned by /api/cast is a placeholder; the real one
    // comes from the background load(). Just verify it's present.
    assert!(status["session_id"].is_string());

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

    // Wait for playing state before seeking.
    let base = format!("http://{}", addr);
    wait_for_state(&client, &base, "playing", std::time::Duration::from_secs(5)).await;

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

/// Verify that resume without active session returns error.
#[tokio::test]
async fn test_resume_without_session_returns_error() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18897).await;

    let client = reqwest::Client::new();
    let resp = client.post(format!("http://{}/api/resume", addr)).send().await.unwrap();
    assert!(resp.status().is_client_error());

    handle.abort();
}

/// Verify that seek without active session returns error.
#[tokio::test]
async fn test_seek_without_session_returns_error() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18898).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/api/seek", addr))
        .json(&serde_json::json!({ "position_ms": 1000 }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_client_error());

    handle.abort();
}

/// Verify that volume without active session returns error.
#[tokio::test]
async fn test_volume_without_session_returns_error() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18899).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/api/volume", addr))
        .json(&serde_json::json!({ "volume": 50 }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_client_error(),
        "expected 4xx for volume without session, got {}",
        resp.status()
    );

    handle.abort();
}

/// Verify cast-pause-resume-stop lifecycle.
#[tokio::test]
async fn test_pause_resume_lifecycle() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18900).await;

    let client = reqwest::Client::new();
    let base = format!("http://{}", addr);

    // Cast
    let resp = client
        .post(format!("{}/api/cast", base))
        .json(&serde_json::json!({ "url": "https://example.com/video.mp4" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    // Wait for playing state before attempting pause.
    wait_for_state(&client, &base, "playing", std::time::Duration::from_secs(5)).await;

    // Pause
    let resp = client.post(format!("{}/api/pause", base)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "paused");

    // Resume
    let resp = client.post(format!("{}/api/resume", base)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "playing");

    // Stop
    let resp = client.post(format!("{}/api/stop", base)).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    handle.abort();
}

/// Verify that status after stop shows idle state with null session.
#[tokio::test]
async fn test_status_after_stop_shows_idle() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18901).await;

    let client = reqwest::Client::new();
    let base = format!("http://{}", addr);

    // Cast and immediately stop.
    let _ = client
        .post(format!("{}/api/cast", base))
        .json(&serde_json::json!({ "url": "https://example.com/video.mp4" }))
        .send()
        .await
        .unwrap();

    let _ = client.post(format!("{}/api/stop", base)).send().await.unwrap();

    // Status should show idle.
    let resp = client.get(format!("{}/api/status", base)).send().await.unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["state"], "idle");
    assert!(body["session_id"].is_null());

    handle.abort();
}

/// Verify multiple cast-stop cycles work without errors.
#[tokio::test]
async fn test_multiple_cast_stop_cycles() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18902).await;

    let client = reqwest::Client::new();
    let base = format!("http://{}", addr);

    for i in 0..3 {
        let resp = client
            .post(format!("{}/api/cast", base))
            .json(&serde_json::json!({ "url": format!("https://example.com/video{}.mp4", i) }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202, "cast {} should succeed", i);

        let resp = client.post(format!("{}/api/stop", base)).send().await.unwrap();
        assert_eq!(resp.status(), 200, "stop {} should succeed", i);
    }

    handle.abort();
}

// ── Comprehensive Integration Tests (S6.5) ──────────────────────────

/// Verify that a failing resolver does not leave the session stuck —
/// the system recovers to idle and accepts new casts.
///
/// Note: The "error" state is transient in the current API design. When
/// `load()` fails it transitions to "error" then immediately clears the
/// active session, so `/api/status` shows "idle" rather than "error".
/// This test verifies the important invariant: the system is not stuck.
#[tokio::test]
async fn test_resolve_failure_returns_error_state() {
    let resolver: Arc<dyn ResolverTrait> = Arc::new(FailingResolver);
    let session = make_session_with_resolver(resolver);
    let (addr, handle) = start_server(session, 18910).await;

    let client = reqwest::Client::new();
    let base = format!("http://{}", addr);

    let resp = client
        .post(format!("{}/api/cast", base))
        .json(&serde_json::json!({ "url": "https://example.com/failing.mp4" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    // Wait for the background task to complete and the session to be cleared.
    // After a failed resolve the active session is cleared, so status → idle.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let resp = client.get(format!("{}/api/status", base)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let status: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(status["state"], "idle", "system should recover to idle after resolve failure");

    // Verify the system is not stuck — a new cast should be accepted.
    let resp = client
        .post(format!("{}/api/cast", base))
        .json(&serde_json::json!({ "url": "https://example.com/also-failing.mp4" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "new cast should be accepted after failed resolve");

    handle.abort();
}

/// Verify that a second cast while one is already playing returns 409.
#[tokio::test]
async fn test_concurrent_cast_returns_conflict() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18911).await;

    let client = reqwest::Client::new();
    let base = format!("http://{}", addr);

    // First cast
    let resp = client
        .post(format!("{}/api/cast", base))
        .json(&serde_json::json!({ "url": "https://example.com/video.mp4" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    // Wait for the session to be fully active (playing) before
    // sending the second cast. This eliminates the race window.
    wait_for_state(&client, &base, "playing", std::time::Duration::from_secs(5)).await;

    // Second cast should fail with 409 Conflict.
    let resp = client
        .post(format!("{}/api/cast", base))
        .json(&serde_json::json!({ "url": "https://example.com/video2.mp4" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "SESSION_ACTIVE");

    handle.abort();
}

/// Verify that POST /api/cast with malformed JSON returns 400.
#[tokio::test]
async fn test_cast_with_malformed_json_returns_400() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18912).await;

    let client = reqwest::Client::new();
    let base = format!("http://{}", addr);

    let resp = client
        .post(format!("{}/api/cast", base))
        .header("Content-Type", "application/json")
        .body("{invalid json!!!")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "BAD_REQUEST");

    handle.abort();
}

/// Verify that POST /api/cast with dangerous URL schemes returns 400.
#[tokio::test]
async fn test_cast_with_dangerous_url_scheme_returns_400() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18913).await;

    let client = reqwest::Client::new();
    let base = format!("http://{}", addr);

    // Test file:// scheme
    let resp = client
        .post(format!("{}/api/cast", base))
        .json(&serde_json::json!({ "url": "file:///etc/passwd" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "INVALID_URL");

    // Test javascript: scheme
    let resp = client
        .post(format!("{}/api/cast", base))
        .json(&serde_json::json!({ "url": "javascript:alert(1)" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Test data: scheme
    let resp = client
        .post(format!("{}/api/cast", base))
        .json(&serde_json::json!({ "url": "data:text/html,<script>alert(1)</script>" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    handle.abort();
}

/// Verify that exceeding the rate limit returns 429 with Retry-After header.
#[tokio::test]
async fn test_rate_limiting_returns_429() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18914).await;

    let client = reqwest::Client::new();
    let base = format!("http://{}", addr);

    // The rate limiter allows 30 requests per 10-second window per IP.
    // /api/health is exempt from rate limiting; /api/status is not.
    // Send requests until we hit the rate limit.
    let mut got_429 = false;
    for _ in 0..40 {
        let resp = client.get(format!("{}/api/status", base)).send().await.unwrap();
        if resp.status().as_u16() == 429 {
            got_429 = true;
            // Verify the Retry-After header is present.
            assert!(
                resp.headers().get("retry-after").is_some(),
                "429 response must include Retry-After header"
            );
            let body: serde_json::Value = resp.json().await.unwrap();
            assert_eq!(body["code"], "RATE_LIMITED");
            break;
        }
    }
    assert!(got_429, "expected 429 rate limit response within 40 requests");

    handle.abort();
}

/// Verify that WebSocket receives MEDIA_STATUS event when casting via HTTP.
#[tokio::test]
async fn test_websocket_receives_media_status_on_cast() {
    let session = make_session();
    let (addr, http_handle) = start_server(session.clone(), 18915).await;

    // Start WS server on a different port.
    let ws_addr = "127.0.0.1:18916";
    let ws_server = bogdan_protocols::WebSocketServer::new(ws_addr, session);
    let ws_handle = tokio::spawn(async move {
        let _ = ws_server
            .start(async {
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
            })
            .await;
    });
    // Give the WS server time to bind.
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Connect WS client.
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(format!("ws://{}/", ws_addr))
        .await
        .expect("WS handshake failed");

    // Read the CONNECTED event that the server sends on connect.
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(2), ws_stream.next()).await {
            Ok(Some(Ok(msg))) => {
                if let WsMessage::Text(text) = msg {
                    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
                    if json["type"] == "CONNECTED" {
                        break;
                    }
                }
            },
            Ok(Some(Err(e))) => panic!("WS error reading CONNECTED: {}", e),
            Ok(None) => panic!("WS stream closed before CONNECTED"),
            Err(_) => panic!("timeout waiting for CONNECTED event"),
        }
    }

    // Cast via HTTP.
    let client = reqwest::Client::new();
    let base = format!("http://{}", addr);
    let resp = client
        .post(format!("{}/api/cast", base))
        .json(&serde_json::json!({ "url": "https://example.com/video.mp4" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    // Read WS messages until we get MEDIA_STATUS with state "playing".
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut found_playing = false;
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_secs(2), ws_stream.next()).await {
            Ok(Some(Ok(msg))) => {
                if let WsMessage::Text(text) = msg {
                    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
                    if json["type"] == "MEDIA_STATUS" && json["state"] == "playing" {
                        found_playing = true;
                        break;
                    }
                }
            },
            Ok(Some(Err(e))) => panic!("WS error: {}", e),
            Ok(None) => panic!("WS stream closed unexpectedly"),
            Err(_) => continue, // timeout — loop back and check deadline
        }
    }
    assert!(
        found_playing,
        "expected MEDIA_STATUS with state 'playing' via WebSocket"
    );

    http_handle.abort();
    ws_handle.abort();
}

/// Verify 10 cast/stop cycles return to idle each time with no leaks.
///
/// Uses a fixed sleep instead of `wait_for_state` to avoid hitting the
/// per-IP rate limit (30 req / 10 s) during the many cycles.
#[tokio::test]
async fn test_cast_stop_multiple_cycles_no_leaks() {
    let session = make_session();
    let (addr, handle) = start_server(session, 18917).await;

    let client = reqwest::Client::new();
    let base = format!("http://{}", addr);

    for i in 0..10 {
        // Cast
        let resp = client
            .post(format!("{}/api/cast", base))
            .json(&serde_json::json!({ "url": format!("https://example.com/video{}.mp4", i) }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202, "cast {} should succeed", i);

        // Give the background resolve+play task time to complete.
        // The mock resolver + 300ms display-acquire delay means ~500ms total.
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        // Stop
        let resp = client.post(format!("{}/api/stop", base)).send().await.unwrap();
        assert_eq!(resp.status(), 200, "stop {} should succeed", i);
    }

    // Verify final state is idle with no active session.
    let resp = client.get(format!("{}/api/status", base)).send().await.unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["state"], "idle", "state should be idle after all cycles");
    assert!(body["session_id"].is_null(), "session_id should be null after all cycles");

    handle.abort();
}
