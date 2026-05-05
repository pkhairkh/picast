# PiCast — Agent Context

> **This file is the primary entry point for any AI agent working on PiCast.**
> Read this file first. It defines the project, its conventions, and how to navigate the codebase.

## What Is PiCast?

PiCast is a **Tor-routed, zero-copy media casting appliance** for the Raspberry Pi 4B+.
It turns any HDMI-connected display into a network media receiver where:

- All content resolution (yt-dlp) and media fetching routes through **Tor**
- Video playback uses the Pi's **dedicated H.264 hardware decoder** with **zero-copy DMA-BUF → HVS → HDMI**
- No display server (X11/Wayland), no browser, no DRM — pure **DRM/KMS direct**
- Senders use a **browser extension**, **VLC/DLNA**, or the **HTTP API** to cast URLs

## Architecture At A Glance

```
Sender Device                    Pi 4 (Receiver)
┌──────────────┐  URL via HTTP   ┌─────────────────────────────┐
│ Browser Ext  │────────────────→│ protocols (HTTP+WS+DLNA)    │
│ VLC / DLNA   │  UPnP/DLNA      │         │                   │
│ HA / curl    │                 │    session (state machine)   │
└──────────────┘                 │         │                   │
                                 │  resolver (yt-dlp via Tor)  │
                                 │         │                   │
                                 │  playback (GStreamer+V4L2)  │
                                 │         │                   │
                                 │  display (DRM/KMS → HDMI)   │
                                 │         │                   │
                                 │  tor (C daemon, SOCKS5)     │
                                 └─────────────────────────────┘
```

## Crate Map (Rust Workspace)

| Crate | Path | Responsibility | Depends On |
|-------|------|----------------|------------|
| `picast-server` | `src/server/` | Main binary, ties all crates together | all |
| `picast-protocols` | `src/protocols/` | HTTP API, WebSocket, DLNA responder | session |
| `picast-session` | `src/session/` | State machine, queue, ABR controller | resolver, playback |
| `picast-resolver` | `src/resolver/` | URL classification, yt-dlp subprocess, format selection | tor |
| `picast-playback` | `src/playback/` | GStreamer pipeline management, buffer monitoring | display |
| `picast-display` | `src/display/` | DRM/KMS plane control, atomic modesetting | — |
| `picast-tor` | `src/tor/` | SOCKS5 proxy pool, stream isolation, circuit health | — |

## Key Technical Constraints

1. **H.264 only for v1** — The HEVC decoder outputs SAND format (NC12/NC30) which the HVS cannot display. Force `bestvideo[vcodec^=avc1]` in yt-dlp.
2. **No Cast V2** — Google enforces device authentication; unofficial receivers cannot appear in Chrome's native cast menu.
3. **No DRM** — Widevine L3 on ARM is unreliable. DRM content is explicitly out of scope.
4. **Tor bandwidth is 500Kbps–5Mbps** — Default to 720p max; use 50MB buffer (queue2); ABR monitors GStreamer buffer fill level.
5. **Zero-copy is sacred** — Never `.map()` a DMA-BUF into userspace. Pass file descriptors only. If you copy, you've failed.
6. **DRM/KMS direct** — No X11, no Wayland, no compositor. The app is DRM master. Use `drmModeAtomicCommit()` for all plane updates.
7. **Process isolation for yt-dlp** — yt-dlp runs as a subprocess (`tokio::process::Command`), never as an embedded Python library. Kill it with timeout if it hangs.

## Documentation Map

| File | Content |
|------|---------|
| `ARCHITECTURE.md` | Full system architecture (hardware, pipeline, protocols, Tor) |
| `SPECIFICATION.md` | API contracts, format matrix, GStreamer pipelines, config specs |
| `DECISIONS.md` | Architecture Decision Records (ADR-001 through ADR-009) |
| `docs/hardware/` | BCM2711 deep dives, V4L2 pipeline details, HVS internals |
| `docs/protocols/` | HTTP API, WebSocket, DLNA, discovery specs |
| `docs/playback/` | GStreamer pipeline configs, ABR controller, DRM/KMS |
| `docs/tor/` | Tor integration, stream isolation, bandwidth expectations |
| `docs/extension/` | Browser extension manifest, interception logic |
| `docs/decisions/` | Individual ADR files with full context |

## How To Work On This Project

### For AI Agents

1. **Read this file first**, then the relevant module's `README.md` in `src/<crate>/`
2. **Check `SPECIFICATION.md`** for exact API contracts before implementing endpoints
3. **Check `ARCHITECTURE.md`** for pipeline constraints before modifying playback
4. **Check `DECISIONS.md`** before proposing architectural changes — the decision may already be made
5. **Run `cargo check`** after any code change — zero warnings policy
6. **Run `cargo test`** before marking a task complete
7. **Update the module's README.md** if you add new public APIs

### Module Independence

Each crate in `src/` is designed to be implementable by a **single agent session** without needing deep knowledge of other crates. The interfaces between crates are defined by Rust traits in `picast-session/src/interfaces.rs`. If you're working on one crate, you only need to understand the trait it implements or consumes.

### Task Granularity

A good agent task is: "Implement `HttpApiServer::handle_cast()` in `src/protocols/` per the spec in `docs/protocols/http-api.md`". A bad task is: "Build the whole server."

## Project Conventions

- **Language**: Rust (edition 2021, MSRV 1.70+)
- **Async runtime**: tokio (multi-thread)
- **Error handling**: thiserror for crate errors, anyhow for application-level
- **Logging**: tracing + tracing-subscriber (NOT log/env_logger)
- **Serialization**: serde + serde_json
- **GStreamer**: gstreamer-rs bindings (C FFI via `gstreamer` crate)
- **DRM/KMS**: `drm` crate + custom ioctls via `nix`
- **HTTP**: hyper (NOT actix/rocket/axum — keep it minimal)
- **WebSocket**: tokio-tungstenite
- **DLNA**: Custom SSDP responder + roxmltree for XML parsing
- **Commit messages**: Conventional commits (`feat:`, `fix:`, `docs:`, `refactor:`)
- **Branch naming**: `feat/<topic>`, `fix/<topic>`, `docs/<topic>`

## File Encoding

All markdown docs use **UTF-8**. All Rust source files use **Unix line endings** (LF). No BOM.

## Threat Model Summary

| Threat | Mitigation |
|--------|-----------|
| ISP sees browsing history | All resolution via Tor SOCKS5 |
| LAN attacker controls Pi | iptables: only LAN sources on control ports |
| DNS leak | socks5h:// (remote DNS), dnsmasq refuses local queries |
| yt-dlp RCE | Subprocess isolation, 30s timeout, dedicated user |
| Tor circuit correlation | IsolateSOCKSAuth per-site, shared circuit per-site |
