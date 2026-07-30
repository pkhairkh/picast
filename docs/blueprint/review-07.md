---
doc: code_review
project: picast
version: 1
phase: code_review
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# Code Review: `src/resolver/src/lib.rs`

**File:** `src/resolver/src/lib.rs`
**Lines:** 1118
**Reviewer:** agent
**Date:** 2026-07-30

## Summary

The URL resolver takes a user-supplied URL and resolves it to a direct, playable media URL. It classifies URLs (direct media, HLS, DASH, web page, onion), resolves web pages via yt-dlp subprocess or custom resolvers (Voe, DoodStream), routes through Tor, and caches results. The resolver is the gateway between the user's intent (a URL) and the playback engine — its correctness directly affects whether the user can watch their video. The implementation is well-structured with a clear classification-based dispatch, provider registry for extensibility, and persistent caching. However, there are several issues.

## Key Components Reviewed

| Component | Lines | Purpose |
|-----------|-------|---------|
| `ResolveError` enum | 45–60 | Error types (InvalidUrl, NoMediaFound, Network, TorUnavailable) |
| `ResolveResult` struct | 69–130 | Rich result with 18 fields (URL, codecs, dimensions, cookies, etc.) |
| `Resolver` struct | 168–175 | Main resolver with Tor, cache, and provider registry |
| `resolve()` | 254–530 | Category-based dispatch: direct/HLS/DASH/onion/webpage |
| `resolve_direct()` | 534–580 | Direct URL resolution with HTTP HEAD probe |
| `classify()` | 583–625 | URL classification wrapper |
| `ResolverTrait` impl | 630–680 | Adapts `Resolver` to the session layer's trait |

## Findings

### Bugs

#### BUG-001: Cache is checked but never invalidated on failure
- **Severity:** Medium
- **Location:** Lines 265–275 (cache check in `resolve()`)
- **Description:** The `resolve()` method checks the cache at the start and returns a cached result if present. However, if a previously cached URL's media has since been removed or the CDN token has expired, the stale cache entry is returned without verification. The `invalidate_cache()` method exists (called by the session layer on CDN 403), but there's no TTL-based expiry or freshness check.
- **Impact:** A user who casts a URL that was previously resolved successfully but has since expired will get a stale result, leading to a 403 during playback. The session layer's CDN retry will then invalidate the cache and re-resolve, but this adds a 5–15 second delay.
- **Recommendation:** Add a timestamp to cached entries and a TTL check (the `with_cache_ttl` builder exists but the TTL enforcement isn't visible in the read path). Verify the cache entry hasn't exceeded its TTL before returning it.

#### BUG-002: `resolve_direct` does an HTTP HEAD probe that many CDNs reject
- **Severity:** Medium
- **Location:** Lines 534–580 (`resolve_direct`)
- **Description:** The `resolve_direct` method does an HTTP HEAD request to probe the media URL. As noted in the http.rs review and the architecture docs, many CDNs return 404 or 403 for HEAD requests on media URLs. The architecture doc recommends `GET` with `Range: bytes=0-0` instead.
- **Impact:** Direct media URLs on CDNs that reject HEAD will fail resolution unnecessarily, falling back to yt-dlp or erroring out.
- **Recommendation:** Change the HEAD probe to `GET` with `Range: bytes=0-0`, consistent with the CDN preflight logic described in the architecture.

#### BUG-003: Provider registry lookup falls back to Voe for unknown providers
- **Severity:** Low
- **Location:** Lines 390–420 (the `_ =>` arm in provider matching)
- **Description:** When a provider is matched in the registry but its ID is not "doodstream", the code falls back to the Voe resolver (`custom::resolve_voe`). This means any new provider added to the registry will be routed to the Voe resolver, which will likely fail if the provider isn't Voe-compatible.
- **Impact:** Adding a new provider to `providers.d/*.toml` without also adding a matching resolver arm will silently fall back to Voe, producing confusing errors.
- **Recommendation:** Return an error for unknown provider IDs, or log a warning that no specific resolver exists and fall back to yt-dlp instead of Voe.

#### BUG-004: Cache lock held during `cache.insert()` which clones the result
- **Severity:** Low
- **Location:** Lines 380–385, 415–420, and throughout `resolve()`
- **Description:** The cache is locked with `self.cache.lock().await`, then `cache.insert(url, result.clone())` is called. The `result.clone()` happens while holding the lock. Since `ResolveResult` has 18 fields including `Vec<String>` for cookies and subtitle tracks, cloning it can be non-trivial.
- **Impact:** Minimal — the clone is fast enough. But the lock is held longer than necessary.
- **Recommendation:** Clone the result before acquiring the lock, or use `Arc<ResolveResult>` in the cache to avoid cloning entirely.

### Design Issues

#### DESIGN-001: `ResolveResult` has 18 fields — too many for a single struct
- **Severity:** Low
- **Location:** Lines 69–130 (`ResolveResult`)
- **Description:** `ResolveResult` has 18 fields: `source_url`, `direct_url`, `audio_url`, `category`, `mime_type`, `content_length`, `used_tor`, `title`, `duration`, `thumbnail`, `vcodec`, `acodec`, `width`, `height`, `subtitle_tracks`, `cookies`, `resolver_type`, and more. Many of these are `Option` and are `None` for direct media URLs.
- **Impact:** The struct is unwieldy to construct (as seen in the `resolve()` method where each category arm repeats all 18 fields). Adding a new field requires updating every construction site.
- **Recommendation:** Group related fields into sub-structs: `MediaInfo` (title, duration, thumbnail), `StreamInfo` (vcodec, acodec, width, height), `NetworkInfo` (used_tor, cookies, resolver_type). Use `#[derive(Default)]` and struct update syntax to reduce repetition.

#### DESIGN-002: Hardcoded fallback to Voe and DoodStream resolvers
- **Severity:** Medium
- **Location:** Lines 350–430 (the WebPage resolution strategy)
- **Description:** The resolver has hardcoded checks for `is_doodstream_domain()` and `is_voe_domain()`, plus a provider registry. The comment acknowledges this: "The hardcoded checks serve as fallback when no provider configs are loaded." This means the Voe and DoodStream resolvers are effectively special-cased in the core resolver, which contradicts the provider registry's extensibility goal.
- **Impact:** Adding a new provider requires either (a) adding it to the registry AND adding a hardcoded arm, or (b) accepting that it will fall back to Voe (BUG-003). The extensibility is partially illusory.
- **Recommendation:** Move the Voe and DoodStream resolvers into the provider registry as plugin-like entries. The registry should map provider IDs to resolver functions (a `Box<dyn ResolverProvider>` trait), and the core resolver should dispatch through the registry without hardcoded arms.

#### DESIGN-003: `resolve_onion` forces yt-dlp for all `.onion` URLs
- **Severity:** Low
- **Location**: Lines 335–340 (Onion category in `resolve()`)
- **Description:** All `.onion` URLs are routed to `resolve_onion`, which uses yt-dlp. However, some `.onion` URLs might be direct media URLs (e.g., `http://something.onion/video.mp4`). The classifier doesn't check the path extension for `.onion` URLs — it categorizes them all as `Onion`.
- **Impact:** A direct media URL on an `.onion` site will be unnecessarily processed by yt-dlp, adding 3–15 seconds of latency.
- **Recommendation:** In the classifier, check the path extension for `.onion` URLs too. If it's a direct media extension (`.mp4`, `.webm`, etc.), classify as `DirectMedia` with `used_tor = true` rather than `Onion`.

#### DESIGN-004: No concurrent resolution limit
- **Severity:** Low
- **Location:** `resolve()` method
- **Description:** There's no limit on concurrent resolutions. If multiple clients (HTTP, WS, DLNA) cast simultaneously, each spawns a yt-dlp subprocess through Tor. On a Pi 4 with 4 GB RAM, 5+ concurrent yt-dlp processes could exhaust memory.
- **Impact:** Under concurrent load, the Pi could OOM or become unresponsive.
- **Recommendation:** Add a `Semaphore` to limit concurrent resolutions (e.g., 2 or 3). Queue excess requests rather than spawning more subprocesses.

### Security

#### SEC-001: No URL length validation
- **Severity:** Low
- **Location:** Line 256 (`Url::parse(url)`)
- **Description:** The URL is parsed without any length validation. A malicious client could send a multi-megabyte URL string. While `Url::parse` has its own limits, the string is passed to yt-dlp as a command-line argument, which could exceed OS argument length limits (typically 128 KB on Linux).
- **Impact:** A very long URL could cause yt-dlp to fail with a confusing error, or potentially trigger a buffer overflow in yt-dlp's argument parsing (unlikely but defense-in-depth).
- **Recommendation:** Add a URL length check (e.g., max 8192 characters) and reject longer URLs with a clear error.

#### SEC-002: Cookies from resolution are passed to playback without validation
- **Severity:** Low
- **Location:** Lines 125–128 (`cookies` field in `ResolveResult`)
- **Description:** The `cookies` field collects Set-Cookie headers from the resolver's HTTP requests and passes them to the playback engine. These cookies are sent with the media request. If a malicious resolver (or a compromised CDN) sets cookies with special characters, they could potentially be injected into GStreamer's HTTP headers.
- **Impact:** Low — GStreamer's `souphttpsrc` should handle cookie encoding safely. But the cookies are not validated before being passed through.
- **Recommendation:** Validate cookie format (alphanumeric, `=`, `;`, ` ` only) before adding to the `cookies` vector. Reject cookies with newlines, quotes, or other special characters.

### Missing Tests

#### TEST-001: No test for the cache hit path
- **Severity:** Low
- **Description:** The cache check in `resolve()` (lines 265–275) is not tested. There's no test that verifies a cached result is returned without re-resolving.
- **Recommendation:** Add a test that resolves a URL, then resolves it again and verifies the second call returns the cached result (no subprocess spawned).

#### TEST-002: No test for provider registry dispatch
- **Severity:** Low
- **Description:** The provider registry lookup (lines 360–430) is not tested. The routing of different provider IDs to different resolvers is untested.
- **Recommendation:** Add tests with mock providers that verify the correct resolver is called for each provider ID.

#### TEST-003: No test for the fallback chain (provider → Voe → yt-dlp)
- **Severity:** Low
- **Description:** The fallback logic (try provider resolver, then Voe, then yt-dlp) is not tested.
- **Recommendation:** Add tests with mock resolvers that fail, verifying the fallback chain works correctly.

## Positive Observations

1. **Clear URL classification** — the `UrlCategory` enum (DirectMedia, HlsManifest, DashManifest, WebPage, Onion, Magnet) with a `classify_url` function makes the dispatch logic easy to follow.
2. **Provider registry for extensibility** — the `ProviderRegistry` allows adding new providers via TOML config files without code changes (though the hardcoded fallbacks in DESIGN-002 partially undermine this).
3. **Persistent cache** — `with_persistent_cache` allows the cache to survive restarts, avoiding re-resolution of recently-cast URLs.
4. **Rich `ResolveResult`** — the 18-field result carries all the metadata the playback engine needs (codecs, dimensions, cookies, subtitles), enabling informed pipeline construction.
5. **Tor isolation per host** — the resolver correctly uses `TorManager::isolation_username(host)` to ensure per-site circuit isolation (line 353).
6. **`ResolverTrait` adaptation** — the session layer interacts via a trait, allowing mock resolvers for testing.
7. **Cookie forwarding** — cookies from the resolver session are passed to playback, handling CDNs that require session cookies for media access.
8. **`audio_url` for future multi-stream support** — the `audio_url` field is stored even though it's currently unused, anticipating future DASH adaptive streaming support.

## Recommendations Summary

| Priority | Finding | Effort |
|----------|---------|--------|
| Medium | BUG-001: Verify cache TTL on read | S (1 h) |
| Medium | BUG-002: Use GET Range instead of HEAD for direct probe | S (30 min) |
| Medium | DESIGN-002: Move Voe/DoodStream into provider registry | L (4–8 h) |
| Low | BUG-003: Error on unknown provider ID instead of Voe fallback | S (30 min) |
| Low | BUG-004: Clone result before acquiring cache lock | S (15 min) |
| Low | DESIGN-001: Group ResolveResult fields into sub-structs | M (2–3 h) |
| Low | DESIGN-003: Classify .onion direct media correctly | S (1 h) |
| Low | DESIGN-004: Add concurrent resolution limit | S (1 h) |
| Low | SEC-001: Add URL length validation | S (15 min) |
| Low | SEC-002: Validate cookie format | S (30 min) |
| Low | TEST-001: Add cache hit test | S (1 h) |
| Low | TEST-002: Add provider dispatch tests | S (1–2 h) |
| Low | TEST-003: Add fallback chain tests | S (1–2 h) |
