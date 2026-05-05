# ADR-001: No Display Server

| Field        | Value          |
|--------------|----------------|
| **ID**       | ADR-001        |
| **Status**   | ACCEPTED       |
| **Date**     | 2025-01-15     |
| **Supersedes** | —            |
| **Superseded by** | —         |

## Context

PiCast runs on a Raspberry Pi 4B+ with 1–8 GB of RAM. The appliance must dedicate as many resources as possible to media decoding, Tor routing, and network I/O. Traditional Linux desktop environments run a display server (X11 or Wayland compositor) that consumes RAM and CPU cycles even when no GUI applications are visible. On a Pi 4 with 2 GB RAM, every megabyte saved is a megabyte available for GStreamer buffers and Tor circuit management.

A display server introduces:

- **RAM overhead**: X11 + window manager ≈ 50–100 MB; Wayland compositors vary but typically 40–80 MB
- **CPU overhead**: Compositor repaint cycles, damage tracking, and vsync management consume CPU even for a single fullscreen surface
- **Latency**: An extra process in the rendering pipeline adds 1–3 frames of compositor-induced latency
- **Attack surface**: A display server opens local socket interfaces, D-Bus endpoints, and IPC channels that are unnecessary for a single-purpose appliance

PiCast only ever renders one thing: a fullscreen video surface via DRM/KMS atomic modesetting. There is no need for window management, input routing to multiple clients, or surface stacking.

## Decision

PiCast will not run any display server. The `picast-display` crate opens the DRM device directly (`/dev/dri/card0`) and programs the CRTC via atomic modesetting to display a single fullscreen plane backed by a GStreamer `kmssink` output. No X11, no Wayland compositor, and no window manager will be launched.

The rendering pipeline is:

1. GStreamer V4L2 M2M decoder produces DMA-BUFs
2. `kmssink` imports DMA-BUFs into DRM plane 0
3. HVS (Hardware Video Scaler) on BCM2711 scans out directly

This is a zero-compositor, zero-copy path from decoder to display.

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ Saves 50–100 MB RAM | No X11/Wayland process, no compositor, no window manager resident in memory |
| ✅ Zero compositor overhead | No repaint cycles, no damage tracking, no vsync management — CPU is free for decoding and Tor |
| ✅ Reduced attack surface | No local display server sockets, no D-Bus display interfaces, no IPC attack vectors |
| ✅ Minimal display latency | Direct DRM/KMS atomic commit path; no compositor round-trip |
| ✅ Deterministic boot | Systemd starts `picast-display` directly; no dependency on display-manager.service |
| ❌ Cannot run GUI apps alongside | No desktop environment; debugging requires SSH or serial console |
| ❌ No GPU-accelerated GUI toolkit | Qt/GTK apps require a compositor; PiCast UI must be overlay-based (GStreamer textoverlay or custom OSD plane) |
| ❌ Harder local development | Developers must use DRM mock or virtual KMS (vkms) for testing on non-Pi hardware |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **X11 + openbox** | Adds ~50 MB RAM overhead; openbox provides window management that is unnecessary for a single fullscreen surface; X11's software rendering path adds latency and complexity |
| **Weston (Wayland reference compositor)** | Adds ~40–60 MB RAM; Weston's simple-direct backend still introduces a compositor process; PiCast gains nothing from Wayland protocol since there are no clients to coordinate |
| **matchbox-wm** | Lightweight X11 window manager designed for embedded; still requires X11 server (~50 MB overhead); only benefit is fullscreen window management which DRM/KMS provides natively via atomic modesetting |
