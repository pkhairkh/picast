# boGDan v0.1.0-alpha

**Release Date:** 2026-05-10
**Tag:** `v0.1.0-alpha`

This is the first public alpha release of boGDan — a privacy-first, Tor-routed media casting appliance for Raspberry Pi 4B+.

## What is boGDan?

boGDan turns your Raspberry Pi 4 into a secure media receiver. All content resolution and media fetching routes through the Tor network — your ISP cannot see what you watch. Video is decoded by the Pi's H.264 hardware decoder and displayed on your TV via HDMI through a zero-copy DMA-BUF pipeline, with no display server, no browser, and no DRM stack.

## Features

- **Tor-routed media** — All URL resolution and media downloads go through Tor SOCKS5h with per-site circuit isolation
- **Hardware-accelerated playback** — H.264 V4L2 stateful decode at up to 1080p60, zero-copy DMA-BUF to DRM/KMS
- **Config-driven providers** — Video hosting providers (Voe, DoodStream) are defined in TOML files; adding a new provider requires only a new `.toml` file
- **Deobfuscation pipeline** — Pluggable deobfuscation steps (ROT13, Base64, char-shift, reverse, marker-strip) composed from provider configs
- **Browser extension** — Chrome and Firefox (Manifest V3) with CSP, optional host permissions, and no innerHTML
- **HTTP API** — RESTful API with per-IP rate limiting, URL scheme validation, and machine-readable error codes
- **WebSocket** — Real-time playback status updates for the browser extension
- **DLNA/UPnP** — Appears in VLC's renderer list; auto-restart on gmediarender crash
- **iptables firewall** — Default-DROP OUTPUT policy ensures no traffic bypasses Tor
- **DNS leak prevention** — dnsmasq forwards to Tor DNSPort; SOCKS5h for all proxy connections

## Installation

### Option 1: One-Command Install
```bash
curl -sSL https://raw.githubusercontent.com/pkhairkh/picast/main/scripts/setup.sh | sudo bash
sudo reboot
```

### Option 2: Debian Package
```bash
sudo dpkg -i bogdan_0.1.0_arm64.deb
sudo systemctl start bogdan
```

### Option 3: SD Card Image
Flash `bogdan-0.1.0-pi4-arm64.img.xz` to an SD card with Raspberry Pi Imager, insert into Pi 4, and power on.

## Downloads

| File | Size | SHA-256 |
|------|------|---------|
| `bogdan_0.1.0_arm64.deb` | ~15 MB | See `checksums.txt` |
| `bogdan-0.1.0-pi4-arm64.img.xz` | ~800 MB | See `checksums.txt` |
| `bogdan-chrome-0.3.0.zip` | ~50 KB | See `checksums.txt` |
| `bogdan-firefox-0.3.0.zip` | ~50 KB | See `checksums.txt` |
| `checksums.txt` | — | — |

## Known Issues

- **CDN speed limits** — Some Voe CDN URLs include `sp=380` capping throughput at ~380 kbps. Bypass attempts (sp=99999) typically return 403. Falls back to rate-limited URL, which may cause stuttering.
- **DRM master busy on restart** — If gmediarender hasn't released DRM master when a new session starts, the pipeline may fail. Service restart handles this with a brief delay.
- **Tor bandwidth variability** — Tor provides 0.5–5 Mbps. High-bitrate streams may buffer frequently. 720p is more reliable than 1080p.
- **yt-dlp extractor breakage** — Site changes can break yt-dlp extractors. Update with `sudo yt-dlp -U`.
- **HEVC not supported** — The BCM2711 HEVC decoder outputs SAND format incompatible with HVS display. H.264 only for now.
- **No DRM content** — Netflix, Disney+, etc. require Widevine CDM + Chromium, which is incompatible with the appliance model.
- **DLNA not Tor-routed** — gmediarender fetches directly, not through Tor. Use HTTP API or browser extension for Tor-routed playback.

## Test Results

- **632 workspace tests** passing (display: 41, playback: 40, protocols: 50, integration: 25, resolver: 476+)
- **Security audit:** 3 PASS, 5 PARTIAL, 0 FAIL, 10 prioritized remediations
- **Clippy:** Clean with `-D warnings`
- **Sprint 1–7:** All DoD items met

## Hardware Requirements

| Component | Specification |
|-----------|---------------|
| Board | Raspberry Pi 4B+ (4 GB recommended) |
| Display | HDMI monitor or TV |
| Storage | 8 GB+ microSD card |
| Network | Ethernet (Wi-Fi not recommended) |
| Power | 5 V / 3 A USB-C |
| OS | Raspberry Pi OS Lite 64-bit (Bookworm) |

## Documentation

- [User Guide](docs/USER_GUIDE.md) — Installation, configuration, daily usage, troubleshooting, FAQ
- [Security Hardening](docs/SECURITY.md) — iptables, DNS leaks, circuit isolation, TLS, attack surface, physical security
- [Architecture](ARCHITECTURE.md) — System architecture deep-dive
- [API Reference](SPECIFICATION.md) — HTTP API contracts and GStreamer pipeline specs

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development workflow.

## License

[MIT](LICENSE)
