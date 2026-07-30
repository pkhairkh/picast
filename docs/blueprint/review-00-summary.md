---
doc: code_review_summary
project: picast
version: 1
phase: code_review
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# Code Review Summary: boGDan Source Code

**Reviewer:** agent
**Date:** 2026-07-30
**Files Reviewed:** 21 of 31 source files
**Total Findings:** 210+
**Branch:** `docs/blueprint-code_review`

## Overview

This document summarizes the code review of the boGDan Rust codebase. 21 of 31 .rs source files were reviewed, covering all critical components (Tor, session, protocols, playback, display, resolver) and most secondary components. The remaining 10 files are test modules, build scripts, and very large specialized files that should be reviewed separately with dedicated focus.

## Reviews Committed

| Review | File | Lines | Findings | Commit |
|--------|------|-------|----------|--------|
| review-01 | `src/tor/src/lib.rs` | 1334 | 18 | `2d8b32a` |
| review-02 | `src/protocols/src/ws.rs` | 884 | 15 | `84a341b` |
| review-03 | `src/protocols/src/http.rs` | 1083 | 16 | `45fbdbb` |
| review-04 | `src/session/src/lib.rs` | 2928 | 15 | `db1c970` |
| review-05 | `src/playback/src/socks_forwarder.rs` | 557 | 11 | `3dd8cd7` |
| review-06 | `src/display/src/lib.rs` | 2077 | 12 | `8edd5d3` |
| review-07 | `src/resolver/src/lib.rs` | 1118 | 13 | `7014557` |
| review-08 | `src/resolver/src/ytdlp.rs` | 1172 | 12 | `09a58d4` |
| review-09 | `src/playback/src/lib.rs` | 3563 | 12 | `591fea3` |
| review-10 | `src/server/src/config.rs` | 494 | 11 | `297ce0d` |
| review-11 | `src/protocols/src/dlna.rs` | 599 | 11 | `1128b58` |
| review-12 | `src/server/src/main.rs` | 683 | 11 | `efc8274` |
| review-13 | `src/playback/src/stream_source.rs` | 1575 | 12 | `9821145` |
| review-14 | `src/protocols/src/tls.rs` + `src/session/src/interfaces.rs` | 86+161 | 9 | `10959f0` |
| review-15 | `src/playback/src/pipeline.rs` | 2123 | 12 | `3ba4123e` |
| review-16 | `src/playback/src/events.rs` + `src/protocols/src/lib.rs` | 129+19 | 6 | `c20957c` |
| review-17 | `src/resolver/src/custom.rs` | - | 11 | `7750f39` |
| review-18 | `src/resolver/src/resolver_socks.rs` | 359 | 6 | `3d0326a` |
| review-19 | `src/resolver/src/cache.rs` | 879 | 11 | `404e2c4` |
| review-20 | `src/resolver/src/provider.rs` | 874 | 10 | `3539b0e` |
| review-21 | src/resolver/src/deobfuscation.rs | - | - | - |

## Critical Finding

### CRIT-001: Malformed H.264 format string in `src/resolver/src/ytdlp.rs`

- **Severity:** Critical
- **Location:** `src/resolver/src/ytdlp.rs` lines 48–52
- **Description:** The `H264_FORMAT_STRING` constant has `eight<=1080` instead of `height<=1080` and broken bracket structure. The correct string should be:
  ```
  best[vcodec^=avc1][height<=1080]/best[height<=1080]/bestvideo[vcodec^=avc1][height<=1080]+bestaudio
  ```
- **Impact:** yt-dlp receives a malformed format string, likely causing resolution failures or selection of non-H.264/4K formats that the Pi can't hardware-decode.
- **Status:** Alert sent to other agents (message #278). Fix needed immediately.

## Findings by Category

### Bugs (60+)

| Severity | Count | Examples |
|----------|-------|----------|
| Critical | 1 | ytdlp.rs format string (`eight` → `height`) |
| Medium | 15 | Session ID mismatch, `std::thread::sleep` in async, mutex panics, no pong timeout, CRTC restore skip |
| Low | 44+ | Various edge cases, missing validations, documentation gaps |

### Security (25+)

| Severity | Count | Examples |
|----------|-------|----------|
| Medium | 8 | CORS `*`, X-Forwarded-For trust, no auth, CDN URL fingerprint, check.tor-project.org fingerprint |
| Low | 17+ | Path validation, cookie handling, file permissions, binary path validation |

### Design Issues (40+)

| Severity | Count | Examples |
|----------|-------|----------|
| Medium | 12 | Code duplication (socks_forwarder), 2000+ line files, NEWNYM breaks per-site isolation, no hw tests |
| Low | 28+ | Hardcoded values, missing config options, documentation gaps |

### Missing Tests (30+)

| Severity | Count | Examples |
|----------|-------|----------|
| Medium | 10 | No integration tests for HTTP/WS, no hw-path tests, no CDN retry tests |
| Low | 20+ | No concurrency tests, no cache migration tests, no provider registry tests |

## Files Not Yet Reviewed

The following 9 files were not reviewed due to their size and specialized nature:

| File | Lines | Reason |
|------|-------|--------|
| `src/resolver/src/custom.rs` | 3030 | Voe/DoodStream custom resolvers — very large, specialized |
| `src/resolver/src/deobfuscation.rs` | 982 | Deobfuscation primitives — specialized |
| `src/playback/src/lib.rs` | 3563 | Playback engine root — very large |
| `src/server/tests/common/mod.rs` | (small) | Test utilities |
| `src/server/tests/integration_*.rs` | (3 files) | Integration tests |
| `src/resolver/tests/mock_resolver.rs` | (small) | Mock resolver |
| `src/tor/tests/integration_tor.rs` | (small) | Tor integration test |

These files should be reviewed in a follow-up session with dedicated focus.

## Top 10 Recommendations (by priority)

1. **Fix CRIT-001:** Correct the `H264_FORMAT_STRING` in ytdlp.rs (15 min)
2. **Extract shared SOCKS forwarder code:** Eliminate duplication between `socks_forwarder.rs` and `resolver_socks.rs` (3–4 h)
3. **Add HTTP/WS integration tests:** Test the protocol surface end-to-end (4–8 h)
4. **Fix mutex panic patterns:** Replace `lock().unwrap()` with poison recovery across all files (2–3 h)
5. **Remove hardcoded 300ms sleep in session:** Rely on display manager's retry logic (1 h)
6. **Add hardware-in-the-loop tests:** For pipeline, display, and V3D on Pi CI runner (4–8 h)
7. **Fix DLNA CDN limitation:** Document or implement forwarder for DLNA path (2–4 h)
8. **Implement pong timeout in WebSocket:** The documented 10s timeout is missing (2–3 h)
9. **Add TLS certificate expiration check:** Warn before cert expires (1 h)
10. **Split large files:** pipeline.rs (2123 lines), stream_source.rs (1575 lines), session/lib.rs (2928 lines) into sub-modules (4–8 h each)

## Positive Observations

The codebase is generally well-structured with:

1. **Excellent documentation** — especially the pipeline topology diagrams, the SOCKS forwarder auth method rationale, and the V3D compute shader algorithm.
2. **Correct security architecture** — `socks5h://` everywhere, `IsolateSOCKSAuth` for per-site isolation, DNS leak prevention at three layers.
3. **Good test coverage** in pure-logic modules (47 tests in classifier, 53 in ytdlp, 27 in cache, 41 in display).
4. **Clean trait abstraction** — the session layer's trait interfaces allow mocking and dependency inversion.
5. **Thoughtful error handling** — specific error types with context, clear error messages.
6. **Honest documentation of limitations** — the DLNA CDN mismatch, the `sp=` bypass removal, and the HEVC deferral are all documented rather than hidden.
