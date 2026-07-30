---
doc: rough_draft
project: picast
version: 1
phase: rough_draft
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# boGDan Blueprint — Rough Draft

This rough draft proposes a concrete solution for each of the twelve problems catalogued in `docs/blueprint/01-problem-catalog.md`. It is intentionally concrete — referencing the existing ADRs, the GStreamer pipeline already described in the README, and the V4L2/Tor/DRM primitives already chosen — but it is not final: every entry carries an alternative that was rejected and a risk with a mitigation sketch, so that the next blueprint phase (detailed design) has a clear target to validate, refine, or overturn.

## Approach

The rough draft inherits the architectural commitments already ratified in `DECISIONS.md` (ADR-001 through ADR-009): all traffic through Tor with per-site circuit isolation, a zero-copy DMA-BUF pipeline from `v4l2h264dec` to `kmssink` with no display server, GStreamer as the media engine, yt-dlp as the long-tail resolver, and a single session state machine fronted by three protocol facades (HTTP REST, WebSocket, UPnP/DLNA). For each problem below, the proposed solution names the exact component that owns it, the alternative explains what else was on the table and why it lost, and the risk names the most plausible failure mode plus the first-pass mitigation. Nice-to-have problems (P-010, P-011, P-012) are scoped down or deferred rather than over-engineered, so that v1 ships on the must-have critical path.

## Problems Addressed

### [[P-001]] — Tor-only network path with per-site circuit isolation
- **Approach:** Route DNS, content resolution, and media fetch through a local Tor SOCKS5h proxy (`127.0.0.1:29050`) with `IsolateSOCKSAuth` enabled in `torrc`; the per-request SOCKS username encodes the destination host, so each site lands on a dedicated circuit. A local SOCKS5 forwarder pins the resolver's exit IP to the media-fetch client's exit IP, preventing CDN token mismatches when circuits rotate. Kernel-level `iptables` rules in `config/` block any non-Tor outbound traffic, and `scripts/verify-network-isolation.sh` runs `tcpdump` during a cast to assert zero non-Tor packets.
- **Alternative considered:** A VPN (Mullvad/WireGuard) was rejected because it shifts trust to a single operator that can correlate traffic across sites, requires account/payment linkage, and offers no stream isolation. I2P was rejected because its exit bandwidth is far below what 1080p streaming requires.
- **Risk:** Tor throughput (0.5–5 Mbps) is below the bitrate of some 1080p streams, so playback may stutter. Mitigation: progressive-download buffer with a CDN preflight (`GET Range: bytes=0-0`) to measure achievable throughput before committing to playback, plus a graceful fallback to a lower-bitrate variant when `bufferPercent` drops below 25%.

### [[P-002]] — DRM/KMS direct scanout, no display server
- **Approach:** Skip X11, Wayland, Chromium, and the Widevine CDM entirely; drive HDMI through DRM/KMS atomic modesetting on BCM2711 plane 0. Decoded frames are DMA-BUF file descriptors imported directly by `kmssink`, so the CPU never touches pixel data. This keeps total resident memory under 200 MB during 1080p60 playback and removes an entire frame copy per refresh.
- **Alternative considered:** A minimal Wayland compositor (sway or cage) was rejected because even a minimal compositor adds ~50 MB RAM, an extra frame copy, and the wlroots/libwayland attack surface. A full X11 stack was rejected outright as strictly worse on every axis that matters for an appliance.
- **Risk:** DRM master contention if another process (e.g. `gmediarender` for DLNA) has not released master when a new session starts — already listed as a known issue in the README. Mitigation: the session state machine serialises teardown before pipeline construction with a 500 ms grace window and a 2 s retry budget before surfacing the error to the user.

### [[P-003]] — V4L2 stateful H.264 decoder in a zero-copy DMA-BUF pipeline
- **Approach:** Use the V4L2 stateful M2M decoder (`v4l2h264dec`) in DMA-BUF export mode, fed by GStreamer `parsebin` which auto-detects container/codec and builds the decode chain in a pad-added callback. The bcm2835-ISP (`v4l2convert`) handles pixel-format conversion so the Hardware Video Scaler can scan out the buffer. Target: 1080p60 at < 50% CPU.
- **Alternative considered:** The V4L2 stateless decoder (`v4l2slh265dec`) as a unified path for both H.264 and HEVC was rejected for H.264 because the stateful API is more mature on BCM2711, needs no per-slice header parsing in userspace, and already meets the 1080p60 target. Software decode (`avdec_h264`) is kept only as a fallback for codecs the hardware cannot handle.
- **Risk:** HEVC content cannot use zero-copy today because the HVS cannot display SAND128 format; the experimental V3D compute shader (SAND→NV12) is unfinished. Mitigation: document HEVC as unsupported in v1 (per the README's "What boGDan Cannot Do" table) and revisit the V3D shader path in v2; do not advertise HEVC support.

### [[P-004]] — boGCast as a unified protocol layer with three facades
- **Approach:** Implement boGCast as a single session state machine exposed through three facades: HTTP REST on `:8585` for control, WebSocket on `:8586` for real-time events, and a UPnP/DLNA MediaRenderer (via `gmediarender`) for legacy clients. All three facades translate their inputs into the same internal `Cast` command so semantics stay consistent and the surface area for interop bugs is bounded.
- **Alternative considered:** HTTP-only was rejected because DLNA is the only zero-integration path to existing clients (VLC, Plex, MiniDLNA, Home Assistant). A custom binary protocol was rejected because it would force every sender to adopt a new SDK, killing the "one-click cast" goal of P-007.
- **Risk:** `gmediarender` is a C dependency outside the Rust workspace, complicating supply-chain review. Mitigation: pin to a specific upstream commit, build reproducibly inside the Debian packaging step, document the C-dependency boundary in `docs/SECURITY.md`, and run `cargo-deny` on the Rust crate graph independently.

### [[P-005]] — Per-site circuit pinning with health-monitored re-resolution
- **Approach:** Use a per-site SOCKS5 username that encodes the destination host so Tor's `IsolateSOCKSAuth` keeps each site on a dedicated circuit. The local SOCKS5 forwarder pins the resolver's exit IP to the media-fetch client's exit IP, preventing CDN IP-bound token mismatches when circuits rotate. The session monitors stream health and, on 5xx or timeout, re-resolves through a fresh circuit before failing.
- **Alternative considered:** A single shared circuit for all traffic was rejected because one site's traffic pattern would dominate and make cross-site traffic analysis easier. A custom Tor controller for fine-grained stream attachment (e.g. `arti-client`) was rejected as over-engineering for v1, since SOCKS auth isolation already covers the threat model.
- **Risk:** Circuit rotation mid-stream can cause a brief interruption while the new circuit builds, and CDN IP-bound signed URLs may 403 on the new exit. Mitigation: keep a 10 s rolling buffer ahead of the decode point; on 403, re-resolve via yt-dlp reusing the same pinned-exit username before failing the session; surface `circuit_rotations` in `/api/status` so the user can correlate dropouts.

### [[P-006]] — Layered resolvers: in-tree fast paths plus yt-dlp long-tail
- **Approach:** Layer two resolvers: custom in-tree resolvers for the highest-volume sites (YouTube, Vimeo, direct media links) tuned for low-latency Tor fetches, and yt-dlp as the long-tail fallback (1,800+ sites) invoked as a subprocess with `--proxy socks5h://127.0.0.1:29050`. All DNS goes through Tor's SOCKS5h — the trailing `h` forces remote resolution, preventing local DNS leaks.
- **Alternative considered:** Pure yt-dlp (no custom resolvers) was rejected because yt-dlp's general-purpose extractor adds 5–15 s of overhead per cast on sites where a 50-line custom resolver could resolve in under 2 s. Embedding yt-dlp as a Python library via PyO3 was rejected to avoid pulling a Python runtime into the appliance image.
- **Risk:** yt-dlp extractors break when sites change, and the project depends on upstream updates. Mitigation: ship a pinned yt-dlp commit in the Debian package, document `sudo yt-dlp -U` for users to update independently of boGDan releases, and fall back to direct-URL pass-through if yt-dlp returns no media URL.

### [[P-007]] — Single Manifest V3 codebase for Chrome and Firefox
- **Approach:** Ship one Manifest V3 codebase in `src/extension/` that builds for both Chrome and Firefox. The extension detects media URLs on the active tab via `chrome.tabs`/`browser.tabs` and DOM scraping for `<video>`/`<source>`, POSTs to `http://<pi-ip>:8585/api/cast`, and surfaces playback status over the WebSocket on `:8586`. Build-time browser polyfill normalises the `chrome.*` vs `browser.*` namespace gap.
- **Alternative considered:** Manifest V2 was rejected because both Google and Mozilla are sunsetting it. Two separate codebases were rejected as a maintenance burden with no upside. A PWA-installed sender page was rejected because it cannot intercept the active tab's media without manual copy/paste, breaking the one-click cast goal.
- **Risk:** Manifest V3 service workers can be evicted mid-cast, dropping the WebSocket. Mitigation: keep all cast state on the Pi (the extension is stateless); reconnect the WebSocket on service-worker wake using a `Last-Event-ID`-style resume so the UI re-syncs from `/api/status` without losing the in-progress session.

### [[P-008]] — One-command installer plus first-boot web UI at bogdan.local
- **Approach:** Ship `scripts/setup.sh` invoked via `curl | sudo bash` that installs the systemd unit, `torrc`, and `iptables` rules. On first boot, a web UI at `http://bogdan.local` (mDNS) handles Tor bridge selection, network config, and media source preferences without requiring SSH. Configuration persists to `/etc/bogdan/bogdan.toml` with environment-variable overrides for headless deployments.
- **Alternative considered:** A pre-built SD card image only was rejected because users with an existing Pi OS install then have no path. A TUI configurator was rejected because it requires SSH, which contradicts the zero-SSH success metric. A first-boot wizard baked into a custom Pi OS image was kept as a parallel option (the README already documents it) but not as the only path.
- **Risk:** `curl | bash` is a supply-chain attack vector if GitHub serving is compromised. Mitigation: serve the installer from a pinned commit SHA rather than `main`, ship a detached GPG signature alongside, and document the manual Debian-package install path as the verified-install alternative for paranoid users.

### [[P-009]] — DLNA MediaRenderer via gmediarender subprocess
- **Approach:** Run `gmediarender` as a subprocess managed by the session state machine. It advertises boGDan as a DLNA MediaRenderer via SSDP and accepts `SetAVTransportURI` calls, translating each into the same internal `Cast` command used by the HTTP path. This gives VLC, Plex, MiniDLNA, and Home Assistant zero-config interop.
- **Alternative considered:** A native Rust DLNA stack (e.g. the `rupnp` crate) was rejected because the DLNA spec is large and bug-prone; `gmediarender` is battle-tested for the renderer role. Re-implementing only the MediaRenderer subset was estimated at 2–3 weeks for no functional gain over the subprocess approach.
- **Risk:** `gmediarender` may hold DRM master when rendering, conflicting with the boGDan pipeline on restart — the "DRM master busy on restart" known issue. Mitigation: the session state machine tears down `gmediarender` before constructing the playback pipeline, with a 500 ms grace window; if DRM master is still held, the pipeline retries up to four times before surfacing an error.

### [[P-010]] — Thermal supervisor with bitrate fallback above 80°C
- **Approach:** Poll `/sys/class/thermal/thermal_zone0/temp` every 5 s from the playback supervisor. Above 75 °C emit a warning to `/api/status`; above 80 °C request a lower-bitrate variant from the resolver (preferring `itag=18` 360p over higher itags) and stretch the buffer window. Above 85 °C pause the pipeline and surface a user-visible "cooling down" state until temperature drops below 75 °C.
- **Alternative considered:** CPU frequency scaling via `cpufreq` governors was rejected because the actual heat source is the V4L2 decode block, not the CPU cores — scaling mainly affects the parser/audio path. Active fan control was considered out of scope for v1 because most Pi 4 cases ship with passive coolers and PWM fan wiring is hardware-specific.
- **Risk:** Lower-bitrate fallback degrades user-visible quality and may confuse users into thinking their network is the problem. Mitigation: surface a `thermal_throttled: true` field in `/api/status` and an OSD indicator; document the behaviour in `docs/USER_GUIDE.md` so the user understands the trade-off.

### [[P-011]] — Multi-room sync deferred to v2; leader-follower sketch recorded
- **Approach:** Defer multi-room sync to v2. For v1, document multi-room as unsupported and recommend one appliance per TV. The recorded v2 sketch: a leader appliance broadcasts PTP-style timestamps over the local WebSocket bus; follower appliances align their `appsrc` PTS offsets to the leader's clock, with a 100 ms tolerance window enforced by dropping or repeating frames at the queue boundary.
- **Alternative considered:** NTP-based clock sync was rejected because NTP's millisecond accuracy is insufficient for lip-sync (humans detect > 45 ms audio drift). Building the sync layer into v1 was rejected because the success metric is nice-to-have and would block the v1 release; the WebSocket-bus sketch is cheap to record now and revisit later.
- **Risk:** Even with PTP, Wi-Fi jitter on the LAN can blow the 100 ms budget, so the feature may end up ethernet-only in practice. Mitigation: ship behind a `multi_room` feature flag, clearly document the ethernet requirement in the user guide, and do not advertise the feature in v1 marketing.

### [[P-012]] — Keyboard-first accessible web UI with CI-enforced a11y checks
- **Approach:** Build the configuration web UI with semantic HTML, ARIA landmarks, and a keyboard-first interaction model (Tab / Shift-Tab / Enter / Escape cover all actions). High-contrast mode is a CSS variable theme toggled from the UI header. Automated checks run WAVE and axe-core in CI against every PR that touches `src/server/web/`.
- **Alternative considered:** Shipping a CLI-only config path and skipping the web UI accessibility work was rejected because the zero-SSH success metric (P-008) makes the web UI the only configuration surface for non-technical users. A third-party accessible admin panel (e.g. a Cockpit plugin) was rejected because it introduces a heavyweight dependency for a feature that fits in a few hundred lines of HTML.
- **Risk:** WAVE and axe-core passing does not guarantee real screen-reader usability; the success metric may give false confidence. Mitigation: schedule one manual NVDA + VoiceOver test pass before the v1 release, document known issues in an `a11y.md` file, and tag accessibility regressions as release-blocking in the issue tracker.
