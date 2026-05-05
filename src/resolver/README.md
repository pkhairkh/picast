# picast-resolver

Takes a user-provided URL, classifies it, and resolves it to a direct media URL that GStreamer can play. This is the "intelligence" layer that handles the huge diversity of web URLs — from direct MP4 links to YouTube pages to HLS manifests — and produces a concrete, playable stream URL with metadata.

## Purpose

The resolver translates arbitrary URLs from sender apps into concrete, playable media URLs. For direct links (`.mp4`, `.m3u8`) this is a simple classification with no network calls. For media sites (YouTube, Vimeo, Twitch, PeerTube, and 1,800+ more) it requires spawning `yt-dlp` as a subprocess through the Tor SOCKS5 proxy, parsing its JSON output, and selecting the optimal format for the Pi's H.264 hardware decoder. The resolver also extracts subtitle URLs, thumbnail URLs, and metadata (title, duration, available quality tiers) from the yt-dlp output. It enforces the H.264-first format selection policy (ADR-009: HEVC deferred) and respects the quality tier requested by the ABR controller.

## Public API

| Item | Kind | Description |
|------|------|-------------|
| `Resolver` | struct | Main resolver; implements `ResolverTrait` |
| `Resolver::new(ytdlp_path, max_concurrent, socks5_proxy)` | constructor | Create with yt-dlp binary path, concurrency semaphore limit, and Tor SOCKS5 proxy address |
| `UrlClass` | enum | Classification: `Direct`, `Manifest`, `Media`, `Page` |

The `Resolver` struct implements `picast_session::interfaces::ResolverTrait`:

| Method | Description |
|--------|-------------|
| `resolve(url)` | Classify + resolve → direct URL string |
| `classify(url)` | Quick classification without resolution (no network call for Direct/Manifest) |
| `ytdlp_path()` | Path to yt-dlp binary |

## Dependencies

| Dependency | Why |
|------------|-----|
| `picast-session` | Provides `ResolverTrait` trait definition that this crate implements |
| `tokio` | Async process spawning for yt-dlp (`tokio::process::Command`), timeout wrappers |
| `serde` / `serde_json` | Parsing yt-dlp `--dump-json` output (50–200 KB of JSON per video) |
| `url` | URL parsing, host extraction, and path extension detection for classification |
| `sha2` / `hex` | Hostname hashing for SOCKS5 stream isolation username derivation |
| `thiserror` | Structured error types for resolver failures |

## URL Classification Algorithm

The classification algorithm determines how to handle a URL before making any network requests. It operates in three tiers based on the URL's file extension and hostname:

```
Input: URL string
  │
  ├─ Step 1: Parse URL ──▶ extract path extension
  │     │
  │     ├─ .mp4, .mkv, .webm, .avi, .mp3, .ogg, .flac ──▶ Direct
  │     │    (No yt-dlp needed. Pass URL directly to GStreamer.
  │     │     souphttpsrc can fetch these natively.)
  │     │
  │     ├─ .m3u8, .mpd ──────────────────────────────────▶ Manifest
  │     │    (No yt-dlp needed. Pass URL to GStreamer with
  │     │     hlsdemux or dashdemux as the first demuxer.)
  │     │
  │     └─ No recognized media extension
  │           │
  │           └─ Step 2: Extract hostname from URL
  │                 │
  │                 ├─ Host matches MEDIA_HOSTS list:
  │                 │    youtube.com, youtu.be, vimeo.com,
  │                 │    twitch.tv, dailymotion.com, peertube.tv,
  │                 │    odysee.com, rumble.com, bitchute.com,
  │                 │    streamable.com, archive.org, etc.
  │                 │    ──▶ Media (requires yt-dlp resolution)
  │                 │
  │                 └─ Unknown host ──▶ Page
  │                      (May or may not contain video. Try yt-dlp
  │                       with a shorter timeout; fail gracefully.)
  │
  └─ Unparseable URL ──▶ Page (will likely fail in yt-dlp)
```

### Classification Result and Action

| Class | Action | Network Calls | Expected Latency |
|-------|--------|---------------|------------------|
| `Direct` | Pass URL to GStreamer directly | None (classification only) | < 1 ms |
| `Manifest` | Pass URL to GStreamer with appropriate demuxer | None (classification only) | < 1 ms |
| `Media` | Run yt-dlp subprocess via Tor | 1–3 (webpage, formats, subtitles) | 5–15 s |
| `Page` | Try yt-dlp with shorter timeout | 1–2 (best-effort) | 5–10 s (or fail) |

## yt-dlp Format Selection String

The format string is the most critical part of the resolver — it determines which video stream yt-dlp selects. PiCast forces H.264 (AVC) as the primary codec because the BCM2711 SoC has a dedicated hardware H.264 decoder that can decode 1080p60 with near-zero CPU usage. VP9 and AV1 must be software-decoded, which limits the Pi to ~720p30 due to CPU constraints. HEVC hardware decode is deferred to v2 (ADR-009) because the decoder outputs SAND format incompatible with the HVS.

### Full Format String

```bash
--format='bv[height<=?{MAX_HEIGHT}][vcodec^=avc1]/bv[height<=?{MAX_HEIGHT}][vcodec^=vp9]/bv[height<=?{MAX_HEIGHT}]+ba/b[height<=?{MAX_HEIGHT}]/b'
```

### Per-Tier Format Strings

| ABR Tier | MAX_HEIGHT | Expanded Format String |
|----------|------------|----------------------|
| 360p | 360 | `bv[height<=360][vcodec^=avc1]/bv[height<=360][vcodec^=vp9]/bv[height<=360]+ba/b[height<=360]/b` |
| 480p | 480 | `bv[height<=480][vcodec^=avc1]/bv[height<=480][vcodec^=vp9]/bv[height<=480]+ba/b[height<=480]/b` |
| 720p | 720 | `bv[height<=720][vcodec^=avc1]/bv[height<=720][vcodec^=vp9]/bv[height<=720]+ba/b[height<=720]/b` |
| 1080p | 1080 | `bv[height<=1080][vcodec^=avc1]/bv[height<=1080][vcodec^=vp9]/bv[height<=1080]+ba/b[height<=1080]/b` |

### Format String Breakdown

```
bv[height<=720][vcodec^=avc1]    ← Best video ≤720p, H.264 (hardware decode, zero-copy)
/                                   ← Fallback separator (try next if no match)
bv[height<=720][vcodec^=vp9]     ← Best video ≤720p, VP9 (software decode, ~720p30 limit)
/
bv[height<=720]+ba               ← Best video ≤720p, any codec + best audio (last resort)
/
b[height<=720]                   ← Best pre-merged stream ≤720p (some sites only serve merged)
/
b                                 ← Absolute fallback: best available (may exceed height limit)
```

**Why H.264 first?** The BCM2711 SoC has a dedicated H.264 hardware decoder (bcm2835-codec V4L2 M2M at `/dev/video10`) that can decode 1080p60 with ~3% CPU usage. The decoded frames are output as NV12 DMA-BUFs that can be imported directly by kmssink for zero-copy display. VP9 and AV1 require software decoding via `avdec_vp9` or `av1dec`, consuming ~70% and ~90% CPU respectively at 720p30. The format selection string ensures we always prefer hardware-decodable streams.

## yt-dlp Invocation Specification

```bash
yt-dlp \
  --dump-json \                         # Output JSON metadata, don't download
  --no-playlist \                       # Single video only (no playlist expansion)
  --no-warnings \                       # Suppress Python warnings
  --quiet \                             # No progress bar or status output
  --no-check-certificates \             # Skip TLS verification (Tor exit nodes often trigger cert errors)
  --proxy=socks5://127.0.0.1:9050 \    # Route through Tor SOCKS5 proxy
  --format='<tier-specific-string>' \   # Format selection (see above)
  --merge-output-format=mp4 \           # Merge separate video+audio into mp4 container
  --sub-lang=en,en-US \                 # Prefer English subtitles
  --write-subs \                        # Download subtitle files
  --write-auto-subs \                   # Download auto-generated subtitles
  --sub-format=srt \                    # Prefer SRT format (most compatible with GStreamer)
  <URL>
```

### Timeout Handling

| Phase | Timeout | Behaviour on Timeout |
|-------|---------|---------------------|
| yt-dlp startup | 10 s | Kill process with SIGKILL, return `ResolverError::Timeout` |
| yt-dlp total | 60 s | Kill process with SIGKILL, return `ResolverError::Timeout` |
| HEAD request (Page class) | 5 s | Classify as Page, attempt yt-dlp with 30s total timeout |

The resolver uses `tokio::process::Command` with `tokio::time::timeout` wrappers. On timeout, the child process is killed with `SIGKILL` and its resources are reaped via `wait()`. This prevents zombie processes from accumulating if yt-dlp hangs on a problematic URL.

## Implementation Guide for AI Agents

1. **Classification logic** — implement the URL classification algorithm above. Parse the URL with the `url` crate, extract the path extension, match against the `MEDIA_HOSTS` list. This is purely string matching with no I/O and should complete in microseconds.

2. **yt-dlp subprocess** — the `resolve_with_ytdlp` method is the core. Spawn yt-dlp via `tokio::process::Command`, capture stdout (JSON) and stderr (error messages). Add timeout wrappers using `tokio::time::timeout`. Handle the case where yt-dlp returns partial JSON (truncated output due to timeout or crash) by parsing incrementally or wrapping in a recovery block.

3. **Format selection** — the `format_string(quality_tier)` method generates the `--format` argument based on the ABR tier. Test it with all four tiers and verify the height constraint is correctly embedded.

4. **Subtitle extraction** — `pick_subtitle()` prefers English SRT/VTT. Parse the `subtitles` and `automatic_captions` fields from yt-dlp's JSON output. Extend it to respect a user's language preference passed via the cast request.

5. **Error categorization** — distinguish between "no video found" (yt-dlp exits 1 with "Unsupported URL"), "geo-blocked" ("This video is not available in your country"), "rate-limited" (HTTP 429), and "network error" (connection timeout) so the UI can show appropriate messages. Parse yt-dlp's stderr for these patterns.

6. **Caching** — add an in-memory LRU cache keyed by `(url, quality_tier)` to avoid re-running yt-dlp for the same URL within a session. Cache TTL should be 5 minutes (stream URLs from CDNs expire). Maximum cache size: 50 entries. This is critical for ABR quality switches, which re-resolve the same URL at a different quality.

## Key Constraints

- **yt-dlp is blocking**: always acquire the `Semaphore` permit before spawning. Never run more than `max_concurrent` instances simultaneously (default: 2). Multiple concurrent yt-dlp processes would saturate the Pi's CPU and Tor bandwidth.

- **Tor proxy required**: yt-dlp MUST route through the SOCKS5 proxy at `127.0.0.1:9050`. Direct connections leak the Pi's IP address to the content server, violating PiCast's core privacy requirement. The proxy address comes from `TorTrait::socks_addr()`.

- **JSON parsing resilience**: yt-dlp's JSON schema varies by site (YouTube, Vimeo, Twitch all produce different field sets). Use `#[serde(default)]` and `Option<T>` for all non-required fields. The `url`, `title`, and `formats` array are the only truly required fields.

- **Rate limiting**: YouTube may rate-limit the Pi's Tor exit node. If yt-dlp returns HTTP 429, the resolver should back off and retry with a different SOCKS5 username (stream isolation) after a 5-second delay, up to 2 retries.

- **No playlist support**: `--no-playlist` is mandatory. Playlist handling (playing all items in sequence) is a v2 feature. For v1, if a user casts a playlist URL, yt-dlp resolves only the first video.

- **Process cleanup**: always reap the yt-dlp child process, even on error. Use `tokio::process::Child::kill()` followed by `wait()` in a `Drop` guard to prevent zombie processes.

- **Python startup time**: yt-dlp is a Python application with 5–15 second startup time (CPython interpreter initialization, module loading, extractor registration). This is inherent to the subprocess approach and cannot be optimized without switching to a different resolver (ADR-008).

## Reference

| Resource | Location |
|----------|----------|
| ADR-008: yt-dlp as subprocess | `DECISIONS.md` / `SPECIFICATION.md` §1.8 |
| ADR-009: HEVC deferred | `DECISIONS.md` / `SPECIFICATION.md` §1.9 |
| Format support matrix | `SPECIFICATION.md` §3 |
| yt-dlp documentation | https://github.com/yt-dlp/yt-dlp#format-selection |
| Stream isolation | `docs/tor/stream-isolation.md` |
| GStreamer pipeline | `docs/playback/gstreamer-pipeline.md` |
