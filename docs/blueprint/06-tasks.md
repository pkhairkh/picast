---
doc: tasks
project: picast
version: 1
phase: tasks
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# boGDan Blueprint — Task Breakdown

This document decomposes the boGDan v1 implementation into actionable tasks. Each task has a stable ID ([[T-NNN]]), an owner role, a rough estimate in ideal days, a list of dependencies (other tasks that must complete first), and traces to one or more requirements ([[R-NNN]]) from `docs/blueprint/05-spec.md` and components ([[C-NNN]]) from `docs/blueprint/04-fine-draft.md`. Tasks are grouped into six milestones; milestones are sequenced so that each milestone produces a demonstrable slice of behaviour.

Estimates are in **ideal days** (one focused engineer-day with no meetings, no interrupts, working on Pi 4 hardware or a fast x86_64 dev machine). Wall-clock time will be longer. Roles are abbreviated:

- **ENG-RUST** — Rust engineer (core pipeline, protocol facades, resolver, session)
- **ENG-TOR** — Tor / privacy engineer (circuit management, isolation, hardening)
- **ENG-PI** — Pi / kernel engineer (DRM/KMS, V4L2, thermal, packaging)
- **ENG-WEB** — Web / extension engineer (browser extension, configuration UI, a11y)
- **ENG-QA** — QA / test engineer (test strategy, CI, conformance, hardware loop)
- **ENG-SEC** — Security engineer (threat model, seccomp, supply chain, audit)
- **PM** — Project maintainer (branch merge, release, docs)

## Milestones

| Milestone | Theme | Demonstrable outcome | Estimated total |
|---|---|---|---|
| M1 | Tor + isolation foundation | `verify-network-isolation.sh` passes; YouTube URL resolves through Tor | 14 d |
| M2 | Decode + display pipeline | 1080p60 H.264 plays via DRM/KMS zero-copy on Pi 4 | 16 d |
| M3 | boGCast protocol surface | HTTP + WebSocket + DLNA facades all pass conformance | 18 d |
| M4 | Browser extension + installer | One-click cast from Chrome and Firefox; `curl | bash` install | 16 d |
| M5 | Reliability + thermal + a11y | Circuit rotation, thermal supervisor, a11y CI green | 14 d |
| M6 | Release hardening | Reproducible build, security audit, manual test pass | 12 d |
| **Total** | | | **90 d** |

## M1 — Tor + Isolation Foundation

Goal: the appliance routes all traffic through Tor with per-site circuit isolation, and YouTube URLs resolve through Tor to a direct media URL.

- [ ] **[[T-101]] Bootstrap Rust workspace skeleton.** Create the workspace `Cargo.toml`, the 13 crates from the fine draft ([[C-001]]..[[C-013]]) as empty crates with stub `lib.rs` / `main.rs`, and the `clippy::unwrap_used` lint config. _Owner: ENG-RUST. Estimate: 1 d. Dependencies: none. Traces to: [[C-001]]..[[C-013]]._
- [ ] **[[T-102]] `bogdan-config` crate.** Implement TOML parser, env-var overlay, unknown-field rejection, security-field-restart invariant. _Owner: ENG-RUST. Estimate: 2 d. Dependencies: [[T-101]]. Traces to: [[R-019]], [[C-013]]._
- [ ] **[[T-103]] `bogdan-tor` crate: daemon supervisor.** Start the C Tor daemon via systemd, monitor via control port, restart on crash. _Owner: ENG-TOR. Estimate: 2 d. Dependencies: [[T-102]]. Traces to: [[R-001]], [[C-005]]._
- [ ] **[[T-104]] `bogdan-tor`: `IsolateSOCKSAuth` username derivation.** Implement `TorProxy::username_for_host` (SHA-256 of host, first 16 hex chars). Deterministic and collision-resistant. _Owner: ENG-TOR. Estimate: 1 d. Dependencies: [[T-103]]. Traces to: [[R-002]], [[C-005]]._
- [ ] **[[T-105]] `config/torrc` and `config/iptables.rules`.** Hardened torrc (`AvoidDiskWrites`, `SafeLogging`, `CookieAuthentication`, `IsolateSOCKSAuth`); iptables rules dropping all non-Tor outbound except Tor ORPort 9001/9030. _Owner: ENG-TOR + ENG-SEC. Estimate: 2 d. Dependencies: [[T-103]]. Traces to: [[R-001]], [[R-003]]._
- [ ] **[[T-106]] `scripts/verify-network-isolation.sh`.** Runs `tcpdump` during a 60-second mock cast and asserts zero non-Tor packets. Exit non-zero on leak. _Owner: ENG-TOR + ENG-QA. Estimate: 2 d. Dependencies: [[T-105]]. Traces to: [[R-001]]._
- [ ] **[[T-107]] `bogdan-resolver` crate: in-tree YouTube resolver.** ~150-line Rust resolver using `reqwest` over `socks5h://`, returns `ResolvedMedia` within 10 s on a healthy Tor circuit. _Owner: ENG-RUST. Estimate: 3 d. Dependencies: [[T-104]]. Traces to: [[R-014]], [[C-007]]._
- [ ] **[[T-108]] `bogdan-resolver`: yt-dlp subprocess fallback.** Spawn `yt-dlp --dump-json --proxy socks5h://... --username <hosthash>` with 30 s timeout; parse JSON. _Owner: ENG-RUST. Estimate: 2 d. Dependencies: [[T-104]]. Traces to: [[R-015]], [[C-007]], [[BP-ADR-006]], [[ADR-008]]._
- [ ] **[[T-109]] `bogdan-resolver`: in-tree Vimeo + direct-media resolvers.** Two more custom resolvers to satisfy the 5-source coverage requirement. _Owner: ENG-RUST. Estimate: 2 d. Dependencies: [[T-107]]. Traces to: [[R-015]]._
- [ ] **[[T-110]] M1 demo: end-to-end resolve through Tor.** Manually invoke the resolver from a CLI harness, prove YouTube URL resolves through Tor to a direct media URL within 10 s, and `verify-network-isolation.sh` passes. _Owner: ENG-RUST + ENG-TOR. Estimate: 1 d. Dependencies: [[T-106]], [[T-107]], [[T-108]]. Traces to: [[R-001]], [[R-002]], [[R-003]], [[R-014]]._

**M1 critical path:** [[T-101]] → [[T-102]] → [[T-103]] → [[T-104]] → [[T-107]] → [[T-110]]. Total critical-path estimate: 10 d. Buffer for parallel work: 4 d. M1 budget: 14 d.

## M2 — Decode + Display Pipeline

Goal: a 1080p60 H.264 stream plays through the V4L2 hardware decoder to DRM/KMS HDMI output with zero copy, under 200 MB RSS, on a Pi 4.

- [ ] **[[T-201]] `bogdan-display` crate: DRM master + atomic modesetting.** Open `/dev/dri/card0`, acquire DRM master, program CRTC via `drmModeAtomicCommit` for plane 0. _Owner: ENG-PI. Estimate: 3 d. Dependencies: [[T-101]]. Traces to: [[R-004]], [[R-007]], [[C-006]]._
- [ ] **[[T-202]] `bogdan-playback` crate: GStreamer pipeline construction.** Build `appsrc → queue2 → parsebin → (pad-added) → queue → v4l2h264dec → v4l2convert → kmssink` for video, `→ avdec_aac → audioconvert → alsasink` for audio. Software-decode fallback to `avdec_h264`. _Owner: ENG-PI + ENG-RUST. Estimate: 4 d. Dependencies: [[T-201]]. Traces to: [[R-006]], [[R-007]], [[C-011]], [[BP-ADR-003]]._
- [ ] **[[T-203]] `bogdan-playback`: pad-added callback for codec dispatch.** Detect H.264 vs other codecs; route to `v4l2h264dec` for H.264, `avdec_h264` for unsupported codecs. _Owner: ENG-RUST. Estimate: 2 d. Dependencies: [[T-202]]. Traces to: [[R-006]], [[C-011]]._
- [ ] **[[T-204]] `bogdan-playback`: SOCKS5 forwarder.** Local SOCKS5 forwarder that pins the resolver's exit IP to the media-fetch client's exit IP via the per-host username (resolves [[T-001]] from the fine draft — choose tokio task inside `bogdan-playback`). _Owner: ENG-TOR + ENG-RUST. Estimate: 2 d. Dependencies: [[T-104]], [[T-202]]. Traces to: [[R-002]], [[R-013]], [[C-011]], [[BP-ADR-005]]._
- [ ] **[[T-205]] `bogdan-playback`: progressive-download buffer.** 10 s rolling buffer ahead of decode point, exposed via `buffer_percent` in `/api/status`. _Owner: ENG-RUST. Estimate: 2 d. Dependencies: [[T-202]]. Traces to: [[R-012]], [[C-011]]._
- [ ] **[[T-206]] `bogdan-playback`: CDN preflight check.** `GET Range: bytes=0-0` before starting full download; if `sp=380` speed-limit param present, try bypass URLs (`sp=99999`, `sp=` stripped); fall back to rate-limited URL. _Owner: ENG-RUST. Estimate: 2 d. Dependencies: [[T-202]]. Traces to: [[R-014]], [[C-011]]._
- [ ] **[[T-207]] `tests/hw_1080p60.rs` and `tests/hw_zero_copy.rs`.** Nightly Pi 4 tests: 1080p60 with < 50% CPU, < 200 MB RSS; `v4l2-ctl` shows DMA-BUF passthrough; `perf` shows no memcpy in decode→display. _Owner: ENG-PI + ENG-QA. Estimate: 3 d. Dependencies: [[T-202]], [[T-203]]. Traces to: [[R-005]], [[R-006]], [[R-007]]._
- [ ] **[[T-208]] M2 demo: 1080p60 zero-copy playback.** Play a 1080p60 H.264 test stream on a Pi 4 with HDMI attached; show < 50% CPU, < 200 MB RSS, `v4l2-ctl` shows DMA-BUF. _Owner: ENG-PI. Estimate: 1 d. Dependencies: [[T-207]]. Traces to: [[R-005]], [[R-006]], [[R-007]]._

**M2 critical path:** [[T-201]] → [[T-202]] → [[T-203]] → [[T-207]] → [[T-208]]. Total critical-path estimate: 13 d. Buffer: 3 d. M2 budget: 16 d.

## M3 — boGCast Protocol Surface

Goal: the three protocol facades (HTTP, WebSocket, DLNA) all pass conformance and at least two third-party clients interoperate.

- [ ] **[[T-301]] `bogdan-session` crate: state machine.** Implement `Session`, `CastCommand`, `SessionState`, `ErrorCode` per the data model in the fine draft. Single-threaded behind `Arc<Mutex<Session>>`. _Owner: ENG-RUST. Estimate: 3 d. Dependencies: [[T-102]]. Traces to: [[C-010]], [[R-008]], [[R-009]]._
- [ ] **[[T-302]] HTTP REST facade (POST /api/cast, /stop, /pause, /resume, /seek, GET /api/status).** Translate each into a `CastCommand`; CORS `*`; TLS opt-in. _Owner: ENG-RUST. Estimate: 3 d. Dependencies: [[T-301]]. Traces to: [[R-008]], [[C-002]]._
- [ ] **[[T-303]] WebSocket facade (`:8586/events`).** Push `state_changed`, `buffer_update`, `circuit_rotated`, `thermal_throttled`, `error` events; 1024-entry ring buffer; `last_event_id` reconnect. _Owner: ENG-RUST. Estimate: 3 d. Dependencies: [[T-301]]. Traces to: [[R-009]], [[C-003]]._
- [ ] **[[T-304]] DLNA facade: gmediarender subprocess management.** Spawn `gmediarender`, advertise via SSDP, accept `SetAVTransportURI`, translate to `CastCommand`. _Owner: ENG-RUST + ENG-PI. Estimate: 3 d. Dependencies: [[T-301]], [[T-201]]. Traces to: [[R-010]], [[R-021]], [[C-004]], [[BP-ADR-009]]._
- [ ] **[[T-305]] DRM-master contention protocol.** Teardown `gmediarender` before pipeline construction; 500 ms grace; 2 s retry budget (4 × 500 ms); surface `drm_master_busy` error on exhaustion. _Owner: ENG-PI. Estimate: 2 d. Dependencies: [[T-304]], [[T-201]]. Traces to: [[R-021]], [[BP-ADR-009]]._
- [ ] **[[T-306]] `bogdan-server` main binary: startup orchestration.** Parse config, spawn tor supervisor, three facades, single `Session`. Expose `/api/status`. _Owner: ENG-RUST. Estimate: 2 d. Dependencies: [[T-103]], [[T-302]], [[T-303]], [[T-304]]. Traces to: [[C-001]]._
- [ ] **[[T-307]] Conformance suites: HTTP, WebSocket, DLNA.** `tests/conformance/{http,ws,dlna}/` with `pytest` + `curl` + `wscat` + `gupnp-universal-cp`. Run in CI on every PR. _Owner: ENG-QA. Estimate: 3 d. Dependencies: [[T-302]], [[T-303]], [[T-304]]. Traces to: [[R-008]], [[R-009]], [[R-010]]._
- [ ] **[[T-308]] Third-party client interop smoke matrix.** VLC, MiniDLNA, Home Assistant, Plex. Documented in `docs/interop-matrix.md`; manual run pre-release. _Owner: ENG-QA + PM. Estimate: 2 d. Dependencies: [[T-307]]. Traces to: [[R-011]], [[R-022]]._

**M3 critical path:** [[T-301]] → [[T-302]] → [[T-306]] → [[T-307]]. Total critical-path estimate: 11 d. Buffer: 7 d. M3 budget: 18 d.

## M4 — Browser Extension + Installer

Goal: one-click cast from Chrome and Firefox; one-command install on a fresh Pi.

- [ ] **[[T-401]] Browser extension skeleton (Manifest V3, single codebase).** `src/extension/` with `manifest.json`, `background.js` (service worker), `content.js`, `popup.html` / `popup.js`. `webextension-polyfill` for `chrome.*` / `browser.*` namespace. _Owner: ENG-WEB. Estimate: 2 d. Dependencies: [[T-302]]. Traces to: [[R-016]], [[R-017]], [[C-008]], [[BP-ADR-007]]._
- [ ] **[[T-402]] Extension: media URL detection on active tab.** `chrome.tabs` / `browser.tabs` query; DOM scraping for `<video>` and `<source>` elements; one-click cast to `/api/cast`. _Owner: ENG-WEB. Estimate: 3 d. Dependencies: [[T-401]]. Traces to: [[R-016]], [[R-017]]._
- [ ] **[[T-403]] Extension: WebSocket status subscriber.** Subscribe to `:8586/events`; update popup badge on `state_changed`; reconnect on service-worker eviction with `last_event_id`. _Owner: ENG-WEB. Estimate: 2 d. Dependencies: [[T-303]], [[T-401]]. Traces to: [[R-009]], [[R-016]], [[R-017]]._
- [ ] **[[T-404]] Extension: build for Chrome and Firefox.** `npm run build:chrome` and `npm run build:firefox` producing `.zip` and `.xpi` respectively. Resolves [[T-008]] from the fine draft — use `chrome.storage.local` for the last-used Pi address. _Owner: ENG-WEB. Estimate: 1 d. Dependencies: [[T-402]], [[T-403]]. Traces to: [[R-016]], [[R-017]]._
- [ ] **[[T-405]] `scripts/setup.sh` installer.** Pinned to commit SHA; installs systemd unit, torrc, iptables rules, boGDan binary; reboots on completion. _Owner: ENG-PI + ENG-SEC. Estimate: 2 d. Dependencies: [[T-306]], [[T-105]]. Traces to: [[R-018]], [[C-009]], [[BP-ADR-008]]._
- [ ] **[[T-406]] Installer GPG signature + SHA pinning.** Detached signature for `setup.sh`; README documents the pinned SHA (not `main`). _Owner: ENG-SEC. Estimate: 1 d. Dependencies: [[T-405]]. Traces to: [[R-018]]._
- [ ] **[[T-407]] First-boot web UI at `http://bogdan.local`.** mDNS via Avahi; web UI handles Tor bridge selection, network config, media source prefs; persists to `/etc/bogdan/bogdan.toml`. Resolves [[T-002]] from the fine draft — use vanilla HTML + a few hundred lines (no framework) to keep the a11y test surface small. _Owner: ENG-WEB + ENG-PI. Estimate: 4 d. Dependencies: [[T-102]], [[T-306]]. Traces to: [[R-019]], [[R-020]], [[C-009]], [[BP-ADR-008]]._
- [ ] **[[T-408]] `/etc/issue` IP-address fallback.** Print `boGDan web UI: http://<ip>:8585/` to `/etc/issue` on boot, for networks where mDNS doesn't work. _Owner: ENG-PI. Estimate: 1 d. Dependencies: [[T-407]]. Traces to: [[R-020]]._

**M4 critical path:** [[T-401]] → [[T-402]] → [[T-404]] (extension track) and [[T-405]] → [[T-407]] (installer track), joined at M4 demo. Critical path estimate: 8 d. Buffer: 8 d. M4 budget: 16 d.

## M5 — Reliability + Thermal + Accessibility

Goal: stream survives circuit rotation, thermal supervisor enforces thresholds, web UI passes WAVE / axe-core.

- [ ] **[[T-501]] Circuit rotation survival.** On `NEWNYM` or stream error, re-establish playback within 5 s using the 10 s rolling buffer. _Owner: ENG-TOR + ENG-RUST. Estimate: 3 d. Dependencies: [[T-204]], [[T-205]], [[T-301]]. Traces to: [[R-012]], [[C-010]], [[C-011]]._
- [ ] **[[T-502]] Automatic circuit replacement within 10 s.** Detect failed circuit (GStreamer bus error or `reqwest` timeout) within 5 s; re-resolve via same per-host username; re-establish within 10 s. _Owner: ENG-TOR + ENG-RUST. Estimate: 3 d. Dependencies: [[T-501]]. Traces to: [[R-013]], [[BP-ADR-005]]._
- [ ] **[[T-503]] `bogdan-thermal` crate: threshold monitor.** Poll `thermal_zone0/temp` every 5 s; warn above 75 °C; throttle above 80 °C; pause above 85 °C; resume below 75 °C. Expose `thermal_throttled`, `cpu_temp_celsius` in `/api/status`. _Owner: ENG-PI. Estimate: 2 d. Dependencies: [[T-306]]. Traces to: [[R-023]], [[C-012]], [[BP-ADR-010]]._
- [ ] **[[T-504]] Thermal bitrate fallback contract with resolver.** Resolver returns `ResolveError::NoLowerVariant` when no lower-bitrate variant exists; thermal supervisor requests lower variant above 80 °C. Resolves [[T-007]] from the fine draft. _Owner: ENG-PI + ENG-RUST. Estimate: 2 d. Dependencies: [[T-503]], [[T-107]]. Traces to: [[R-024]], [[BP-ADR-010]]._
- [ ] **[[T-505]] Web UI: semantic HTML + ARIA landmarks.** Refactor `src/server/web/` to use semantic HTML5 (`<header>`, `<nav>`, `<main>`, `<section>`, `<footer>`) and ARIA landmarks. _Owner: ENG-WEB. Estimate: 2 d. Dependencies: [[T-407]]. Traces to: [[R-026]], [[R-027]], [[BP-ADR-012]]._
- [ ] **[[T-506]] Web UI: keyboard-only navigation.** Tab / Shift-Tab / Enter / Escape cover all actions; visible focus indicator with ≥ 3:1 contrast; focus order follows visual order. _Owner: ENG-WEB. Estimate: 2 d. Dependencies: [[T-505]]. Traces to: [[R-027]]._
- [ ] **[[T-507]] Web UI: high-contrast mode.** CSS variable theme; toggle in UI header; persisted in `localStorage`; achieves ≥ 7:1 contrast. _Owner: ENG-WEB. Estimate: 1 d. Dependencies: [[T-505]]. Traces to: [[R-028]]._
- [ ] **[[T-508]] CI: WAVE + axe-core on every PR.** `.github/workflows/ci.yml` `a11y` job runs WAVE CLI and `@axe-core/playwright` against a built web UI; fails on any violation. _Owner: ENG-QA + ENG-WEB. Estimate: 2 d. Dependencies: [[T-505]], [[T-506]], [[T-507]]. Traces to: [[R-026]], [[R-027]], [[R-028]]._

**M5 critical path:** [[T-501]] → [[T-502]] (reliability track, 6 d) and [[T-505]] → [[T-506]] → [[T-508]] (a11y track, 6 d), in parallel. Critical path estimate: 6 d. Buffer: 8 d. M5 budget: 14 d.

## M6 — Release Hardening

Goal: reproducible Debian build, security audit clean, manual test pass complete; v1 ships.

- [ ] **[[T-601]] `cargo-deny` + `cargo-audit` in CI.** Block on vulnerable / yanked / unlicensed Rust dependencies; nightly `cargo-audit` run. _Owner: ENG-SEC + ENG-QA. Estimate: 1 d. Dependencies: [[T-101]]. Traces to: security controls inventory in fine draft._
- [ ] **[[T-602]] `seccomp` filter for gmediarender and yt-dlp.** `SCMP_ACT_KILL` baseline; whitelist TBD; nightly escape-attempt test. _Owner: ENG-SEC. Estimate: 3 d. Dependencies: [[T-108]], [[T-304]]. Traces to: [[BP-ADR-006]], [[BP-ADR-009]]._
- [ ] **[[T-603]] Reproducible Debian build.** `packaging/build-deb.sh`; build twice; compare `sha256sum` of `.deb`. _Owner: ENG-PI + ENG-SEC. Estimate: 2 d. Dependencies: [[T-306]]. Traces to: [[R-018]], [[BP-ADR-008]]._
- [ ] **[[T-604]] Pre-built SD card image.** Pi OS Lite + boGDan pre-installed; compressed image on GitHub Releases. _Owner: ENG-PI. Estimate: 2 d. Dependencies: [[T-603]]. Traces to: [[R-018]]._
- [ ] **[[T-605]] Security audit checklist pass.** Run through `docs/SECURITY_AUDIT.md`; close all open findings or document accepted risk. _Owner: ENG-SEC. Estimate: 3 d. Dependencies: [[T-601]], [[T-602]], [[T-603]]. Traces to: security controls inventory._
- [ ] **[[T-606]] Manual accessibility test pass (NVDA + VoiceOver).** Per `docs/blueprint/04-fine-draft.md` Test Strategy → Accessibility Tests. Document findings in `docs/a11y.md`. _Owner: ENG-WEB + ENG-QA. Estimate: 2 d. Dependencies: [[T-508]]. Traces to: [[R-026]], [[R-027]], [[R-028]]._
- [ ] **[[T-607]] Third-party client interop smoke matrix.** Run [[T-308]] against the v1 release candidate. _Owner: ENG-QA + PM. Estimate: 1 d. Dependencies: [[T-308]], [[T-604]]. Traces to: [[R-011]], [[R-022]]._
- [ ] **[[T-608]] v1 release: tag, changelog, GitHub Release.** Tag `v1.0.0`; generate changelog from PRs; upload `.deb`, SD card image, `setup.sh`, and signature to GitHub Releases. _Owner: PM. Estimate: 1 d. Dependencies: [[T-603]], [[T-604]], [[T-605]], [[T-606]], [[T-607]]. Traces to: [[R-018]]._

**M6 critical path:** [[T-602]] → [[T-605]] → [[T-608]]. Critical path estimate: 7 d. Buffer: 5 d. M6 budget: 12 d.

## Dependency Graph (Critical Path)

```
M1: T-101 → T-102 → T-103 → T-104 → T-107 → T-110          (10 d)
                          ↓
M2:                       T-201 → T-202 → T-203 → T-207 → T-208   (13 d)
                                    ↓
M3:                                T-301 → T-302 → T-306 → T-307   (11 d)
                                                ↓
M4:                                          T-405 → T-407 → T-408   (7 d)
                                                ↓
M5:                                          T-501 → T-502           (6 d, parallel with a11y)
                                                ↓
M6:                                          T-602 → T-605 → T-608   (7 d)
```

Total critical path: ~54 ideal days. With parallelism, the full v1 ships in ~90 ideal days (per the milestone table above).

## Open Questions from the Fine Draft, Resolved by Tasks

| Fine-draft question | Resolved by | Resolution |
|---|---|---|
| [[T-001]] (fine-draft) SOCKS5 forwarder impl | [[T-204]] (this doc) | Tokio task inside `bogdan-playback` |
| [[T-002]] (fine-draft) Web UI framework | [[T-407]] (this doc) | Vanilla HTML + a few hundred lines |
| [[T-003]] (fine-draft) yt-dlp pinning strategy | [[T-108]] (this doc) | Pin to a release tag; document `sudo yt-dlp -U` for users |
| [[T-004]] (fine-draft) WebSocket ring buffer eviction | [[T-303]] (this doc) | Drop oldest; `events_dropped: true` in first replayed event |
| [[T-005]] (fine-draft) gmediarender seccomp profile | [[T-602]] (this doc) | Baseline `SCMP_ACT_KILL`; whitelist TBD in security audit |
| [[T-006]] (fine-draft) mDNS fallback | [[T-408]] (this doc) | Print IP to `/etc/issue` on boot |
| [[T-007]] (fine-draft) Thermal supervisor / resolver contract | [[T-504]] (this doc) | Typed `ResolveError::NoLowerVariant` |
| [[T-008]] (fine-draft) Browser extension MV3 storage | [[T-404]] (this doc) | `chrome.storage.local` |

## Deferred Requirements

The following requirement from `05-spec.md` is intentionally not tasked in v1. It is recorded here so the coverage gap is explicit rather than silent.

| Requirement | Problem | Reason | Deferred to |
|-------------|---------|--------|-------------|
| [[R-025]] | [[P-011]] | Multi-room sync is a nice-to-have; v1 focuses on the must-have and should-have problems. The leader-follower sketch is recorded in [[BP-ADR-011]]. | v2 (future M7 milestone) |

When v2 multi-room work begins, a new task (e.g., `[[T-701]]` in a future M7 milestone) will trace to [[R-025]]. Until then, each appliance plays independently — this is documented as a known limitation in the user guide.



## Roles Summary

| Role | Headcount (ideal) | Milestones |
|---|---|---|
| ENG-RUST | 2 | M1, M2, M3, M5 |
| ENG-TOR | 1 | M1, M2, M5 |
| ENG-PI | 1 | M2, M3, M4, M6 |
| ENG-WEB | 1 | M4, M5, M6 |
| ENG-QA | 1 | M1, M2, M3, M5, M6 |
| ENG-SEC | 0.5 | M1, M4, M6 |
| PM | 0.25 | M3, M6 |

Total: ~6.75 full-time equivalents for ~90 ideal days, which is ~12 calendar weeks at 75% efficiency.

## Risks to the Schedule

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| V4L2 stateful decoder bug requires upstream kernel patch | Medium | High (blocks M2) | [[T-202]] has a 4-day estimate with buffer; if a kernel patch is needed, fall back to stateless `v4l2slh264dec` (rejected in [[BP-ADR-003]] but available as a contingency) |
| yt-dlp YouTube extractor breaks during M1 | High | Medium (delays M1 demo) | [[T-107]] in-tree resolver bypasses yt-dlp for YouTube; yt-dlp fallback covers other sites |
| DRM master contention worse than expected | Medium | Medium (delays M3) | [[T-305]] has a 2-day estimate; if 4 retries × 500 ms is insufficient, extend to 8 retries × 500 ms (4 s budget) before redesigning |
| gmediarender seccomp profile too restrictive | Medium | Medium (delays M6) | [[T-602]] has 3 days; if whitelist is too hard, fall back to AppArmor confinement |
| Chrome MV3 service-worker eviction breaks extension UX | Medium | Low (extension only) | [[T-403]] reconnect logic is mandatory; if eviction is too aggressive, persist last `last_event_id` in `chrome.storage.local` |
| Reproducible build fails for `.deb` | Low | Medium (delays M6) | [[T-603]] has 2 days; if `dpkg` reproducibility is broken, ship a `.tar.gz` with a checksum as the verified-install alternative |
| Manual a11y test pass finds major issues | Medium | Medium (delays M6) | [[T-606]] has 2 days; if issues are major, schedule a follow-up v1.1 release; do not block v1 on nice-to-have a11y |
