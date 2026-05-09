# boGDan — Agent Context

> **This file is the primary entry point for any AI agent working on boGDan.**
> Read this file first. It defines the project, its conventions, and how to navigate the codebase.
> If you are an autonomous coding agent, follow the workflows in § Agent Workflows precisely.

---

## What Is boGDan?

boGDan is a **Tor-routed, zero-copy media casting appliance** for the Raspberry Pi 4B+.
It turns any HDMI-connected display into a network media receiver where:

- All content resolution (yt-dlp) and media fetching routes through **Tor**
- Video playback uses the Pi's **dedicated H.264 hardware decoder** with **zero-copy DMA-BUF → HVS → HDMI**
- No display server (X11/Wayland), no browser, no DRM — pure **DRM/KMS direct**
- Senders use a **browser extension**, **VLC/DLNA**, or the **HTTP API** to cast URLs

---

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

---

## Crate Map (Rust Workspace)

| Crate | Path | Responsibility | Depends On |
|-------|------|----------------|------------|
| `bogdan-server` | `src/server/` | Main binary, config, ties all crates together | all |
| `bogdan-protocols` | `src/protocols/` | HTTP API, WebSocket, DLNA responder | session |
| `bogdan-session` | `src/session/` | State machine, CDN retry logic, queue | resolver, playback |
| `bogdan-resolver` | `src/resolver/` | URL classification, custom resolvers, yt-dlp | tor, session |
| `bogdan-playback` | `src/playback/` | Progressive download, GStreamer pipeline, SOCKS forwarder | display, v3d |
| `bogdan-display` | `src/display/` | DRM/KMS plane control, atomic modesetting | — |
| `bogdan-v3d` | `src/v3d/` | V3D GPU compute shader (SAND→NV12 for HEVC) | — |
| `bogdan-tor` | `src/tor/` | SOCKS5 proxy pool, stream isolation, circuit health | — |

### Dependency Graph (build order)

```
bogdan-tor ──────┐
bogdan-display ──┤
bogdan-v3d ──────┤
                 ├──► bogdan-resolver ──┐
                 │                      ├──► bogdan-session ──► bogdan-protocols ──► bogdan-server
                 └──► bogdan-playback ──┘
```

Leaf crates (`bogdan-tor`, `bogdan-display`, `bogdan-v3d`) have zero internal dependencies and can be
implemented in parallel. Mid-layer crates (`bogdan-resolver`, `bogdan-playback`) depend
on leaf crates. `bogdan-session` is the integration point. `bogdan-protocols` wraps
session for the network. `bogdan-server` is the binary entry point.

---

## Key Technical Constraints

1. **H.264 primary, HEVC experimental** — The HEVC decoder outputs SAND format (NC12/NC30) which the HVS cannot display natively. The `v3d` crate implements a GPU compute shader for SAND→NV12 conversion (behind `hevc` feature flag). For v1, prefer H.264 via `bestvideo[vcodec^=avc1]` in yt-dlp.
2. **No Cast V2** — Google enforces device authentication; unofficial receivers cannot appear in Chrome's native cast menu.
3. **No DRM** — Widevine L3 on ARM is unreliable. DRM content is explicitly out of scope.
4. **Tor bandwidth is 500Kbps–5Mbps** — Some CDNs also impose speed limits (`sp=380` → 380 kbps cap). Progressive download via appsrc pre-buffers data. Playback may stutter when CDN rate limit is below video bitrate.
5. **Zero-copy is sacred** — Never `.map()` a DMA-BUF into userspace. Pass file descriptors only. If you copy, you've failed.
6. **DRM/KMS direct** — No X11, no Wayland, no compositor. The app is DRM master. Use `drmModeAtomicCommit()` for all plane updates.
7. **Process isolation for yt-dlp** — yt-dlp runs as a subprocess (`tokio::process::Command`), never as an embedded Python library. Kill it with timeout if it hangs. However, custom resolvers (Voe, etc.) use reqwest via Tor directly — no yt-dlp subprocess for known domains.
8. **No `unsafe` without justification** — Any `unsafe` block must have a `// SAFETY:` comment explaining why it is sound.
9. **No `unwrap()` in production code** — Use `?`, `.ok_or(...)`, or explicit error handling. `unwrap()` is acceptable in `#[test]` only.

---

## Documentation Map

| File | Content |
|------|---------|
| `ARCHITECTURE.md` | Full system architecture (hardware, pipeline, protocols, Tor) |
| `SPECIFICATION.md` | API contracts, format matrix, GStreamer pipelines, config specs |
| `DECISIONS.md` | Architecture Decision Records (ADR-001 through ADR-009) |
| `docs/decisions/` | Individual ADR files with full context and rationale |
| `ROADMAP.md` | Version milestones v0.1.0 through v2.0.0 |
| `docs/GLOSSARY.md` | Technical term definitions with boGDan-specific context |
| `docs/hardware/` | BCM2711 deep dives, V4L2 pipeline details, HVS internals |
| `docs/protocols/` | HTTP API, WebSocket, DLNA, discovery specs |
| `docs/playback/` | GStreamer pipeline configs, ABR controller, DRM/KMS |
| `docs/tor/` | Tor integration, stream isolation, bandwidth expectations |
| `docs/extension/` | Browser extension manifest, interception logic |

---

## Agent Workflows

### Workflow 1: Implement a Crate Feature

When assigned a task like "Implement `HttpApiServer::handle_cast()`":

1. **Read the spec** — Open `SPECIFICATION.md` and find the relevant API contract section. Read the corresponding `docs/<module>/<topic>.md` for deep context.
2. **Read existing code** — Open `src/<crate>/src/lib.rs` and `src/<crate>/README.md` to understand current state.
3. **Check interfaces** — If the crate implements a trait from `src/session/src/interfaces.rs`, read the trait definition carefully.
4. **Check decisions** — Look at `DECISIONS.md` and `docs/decisions/` to ensure no conflicting decisions exist.
5. **Implement** — Write the code following the patterns in § Code Patterns below.
6. **Write tests** — Add `#[cfg(test)] mod tests` with at least one test per public method.
7. **Run verification** — Execute `cargo check -p <crate>`, then `cargo test -p <crate>`, then `cargo clippy -p <crate>`.
8. **Update docs** — If you added a new public type or function, update `src/<crate>/README.md`.
9. **Report** — Summarize what was implemented, what tests pass, and any deviations from the spec.

### Workflow 2: Fix a Bug

1. **Reproduce** — Write a failing test first (even if it requires mocking hardware).
2. **Root cause** — Trace the code path from the entry point (HTTP endpoint, DLNA action, etc.) to the failure.
3. **Fix** — Minimal change that resolves the bug. Do not refactor surrounding code.
4. **Verify** — `cargo test -p <crate>` passes. The original failing test now passes.
5. **Regression test** — Ensure the test you wrote in step 1 is committed.

### Workflow 3: Add an ADR

When a design decision is made that isn't in `DECISIONS.md`:

1. Copy `docs/decisions/TEMPLATE.md` to `docs/decisions/NNN-<slug>.md` (next number).
2. Fill in all sections: Context, Decision, Consequences, Alternatives Rejected.
3. Add a summary entry to `DECISIONS.md` following the existing format.
4. Update `docs/decisions/TEMPLATE.md` last-used number comment if needed.

### Workflow 4: Cross-Compilation Check

After any change that affects the display, playback, or tor crates:

1. Run `cargo check --target aarch64-unknown-linux-gnu` to verify it compiles for Pi 4.
2. If it fails due to missing system libraries, document the dependency in the crate's README.
3. If it fails due to platform-specific code, ensure `#[cfg(target_arch = "aarch64")]` gates are correct.

---

## Code Patterns

### Error Handling

```rust
// Crate-level errors: thiserror
#[derive(thiserror::Error, Debug)]
pub enum PlaybackError {
    #[error("pipeline creation failed: {0}")]
    PipelineCreation(String),
    #[error("gstreamer error: {0}")]
    Gstreamer(String),
}

// Application-level: anyhow
fn main() -> anyhow::Result<()> {
    // ...
}
```

### Async Trait Implementation

```rust
use async_trait::async_trait;

#[async_trait]
pub trait ResolverTrait: Send + Sync {
    async fn resolve(&self, url: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>>;
}

#[async_trait]
impl ResolverTrait for Resolver {
    async fn resolve(&self, url: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // ...
    }
}
```

### Subprocess Invocation (yt-dlp)

```rust
use tokio::process::Command;
use std::time::Duration;

async fn resolve_with_ytdlp(url: &str, socks_proxy: &str) -> Result<String, ResolveError> {
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new("yt-dlp")
            .args([
                "--proxy", &format!("socks5h://{socks_proxy}"),
                "--format", "bestvideo[vcodec^=avc1]+bestaudio/best[vcodec^=avc1]/best",
                "--dump-json",
                "--no-playlist",
                url,
            ])
            .output()
    )
    .await
    .map_err(|_| ResolveError::Network("yt-dlp timeout".into()))?
    .map_err(|e| ResolveError::Network(e.to_string()))?;

    if !output.status.success() {
        return Err(ResolveError::NoMediaFound(url.into()));
    }

    // Parse JSON output...
    Ok(direct_url)
}
```

### GStreamer Pipeline Construction

```rust
use gstreamer::prelude::*;

fn build_h264_pipeline(uri: &str) -> Result<gstreamer::Pipeline, PlaybackError> {
    gstreamer::init().map_err(|e| PlaybackError::Gstreamer(e.to_string()))?;

    let pipeline = gstreamer::parse_launch(
        &format!(
            "uridecodebin uri={uri} name=src \
             src. ! queue ! v4l2h264dec ! kmssink \
             src. ! queue ! audioconvert ! alsasink"
        )
    ).map_err(|e| PlaybackError::PipelineCreation(e.to_string()))?;

    Ok(pipeline.downcast::<gstreamer::Pipeline>().unwrap())
}
```

### DRM/KMS Atomic Modesetting

```rust
// Always use atomic commits for tear-free display
// Never map DMA-BUFs into userspace
// Pass file descriptors between V4L2 decoder and DRM planes

fn atomic_commit(
    drm_fd: RawFd,
    plane_id: u32,
    crtc_id: u32,
    fb_id: u32,
    src_rect: (u32, u32, u32, u32),
    dst_rect: (u32, u32, u32, u32),
) -> Result<(), DisplayError> {
    // Use drmModeAtomicCommit() via nix::sys::ioctl
    // This is the ONLY acceptable way to update the display
    todo!("Implement via drm-rs + nix ioctl bindings")
}
```

---

## Module Independence

Each crate in `src/` is designed to be implementable by a **single agent session** without needing deep knowledge of other crates. The interfaces between crates are defined by Rust traits in `src/session/src/interfaces.rs`. If you're working on one crate, you only need to understand the trait it implements or consumes.

### Crate-by-Crate Implementation Order

| Phase | Crate | Why This Order |
|-------|-------|---------------|
| 1 | `bogdan-tor` | Leaf crate, no dependencies, well-scoped |
| 1 | `bogdan-display` | Leaf crate, no dependencies, DRM/KMS focused |
| 2 | `bogdan-resolver` | Depends only on `bogdan-tor`, URL logic is self-contained |
| 2 | `bogdan-playback` | Depends only on `bogdan-display`, GStreamer pipeline logic |
| 3 | `bogdan-session` | Integrates resolver + playback + display + tor |
| 4 | `bogdan-protocols` | HTTP/WS/DLNA layer over session |
| 5 | `bogdan-server` | Binary that wires everything together |

Phases can run in parallel within the same phase number.

---

## Task Granularity

A good agent task is:

- "Implement `HttpApiServer::handle_cast()` in `src/protocols/` per the spec in `docs/protocols/http-api.md`"
- "Add `TorManager::ensure_running()` that checks SOCKS reachability and spawns Tor if needed"
- "Write integration tests for URL classification in `bogdan-resolver`"

A bad task is:

- "Build the whole server"
- "Make playback work"
- "Fix all the TODOs"

---

## Project Conventions

- **Language**: Rust (edition 2021, MSRV 1.88+)
- **Async runtime**: tokio (multi-thread)
- **Error handling**: thiserror for crate errors, anyhow for application-level
- **Logging**: tracing + tracing-subscriber (NOT log/env_logger)
- **Serialization**: serde + serde_json
- **GStreamer**: gstreamer-rs bindings (C FFI via `gstreamer` crate)
- **DRM/KMS**: `drm` crate + custom ioctls via `nix`
- **HTTP**: hyper (NOT actix/rocket/axum — keep it minimal)
- **WebSocket**: tokio-tungstenite
- **DLNA**: Custom SSDP responder + roxmltree for XML parsing
- **TLS**: rustls (NOT openssl — see `deny.toml`)
- **Commit messages**: Conventional commits (`feat:`, `fix:`, `docs:`, `refactor:`)
- **Branch naming**: `feat/<topic>`, `fix/<topic>`, `docs/<topic>`
- **No `unwrap()`** in production code (tests only)
- **No `unsafe`** without `// SAFETY:` comment

---

## Verification Checklist

Before marking any task as complete, verify:

- [ ] `cargo check -p <crate>` — zero errors, zero warnings
- [ ] `cargo test -p <crate>` — all tests pass
- [ ] `cargo clippy -p <crate>` — zero warnings
- [ ] `cargo fmt -p <crate> --check` — formatted correctly
- [ ] Documentation updated if public API changed
- [ ] No conflicting decisions in `DECISIONS.md`
- [ ] Cross-compilation: `cargo check -p <crate> --target aarch64-unknown-linux-gnu`

---

## File Encoding

All markdown docs use **UTF-8**. All Rust source files use **Unix line endings** (LF). No BOM.

---

## Threat Model Summary

| Threat | Mitigation |
|--------|-----------|
| ISP sees browsing history | All resolution via Tor SOCKS5 |
| LAN attacker controls Pi | iptables: only LAN sources on control ports |
| DNS leak | socks5h:// (remote DNS), dnsmasq refuses local queries |
| yt-dlp RCE | Subprocess isolation, 30s timeout, dedicated user |
| Tor circuit correlation | IsolateSOCKSAuth per-site, shared circuit per-site |
| Supply chain attack | cargo-deny audits, no openssl/curl dependencies |
