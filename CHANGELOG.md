# Changelog

All notable changes to boGDan will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
