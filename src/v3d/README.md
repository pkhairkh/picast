# bogdan-v3d

V3D GPU compute shader engine for SAND128 to NV12 near-zero-copy conversion on Raspberry Pi. This crate converts HEVC decoder output from Broadcom SAND128 column-tiled format into linear NV12 that the HVS can scan out directly, using OpenGL ES 3.1 compute shaders on the V3D GPU. The CPU never touches the pixel data.

## Purpose

The Broadcom HEVC decoder (rpivid) outputs pixels in SAND128 column format, which is efficient for the decoder SDRAM access patterns but incompatible with the Hardware Video Scaler (HVS) that handles HDMI scanout. This crate bridges that gap by running a GPU compute shader that reads SAND128 data from a DMA-BUF, converts it to linear NV12, and writes the result into a second DMA-BUF for HVS scanout.

## Architecture

```
HEVC decoder -> DMA-BUF (SAND128) -> V3D compute shader -> DMA-BUF (NV12) -> HVS scanout
```

The conversion is near-zero-copy: data moves through the GPU but the CPU is never involved in the pixel path. DMA-BUF file descriptors are passed between subsystems without mapping into userspace.

## Features

- `hw` — Real V3D GPU path (requires Raspberry Pi with V3D GPU, libEGL, libGLESv2)
- Default (no `hw`) — Mock/stub path for x86_64 development and CI

## Dependencies

When built with `hw`:
- `glow` — Safe OpenGL ES 3.1 bindings for compute shader dispatch
- `nix` — Linux APIs for memfd_create, ioctl, mmap
- `libloading` — Dynamic loading of libEGL.so and libGLESv2.so

## Status

HEVC support is deferred to v2 (see ADR-009). For v1, H.264 is preferred via yt-dlp format selection (`bestvideo[vcodec^=avc1]`). The v3d crate is built but not exercised in the default v1 playback path.

## References

- [ARCHITECTURE.md](../../ARCHITECTURE.md) — Full system architecture
- [docs/hardware/bcm2711.md](../../docs/hardware/bcm2711.md) — BCM2711 SoC details
- [docs/decisions/009-hevc-deferred.md](../../docs/decisions/009-hevc-deferred.md) — HEVC deferral rationale
