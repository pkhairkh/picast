# PiCast

[![CI](https://github.com/pkhairkh/picast/actions/workflows/ci.yml/badge.svg)](https://github.com/pkhairkh/picast/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.75+](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)

**Tor-routed, zero-copy media casting appliance for Raspberry Pi 4B+**

PiCast turns your Raspberry Pi 4 into a privacy-focused media receiver that fetches and plays video through the Tor network, using the Pi's dedicated H.264 hardware decoder with a zero-copy DMA-BUF pipeline directly to HDMI — no display server, no browser, no DRM, just pure hardware-accelerated playback on your TV at ~3% CPU and ~5W.

## Quick Start

```bash
curl -sSL https://raw.githubusercontent.com/pkhairkh/picast/main/scripts/setup.sh | sudo bash
```

This one command installs all dependencies, builds PiCast, configures Tor and firewall rules, and installs the systemd service. After it completes:

1. **Install the browser extension** — [Chrome Web Store](#) or [Firefox Add-ons](#) (Manifest V3)
2. **Cast your first video** — open a YouTube video, click the PiCast extension icon, then press **Cast**
3. Your TV connected to the Pi starts playing within seconds

## Hardware Requirements

| Requirement | Specification |
|-------------|---------------|
| Board | Raspberry Pi 4B+ (4 GB recommended) |
| Display | HDMI monitor or TV |
| Storage | 8 GB+ microSD card |
| Network | Ethernet (Wi-Fi not recommended) |
| Power | 5 V / 3 A USB-C |

## What PiCast Does

- Resolves video URLs from 1,800+ sites via **yt-dlp** through Tor
- Fetches media streams through **Tor SOCKS5** with per-domain circuit isolation
- Decodes H.264 in hardware using **V4L2 M2M** at 1080p60
- Displays on HDMI via **DRM/KMS** zero-copy pipeline — no X11, no Wayland
- Accepts cast commands from browser extension, VLC, DLNA apps, or HTTP API

## Features

- **Tor routing** — all resolution and fetching routes through Tor; DNS never leaks
- **Zero-copy H.264** — DMA-BUF from V4L2 decoder through HVS to HDMI; CPU stays out of the display path
- **No display server** — DRM/KMS direct-to-HDMI; no compositor, no browser, minimal attack surface
- **Multi-protocol input** — HTTP API, WebSocket, UPnP/DLNA, and browser extension (Chrome & Firefox)
- **Adaptive bitrate** — Tor-aware ABR controller monitors buffer health and switches quality automatically
- **Subtitle support** — SRT, VTT, and auto-generated subtitles via yt-dlp
- **Software fallback** — VP9/AV1 decoded in software (720p30) when hardware H.264 isn't available

## Architecture

```
Any device on LAN                    Raspberry Pi 4
┌────────────────┐   URL via HTTP    ┌──────────────────────┐
│ Browser        │──────────────────→│ PiCast Receiver       │
│ Extension      │                   │                       │
├────────────────┤   UPnP/DLNA       │  yt-dlp ──→ Tor ──→  │
│ VLC / DLNA app │──────────────────→│  resolve stream URL   │
├────────────────┤                   │       │               │
│ Home Assistant │   WebSocket       │  GStreamer + V4L2 HW  │
│ / curl         │──────────────────→│  decode → DRM/KMS     │
└────────────────┘                   │       │               │
                                     │  HDMI Monitor ◄───────│
                                     └──────────────────────┘
```

## Configuration

PiCast reads configuration from environment variables or a `.env` file:

| Variable | Default | Description |
|----------|---------|-------------|
| `PICAST_HTTP_PORT` | `8585` | HTTP API listen port |
| `PICAST_WS_PORT` | `8586` | WebSocket listen port |
| `PICAST_TOR_MODE` | `enabled` | `enabled`, `disabled`, or `optional` |
| `PICAST_TOR_SOCKS_PORT` | `9050` | Tor SOCKS5 proxy port |
| `PICAST_TOR_CONTROL_PORT` | `9051` | Tor control port |
| `PICAST_VOLUME` | `80` | Default volume (0–100) |

## Troubleshooting

| Problem | Solution |
|---------|----------|
| **No video on HDMI** | Verify `dtoverlay=vc4-kms-v3d` in `/boot/config.txt` and reboot |
| **Tor won't connect** | Check `sudo systemctl status tor` and ensure port 9050 is open |
| **yt-dlp fails to resolve** | Update yt-dlp: `sudo yt-dlp -U` — site extractors change frequently |
| **High CPU during playback** | Software fallback is active; check `vc4-kms-v3d` overlay is loaded |
| **Extension can't find Pi** | Ensure Pi and browser are on the same LAN; check `PICAST_HTTP_PORT` |
| **Choppy playback over Tor** | Normal for high-bitrate streams; the ABR controller will downshift quality |

## Development

```bash
# Clone and build (works on x86_64 without Pi hardware)
git clone https://github.com/pkhairkh/picast.git
cd picast
cargo build --workspace

# Run tests
cargo test --workspace

# Lint
cargo clippy --workspace -- -D warnings
cargo fmt --check

# Cross-compile for Pi (requires aarch64-linux-gnu-gcc)
cargo build --target aarch64-unknown-linux-gnu --release
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full development workflow.

## Project Structure

```
picast/
├── src/
│   ├── server/        Main binary + integration tests
│   ├── protocols/     HTTP API, WebSocket, DLNA
│   ├── session/       State machine, queue, ABR
│   ├── resolver/      URL classification, yt-dlp subprocess
│   ├── playback/      GStreamer pipeline management
│   ├── display/       DRM/KMS plane control
│   ├── tor/           SOCKS5 proxy, stream isolation
│   └── extension/     Browser extension (Manifest V3)
├── config/            systemd unit, torrc, iptables rules
├── scripts/           Setup and development scripts
└── docs/              Per-module documentation and ADRs
```

## Documentation

| Document | Description |
|----------|-------------|
| [ARCHITECTURE.md](ARCHITECTURE.md) | Complete system architecture |
| [SPECIFICATION.md](SPECIFICATION.md) | API contracts and technical specs |
| [DECISIONS.md](DECISIONS.md) | Architecture Decision Records |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Development workflow and conventions |
| [SECURITY.md](SECURITY.md) | Vulnerability reporting and threat model |
| [CHANGELOG.md](CHANGELOG.md) | Version history |
| [AGENT.md](AGENT.md) | AI agent onboarding |
| [docs/](docs/) | Per-module deep dives |

## License

[MIT](LICENSE)
