---
doc: progress
project: picast
version: 1
phase: progress
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# boGDan Blueprint — Progress

> **This is a living document.** It is updated whenever a blueprint phase completes, an implementation milestone starts or finishes, a blocker emerges or clears, or the next-steps list changes. The most recent update is at the top of the **Update Log**; the current state is in the **Current Status** section. Editors: prepend new entries to the Update Log, never overwrite history.

This document tracks the boGDan blueprint pipeline (`docs/blueprint/01-problem-catalog.md` through `docs/blueprint/06-tasks.md`) and the v1 implementation milestones (M1 through M6 from `docs/blueprint/06-tasks.md`). It is the single source of truth for "where are we, what's blocking us, what's next."

Conventions:
- Blueprint phases: `problem_catalog` → `rough_draft` → `adrs` → `fine_draft` → `spec` → `tasks` → `progress` (this doc) → `implementation`.
- Implementation milestones: M1 (Tor foundation) → M2 (Decode + display) → M3 (boGCast) → M4 (Extension + installer) → M5 (Reliability + thermal + a11y) → M6 (Release hardening).
- Statuses: `done`, `in_progress`, `blocked`, `not_started`, `deferred`, `cancelled`.
- IDs: [[P-NNN]] problems, [[R-NNN]] requirements, [[T-NNN]] tasks (this doc's task IDs), [[BP-ADR-NNN]] blueprint ADRs, [[C-NNN]] components, [[ADR-NNN]] ratified ADRs.

## Current Status

**As of:** 2026-07-30

**Blueprint pipeline:** 6 of 7 phases complete. Only `implementation` remains, which is the v1 build itself.

**Implementation:** 0 of 6 milestones started. The blueprint is ready to hand off to implementation.

**Overall health:** green. No blockers. Ready to start M1.

### Blueprint Phase Status

| Phase | Doc | Status | Owner | Completed | Notes |
|---|---|---|---|---|---|
| problem_catalog | `docs/blueprint/01-problem-catalog.md` | done | stronghold-agent | 2026-07-29 | 12 problems P-001..P-012 catalogued with priorities, impacts, success metrics |
| rough_draft | `docs/blueprint/02-rough-draft.md` | done | agent | 2026-07-30 | All 12 problems addressed; each with Approach / Alternative / Risk. Branch `docs/blueprint-rough-draft` |
| adrs | `docs/blueprint/03-adrs/` | done | agent | 2026-07-30 | 12 blueprint ADRs BP-ADR-001..012 + README index. Branch `docs/blueprint-adrs` |
| fine_draft | `docs/blueprint/04-fine-draft.md` | done | agent | 2026-07-30 | 13 components, data model, 12-entry threat model, test strategy, 8 open questions. Branch `docs/blueprint-fine-draft` |
| spec | `docs/blueprint/05-spec.md` | done | agent | 2026-07-30 | 28 requirements R-001..R-028, every problem covered, Given/When/Then acceptance criteria, traceability tables. Branch `docs/blueprint-spec` |
| tasks | `docs/blueprint/06-tasks.md` | done | agent | 2026-07-30 | 50 tasks T-101..T-608 across 6 milestones, ~90 ideal days, all 8 fine-draft open questions resolved. Branch `docs/blueprint-tasks` |
| progress | `docs/blueprint/07-progress.md` | done | agent | 2026-07-30 | This document. Branch `docs/blueprint-progress` |
| implementation | (TBD) | not_started | TBD | — | Implementation has not started; M1 is the next milestone |

### Implementation Milestone Status

| Milestone | Theme | Status | Started | Estimated complete | Tasks done / total |
|---|---|---|---|---|---|
| M1 | Tor + isolation foundation | not_started | — | — | 0 / 10 |
| M2 | Decode + display pipeline | not_started | — | — | 0 / 8 |
| M3 | boGCast protocol surface | not_started | — | — | 0 / 8 |
| M4 | Browser extension + installer | not_started | — | — | 0 / 8 |
| M5 | Reliability + thermal + a11y | not_started | — | — | 0 / 8 |
| M6 | Release hardening | not_started | — | — | 0 / 8 |

### Branch Status

| Branch | Purpose | Status | Merged to main? |
|---|---|---|---|
| `docs/blueprint-rough-draft` | Rough draft (phase 2) | pushed | no |
| `docs/blueprint-adrs` | Blueprint ADRs (phase 3) | pushed | no |
| `docs/blueprint-fine-draft` | Fine draft (phase 4) | pushed | no |
| `docs/blueprint-spec` | Spec (phase 5) | pushed | no |
| `docs/blueprint-tasks` | Tasks (phase 6) | pushed | no |
| `docs/blueprint-progress` | This doc (phase 7) | pushed | no |
| `main` | Integration branch | clean | — |

**Action required:** A maintainer should review and merge the six blueprint branches into `main` (in order: rough-draft → adrs → fine-draft → spec → tasks → progress) before implementation starts, so that implementation PRs can reference the canonical paths.

## Blockers

**None.** The blueprint is complete and ready for implementation handoff.

## Next Steps

### Immediate (this week)

1. **Merge blueprint branches to `main`** in dependency order: `docs/blueprint-rough-draft` → `docs/blueprint-adrs` → `docs/blueprint-fine-draft` → `docs/blueprint-spec` → `docs/blueprint-tasks` → `docs/blueprint-progress`. Each is a docs-only PR with no source-code impact.
2. **Open 6 GitHub PRs** (one per branch) so reviewers can sign off per phase. Suggested reviewers: project maintainer (all phases), ENG-RUST (fine_draft, spec, tasks), ENG-TOR (rough_draft, adrs, spec for Tor-related requirements), ENG-PI (fine_draft for hardware, tasks for milestone estimates).
3. **Update `docs/AGENT.md`** to reference the blueprint pipeline so new contributors and AI agents find the canonical docs.

### Short-term (next 2 weeks)

4. **Spin up the implementation team** per the roles table in `docs/blueprint/06-tasks.md` (Roles Summary). Minimum viable team for M1: 1× ENG-RUST, 1× ENG-TOR.
5. **Start M1 — Tor + isolation foundation.** First task: [[T-101]] (bootstrap Rust workspace skeleton). Critical path through M1 is 10 ideal days; M1 budget is 14 days.
6. **Provision a Pi 4 self-hosted CI runner** for the nightly hardware-in-the-loop tests (`tests/hw_*.rs`). Without this, [[R-005]], [[R-006]], [[R-007]], [[R-014]] cannot be verified in CI.

### Medium-term (next 6 weeks)

7. **M1 demo (end of week 2):** `verify-network-isolation.sh` passes; YouTube URL resolves through Tor to a direct media URL within 10 s.
8. **M2 demo (end of week 4):** 1080p60 H.264 plays via DRM/KMS zero-copy on a Pi 4 with < 50% CPU and < 200 MB RSS.
9. **M3 demo (end of week 6):** HTTP + WebSocket + DLNA facades all pass conformance; VLC casts to boGDan via DLNA.

### Long-term (next 12 weeks)

10. **M4–M6** per the milestone schedule in `docs/blueprint/06-tasks.md`. v1 ships at the end of week 12 (~90 ideal days).
11. **v1 release:** tag `v1.0.0`, publish `.deb` + SD card image + `setup.sh` + GPG signature to GitHub Releases (per [[T-608]]).

## Decision Log (This Phase)

Decisions made during the blueprint pipeline that are worth surfacing for future readers:

| Date | Decision | Rationale | Ref |
|---|---|---|---|
| 2026-07-29 | Catalogue 12 problems with priorities must-have / should-have / nice-to-have | Anchors the rest of the pipeline in user-visible value; nice-to-haves (P-010, P-011, P-012) are scoped to not block v1 | `01-problem-catalog.md` |
| 2026-07-30 | Inherit ratified ADR-001..009 rather than re-deciding | The 9 ratified ADRs in `docs/decisions/` already cover display server, browser, GStreamer, Tor, Cast V2, DLNA, DRM, yt-dlp, HEVC. Blueprint ADRs elaborate, they don't re-litigate | `03-adrs/README.md` |
| 2026-07-30 | Defer multi-room sync (P-011) to v2 | P-011 is nice-to-have; building it into v1 would block the release. Recorded the v2 sketch (PTP over WebSocket, 100 ms tolerance, ethernet-only) so future implementers have a starting point | [[BP-ADR-011]] |
| 2026-07-30 | Use `BP-ADR-NNN` IDs for blueprint ADRs, separate from `ADR-NNN` | Avoids collision with the 9 ratified ADRs; signals "proposed, not yet ratified" | `03-adrs/README.md` |
| 2026-07-30 | All blueprint ADRs PROPOSED except BP-ADR-011 (DEFERRED) | Blueprint-phase decisions need validation in detailed design before ratification | `03-adrs/README.md` |
| 2026-07-30 | Layered resolvers: in-tree for YouTube/Vimeo/direct, yt-dlp fallback | yt-dlp's general-purpose extractor adds 5–15 s overhead per cast on Pi 4, exceeding the 10 s budget for YouTube. Custom resolvers for the top 3 sources meet the budget; yt-dlp covers the long tail | [[BP-ADR-006]] |
| 2026-07-30 | Single Manifest V3 codebase for Chrome + Firefox | MV2 is being sunset by both Google and Mozilla; two codebases double the maintenance for no upside | [[BP-ADR-007]] |
| 2026-07-30 | Keep `curl | bash`, Debian package, and SD card image as parallel install paths | Different users have different trust models and starting points; collapsing to one path would exclude a user segment | [[BP-ADR-008]] |
| 2026-07-30 | Run `gmediarender` under boGDan control (not separate systemd unit) | DRM master contention is unmanageable without a single arbiter; the session state machine must own both gmediarender and the playback pipeline | [[BP-ADR-009]] |
| 2026-07-30 | Bitrate fallback above 80 °C, pause above 85 °C | Hard-failing a movie mid-playback is materially worse UX than downshifting to 360p | [[BP-ADR-010]] |
| 2026-07-30 | All 28 requirements use RFC 2119 keywords + Given/When/Then acceptance criteria | Makes every requirement mechanically testable; pairs each requirement with a primary test in the traceability table | `05-spec.md` |
| 2026-07-30 | 6-milestone breakdown, ~90 ideal days total | Sequenced so each milestone produces a demonstrable slice; critical path is ~54 days with parallelism extending to 90 | `06-tasks.md` |
| 2026-07-30 | Resolve all 8 fine-draft open questions in the tasks doc | Each fine-draft T-NNN question is mapped to a tasks-doc T-NNN that resolves it, so implementation starts with no open design questions | `06-tasks.md` Open Questions table |

## Open Questions for Implementation

These are intentionally left open for the implementation phase to resolve (they are too narrow for the blueprint, but worth flagging):

1. **V4L2 stateful decoder firmware version pinning.** Should the boGDan image pin a specific BCM2711 firmware version, or track Raspberry Pi OS updates? Affects [[R-006]] stability.
2. **Tor bridge distribution.** The web UI lets users paste obfs4 bridge lines, but does boGDan bundle a default bridge for users in censored regions? If yes, which? Affects [[R-019]] UX.
3. **WebSocket ring buffer: per-event-type eviction?** The spec says drop oldest; in practice, dropping all `buffer_update` events before any `state_changed` may be better UX. Affects [[R-009]].
4. **Extension auto-update mechanism.** Chrome Web Store auto-updates; Firefox AMO auto-updates; but the README also documents sideloading. How do sideloaded installs get updates? Affects [[R-016]], [[R-017]].
5. **Plex cast via DLNA vs. Plex's native protocol.** The spec accepts DLNA-only; if Plex users want native protocol, that's a separate epic. Affects [[R-011]].
6. **Thermal supervisor on Pi 5.** The blueprint targets Pi 4B+; Pi 5 has a different SoC and different thermal characteristics. Out of scope for v1 but worth a note in the user guide.

## Update Log

Most recent first. Append new entries to the top.

### 2026-07-30 — Tasks doc: Deferred Requirements section added (post-pipeline follow-up)

- **Phase:** tasks (post-pipeline follow-up)
- **By:** stronghold-agent (orchestrator-applied, commit `fff52aa`)
- **Change:** `docs/blueprint/06-tasks.md` gained a "Deferred Requirements" section explicitly recording the [[R-025]] coverage gap (multi-room sync deferred to v2). Previously the tasks doc's M1..M6 milestones did not trace to [[R-025]]; the gap is now explicit rather than silent. The new section names a future `[[T-701]]` in a future M7 milestone as the v2 placeholder for [[R-025]].
- **Status:** green. The coverage gap is now explicit.
- **Next:** fast-forward `docs/blueprint-progress` branch to include `fff52aa` (done in this commit); continue polling for new orchestrator tasks. No implementation work has started.

### 2026-07-30 — Quality review: R-025 coverage gap fixed

- **Phase:** tasks (quality review)
- **By:** agent
- **Change:** `docs/blueprint/06-tasks.md` updated on `docs/blueprint-tasks` branch (commit `fff52aa`). Added a "Deferred Requirements" section explicitly acknowledging that [[R-025]] (multi-room sync, deferred to v2 per [[BP-ADR-011]]) is intentionally not tasked in v1. The tasks doc previously traced to 27 of 28 requirements; the gap is now documented rather than silent.
- **Status:** green. All 28 requirements now accounted for (27 tasked in v1, 1 explicitly deferred to v2).
- **Next:** continue quality review of remaining blueprint docs; poll for new implementation tasks.



### 2026-07-30 — Blueprint pipeline complete

- **Phase:** progress (this doc)
- **By:** agent
- **Change:** All 6 prior blueprint phases complete and pushed to their respective branches. This progress doc is the 7th and final blueprint phase. Implementation has not started.
- **Status:** green. No blockers. Ready for M1.
- **Next:** merge the 6 blueprint branches to `main`; open 6 PRs; spin up the M1 team; provision a Pi 4 CI runner.

### 2026-07-30 — Tasks doc complete

- **Phase:** tasks
- **By:** agent
- **Change:** `docs/blueprint/06-tasks.md` written and pushed to `docs/blueprint-tasks` branch (commit `e167176`). 50 tasks T-101..T-608 across 6 milestones, ~90 ideal days. All 8 fine-draft open questions resolved.
- **Status:** green.
- **Next:** write this progress doc.

### 2026-07-30 — Spec complete

- **Phase:** spec
- **By:** agent
- **Change:** `docs/blueprint/05-spec.md` written and pushed to `docs/blueprint-spec` branch (commit `2d109b4`). 28 requirements R-001..R-028, every problem P-001..P-012 addressed, Given/When/Then acceptance criteria, traceability tables.
- **Status:** green.
- **Next:** write the tasks doc.

### 2026-07-30 — Fine draft complete

- **Phase:** fine_draft
- **By:** agent
- **Change:** `docs/blueprint/04-fine-draft.md` written and pushed to `docs/blueprint-fine-draft` branch (commit `9d94c6b`). 13 components, data model, 12-entry threat model, test strategy, 8 open questions for detailed design.
- **Status:** green.
- **Next:** write the spec.

### 2026-07-30 — Blueprint ADRs complete

- **Phase:** adrs
- **By:** agent
- **Change:** `docs/blueprint/03-adrs/` written and pushed to `docs/blueprint-adrs` branch (commit `0716a9b`, plus an orchestrator-applied YAML front-matter follow-up at `791051b`). 12 blueprint ADRs BP-ADR-001..012 + README index, following the project's ADR template.
- **Status:** green.
- **Next:** write the fine draft.

### 2026-07-30 — Rough draft complete

- **Phase:** rough_draft
- **By:** agent
- **Change:** `docs/blueprint/02-rough-draft.md` written and pushed to `docs/blueprint-rough-draft` branch (commit `363645a`). All 12 problems addressed with Approach / Alternative considered / Risk.
- **Status:** green.
- **Next:** write the blueprint ADRs.

### 2026-07-29 — Problem catalog complete

- **Phase:** problem_catalog
- **By:** stronghold-agent
- **Change:** `docs/blueprint/01-problem-catalog.md` written. 12 problems P-001..P-012 catalogued with priorities, impacts, success metrics, stakeholder list, constraints. Pushed as part of the rough-draft branch lineage.
- **Status:** green.
- **Next:** write the rough draft.

## How to Update This Document

This is a living document. To update it:

1. **Read first.** Always start by reading the current state. The Update Log is append-only; never rewrite history.
2. **Update Current Status.** If a phase or milestone status changed, update the relevant table. The `Updated` field in the YAML front-matter MUST be bumped to the current date.
3. **Update Blockers.** If a blocker emerged or cleared, update this section. Be specific: what is blocked, by what, since when, who owns unblocking.
4. **Update Next Steps.** If the next-steps list changed (e.g. a step completed, a new step emerged), update the list. Keep the most immediately actionable items at the top.
5. **Append to Decision Log.** If a decision was made, add a row to the Decision Log with the date, the decision, the rationale, and a reference to where it's documented.
6. **Append to Update Log.** Prepend a new entry at the top of the Update Log with today's date, the phase, who made the change, what changed, the status, and the next step. Never overwrite or delete existing Update Log entries.
7. **Commit and push.** Use a docs-only commit message: `docs: update progress doc (<reason>)`. Open a PR if the change is non-trivial.

### Update cadence

- **At every blueprint phase completion** — mandatory.
- **At every implementation milestone start / completion** — mandatory.
- **When a blocker emerges or clears** — mandatory, same day.
- **When the next-steps list changes** — mandatory, same day.
- **Otherwise** — weekly, even if nothing changed, to confirm the document is still accurate.

### Owners

- **Blueprint phases (1–7):** agent (or whoever runs the blueprint pipeline).
- **Implementation milestones (M1–M6):** the milestone's lead engineer (ENG-RUST for M1, ENG-PI for M2, etc.).
- **Blocker tracking:** whoever discovers the blocker is responsible for updating this doc within 24 hours.
- **PR review:** project maintainer (PM) reviews all updates to this doc before merge.

## Cross-References

- Problem catalog: `docs/blueprint/01-problem-catalog.md` — what we're solving
- Rough draft: `docs/blueprint/02-rough-draft.md` — first-pass solutions
- Blueprint ADRs: `docs/blueprint/03-adrs/` — formalised decisions
- Fine draft: `docs/blueprint/04-fine-draft.md` — components, data model, security, tests
- Spec: `docs/blueprint/05-spec.md` — requirements with acceptance criteria
- Tasks: `docs/blueprint/06-tasks.md` — implementation breakdown
- This doc: `docs/blueprint/07-progress.md` — status, blockers, next steps
- Ratified ADRs: `docs/decisions/` + `DECISIONS.md`
- Architecture: `ARCHITECTURE.md`
- API contract: `SPECIFICATION.md`
- Security: `docs/SECURITY.md`, `docs/SECURITY_AUDIT.md`
