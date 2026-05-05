# picast-tor

Manages the Tor daemon lifecycle, SOCKS5 proxy pool, stream isolation via per-site username hashing, and circuit health monitoring. Ensures all outbound traffic from PiCast is anonymized and DNS-leak-proof.

## Purpose

The tor crate routes all outbound network traffic (yt-dlp URL resolution, GStreamer HTTP media fetches) through the Tor network to protect the user's IP address from content servers, ISPs, and network observers. It provides stream isolation so that different websites use independent Tor circuits, preventing cross-site correlation attacks — YouTube and Vimeo traffic never share a circuit, even though they originate from the same Pi device. The crate also monitors circuit health via periodic SOCKS5 connectivity checks, and automatically restarts the Tor daemon if it becomes unresponsive. All DNS resolution is forced through Tor's DNSPort to prevent leakage.

## Public API

| Item | Kind | Description |
|------|------|-------------|
| `TorManager` | struct | Implements `TorTrait`; manages daemon lifecycle, SOCKS5 proxy, and stream isolation |
| `TorManager::new(binary, torrc, socks_port)` | constructor | Configure paths and port; does NOT start the daemon yet |
| `CircuitMonitor` | struct | Background task performing periodic SOCKS5 connectivity checks |
| `CircuitHealth` | struct | Health check result: `reachable` (bool), `latency_ms` (u64), `circuit_count` (u32) |
| `TorError` | enum | Error variants: `DaemonStart`, `NotReady`, `SocksHandshake`, `CircuitFailed`, `DnsLeak` |

Implements `picast_session::interfaces::TorTrait`:

| Method | Description |
|--------|-------------|
| `ensure_running()` | Start Tor daemon if not running, wait until SOCKS5 is accepting connections |
| `socks_addr()` | Return proxy address string (e.g., `"127.0.0.1:9050"`) |
| `health_check()` | Try connecting to SOCKS5 proxy, return `CircuitHealth` with latency |

Additional methods:

| Method | Description |
|--------|-------------|
| `start()` | Spawn Tor daemon as child process with configured torrc |
| `wait_ready(timeout)` | Poll SOCKS5 port until it accepts connections (up to `timeout` seconds) |
| `shutdown()` | Send SIGTERM to daemon, wait 5s, SIGKILL if still running, reap child |
| `isolated_stream(site_host)` | Create a SOCKS5 connection with per-site isolation credentials |
| `socks5_credentials(site_host)` | Derive `(username, password)` from hostname hash for circuit isolation |
| `dns_port()` | Return DNSPort address (e.g., `"127.0.0.1:9053"`) |

## Dependencies

| Dependency | Why |
|------------|-----|
| `picast-session` | Provides `TorTrait` trait definition that this crate implements |
| `tokio` | Async process management (`tokio::process::Command` for Tor daemon), TCP streams for SOCKS5 |
| `sha2` | SHA-256 hashing of hostnames for SOCKS5 stream isolation usernames |
| `hex` | Hex encoding of SHA-256 hash bytes for SOCKS5 username field |
| `thiserror` | Structured error types for Tor operations |
| `tracing` | Debug logging for daemon lifecycle, SOCKS5 handshakes, circuit health |

## SOCKS5 Username Hashing Scheme (Stream Isolation)

Tor's `IsolateSOCKSAuth` flag causes connections with different SOCKS5 username/password credentials to use separate circuits. PiCast exploits this by deriving a unique username from each site's hostname, ensuring that different sites get different circuits while the same site consistently uses the same circuit.

### Algorithm

```
username = hex(SHA-256(hostname))[0..16]    ← First 16 hex characters of SHA-256
password = "picast-isolation"               ← Constant (not security-critical)
```

### Examples

| Site Host | SHA-256 Hash (first 16 hex) | SOCKS5 Username | Assigned Circuit |
|-----------|----------------------------|-----------------|------------------|
| youtube.com | `7d2d3e1f4a5b6c8d...` | `7d2d3e1f4a5b6c8d` | Circuit A |
| vimeo.com | `9e8f7a6b5c4d3e2f...` | `9e8f7a6b5c4d3e2f` | Circuit B |
| twitch.tv | `a1b2c3d4e5f6a7b8...` | `a1b2c3d4e5f6a7b8` | Circuit C |
| youtube.com | `7d2d3e1f4a5b6c8d...` | `7d2d3e1f4a5b6c8d` | Circuit A (reused!) |

### Why This Works

1. **Same site → same username → same circuit**: All requests to `youtube.com` produce the same hash, so Tor assigns them to the same circuit. This is beneficial because CDN caches (like Google's edge servers) are per-exit-node — reusing the circuit means the CDN cache is warm.

2. **Different sites → different usernames → different circuits**: YouTube and Vimeo traffic never share a circuit, so an observer at one site cannot correlate traffic with the other. This prevents cross-site tracking even if the Tor exit node is compromised.

3. **Deterministic**: The hash is deterministic — no random state is involved. Restarting PiCast or reconnecting produces the same username for the same site, which means the same circuit (if it still exists). This avoids unnecessary circuit builds.

4. **Not security-critical**: The SOCKS5 username/password fields are used purely for circuit isolation, not authentication. They are visible only on the loopback interface between PiCast and the local Tor daemon — they never leave the Pi. An attacker who can read loopback traffic has already compromised the Pi.

### Implementation

```rust
fn socks5_credentials(site_host: &str) -> (String, String) {
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(site_host.as_bytes());
    let hash = hasher.finalize();
    let username = hex::encode(&hash[..8]);  // First 8 bytes = 16 hex chars
    let password = "picast-isolation".to_string();
    (username, password)
}
```

## Bandwidth Expectations

Tor's bandwidth is fundamentally limited by the voluntary exit relay infrastructure. PiCast targets 720p as the default quality tier because 1080p streaming through Tor is unreliable on many exit nodes. The ABR controller monitors buffer fill and downshifts automatically when throughput drops.

| Scenario | Expected Speed | Notes |
|----------|---------------|-------|
| YouTube 1080p via Tor | 2–5 Mbps | Unreliable; depends on exit relay capacity. Works ~60% of the time. |
| YouTube 720p via Tor | 1.5–3 Mbps | Usually sufficient for smooth playback. Works ~85% of the time. |
| YouTube 480p via Tor | 0.8–1.5 Mbps | Very reliable; fallback when 720p is too slow. |
| Direct (no Tor) | 20–50 Mbps | Pi 4 Ethernet limit. Tor adds 10–50× latency overhead. |
| Tor circuit build | 1–5 s | Initial connection delay. Warm circuits are reused for same-site requests. |
| Tor circuit rotation | every 10 min | Default Tor behavior. Causes a brief (~200ms) interruption; buffer absorbs it. |
| First Tor bootstrap | 30–60 s | Download consensus, build initial circuits. Only on first run or stale data. |
| Subsequent starts | 5–15 s | Consensus is cached; only new descriptors need fetching. |

### Bandwidth and Quality Tier Mapping

| ABR Tier | Target Bitrate | Tor Viability | Recommended Default |
|----------|---------------|---------------|---------------------|
| 360p | 800 Kbps | Always works | Emergency fallback |
| 480p | 1.5 Mbps | Works reliably | Good for congested exits |
| 720p | 3 Mbps | Works most of the time | **Default for Tor mode** |
| 1080p | 5 Mbps | Unreliable | Only with fast exit + buffer |

### Buffer Requirements for Tor Streaming

At 720p with a 3 Mbps stream over Tor, the GStreamer `queue2` buffer should be at least 50 MB (providing ~30 seconds of playback buffer). This absorbs Tor's variable-bandwidth delivery: a circuit might deliver 4 Mbps for several seconds and then stall for 500ms while a new relay is selected. The buffer smooths these interruptions without visible playback impact.

## torrc Configuration

The Tor daemon is started with a custom torrc that includes these critical settings:

```torrc
# SOCKS5 proxy with per-username circuit isolation
SocksPort 9050 IsolateSOCKSAuth

# DNS resolution through Tor (prevents DNS leaks)
DNSPort 9053

# Control port for circuit inspection (optional)
ControlPort 9051

# No exit relaying — we only use Tor as a client
ExitPolicy reject *:*

# Data directory (avoids conflict with system Tor)
DataDirectory /var/lib/picast/tor

# Log level
Log notice stderr

# Bandwidth limits (reduce Tor network load)
RelayBandwidthRate 5 MBytes
RelayBandwidthBurst 10 MBytes

# Connection padding for traffic analysis resistance
ConnectionPadding 1
ReducedConnectionPadding 0

# Circuit build timeout (faster startup)
CircuitBuildTimeout 30

# Entry guards (3 long-lived, rotated every 30 days)
NumEntryGuards 3
GuardLifetime 30 days
```

## DNS Leak Prevention

All DNS queries MUST go through Tor's DNSPort (9053), not the system resolver. This is enforced at multiple layers:

1. **torrc**: `DNSPort 9053` enables Tor's built-in DNS resolver on localhost.
2. **yt-dlp**: `--proxy=socks5://127.0.0.1:9050` causes yt-dlp to resolve hostnames through the SOCKS5 proxy (which routes DNS through Tor).
3. **GStreamer**: `souphttpsrc proxy=socks5://127.0.0.1:9050` routes all HTTP connections (including DNS) through the SOCKS5 proxy.
4. **iptables**: the `config/iptables.rules` file blocks all outbound traffic on port 53 except to `127.0.0.1:9053`. This is a hard firewall rule that prevents any application from accidentally using the system DNS resolver.

## Implementation Guide for AI Agents

1. **Daemon lifecycle** — `start()` spawns `tor` as a child process via `tokio::process::Command`. `wait_ready()` polls the SOCKS5 port (127.0.0.1:9050) by attempting a TCP connect every 500ms, up to the configured timeout (60s on first boot, 15s on subsequent starts). `shutdown()` sends SIGTERM and reaps the process. Implement this first and verify it works on a Pi.

2. **SOCKS5 handshake** — the `socks5_handshake()` method implements RFC 1928 (SOCKS5 protocol) and RFC 1929 (username/password authentication). This is necessary because Tokio does not have a built-in SOCKS5 client. The handshake sequence is: (1) send version + auth methods, (2) receive method selection, (3) send username/password, (4) receive auth result, (5) send connect request with destination, (6) receive connect response. Test with a known-good Tor proxy.

3. **Stream isolation** — the `socks5_credentials()` method hashes the hostname using SHA-256 and takes the first 16 hex characters. Test that: (a) two different hostnames produce different usernames, (b) the same hostname always produces the same username, (c) the username is exactly 16 hex characters.

4. **Circuit monitor** — `CircuitMonitor::check()` tries to establish a SOCKS5 connection through Tor and measures the latency. If the connection fails 3 times in a row, trigger an automatic restart. Implement as a background task with a 30-second check interval.

5. **Auto-restart** — if the Tor process exits unexpectedly, detect it via `Child::try_wait()` in the circuit monitor task. Call `TorManager::start()` again, then `wait_ready(60)`. If restart fails 3 times in a row, enter degraded mode (log critical error, return errors from all network operations).

6. **Port conflict detection** — before starting, check if port 9050 is already in use (maybe the system Tor service is running). If so, either use a different port or stop the system Tor service. The setup script should handle this, but the runtime should also detect and report the conflict.

## Key Constraints

- **Tor is slow**: accept that 1080p streaming through Tor may not be possible on some exit relays. The ABR controller handles this by downshifting to 720p/480p. Do not attempt to work around this by bypassing Tor — that would violate PiCast's core privacy requirement.

- **SOCKS5 auth is not security**: the username/password in SOCKS5 are used purely for circuit isolation, not authentication. Anyone who can connect to 127.0.0.1:9050 can use the proxy. The SOCKS5 port must not be exposed to the network — bind to 127.0.0.1 only.

- **Daemon data directory**: must be writable by the `picast` user and have mode 0700 (Tor will refuse to start if the directory is world-readable). The setup script creates `/var/lib/picast/tor` with correct permissions.

- **Bootstrap time**: the first time Tor runs, it may take 30–60 seconds to bootstrap (download consensus, build circuits). Subsequent starts are faster (5–15s) if the data directory is preserved. The `wait_ready()` timeout must account for this.

- **Circuit rotation**: Tor rotates circuits every 10 minutes by default. This causes a brief interruption in streaming (~200ms) as the new circuit is built. The GStreamer `queue2` buffer should absorb this. Do NOT disable circuit rotation — it is a security feature.

- **No relay**: PiCast explicitly does not run as a relay or exit node. The Pi's bandwidth is entirely used for its own traffic. The `ExitPolicy reject *:*` and zero relay bandwidth ensure the Pi does not carry third-party Tor traffic.

- **System Tor conflicts**: if the system Tor service (`tor.service`) is running, it may already occupy port 9050. PiCast's setup script should stop the system Tor service or configure PiCast's Tor to use a different SOCKS port (e.g., 9054). At runtime, `TorManager::start()` should detect the port conflict and fail with a clear error message.

## Reference

| Resource | Location |
|----------|----------|
| ADR-004: C Tor daemon over arti | `DECISIONS.md` / `SPECIFICATION.md` §1.4 |
| Tor integration guide | `docs/tor/integration.md` |
| Stream isolation details | `docs/tor/stream-isolation.md` |
| Threat model (DNS leak) | `AGENT.md` (Threat Model Summary) |
| iptables rules | `config/iptables.rules` |
| torrc template | `config/torrc` |
