---
doc: problem_catalog
project: picast
version: 1
phase: problem_catalog
author: stronghold-agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# Problem Catalog: boGDan

## Context

boGDan is a privacy-first Tor-routed media casting appliance for Raspberry Pi 4B+. It turns a Raspberry Pi into a media receiver where all content resolution and media fetching routes through the Tor network, preventing ISPs from seeing what users watch. Video is decoded by the Pi's H.264 hardware decoder via a zero-copy DMA-BUF pipeline, displayed on TV through HDMI — no display server, no browser, no DRM stack.

The project targets privacy-conscious users who want to cast media to their TV without exposing their viewing habits to network observers. It supports HTTP REST, WebSocket, and UPnP/DLNA interfaces via the boGCast protocol.

## Stakeholders

- [[S-001]] Privacy-conscious users — end user — wants to cast media without ISP surveillance
- [[S-002]] Raspberry Pi owners — end user — wants a low-power, always-on media appliance
- [[S-003]] Content senders — extension users — wants to cast from browser/phone to TV
- [[S-004]] Open-source contributors — developer — wants a clean, hackable Rust codebase
- [[S-005]] Tor community — advocate — wants more Tor use cases beyond web browsing

## Constraints

- **Technical:** Must run on Raspberry Pi 4B+ (ARM64, 4GB+ RAM). Must use H.264 hardware decoder (V4L2 stateful API). Must route all traffic through Tor. Must support zero-copy DMA-BUF pipeline (no GPU display server).
- **Time:** Open-source project, no fixed deadline. Community-driven.
- **Budget:** Zero budget — volunteer development. Must use free/open-source dependencies only.
- **Regulatory:** Tor usage is legal in most jurisdictions but stigmatized. Must not log user activity. Must support pluggable transports for censorship-resistant regions.

## Problems

### [[P-001]] ISP surveillance of media viewing habits
- **Priority:** must-have
- **Description:** When users cast media from the internet, their ISP can see which URLs they fetch, revealing their viewing habits. This is a privacy violation — the ISP has no need to know what media the user watches.
- **Impact:** All users who cast media from internet sources. High impact — viewing habits are sensitive personal data.
- **Success metric:** Zero non-Tor network traffic from the boGDan appliance during media playback. Verified by `tcpdump` showing only Tor connections.

### [[P-002]] DRM and display server overhead on embedded devices
- **Priority:** must-have
- **Description:** Traditional media casting solutions (Chromecast, AirPlay) require a display server, browser engine, and DRM stack. On Raspberry Pi, this consumes significant RAM and CPU, reducing the resources available for media decoding.
- **Impact:** Raspberry Pi users — reduced performance, higher power consumption, potential thermal throttling.
- **Success metric:** boGDan runs with < 200MB RAM usage during 1080p H.264 playback. No display server (X11/Wayland) process running.

### [[P-003]] Lack of hardware-accelerated video decoding pipeline
- **Priority:** must-have
- **Description:** Software video decoding on Raspberry Pi 4 can achieve ~30fps at 1080p, insufficient for smooth playback. The Pi 4's H.264 hardware decoder (V4L2) must be used, but integrating it with a zero-copy DMA-BUF pipeline is non-trivial.
- **Impact:** All users — without hardware decoding, 1080p playback is choppy and the Pi overheats.
- **Success metric:** 1080p H.264 playback at 30fps+ with < 50% CPU usage. Zero-copy pipeline verified by `v4l2-ctl` showing buffer passthrough.

### [[P-004]] Complex protocol landscape for media casting
- **Priority:** should-have
- **Description:** Media casting involves multiple protocols: HTTP REST for control, WebSocket for real-time events, UPnP/DLNA for device discovery. Implementing all three correctly and interoperably is complex.
- **Impact:** Developers — high implementation effort. Users — interoperability issues with existing casting clients.
- **Success metric:** All three protocols (HTTP, WebSocket, UPnP) pass conformance tests. At least 2 third-party casting clients work without modification.

### [[P-005]] Tor circuit management for long-running media sessions
- **Priority:** must-have
- **Description:** Tor circuits are designed for short-lived web browsing. Media streaming sessions can last hours, and Tor circuits can become slow or fail mid-stream. The system must manage circuit rotation, handle stream failures, and maintain acceptable latency.
- **Impact:** All users — stream interruptions, buffering, failed playback.
- **Success metric:** Media stream survives a Tor circuit rotation without > 5s interruption. Automatic circuit replacement within 10s of failure.

### [[P-006]] Content resolution through Tor
- **Priority:** must-have
- **Description:** Users provide a URL or media identifier. The system must resolve it to a direct media stream URL, fetching any redirect chains through Tor. This includes resolving YouTube URLs, direct media links, and RSS feed entries.
- **Impact:** All users — without resolution, the user must manually find the direct stream URL.
- **Success metric:** Resolution of YouTube URLs to direct media stream within 10s through Tor. Support for at least 5 content sources.

### [[P-007]] Browser extension for sending media
- **Priority:** should-have
- **Description:** Users need a way to send media from their browser to the boGDan appliance. A browser extension that detects media on web pages and sends the URL to boGDan is needed. The extension should work with Chromium and Firefox.
- **Impact:** All users — without the extension, users must manually copy/paste URLs.
- **Success metric:** Extension works with Chrome and Firefox. One-click cast from YouTube, Vimeo, and direct media links.

### [[P-008]] Headless appliance setup and configuration
- **Priority:** must-have
- **Description:** Raspberry Pi appliances must be easy to set up — ideally a single command. Configuration (Tor bridges, network, media sources) must be possible without SSH access, via a web UI or config file.
- **Impact:** All users — complex setup is a barrier to adoption.
- **Success metric:** One-command install (`curl | bash`). Web UI for configuration accessible at `http://bogdan.local`. Zero SSH required for normal operation.

### [[P-009]] UPnP/DLNA compatibility with existing devices
- **Priority:** should-have
- **Description:** Many users have existing UPnP/DLNA servers (Plex, Kodi, MiniDLNA). boGDan should act as a DLNA renderer, receiving media from these servers via the DLNA protocol.
- **Impact:** Users with existing media libraries — can use boGDan without changing their setup.
- **Success metric:** boGDan appears as a DLNA renderer on the network. Media from MiniDLNA and Plex plays successfully.

### [[P-010]] Thermal management on Raspberry Pi
- **Priority:** nice-to-have
- **Description:** H.264 hardware decoding generates significant heat. Without thermal management, the Pi 4 can throttle, causing frame drops. The system should monitor temperature and adjust decode quality if needed.
- **Impact:** Users without active cooling — thermal throttling reduces playback quality.
- **Success metric:** CPU temperature stays below 75°C during 1080p playback. If temperature exceeds 80°C, decode quality is automatically reduced.

### [[P-011]] Multi-room audio/video synchronization
- **Priority:** nice-to-have
- **Description:** Users with multiple boGDan appliances (multiple TVs) may want synchronized playback across rooms. This requires clock synchronization and buffer management.
- **Impact:** Power users with multiple appliances — currently unsupported.
- **Success metric:** Two boGDan appliances play the same media with < 100ms audio offset.

### [[P-012]] Accessibility — screen reader support for web UI
- **Priority:** nice-to-have
- **Description:** The web UI for configuration should be accessible to users with visual impairments. Screen reader support, keyboard navigation, and high-contrast mode are needed.
- **Impact:** Users with visual impairments — currently excluded from configuration.
- **Success metric:** Web UI passes WAVE accessibility evaluation. Keyboard-only navigation works for all actions.