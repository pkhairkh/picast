# boGDan v0.1.0-alpha Release Checklist

## Pre-Release Verification

### Build & Test
- [ ] `cargo check --workspace` passes
- [ ] `cargo test --workspace` passes (632+ tests)
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` passes
- [ ] `cargo audit` shows no critical vulnerabilities
- [ ] Cross-compile for `aarch64-unknown-linux-gnu` succeeds

### Sprint DoD Verification
- [ ] Sprint 1 DoD: Provider config-driven, DeobfuscationPipeline trait, no VOE_DOMAINS in custom.rs
- [ ] Sprint 2 DoD: Deobfuscation step tests, mock HTTP server tests, cache integration
- [ ] Sprint 3 DoD: DRM device open, plane enumeration, atomic modesetting, GBM surface (mock on x86)
- [ ] Sprint 4 DoD: Mock playback with CDN error simulation, download progress, buffer health
- [ ] Sprint 5 DoD: Extension CSP, rate limiting (429), WebSocket resilience, DLNA auto-restart
- [ ] Sprint 6 DoD: Integration tests (25+), security audit, QA scripts, SECURITY.md
- [ ] Sprint 7 DoD: Debian package, user guide, security hardening guide, SD image script

### Artifacts
- [ ] `bogdan_0.1.0_arm64.deb` — built with `packaging/build-deb.sh`
- [ ] `bogdan_0.1.0_arm64.deb.sha256` — SHA-256 checksum
- [ ] `bogdan-0.1.0-pi4-arm64.img.xz` — pre-built SD card image (if available)
- [ ] `bogdan-0.1.0-pi4-arm64.img.xz.sha256` — SHA-256 checksum
- [ ] `bogdan-server` — bare aarch64 binary (optional)
- [ ] `bogdan-chrome-0.3.0.zip` — Chrome extension package
- [ ] `bogdan-firefox-0.3.0.zip` — Firefox extension package

### Documentation
- [ ] README.md quick-start is accurate and tested
- [ ] docs/USER_GUIDE.md covers installation, configuration, troubleshooting, FAQ
- [ ] docs/SECURITY.md covers all 11 sections including attack surface and physical security
- [ ] docs/SECURITY_AUDIT.md is current
- [ ] CHANGELOG.md updated for v0.1.0-alpha
- [ ] All documentation matches current code behavior

### Security
- [ ] `cargo audit` — no unfixed critical/high vulnerabilities
- [ ] No `unwrap()`/`expect()` in production HTTP handler paths
- [ ] iptables rules tested (verify-network-isolation.sh passes on Pi)
- [ ] TLS certificates generated with `generate-certs.sh` work in browser
- [ ] Extension CSP is valid and no innerHTML usage
- [ ] Rate limiting returns 429 after 30 requests/10s

## Release Process

1. **Create release branch**
   ```bash
   git checkout -b release/0.1.0-alpha main
   ```

2. **Update version numbers**
   - `Cargo.toml` workspace version → `0.1.0`
   - `packaging/debian/control` Version → `0.1.0`
   - `packaging/build-deb.sh` PACKAGE_VERSION → `0.1.0`
   - `scripts/setup.sh` BOGDAN_SETUP_VERSION → `0.1.0`
   - `src/extension/manifest*.json` version → `0.3.0`

3. **Build on Pi 4** (or cross-compile)
   ```bash
   cargo build --release --features hw
   ```

4. **Build .deb package**
   ```bash
   bash packaging/build-deb.sh
   ```

5. **Build SD card image** (requires Pi OS base image)
   ```bash
   sudo bash packaging/build-image.sh --deb packaging/build/bogdan_0.1.0_arm64.deb --compress
   ```

6. **Build extension packages**
   ```bash
   cd src/extension && bash build.sh
   ```

7. **Compute all checksums**
   ```bash
   sha256sum packaging/build/bogdan_0.1.0_arm64.deb > checksums.txt
   sha256sum packaging/build/bogdan-0.1.0-pi4-arm64.img.xz >> checksums.txt
   ```

8. **Tag and push**
   ```bash
   git tag -a v0.1.0-alpha -m "boGDan v0.1.0-alpha — First public alpha release"
   git push origin v0.1.0-alpha
   ```

9. **Create GitHub Release**
   - Title: `boGDan v0.1.0-alpha`
   - Tag: `v0.1.0-alpha`
   - Body: See release notes below
   - Attachments: .deb, .sha256, .img.xz, checksums.txt, extension zips

10. **Update README**
    - Add link to GitHub Release
    - Update download URLs

## Post-Release
- [ ] Verify GitHub Release page has all artifacts
- [ ] Verify `sha256sum -c checksums.txt` passes for all downloads
- [ ] Test install on fresh Pi OS Lite: `dpkg -i` → `systemctl start bogdan` → cast from extension
- [ ] Test SD card image: flash → boot → cast from extension
- [ ] Announce release
