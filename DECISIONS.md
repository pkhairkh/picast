# boGDan — Architecture Decision Records

> This file is the index for all ADRs. Full decision records with context,
> consequences, and alternatives are in `docs/decisions/`.

---

## ADR Index

| ADR | Title | Status | File |
|-----|-------|--------|------|
| ADR-001 | No Display Server | ACCEPTED | [docs/decisions/001-no-display-server.md](docs/decisions/001-no-display-server.md) |
| ADR-002 | No Chromium / No Browser Runtime | ACCEPTED | [docs/decisions/002-no-chromium.md](docs/decisions/002-no-chromium.md) |
| ADR-003 | GStreamer Over mpv | ACCEPTED | [docs/decisions/003-gstreamer-over-mpv.md](docs/decisions/003-gstreamer-over-mpv.md) |
| ADR-004 | C Tor Daemon Over arti | ACCEPTED | [docs/decisions/004-c-tor-over-arti.md](docs/decisions/004-c-tor-over-arti.md) |
| ADR-005 | Cast V2 Protocol Rejected | REJECTED | [docs/decisions/005-cast-v2-rejected.md](docs/decisions/005-cast-v2-rejected.md) |
| ADR-006 | UPnP/DLNA MediaRenderer | ACCEPTED | [docs/decisions/006-dlna-mediarenderer.md](docs/decisions/006-dlna-mediarenderer.md) |
| ADR-007 | DRM Out of Scope | ACCEPTED | [docs/decisions/007-no-drm-v1.md](docs/decisions/007-no-drm-v1.md) |
| ADR-008 | yt-dlp as Subprocess | ACCEPTED | [docs/decisions/008-ytdlp-subprocess.md](docs/decisions/008-ytdlp-subprocess.md) |
| ADR-009 | HEVC Deferred to v2 | DEFERRED | [docs/decisions/009-hevc-deferred.md](docs/decisions/009-hevc-deferred.md) |

---

## Key Decisions Summary

- **No display server** (ADR-001): boGDan drives DRM/KMS directly via `drmSetMaster()` + `drmModeAtomicCommit()`. No X11, Wayland, or compositor.
- **No browser** (ADR-002): Content resolution via yt-dlp (subprocess) and custom resolvers (reqwest). No Chromium, no Widevine, no DRM content.
- **GStreamer over mpv** (ADR-003): GStreamer's pipeline architecture provides zero-copy DMA-BUF → kmssink, `queue2` buffering, and dynamic codec detection via `parsebin`.
- **C Tor daemon** (ADR-004): Required for `IsolateSOCKSAuth` per-site circuit isolation, which arti doesn't support.
- **No Cast V2** (ADR-005): Google enforces device authentication; unofficial receivers cannot complete the handshake.
- **DLNA via gmediarender** (ADR-006): Mature, lightweight DLNA renderer. Session sync via D-Bus/GStreamer bus monitoring.
- **No DRM** (ADR-007): Widevine on ARM is slow and unreliable. Open video platforms are the target use case.
- **yt-dlp as subprocess** (ADR-008): Process isolation ensures yt-dlp crashes don't affect boGDan. Custom resolvers (Voe, DoodStream) use reqwest directly via Tor.
- **HEVC deferred** (ADR-009): HEVC decoder outputs SAND format incompatible with HVS. v4l2convert (ISP) conversion works but breaks zero-copy. H.264 forced via yt-dlp format selection.

---

## Creating New ADRs

1. Copy `docs/decisions/TEMPLATE.md` to `docs/decisions/NNN-<slug>.md` (next number).
2. Fill in all sections: Context, Decision, Consequences, Alternatives Rejected.
3. Add an entry to the index table above.
4. Update `docs/decisions/TEMPLATE.md` last-used number comment if needed.
