---
doc: adr
project: picast
version: 1
phase: adrs
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
adr: BP-ADR-002
problem: "[[P-002]]"
title: "DRM/KMS direct scanout, no display server"
---
# BP-ADR-002: DRM/KMS direct scanout, no display server

| Field        | Value          |
|--------------|----------------|
| **ID**       | BP-ADR-002        |
| **Status**   | PROPOSED       |
| **Date**     | 2026-07-30     |


| **Related** | ADR-001 (No Display Server), ADR-002 (No Chromium / No Browser Runtime) |

## Context

Problem [[P-002]] requires boGDan to run with < 200 MB RAM during 1080p playback and to have no X11 or Wayland process running. The ratified ADR-001 already commits to driving DRM/KMS directly; this blueprint ADR elaborates the *scanout* path that delivers the < 200 MB target, in particular the zero-copy DMA-BUF handoff from `v4l2h264dec` to `kmssink` that lets the Hardware Video Scaler (HVS) on BCM2711 scan out decoded frames without a compositor round-trip.

## Decision

boGDan drives HDMI through DRM/KMS atomic modesetting on BCM2711 plane 0, with no X11, no Wayland, no Chromium, and no Widevine CDM. Decoded frames are DMA-BUF file descriptors imported directly by `kmssink`, so the CPU never touches pixel data. The `bogdan-display` crate opens `/dev/dri/card0`, acquires DRM master, and commits an atomic request that binds the GStreamer kmssink output to plane 0 of the active CRTC. Total resident memory target: < 200 MB during 1080p60 playback.

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ < 200 MB RAM during 1080p playback | No display server, no compositor, no browser engine — matches P-002 success metric |
| ✅ Zero-copy to display | DMA-BUF handoff removes an entire frame copy per refresh; CPU stays below 50% even at 1080p60 |
| ✅ Lower power / thermal load | Fewer resident processes and no compositor repaint cycles reduce heat output, supporting BP-ADR-010 |
| ✅ Reduced attack surface | No local display server sockets, no D-Bus display interfaces, no IPC attack vectors — consistent with ADR-001 |
| ❌ DRM master contention | Another process holding DRM master (notably `gmediarender` for DLNA — see BP-ADR-009) can block pipeline construction on restart; this is already listed as a known issue in the README |
| ❌ No GUI toolkit alongside | No Qt/GTK; any on-screen display must be an overlay plane or GStreamer textoverlay rather than a window |
| ❌ Harder local development | Developers on x86_64 must use vkms (virtual KMS) or a DRM mock; CI must skip the real DRM path |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **Minimal Wayland compositor (sway or cage)** | Even a minimal compositor adds ~50 MB RAM, an extra frame copy, and the wlroots/libwayland attack surface; gains nothing for a single fullscreen video surface |
| **Full X11 stack** | Strictly worse than Wayland on every axis that matters for an appliance; larger memory footprint, more attack surface, no benefit |
| **Broadcom dispersion / MMAL legacy path** | Legacy vendor-specific API; deprecated on BCM2711; no zero-copy DMA-BUF export; would lock the project to an unmaintained stack |
