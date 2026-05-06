//! PiCast Server Entry Point
//!
//! Initializes the tracing subscriber, loads configuration, wires up all
//! subsystems (session manager, resolver, playback, display, Tor), and
//! runs the main event loop with graceful shutdown on SIGINT / SIGTERM.
//!
//! ## Startup Order
//!
//! ```text
//! Tor → Display → Playback → Resolver → Session → HTTP → WebSocket → DLNA
//! ```
//!
//! Each subsystem depends on the ones before it. If any fails to
//! initialise, the server exits with an error.
//!
//! When compiled without the `hw` feature, the display and playback
//! subsystems run in mock mode — the server starts for protocol testing
//! but actual media playback is unavailable.

mod config;

use anyhow::Result;
use config::AppConfig;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

// ── Trait adapters ───────────────────────────────────────────────────
//
// These wrappers implement the session trait interfaces by delegating
// to the concrete subsystem types. They bridge the gap between the
// concrete crates and the session manager's trait-object requirements.

/// Adapter: `picast_tor::TorManager` → `TorTrait`
struct TorAdapter(Arc<picast_tor::TorManager>);

#[async_trait::async_trait]
impl picast_session::interfaces::TorTrait for TorAdapter {
    async fn ensure_running(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.0.ensure_running(30000).await?;
        Ok(())
    }

    fn socks_addr(&self) -> String {
        self.0.socks_addr()
    }

    async fn health_check(&self) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let health = self.0.health_check().await?;
        Ok(health.is_healthy)
    }

    fn isolation_username(&self, hostname: &str) -> String {
        picast_tor::TorManager::isolation_username(hostname)
    }
}

/// Adapter: `picast_display::DisplayManager` → `DisplayTrait`
struct DisplayAdapter(Arc<tokio::sync::Mutex<picast_display::DisplayManager>>);

#[async_trait::async_trait]
impl picast_session::interfaces::DisplayTrait for DisplayAdapter {
    async fn acquire(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut dm = self.0.lock().await;
        dm.acquire()?;
        Ok(())
    }

    async fn release(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut dm = self.0.lock().await;
        dm.release()?;
        Ok(())
    }

    async fn resolution(&self) -> Result<(u32, u32), Box<dyn std::error::Error + Send + Sync>> {
        let dm = self.0.lock().await;
        Ok(dm.resolution()?)
    }
}

/// Adapter: `picast_playback::PlaybackEngine` → `PlaybackTrait`
struct PlaybackAdapter(Arc<picast_playback::PlaybackEngine>);

#[async_trait::async_trait]
impl picast_session::interfaces::PlaybackTrait for PlaybackAdapter {
    async fn play(
        &self,
        url: &str,
        socks_addr: &str,
        isolation_username: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.0.play(url, socks_addr, isolation_username).await?;
        Ok(())
    }

    async fn pause(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.0.pause().await?;
        Ok(())
    }

    async fn resume(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.0.resume().await?;
        Ok(())
    }

    async fn stop(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.0.stop().await?;
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.0.seek(position_ms).await?;
        Ok(())
    }

    async fn set_volume(
        &self,
        volume: f64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.0.set_volume(volume).await?;
        Ok(())
    }

    async fn position_ms(&self) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        Ok(self.0.position_ms().await?)
    }

    async fn duration_ms(&self) -> Result<Option<u64>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(self.0.duration_ms().await?)
    }
}

/// Adapter: `picast_resolver::Resolver` → `ResolverTrait`
struct ResolverAdapter(Arc<picast_resolver::Resolver>);

#[async_trait::async_trait]
impl picast_session::interfaces::ResolverTrait for ResolverAdapter {
    async fn resolve(&self, url: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let result = self.0.resolve(url).await?;
        Ok(result.direct_url)
    }
}

// AppConfig is now in config.rs with full TOML support.

/// Initialize the `tracing-subscriber` with an `env-filter`.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    // ── 1. Logging ────────────────────────────────────────────────────
    init_tracing();
    info!("PiCast starting …");

    // ── 2. Configuration ──────────────────────────────────────────────
    let config = AppConfig::load().unwrap_or_else(|e| {
        error!(error = %e, "failed to load configuration");
        std::process::exit(1);
    });
    info!(
        http_addr = %config.server.http_addr,
        ws_addr = %config.server.ws_addr,
        dlna_name = %config.dlna.friendly_name,
        tor_socks = %config.tor.socks_addr,
        "configuration loaded"
    );

    // ── 3. Shutdown signal ────────────────────────────────────────────
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
        .expect("failed to install SIGTERM handler");
    let shutdown_signal = async {
        tokio::select! {
            _ = signal::ctrl_c() => info!("received SIGINT"),
            _ = sigterm.recv() => info!("received SIGTERM"),
        }
    };

    // ── 4. Subsystem initialisation ───────────────────────────────────
    info!("initialising subsystems");

    // 4a. Tor
    let tor_manager = Arc::new(picast_tor::TorManager::new(&config.tor.socks_addr));
    info!(socks = %config.tor.socks_addr, "Tor manager created");

    // 4b. Display
    #[cfg(feature = "hw")]
    let display_manager: Arc<tokio::sync::Mutex<picast_display::DisplayManager>> = {
        let dm = picast_display::DisplayManager::new(&config.display.drm_device)?;
        Arc::new(tokio::sync::Mutex::new(dm))
    };
    #[cfg(not(feature = "hw"))]
    let display_manager: Arc<tokio::sync::Mutex<picast_display::DisplayManager>> = {
        info!("hw feature disabled — display manager running in mock mode");
        let dm = picast_display::DisplayManager::new(&config.display.drm_device)?;
        Arc::new(tokio::sync::Mutex::new(dm))
    };
    info!(device = %config.display.drm_device, "Display manager created");

    // 4c. Playback
    #[cfg(feature = "hw")]
    let playback_engine: Arc<picast_playback::PlaybackEngine> = {
        Arc::new(picast_playback::PlaybackEngine::new(picast_playback::PipelineConfig::default())?)
    };
    #[cfg(not(feature = "hw"))]
    let playback_engine: Arc<picast_playback::PlaybackEngine> = {
        info!("hw feature disabled — playback engine running in mock mode");
        Arc::new(picast_playback::PlaybackEngine::new(picast_playback::PipelineConfig::default())?)
    };
    info!("Playback engine created");

    // 4d. Resolver
    let resolver = Arc::new(picast_resolver::Resolver::new(tor_manager.clone()));
    info!("Resolver created");

    // ── 5. Session manager ────────────────────────────────────────────
    // Wrap concrete types in trait adapters and wire them into the
    // SessionManager so it can drive each subsystem through its trait
    // interface.
    let tor_trait: Arc<dyn picast_session::interfaces::TorTrait> =
        Arc::new(TorAdapter(tor_manager.clone()));
    let display_trait: Arc<dyn picast_session::interfaces::DisplayTrait> =
        Arc::new(DisplayAdapter(display_manager.clone()));
    let playback_trait: Arc<dyn picast_session::interfaces::PlaybackTrait> =
        Arc::new(PlaybackAdapter(playback_engine.clone()));
    let resolver_trait: Arc<dyn picast_session::interfaces::ResolverTrait> =
        Arc::new(ResolverAdapter(resolver.clone()));

    let session = Arc::new(picast_session::SessionManager::with_subsystems(
        &config.server.db_path,
        resolver_trait,
        playback_trait,
        display_trait,
        tor_trait,
    )?);
    info!(db = %config.server.db_path, "Session manager created (subsystems wired)");

    // ── 6. Protocol servers ───────────────────────────────────────────
    let http_server =
        picast_protocols::HttpApiServer::new(&config.server.http_addr, session.clone());
    let ws_server = picast_protocols::WebSocketServer::new(&config.server.ws_addr, session.clone());
    let dlna_renderer =
        picast_protocols::DlnaRenderer::new(&config.dlna.friendly_name, &config.tor.socks_addr);

    info!("all components initialised");

    // ── 7. Start servers ──────────────────────────────────────────────
    let shutdown_http = shutdown_tx.subscribe();
    let shutdown_ws = shutdown_tx.subscribe();

    let http_handle = tokio::spawn(async move {
        if let Err(e) = http_server
            .start(async {
                let mut rx = shutdown_http;
                let _ = rx.recv().await;
            })
            .await
        {
            error!(error = %e, "HTTP server error");
        }
    });

    let ws_handle = tokio::spawn(async move {
        if let Err(e) = ws_server
            .start(async {
                let mut rx = shutdown_ws;
                let _ = rx.recv().await;
            })
            .await
        {
            error!(error = %e, "WebSocket server error");
        }
    });

    // Start DLNA renderer (non-blocking, may fail if gmediarender not installed).
    let dlna_handle = tokio::spawn(async move {
        if let Err(e) = dlna_renderer.start().await {
            warn!(error = %e, "DLNA renderer failed to start — DLNA casting will be unavailable");
        }
    });

    // ── 8. Run until shutdown ─────────────────────────────────────────
    shutdown_signal.await;

    // Signal all tasks to wind down.
    let _ = shutdown_tx.send(());
    info!("shutdown signal broadcast — waiting for tasks to finish …");

    // Wait for servers to stop (with timeout).
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let _ = http_handle.await;
        let _ = ws_handle.await;
        let _ = dlna_handle.await;
    })
    .await;

    // Shutdown Tor if we own it.
    if let Err(e) = tor_manager.shutdown().await {
        warn!(error = %e, "Tor shutdown error");
    }

    info!("PiCast stopped.");
    Ok(())
}
