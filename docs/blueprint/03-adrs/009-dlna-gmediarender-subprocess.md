---
doc: adr
project: picast
version: 1
phase: adrs
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
adr: BP-ADR-009
problem: "[[P-009]]"
title: "DLNA MediaRenderer via gmediarender subprocess"
---
# BP-ADR-009: DLNA MediaRenderer via gmediarender subprocess

| Field        | Value          |
|--------------|----------------|
| **ID**       | BP-ADR-009        |
| **Status**   | PROPOSED       |
| **Date**     | 2026-07-30     |


| **Related** | ADR-006 (UPnP/DLNA MediaRenderer), BP-ADR-002 (DRM/KMS scanout), BP-ADR-004 (boGCast facades) |

## Context

Problem [[P-009]] requires boGDan to appear as a DLNA renderer on the network and to play media from MiniDLNA and Plex successfully. Many users have existing UPnP/DLNA servers; boGDan should act as a renderer so they can use it without changing their setup. ADR-006 already chose `gmediarender` as the DLNA renderer; this blueprint ADR elaborates how it integrates with the boGCast session state machine (BP-ADR-004) and how the known DRM master contention issue (BP-ADR-002) is handled.

## Decision

Run `gmediarender` as a subprocess managed by the boGDan session state machine. It advertises boGDan as a DLNA MediaRenderer via SSDP and accepts `SetAVTransportURI` calls, translating each into the same internal `Cast` command used by the HTTP path (per BP-ADR-004). The session state machine owns the lifecycle: it tears down `gmediarender` before constructing the playback pipeline, with a 500 ms grace window, and retries DRM master acquisition up to four times (2 s budget) before surfacing an error. The gmediarender process is pinned to a specific upstream commit and built reproducibly inside the Debian packaging step.

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ Zero-config interop with existing clients | VLC, Plex, MiniDLNA, Home Assistant all work via DLNA without a boGDan-specific sender |
| ✅ Mature renderer implementation | gmediarender is battle-tested for the DLNA renderer role; reimplementing only the MediaRenderer subset would be 2–3 weeks for no functional gain |
| ✅ Translation into existing Cast command | DLNA semantics map onto the same internal Cast command as HTTP, so BP-ADR-004's single-source-of-truth property holds |
| ✅ Reproducible build | Pinned upstream commit + reproducible Debian build means the C dependency is auditable |
| ❌ C dependency outside the Rust workspace | Complicates supply-chain review; mitigated by `cargo-deny` on the Rust graph independently and a documented C-dependency boundary in `docs/SECURITY.md` |
| ❌ DRM master contention on restart | gmediarender may hold DRM master when rendering, conflicting with the boGDan pipeline on restart — the README's known issue; mitigated by teardown grace window and retry budget |
| ❌ Subprocess lifecycle complexity | Must handle SIGCHLD, crash detection, restart, and clean teardown; adds ~150 lines to the session state machine |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **Native Rust DLNA stack (rupnp crate)** | DLNA spec is large and bug-prone; `rupnp` is incomplete for the MediaRenderer role; estimated 2–3 weeks to reach gmediarender parity for no functional gain |
| **Re-implement only the MediaRenderer subset in Rust** | Same estimate as above, same rejection; the DLNA state machine (AVTransport service) is the hard part and gmediarender already has it |
| **Run gmediarender in a separate systemd unit, not under boGDan control** | Rejected because DRM master contention becomes unmanageable without a single arbiter; the session state machine must own both gmediarender and the playback pipeline |
