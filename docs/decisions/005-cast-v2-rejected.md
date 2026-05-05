# ADR-005: Cast V2 Protocol Rejected

| Field        | Value          |
|--------------|----------------|
| **ID**       | ADR-005        |
| **Status**   | REJECTED       |
| **Date**     | 2025-01-18     |
| **Supersedes** | —            |
| **Superseded by** | —         |

## Context

Google Cast (Cast V2) is the protocol used by Chromecast devices. It allows users to "cast" content from Chrome browsers, YouTube apps, and other Cast-enabled applications directly to a receiver device. The protocol operates over TLS-encrypted WebSocket connections and supports both media URL relay and remote control (play, pause, seek, volume).

The appeal of Cast V2 for PiCast is obvious: billions of devices support it, and users are familiar with the "cast" button in Chrome and mobile apps. Supporting Cast V2 would make PiCast instantly usable from any Cast-enabled app.

However, Cast V2 has a critical limitation for unofficial receivers:

### Device Authentication

Google enforces **device authentication** on Cast V2. Official Chromecast devices have a cryptographic certificate provisioned at manufacture. When a sender (Chrome, YouTube app) discovers a Cast receiver via mDNS, it validates the receiver's certificate before establishing the Cast V2 session. Unofficial receivers cannot obtain these certificates.

Consequences of missing authentication:

- **Chrome's cast menu**: Unofficial receivers may appear in Chrome's cast menu via mDNS discovery, but the Cast V2 session handshake will fail when Chrome cannot validate the device certificate. Behavior varies across Chrome versions — sometimes the device appears briefly then disappears, sometimes it appears but connections fail silently.
- **Mobile apps**: The YouTube, Netflix, and Spotify mobile apps strictly validate certificates. Unofficial receivers never appear.
- **Fragility**: Even when a specific Chrome version tolerates unauthenticated receivers, the next Chrome update may break compatibility. This has happened repeatedly in the Cast V2 reverse-engineering community.

### Reverse-Engineering Status

The Cast V2 protocol has been partially reverse-engineered by projects like [pychromecast](https://github.com/home-assistant-libs/pychromecast) and [node-castv2](https://github.com/thibauts/node-castv2). These libraries can communicate with official Chromecast devices as senders but cannot impersonate receivers due to the certificate requirement.

## Decision

PiCast will not implement the Cast V2 protocol. The device authentication requirement makes it impossible to create a reliable, future-proof Cast V2 receiver. Instead, PiCast provides:

1. **DLNA MediaRenderer** (ADR-006) — Works with VLC, DLNA apps, and Home Assistant
2. **Browser extension** — Chrome Manifest V3 extension that intercepts media URLs from web pages and sends them to PiCast via the HTTP API
3. **HTTP API** — Direct URL submission for programmatic control

These alternatives provide the same user experience (click a button, video plays on TV) without depending on Google's certificate infrastructure.

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ No dependency on Google certificates | PiCast is fully self-contained; no proprietary auth tokens or certificate provisioning |
| ✅ Stable protocol surface | DLNA and HTTP are open standards with no single-vendor control |
| ✅ No Chrome version fragility | PiCast's browser extension uses standard webRequest API, not reverse-engineered Cast V2 |
| ❌ No native "Cast" button integration | Users cannot use the built-in Cast button in Chrome or YouTube; must use PiCast extension or DLNA app |
| ❌ No Cast V2 remote control | Pause/seek/volume from Cast senders won't work; PiCast provides its own control API |
| ❌ User education required | Users familiar with Chromecast must learn the PiCast extension or DLNA workflow |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **Cast V2 with auth bypass** | Requires exploiting Chrome's certificate validation; breaks on every Chrome update; not a sustainable or secure approach; violates Google's terms of service |
| **Shanocast** | Community project that mimics Chromecast mDNS discovery; suffers from the same certificate validation issues; unreliable across Chrome versions; not maintained at production quality |
| **DIAL-only** | DIAL (Discovery and Launch) is the discovery protocol that predates Cast V2; Google deprecated DIAL in favor of Cast V2; DIAL only launches apps, it doesn't provide media control; extremely limited functionality |
