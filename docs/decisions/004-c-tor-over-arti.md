# ADR-004: C Tor Daemon Over arti

| Field        | Value          |
|--------------|----------------|
| **ID**       | ADR-004        |
| **Status**   | ACCEPTED       |
| **Date**     | 2025-01-17     |
| **Supersedes** | —            |
| **Superseded by** | —         |

## Context

PiCast routes all media traffic through Tor to provide privacy for the user. The Tor routing layer must support:

1. **Per-site circuit isolation** — Different media sources should use separate Tor circuits to prevent correlation attacks. If a user casts from YouTube and then from a news site, these should not share exit nodes.
2. **SOCKS5 proxy interface** — GStreamer's `souphttpsrc` needs a SOCKS5 proxy endpoint to route HTTP requests through Tor.
3. **Stable, production-grade operation** — The Tor layer must be reliable since it handles all network traffic.
4. **Rust integration** — PiCast is a Rust project; a Rust-native Tor implementation would be ideal from a build and integration perspective.

### arti Assessment

[arti](https://gitlab.tor-project.org/tpo/core/arti/) is the Tor Project's Rust implementation of the Tor client. It is under active development and has made significant progress:

- **Missing `IsolateSOCKSAuth`**: arti does not support `IsolateSOCKSAuth` — the feature that allows different SOCKS5 usernames to be mapped to separate Tor circuits. This is the critical gap. PiCast needs per-site circuit isolation, and `IsolateSOCKSAuth` is the mechanism for achieving this in a SOCKS5 proxy.
- **SOCKS5 support**: arti does provide a SOCKS5 proxy, but without `IsolateSOCKSAuth`, all connections through the proxy share circuits by default.
- **Maturity**: arti is not yet recommended for production use by the Tor Project. The C Tor daemon has 20+ years of production deployment.
- **Future potential**: arti is the long-term future of Tor. Once it gains `IsolateSOCKSAuth` support, PiCast should migrate.

### C Tor Daemon Assessment

The C Tor daemon (`tor`) is the production-grade Tor client:

- **`IsolateSOCKSAuth` support**: The `SocksPort` option supports `IsolateSOCKSAuth` which isolates circuits based on SOCKS5 username/password. PiCast uses the hostname (or a hash of it) as the SOCKS5 username, ensuring each media source gets its own circuit.
- **Separate process**: Runs as a separate systemd service (`tor.service`), providing process isolation. If Tor crashes, PiCast can detect and restart it without affecting the media pipeline.
- **Mature and stable**: Decades of production use, extensive documentation, and well-understood failure modes.
- **Overhead**: The C daemon consumes ~30 MB RAM on Pi 4, which is acceptable.

## Decision

PiCast uses the C Tor daemon as a separate process managed by systemd. The `picast-tor` crate interacts with Tor via:

1. **SOCKS5 proxy** at `127.0.0.1:9050` with `IsolateSOCKSAuth` enabled
2. **Control port** at `127.0.0.1:9051` for circuit monitoring and signal delivery
3. **Tor config** managed via `config/torrc` with hardened settings:

```
SocksPort 9050 IsolateSOCKSAuth
ControlPort 9051
CookieAuthentication 1
AvoidDiskWrites 1
SafeLogging 1
```

The `picast-tor` crate generates per-hostname SOCKS5 credentials:
- Username: SHA-256 hash of the media URL hostname (first 16 hex chars)
- Password: empty (IsolateSOCKSAuth uses username for isolation; password is not required)

This ensures that `youtube.com` and `example.com` traffic uses different Tor circuits.

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ Per-site circuit isolation | `IsolateSOCKSAuth` maps each media hostname to a separate Tor circuit, preventing cross-site correlation |
| ✅ Production-grade Tor | C daemon has 20+ years of deployment; stable, well-documented, well-tested |
| ✅ Process isolation | Tor runs in separate process; crashes don't affect PiCast media pipeline; systemd manages restarts |
| ✅ Control port access | Can monitor circuit status, send NEWNYM signal for circuit rotation, detect Tor downtime |
| ✅ Hardened configuration | `AvoidDiskWrites`, `SafeLogging`, and `CookieAuthentication` reduce forensic footprint on SD card |
| ❌ Separate process overhead | ~30 MB RAM for the Tor daemon process; additional systemd service to manage |
| ❌ Not Rust-native | C Tor daemon is a C program; cannot be embedded as a Rust crate; requires system package (`apt install tor`) |
| ❌ Longer build/flash time | OS image must include Tor package and configuration; increases base image size by ~20 MB |
| ❌ Future migration needed | When arti gains `IsolateSOCKSAuth`, PiCast should migrate to reduce process overhead and simplify deployment |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **arti (Rust Tor client)** | No `IsolateSOCKSAuth` support — cannot achieve per-site circuit isolation; not yet production-ready per Tor Project's own guidance; lacks control port feature parity with C daemon |
| **No Tor** | PiCast's core value proposition is privacy-preserving media casting; removing Tor eliminates the project's reason for existence; users could just use VLC or any DLNA renderer without Tor |
