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

/// PiCast version — set at build time via `env!` / `cargo:rerun-if-changed`.
const VERSION: &str = env!("CARGO_PKG_VERSION");

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

    async fn set_audio_device(
        &self,
        device: String,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.0.set_audio_device(device);
        Ok(())
    }

    async fn audio_device(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        Ok(self.0.audio_device())
    }
}

/// Adapter: `picast_resolver::Resolver` → `ResolverTrait`
struct ResolverAdapter(Arc<picast_resolver::Resolver>);

#[async_trait::async_trait]
impl picast_session::interfaces::ResolverTrait for ResolverAdapter {
    async fn resolve(
        &self,
        url: &str,
    ) -> Result<picast_session::interfaces::ResolveInfo, Box<dyn std::error::Error + Send + Sync>>
    {
        let result = self.0.resolve(url).await?;
        Ok(picast_session::interfaces::ResolveInfo {
            direct_url: result.direct_url,
            title: result.title,
            duration_ms: result.duration,
        })
    }
}

// AppConfig is now in config.rs with full TOML support.

/// Initialize the `tracing-subscriber` with an `env-filter`.
///
/// The log level defaults to `info` but can be overridden via:
/// 1. The `PICAST_LOG_LEVEL` environment variable
/// 2. The `RUST_LOG` environment variable (standard tracing convention)
/// 3. The `logging.level` field in the TOML config file
fn init_tracing(config_level: &str) {
    use tracing_subscriber::EnvFilter;
    let level_directive = config_level
        .parse()
        .unwrap_or_else(|_| "info".parse().unwrap());
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(level_directive))
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .init();
}

/// Parse command-line arguments.
///
/// Supports `--version`, `--help`, and `--config <path>`.
/// Everything else is handled by the TOML config + env vars.
fn parse_cli_args() -> clap::ArgMatches {
    clap::Command::new("picast")
        .version(VERSION)
        .about("PiCast — Tor-routed media casting appliance")
        .arg(
            clap::Arg::new("config")
                .short('c')
                .long("config")
                .value_name("FILE")
                .help("Path to picast.toml configuration file"),
        )
        .get_matches()
}

#[tokio::main]
async fn main() -> Result<()> {
    // ── 0a. Install rustls CryptoProvider ─────────────────────────────
    // reqwest/rustls-tls pulls in `ring` while tokio-rustls 0.26 pulls
    // in `aws-lc-rs`.  When both features are present rustls refuses to
    // auto-select a provider, so we pick one explicitly before any TLS
    // code runs.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls ring CryptoProvider");

    // ── 0b. CLI arguments ─────────────────────────────────────────────
    let cli = parse_cli_args();

    // If --config was given, set PICAST_CONFIG before loading config.
    if let Some(config_path) = cli.get_one::<String>("config") {
        std::env::set_var("PICAST_CONFIG", config_path);
    }

    // ── 1. Configuration ──────────────────────────────────────────────
    let config = AppConfig::load().unwrap_or_else(|e| {
        eprintln!("failed to load configuration: {}", e);
        std::process::exit(1);
    });

    // ── 2. Logging ────────────────────────────────────────────────────
    init_tracing(&config.logging.level);
    info!("PiCast starting …");
    info!(
        http_addr = %config.server.http_addr,
        ws_addr = %config.server.ws_addr,
        dlna_name = %config.dlna.friendly_name,
        tor_socks = %config.tor.socks_addr,
        tor_control_port = config.tor.control_port,
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
    let tor_manager = Arc::new(
        picast_tor::TorManager::new(&config.tor.socks_addr)
            .with_control_port(config.tor.control_port)
            .with_cookie_path(&config.tor.cookie_path),
    );
    info!(
        socks = %config.tor.socks_addr,
        control_port = config.tor.control_port,
        cookie_path = %config.tor.cookie_path,
        "Tor manager created"
    );

    // Start the Tor background monitor — watches for process crashes
    // and queries circuit health from the control port every 30 seconds.
    tor_manager.start_monitor();
    info!("Tor background monitor started");

    // 4b. Display
    //
    // Create the display manager and acquire DRM resources immediately.
    // This enumerates connectors/CRTCs/planes and caches them, then closes
    // the DRM fd so kmssink can open it fresh and become DRM master.
    // The connector_id is passed to the PlaybackEngine so kmssink uses
    // the correct HDMI output explicitly rather than relying on auto-detect.
    #[cfg(feature = "hw")]
    let (display_manager, connector_id) = {
        let mut dm = picast_display::DisplayManager::new(&config.display.drm_device)?;
        if let Err(e) = dm.acquire() {
            warn!(error = %e, "display acquire failed at startup — kmssink will auto-detect display");
        }
        let conn_id = dm.active_connector_id();
        if let Some(id) = conn_id {
            info!(connector_id = id, "display acquired — will pass to playback engine");
        }
        (Arc::new(tokio::sync::Mutex::new(dm)), conn_id)
    };
    #[cfg(not(feature = "hw"))]
    let display_manager: Arc<tokio::sync::Mutex<picast_display::DisplayManager>> = {
        info!("hw feature disabled — display manager running in mock mode");
        let dm = picast_display::DisplayManager::new(&config.display.drm_device)?;
        Arc::new(tokio::sync::Mutex::new(dm))
    };
    #[cfg(not(feature = "hw"))]
    let connector_id: Option<u32> = None;
    info!(device = %config.display.drm_device, "Display manager created");

    // 4c. Playback
    //
    // Create the playback engine with display info from the DisplayManager.
    // Passing connector_id explicitly ensures kmssink renders to the correct
    // HDMI output — auto-detect can misdetect on multi-output setups.
    let mut pipeline_config = picast_playback::PipelineConfig::default();
    if let Some(conn_id) = connector_id {
        pipeline_config.connector_id = Some(conn_id);
        info!(connector_id = conn_id, "playback engine configured with explicit connector ID");
    }
    if !config.playback.audio_device.is_empty() {
        pipeline_config.audio_device = config.playback.audio_device.clone();
        info!(audio_device = %config.playback.audio_device, "playback engine configured with explicit ALSA device from config");
    }
    info!(audio_device = %pipeline_config.audio_device, "playback engine will use ALSA device");
    #[cfg(feature = "hw")]
    let playback_engine: Arc<picast_playback::PlaybackEngine> = {
        Arc::new(picast_playback::PlaybackEngine::new(pipeline_config)?)
    };
    #[cfg(not(feature = "hw"))]
    let playback_engine: Arc<picast_playback::PlaybackEngine> = {
        info!("hw feature disabled — playback engine running in mock mode");
        Arc::new(picast_playback::PlaybackEngine::new(pipeline_config)?)
    };
    info!("Playback engine created");

    // 4d. Resolver — use a persistent cache so resolved URLs survive restarts.
    let cache_path = std::path::Path::new("/var/lib/picast/resolve-cache.db");
    let resolver = Arc::new(picast_resolver::Resolver::with_persistent_cache(
        tor_manager.clone(),
        cache_path,
    ));
    info!(cache = %cache_path.display(), "Resolver created (persistent cache)");

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

    // Load TLS acceptor if cert/key are configured.
    let tls_acceptor = if config.server.tls_enabled() {
        match picast_protocols::load_tls_acceptor(
            &config.server.tls_cert_path,
            &config.server.tls_key_path,
        ) {
            Ok(Some(acceptor)) => {
                info!("TLS enabled — serving HTTPS and WSS");
                Some(acceptor)
            }
            Ok(None) => {
                warn!("TLS cert/key paths set but acceptor returned None — falling back to plain HTTP/WS");
                None
            }
            Err(e) => {
                warn!(error = %e, "Failed to load TLS cert/key — falling back to plain HTTP/WS");
                None
            }
        }
    } else {
        info!("TLS not configured — serving plain HTTP and WS");
        None
    };

    let mut http_server =
        picast_protocols::HttpApiServer::new(&config.server.http_addr, session.clone());
    let mut ws_server = picast_protocols::WebSocketServer::new(&config.server.ws_addr, session.clone());

    if let Some(acceptor) = tls_acceptor {
        http_server = http_server.with_tls(acceptor.clone());
        ws_server = ws_server.with_tls(acceptor);
    }

    let dlna_renderer = Arc::new(picast_protocols::DlnaRenderer::new(
        &config.dlna.friendly_name,
        &config.tor.socks_addr,
    ));

    info!("all components initialised");

    // ── 6b. Notify systemd that we're ready ────────────────────────────
    // Send READY=1 so systemd knows the service has started.
    // If not running under systemd (e.g. dev mode), this is a no-op.
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        warn!(error = %e, "sd_notify READY failed (not running under systemd?)");
    } else {
        info!("sd_notify: READY=1 sent to systemd");
    }

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
            // H17: Signal the process to exit — a critical server has failed.
            std::process::exit(1);
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
            // H17: Signal the process to exit — a critical server has failed.
            std::process::exit(1);
        }
    });

    // Start DLNA renderer (non-blocking, may fail if gmediarender not installed).
    let dlna_start = dlna_renderer.clone();
    let dlna_handle = tokio::spawn(async move {
        if let Err(e) = dlna_start.start().await {
            warn!(error = %e, "DLNA renderer failed to start — DLNA casting will be unavailable");
        }
    });

    // Start DLNA session sync — mirrors PiCast session state to gmediarender.
    let dlna_sync = dlna_renderer.clone();
    let dlna_event_rx = session.subscribe();
    let dlna_sync_handle = tokio::spawn(async move {
        picast_protocols::run_dlna_sync(dlna_sync, dlna_event_rx).await;
    });

    // ── 8. Background tasks ──────────────────────────────────────────

    // 8a. Periodic watchdog notification for systemd (if WatchdogSec is set).
    let watchdog_interval = tokio::time::interval(std::time::Duration::from_secs(10));
    let (watchdog_stop_tx, watchdog_stop_rx) = tokio::sync::oneshot::channel::<()>();
    let watchdog_handle = tokio::spawn(async move {
        let mut interval = watchdog_interval;
        let mut stop_rx = watchdog_stop_rx;
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]) {
                        // Not running under systemd — this is fine.
                        let _ = e; // suppress unused warning
                    }
                }
                _ = &mut stop_rx => break,
            }
        }
    });

    // 8b. Periodic position update during playback.
    // Queries the playback engine every 2 seconds while a session is
    // active and broadcasts PositionUpdate events so that WebSocket
    // clients receive real-time progress.
    let position_session = session.clone();
    let position_playback = playback_engine.clone();
    let (position_stop_tx, position_stop_rx) = tokio::sync::oneshot::channel::<()>();
    let position_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        interval.tick().await; // skip the first immediate tick
        let mut stop_rx = position_stop_rx;
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // Only query position if there's an active session playing.
                    if position_playback.is_playing() {
                        if let Ok(id) = position_session.active_session_id_public().await {
                            position_session.refresh_playback_position_public(id).await;
                        }
                    }
                }
                _ = &mut stop_rx => break,
            }
        }
    });

    shutdown_signal.await;

    // Stop the watchdog before shutdown.
    let _ = watchdog_stop_tx.send(());
    let _ = watchdog_handle.await;

    // Stop the position update task.
    let _ = position_stop_tx.send(());
    let _ = position_handle.await;

    // Signal all tasks to wind down.
    let _ = shutdown_tx.send(());
    info!("shutdown signal broadcast — waiting for tasks to finish …");

    // Wait for servers to stop (with timeout).
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let _ = http_handle.await;
        let _ = ws_handle.await;
        let _ = dlna_handle.await;
        let _ = dlna_sync_handle.await;
    })
    .await;

    // H16: Stop playback engine and release display before shutting down
    // other subsystems. This ensures DRM master is released and GStreamer
    // resources are cleaned up.
    info!("stopping playback engine…");
    if let Err(e) = playback_engine.stop().await {
        warn!(error = %e, "Playback engine shutdown error");
    }

    info!("releasing display…");
    {
        let mut dm = display_manager.lock().await;
        if let Err(e) = dm.release() {
            warn!(error = %e, "Display release error");
        }
    }

    // Stop DLNA renderer subprocess.
    if let Err(e) = dlna_renderer.stop().await {
        warn!(error = %e, "DLNA renderer shutdown error");
    }

    // Shutdown Tor if we own it.
    if let Err(e) = tor_manager.shutdown().await {
        warn!(error = %e, "Tor shutdown error");
    }

    info!("PiCast stopped.");
    Ok(())
}
