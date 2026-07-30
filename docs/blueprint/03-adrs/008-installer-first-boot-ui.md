# BP-ADR-008: One-command installer plus first-boot web UI at bogdan.local

| Field        | Value          |
|--------------|----------------|
| **ID**       | BP-ADR-008        |
| **Status**   | PROPOSED       |
| **Date**     | 2026-07-30     |




## Context

Problem [[P-008]] requires one-command install (`curl | bash`), a web UI for configuration accessible at `http://bogdan.local`, and zero SSH required for normal operation. Raspberry Pi appliances must be easy to set up — complex setup is a barrier to adoption. The user base includes privacy-conscious non-technical users who cannot be expected to SSH in to edit `/etc/bogdan/bogdan.toml`.

## Decision

Ship `scripts/setup.sh` invoked via `curl | sudo bash` that installs the systemd unit, `torrc`, `iptables` rules, and the boGDan binary. On first boot, a web UI at `http://bogdan.local` (resolved via mDNS / Avahi) handles Tor bridge selection, network config, and media source preferences without requiring SSH. Configuration persists to `/etc/bogdan/bogdan.toml` with environment-variable overrides for headless deployments. A pre-built SD card image and a Debian package are kept as parallel install paths (already documented in the README) for users who want a verified-install alternative to `curl | bash`.

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ One-command install | `curl | sudo bash` path matches P-008 success metric for users with an existing Pi OS install |
| ✅ Zero-SSH configuration | Web UI at `bogdan.local` covers Tor bridges, network, and sources; non-technical users never touch a shell |
| ✅ Multiple install paths | curl-install, Debian package, and pre-built SD image cover different user preferences and trust models |
| ✅ mDNS discoverability | `bogdan.local` works on most home networks without configuration; Avahi is already part of Pi OS Lite |
| ❌ `curl | bash` supply-chain risk | If GitHub serving is compromised, the installer could be tampered; mitigated by pinning to a commit SHA and shipping a detached GPG signature |
| ❌ mDNS not always reliable | Some routers disable mDNS; users on those networks must fall back to the Pi's IP address |
| ❌ Web UI attack surface | A configuration web UI is an attack surface; mitigated by binding to the LAN interface only and documenting TLS setup in the user guide |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **Pre-built SD card image only** | Rejected because users with an existing Pi OS install then have no path; also requires the user to flash an SD card, which is a higher barrier than `curl | bash` |
| **TUI configurator over SSH** | Rejected because it requires SSH, contradicting the zero-SSH success metric of P-008 |
| **First-boot wizard baked into a custom Pi OS image (no curl path)** | Kept as a parallel option but rejected as the only path; users who already have a Pi running something else would have to reflash |
