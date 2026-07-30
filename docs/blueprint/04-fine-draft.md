---
doc: fine_draft
project: picast
version: 1
phase: fine_draft
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# boGDan Blueprint — Fine Draft

This fine draft expands the rough draft (`docs/blueprint/02-rough-draft.md`) and the blueprint ADRs (`docs/blueprint/03-adrs/`) into four concrete sections that the implementation phase can build against: **Components** (what Rust crates exist and what each owns), **Data Model** (the types, state machine, and config schema), **Security** (threat model and per-layer mitigations), and **Test Strategy** (unit, integration, conformance, and security testing). Every component is named so a contributor can `grep` the codebase and find it; every data type is named so the API contract is unambiguous; every security claim is paired with the control that enforces it; every test category is paired with the success metric from `docs/blueprint/01-problem-catalog.md` that it validates.

Cross-references use the established conventions: [[P-NNN]] for problems, [[BP-ADR-NNN]] for blueprint ADRs, [[ADR-NNN]] for ratified project ADRs, [[C-NNN]] for components, [[T-NNN]] for tasks, [[R-NNN]] for requirements.

## Scope and Non-Goals

This document covers the v1 appliance as scoped by the rough draft. Specifically in scope: the Rust workspace layout, the GStreamer pipeline, the boGCast protocol surface, the Tor integration, the resolver layer, the browser extension, the installer, the thermal supervisor, and the accessibility surface. Specifically out of scope: multi-room sync (deferred, [[BP-ADR-011]]), HEVC hardware decode (deferred, [[ADR-009]]), DRM content playback (rejected, [[ADR-007]]), and Google Cast V2 protocol (rejected, [[ADR-005]]).

## Components

The boGDan Rust workspace is organised as a set of narrow crates, each owning one concern. Crate boundaries are drawn so that any single crate can be tested in isolation on an x86_64 developer machine without Pi-specific hardware, except for `bogdan-display` and the V4L2 path inside `bogdan-playback` which require vkms or real Pi hardware.

### [[C-001]] `bogdan-server`

The main binary crate. Owns startup orchestration: parses `bogdan.toml`, initialises the tokio runtime, spawns the Tor daemon supervisor, starts the three protocol facades ([[C-002]], [[C-003]], [[C-004]]), and owns the single `Session` ([[C-010]]) instance that all facades translate into. Exposes the `/api/status` HTTP endpoint and the WebSocket event bus. Implements [[BP-ADR-004]] (boGCast facades) and [[BP-ADR-008]] (first-boot web UI at `bogdan.local`).

### [[C-002]] `bogdan-protocols` (HTTP REST facade)

HTTP REST API on `:8585`. Endpoints: `POST /api/cast`, `POST /api/stop`, `GET /api/status`, `POST /api/seek`, `POST /api/pause`, `POST /api/resume`. Each endpoint translates its JSON request body into a `CastCommand` ([[C-010]]) and dispatches to the `Session`. CORS is permissive (`Access-Control-Allow-Origin: *`) so the browser extension ([[C-008]]) can call from any origin. TLS is opt-in via `tls_cert_path` / `tls_key_path` in `bogdan.toml`. Conforms to [[BP-ADR-004]].

### [[C-003]] `bogdan-protocols` (WebSocket facade)

WebSocket server on `:8586`. Pushes session events (`state_changed`, `buffer_update`, `circuit_rotated`, `thermal_throttled`, `error`) to subscribed clients. Stateless reconnect: clients send `Last-Event-ID` on reconnect and the server replays missed events from a bounded ring buffer (1024 events). Used by the browser extension ([[C-008]]) to update cast UI in real time. Conforms to [[BP-ADR-004]] and [[BP-ADR-007]].

### [[C-004]] `bogdan-protocols` (DLNA facade)

Manages `gmediarender` as a subprocess. Advertises boGDan as a UPnP MediaRenderer via SSDP, accepts `SetAVTransportURI` calls, and translates each into a `CastCommand`. Owns the DRM-master contention protocol with [[C-006]]: tears down `gmediarender` before pipeline construction with a 500 ms grace window and a 2 s retry budget (see [[BP-ADR-009]]). The subprocess is pinned to a specific upstream commit and built reproducibly in the Debian packaging step.

### [[C-005]] `bogdan-tor`

Owns the Tor daemon lifecycle: starts the C Tor daemon (`tor` package, see [[ADR-004]]) with `torrc` from `config/`, monitors it via the control port (`9052`), restarts on crash. Provides the `TorProxy` handle used by [[C-007]] and [[C-011]] to obtain per-host SOCKS5 usernames for `IsolateSOCKSAuth` circuit isolation. Implements [[BP-ADR-001]] and [[BP-ADR-005]].

### [[C-006]] `bogdan-display`

Opens `/dev/dri/card0`, acquires DRM master, and programs the CRTC via atomic modesetting to display a single fullscreen plane. No X11, no Wayland ([[ADR-001]]). Exposes a `DisplayPlane` handle that [[C-011]] uses to import DMA-BUFs from `kmssink`. Owns the DRM-master contention protocol with [[C-004]]: serialises acquisition with a mutex and a grace window. Implements [[BP-ADR-002]].

### [[C-007]] `bogdan-resolver`

Owns URL resolution. Layered: in-tree custom resolvers for high-volume sites (YouTube, Vimeo, direct media links — see [[BP-ADR-006]]) tried first; yt-dlp subprocess ([[ADR-008]]) as long-tail fallback. All resolution uses `socks5h://127.0.0.1:29050` with per-host SOCKS5 username from [[C-005]]. Returns `ResolvedMedia {{ url, format, duration, title, content_type }}` or a structured `ResolveError`. Enforces the 30 s subprocess timeout on yt-dlp.

### [[C-008]] `bogdan-extension` (browser extension)

Manifest V3 codebase in `src/extension/`, built for both Chrome and Firefox via a build-time polyfill ([[BP-ADR-007]]). Detects media URLs on the active tab via `chrome.tabs` / `browser.tabs` and DOM scraping for `<video>` and `<source>`. POSTs to `http://<pi-ip>:8585/api/cast`. Subscribes to the WebSocket on `:8586` for real-time status. Stateless: reconnects on service-worker eviction and re-syncs from `/api/status`.

### [[C-009]] `bogdan-installer`

The `scripts/setup.sh` installer and the first-boot web UI. The installer writes the systemd unit, `torrc`, `iptables` rules, and the boGDan binary; the web UI at `http://bogdan.local` (mDNS via Avahi) handles Tor bridge selection, network config, and media source preferences. Configuration persists to `/etc/bogdan/bogdan.toml` with environment-variable overrides. Implements [[BP-ADR-008]] and [[BP-ADR-012]] (accessibility of the web UI).

### [[C-010]] `bogdan-session`

The session state machine. Owns cast lifecycle, queue, and playback state. Accepts `CastCommand` from any facade ([[C-002]], [[C-003]], [[C-004]]) and translates into pipeline actions on [[C-011]]. Owns the 10 s rolling buffer for circuit-rotation masking ([[BP-ADR-005]]) and the circuit-rotation counter exposed via `/api/status`. State machine described in **Data Model** below.

### [[C-011]] `bogdan-playback`

Owns the GStreamer pipeline. Builds `appsrc → queue2 → parsebin → (pad-added) → queue → v4l2h264dec → v4l2convert → kmssink` for video and `→ avdec_aac → audioconvert → alsasink` for audio ([[BP-ADR-003]]). Owns the SOCKS5 forwarder that pins the resolver's exit IP to the media-fetch client's exit IP. Monitors stream health on the GStreamer bus; on 5xx or timeout, triggers re-resolution via [[C-007]] through [[C-010]]. Software-decode fallback (`avdec_h264`) for codecs the hardware cannot handle.

### [[C-012]] `bogdan-thermal`

The thermal supervisor ([[BP-ADR-010]]). Polls `/sys/class/thermal/thermal_zone0/temp` every 5 s. Above 75 °C emits a warning to `/api/status`; above 80 °C requests a lower-bitrate variant from [[C-007]]; above 85 °C pauses the pipeline and surfaces a 'cooling down' state until temperature drops below 75 °C. Exposes `thermal_throttled: bool` and `cpu_temp_celsius: f32` via `/api/status`.

### [[C-013]] `bogdan-config`

TOML config parser and validator. Reads `/etc/bogdan/bogdan.toml`, overlays environment variables (`BOGDAN_HTTP_ADDR`, `BOGDAN_TOR_SOCKS`, etc.), and produces a validated `Config` struct. Exits with a clear error message on malformed config — never silently falls back to defaults for security-sensitive fields (Tor SOCKS address, listen addresses).

### Cross-Cutting Concerns

| Concern | Owner | Notes |
|---|---|---|
| Logging | All crates via `tracing` | `BOGDAN_LOG_LEVEL` env var; `SafeLogging 1` in `torrc` ensures Tor itself does not log sensitive data |
| Error handling | All crates via `anyhow` + structured `thiserror` enums at crate boundaries | No `unwrap()` outside tests; CI lint with `clippy::unwrap_used` |
| Concurrency | `tokio` for async I/O; `std::sync` for pipeline state | Single `Session` instance behind an `Arc<Mutex<>>`; facades are stateless translators |
| Observability | `/api/status` HTTP endpoint + WebSocket events | Surfaces `state`, `buffer_percent`, `circuit_rotations`, `thermal_throttled`, `cpu_temp_celsius` |

## Data Model

### Configuration Schema (`bogdan.toml`)

```toml
# /etc/bogdan/bogdan.toml

[http]
addr = "0.0.0.0:8585"           # BOGDAN_HTTP_ADDR
tls_cert_path = ""               # empty = plain HTTP
tls_key_path = ""

[ws]
addr = "0.0.0.0:8586"            # BOGDAN_WS_ADDR

[tor]
socks = "127.0.0.1:29050"        # BOGDAN_TOR_SOCKS
control_port = 9052              # BOGDAN_TOR_CONTROL_PORT
bridges = []                     # obfs4 bridge lines, populated by web UI

[audio]
device = ""                      # BOGDAN_AUDIO_DEVICE; empty = default ALSA

[dlna]
name = "boGDan"                  # BOGDAN_DLNA_NAME

[log]
level = "info"                   # BOGDAN_LOG_LEVEL: trace|debug|info|warn|error

[thermal]
warn_celsius = 75
throttle_celsius = 80
pause_celsius = 85
poll_interval_secs = 5
```

Environment variables override the config file. Unknown fields cause a hard error (no silent acceptance). The config is re-read on `SIGHUP` for non-destructive changes (log level, DLNA name); security-sensitive fields (Tor SOCKS, listen addresses) require a service restart.

### Session State Machine

The `Session` ([[C-010]]) is a finite-state machine. States:

```
                ┌─────────┐
                │  Idle   │ ←───── stop / end-of-stream
                └────┬────┘
                     │ cast(url)
                     ▼
                ┌─────────┐
                │Resolving│ ──── resolve fails ───► Error → Idle
                └────┬────┘
                     │ resolved
                     ▼
                ┌─────────┐
                │Preflight│ ──── preflight fails ──► Error → Idle
                │ (CDN)   │
                └────┬────┘
                     │ ok
                     ▼
   ┌─────────────────────────────┐
   │Buffering (≤10s rolling buf) │
   └────────────┬────────────────┘
                │ buffer_percent ≥ 25
                ▼
            ┌────────┐  pause   ┌────────┐
            │Playing │ ───────► │ Paused │
            └────┬───┘  resume  └────────┘
                 │ stop
                 ▼
            ┌────────┐
            │ Idle   │
            └────────┘
```

State transitions emit events on the WebSocket bus ([[C-003]]). All transitions are observable via `/api/status`'s `state` field. The state machine is single-threaded behind an `Arc<Mutex<Session>>`; concurrent `CastCommand`s are serialised, not racing.

### Core Types

```rust
// bogdan-session/src/types.rs

pub enum CastCommand {
    Cast { url: String, title: Option<String>, resume_position: Option<u64> },
    Stop,
    Pause,
    Resume,
    Seek { position_secs: u64 },
}

pub enum SessionState {
    Idle,
    Resolving,
    Preflight,
    Buffering,
    Playing,
    Paused,
    Error { code: ErrorCode, message: String },
}

pub enum ErrorCode {
    ResolveFailed,
    PreflightFailed,
    CircuitExhausted,
    DrmMasterBusy,
    ThermalPause,
    InternalError,
}

pub struct SessionStatus {
    pub state: SessionState,
    pub buffer_percent: u8,
    pub circuit_rotations: u32,
    pub thermal_throttled: bool,
    pub cpu_temp_celsius: f32,
    pub current_url: Option<String>,
    pub position_secs: Option<u64>,
    pub duration_secs: Option<u64>,
}
```

```rust
// bogdan-resolver/src/types.rs

pub struct ResolvedMedia {
    pub url: String,              // direct media URL
    pub format: String,           // e.g. "1080p H.264 + AAC"
    pub duration: Option<u64>,    // seconds
    pub title: Option<String>,
    pub content_type: String,     // MIME, e.g. "video/mp4"
    pub source: ResolveSource,    // InTree | YtDlp
}

pub enum ResolveError {
    UnsupportedSite,
    NetworkTimeout,
    YtDlpFailed { stderr: String },
    InvalidUrl,
}
```

```rust
// bogdan-tor/src/types.rs

pub struct TorProxy {
    socks_addr: SocketAddr,
    control_addr: SocketAddr,
}

impl TorProxy {
    /// Returns a SOCKS5 username that isolates this host onto its own
    /// Tor circuit via IsolateSOCKSAuth (see BP-ADR-001).
    pub fn username_for_host(&self, host: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(host.as_bytes());
        let hash = hasher.finalize();
        hex::encode(&hash[..8])  // first 16 hex chars
    }
}
```

### HTTP API Contract

All endpoints are JSON in / JSON out. CORS is `Access-Control-Allow-Origin: *`. The browser extension ([[C-008]]) is the primary consumer; curl and Home Assistant are secondary consumers.

| Method | Path | Body | Response | Maps to |
|---|---|---|---|---|
| POST | `/api/cast` | `{{"url","title"?,resumePosition?,torMode?}}` | `{{"session_id","state"}}` | `CastCommand::Cast` |
| POST | `/api/stop` | `{}` | `{{"state":"idle"}}` | `CastCommand::Stop` |
| POST | `/api/pause` | `{}` | `{{"state":"paused"}}` | `CastCommand::Pause` |
| POST | `/api/resume` | `{}` | `{{"state":"playing"}}` | `CastCommand::Resume` |
| POST | `/api/seek` | `{{"position_secs":N}}` | `{{"position_secs":N}}` | `CastCommand::Seek` |
| GET | `/api/status` | — | `SessionStatus` | — |

### WebSocket Event Bus

Single endpoint `ws://<pi-ip>:8586/events`. Server pushes JSON events:

```json
{"event":"state_changed","from":"resolving","to":"buffering","ts":1234567890}
{"event":"buffer_update","percent":42,"ts":1234567891}
{"event":"circuit_rotated","count":3,"ts":1234567892}
{"event":"thermal_throttled","temp_celsius":81.2,"ts":1234567893}
{"event":"error","code":"drm_master_busy","message":"...","ts":1234567894}
```

Reconnect protocol: client sends `{{"reconnect":"last_event_id"}}` on open; server replays missed events from a 1024-entry ring buffer.

## Security

### Threat Model

| Threat | Adversary | Mitigation |
|---|---|---|
| ISP observes media URLs | ISP | All traffic through Tor ([[BP-ADR-001]]); `iptables` blocks non-Tor outbound; `verify-network-isolation.sh` asserts zero leaks |
| DNS leak to local resolver | Local network operator | `socks5h://` forces remote DNS resolution through Tor ([[BP-ADR-001]]) |
| Cross-site traffic correlation | Tor exit node | Per-host SOCKS5 username via `IsolateSOCKSAuth` ([[BP-ADR-001]], [[BP-ADR-005]]) |
| CDN IP-bound signed URL 403 on circuit rotation | CDN | Per-host SOCKS5 username pins resolver and fetcher to same exit; re-resolution on 403 ([[BP-ADR-005]]) |
| Local attacker on LAN abuses HTTP API | LAN neighbour | TLS opt-in; bind to specific interface via `BOGDAN_HTTP_ADDR`; document LAN trust model in user guide |
| `curl | bash` supply-chain tampering | GitHub compromise | Pin installer to commit SHA; ship detached GPG signature; document Debian-package install as verified alternative ([[BP-ADR-008]]) |
| gmediarender C vulnerability | Remote DLNA client | Pin to specific upstream commit; reproducible Debian build; `cargo-deny` on Rust crate graph independently; run subprocess as dedicated `bogdan-dlna` user with `seccomp` filter ([[BP-ADR-009]]) |
| DRM master DoS by local process | Local attacker | `bogdan-display` holds DRM master for the appliance's lifetime; `gmediarender` is torn down before pipeline construction ([[BP-ADR-002]], [[BP-ADR-009]]) |
| Tor circuit congestion stalls playback | Network | 10 s rolling buffer; re-resolution within 10 s budget; `/api/status` exposes `buffer_percent` ([[BP-ADR-005]]) |
| Thermal runaway damages hardware | Hardware failure | Thermal supervisor polls every 5 s; pauses pipeline above 85 °C ([[BP-ADR-010]]) |
| yt-dlp extractor executes malicious code | Compromised upstream | Subprocess isolation (not embedded); 30 s timeout; `seccomp` filter on subprocess ([[ADR-008]], [[BP-ADR-006]]) |
| Web UI accessible to non-configured users | LAN attacker | Web UI requires first-boot setup token; mDNS hostname `bogdan.local` only resolves after setup is complete |

### Security Controls Inventory

| Control | Owner | Verified By |
|---|---|---|
| `iptables` rules drop non-Tor outbound | `config/iptables.rules` | `scripts/verify-network-isolation.sh` runs `tcpdump` during a cast; CI runs the script in a network namespace |
| `torrc` hardening (`AvoidDiskWrites`, `SafeLogging`, `CookieAuthentication`) | `config/torrc` | Diff against expected baseline in CI |
| TLS opt-in for HTTP / WebSocket | `bogdan-config` | Unit test: TLS fields present in `Config` |
| DRM master held by `bogdan-display` only | `bogdan-display` | Integration test: `fuser /dev/dri/card0` shows only `bogdan-display` |
| `seccomp` filter on `gmediarender` and `yt-dlp` subprocesses | `bogdan-session`, `bogdan-resolver` | Integration test: subprocess cannot `open()` outside its whitelist |
| First-boot setup token for web UI | `bogdan-installer` | Integration test: web UI returns 401 without token, 200 with |
| No PII logged | All crates via `tracing` | CI grep: no `info!()` call logs URL, title, or SOCKS username |
| Reproducible Debian build | `packaging/build-deb.sh` | CI: build twice, compare `sha256sum` of `.deb` |

### Out of Scope

- Anonymous **sender** device: the browser extension sends the user's URL over the LAN to the Pi; the LAN can observe the URL in transit. TLS mitigates passive sniffing but not a determined LAN attacker. Documented as a known limitation.
- Tor bridge obfuscation against **active** censors: pluggable transports (obfs4) are supported via `torrc` bridges, but the project does not maintain its own bridge distribution.
- Hardware attacks: physical access to the Pi's SD card reveals the Tor state and config. Documented; user is told to physically secure the appliance.

## Test Strategy

Test categories are mapped to success metrics from `docs/blueprint/01-problem-catalog.md` so every metric has at least one test that validates it.

### Unit Tests (per crate)

Unit tests live in `src/` alongside the code (`#[cfg(test)] mod tests`). They run on every CI build, on x86_64, in seconds.

| Crate | Key unit tests |
|---|---|
| `bogdan-config` | TOML parse + env override; unknown-field rejection; security-field-restart-required invariant |
| `bogdan-tor` | `username_for_host` determinism (same host → same username); different hosts → different usernames |
| `bogdan-resolver` | In-tree resolver happy path per supported site; yt-dlp JSON parsing; 30 s timeout enforcement (with mock subprocess) |
| `bogdan-session` | State machine transitions (all edges); `CastCommand` serialization; `SessionStatus` shape |
| `bogdan-playback` | Pipeline construction (with vkms mock); pad-added callback dispatch; software-decode fallback selection |
| `bogdan-thermal` | Threshold transitions (warn/throttle/pause); hysteresis (resume below 75 °C) |

### Integration Tests

Integration tests live in `tests/` at the workspace root. They spin up a real boGDan process (with mocked Tor and mocked CDN) and exercise the HTTP/WebSocket APIs end-to-end.

| Test | Validates | Success metric |
|---|---|---|
| `tests/integration_cast_http.rs` | Full cast lifecycle via HTTP | [[P-004]] — HTTP REST conformance |
| `tests/integration_cast_ws.rs` | WebSocket event stream during a cast | [[P-004]] — WebSocket conformance |
| `tests/integration_cast_dlna.rs` | DLNA `SetAVTransportURI` → playback | [[P-009]] — MiniDLNA interop |
| `tests/integration_network_isolation.rs` | `tcpdump` shows zero non-Tor packets during cast | [[P-001]] — zero non-Tor traffic |
| `tests/integration_circuit_rotation.rs` | Stream survives a `NEWNYM` signal without > 5 s interruption | [[P-005]] — circuit rotation survival |
| `tests/integration_thermal_throttle.rs` | Inject fake thermal zone above 80 °C; verify bitrate fallback | [[P-010]] — thermal management |
| `tests/integration_drm_master_contention.rs` | `gmediarender` holds master; new cast re-acquires within 2 s | [[BP-ADR-009]] — DRM master retry budget |

### Hardware-in-the-Loop Tests (Pi 4 only)

Run nightly on a real Pi 4 in CI (self-hosted runner). Cannot run on x86_64.

| Test | Validates | Success metric |
|---|---|---|
| `tests/hw_1080p60.rs` | 1080p60 H.264 playback; CPU < 50%; RAM < 200 MB | [[P-002]], [[P-003]] |
| `tests/hw_zero_copy.rs` | `v4l2-ctl` shows buffer passthrough; no memcpy in decode→display | [[P-003]] |
| `tests/hw_thermal_real.rs` | Real thermal zone; sustained 1080p playback stays < 75 °C with passive cooler | [[P-010]] |
| `tests/hw_youtube_cast.rs` | Real YouTube URL → playback within 10 s through Tor | [[P-006]] |

### Conformance Tests

| Protocol | Tool | Cadence |
|---|---|---|
| HTTP REST | `curl` + `pytest` suite in `tests/conformance/http/` | Every PR |
| WebSocket | `wscat` + `pytest` suite in `tests/conformance/ws/` | Every PR |
| UPnP/DLNA | `gupnp-universal-cp` + `pytest` suite in `tests/conformance/dlna/` | Every PR |
| Third-party client interop | VLC, MiniDLNA, Home Assistant, Plex — manual smoke test matrix | Pre-release |

### Security Tests

| Test | Cadence | Validates |
|---|---|---|
| `scripts/verify-network-isolation.sh` (`tcpdump`) | Every PR (network namespace) + nightly on Pi | [[P-001]] — zero non-Tor traffic |
| `cargo-deny` on Rust crate graph | Every PR | No vulnerable / yanked / unlicensed dependencies |
| `cargo-audit` | Every PR + nightly | No RUSTSEC advisories |
| `seccomp` filter escape attempts on `gmediarender` / `yt-dlp` | Nightly | [[BP-ADR-009]], [[BP-ADR-006]] |
| Reproducible Debian build (`sha256sum` match) | Every release | [[BP-ADR-008]] — supply-chain integrity |
| Tor `torrc` baseline diff | Every PR | Hardened config not silently weakened |
| GPG signature verification of installer | Every release | [[BP-ADR-008]] — installer integrity |

### Accessibility Tests

| Test | Cadence | Validates |
|---|---|---|
| `axe-core` against `src/server/web/` | Every PR | [[P-012]] — automated a11y |
| WAVE against `src/server/web/` | Every PR | [[P-012]] — automated a11y |
| Keyboard-only navigation smoke (Tab / Shift-Tab / Enter / Escape) | Pre-release | [[P-012]] — keyboard nav |
| Manual NVDA + VoiceOver pass | Pre-release | [[P-012]] — real screen-reader |

### CI Matrix

| Job | Runner | Trigger | Approx duration |
|---|---|---|---|
| `fmt + clippy -D warnings` | x86_64 Linux | Every PR | 2 min |
| `cargo test --workspace` (unit + integration mocks) | x86_64 Linux | Every PR | 5 min |
| `cargo-deny + cargo-audit` | x86_64 Linux | Every PR | 1 min |
| Conformance suites (HTTP, WS, DLNA) | x86_64 Linux | Every PR | 4 min |
| `verify-network-isolation.sh` in netns | x86_64 Linux | Every PR | 2 min |
| Hardware-in-the-loop | Pi 4 self-hosted | Nightly + release tag | 25 min |
| Reproducible Debian build | x86_64 Linux | Release tag | 8 min |
| Manual accessibility pass | Developer machine | Pre-release | 2 hours |

## Open Questions for Detailed Design

These are deferred to the next blueprint phase and tracked here so they are not lost:

1. **[[T-001]] SOCS5 forwarder implementation** — Is the forwarder a separate process or a tokio task inside `bogdan-playback`? Affects BP-ADR-005's failure modes.
2. **[[T-002]] Web UI framework** — Vanilla HTML + a few hundred lines, or a lightweight framework (e.g. leptos)? Affects BP-ADR-012's test surface.
3. **[[T-003]] yt-dlp pinning strategy** — Pin to a release tag, or to a commit SHA? Affects BP-ADR-006's update cadence.
4. **[[T-004]] WebSocket ring buffer eviction policy** — Drop oldest, or drop on category? Affects BP-ADR-004's reconnect semantics.
5. **[[T-005]] `gmediarender` `seccomp` profile** — Baseline `SCMP_ACT_KILL` for everything except the syscall whitelist; whitelist TBD.
6. **[[T-006]] mDNS fallback** — If `bogdan.local` doesn't resolve, what's the documented fallback? IP-address printed on first boot?
7. **[[T-007]] Thermal supervisor and resolver contract** — What does the resolver return when no lower-bitrate variant exists? Needs a typed `NoLowerVariant` error.
8. **[[T-008]] Browser extension MV3 storage** — Persist last-used Pi address in `chrome.storage.local` or `sync`? Affects BP-ADR-007 multi-device UX.
