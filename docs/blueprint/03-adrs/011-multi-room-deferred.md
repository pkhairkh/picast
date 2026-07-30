---
doc: adr
project: picast
version: 1
phase: adrs
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
adr: BP-ADR-011
problem: "[[P-011]]"
title: "Multi-room sync deferred to v2; leader-follower sketch recorded"
---
# BP-ADR-011: Multi-room sync deferred to v2; leader-follower sketch recorded

| Field        | Value          |
|--------------|----------------|
| **ID**       | BP-ADR-011        |
| **Status**   | DEFERRED       |
| **Date**     | 2026-07-30     |


| **Related** | BP-ADR-004 (boGCast facades — sync bus would ride the WebSocket facade) |

## Context

Problem [[P-011]] requires two boGDan appliances to play the same media with < 100 ms audio offset. Multi-room sync needs clock synchronisation and buffer management across appliances. P-011 is nice-to-have — power users with multiple appliances are currently unsupported, and the v1 release should not block on this feature. However, recording the design sketch now is cheap and de-risks a v2 implementation.

## Decision

Defer multi-room sync to v2. For v1, document multi-room as unsupported and recommend one appliance per TV. The recorded v2 sketch: a leader appliance broadcasts PTP-style timestamps over the local WebSocket bus (BP-ADR-004's WebSocket facade, extended for inter-appliance traffic); follower appliances align their `appsrc` PTS offsets to the leader's clock, with a 100 ms tolerance window enforced by dropping or repeating frames at the queue boundary. The feature would ship behind a `multi_room` feature flag and require an ethernet backhaul (Wi-Fi jitter can blow the 100 ms budget).

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ v1 ships on the must-have critical path | Deferral removes a complex feature from the v1 release; multi-room is nice-to-have per P-011 |
| ✅ Design sketch is recorded for v2 | Future implementers have a starting point; PTP-over-WebSocket approach is documented with its ethernet-only constraint |
| ✅ No v1 complexity cost | v1 does not carry clock-sync, leader-election, or inter-appliance protocol code |
| ✅ Feature flag for v2 | When implemented, the `multi_room` flag keeps v1 deployments unaffected |
| ❌ Power users with multiple appliances unsupported in v1 | Users with multiple TVs must run independent sessions per TV; documented as a known limitation |
| ❌ v2 implementation risk remains | Even with PTP, Wi-Fi jitter on the LAN can blow the 100 ms budget; the feature may end up ethernet-only in practice |
| ❌ Sketch may be overtaken by v2 requirements | By the time v2 starts, the WebSocket facade (BP-ADR-004) may have evolved; the sketch is a starting point, not a contract |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **NTP-based clock sync for v1** | Rejected because NTP's millisecond accuracy is insufficient for lip-sync (humans detect > 45 ms audio drift); would give false confidence without meeting the success metric |
| **Build the sync layer into v1** | Rejected because P-011 is nice-to-have and would block the v1 release; the WebSocket-bus sketch is cheap to record now and revisit later |
| **Use PulseAudio network streaming for audio sync** | Rejected because it solves only audio, not video; and pulls a heavyweight audio daemon into the appliance image, contradicting BP-ADR-002's memory target |
