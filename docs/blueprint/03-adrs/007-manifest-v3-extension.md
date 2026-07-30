# BP-ADR-007: Single Manifest V3 codebase for Chrome and Firefox

| Field        | Value          |
|--------------|----------------|
| **ID**       | BP-ADR-007        |
| **Status**   | PROPOSED       |
| **Date**     | 2026-07-30     |




## Context

Problem [[P-007]] requires a browser extension that works with both Chrome and Firefox, supports one-click cast from YouTube, Vimeo, and direct media links, and lets users send media to the boGDan appliance without copy/pasting URLs. Manifest V2 is being sunset by both Google and Mozilla, so a new extension must target Manifest V3. Maintaining two codebases (one per browser) doubles the maintenance cost for no functional benefit.

## Decision

Ship one Manifest V3 codebase in `src/extension/` that builds for both Chrome and Firefox. The extension detects media URLs on the active tab via `chrome.tabs` / `browser.tabs` (polyfilled) and DOM scraping for `<video>` and `<source>` elements, then POSTs to `http://<pi-ip>:8585/api/cast`. Playback status is surfaced over WebSocket on `:8586`. A build-time browser polyfill (`webextension-polyfill`) normalises the `chrome.*` vs `browser.*` namespace gap. The extension is stateless — all cast state lives on the Pi — so a service-worker eviction mid-cast just reconnects the WebSocket and re-syncs from `/api/status`.

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ One-click cast on Chrome and Firefox | Single codebase, two build targets; matches P-007 success metric |
| ✅ Stateless extension survives service-worker eviction | MV3 service workers can be evicted mid-cast; reconnecting the WebSocket and re-syncing from `/api/status` is cheap and lossless |
| ✅ No MV2 sunset risk | MV2 is being sunset by both Google and Mozilla; targeting MV3 from day one avoids a forced migration later |
| ✅ Single CI pipeline | One test suite, one lint config, one release artefact per browser |
| ❌ MV3 service-worker lifecycle complexity | Background workers are ephemeral; WebSocket reconnect logic is mandatory and easy to get wrong |
| ❌ Cross-origin restrictions | MV3 cross-origin fetch is more restricted than MV2; the extension may need to relay certain requests through the boGDan HTTP API rather than fetching directly |
| ❌ Firefox MV3 quirks | Firefox MV3 has minor divergences from Chrome (event page vs service worker, native messaging differences); the polyfill smooths most but not all of these |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **Manifest V2** | Rejected because both Google and Mozilla are sunsetting MV2; shipping MV2 today guarantees a forced migration within the project's lifetime |
| **Two separate codebases (Chrome MV3 + Firefox MV2)** | Rejected as a maintenance burden with no upside; double the bug surface, double the release process, and the Firefox MV2 path is on borrowed time |
| **PWA-installed sender page (no extension)** | Rejected because a PWA cannot intercept the active tab's media without manual copy/paste, breaking the one-click cast goal of P-007 |
