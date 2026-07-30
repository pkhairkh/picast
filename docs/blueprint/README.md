# Code Reviews

This directory contains code reviews for all major source files in the boGDan
project. Each review covers security, correctness, design, testing, and
documentation quality.

## Summary

- [review-00-summary.md](review-00-summary.md) - Aggregates all reviews with priority findings.

## Review Index

| # | File | Review |
|---|------|--------|
| 01 | src/tor/src/lib.rs | [review-01](review-01.md) |
| 02 | src/protocols/src/ws.rs | [review-02](review-02.md) |
| 03 | src/protocols/src/http.rs | [review-03](review-03.md) |
| 04 | src/session/src/lib.rs | [review-04](review-04.md) |
| 05 | src/playback/src/socks_forwarder.rs | [review-05](review-05.md) |
| 06 | src/display/src/lib.rs | [review-06](review-06.md) |
| 07 | src/resolver/src/lib.rs | [review-07](review-07.md) |
| 08 | src/resolver/src/ytdlp.rs | [review-08](review-08.md) |
| 09 | src/playback/src/lib.rs | [review-09](review-09.md) |
| 10 | src/server/src/config.rs | [review-10](review-10.md) |
| 11 | src/protocols/src/dlna.rs | [review-11](review-11.md) |
| 12 | src/server/src/main.rs | [review-12](review-12.md) |
| 13 | src/playback/src/stream_source.rs | [review-13](review-13.md) |
| 14 | src/protocols/src/tls.rs + src/session/src/interfaces.rs | [review-14](review-14.md) |
| 15 | src/playback/src/pipeline.rs | [review-15](review-15.md) |
| 16 | src/playback/src/events.rs + src/protocols/src/lib.rs | [review-16](review-16.md) |
| 17 | src/resolver/src/custom.rs | [review-17](review-17.md) |
| 18 | src/resolver/src/resolver_socks.rs | [review-18](review-18.md) |
| 19 | src/resolver/src/cache.rs | [review-19](review-19.md) |
| 20 | src/resolver/src/provider.rs | [review-20](review-20.md) |
| 21 | src/resolver/src/deobfuscation.rs | [review-21](review-21.md) |
| 22 | src/v3d/src/lib.rs | [review-22](review-22.md) |

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
- bogdan-resolver (review-07, 08, 17, 18, 19, 20)
- bogdan-server (review-10, 12)
- bogdan-v3d (review-22)
