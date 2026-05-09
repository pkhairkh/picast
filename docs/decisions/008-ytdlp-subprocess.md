# ADR-008: yt-dlp as Subprocess

| Field        | Value          |
|--------------|----------------|
| **ID**       | ADR-008        |
| **Status**   | ACCEPTED       |
| **Date**     | 2025-01-21     |
| **Supersedes** | —            |
| **Superseded by** | —         |

## Context

boGDan receives media URLs from users via the HTTP API, browser extension, or DLNA. Many of these URLs are not direct media links — they point to web pages that embed video players (YouTube, Vimeo, Twitch, etc.). boGDan needs to resolve these page URLs into direct media stream URLs that GStreamer can play.

[yt-dlp](https://github.com/yt-dlp/yt-dlp) is the de facto standard tool for extracting direct media URLs from web pages. It supports 1800+ sites, handles format selection, and outputs structured JSON with media URLs, format information, and metadata.

boGDan must decide how to integrate yt-dlp:

### Option A: Subprocess

Run `yt-dlp` as a child process via Rust's `std::process::Command`. Parse its JSON output (`--dump-json`) to extract media URLs.

### Option B: Embedded Python Library

Embed a Python runtime (via `pyo3` crate) and call yt-dlp's Python API directly.

### Option C: Custom Extractors

Write site-specific URL resolvers in Rust for the most popular sites (YouTube, Vimeo, Twitch) without depending on yt-dlp.

## Decision

boGDan runs yt-dlp as a subprocess. The `bogdan-resolver` crate spawns `yt-dlp` with the following configuration:

```bash
yt-dlp \
  --dump-json \
  --no-download \
  --no-warnings \
  --socket-timeout 30 \
  --proxy socks5://127.0.0.1:9050 \
  --format "bv[height<=1080][vcodec^=avc1]+ba/b[height<=1080]/bv+ba" \
  --username <hosthash> \   # For Tor IsolateSOCKSAuth
  <URL>
```

Key parameters:

- **`--dump-json`**: Outputs a JSON object with `url`, `format_id`, `height`, `vcodec`, and other fields. boGDan parses this to get the direct media URL.
- **`--no-download`**: Only extract metadata, don't download.
- **`--socket-timeout 30`**: 30-second timeout for network operations. If yt-dlp hangs (e.g., Tor circuit is slow), boGDan kills the subprocess after 30 seconds.
- **`--proxy socks5://127.0.0.1:9050`**: Route yt-dlp's HTTP requests through Tor SOCKS5.
- **`--username <hosthash>`**: SOCKS5 username for Tor's `IsolateSOCKSAuth` circuit isolation (see ADR-004).
- **`--format "bv[height<=1080][vcodec^=avc1]+ba/b[height<=1080]/bv+ba"`**: Prefer H.264 video ≤1080p with best audio (see ADR-009 for HEVC rationale).

The `bogdan-resolver` crate:

1. Spawns `yt-dlp` with `std::process::Command`
2. Captures stdout and parses JSON with `serde_json`
3. Extracts the `url` field for GStreamer
4. Applies a 30-second process timeout via `child.kill()` in a `tokio::time::timeout` wrapper
5. Returns `ResolvedMedia { url, format, duration, title }` or a structured error

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ Process isolation | If yt-dlp crashes (Python exception, OOM, segfault), it doesn't affect the boGDan process; the resolver just returns an error and boGDan continues running |
| ✅ Independent updates | yt-dlp can be updated independently via `pip install --upgrade yt-dlp` without rebuilding boGDan; site extractors change frequently and independent updates are critical |
| ✅ Full yt-dlp feature set | All 1800+ site extractors are available; no need to reimplement anything |
| ✅ Structured JSON output | `--dump-json` provides reliable, parseable output; `serde_json` deserialization is straightforward |
| ✅ Tor SOCKS5 routing | yt-dlp natively supports `--proxy socks5://` for Tor routing |
| ❌ Python startup overhead | yt-dlp is a Python script; cold start takes 5–15 seconds on Pi 4 due to Python interpreter + yt-dlp module loading. Mitigated by: (a) boGDan keeps a "warm" resolver by pre-fetching URL info, (b) the HTTP API shows a "resolving" status to the user during this time |
| ❌ Subprocess management | Must handle process spawning, timeout enforcement, stdout capture, and error parsing; adds ~200 lines of Rust code to `bogdan-resolver` |
| ❌ Python dependency | yt-dlp requires Python 3.7+ on the Pi; boGDan OS image must include Python (~30 MB) and yt-dlp pip package (~10 MB) |
| ❌ No streaming during resolve | User must wait 5–15 seconds for yt-dlp to resolve before playback starts; cannot begin streaming until the direct URL is known |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **Embedded Python via pyo3** | Embedding a Python interpreter adds ~50 MB RAM overhead permanently; pyo3 FFI bridge is complex; Python GIL limits concurrency; yt-dlp crashes inside the embedded interpreter would crash boGDan (no process isolation); harder to update yt-dlp independently |
| **Custom Rust extractors** | Would need to reimplement YouTube, Vimeo, Twitch, and 1800+ other site extractors; each site changes its page structure frequently, requiring constant maintenance; YouTube alone has anti-bot measures that yt-dlp has spent years circumventing; this is a full-time project in itself |
