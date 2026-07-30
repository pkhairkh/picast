---
doc: adr
project: picast
version: 1
phase: adrs
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
adr: BP-ADR-004
problem: "[[P-004]]"
title: "boGCast unified protocol layer with three facades"
---
# BP-ADR-004: boGCast unified protocol layer with three facades

| Field        | Value          |
|--------------|----------------|
| **ID**       | BP-ADR-004        |
| **Status**   | PROPOSED       |
| **Date**     | 2026-07-30     |


| **Related** | ADR-005 (Cast V2 protocol rejected) |

## Context

Problem [[P-004]] requires HTTP REST, WebSocket, and UPnP/DLNA to all pass conformance tests and at least two third-party casting clients to work without modification. Implementing all three protocols correctly and interoperably is complex; doing it three times in three different code paths would be a maintenance and correctness disaster. ADR-005 already rejected Google Cast V2 — so boGDan must define its own protocol surface (boGCast) and expose it through three facades.

## Decision

boGCast is implemented as a single session state machine exposed through three facades: HTTP REST on `:8585` for control, WebSocket on `:8586` for real-time events, and a UPnP/DLNA MediaRenderer (via `gmediarender` — see BP-ADR-009) for legacy clients. All three facades translate their inputs into the same internal `Cast` command, so semantics stay consistent and the surface area for interop bugs is bounded to a single translation layer per facade. The session state machine owns cast lifecycle, queue, and playback state; facades are stateless translators.

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ Single source of truth for cast semantics | All three protocols behave identically; bugs are fixed once in the state machine |
| ✅ Interop with existing clients | VLC, Plex, MiniDLNA, Home Assistant work via DLNA; browsers and curl work via HTTP; browser extension uses both HTTP and WebSocket |
| ✅ Testable surface | Facade→state-machine translation is unit-testable per protocol; conformance tests can drive each facade independently |
| ✅ Extensible | Adding a fourth facade (e.g. MQTT for home automation) is a translation layer, not a re-implementation of cast logic |
| ❌ Translation layer overhead | Each facade must map its protocol's vocabulary onto the internal Cast command; mismatched capabilities (e.g. DLNA has no 'queue insert at position') require careful design |
| ❌ gmediarender C dependency | DLNA facade goes through a C subprocess, complicating supply-chain review — see BP-ADR-009 for mitigation |
| ❌ Conformance test burden | Three protocols means three conformance suites (HTTP curl, WebSocket wscat, DLNA gupnp-universal-cp) must all pass per CI run |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **HTTP-only** | Rejected because DLNA is the only zero-integration path to existing clients (VLC, Plex, MiniDLNA, Home Assistant); without DLNA the interop success metric of P-004 is unachievable |
| **Custom binary protocol over WebSocket** | Rejected because it forces every sender to adopt a new SDK, killing the one-click cast goal of BP-ADR-007; no existing client would work without a bespoke sender |
| **Re-implement Cast V2 (Chromecast protocol)** | Rejected by ADR-005: Google enforces device authentication and unofficial receivers cannot complete the handshake |
