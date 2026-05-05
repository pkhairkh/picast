# Tor Integration

This document describes how PiCast integrates the Tor anonymity network for privacy-preserving media retrieval. It covers the daemon lifecycle, SOCKS5 proxy configuration, port allocation, data directory management, security considerations, performance tuning, and the auto-restart mechanism.

## Architecture

```
┌──────────────────────────────────────────────────┐
│                 PiCast Process                     │
│                                                   │
│  ┌──────────┐  ┌──────────┐  ┌──────────────┐   │
│  │ Resolver  │  │ GStreamer│  │ CircuitMonitor│   │
│  │ (yt-dlp)  │  │ (soup)   │  │ (health)     │   │
│  └─────┬────┘  └────┬─────┘  └──────┬───────┘   │
│        │             │               │            │
│        ▼             ▼               │            │
│  ┌──────────────────────────┐        │            │
│  │      TorManager          │◀───────┘            │
│  │  ┌──────────────────┐   │                     │
│  │  │  SOCKS5 Proxy    │   │                     │
│  │  │  127.0.0.1:9050  │   │                     │
│  │  └────────┬─────────┘   │                     │
│  └───────────┼─────────────┘                     │
└──────────────┼────────────────────────────────────┘
               │
               ▼
┌──────────────────────────┐
│     Tor Daemon Process    │
│  (child of PiCast)       │
│                          │
│  SOCKS5: 127.0.0.1:9050 │
│  DNS:    127.0.0.1:9053  │
│  Control: 9051           │
│                          │
│  Data: /var/lib/picast/tor│
└──────────┬───────────────┘
           │
           ▼
┌──────────────────────────┐
│      Tor Network          │
│  (entry → middle → exit) │
└──────────────────────────┘
```

PiCast's `TorManager` spawns the C Tor daemon as a child process, configures it with a custom torrc that enables `IsolateSOCKSAuth` for per-site circuit isolation, monitors its health via periodic SOCKS5 connectivity checks, and automatically restarts it if it becomes unresponsive. The resolver (yt-dlp) and GStreamer (souphttpsrc) both route their HTTP connections through the Tor SOCKS5 proxy at `127.0.0.1:9050`.

## Daemon Lifecycle

### Startup Sequence

```
1. TorManager::new()
   └─ Store configuration: binary path, torrc path, SOCKS port, DNS port

2. TorManager::start()
   ├─ Check if SOCKS port is already in use (system Tor conflict)
   ├─ Spawn: tor --defaults-torrc /etc/picast/torrc \
   │             --SocksPort 9050 IsolateSOCKSAuth \
   │             --DNSPort 9053 \
   │             --ControlPort 9051 \
   │             --Log notice stderr \
   │             --DataDirectory /var/lib/picast/tor
   └─ Child process running in background

3. TorManager::wait_ready(timeout=60)
   ├─ Every 500ms: try TCP connect to 127.0.0.1:9050
   ├─ Check: is child process still alive? (try_wait)
   ├─ If connection succeeds: SOCKS5 is accepting connections
   ├─ If timeout expires: return TorError::NotReady
   └─ On first boot: 30–60s for consensus download
       On subsequent boots: 5–15s (cached consensus)

4. Notify SessionManager that Tor is ready
   └─ Protocol servers can now accept cast requests
```

### Runtime Monitoring (CircuitMonitor)

The `CircuitMonitor` runs as a background tokio task, checking Tor health every 30 seconds:

```
Every 30 seconds:
  1. Try connecting to SOCKS5 proxy (127.0.0.1:9050)
  2. If success: record latency in milliseconds
  3. If failure: increment failure counter
  4. If failures > 3 consecutive: trigger TorManager::restart()
  5. Also check: is the Tor child process still alive?
     If not: trigger TorManager::restart()
```

### Shutdown Sequence

```
1. TorManager::shutdown()
   ├─ Send SIGTERM to child process
   ├─ Wait for process to exit (5 second timeout)
   │   └─ If still running after 5s: send SIGKILL
   ├─ Reap child process (wait() to prevent zombie)
   └─ Set ready = false

2. Clean up:
   ├─ Set internal state to NotRunning
   └─ Drop child process handle
```

### Auto-Restart on Failure

If the Tor daemon crashes or becomes unresponsive:

```
1. CircuitMonitor detects SOCKS5 is unreachable (3 consecutive failures)
2. TorManager::start() is called again
3. wait_ready(timeout=60) is called
4. If restart succeeds: resume normal operation
5. If restart fails 3 times in a row:
   └─ Log critical error
   └─ Enter degraded mode: return TorError from all network operations
   └─ Protocol servers report "Tor unavailable" to clients
   └─ Do NOT fall back to direct connections (violates privacy requirement)
```

## SOCKS5 Proxy Configuration

### For yt-dlp (URL Resolution)

```bash
yt-dlp --proxy=socks5://127.0.0.1:9050 <URL>
```

This causes yt-dlp to route all HTTP requests through the Tor SOCKS5 proxy. DNS resolution is also handled by Tor (not the system resolver), preventing DNS leaks. The SOCKS5 username/password fields are used for stream isolation (see `docs/tor/stream-isolation.md`).

### For GStreamer (Media Fetching)

GStreamer's `souphttpsrc` element supports SOCKS5 proxies:

```bash
# Environment variable approach
export SOUP_PROXY=socks5://127.0.0.1:9050

# Or in the pipeline definition
souphttpsrc proxy=socks5://127.0.0.1:9050 location=<URL>
```

**Note on stream isolation**: `souphttpsrc`'s built-in SOCKS5 support does not include username/password authentication. For connections that need per-site stream isolation (different sites using different Tor circuits), PiCast uses its own SOCKS5 client implementation in `TorManager` that sends the appropriate username/password credentials derived from the destination hostname. For connections that don't need isolation (e.g., fetching a resolved CDN URL that's already been isolated by the resolver), the simple `proxy` property suffices.

## Port Allocation

| Port | Protocol | Purpose | Configuration |
|------|----------|---------|---------------|
| 9050 | TCP | SOCKS5 proxy with IsolateSOCKSAuth | `SocksPort 9050 IsolateSOCKSAuth` |
| 9051 | TCP | Tor control port (circuit inspection, signal delivery) | `ControlPort 9051` |
| 9053 | UDP | DNS resolver (prevents DNS leaks) | `DNSPort 9053` |

## Data Directory

```
/var/lib/picast/tor/
├── cached-certs              ← Certificate cache (X.509 certs for directory authorities)
├── cached-microdesc-consensus ← Network consensus (refreshed every hour)
├── cached-microdescs/        ← Microdescriptor cache (relay capability info)
├── lock                      ← Lock file (prevents double-start)
├── state                     ← Tor state file (guard selection, circuit history)
└── keys/                     ← (empty – PiCast doesn't run as a relay or onion service)
```

The data directory must be:
- Owned by the `picast` user
- Mode `0700` (Tor will refuse to start if the directory is group- or world-readable)
- On a filesystem with sufficient space (~50 MB for cached consensus and descriptors)
- Preserved across restarts (contains guard selections that improve security if stable)

## Security Considerations

### 1. No Exit Relaying

PiCast explicitly disables all relay and exit functionality. The Pi's bandwidth is used exclusively for its own traffic:

```torrc
RelayBandwidthRate 0          # No relay traffic
RelayBandwidthBurst 0         # No relay burst
ExitPolicy reject *:*         # Reject all exit connections
```

### 2. DNS Leak Prevention

All DNS queries MUST go through Tor's DNSPort. This is enforced at multiple layers:

| Layer | Mechanism | What It Prevents |
|-------|-----------|-----------------|
| torrc | `DNSPort 9053` | Enables Tor's built-in DNS resolver on localhost |
| yt-dlp | `--proxy=socks5://127.0.0.1:9050` | yt-dlp resolves hostnames through SOCKS5, which routes DNS through Tor |
| GStreamer | `proxy=socks5://127.0.0.1:9050` | souphttpsrc resolves hostnames through SOCKS5 |
| iptables | Block outbound port 53 except to 127.0.0.1:9053 | Hard firewall rule preventing any application from using system DNS |

### 3. No Hidden Services

PiCast does not host any onion services. The torrc disables them:

```torrc
HiddenServiceDir disabled
```

### 4. System Tor Conflicts

PiCast runs its own Tor instance to avoid conflicts with the system Tor service. If both are running on the same port (9050), one will fail to bind. The setup script should detect the conflict and either:

- Stop the system Tor service (`sudo systemctl stop tor`), OR
- Configure PiCast's Tor to use a different SOCKS5 port (e.g., 9054)

### 5. SOCKS5 Port Binding

The SOCKS5 port (9050) binds to `127.0.0.1` only. It must not be exposed to the network — anyone who can connect to the SOCKS5 port can use the Tor proxy, which could be abused for unauthorized traffic. The iptables rules should block external access to port 9050.

## Performance Tuning

### Circuit Build Timeout

Default: 60 seconds. Can be reduced for faster startup on subsequent runs:

```torrc
CircuitBuildTimeout 30
```

### Entry Guards

Tor uses 3 long-lived entry guards for security (preventing entry-node profiling attacks). PiCast uses the default configuration:

```torrc
NumEntryGuards 3
GuardLifetime 30 days
```

### Connection Padding

To reduce traffic analysis attacks (where an observer correlates traffic patterns), enable padding:

```torrc
ConnectionPadding 1
ReducedConnectionPadding 0
```

This adds small amounts of cover traffic, increasing bandwidth usage by ~5% but significantly improving anonymity by making traffic patterns less distinctive.

### Bandwidth Accounting

To limit Tor's bandwidth impact on the Pi's network connection (important on metered connections):

```torrc
AccountingMax 5 GB
AccountingStart day 00:00
```

This caps Tor's daily bandwidth at 5 GB — enough for several hours of 720p streaming but preventing unbounded usage.

### Bandwidth Rate Limiting

To prevent Tor from consuming all available bandwidth:

```torrc
RelayBandwidthRate 5 MBytes
RelayBandwidthBurst 10 MBytes
```

These limits apply to relay traffic only (which PiCast doesn't use), but they also serve as a safety net. Client traffic (PiCast's own streaming) is not rate-limited by these settings — it's limited by the Tor network's available exit relay bandwidth (typically 2–5 Mbps).
