---
doc: code_review
project: picast
version: 1
phase: code_review
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T12:40:00Z
---

# Code Review: `src/resolver/src/custom.rs`

**File:** `src/resolver/src/custom.rs`
**Lines:** 3030 (including ~1360 lines of tests — 125 test functions)
**Reviewer:** agent
**Date:** 2026-07-30

## Summary

This file implements custom resolvers for video hosting sites that yt-dlp doesn't support well: Voe (and its front-ends) and DoodStream. The Voe resolver decodes an obfuscated JSON blob through a multi-step pipeline (ROT13 → strip markers → Base64 → char-shift → reverse → Base64 → JSON parse) to extract the direct media URL. The DoodStream resolver follows embed iframes and extracts download tokens. The file is notable for its excellent test coverage (125 tests — the highest in the project), outstanding documentation of the SOCKS5 auth method invariant (why only 0x02 is offered, not 0x00), and thoughtful bait-URL filtering. However, there's a data-integrity bug (`used_tor` is always `false`), a security concern with the SOCKS5 fallback path, and the 8-method extraction pipeline is fragile against CDN changes.

## Scope Reviewed

| Component | Lines | Purpose |
|-----------|-------|---------|
| `build_client()` | 37–100 | reqwest client with cookie jar, SOCKS5 forwarder |
| `is_voe_domain()` / `is_doodstream_domain()` | 467–550 | Domain classification |
| `resolve_voe()` | 552–655 | Voe URL → direct media URL |
| `resolve_doodstream()` | 656–783 | DoodStream URL → direct media URL |
| `try_method8()` / `deobfuscate_embedded_json()` | 784–850 | Obfuscated JSON decode pipeline |
| `try_method7()` / `try_method6()` | 1011–1160 | Alternative extraction methods |
| `is_bait_source()` | 1326–1340 | Bait URL/domain filtering |
| `voe_engine_update()` | 1466–1540 | CDN session activation POST |
| `build_result()` | 1619–1667 | ResolveResult construction |
| Tests | 1668–3030 | 125 test functions |

## Findings

### Bugs

#### BUG-001: `build_result()` always sets `used_tor: false` — breaks CDN IP check logic
- **Severity:** High
- **Location:** Line 1657 (`used_tor: false` in `build_result()`)
- **Description:** The `build_result()` function unconditionally sets `used_tor: false` in the returned `ResolveResult`. However, when `resolve_voe()` or `resolve_doodstream()` is called with a `socks5_proxy` parameter, the resolver DOES route through Tor (via the SOCKS5 forwarder). The session layer's `load()` method uses `resolve_info.used_tor` to decide whether playback should also use Tor (lines 830-870 of `session/src/lib.rs`): if `used_tor` is `false`, playback connects directly without SOCKS. This means a Tor-resolved URL is played back directly, causing a CDN IP mismatch (the URL is bound to the Tor exit IP, but playback connects from the local IP).
- **Impact:** CDN 403 Forbidden on every Tor-resolved custom site URL. The user casts a Voe URL, it resolves through Tor, but playback tries to connect directly — the CDN sees a different IP than the one the URL was bound to, and returns 403.
- **Recommendation:** Pass the `used_tor` flag through from `resolve_voe()` / `resolve_doodstream()`:
  ```rust
  fn build_result(source_url: &str, media_url: &str, title: &Option<String>,
                  thumbnail: &Option<String>, used_tor: bool) -> ResolveResult {
      // ...
      ResolveResult {
          used_tor,
          // ...
      }
  }
  ```
  And in `resolve_voe()`:
  ```rust
  let used_tor = socks5_proxy.is_some();
  let mut result = build_result(&final_url, &media_url, &title, &thumbnail, used_tor);
  ```

#### BUG-002: `file_code` extracted from original URL, not `final_url` after redirect
- **Severity:** Low
- **Location:** Lines 595–603 (`resolve_voe()` — `file_code` extraction uses `url` not `final_url`)
- **Description:** The `file_code` is extracted from the original `url` parameter using `url::Url::parse(url)`. But if `follow_js_redirect()` found a redirect (line 565), the `final_url` may be different. The `file_code` should be extracted from `final_url` to match the page that was actually fetched.
- **Impact:** If the URL was redirected (e.g., from a front-end domain to `voe.sx`), the `file_code` sent to `/engine/update` might be wrong, causing the CDN session activation to fail.
- **Recommendation:** Use `final_url` for `file_code` extraction:
  ```rust
  let file_code = url::Url::parse(&final_url)
      .ok()
      .and_then(|u| {
          u.path_segments().and_then(|mut segments| segments.next_back().map(|s| s.to_string()))
      })
      .filter(|s| !s.is_empty());
  ```

#### BUG-003: `voe_engine_update()` has unused `_page_html` parameter
- **Severity:** Low (informational)
- **Location:** Line 1469 (`_page_html: &str` — prefixed with `_` indicating unused)
- **Description:** The `voe_engine_update()` function takes a `_page_html` parameter that is never used in the function body. The prefix `_` suppresses the dead_code warning. The parameter was likely intended to extract additional telemetry data from the page, but the current implementation sends a static payload.
- **Impact:** Dead code. No functional issue.
- **Recommendation:** Either remove the parameter, or use it to extract dynamic values (e.g., the actual file code from the page, a CSRF token, etc.).

### Security

#### SEC-001: SOCKS5 forwarder fallback breaks circuit isolation invariant
- **Severity:** Medium
- **Location:** Lines 72–80 (`build_client()` — fallback to reqwest's built-in SOCKS5)
- **Description:** If `ResolverSocksForwarder::start()` fails, the code falls back to `reqwest::Proxy::all(proxy_url)` which uses reqwest's built-in SOCKS5 support. The built-in SOCKS5 client offers BOTH no-auth (0x00) and username/password (0x02), allowing Tor to choose no-auth. When Tor chooses no-auth, the isolation username is never sent, and the stream gets assigned to a DIFFERENT circuit than the playback path. This breaks the CDN IP-binding invariant and causes 403 Forbidden.
- **Impact:** When the forwarder fails to start (e.g., port binding issue), the resolver silently falls back to a broken SOCKS5 configuration. The CDN URL is bound to a different Tor exit IP than the one used for playback, causing 403.
- **Recommendation:** Don't fall back — if the forwarder fails, return an error:
  ```rust
  match ResolverSocksForwarder::start(socks_addr, username).await {
      Ok(fwd) => { /* use forwarder */ },
      Err(e) => {
          return Err(ResolveError::Network(format!(
              "failed to start SOCKS5 forwarder (circuit isolation required): {}", e
          )));
      },
  }
  ```
  The current fallback is worse than failing — it produces a URL that looks valid but will fail on playback.

#### SEC-002: `is_bait_source()` doesn't validate URL scheme
- **Severity:** Low
- **Location:** Lines 1326–1340 (`is_bait_source()`)
- **Description:** The function checks if a source URL matches known bait domains or filenames, but doesn't validate that the URL scheme is `http://` or `https://`. A `javascript:`, `data:`, or `file://` URL that doesn't match any bait pattern would pass through and be returned as a valid media URL.
- **Impact:** Low — the URL is passed to `souphttpsrc` or `StreamSource`, which would reject non-HTTP schemes. But the resolver should validate this before returning.
- **Recommendation:** Add a scheme check at the top of `is_bait_source()` or in `build_result()`:
  ```rust
  if let Ok(parsed) = url::Url::parse(source) {
      if !matches!(parsed.scheme(), "http" | "https") {
          return true; // treat non-HTTP as bait
      }
  }
  ```

#### SEC-003: Hardcoded GPU fingerprint in `voe_engine_update()`
- **Severity:** Low
- **Location:** Line 1497 (`"k1l": "ANGLE (NVIDIA, NVIDIA GeForce GTX 1060 6GB ...)"`)
- **Description:** The `/engine/update` POST includes a hardcoded GPU name ("NVIDIA GeForce GTX 1060 6GB") as part of the bot-detection payload. Every boGDan appliance sends the same GPU fingerprint. If Voe detects that many requests come from "GTX 1060" GPUs with different Tor exit IPs, it could flag this as bot activity.
- **Impact:** Low — Voe may not currently check GPU fingerprint consistency. But it's a static fingerprint that could be used to identify boGDan appliances.
- **Recommendation:** For v1, acceptable. For v2, consider rotating GPU names from a pool of common GPUs, or removing the GPU field entirely if it's not required.

### Design Issues

#### DESIGN-001: 8 sequential extraction methods are fragile against CDN changes
- **Severity:** Medium
- **Location:** Lines 610–650 (`resolve_voe()` — tries method8, method7, method6, fallback in sequence)
- **Description:** The Voe resolver tries 4 extraction methods sequentially (Method 8: obfuscated JSON, Method 7: MKGMa-encoded, Method 6: a168c Base64, fallback: var source). Each method targets a specific obfuscation pattern. If Voe changes its obfuscation (which they do periodically to break scrapers), all 4 methods may break simultaneously, and the resolver returns `NoMediaFound` with no diagnostic about which method was closest.
- **Impact:** When Voe changes their page structure, all boGDan casts to Voe URLs break. The user sees "resolution failed" with no indication of what changed.
- **Recommendation:** (a) Log which methods were tried and what they found (even partial matches) at `debug` level, so diagnosing breakages is easier. (b) Consider a plugin architecture where new methods can be added without modifying `resolve_voe()`. (c) Monitor for breakages with a periodic health check that tries resolving a known Voe URL.

#### DESIGN-002: `build_client()` creates a new SOCKS forwarder for every resolve
- **Severity:** Low
- **Location:** Lines 37–100 (`build_client()` — `ResolverSocksForwarder::start()` called per resolve)
- **Description:** Every call to `resolve_voe()` or `resolve_doodstream()` calls `build_client()`, which starts a new local SOCKS forwarder. The forwarder binds to a random port, accepts connections, and is dropped when the resolve completes. For rapid successive resolves (e.g., user casts, stops, casts again), this means starting and stopping forwarders repeatedly.
- **Impact:** Minor latency overhead (forwarder startup ~10-50ms). Not a performance bottleneck for single-session use.
- **Recommendation:** For v1, acceptable. For v2, consider a shared forwarder pool keyed by isolation username, so the same circuit's forwarder is reused across resolves.

#### DESIGN-003: Obfuscation pipeline is Voe-specific with no abstraction
- **Severity:** Low
- **Location:** Lines 800–840 (`deobfuscate_embedded_json()` — ROT13 → strip → Base64 → shift → reverse → Base64)
- **Description:** The 6-step decode pipeline is hardcoded for Voe's current obfuscation. Each step is a separate function call (`rot13`, `replace_patterns`, `safe_b64_decode`, `shift_chars`, `.rev().collect()`, `safe_b64_decode`). If a new site uses a different obfuscation pipeline, a completely new function must be written.
- **Impact:** Low — the current design is readable and the steps are well-named. Adding a new site's pipeline is straightforward (write a new `deobfuscate_*` function).
- **Recommendation:** Acceptable for v1. If more sites with obfuscation are added, consider a trait-based `Deobfuscator` with a `decode(&self, input: &str) -> Option<String>` method.

### Missing Tests

#### TEST-001: No test for the `used_tor` flag propagation (related to BUG-001)
- **Severity:** High
- **Description:** The `build_result()` function always sets `used_tor: false`, but no test verifies that `used_tor` is `true` when a SOCKS proxy is used. This is the bug described in BUG-001, and it would have been caught by a test that calls `resolve_voe()` with a mock SOCKS proxy and asserts `result.used_tor == true`.
- **Impact:** The CDN IP mismatch bug (BUG-001) went undetected because no test checks this flag.
- **Recommendation:** Add a test that verifies `used_tor` is correctly set based on whether `socks5_proxy` was provided.

#### TEST-002: No test for the SOCKS5 fallback path (SEC-001)
- **Severity:** Medium
- **Description:** The fallback path in `build_client()` (lines 72–80) — where reqwest's built-in SOCKS5 is used if the forwarder fails — is not tested. A test that forces the forwarder to fail and verifies the fallback behavior would catch the circuit isolation breakage.
- **Impact:** The security-critical fallback path is untested.
- **Recommendation:** Add a test that mocks `ResolverSocksForwarder::start()` to fail and verifies that `build_client()` either returns an error or logs the appropriate warning.

#### TEST-003: No test for `is_bait_source()` with non-HTTP schemes
- **Severity:** Low
- **Description:** The `is_bait_source()` function is tested with bait domains and filenames, but not with non-HTTP URL schemes (`javascript:`, `data:`, `file://`).
- **Impact:** The scheme validation gap (SEC-002) went undetected.
- **Recommendation:** Add tests:
  ```rust
  assert!(is_bait_source("javascript:alert(1)"));
  assert!(is_bait_source("data:text/html,<script>...</script>"));
  assert!(is_bait_source("file:///etc/passwd"));
  assert!(!is_bait_source("https://cdn.example.com/video.mp4"));
  ```

## Positive Observations

1. **Outstanding test coverage** — 125 test functions covering obfuscation decoding, URL extraction, bait filtering, domain classification, and edge cases. This is the highest test count in the project and sets a standard for other modules.

2. **Excellent SOCKS5 auth invariant documentation** — the comment at lines 20–30 explaining why only username/password auth (0x02) is offered (not no-auth 0x00) is one of the best security-critical comments in the codebase. It explains the failure mode (Tor choosing no-auth → wrong circuit → CDN 403), the root cause (reqwest offering both methods), and the fix (custom forwarder with only 0x02).

3. **Multiple fallback extraction methods** — Methods 6, 7, 8, and fallback URL extraction provide resilience against Voe changing their page structure. If one method breaks, others may still work.

4. **Bait URL filtering** — `is_bait_source()` correctly filters out decoy URLs (BigBuckBunny, known bait domains) that CDNs use to detect scrapers. This prevents the resolver from returning fake media URLs.

5. **Proper `Html` Send handling** — the comment at line 583 explains that `Html` is NOT `Send` (contains `Cell<usize>`) and must be dropped before `.await` points. The code correctly scopes `Html` in a block and extracts only `String` values.

6. **CDN session activation** — `voe_engine_update()` sends a POST to `/engine/update` with an obfuscated payload to activate the CDN session before downloading. The comment explains that without this POST, the CDN may reject the download URL.

7. **HLS detection in `build_result()`** — the function correctly detects `.m3u8` URLs and sets the appropriate `mime_type` and `UrlCategory`, so the playback engine knows to use the HLS client rather than direct MP4 download.

8. **Cookie collection across redirects** — `resolve_voe()` collects cookies from both the initial fetch and any redirect fetch, passing them to the playback engine so CDN requests include the same session cookies.

9. **Rate limit detection** — the resolver detects CDN rate limits (the `sp=` parameter) and logs warnings when the rate limit is below the estimated video bitrate.

10. **Comprehensive obfuscation pipeline tests** — the test suite includes tests for each step of the decode pipeline (ROT13, Base64, char-shift, reverse) and for the complete pipeline with real Voe page samples.

## Recommendations Summary

| Priority | Finding | Effort |
|----------|---------|--------|
| High | BUG-001: Fix `used_tor` flag in `build_result()` | S (30 min) |
| High | TEST-001: Add `used_tor` flag propagation test | S (30 min) |
| Medium | SEC-001: Don't fall back to reqwest SOCKS5 on forwarder failure | S (1 h) |
| Medium | DESIGN-001: Log extraction method diagnostics for breakage analysis | S (1–2 h) |
| Medium | TEST-002: Add SOCKS5 fallback path test | M (2 h) |
| Low | BUG-002: Extract `file_code` from `final_url` not `url` | S (15 min) |
| Low | BUG-003: Remove unused `_page_html` parameter | S (5 min) |
| Low | SEC-002: Validate URL scheme in `is_bait_source()` | S (15 min) |
| Low | SEC-003: Rotate GPU fingerprint or remove if not required | S (30 min) |
| Low | DESIGN-002: Reuse SOCKS forwarder across resolves (v2) | M (2–3 h) |
| Low | DESIGN-003: Trait-based `Deobfuscator` for new sites (v2) | M (2 h) |
| Low | TEST-003: Add non-HTTP scheme tests for `is_bait_source()` | S (15 min) |
