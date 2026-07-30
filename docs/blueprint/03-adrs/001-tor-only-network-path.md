# BP-ADR-001: Tor-only network path with per-site circuit isolation

| Field        | Value          |
|--------------|----------------|
| **ID**       | BP-ADR-001        |
| **Status**   | PROPOSED       |
| **Date**     | 2026-07-30     |


| **Related** | ADR-004 (C Tor daemon over arti) |

## Context

Problem [[P-001]] in the problem catalog requires that the ISP cannot see which URLs the user casts. boGDan's value proposition is privacy, so any non-Tor outbound packet is a regression. Tor's C daemon already supports `IsolateSOCKSAuth` (see ADR-004), which gives us per-site circuit isolation for free if we encode the destination host into the SOCKS5 username. The remaining gap is (a) closing every other path off the box, and (b) ensuring the resolver's exit IP matches the media-fetcher's exit IP so CDN IP-bound signed URLs do not 403 on circuit rotation.

## Decision

All outbound network traffic from the appliance traverses the local Tor SOCKS5h proxy at `127.0.0.1:29050`, with `IsolateSOCKSAuth` enabled in `torrc`. The `bogdan-tor` crate derives a per-host SOCKS5 username (SHA-256 of the destination hostname, first 16 hex chars) so each site lands on a dedicated circuit. A local SOCKS5 forwarder pins the resolver's exit IP to the media-fetch client's exit IP — the same per-host username is used for both resolution and fetch, so Tor reuses the same circuit. DNS is forced through Tor via the `h` suffix on `socks5h://`, preventing local DNS leakage. Kernel `iptables` rules shipped in `config/` drop any non-Tor outbound traffic at the firewall, and `scripts/verify-network-isolation.sh` runs `tcpdump` during a cast and fails the test if any non-Tor packet is observed.

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ Zero non-Tor network traffic | `tcpdump` during a cast shows only Tor connections; matches P-001 success metric |
| ✅ Per-site circuit isolation | Different media hosts never share an exit; ISP and exit node cannot correlate cross-site viewing |
| ✅ CDN token continuity | Resolver and fetcher share an exit IP, so IP-bound signed URLs stay valid across circuit rotations |
| ✅ DNS leak prevention | All resolution is remote via socks5h; the appliance never asks a local resolver |
| ❌ Throughput ceiling | Tor bandwidth (0.5–5 Mbps) is below the bitrate of some 1080p streams; playback may stutter and require bitrate fallback (BP-ADR-010) |
| ❌ Latency on cold circuit | First-byte latency on a new circuit can be 1–3 s; progressive-download buffer must hide this from the user |
| ❌ iptables fragility | Mis-ordered iptables rules could silently allow leaks; verify-network-isolation.sh is the safety net, not a guarantee |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **VPN (Mullvad / WireGuard)** | Shifts trust to a single operator that can correlate traffic across all sites; requires account/payment linkage; offers no per-site stream isolation; reduces the privacy threat model to 'trust the VPN' rather than 'trust no one' |
| **I2P** | I2P's exit bandwidth is far below what 1080p streaming requires; I2P is designed for in-network eepsites, not for exit-to-clearnet streaming |
| **Hybrid (Tor for resolution, direct fetch for media)** | Defeats the purpose: the CDN — and therefore the ISP — sees the media fetch, which is exactly the data the user wants to keep private |
