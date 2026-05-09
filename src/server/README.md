# bogdan-server

The main binary entry point for the boGDan appliance. It orchestrates subsystem initialization, configuration loading, signal handling, and graceful shutdown. This crate does not contain business logic — it wires together the other six crates and drives the application lifecycle.

## Purpose

boGDan server is the executable harness that bootstraps all boGDan subsystems (Tor, display, playback, resolver, session, protocols) in the correct dependency order, spawns their async tasks, and waits for a shutdown signal. It is the single process that runs on the Raspberry Pi as a systemd service (`bogdan.service`), started directly on `tty1` by autologin — no display server, no window manager, no desktop environment. The server process holds DRM master privileges for the duration of its lifetime, and if it crashes, systemd restarts it within seconds.

## Public API

This crate produces a single binary, not a library. There are no public structs or traits. The entry point is `main()`.

| Item | Kind | Description |
|------|------|-------------|
| `AppConfig` | struct | Configuration loaded from environment variables with sensible defaults |
| `AppConfig::from_env()` | method | Reads `BOGDAN_HTTP_ADDR`, `BOGDAN_WS_ADDR`, `BOGDAN_DLNA_NAME`, `BOGDAN_TOR_SOCKS` |
| `init_tracing()` | function | Sets up `tracing-subscriber` with `env-filter` for `RUST_LOG` control |

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `BOGDAN_HTTP_ADDR` | `0.0.0.0:8080` | HTTP REST API listen address |
| `BOGDAN_WS_ADDR` | `0.0.0.0:8081` | WebSocket listen address |
| `BOGDAN_DLNA_NAME` | `boGDan` | Friendly name for DLNA/SSDP announcements |
| `BOGDAN_TOR_SOCKS` | `127.0.0.1:9050` | Tor SOCKS5 proxy address |
| `BOGDAN_DB_PATH` | `/var/lib/bogdan/sessions.db` | SQLite database path |
| `RUST_LOG` | `info` | Tracing filter (e.g. `bogdan_resolver=debug`) |

## Dependencies

The server crate depends on **every other boGDan crate** because it is responsible for constructing and wiring them together:

| Dependency | Why |
|------------|-----|
| `bogdan-tor` | Starts the Tor daemon first; all subsequent network operations depend on it |
| `bogdan-display` | Opens `/dev/dri/card0`, acquires DRM master, configures HVS planes |
| `bogdan-playback` | Initializes GStreamer; requires display handle for kmssink |
| `bogdan-resolver` | URL classification and yt-dlp subprocess; requires Tor SOCKS address |
| `bogdan-session` | Central coordinator; receives Arc references to all four subsystems |
| `bogdan-protocols` | HTTP, WebSocket, and DLNA servers; receives Arc<SessionManager> |
| `tokio` | Async runtime (multi-thread) |
| `tracing` + `tracing-subscriber` | Structured logging |
| `anyhow` | Top-level error handling |

## Initialization Sequence

The subsystems MUST be initialized in this exact order due to hard dependencies:

```
1. init_tracing()                  — Configure logging first so all subsequent
                                      steps produce visible output.

2. AppConfig::from_env()           — Load configuration from environment.

3. TorManager::start()             — Start the C Tor daemon as a child process.
   └─ TorManager::wait_ready()     — Poll SOCKS5 port until it accepts connections
                                      (up to 60 seconds on first boot for consensus
                                      download).

4. DisplayManager::open()          — Open /dev/dri/card0, call drmSetMaster(),
   └─ DisplayManager::set_mode()   — Set preferred HDMI mode (1080p60).
                                      This MUST happen before GStreamer init because
                                      kmssink probes the DRM device.

5. PlaybackEngine::new(display)    — Initialize GStreamer (gst_init), pass the
                                      display's DRM FD so kmssink can use the same
                                      CRTC/plane configuration.

6. Resolver::new(tor)              — Create the resolver with Tor proxy address and
                                      yt-dlp binary path.

7. SessionManager::new(            — Wire all four subsystems into the session
      resolver, playback,            manager via their trait objects.
      display, tor)

8. HttpApiServer::new(session)     — Start HTTP REST API on port 8080.
9. WebSocketServer::new(session)   — Start WebSocket server on port 8081.
10. DlnaRenderer::new(session)     — Start DLNA/SSDP on port 8200 + 1900.

11. Wait for SIGINT / SIGTERM.
12. shutdown_tx.send(())           — Broadcast shutdown to all tasks.
13. TorManager::shutdown()         — SIGTERM to Tor daemon, reap child.
14. DisplayManager::release()      — Drop DRM master, close /dev/dri/card0.
```

## Implementation Guide for AI Agents

1. **Start with the skeleton** — the current `main.rs` has stubs for all components. Replace each `Arc::new(())` with the real constructor call as each crate becomes available.

2. **Initialization order is critical** — do not attempt to initialize GStreamer before the display manager has set the DRM mode. GStreamer's `kmssink` element probes the DRM device during pipeline construction and will fail if no mode is set.

3. **Graceful shutdown** — the broadcast channel pattern is already in place. Each long-running task should select on both its work channel and the shutdown receiver. On shutdown signal: stop the playback pipeline first (set GStreamer to NULL), then drop DRM master, then kill the Tor daemon.

4. **Health check endpoint** — consider adding `GET /api/v1/health` that returns 200 if all subsystems are alive (Tor reachable, DRM master held, GStreamer initialized).

5. **Testing strategy** — the main binary is difficult to unit test directly. Instead, test each subsystem's initialization independently. Integration tests should use a test harness that starts the server on a Pi with a loopback HDMI adapter.

6. **Configuration validation** — add validation to `AppConfig::from_env()` that checks: HTTP port is not 0, Tor SOCKS address resolves, DRM device path exists, yt-dlp binary is found on PATH.

## Key Constraints

- **DRM master is exclusive** — the server process must be the only DRM master. If X11 or Wayland is running, `drmSetMaster()` will fail with EPERM. The systemd unit file must not start a desktop session.

- **Single process model** — boGDan runs as one process (plus the Tor child). Do not split into multiple daemons or microservices. The zero-copy DMA-BUF pipeline requires the GStreamer process and the DRM master to be the same process.

- **No forking** — the tokio runtime must not fork. The Tor daemon is spawned as a child process via `tokio::process::Command`, which is safe because the child is a completely separate executable (not a forked copy of boGDan).

- **Signal handling** — both SIGINT and SIGTERM must trigger graceful shutdown. The server runs under systemd, which sends SIGTERM on `systemctl stop`. A SIGINT handler is needed for interactive testing.

- **GStreamer threading** — GStreamer creates its own threads internally. All GStreamer calls from the tokio runtime must be serialized (e.g., via a `Mutex`). The server must not call GStreamer methods from multiple tokio tasks concurrently.

- **Process user** — boGDan should run as a dedicated `bogdan` user, not root. The `bogdan` user needs membership in the `video` group (for `/dev/dri/card0` access) and `audio` group (for ALSA). The systemd unit uses `User=bogdan` and `Group=bogdan`.

- **Boot time** — the server should reach "ready" state (all subsystems initialized, all servers listening) within 10 seconds on a Pi 4 with warm Tor cache. Cold boot with Tor consensus download may take 30–60 seconds.

## Reference

| Resource | Location |
|----------|----------|
| Main entry point | `src/server/src/main.rs` |
| Cargo.toml | `src/server/Cargo.toml` |
| Systemd unit file | `config/bogdan.service` |
| Architecture overview | `ARCHITECTURE.md` §2 (System Overview) |
| ADR-001: No Display Server | `DECISIONS.md` / `SPECIFICATION.md` §1.1 |
| Startup sequence | `SPECIFICATION.md` §4 (Operational Configuration) |
| AGENT.md (project conventions) | `AGENT.md` |
