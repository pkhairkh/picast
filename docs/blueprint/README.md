# Code Reviews

This directory contains code reviews for all major source files in the boGDan
project. Each review covers security, correctness, design, testing, and
documentation quality.

## Review Index

| # | File | Lines | Review |
|---|------|-------|--------|
| 01 | src/tor/src/lib.rs | 1334 | review-01.md |
| 02 | src/protocols/src/ws.rs | 884 | review-02.md |
| 03 | src/protocols/src/http.rs | 1083 | review-03.md |
| 04 | src/session/src/lib.rs | 2928 | review-04.md |
| 05 | src/playback/src/socks_forwarder.rs | 557 | review-05.md |
| 06 | src/display/src/lib.rs | 2077 | review-06.md |
| 07 | src/resolver/src/lib.rs | 1118 | review-07.md |
| 08 | src/resolver/src/ytdlp.rs | 1172 | review-08.md |
| 09 | src/playback/src/lib.rs | 3563 | review-09.md |
| 10 | src/server/src/config.rs | 494 | review-10.md |
| 11 | src/protocols/src/dlna.rs | 599 | review-11.md |
| 12 | src/server/src/main.rs | 683 | review-12.md |
| 13 | src/playback/src/stream_source.rs | 1575 | review-13.md |
| 14 | src/protocols/src/tls.rs + src/session/src/interfaces.rs | - | review-14.md |
| 15 | src/playback/src/pipeline.rs | 2123 | review-15.md |
| 16 | src/playback/src/events.rs + src/protocols/src/lib.rs | - | review-16.md |
| 17 | src/resolver/src/custom.rs | - | review-17.md |
| 18 | src/resolver/src/resolver_socks.rs | 359 | review-18.md |
| 19 | src/resolver/src/cache.rs | 879 | review-19.md |

## Severity Legend

- **High** - Security vulnerability or data-loss risk. Must fix before release.
- **Medium** - Bug or design issue that could cause incorrect behavior. Should fix.
- **Low** - Code quality, style, or minor improvement. Nice to fix.

## Coverage

These reviews cover all 8 workspace crates:

- bogdan-tor (review-01)
- bogdan-protocols (review-02, 03, 11, 14, 16)
- bogdan-session (review-04, 14)
- bogdan-playback (review-05, 09, 13, 15, 16)
- bogdan-display (review-06)
- bogdan-resolver (review-07, 08, 17, 18, 19)
- bogdan-server (review-10, 12)
- bogdan-v3d (no review yet - HEVC deferred to v2)
