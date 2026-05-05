# PiCast

**Tor-routed, zero-copy media casting appliance for Raspberry Pi 4B+**

PiCast turns your Pi 4 into a networked media receiver that fetches and plays video through the Tor network, using the Pi's dedicated H.264 hardware decoder with a zero-copy DMA-BUF pipeline directly to HDMI. No display server, no browser, no DRM — just pure hardware-accelerated playback on your TV.

## How It Works

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

1. **Send a URL** from any device — browser extension, VLC, DLNA app, or HTTP API
2. **Pi resolves it** via yt-dlp through Tor (1,800+ sites supported)
3. **Pi fetches and decodes** the stream using H.264 hardware decode (V4L2 M2M)
4. **Pi displays** on HDMI via DRM/KMS zero-copy pipeline (~3% CPU, ~5W)

## Key Properties

| Property | Implementation |
|----------|---------------|
| **Privacy** | All content resolution and media fetching routes through Tor |
| **Efficiency** | Zero-copy DMA-BUF from V4L2 decoder to HVS to HDMI — no CPU in the display path |
| **Minimalism** | No X11, no Wayland, no compositor, no browser — DRM/KMS direct |
| **Compatibility** | UPnP/DLNA (VLC, Home Assistant), HTTP API, browser extension |
| **Adaptive** | Tor-aware ABR controller monitors buffer fill, switches quality automatically |

## Supported Content

- **Sites**: YouTube, Vimeo, Twitch, PeerTube, Odysee, Rumble, and 1,800+ more via yt-dlp
- **Codecs**: H.264 hardware (1080p60), VP9/AV1 software fallback
- **Protocols**: HLS, DASH, progressive HTTP
- **Subtitles**: SRT, VTT, auto-generated (via yt-dlp)
- **Not supported**: DRM content (Netflix, Disney+), HEVC hardware (deferred to v2)

## Quick Start

```bash
# Flash Raspberry Pi OS Lite 64-bit (bookworm)
# Enable overlays in /boot/config.txt:
#   dtoverlay=vc4-kms-v3d

# Install dependencies
sudo apt install -y tor gstreamer1.0-plugins-{base,bad,good,ugly} \
  gstreamer1.0-tools gmediarender yt-dlp

# Build PiCast
cargo build --release

# Configure Tor
sudo cp config/torrc /etc/tor/torrc
sudo systemctl restart tor

# Configure firewall
sudo cp config/iptables.rules /etc/iptables/rules.v4

# Run
sudo -u picast ./target/release/picast

# Or install as a service
sudo cp config/picast.service /etc/systemd/system/
sudo systemctl enable --now picast
```

## Project Structure

```
picast/
├── AGENT.md              # AI agent instructions (read this first)
├── ARCHITECTURE.md       # Full system architecture document
├── SPECIFICATION.md      # API contracts, format matrix, config specs
├── DECISIONS.md          # Architecture Decision Records
├── src/
│   ├── server/           # Main binary
│   ├── protocols/        # HTTP API, WebSocket, DLNA
│   ├── session/          # State machine, queue, ABR
│   ├── resolver/         # URL classification, yt-dlp subprocess
│   ├── playback/         # GStreamer pipeline management
│   ├── display/          # DRM/KMS plane control
│   ├── tor/              # SOCKS5 proxy, stream isolation
│   └── extension/        # Browser extension (Manifest V3)
├── config/               # systemd unit, torrc, iptables rules
├── docs/                 # Detailed per-module documentation
└── tests/                # Integration tests
```

## Documentation

| Document | Description |
|----------|-------------|
| [AGENT.md](AGENT.md) | AI agent onboarding — read this first |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Complete system architecture |
| [SPECIFICATION.md](SPECIFICATION.md) | API contracts and technical specs |
| [DECISIONS.md](DECISIONS.md) | Architecture Decision Records |
| [docs/hardware/](docs/hardware/) | BCM2711, V4L2, HVS deep dives |
| [docs/protocols/](docs/protocols/) | HTTP, WebSocket, DLNA specs |
| [docs/playback/](docs/playback/) | GStreamer pipelines, ABR, DRM/KMS |
| [docs/tor/](docs/tor/) | Tor integration, stream isolation |
| [docs/extension/](docs/extension/) | Browser extension design |

## Hardware Requirements

| Requirement | Specification |
|-------------|--------------|
| Board | Raspberry Pi 4B+ (any RAM variant, 4GB recommended) |
| Display | Any HDMI monitor/TV |
| Network | Ethernet (Wi-Fi not recommended) |
| Power | 5V/3A USB-C |
| Storage | 8GB+ microSD |

## License

MIT
