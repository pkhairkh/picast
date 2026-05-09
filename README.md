# boGDan

[![CI](https://github.com/pkhairkh/bogdan/actions/workflows/ci.yml/badge.svg)](https://github.com/pkhairkh/bogdan/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org/)

**Privacy-first Tor-routed media casting appliance for Raspberry Pi 4B+**

boGDan turns your Raspberry Pi 4 into a privacy-focused media receiver. All content resolution and media fetching routes through the Tor network — your ISP cannot see what you watch. Video is decoded by the Pi's H.264 hardware decoder and displayed on your TV via HDMI through a zero-copy DMA-BUF pipeline, with no display server, no browser, and no DRM stack.

The boGCast protocol provides the communication layer between senders and the receiver, supporting HTTP REST, WebSocket, and UPnP/DLNA interfaces.

## Quick Start

```bash
curl -sSL https://raw.githubusercontent.com/pkhairkh/bogdan/main/scripts/setup.sh | sudo bash
```

This installs all dependencies, builds boGDan with hardware acceleration, configures Tor and firewall rules, and installs the systemd service. Then:

1. **Install the browser extension** — [Chrome](#) or [Firefox](#) (Manifest V3)
2. **Cast a video** — open a video page, click the boGDan extension icon, press **Cast**
3. Your TV starts playing within seconds

## Hardware Requirements

| Requirement | Specification |
|-------------|---------------|
| Board | Raspberry Pi 4B+ (4 GB recommended) |
| Display | HDMI monitor or TV |
| Storage | 8 GB+ microSD card |
| Network | Ethernet (Wi-Fi not recommended) |
| Power | 5 V / 3 A USB-C |

## What boGDan Does

- **Resolves** video URLs from 1,800+ sites via custom resolvers and yt-dlp through Tor
- **Fetches** media streams through Tor SOCKS5 with per-site circuit isolation
- **Decodes** H.264 in hardware using V4L2 stateful M2M at up to 1080p60
- **Displays** on HDMI via DRM/KMS zero-copy pipeline — no X11, no Wayland
- **Accepts** cast commands from browser extension, VLC, DLNA apps, HTTP API, or WebSocket

## What boGDan Cannot Do

| Limitation | Reason |
|---|---|
| **DRM content** (Netflix, Disney+, etc.) | Requires Widevine CDM + Chromium — incompatible with the appliance model |
| **Google Cast V2** | Google enforces device authentication; unofficial receivers cannot complete the handshake |
| **HEVC/H.265 hardware decode** | The BCM2711 HEVC decoder outputs SAND format, which the HVS cannot display; requires format conversion that breaks zero-copy |
| **Screen mirroring** | Requires a display server (X11/Wayland) to capture windows |
| **AirPlay** | Requires FairPlay DRM for protected content |
| **Guaranteed smooth 1080p over Tor** | Tor bandwidth is 0.5–5 Mbps; 1080p H.264 needs ~2–4 Mbps. Some CDN providers also add speed limits (`sp=380` → 380 kbps cap). Playback may stutter on high-bitrate streams. |

## Architecture

```
Sender Device                          Raspberry Pi 4
┌──────────────┐   boGCast (HTTP)     ┌──────────────────────────────────┐
│ Browser      │──────────────────────→│ protocols (HTTP + WS + DLNA)     │
│ Extension    │   boGCast (WS)        │          │                       │
├──────────────┤──────────────────────→│    session (state machine)       │
│ VLC / DLNA   │   UPnP/DLNA           │          │                       │
├──────────────┤──────────────────────→│  resolver (custom + yt-dlp)     │
│ Home Asst.   │                       │          │ via Tor SOCKS5h       │
│ / curl       │                       │          ▼                       │
└──────────────┘                       │  playback (progressive download) │
                                       │                                  │
                                       │  CDN → Tor → SOCKS Fwd → reqwest│
                                       │    → channel → appsrc → queue2   │
                                       │         ↓                        │
                                       │   parsebin (auto-detect codec)   │
                                       │    ├→ queue → v4l2h264dec (HW)   │
                                       │    │   → v4l2convert (ISP)       │
                                       │    │   → kmssink (DRM Plane 0)   │
                                       │    └→ queue → avdec_aac          │
                                       │        → audioconvert → vol      │
                                       │        → alsasink / pulsesink    │
                                       │                                  │
                                       │  display (DRM/KMS → HDMI)       │
                                       │  tor (C daemon, IsolateSOCKSAuth)│
                                       └──────────────────────────────────┘
```

The data path uses **progressive download** rather than real-time streaming. A `reqwest` HTTP/2 client fetches data from the CDN through a local SOCKS5 forwarder (which tunnels through Tor with per-site circuit isolation), and feeds it into a GStreamer `appsrc` element. This allows pre-buffering, throughput measurement, and CDN preflight checks before starting playback. The SOCKS forwarder ensures that the CDN sees the same Tor exit IP as the resolver, preventing IP-bound CDN token mismatches.

Before starting the full download, boGDan performs a **CDN preflight check** using GET with `Range: bytes=0-0` (not HEAD — many CDNs return 404 for HEAD). If the CDN URL contains a speed-limit parameter (`sp=380` = 380 kbps cap), boGDan tries bypass URLs (sp=99999, sp= stripped). If all bypasses return 403, it falls back to the original rate-limited URL. Only if the original URL returns 403 does playback fail — this indicates an IP block requiring re-resolution through a different Tor circuit.

The GStreamer pipeline uses `parsebin` for auto-detection of container and codec formats, then dynamically builds the video decode chain in a pad-added callback based on the detected codec (H.264 → v4l2h264dec, HEVC → v4l2slh265dec, software fallback → avdec_h264). The `v4l2convert` element uses the bcm2835-ISP hardware to convert between pixel formats (e.g., SAND128→NV12 for HEVC). Video decode output flows as DMA-BUF file descriptors directly to `kmssink` for DRM/KMS display — the CPU never touches decoded pixel data.

## Configuration

boGDan reads configuration from a TOML file and environment variables. Copy the example config:

```bash
sudo cp bogdan.toml.example /etc/bogdan/bogdan.toml
```

Key settings (environment variables override the config file):

| Variable | Default | Description |
|----------|---------|-------------|
| `BOGDAN_HTTP_ADDR` | `0.0.0.0:8585` | HTTP API listen address |
| `BOGDAN_WS_ADDR` | `0.0.0.0:8586` | WebSocket listen address |
| `BOGDAN_TOR_SOCKS` | `127.0.0.1:29050` | Tor SOCKS5 proxy address |
| `BOGDAN_TOR_CONTROL_PORT` | `9052` | Tor control port |
| `BOGDAN_AUDIO_DEVICE` | `` | ALSA audio device (empty = default) |
| `BOGDAN_DLNA_NAME` | `boGDan` | DLNA friendly name on the LAN |
| `BOGDAN_LOG_LEVEL` | `info` | Log level (trace, debug, info, warn, error) |

TLS is supported — set `tls_cert_path` and `tls_key_path` in the config to enable HTTPS/WSS.

## Known Issues

| Problem | Workaround |
|---------|-----------|
| **CDN speed limit (`sp=380`)** | Some Voe CDN URLs include `sp=380` capping throughput at ~380 kbps. boGDan tries bypass URLs first (sp=99999, sp= stripped) but the CDN treats these as signature violations and returns 403. Falls back to the rate-limited URL, which may cause stuttering. |
| **DRM master busy on restart** | If gmediarender hasn't fully released DRM master when a new session starts, the pipeline may fail to acquire it. The service restart handles this, but there can be a brief delay. |
| **Tor circuit congestion** | Tor bandwidth varies (0.5–5 Mbps). High-bitrate streams may buffer frequently. Use the `/api/status` endpoint to monitor `bufferPercent`. |
| **yt-dlp extractor breakage** | Site changes can break yt-dlp extractors. Update with `sudo yt-dlp -U`. |

## Development

```bash
# Clone and build (works on x86_64 without Pi hardware)
git clone https://github.com/pkhairkh/bogdan.git
cd bogdan
cargo build --workspace

# Build with hardware acceleration (requires GStreamer + DRM dev libs)
cargo build --release --features hw,hevc

# Run tests
cargo test --workspace

# Lint
cargo clippy --workspace -- -D warnings
cargo fmt --check

# Deploy to Pi
./deploy.sh
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full development workflow.

## Project Structure

```
bogdan/
├── src/
│   ├── server/        Main binary, config, startup orchestration
│   ├── protocols/     HTTP REST API, WebSocket, DLNA (gmediarender)
│   ├── session/       State machine, CDN retry logic, queue
│   ├── resolver/      URL classification, custom resolvers, yt-dlp
│   ├── playback/      Progressive download, GStreamer pipeline, SOCKS forwarder
│   ├── display/       DRM/KMS plane control, atomic modesetting
│   ├── tor/           SOCKS5 proxy pool, stream isolation, circuit health
│   ├── v3d/           V3D GPU compute shader (SAND→NV12, experimental)
│   └── extension/     Browser extension (Chrome + Firefox, Manifest V3)
├── config/            systemd unit, torrc, iptables rules
├── deploy/            Pi-specific service file, config, cert generation
├── scripts/           Setup, smoke-test, network isolation verification
├── packaging/         Debian package build scripts
└── docs/              Architecture decisions, per-module documentation
```

## Documentation

| Document | Description |
|----------|-------------|
| [ARCHITECTURE.md](ARCHITECTURE.md) | System architecture (hardware, pipeline, protocols, Tor) |
| [SPECIFICATION.md](SPECIFICATION.md) | API contracts, format matrix, GStreamer pipeline specs |
| [DECISIONS.md](DECISIONS.md) | Architecture Decision Records (ADR-001 through ADR-009) |
| [AGENT.md](AGENT.md) | AI agent onboarding and codebase navigation |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Development workflow and conventions |
| [SECURITY.md](SECURITY.md) | Vulnerability reporting and threat model |
| [docs/](docs/) | Per-module deep dives and ADR files |

## License

[MIT](LICENSE)
