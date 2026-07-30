---
doc: adr
project: picast
version: 1
phase: adrs
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
adr: BP-ADR-005
problem: "[[P-005]]"
title: "Per-site circuit pinning with health-monitored re-resolution"
---
# BP-ADR-005: Per-site circuit pinning with health-monitored re-resolution

| Field        | Value          |
|--------------|----------------|
| **ID**       | BP-ADR-005        |
| **Status**   | PROPOSED       |
| **Date**     | 2026-07-30     |


| **Related** | ADR-004 (C Tor daemon over arti), BP-ADR-001 (Tor-only path) |

## Context

Problem [[P-005]] requires that a media stream survives a Tor circuit rotation without > 5 s interruption and that a failed circuit is replaced within 10 s. Tor circuits are designed for short-lived web browsing; media sessions can last hours. Two failure modes must be handled: (a) circuit rotation mid-stream causing a brief interruption, and (b) CDN IP-bound signed URLs returning 403 when the exit IP changes. BP-ADR-001 established the per-host SOCKS username mechanism; this ADR defines the session-layer health monitoring and re-resolution that runs on top.

## Decision

Per-site SOCKS5 username (SHA-256 hash of hostname, first 16 hex chars) is used for both the resolver request and the media-fetch client, so Tor's `IsolateSOCKSAuth` pins the two onto the same circuit. The session monitors stream health on the GStreamer bus and on `reqwest` HTTP errors. On a 5xx response, timeout, or stalled `appsrc`, the session: (1) keeps the 10 s rolling buffer ahead of the decode point to mask the interruption; (2) re-resolves the URL through yt-dlp reusing the same per-host username (so Tor picks the same circuit if it's still alive, or builds a new one with a fresh exit if not); (3) if the re-resolved URL returns 403 on the new exit, fails the session gracefully and surfaces the error to the user. `/api/status` exposes `circuit_rotations` and `buffer_percent` so the user can correlate dropouts.

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ Stream survives circuit rotation | 10 s rolling buffer masks the brief interruption while a new circuit builds; matches P-005 success metric |
| ✅ Automatic recovery within 10 s | On failure, re-resolution through the same pinned exit (or a new one) completes within the 10 s budget |
| ✅ Observable circuit health | `/api/status` exposes rotation count and buffer level so users can correlate dropouts and the team can spot flaky exits |
| ✅ CDN token continuity in the common case | Same per-host username means same circuit means same exit IP, so IP-bound signed URLs stay valid |
| ❌ 403 on exit change is unrecoverable without re-resolution | When Tor rotates the exit, IP-bound CDN tokens can 403; the only mitigation is full re-resolution through yt-dlp, which adds 5–15 s latency |
| ❌ Buffer grows memory footprint | 10 s rolling buffer at 4 Mbps is ~5 MB; acceptable, but must be accounted for in the < 200 MB RAM target of BP-ADR-002 |
| ❌ Re-resolution cost on hot path | If Tor is unhealthy, every re-resolution spawns a yt-dlp subprocess (5–15 s, see ADR-008); rapid re-resolution thrashes the system |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **Single shared circuit for all traffic** | Rejected because one site's traffic pattern would dominate and make cross-site traffic analysis easier; also reduces throughput because all sites compete for one circuit's bandwidth |
| **Custom Tor controller for fine-grained stream attachment (arti-client)** | Over-engineering for v1; SOCKS auth isolation already covers the threat model; arti lacks IsolateSOCKSAuth anyway (see ADR-004) |
| **No re-resolution — fail fast on 403** | Rejected because it gives the user a hard failure on every circuit rotation, which on a long movie could happen multiple times; the UX of 'retry automatically' is materially better |
