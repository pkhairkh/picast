# PiCast — GitHub Copilot Instructions

> This file provides context for GitHub Copilot when suggesting code in this repository.

## Project Overview

PiCast is a **Tor-routed, zero-copy media casting appliance** for Raspberry Pi 4B+.
Written in Rust, it uses GStreamer + V4L2 hardware decode + DRM/KMS direct display.
No X11, no Wayland, no Chromium, no DRM playback.

## Key Constraints

- **Rust only** — edition 2021, MSRV 1.70+
- **Async**: tokio multi-threaded runtime
- **HTTP**: hyper (never actix/rocket/axum)
- **TLS**: rustls (never openssl)
- **Error handling**: thiserror for crate errors, anyhow for app errors
- **Logging**: tracing (never log/env_logger)
- **No `unwrap()`** in production code
- **No `unsafe`** without `// SAFETY:` comment
- **Zero-copy is sacred** — never map DMA-BUFs into userspace

## Architecture

7 crates in a Cargo workspace:

1. `picast-tor` — SOCKS5 proxy management, circuit health
2. `picast-display` — DRM/KMS plane control, atomic modesetting
3. `picast-resolver` — URL classification, yt-dlp subprocess, format selection
4. `picast-playback` — GStreamer pipeline (v4l2h264dec → kmssink)
5. `picast-session` — State machine, queue, ABR, SQLite session store
6. `picast-protocols` — HTTP API (hyper), WebSocket, DLNA/UPnP
7. `picast-server` — Main binary, wires everything together

## Code Patterns

- All async traits use `async_trait` crate
- GStreamer pipelines built with `parse_launch()` or element-by-element construction
- yt-dlp invoked as subprocess with 30s timeout via `tokio::process::Command`
- DRM operations use atomic commits (`drmModeAtomicCommit`) exclusively
- Tor routing via SOCKS5 with `IsolateSOCKSAuth` for per-site circuit isolation

## H.264 Only (v1)

The Pi 4's HEVC decoder outputs SAND format (NC12/NC30) which the HVS cannot display.
Always force H.264 format selection in yt-dlp: `bestvideo[vcodec^=avc1]+bestaudio`
