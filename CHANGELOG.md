# Changelog

All notable changes to boGDan will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Implemented
- Full implementation of all 7 crates: bogdan-tor, bogdan-display, bogdan-playback, bogdan-resolver, bogdan-session, bogdan-protocols, bogdan-server
- Tor daemon lifecycle management with SOCKS5 stream isolation (SHA-256 hostname hashing)
- DRM/KMS direct mode-setting with GBM zero-copy buffer allocation
- GStreamer playback pipeline: souphttpsrc → queue2 → parsebin → V4L2 → kmssink
- yt-dlp subprocess integration with H.264 format selection and Tor proxying
- Custom resolvers for Voe, DoodStream, and other streaming providers
- Session state machine with CDN retry logic and SQLite WAL-mode persistence
- HTTP REST API (11 endpoints), WebSocket event stream, DLNA MediaRenderer
- SOCKS5 forwarder for Tor circuit isolation between resolver and media fetcher
- Browser extension (Manifest V3) with popup UI
- Configuration system (TOML + env vars + defaults)
- Deploy script for Raspberry Pi 4B+ (`deploy.sh`)
- 7 development sprints complete (see TASKS.md)
- 14 code reviews in docs/blueprint/ (review-01 through review-14)

### Fixed
- SOCKS5 greeting: offer only username/password auth (0x02) to guarantee Tor circuit isolation
- Sec-Fetch-* headers added to souphttpsrc for CDN anti-bot compatibility
- Added missing Voe front-end domains to resolver domain list
- Buffering mode: 100MB buffer with 80% fill threshold for Tor throughput smoothing
- PulseAudio sink support for Bluetooth audio output
- parsebin lookup by factory name instead of hardcoded element name
- Rate-limited buffering logs to prevent SD card I/O overflow
- Bluetooth audio auto-detection with BlueALSA support
- Proactive CDN IP mismatch detection before playback starts
- False "NO video pad linked" alarm eliminated

### Added
- Project scaffold: Rust workspace with 7 crates (bogdan-server, bogdan-protocols, bogdan-session, bogdan-resolver, bogdan-playback, bogdan-display, bogdan-tor)
- Architecture documentation (ARCHITECTURE.md, SPECIFICATION.md, DECISIONS.md)
- Individual ADR files (ADR-001 through ADR-009) in docs/decisions/
- Per-module documentation in docs/ (hardware, protocols, playback, tor, extension)
- Browser extension skeleton (Manifest V3, background.js, popup)
- Configuration files (torrc, iptables.rules, bogdan.service)
- Pi setup script (scripts/setup.sh)
- Development roadmap (docs/ROADMAP.md)
- Technical glossary (docs/GLOSSARY.md)
- CI/CD pipeline (GitHub Actions: check, test, lint, cross-build, audit)
- Release workflow (GitHub Actions: aarch64 binary, checksums, GitHub Release)
- Issue templates (bug, feature, task)
- PR template with crate-specific checklist
- Dependabot configuration (Cargo + GitHub Actions)
- Code owners file
- CONTRIBUTING.md with development workflow
- SECURITY.md with vulnerability reporting policy
- MIT LICENSE
- Makefile with 12 developer targets
- Cross-compilation config (.cargo/config.toml for aarch64)
- Code quality configs (rustfmt.toml, clippy.toml, deny.toml)
- EditorConfig for consistent formatting
- Unit tests in all 7 crates
- Integration test stubs (HTTP API, resolver, playback)
- AGENT.md with AI agent workflow instructions
- GitHub Copilot instructions (.github/copilot-instructions.md)
