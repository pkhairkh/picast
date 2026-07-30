# Blueprint ADR Index

> **Phase:** rough draft → detailed design
> **Status:** All entries PROPOSED unless noted otherwise.
> **Source:** Decisions formalised from `docs/blueprint/02-rough-draft.md`.
> **Relationship to project ADRs:** These blueprint-phase ADRs (`BP-ADR-NNN`)
> elaborate the solution chosen per problem in the problem catalog
> (`docs/blueprint/01-problem-catalog.md`). Where a blueprint ADR builds on or
> re-states an already-ratified architecture ADR, the related ADR is cited in
> the `Supersedes` / `Related` row of its header table.

## Index

| BP-ADR | Problem | Title | Status | Related ADR |
|--------|---------|-------|--------|-------------|
| BP-ADR-001 | [[P-001]] | Tor-only network path with per-site circuit isolation | PROPOSED | ADR-004 |
| BP-ADR-002 | [[P-002]] | DRM/KMS direct scanout, no display server | PROPOSED | ADR-001, ADR-002 |
| BP-ADR-003 | [[P-003]] | V4L2 stateful H.264 decoder in zero-copy DMA-BUF pipeline | PROPOSED | ADR-003, ADR-009 |
| BP-ADR-004 | [[P-004]] | boGCast unified protocol layer with three facades | PROPOSED | ADR-005 |
| BP-ADR-005 | [[P-005]] | Per-site circuit pinning with health-monitored re-resolution | PROPOSED | ADR-004 |
| BP-ADR-006 | [[P-006]] | Layered resolvers — in-tree fast paths plus yt-dlp long-tail | PROPOSED | ADR-008 |
| BP-ADR-007 | [[P-007]] | Single Manifest V3 codebase for Chrome and Firefox | PROPOSED | — |
| BP-ADR-008 | [[P-008]] | One-command installer plus first-boot web UI at bogdan.local | PROPOSED | — |
| BP-ADR-009 | [[P-009]] | DLNA MediaRenderer via gmediarender subprocess | PROPOSED | ADR-006 |
| BP-ADR-010 | [[P-010]] | Thermal supervisor with bitrate fallback above 80C | PROPOSED | — |
| BP-ADR-011 | [[P-011]] | Multi-room sync deferred to v2; leader-follower sketch recorded | DEFERRED | — |
| BP-ADR-012 | [[P-012]] | Keyboard-first accessible web UI with CI-enforced a11y checks | PROPOSED | — |

## Status legend

- **PROPOSED** — Solution chosen in rough draft; not yet validated by detailed design.
- **DEFERRED** — Out of scope for v1; sketch recorded for future phase.
- (ACCEPTED / REJECTED / DEPRECATED — see `docs/decisions/TEMPLATE.md`.)

## How to use this index

1. Each BP-ADR file is `NNN-<slug>.md` under this directory.
2. When a BP-ADR is ratified in the detailed-design phase, promote it to a
   numbered `docs/decisions/NNN-<slug>.md` ADR following the project convention
   and update `DECISIONS.md`. The blueprint file should then be marked
   DEPRECATED with a `Superseded by` pointer to the new ADR.
3. When a BP-ADR is overturned in detailed design, mark it REJECTED and record
   the rationale inline so future readers understand why.
