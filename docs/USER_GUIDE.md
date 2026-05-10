# boGDan User Guide

**Version:** 0.1.0-alpha
**Audience:** End users installing and operating boGDan on a Raspberry Pi 4B+
**Last updated:** 2026-05-10

---

## Table of Contents

1. [Quick Start](#1-quick-start)
2. [Hardware Setup](#2-hardware-setup)
3. [Installation](#3-installation)
4. [Configuration](#4-configuration)
5. [Browser Extension](#5-browser-extension)
6. [Casting Media](#6-casting-media)
7. [DLNA / VLC Casting](#7-dlna-vlc-casting)
8. [HTTP API Reference](#8-http-api-reference)
9. [Monitoring and Logs](#9-monitoring-and-logs)
10. [Troubleshooting](#10-troubleshooting)
11. [FAQ](#11-faq)
12. [Uninstalling](#12-uninstalling)

---

## 1. Quick Start

The fastest way to get boGDan running on your Raspberry Pi 4B+:

```bash
# 1. Flash Raspberry Pi OS Lite 64-bit (Bookworm) to your SD card
# 2. Boot the Pi, connect it to your network via Ethernet
# 3. SSH into the Pi and run:
curl -sSL https://raw.githubusercontent.com/pkhairkh/picast/main/scripts/setup.sh | sudo bash

# 4. Reboot to apply kernel overlay changes
sudo reboot

# 5. After reboot, start boGDan
sudo systemctl start bogdan

# 6. Install the browser extension on your computer
#    Chrome:  Load /usr/share/bogdan/extension-chrome/ as unpacked extension
#    Firefox: Load /usr/share/bogdan/extension-firefox/ as temporary add-on

# 7. Open a video page, click the boGDan extension icon, press Cast
```

Total time from flash to first cast: approximately 15–20 minutes (most of which is downloading dependencies and compiling the Rust binary on the Pi's ARM CPU).

---

## 2. Hardware Setup

### Required Hardware

| Component | Specification | Notes |
|-----------|---------------|-------|
| **Board** | Raspberry Pi 4B+ (4 GB recommended) | The 2 GB model works but may struggle with high-bitrate streams |
| **Display** | HDMI monitor or TV | Connected to either HDMI port (HDMI 0 preferred) |
| **Storage** | 8 GB+ microSD card | Class 10 / A2 recommended for faster boot |
| **Network** | Ethernet cable | Wi-Fi works but is not recommended due to latency and bandwidth variability |
| **Power** | 5 V / 3 A USB-C PSU | Undervoltage causes instability; use the official PSU |

### Optional Hardware

| Component | Purpose |
|-----------|---------|
| Heatsink / fan | Prevents thermal throttling during long playback sessions |
| USB SSD / HDD | Faster storage than microSD; extends card lifespan |
| 3.5mm audio device | Alternative audio output to HDMI |

### Raspberry Pi OS

Use **Raspberry Pi OS Lite 64-bit (Bookworm)**. The desktop version is not supported because X11/Wayland holds DRM master, which prevents boGDan from rendering video through the DRM/KMS direct-rendering pipeline. The Lite image is also smaller, uses less RAM, and has a smaller attack surface.

Download from: https://www.raspberrypi.com/software/operating-systems/

Flash with Raspberry Pi Imager or:

```bash
# Replace /dev/sdX with your SD card device
xzcat raspberry-pi-os-lite-arm64.img.xz | sudo dd bs=4M of=/dev/sdX status=progress conv=fsync
```

### Enable SSH (Headless Setup)

Before booting the Pi, mount the boot partition and create an empty `ssh` file:

```bash
# After flashing, mount the boot partition
sudo mkdir -p /mnt/boot
sudo mount /dev/sdX1 /mnt/boot
sudo touch /mnt/boot/ssh
sudo umount /mnt/boot
```

This enables SSH on first boot so you can configure the Pi remotely.

---

## 3. Installation

### Method A: One-Command Setup (Recommended)

```bash
curl -sSL https://raw.githubusercontent.com/pkhairkh/picast/main/scripts/setup.sh | sudo bash
```

This script performs 9 steps automatically:

1. **Pre-flight checks** — verifies root, OS, Pi hardware
2. **Installs dependencies** — tor, GStreamer, yt-dlp, dev libraries
3. **Installs Rust** — rustup and stable toolchain
4. **Configures kernel overlays** — enables `vc4-kms-v3d` for DRM/KMS
5. **Configures Tor** — installs `config/torrc` with stream isolation
6. **Configures firewall** — applies iptables rules with default-DROP OUTPUT policy
7. **Builds boGDan** — `cargo build --release`
8. **Installs boGDan** — binary, user, config, TLS certs, systemd service
9. **Verifies installation** — checks binary, services, directories

#### Setup Options

```bash
# Skip building from source (use if you've already built)
sudo bash scripts/setup.sh --skip-build

# Skip Tor configuration (use if Tor is already configured)
sudo bash scripts/setup.sh --skip-tor

# Skip firewall configuration (use if iptables is already configured)
sudo bash scripts/setup.sh --skip-firewall

# Cross-compile from x86_64 host for Pi
sudo bash scripts/setup.sh --cross-compile

# Uninstall boGDan completely
sudo bash scripts/setup.sh --uninstall
```

### Method B: Debian Package

If you have a pre-built `.deb` file:

```bash
# Copy the deb to the Pi
scp bogdan_0.1.0_arm64.deb pi@<pi-ip>:/tmp/

# Install it
ssh pi@<pi-ip> 'sudo dpkg -i /tmp/bogdan_0.1.0_arm64.deb'

# The postinst script automatically:
# - Creates the bogdan system user
# - Applies iptables rules
# - Configures Tor
# - Enables the systemd service

# Start the service
ssh pi@<pi-ip> 'sudo systemctl start bogdan'
```

### Method C: Manual Installation

For advanced users who want full control over each step:

```bash
# 1. Install dependencies
sudo apt update
sudo apt install -y tor gstreamer1.0-tools gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly gstreamer1.0-libav gstreamer1.0-alsa \
    yt-dlp gmediarender iptables dnsmasq avahi-daemon \
    build-essential pkg-config libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev libdrm-dev libgbm-dev \
    libegl-dev libgles-dev libsqlite3-dev libssl-dev

# 2. Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# 3. Build boGDan
git clone https://github.com/pkhairkh/picast.git
cd picast
cargo build --release

# 4. Create user
sudo useradd -r -m -s /usr/sbin/nologin bogdan
sudo usermod -aG video,render,audio bogdan

# 5. Install binary
sudo cp target/release/bogdan /usr/local/bin/bogdan-server
sudo chmod 755 /usr/local/bin/bogdan-server

# 6. Install config
sudo mkdir -p /etc/bogdan/providers.d
sudo cp bogdan.toml.example /etc/bogdan/bogdan.toml
sudo cp providers.d/*.toml /etc/bogdan/providers.d/
sudo cp config/torrc /etc/bogdan/torrc
sudo cp config/iptables.rules /etc/bogdan/iptables.rules
sudo chown -R bogdan:bogdan /etc/bogdan

# 7. Configure Tor
sudo cp /etc/tor/torrc /etc/tor/torrc.bogdan-backup
sudo cp config/torrc /etc/tor/torrc
sudo systemctl restart tor
sudo systemctl enable tor

# 8. Apply firewall rules
sudo iptables-restore < config/iptables.rules
sudo apt install -y iptables-persistent
sudo netfilter-persistent save

# 9. Install systemd service
sudo sed 's|ExecStart=/usr/local/bin/bogdan|ExecStart=/usr/local/bin/bogdan-server|' \
    config/bogdan.service | sudo tee /etc/systemd/system/bogdan.service
sudo systemctl daemon-reload
sudo systemctl enable bogdan

# 10. Create data directory
sudo mkdir -p /var/lib/bogdan
sudo chown bogdan:bogdan /var/lib/bogdan

# 11. Start
sudo systemctl start bogdan
```

### Post-Installation Verification

After installation, verify everything is working:

```bash
# Check service status
sudo systemctl status bogdan

# Check that the API responds
curl -s http://localhost:8585/api/health

# Check that Tor is running
sudo systemctl status tor

# Run the smoke test
sudo bash /usr/share/bogdan/scripts/smoke-test.sh
```

---

## 4. Configuration

boGDan reads its configuration from `/etc/bogdan/bogdan.toml`. Environment variables override the config file values.

### Configuration File

```toml
# /etc/bogdan/bogdan.toml

[server]
# HTTP API listen address (0.0.0.0 = all interfaces)
http_addr = "0.0.0.0:8585"

# WebSocket listen address (for browser extension real-time updates)
ws_addr = "0.0.0.0:8586"

# SQLite database path (resolve cache)
db_path = "/var/lib/bogdan/resolve-cache.db"

# TLS certificate paths (enable HTTPS/WSS)
# tls_cert_path = "/etc/bogdan/tls/bogdan.pem"
# tls_key_path = "/etc/bogdan/tls/bogdan-key.pem"

[tor]
# Tor SOCKS5h proxy address (the 'h' forces remote DNS resolution)
socks_addr = "127.0.0.1:29050"

# Tor control port (for circuit health monitoring)
control_port = 9052

# Tor cookie authentication path
# cookie_path = "/var/run/tor/control.authcookie"

[display]
# DRM device path (Pi 4: /dev/dri/card0)
drm_device = "/dev/dri/card0"

[playback]
# ALSA audio device (empty = default HDMI audio)
# audio_device = "hw:0,0"

[dlna]
# Friendly name shown in VLC and other DLNA controllers
friendly_name = "boGDan"

# DLNA port
port = 49152

[logging]
# Log level: trace, debug, info, warn, error
level = "info"
```

### Environment Variable Overrides

Every config value can be overridden with an environment variable:

| Environment Variable | Config Key | Default |
|---------------------|------------|---------|
| `BOGDAN_HTTP_ADDR` | `server.http_addr` | `0.0.0.0:8585` |
| `BOGDAN_WS_ADDR` | `server.ws_addr` | `0.0.0.0:8586` |
| `BOGDAN_TOR_SOCKS` | `tor.socks_addr` | `127.0.0.1:29050` |
| `BOGDAN_TOR_CONTROL_PORT` | `tor.control_port` | `9052` |
| `BOGDAN_AUDIO_DEVICE` | `playback.audio_device` | (default HDMI) |
| `BOGDAN_DLNA_NAME` | `dlna.friendly_name` | `boGDan` |
| `BOGDAN_LOG_LEVEL` | `logging.level` | `info` |

### Provider Configuration

Video hosting providers are configured in `/etc/bogdan/providers.d/`. Each provider has its own `.toml` file:

```bash
/etc/bogdan/providers.d/
├── voe.toml          # Voe video hosting resolver
└── doodstream.toml   # DoodStream video hosting resolver
```

To add a new provider that uses existing deobfuscation primitives, simply create a new `.toml` file in this directory — no Rust code changes are required. See the existing provider configs for the schema reference.

To disable a provider, set `enabled = false` in its config file, or simply delete/rename the `.toml` file.

### TLS Configuration

By default, boGDan uses plain HTTP and WS. To enable HTTPS and WSS:

```bash
# Option A: Use the certificate generated by setup.sh
# (Already configured if you used setup.sh)

# Option B: Generate certificates manually
sudo bash /path/to/picast/deploy/generate-certs.sh

# Option C: Use your own certificates
sudo mkdir -p /etc/bogdan/tls
sudo cp your-cert.pem /etc/bogdan/tls/bogdan.pem
sudo cp your-key.pem /etc/bogdan/tls/bogdan-key.pem
sudo chown bogdan:bogdan /etc/bogdan/tls/*
sudo chmod 600 /etc/bogdan/tls/bogdan-key.pem
```

Then uncomment the `tls_cert_path` and `tls_key_path` lines in `/etc/bogdan/bogdan.toml` and restart the service:

```bash
sudo systemctl restart bogdan
```

The browser extension will need to trust the CA certificate. The `generate-certs.sh` script creates a private CA (`/etc/bogdan/tls/ca.pem`) that can be imported into your browser's trust store.

---

## 5. Browser Extension

The boGDan browser extension detects video URLs on web pages and sends them to your Pi for casting.

### Installing the Chrome Extension

1. Open Chrome and navigate to `chrome://extensions`
2. Enable **Developer mode** (toggle in top-right)
3. Click **Load unpacked**
4. Select the directory `/usr/share/bogdan/extension-chrome/` (or `src/extension/` from the repo on your development machine)
5. The boGDan icon appears in your extensions bar

### Installing the Firefox Extension

1. Open Firefox and navigate to `about:debugging#/runtime/this-firefox`
2. Click **Load Temporary Add-on**
3. Select `manifest.json` from `/usr/share/bogdan/extension-firefox/` (or `src/extension/` from the repo)
4. The extension loads temporarily (until Firefox restarts)

For a persistent Firefox installation, build the extension as an XPI:

```bash
cd src/extension
bash build.sh --firefox
# Output: build/bogdan-firefox-0.3.0.zip
# Install via about:addons → Install Add-on From File
```

### Configuring the Extension

1. Click the boGDan icon in your browser toolbar
2. Click the **Settings** (gear) icon
3. Enter your Pi's IP address (e.g., `192.168.1.42`) and port (default: `8585`)
4. If using TLS, check the "Use HTTPS" option and set port to `8585` (HTTPS)
5. Click **Save**

### Using the Extension

1. Navigate to any page with a video (YouTube, Vimeo, etc.)
2. The extension icon shows a badge when a video URL is detected
3. Click the boGDan icon
4. The detected URL is shown — click **Cast**
5. The popup shows playback status (Playing, Paused, Buffering)
6. Use the popup controls to pause, resume, seek, adjust volume, or stop

### Extension Permissions

The extension requests these permissions:

| Permission | Purpose |
|-----------|---------|
| `activeTab` | Access the current tab's URL when you click the extension icon |
| `storage` | Save your Pi's IP address and port |
| `webRequest` | Detect video URLs in network requests |
| `alarms` | Periodic status polling |
| `notifications` | Show cast success/failure notifications |
| `optional_host_permissions` | Only requested if you enable "detect media on all sites" |

Broad host permissions are **optional** and only requested when you explicitly enable the "detect media on all sites" feature. By default, the extension only accesses the current tab when you click the icon.

---

## 6. Casting Media

### From the Browser Extension

1. Open a video page in your browser
2. Click the boGDan extension icon
3. Press **Cast** — the video starts playing on your TV within seconds
4. Use the popup controls for pause, resume, seek, volume, and stop

### From the HTTP API

```bash
# Cast a YouTube video
curl -X POST http://<pi-ip>:8585/api/cast \
    -H 'Content-Type: application/json' \
    -d '{"url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ"}'

# Cast a direct MP4 URL
curl -X POST http://<pi-ip>:8585/api/cast \
    -H 'Content-Type: application/json' \
    -d '{"url": "https://example.com/video.mp4"}'

# Check playback status
curl http://<pi-ip>:8585/api/status

# Pause
curl -X POST http://<pi-ip>:8585/api/pause

# Resume (pause again toggles back to playing)
curl -X POST http://<pi-ip>:8585/api/pause

# Seek to 60 seconds
curl -X POST http://<pi-ip>:8585/api/seek \
    -H 'Content-Type: application/json' \
    -d '{"seconds": 60}'

# Set volume to 50%
curl -X POST http://<pi-ip>:8585/api/volume \
    -H 'Content-Type: application/json' \
    -d '{"level": 0.5}'

# Stop playback
curl -X POST http://<pi-ip>:8585/api/stop
```

### Supported URL Types

| URL Type | Example | Resolution Method |
|----------|---------|-------------------|
| YouTube | `https://youtube.com/watch?v=...` | yt-dlp |
| Vimeo | `https://vimeo.com/...` | yt-dlp |
| Direct media | `https://example.com/video.mp4` | Passthrough |
| HLS stream | `https://example.com/stream.m3u8` | HLS segment download |
| Voe (provider) | `https://voe.sx/...` | Custom resolver |
| DoodStream (provider) | `https://doodstream.com/...` | Custom resolver |
| 1800+ other sites | Various | yt-dlp |

### How Casting Works

1. **URL Classification** — boGDan checks the URL against known provider domains (`/etc/bogdan/providers.d/*.toml`)
2. **Resolution** — The URL is resolved to a direct media URL:
   - Custom providers (Voe, DoodStream) use config-driven deobfuscation pipelines
   - YouTube and 1800+ sites use yt-dlp through Tor SOCKS5h
   - Direct media URLs are passed through without modification
3. **CDN Preflight** — Before downloading, boGDan checks the CDN URL with a `Range: bytes=0-0` request to verify accessibility
4. **Progressive Download** — Media data is fetched through Tor SOCKS5 with per-site circuit isolation and fed into a GStreamer `appsrc` element
5. **Hardware Decoding** — H.264 is decoded by the Pi's V4L2 stateful decoder (zero-copy DMA-BUF)
6. **DRM/KMS Display** — Decoded video frames are displayed directly on HDMI through the DRM/KMS atomic modesetting pipeline

---

## 7. DLNA / VLC Casting

boGDan appears as a DLNA MediaRenderer on your LAN. Any DLNA-compatible app can cast to it.

### From VLC

1. Open VLC on your computer or phone (must be on the same LAN as the Pi)
2. Play a video or audio file
3. Click the **Render** button (or menu: Playback → Renderer)
4. Select **boGDan** from the renderer list
5. The media starts playing on your TV

### From Home Assistant

1. Add the boGDan device as a DLNA media player in your Home Assistant configuration
2. Use the `media_player.play_media` service to cast URLs

### DLNA Configuration

The DLNA renderer name and port are configurable:

```toml
[dlna]
friendly_name = "boGDan"    # Name shown in VLC
port = 49152                 # DLNA port
```

After changing DLNA settings, restart the service:

```bash
sudo systemctl restart bogdan
```

### How DLNA Works

boGDan uses `gmediarender` as the DLNA MediaRenderer backend. When you cast from VLC:

1. VLC discovers boGDan via SSDP (UDP multicast on port 1900)
2. VLC sends the media URL to boGDan via UPnP AV Transport
3. gmediarender fetches the URL and plays it through a GStreamer pipeline
4. boGDan monitors gmediarender via D-Bus and maps state changes to the session manager

Note: DLNA playback does **not** route through Tor by default because gmediarender fetches the URL directly. For Tor-routed playback, use the HTTP API or browser extension instead.

---

## 8. HTTP API Reference

All endpoints are on the HTTP server (default port 8585).

### POST /api/cast

Cast a URL to the Pi.

**Request:**
```json
{"url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ"}
```

**Response:** `202 Accepted`
```json
{"session_id": "uuid", "status": "resolving"}
```

**Error responses:**
- `400 Bad Request` — Invalid JSON, empty body, or blocked URL scheme (`file://`, `data:`, `javascript:`)
- `409 Conflict` — A session is already active (stop it first)
- `429 Too Many Requests` — Rate limit exceeded (30 req/10s per IP)

### POST /api/pause

Toggle pause/resume. If playing, pauses. If paused, resumes.

**Response:** `200 OK`
```json
{"status": "paused"}
```

### POST /api/seek

Seek to a position in the current media.

**Request:**
```json
{"seconds": 60}
```

**Response:** `200 OK`
```json
{"status": "seeking"}
```

### POST /api/volume

Set the playback volume.

**Request:**
```json
{"level": 0.5}
```

`level` is a float from 0.0 (mute) to 1.0 (maximum).

**Response:** `200 OK`
```json
{"volume": 0.5}
```

### POST /api/stop

Stop playback and return to idle state.

**Response:** `200 OK`
```json
{"status": "idle"}
```

### GET /api/status

Get the current playback status.

**Response:** `200 OK`
```json
{
  "state": "playing",
  "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
  "position_secs": 45.3,
  "duration_secs": 212.0,
  "volume": 0.8,
  "buffer_percent": 75.2
}
```

**States:** `idle`, `resolving`, `buffering`, `playing`, `paused`, `seeking`

### GET /api/health

Health check endpoint.

**Response:** `200 OK`
```json
{"status": "ok"}
```

### WebSocket (port 8586)

Connect to `ws://<pi-ip>:8586/` for real-time playback status updates.

**Messages received:**
```json
{"type": "MEDIA_STATUS", "data": {"state": "playing", "position_secs": 45.3, ...}}
```

The WebSocket server sends `MEDIA_STATUS` events whenever the playback state changes. This is how the browser extension updates its popup in real time.

---

## 9. Monitoring and Logs

### Service Logs

```bash
# Follow boGDan logs in real time
journalctl -u bogdan -f

# Show last 100 lines
journalctl -u bogdan -n 100

# Show logs from the current boot only
journalctl -u bogdan -b

# Filter by log level
journalctl -u bogdan -p err    # Errors only
journalctl -u bogdan -p warning # Warnings and above
```

### GStreamer Debug Logs

For troubleshooting video playback issues, enable GStreamer debug output:

```bash
# Enable GStreamer debug (set in bogdan.service or environment)
export GST_DEBUG=3                    # Warning level
export GST_DEBUG=4                    # Info level (verbose)
export GST_DEBUG=v4l2h264dec:5       # Debug only V4L2 decoder
export GST_DEBUG_FILE=/tmp/gst.log   # Write to file instead of journal
sudo systemctl restart bogdan
```

### Tor Logs

```bash
# Check Tor status and circuits
sudo systemctl status tor
journalctl -u tor -n 50

# List active Tor circuits (requires ControlPort)
echo -e "AUTHENTICATE\r\nGETINFO circuit-status\r\nQUIT\r\n" | nc 127.0.0.1 9052
```

### System Monitoring

```bash
# Check CPU, memory, and process info
top -p $(pgrep bogdan)

# Check open file descriptors (should be stable over time)
ls /proc/$(pgrep bogdan)/fd | wc -l

# Check DRM device usage
fuser /dev/dri/card0

# Monitor network throughput
sudo iftop -i eth0

# Check resolved URL cache size
ls -la /var/lib/bogdan/resolve-cache.db
```

### Automated Monitoring Scripts

boGDan includes several monitoring and QA scripts:

| Script | Purpose | Duration |
|--------|---------|----------|
| `smoke-test.sh` | Quick health and functionality check | ~30 seconds |
| `mem-test.sh` | 8-hour memory leak test with RSS/FD tracking | 8 hours |
| `soak-test.sh` | 100 cast/stop cycle resource exhaustion test | ~60 minutes |
| `verify-network-isolation.sh` | Verify all traffic routes through Tor | ~2 minutes |

---

## 10. Troubleshooting

### boGDan won't start

**Symptom:** `systemctl status bogdan` shows failed status.

**Diagnosis:**

```bash
# Check the journal for error messages
journalctl -u bogdan -n 50 --no-pager

# Common issues:
# 1. DRM master busy (another compositor is running)
# 2. Tor not running
# 3. Config file syntax error
# 4. Permission denied on /dev/dri/card0
```

**Solutions:**

| Error | Cause | Fix |
|-------|-------|-----|
| `DRM master busy` | Another process holds DRM master (X11, Wayland, console) | Use Pi OS Lite (no desktop); kill other DRM users with `sudo fuser -k /dev/dri/card0` |
| `Connection refused (Tor)` | Tor is not running | `sudo systemctl start tor` |
| `Expected vc4 driver` | Wrong DRM driver | Ensure `vc4-kms-v3d` overlay is enabled in `/boot/config.txt` |
| `Permission denied: /dev/dri/card0` | bogdan user not in `video` group | `sudo usermod -aG video,render bogdan` then restart |
| `Config parse error` | Invalid TOML syntax | Validate with `bogdan-server --check-config` or manually inspect |

### Video doesn't play

**Symptom:** Cast succeeds but TV shows black screen or "No Signal".

**Diagnosis:**

```bash
# Check GStreamer pipeline state in the logs
journalctl -u bogdan | grep -i 'pipeline\|error\|warning'

# Verify V4L2 decoder is available
gst-inspect-1.0 v4l2h264dec

# Check DRM/KMS plane status
sudo cat /sys/kernel/debug/dri/0/state
```

**Solutions:**

| Error | Cause | Fix |
|-------|-------|-----|
| No V4L2 decoder | Missing kernel module or GStreamer plugin | `sudo apt install gstreamer1.0-plugins-bad` and ensure `v4l2h264dec` is in `gst-inspect-1.0` output |
| Black screen | Pipeline started but no data | Check Tor bandwidth; try a lower-bitrate video |
| Tearing/Artifacts | vsync timing issue | Ensure `kmssink` is using the correct CRTC; check `config.txt` for `hdmi_enable_4kp60=1` |

### Playback stutters / buffers frequently

**Symptom:** Video pauses frequently to buffer.

**Cause:** Tor bandwidth is insufficient for the video's bitrate. Tor typically provides 0.5–5 Mbps, while 1080p H.264 needs 2–4 Mbps.

**Mitigations:**

1. **Use lower quality** — Cast 720p or 480p instead of 1080p (yt-dlp can be configured to prefer lower resolutions)
2. **Wait for buffering** — Check `buffer_percent` via `/api/status`; higher buffer percentage means smoother playback
3. **Try a different time** — Tor bandwidth varies; retry later
4. **Check for CDN speed limits** — Some Voe CDN URLs include `sp=380` (380 kbps cap); boGDan tries bypass URLs automatically

### Tor connection fails

**Symptom:** URLs fail to resolve, errors mention "SOCKS" or "Tor".

**Diagnosis:**

```bash
# Check Tor service
sudo systemctl status tor

# Test Tor connectivity
curl --socks5-hostname 127.0.0.1:9050 https://check.torproject.org/api/ip

# Check Tor ports are listening
ss -tlnp | grep -E '9050|9051|29050|9052'
```

**Solutions:**

- Restart Tor: `sudo systemctl restart tor`
- Check iptables: `sudo iptables -L OUTPUT -v -n` — ensure Tor SOCKS ports are allowed
- Check torrc: `sudo tor --verify-config` — validate configuration syntax
- Check disk space: Tor fails silently if the disk is full — `df -h`

### yt-dlp errors

**Symptom:** YouTube and other yt-dlp-supported sites fail to resolve.

**Solutions:**

```bash
# Update yt-dlp (site changes frequently break extractors)
sudo yt-dlp -U

# Test yt-dlp directly
yt-dlp --proxy socks5h://127.0.0.1:9050 -j "https://www.youtube.com/watch?v=dQw4w9WgXcQ"

# If yt-dlp is missing
sudo apt install yt-dlp
# or
pip3 install yt-dlp
```

### DNS leaks

**Symptom:** DNS queries go to your ISP instead of through Tor.

**Verification:**

```bash
# Check /etc/resolv.conf
cat /etc/resolv.conf
# Expected: nameserver 127.0.0.1

# Run the network isolation verification script
sudo bash /usr/share/bogdan/scripts/verify-network-isolation.sh
```

**Fix:** Ensure dnsmasq is running and forwarding to Tor's DNSPort:

```bash
sudo apt install dnsmasq
echo "server=127.0.0.1#9053" | sudo tee /etc/dnsmasq.d/bogdan.conf
echo "listen-address=127.0.0.1" | sudo tee -a /etc/dnsmasq.d/bogdan.conf
sudo systemctl restart dnsmasq
```

---

## 11. FAQ

### What video codecs are supported?

| Codec | Hardware Decode | Software Fallback |
|-------|----------------|-------------------|
| H.264 (AVC) | V4L2 stateful M2M at up to 1080p60 | avdec_h264 at 720p30 |
| HEVC (H.265) | Not supported (SAND format incompatibility) | — |
| VP9 | No hardware decode | avdec_vp9 (slow on Pi) |
| MPEG-2 | No hardware decode | avdec_mpeg2 (works) |

H.264 is the primary codec. yt-dlp is configured to prefer H.264 formats when available.

### Does boGDan work with Netflix/Disney+/Amazon Prime?

No. These services use DRM (Widevine CDM) which requires a browser environment. boGDan has no browser component — it renders video directly through DRM/KMS.

### Can I use Wi-Fi instead of Ethernet?

Technically yes, but it is not recommended. Wi-Fi adds latency, jitter, and reduced bandwidth compared to Ethernet. Since all media traffic already routes through Tor (which adds its own latency), Wi-Fi makes buffering more frequent.

### Can I run boGDan on a Raspberry Pi 3 or Pi Zero?

No. The Pi 3 and Pi Zero lack the V4L2 stateful H.264 decoder and the DRM/KMS support required by boGDan. A Raspberry Pi 4B+ with at least 2 GB RAM is the minimum.

### Can I use boGDan with a desktop environment?

No. boGDan requires exclusive access to DRM master, which means no other compositor (X11, Wayland, or console) can be using `/dev/dri/card0`. Use Raspberry Pi OS **Lite** (no desktop).

### How do I add support for a new video hosting site?

Create a new `.toml` file in `/etc/bogdan/providers.d/` following the schema used by `voe.toml` and `doodstream.toml`. If the site uses a deobfuscation method that's already supported (ROT13, Base64, char-shift, reverse, marker-strip), no Rust code changes are needed. If the site uses a novel deobfuscation method, you will need to implement a new `DeobfuscationStep` in the resolver crate.

For sites that yt-dlp already supports (1,800+), no provider config is needed — just cast the URL and yt-dlp handles resolution.

### Is my traffic truly anonymous?

boGDan routes all content resolution and media downloading through Tor with per-site circuit isolation. However, perfect anonymity cannot be guaranteed:

- The Tor exit relay can see which CDN you are connecting to (but not the specific video URL due to HTTPS)
- Traffic correlation attacks are possible if an adversary controls both your entry and exit relays
- The Pi's ISP can see that you are using Tor (but not what you are accessing)
- DNS leaks would reveal your intent — the iptables rules and dnsmasq configuration prevent this

Use `scripts/verify-network-isolation.sh` to verify that no traffic bypasses Tor.

### How much power does the Pi use while playing video?

Approximately 3.5–4.5 watts during H.264 hardware decoding with DRM/KMS display. This is lower than a Pi running a desktop environment because no compositor or browser is running.

---

## 12. Uninstalling

### Via setup.sh

```bash
sudo bash scripts/setup.sh --uninstall
```

This stops the service, removes the binary, user, data directories, and systemd service file. It does **not** remove the Tor config or iptables rules — review those manually.

### Via dpkg

```bash
# Remove (keeps config)
sudo dpkg -r bogdan

# Purge (removes everything including config, data, and user)
sudo dpkg --purge bogdan
```

### Manual Uninstall

```bash
sudo systemctl stop bogdan
sudo systemctl disable bogdan
sudo rm /etc/systemd/system/bogdan.service
sudo systemctl daemon-reload
sudo rm /usr/local/bin/bogdan-server
sudo userdel bogdan
sudo rm -rf /var/lib/bogdan /tmp/bogdan /etc/bogdan /usr/share/bogdan
# Restore Tor config if needed
sudo cp /etc/tor/torrc.bogdan-backup /etc/tor/torrc
sudo systemctl restart tor
```

---

## Getting Help

- **GitHub Issues:** https://github.com/pkhairkh/picast/issues
- **Documentation:** See the `docs/` directory for detailed per-module documentation
- **Security issues:** See [SECURITY.md](../SECURITY.md) for responsible disclosure
