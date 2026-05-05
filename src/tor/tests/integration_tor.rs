//! T-1.6: Tor Integration Test
//!
//! End-to-end test that spawns a real Tor daemon, verifies SOCKS5 connectivity,
//! generates stream isolation IDs, and shuts down cleanly.
//!
//! **This test requires a running Tor daemon.** It is only compiled when the
//! `tor-test` feature is enabled:
//!
//! ```sh
//! cargo test -p picast-tor --features tor-test -- --nocapture
//! ```

#[cfg(feature = "tor-test")]
use picast_tor::TorManager;

/// Full lifecycle test: create → ensure_running → health_check → stream IDs → shutdown.
#[tokio::test]
#[cfg(feature = "tor-test")]
async fn test_tor_lifecycle() {
    // Use a non-standard SOCKS port to avoid conflicts with any
    // system Tor instance.
    let mgr = TorManager::new("127.0.0.1:19050");

    // 1. Start Tor (or detect an existing instance on that port).
    let startup_result = mgr.ensure_running(60_000).await;
    if let Err(e) = &startup_result {
        eprintln!("NOTE: Tor not available on port 19050 — skipping test. Error: {}", e);
        return;
    }

    // 2. Verify SOCKS5 health.
    let health = mgr.health_check().await.expect("health check failed");
    assert!(health.is_healthy, "Tor SOCKS proxy should be healthy after startup");

    // 3. Stream isolation IDs are deterministic.
    let id1 = TorManager::isolation_username("youtube.com");
    let id2 = TorManager::isolation_username("youtube.com");
    let id3 = TorManager::isolation_username("vimeo.com");
    assert_eq!(id1, id2, "same domain should produce same isolation ID");
    assert_ne!(id1, id3, "different domains should produce different isolation IDs");

    // 4. Clean shutdown.
    mgr.shutdown().await.expect("shutdown failed");
}
