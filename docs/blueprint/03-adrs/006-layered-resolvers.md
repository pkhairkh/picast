---
doc: adr
project: picast
version: 1
phase: adrs
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
adr: BP-ADR-006
problem: "[[P-006]]"
title: "Layered resolvers — in-tree fast paths plus yt-dlp long-tail"
---
# BP-ADR-006: Layered resolvers — in-tree fast paths plus yt-dlp long-tail

| Field        | Value          |
|--------------|----------------|
| **ID**       | BP-ADR-006        |
| **Status**   | PROPOSED       |
| **Date**     | 2026-07-30     |


| **Related** | ADR-008 (yt-dlp as subprocess) |

## Context

Problem [[P-006]] requires resolution of YouTube URLs to direct media streams within 10 s through Tor and support for at least 5 content sources. ADR-008 chose yt-dlp as a subprocess for its 1,800+ site coverage. However, yt-dlp's general-purpose extractor adds 5–15 s of overhead per cast on the Pi — Python startup plus extractor logic — which exceeds the 10 s budget for the highest-volume sites. The resolver layer needs a fast path for common sites and the long-tail fallback for everything else.

## Decision

boGDan layers two resolvers. First, custom in-tree Rust resolvers for the highest-volume sites (YouTube, Vimeo, direct media links) — these are tight, ~50–200 line implementations using `reqwest` over Tor SOCKS5h that resolve in under 2 s. Second, yt-dlp as the long-tail fallback for all other sites, invoked as a subprocess per ADR-008 with `--proxy socks5h://127.0.0.1:29050` and `--username <hosthash>` for circuit isolation. All DNS goes through Tor's SOCKS5h (the trailing `h` forces remote resolution, preventing local DNS leaks). The resolver registry tries the in-tree path first; on failure, falls back to yt-dlp; on yt-dlp failure, returns a structured error to the session.

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ YouTube resolved within 10 s through Tor | Custom in-tree resolver avoids Python startup; meets P-006 success metric for the highest-volume source |
| ✅ 1,800+ sites supported via fallback | yt-dlp covers the long tail; boGDan doesn't have to reimplement every extractor |
| ✅ DNS leak prevention | All resolution uses `socks5h://` so the appliance never asks a local resolver |
| ✅ Independent yt-dlp updates | yt-dlp can be updated independently of boGDan releases — critical because site extractors break frequently |
| ❌ Maintenance cost for custom resolvers | Each custom resolver must track its site's API changes; YouTube in particular has anti-bot measures that yt-dlp has spent years circumventing — boGDan's custom resolver will break |
| ❌ Two code paths to test | In-tree and yt-dlp paths must both be exercised by CI; an in-tree resolver bug could shadow a working yt-dlp fallback |
| ❌ yt-dlp Python dependency in image | Python runtime + yt-dlp pip package (~40 MB total) in the OS image, per ADR-008 |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **Pure yt-dlp (no custom resolvers)** | Rejected because yt-dlp's general-purpose extractor adds 5–15 s overhead per cast on Pi 4 due to Python startup, exceeding the 10 s budget for the highest-volume sites |
| **Embed yt-dlp as Python library via PyO3** | Rejected in ADR-008: ~50 MB permanent RAM overhead, GIL limits concurrency, no process isolation, harder to update independently |
| **Custom Rust extractors only (no yt-dlp)** | Rejected because reimplementing 1,800+ site extractors is a full-time project in itself; YouTube alone has anti-bot measures that yt-dlp has spent years circumventing |
