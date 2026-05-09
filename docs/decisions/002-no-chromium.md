# ADR-002: No Chromium/Browser Runtime

| Field        | Value          |
|--------------|----------------|
| **ID**       | ADR-002        |
| **Status**   | ACCEPTED       |
| **Date**     | 2025-01-15     |
| **Supersedes** | —            |
| **Superseded by** | —         |

## Context

Many casting solutions (Chromecast, web-based kiosks, smart TV apps) rely on a browser runtime to render web content and play media. On a Raspberry Pi 4B+, running Chromium is prohibitively expensive:

- **RAM consumption**: Chromium on Pi 4 uses 300–500 MB of RAM with a single tab playing video. On a 2 GB Pi 4, this leaves insufficient headroom for Tor, GStreamer buffers, and the OS.
- **Software decode unreliability**: Chromium's software H.264 decoder on ARM is slow and prone to frame drops at 1080p. V4L2 M2M hardware decode via Chromium is unreliable — it requires specific `--enable-features=V4L2VideoDecoder` flags and the Chromium V4L2 codepath has a history of regressions on Pi 4.
- **No Widevine L1**: Pi 4 lacks Widevine L1 (hardware-verified decryption). Even with Widevine L3 (software CDM), DRM playback on ARM is unreliable (see ADR-007).
- **JavaScript overhead**: A browser runtime includes V8 JIT compiler, renderer process, GPU process, and utility processes — all consuming RAM and CPU that boGDan cannot spare.
- **Security surface**: Chromium's multi-process architecture is designed for security, but each process is an attack vector. On a Tor-routed appliance, minimizing exposed code paths is critical.

boGDan's use case is media casting — playing video URLs on a TV. It does not need to render HTML, execute JavaScript, or provide a web browsing experience.

## Decision

boGDan will not embed or launch Chromium or any browser runtime. Instead, media resolution follows this path:

1. **URL classification**: The `bogdan-resolver` crate classifies URLs as direct media URLs or site URLs requiring extraction.
2. **yt-dlp subprocess**: Site URLs are resolved by the `bogdan-resolver` crate spawning `yt-dlp` as a subprocess with a 30-second timeout, parsing its JSON output for direct media URLs (see ADR-008).
3. **GStreamer pipeline**: Direct media URLs are fed to the `bogdan-playback` crate's GStreamer V4L2 M2M pipeline for hardware-decoded, zero-copy playback (see ADR-003).

This eliminates the browser entirely from the media path.

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ Saves 300–500 MB RAM | No Chromium process, no V8 JIT, no renderer process, no GPU process |
| ✅ Reliable HW decode | GStreamer V4L2 M2M is the proven, stable path for Pi 4 H.264 hardware decoding |
| ✅ Reduced attack surface | No JavaScript engine, no web content rendering, no network-facing browser sandbox |
| ✅ Faster boot | No browser startup time (~5–10 seconds for Chromium cold start on Pi 4) |
| ✅ Predictable resource usage | GStreamer + yt-dlp have well-bounded memory profiles |
| ❌ No DRM playback | Cannot play Netflix, Disney+, or other Widevine-protected content (see ADR-007) |
| ❌ No JavaScript-dependent sites | Sites requiring JS execution to load media (e.g., some embedded players) cannot be resolved by yt-dlp alone |
| ❌ No interactive web content | boGDan cannot display web pages, HTML5 animations, or interactive web apps |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **Chromium kiosk mode** | 300–500 MB RAM is unacceptable; V4L2 decode in Chromium is unreliable; adds enormous attack surface; DRM still doesn't work (no L1); no benefit over GStreamer for pure video playback |
| **deno_core embedded runtime** | Still bundles V8 (~100 MB RAM minimum); JavaScript runtime is unnecessary for media playback; adds complexity for negligible benefit over yt-dlp subprocess |
| **Cog / WPE WebKit** | Lighter than Chromium (~80–120 MB RAM) but still a browser engine; WebKitGTK's V4L2 support on Pi 4 is experimental; same DRM limitations apply; adds Wayland dependency |
