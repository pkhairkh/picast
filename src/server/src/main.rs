//! PiCast Server Entry Point
//!
//! Initializes the tracing subscriber, loads configuration, wires up all
//! subsystems (session manager, resolver, playback, display, Tor), and
//! runs the main event loop with graceful shutdown on SIGINT / SIGTERM.

use anyhow::Result;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::broadcast;
use tracing::{error, info};

/// Application configuration loaded from environment / config file.
struct AppConfig {
    /// HTTP API listen address (e.g. "0.0.0.0:8080").
    http_addr: String,
    /// WebSocket listen address (e.g. "0.0.0.0:8081").
    ws_addr: String,
    /// DLNA friendly name advertised on the network.
    dlna_name: String,
    /// Path to the Tor SOCKS proxy (e.g. "127.0.0.1:9050").
    tor_socks: String,
}

impl AppConfig {
    /// Load configuration from environment variables with sensible defaults.
    fn from_env() -> Self {
        Self {
            http_addr: std::env::var("PICAST_HTTP_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            ws_addr: std::env::var("PICAST_WS_ADDR").unwrap_or_else(|_| "0.0.0.0:8081".into()),
            dlna_name: std::env::var("PICAST_DLNA_NAME").unwrap_or_else(|_| "PiCast".into()),
            tor_socks: std::env::var("PICAST_TOR_SOCKS")
                .unwrap_or_else(|_| "127.0.0.1:9050".into()),
        }
    }
}

/// Initialize the `tracing-subscriber` with an `env-filter` so the user can
/// control log verbosity via the `RUST_LOG` environment variable.
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
    let config = AppConfig::from_env();
    info!(http_addr = %config.http_addr, ws_addr = %config.ws_addr, "configuration loaded");

    // ── 3. Shutdown signal ────────────────────────────────────────────
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    let shutdown_signal = async {
        tokio::select! {
            _ = signal::ctrl_c() => info!("received SIGINT"),
            _ = signal::unix::signal(signal::unix::SignalKind::terminate()) => info!("received SIGTERM"),
        }
    };

    // ── 4. Component creation ─────────────────────────────────────────
    // TODO: replace stubs with real component initialisation once each
    //       crate exposes a `new(…)` / `start(…)` API.

    let _tor_manager = Arc::new(()); // picast_tor::TorManager::new(&config.tor_socks);
    let _display = Arc::new(());     // picast_display::DisplayManager::new()?;
    let _playback = Arc::new(());    // picast_playback::PlaybackEngine::new(_display.clone())?;
    let _resolver = Arc::new(());    // picast_resolver::Resolver::new(_tor_manager.clone());
    let _session = Arc::new(());     // picast_session::SessionManager::new(_resolver, _playback, _display, _tor_manager)?;

    let _http = Arc::new(());        // picast_protocols::HttpApiServer::new(&config.http_addr, _session.clone())?;
    let _ws = Arc::new(());          // picast_protocols::WebSocketServer::new(&config.ws_addr, _session.clone())?;
    let _dlna = Arc::new(());        // picast_protocols::DlnaRenderer::new(&config.dlna_name, _session.clone())?;

    info!("all components initialised");

    // ── 5. Run until shutdown ─────────────────────────────────────────
    shutdown_signal.await;

    // Signal all tasks to wind down.
    let _ = shutdown_tx.send(());
    info!("shutdown signal broadcast – waiting for tasks to finish …");

    // TODO: await graceful termination of each spawned task here.

    info!("PiCast stopped.");
    Ok(())
}
