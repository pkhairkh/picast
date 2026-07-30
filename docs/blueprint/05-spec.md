---
doc: spec
project: picast
version: 1
phase: spec
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# boGDan Blueprint — Specification

This specification defines normative requirements for boGDan v1. Each requirement has a stable ID ([[R-NNN]]), traces to one or more problems ([[P-NNN]]) from `docs/blueprint/01-problem-catalog.md`, and is paired with one or more acceptance criteria written in Given/When/Then form so that any criterion can be verified by a test in the test strategy of `docs/blueprint/04-fine-draft.md`. RFC 2119 keywords (MUST, MUST NOT, SHOULD, MAY) are used in their normative sense. Requirements are grouped by problem; every problem in the catalog is addressed by at least one requirement.

Conventions:
- [[P-NNN]] — problem from the catalog
- [[R-NNN]] — requirement (this document)
- [[BP-ADR-NNN]] — blueprint ADR
- [[ADR-NNN]] — ratified project ADR
- [[C-NNN]] — component from the fine draft
- [[T-NNN]] — open question / task from the fine draft

## Requirement Status

| ID | Problem | Title | Priority | Status |
|----|---------|-------|----------|--------|
| [[R-001]] | [[P-001]] | Tor-only outbound traffic | must | proposed |
| [[R-002]] | [[P-001]] | Per-site Tor circuit isolation | must | proposed |
| [[R-003]] | [[P-001]] | DNS leak prevention | must | proposed |
| [[R-004]] | [[P-002]] | No display server process | must | proposed |
| [[R-005]] | [[P-002]] | Memory budget under 200 MB at 1080p | must | proposed |
| [[R-006]] | [[P-003]] | Hardware H.264 decode at 1080p60 | must | proposed |
| [[R-007]] | [[P-003]] | Zero-copy DMA-BUF pipeline | must | proposed |
| [[R-008]] | [[P-004]] | HTTP REST API conformance | must | proposed |
| [[R-009]] | [[P-004]] | WebSocket event conformance | must | proposed |
| [[R-010]] | [[P-004]] | DLNA MediaRenderer conformance | should | proposed |
| [[R-011]] | [[P-004]] | Third-party client interop | should | proposed |
| [[R-012]] | [[P-005]] | Stream survives circuit rotation | must | proposed |
| [[R-013]] | [[P-005]] | Automatic circuit replacement within 10 s | must | proposed |
| [[R-014]] | [[P-006]] | YouTube URL resolution within 10 s | must | proposed |
| [[R-015]] | [[P-006]] | Multi-source resolver coverage | must | proposed |
| [[R-016]] | [[P-007]] | Chrome extension one-click cast | should | proposed |
| [[R-017]] | [[P-007]] | Firefox extension one-click cast | should | proposed |
| [[R-018]] | [[P-008]] | One-command installer | must | proposed |
| [[R-019]] | [[P-008]] | Zero-SSH web configuration | must | proposed |
| [[R-020]] | [[P-008]] | mDNS discoverability at bogdan.local | should | proposed |
| [[R-021]] | [[P-009]] | DLNA renderer discovery via SSDP | should | proposed |
| [[R-022]] | [[P-009]] | MiniDLNA / Plex media playback | should | proposed |
| [[R-023]] | [[P-010]] | Thermal threshold enforcement | nice | proposed |
| [[R-024]] | [[P-010]] | Bitrate fallback above 80 C | nice | proposed |
| [[R-025]] | [[P-011]] | Multi-room sync deferred to v2 | nice | deferred |
| [[R-026]] | [[P-012]] | Web UI WAVE conformance | nice | proposed |
| [[R-027]] | [[P-012]] | Keyboard-only navigation | nice | proposed |
| [[R-028]] | [[P-012]] | High-contrast mode | nice | proposed |

## Normative Requirements

### [[R-001]] Tor-only outbound traffic

**Problem:** [[P-001]] — ISP surveillance of media viewing habits.
**Decision:** [[BP-ADR-001]].
**Priority:** must.

The boGDan appliance MUST route all outbound network traffic through the local Tor SOCKS5h proxy at `127.0.0.1:29050`. The appliance MUST NOT initiate any direct (non-Tor) outbound TCP connection to a host outside its local network, with the sole exception of the Tor daemon's own connection to its guard relay. The appliance MUST enforce this at the kernel level via `iptables` rules shipped in `config/iptables.rules` and applied at boot.

**Acceptance criteria:**

- Given a boGDan appliance playing a 1080p H.264 stream from an internet source, when `tcpdump -i any 'not port 9001 and not port 9030 and not port 9050'` is run for the duration of a 60-second cast, then zero packets MUST be captured (Tor ORPort is 9001/9030; the local SOCKS port 9050 is loopback-only and exempt).
- Given a boGDan appliance at rest (idle, no cast in progress), when `tcpdump` is run for 30 seconds, then only Tor daemon keepalive traffic to its guard relay MUST be observed.
- Given the `iptables` rules from `config/iptables.rules` are loaded, when a non-Tor process attempts `curl https://example.com`, then the connection MUST be dropped at the firewall and `dmesg` MUST show a DROP log entry.
- The script `scripts/verify-network-isolation.sh` MUST exit 0 on a healthy appliance and MUST exit non-zero with a clear error message on any leak.

### [[R-002]] Per-site Tor circuit isolation

**Problem:** [[P-001]] — ISP surveillance (correlation dimension).
**Decision:** [[BP-ADR-001]], [[BP-ADR-005]], [[ADR-004]].
**Priority:** must.

The boGDan appliance MUST use a distinct Tor circuit for each destination hostname. The circuit MUST be selected by encoding the destination hostname (or a deterministic hash thereof) into the SOCKS5 username passed to the Tor SOCKS5 proxy, leveraging Tor's `IsolateSOCKSAuth` feature. The same SOCKS5 username MUST be used for both the URL resolution request and the subsequent media fetch request for the same hostname, so the two requests share a circuit and therefore an exit IP.

**Acceptance criteria:**

- Given two casts to `youtube.com` and `vimeo.com` in succession, when the Tor control port is queried for circuit state, then the two casts MUST have used circuits with different exit node fingerprints.
- Given a single cast to `youtube.com` involving both a yt-dlp resolution request and a media fetch request, when the Tor control port is queried, then both requests MUST have used the same exit node fingerprint.
- The function `TorProxy::username_for_host(host)` ([[C-005]]) MUST be deterministic: the same input hostname MUST always produce the same output username, across process restarts.
- The function `TorProxy::username_for_host(host)` MUST be collision-resistant: two distinct hostnames MUST produce distinct usernames (probabilistic test over 10,000 generated hostnames).

### [[R-003]] DNS leak prevention

**Problem:** [[P-001]] — ISP surveillance (DNS dimension).
**Decision:** [[BP-ADR-001]], [[BP-ADR-006]].
**Priority:** must.

The boGDan appliance MUST NOT perform any DNS resolution via the local system resolver. All hostname resolution MUST be delegated to the Tor SOCKS5 proxy by using the `socks5h://` URL scheme (the trailing `h` forces remote resolution). The appliance MUST NOT call `getaddrinfo()` for any internet hostname.

**Acceptance criteria:**

- Given a boGDan appliance resolving a YouTube URL, when `tcpdump 'port 53'` is run during the resolution, then zero DNS queries to a non-loopback address MUST be observed.
- Given the `bogdan-resolver` crate's HTTP client configuration, when inspected, then the proxy URL MUST be `socks5h://127.0.0.1:29050` (not `socks5://`).
- The CI grep `rg 'getaddrinfo|socks5://' src/` MUST return zero matches outside of test fixtures.

### [[R-004]] No display server process

**Problem:** [[P-002]] — DRM and display server overhead.
**Decision:** [[BP-ADR-002]], [[ADR-001]], [[ADR-002]].
**Priority:** must.

The boGDan appliance MUST NOT run any X11 server, Wayland compositor, or window manager process. The appliance MUST drive HDMI directly via DRM/KMS atomic modesetting on BCM2711 plane 0. The `bogdan-display` crate ([[C-006]]) MUST be the sole holder of DRM master for the appliance's lifetime.

**Acceptance criteria:**

- Given a boGDan appliance playing a 1080p stream, when `pgrep -a 'Xorg|Xwayland|weston|sway|cage|gnome-shell|kwin'` is run, then zero matches MUST be returned.
- Given a boGDan appliance playing a 1080p stream, when `fuser /dev/dri/card0` is run, then only the `bogdan-display` process MUST be listed (with the brief exception of `gmediarender` during DLNA teardown, per [[R-021]]).
- The systemd unit file `config/bogdan.service` MUST NOT declare any dependency on `display-manager.service`, `gdm.service`, or any X11/Wayland unit.

### [[R-005]] Memory budget under 200 MB at 1080p

**Problem:** [[P-002]] — DRM and display server overhead (memory dimension).
**Decision:** [[BP-ADR-002]], [[BP-ADR-003]].
**Priority:** must.

The boGDan appliance's total resident memory (RSS) during 1080p60 H.264 playback MUST NOT exceed 200 MB, excluding the Linux kernel and page cache. This budget MUST include the boGDan process, the Tor daemon, the `gmediarender` subprocess (if running), and any yt-dlp subprocess (transient, during resolution only).

**Acceptance criteria:**

- Given a boGDan appliance playing a 1080p60 H.264 stream for 60 seconds, when `ps -eo rss --no-headers | awk '{s+=$1} END {print s/1024}'` is run, then the total RSS MUST be ≤ 200 MB (with a 10 MB tolerance for measurement noise; the test asserts ≤ 210 MB).
- Given the same scenario, when `free -m` is run, then `used` (excluding `buff/cache`) MUST be ≤ 200 MB.
- The nightly hardware-in-the-loop test `tests/hw_1080p60.rs` MUST fail the build if RSS exceeds 210 MB at any sample point during the 60-second playback.

### [[R-006]] Hardware H.264 decode at 1080p60

**Problem:** [[P-003]] — Lack of hardware-accelerated video decoding pipeline.
**Decision:** [[BP-ADR-003]], [[ADR-003]].
**Priority:** must.

The boGDan appliance MUST decode H.264 video at 1080p60 using the BCM2711 V4L2 stateful M2M decoder (`v4l2h264dec`). The appliance MUST NOT use software H.264 decode (`avdec_h264`) for any H.264 stream that the hardware decoder can handle. The appliance MUST achieve ≥ 30 fps at 1080p with ≤ 50% CPU utilisation.

**Acceptance criteria:**

- Given a 1080p60 H.264 test stream, when the boGDan pipeline plays it for 60 seconds, then `v4l2-ctl --device=/dev/video10 --log-status` MUST show the decoder is active and the GStreamer `fpsdisplaysink` MUST report ≥ 30 fps (target 60 fps).
- Given the same scenario, when `top -b -n 1` is run, then the boGDan process's CPU% MUST be ≤ 50%.
- Given an H.264 stream with a codec profile the hardware decoder cannot handle (e.g. Hi444PP), then the appliance MAY fall back to software decode and MUST surface a `software_decode_fallback: true` field in `/api/status`.

### [[R-007]] Zero-copy DMA-BUF pipeline

**Problem:** [[P-003]] — Lack of hardware-accelerated video decoding pipeline (zero-copy dimension).
**Decision:** [[BP-ADR-003]].
**Priority:** must.

The boGDan appliance MUST transfer decoded video frames from the V4L2 decoder to the DRM/KMS display plane via DMA-BUF file descriptor passthrough, without copying pixel data through main memory. The pipeline `v4l2h264dec → v4l2convert → kmssink` MUST be zero-copy end-to-end for the common case (H.264 in NV12 format).

**Acceptance criteria:**

- Given a 1080p60 H.264 stream playing, when `v4l2-ctl --device=/dev/video10 --log-status` is queried, then the decoder output MUST be reported as DMA-BUF (not MMAP userptr).
- Given the same scenario, when GStreamer debug logging is enabled at level 3 for `kmssink`, then the log MUST show `importing dmabuf` entries and MUST NOT show `copying to dumb buffer` entries.
- The nightly hardware-in-the-loop test `tests/hw_zero_copy.rs` MUST fail the build if any `memcpy` symbol appears in the boGDan process's `perf` call graph during the decode→display path.

### [[R-008]] HTTP REST API conformance

**Problem:** [[P-004]] — Complex protocol landscape for media casting.
**Decision:** [[BP-ADR-004]].
**Priority:** must.

The boGDan appliance MUST expose an HTTP REST API on `0.0.0.0:8585` (configurable via `BOGDAN_HTTP_ADDR`) with the endpoints `POST /api/cast`, `POST /api/stop`, `POST /api/pause`, `POST /api/resume`, `POST /api/seek`, `GET /api/status`. All endpoints MUST accept and return JSON. CORS MUST be permissive (`Access-Control-Allow-Origin: *`). The API MUST conform to the contract in `docs/blueprint/04-fine-draft.md` (HTTP API Contract table).

**Acceptance criteria:**

- Given the conformance suite in `tests/conformance/http/`, when it is run against a boGDan appliance, then all tests MUST pass.
- Given a `POST /api/cast` with a missing `url` field, when the request is sent, then the response MUST be HTTP 400 with a JSON body `{"error":"missing_url"}`.
- Given a `POST /api/cast` with an unresolvable URL, when the request is sent, then the response MUST be HTTP 200 with `{"state":"error","code":"resolve_failed"}` and the session MUST return to Idle within 5 seconds.
- Given any HTTP request, when the response is returned, then the `Access-Control-Allow-Origin` header MUST be `*`.

### [[R-009]] WebSocket event conformance

**Problem:** [[P-004]] — Complex protocol landscape for media casting.
**Decision:** [[BP-ADR-004]], [[BP-ADR-007]].
**Priority:** must.

The boGDan appliance MUST expose a WebSocket server on `0.0.0.0:8586` (configurable via `BOGDAN_WS_ADDR`) at path `/events`. The server MUST push JSON events of types `state_changed`, `buffer_update`, `circuit_rotated`, `thermal_throttled`, and `error`. The server MUST support reconnect via a `last_event_id` field sent by the client on connection, replaying missed events from a 1024-entry ring buffer.

**Acceptance criteria:**

- Given the conformance suite in `tests/conformance/ws/`, when it is run against a boGDan appliance, then all tests MUST pass.
- Given a client subscribed to `/events`, when a state transition occurs, then the client MUST receive a `state_changed` event within 100 ms of the transition.
- Given a client that disconnects for 5 seconds during a cast, when it reconnects with `last_event_id`, then the server MUST replay all missed events in order.
- Given more than 1024 events have occurred since a client disconnected, when the client reconnects, then the server MUST replay the most recent 1024 events and MUST include an `events_dropped: true` field in the first replayed event.

### [[R-010]] DLNA MediaRenderer conformance

**Problem:** [[P-004]] — Complex protocol landscape for media casting (DLNA dimension).
**Decision:** [[BP-ADR-004]], [[BP-ADR-009]], [[ADR-006]].
**Priority:** should.

The boGDan appliance MUST advertise itself as a UPnP MediaRenderer via SSDP on the local network. The appliance MUST accept `SetAVTransportURI` SOAP calls and translate each into a `CastCommand`. The appliance MUST respond to `GetDeviceDescription` and `GetServiceDescription` requests with valid UPnP XML.

**Acceptance criteria:**

- Given the conformance suite in `tests/conformance/dlna/`, when it is run against a boGDan appliance, then all tests MUST pass.
- Given `gupnp-universal-cp` is run on a host on the same LAN, when it discovers devices, then `boGDan` MUST appear in the device list with a valid device description.
- Given a `SetAVTransportURI` call with a direct media URL, when the call is sent, then the appliance MUST start playback within 5 seconds and MUST emit a `state_changed: playing` event on the WebSocket bus.

### [[R-011]] Third-party client interop

**Problem:** [[P-004]] — Complex protocol landscape for media casting.
**Decision:** [[BP-ADR-004]], [[BP-ADR-009]].
**Priority:** should.

The boGDan appliance MUST interoperate with at least two third-party casting clients without modification. Qualified clients: VLC (via DLNA), MiniDLNA / ReadyMedia (via DLNA), Home Assistant (via UPnP), Plex (via DLNA).

**Acceptance criteria:**

- Given VLC on a host on the same LAN, when the user selects `boGDan` from the renderer list and casts a direct media URL, then playback MUST start within 10 seconds.
- Given MiniDLNA / ReadyMedia serving a directory of MP4 files, when the user selects a file and casts to `boGDan`, then playback MUST start within 10 seconds.
- Given Home Assistant with the UPnP integration configured, when the user triggers `media_player.play_media` on the `boGDan` entity, then playback MUST start within 10 seconds.

### [[R-012]] Stream survives circuit rotation

**Problem:** [[P-005]] — Tor circuit management for long-running media sessions.
**Decision:** [[BP-ADR-005]].
**Priority:** must.

The boGDan appliance MUST keep a media stream playing through a Tor circuit rotation with no more than 5 seconds of interrupted playback. The appliance MUST maintain a rolling buffer of at least 10 seconds of decoded-but-undisplayed media to mask circuit rotation latency.

**Acceptance criteria:**

- Given a boGDan appliance playing a stream, when `NEWNYM` is sent to the Tor control port to force circuit rotation, then the user-visible playback MUST NOT pause for more than 5 seconds.
- Given the same scenario, when `/api/status` is polled during the rotation, then `buffer_percent` MUST NOT drop below 25% (proving the buffer covered the rotation).
- The integration test `tests/integration_circuit_rotation.rs` MUST fail the build if the user-visible pause exceeds 5 seconds during a forced `NEWNYM`.

### [[R-013]] Automatic circuit replacement within 10 s

**Problem:** [[P-005]] — Tor circuit management for long-running media sessions.
**Decision:** [[BP-ADR-005]].
**Priority:** must.

The boGDan appliance MUST detect a failed Tor circuit within 5 seconds of the failure and MUST have a replacement circuit built and the stream re-established within 10 seconds of the failure. The appliance MUST re-resolve the URL through the same per-host SOCKS5 username (so Tor picks the same circuit if it's still alive or builds a new one with a fresh exit).

**Acceptance criteria:**

- Given a boGDan appliance playing a stream, when the Tor exit node for that stream's circuit becomes unreachable (simulated by `iptables` dropping the ORPort), then the appliance MUST detect the failure within 5 seconds (via GStreamer bus error or `reqwest` timeout) and MUST re-establish playback within 10 seconds.
- Given the same scenario, when `/api/status` is polled, then `circuit_rotations` MUST increment by at least 1 and `state` MUST return to `playing` within 10 seconds.
- If re-resolution returns 403 (CDN IP-bound signed URL on a new exit), then the appliance MUST surface `state: error` with `code: circuit_exhausted` and MUST NOT loop indefinitely.

### [[R-014]] YouTube URL resolution within 10 s

**Problem:** [[P-006]] — Content resolution through Tor.
**Decision:** [[BP-ADR-006]], [[ADR-008]].
**Priority:** must.

The boGDan appliance MUST resolve a YouTube watch URL to a direct media stream URL within 10 seconds through Tor, measured from the receipt of the `POST /api/cast` request to the first byte of media data fetched. The appliance MUST use the in-tree custom resolver for YouTube (per [[BP-ADR-006]]) to meet this budget; yt-dlp fallback is acceptable only for non-YouTube sources.

**Acceptance criteria:**

- Given a YouTube watch URL (e.g. `https://www.youtube.com/watch?v=dQw4w9WgXcQ`), when `POST /api/cast` is sent, then the appliance MUST reach `state: buffering` within 10 seconds.
- Given 10 distinct YouTube URLs, when each is cast in sequence, then at least 9 MUST resolve within 10 seconds (90% success rate, allowing for transient Tor slowness).
- The nightly hardware-in-the-loop test `tests/hw_youtube_cast.rs` MUST fail the build if the median resolution time exceeds 8 seconds.

### [[R-015]] Multi-source resolver coverage

**Problem:** [[P-006]] — Content resolution through Tor.
**Decision:** [[BP-ADR-006]].
**Priority:** must.

The boGDan appliance MUST support resolution from at least 5 content sources. Qualified sources: YouTube (in-tree), Vimeo (in-tree), direct media links (in-tree), and any 2 additional sources via yt-dlp fallback. The resolver layer MUST try the in-tree resolver first; on failure it MUST fall back to yt-dlp; on yt-dlp failure it MUST return a structured `ResolveError`.

**Acceptance criteria:**

- Given URLs from YouTube, Vimeo, a direct `.mp4` link, a Twitch VOD, and a SoundCloud track, when each is cast, then at least 4 of 5 MUST resolve to a direct media URL (80% success rate, allowing for site-specific breakage).
- Given an unsupported URL (e.g. `https://example.com/not-a-video`), when it is cast, then the response MUST be `state: error` with `code: resolve_failed` within 30 seconds (the yt-dlp timeout).
- Given a yt-dlp subprocess that hangs, when the resolver waits for it, then the resolver MUST kill the subprocess after 30 seconds and return `ResolveError::YtDlpFailed`.

### [[R-016]] Chrome extension one-click cast

**Problem:** [[P-007]] — Browser extension for sending media.
**Decision:** [[BP-ADR-007]].
**Priority:** should.

The boGDan browser extension MUST be installable on Chrome (Manifest V3) and MUST support one-click cast from YouTube, Vimeo, and direct media links. The extension MUST be stateless — all cast state lives on the Pi — so a service-worker eviction mid-cast MUST NOT lose the cast.

**Acceptance criteria:**

- Given Chrome with the boGDan extension installed, when the user is on a YouTube watch page and clicks the extension icon, then the extension MUST POST to `/api/cast` and MUST display a "casting" badge within 2 seconds.
- Given the same scenario, when the Chrome service worker is evicted (simulated via `chrome://serviceworker-internals/`), when it restarts, then the extension MUST reconnect the WebSocket and MUST re-sync the cast state from `/api/status` within 5 seconds.
- The extension build MUST produce a `.zip` loadable into Chrome via `chrome://extensions/` developer mode.

### [[R-017]] Firefox extension one-click cast

**Problem:** [[P-007]] — Browser extension for sending media.
**Decision:** [[BP-ADR-007]].
**Priority:** should.

The boGDan browser extension MUST be installable on Firefox (Manifest V3) from the same codebase as the Chrome build, and MUST support the same one-click cast behaviour.

**Acceptance criteria:**

- Given Firefox with the boGDan extension installed (via `about:debugging` temporary extension load), when the user is on a YouTube watch page and clicks the extension icon, then the extension MUST POST to `/api/cast` and MUST display a "casting" badge within 2 seconds.
- The extension build MUST produce an `.xpi` loadable into Firefox.
- The single codebase MUST build for both Chrome and Firefox via a `webextension-polyfill`-based build step, with no per-browser source forks.

### [[R-018]] One-command installer

**Problem:** [[P-008]] — Headless appliance setup and configuration.
**Decision:** [[BP-ADR-008]].
**Priority:** must.

The boGDan appliance MUST be installable on a fresh Raspberry Pi OS Lite 64-bit (Bookworm) via a single command: `curl -sSL https://raw.githubusercontent.com/pkhairkh/picast/<commit-sha>/scripts/setup.sh | sudo bash`. The installer MUST install the systemd unit, `torrc`, `iptables` rules, and the boGDan binary, and MUST reboot the Pi on completion. The installer MUST be pinned to a specific commit SHA (not `main`) for supply-chain integrity, and MUST be accompanied by a detached GPG signature.

**Acceptance criteria:**

- Given a fresh Raspberry Pi OS Lite 64-bit (Bookworm) image on a Pi 4, when the one-command installer is run, then after reboot the `bogdan.service` systemd unit MUST be active and `/api/status` MUST respond on port 8585.
- Given the installer URL in the README, when inspected, then it MUST point to a specific commit SHA, not to `main` or `master`.
- Given the installer script, when `gpg --verify setup.sh.sig setup.sh` is run, then the signature MUST verify against the boGDan release signing key.
- A pre-built Debian package and a pre-built SD card image MUST also be documented as alternative install paths.

### [[R-019]] Zero-SSH web configuration

**Problem:** [[P-008]] — Headless appliance setup and configuration.
**Decision:** [[BP-ADR-008]], [[BP-ADR-012]].
**Priority:** must.

The boGDan appliance MUST expose a web UI for configuration accessible at `http://bogdan.local` (mDNS) on the local network. The web UI MUST handle Tor bridge selection, network configuration, and media source preferences without requiring SSH access. Configuration MUST persist to `/etc/bogdan/bogdan.toml` and environment variables MUST override the config file.

**Acceptance criteria:**

- Given a boGDan appliance on a LAN with mDNS (Avahi) running, when a host on the same LAN browses to `http://bogdan.local`, then the configuration web UI MUST load.
- Given the web UI loaded, when the user configures a Tor bridge line and clicks "Save", then the bridge line MUST appear in `/etc/bogdan/bogdan.toml` under `[tor] bridges` and the Tor daemon MUST be signalled to reload within 5 seconds.
- Given `BOGDAN_LOG_LEVEL=debug` is set in the environment, when `bogdan.service` starts, then the log level MUST be `debug` regardless of the value in `bogdan.toml`.

### [[R-020]] mDNS discoverability at bogdan.local

**Problem:** [[P-008]] — Headless appliance setup and configuration.
**Decision:** [[BP-ADR-008]].
**Priority:** should.

The boGDan appliance MUST advertise itself via mDNS (Avahi) at the hostname `bogdan.local` on the local network. The appliance MUST print its IP address to the serial console and to `/etc/issue` (visible on the Pi's HDMI console, if attached) as a fallback for networks where mDNS does not work.

**Acceptance criteria:**

- Given a boGDan appliance on a LAN with mDNS-capable hosts, when `ping bogdan.local` is run from a macOS host, then the appliance MUST respond within 1 second.
- Given a boGDan appliance, when it boots, then `/etc/issue` MUST contain a line of the form `boGDan web UI: http://<ip-address>:8585/` showing the appliance's current IP.
- Given the appliance boots without a network cable attached, when it later gets a DHCP lease, then Avahi MUST advertise `bogdan.local` within 10 seconds of the lease.

### [[R-021]] DLNA renderer discovery via SSDP

**Problem:** [[P-009]] — UPnP/DLNA compatibility with existing devices.
**Decision:** [[BP-ADR-009]], [[ADR-006]].
**Priority:** should.

The boGDan appliance MUST advertise itself as a UPnP MediaRenderer via SSDP M-SEARCH responses and periodic NOTIFY announcements on port 1900. The appliance MUST release DRM master before pipeline construction when transitioning from a DLNA-initiated cast to a new cast, with a 500 ms grace window and a 2 s retry budget.

**Acceptance criteria:**

- Given a boGDan appliance on a LAN, when an SSDP M-SEARCH for `urn:schemas-upnp-org:device:MediaRenderer:1` is sent, then the appliance MUST respond within 1 second with a NOTIFY containing a valid device description URL.
- Given a DLNA-initiated cast is in progress and the user initiates a new cast via the HTTP API, when the new cast's pipeline is constructed, then the appliance MUST tear down `gmediarender` first, MUST wait 500 ms, MUST acquire DRM master within 2 seconds (4 retries × 500 ms), and MUST surface `state: error` with `code: drm_master_busy` only if all retries fail.

### [[R-022]] MiniDLNA / Plex media playback

**Problem:** [[P-009]] — UPnP/DLNA compatibility with existing devices.
**Decision:** [[BP-ADR-009]].
**Priority:** should.

The boGDan appliance MUST play media cast from MiniDLNA / ReadyMedia and from Plex Media Server. The appliance MUST handle the DLNA `SetAVTransportURI` call with both direct media URLs and URLs that require resolution through the boGDan resolver layer.

**Acceptance criteria:**

- Given MiniDLNA / ReadyMedia serving a directory of MP4 files, when the user selects a file and casts to `boGDan`, then playback MUST start within 10 seconds and MUST play to completion without errors.
- Given Plex Media Server with a library of H.264 MP4 files, when the user casts a file to `boGDan` via the Plex DLNA interface, then playback MUST start within 10 seconds.
- Given a DLNA cast with a URL that points to a web page rather than a direct media file (rare but possible), when the appliance receives it, then the appliance MUST resolve it through the boGDan resolver layer ([[C-007]]) before playback.

### [[R-023]] Thermal threshold enforcement

**Problem:** [[P-010]] — Thermal management on Raspberry Pi.
**Decision:** [[BP-ADR-010]].
**Priority:** nice.

The boGDan appliance MUST poll `/sys/class/thermal/thermal_zone0/temp` at most every 5 seconds. The appliance MUST emit a warning to `/api/status` (`thermal_throttled: true`) when temperature exceeds 80 °C. The appliance MUST pause the playback pipeline when temperature exceeds 85 °C and MUST resume when temperature drops below 75 °C.

**Acceptance criteria:**

- Given a boGDan appliance, when `thermal_zone0/temp` reads above 80000 (millidegree Celsius), then `/api/status` MUST return `thermal_throttled: true` within 5 seconds.
- Given a boGDan appliance, when `thermal_zone0/temp` reads above 85000, then the appliance MUST transition to `state: paused` with `code: thermal_pause` within 5 seconds.
- Given the appliance is in `thermal_pause`, when `thermal_zone0/temp` reads below 75000, then the appliance MUST transition back to `state: playing` within 5 seconds.
- The integration test `tests/integration_thermal_throttle.rs` (with a fake thermal zone) MUST verify all three transitions.

### [[R-024]] Bitrate fallback above 80 °C

**Problem:** [[P-010]] — Thermal management on Raspberry Pi.
**Decision:** [[BP-ADR-010]].
**Priority:** nice.

The boGDan appliance SHOULD request a lower-bitrate media variant from the resolver when temperature exceeds 80 °C, before pausing at 85 °C. The resolver MUST return a typed `NoLowerVariant` error when no lower-bitrate variant exists; on receiving this error the appliance MUST skip the bitrate fallback step and continue monitoring temperature.

**Acceptance criteria:**

- Given a boGDan appliance playing a 1080p stream, when `thermal_zone0/temp` reads above 80000, then the appliance MUST request a lower-bitrate variant from the resolver within 5 seconds and the new variant's bitrate MUST be strictly lower than the current variant's.
- Given a source with no lower-bitrate variant (e.g. a direct MP4 with no alternate streams), when the appliance requests a lower variant, then the resolver MUST return `ResolveError::NoLowerVariant` and the appliance MUST continue playing the current variant.
- The thermal supervisor and resolver contract ([[T-007]] in the fine draft) MUST be resolved before this requirement is implementable.

### [[R-025]] Multi-room sync deferred to v2

**Problem:** [[P-011]] — Multi-room audio/video synchronization.
**Decision:** [[BP-ADR-011]].
**Priority:** nice (deferred to v2).

Multi-room synchronised playback is deferred to v2. The v1 appliance MUST document multi-room as unsupported and MUST recommend one appliance per TV. The v2 design sketch (leader-follower over WebSocket bus, PTP-style timestamps, 100 ms tolerance, ethernet-only) MUST be recorded in `docs/blueprint/03-adrs/011-multi-room-deferred.md` for future implementation.

**Acceptance criteria:**

- Given the boGDan user guide, when a user reads the multi-room section, then the guide MUST state "Multi-room sync is not supported in v1" and MUST link to the v2 design sketch in `docs/blueprint/03-adrs/011-multi-room-deferred.md`.
- Given the v1 appliance, when a user attempts to add a second appliance to a cast session, then the appliance MUST refuse with a clear error message indicating v1 limitation.
- The v2 design sketch MUST describe leader election, clock sync protocol, and the 100 ms tolerance enforcement mechanism.

### [[R-026]] Web UI WAVE conformance

**Problem:** [[P-012]] — Accessibility — screen reader support for web UI.
**Decision:** [[BP-ADR-012]].
**Priority:** nice.

The boGDan configuration web UI MUST pass the WAVE accessibility evaluation with zero errors and zero contrast errors. The web UI MUST also pass the axe-core automated accessibility audit with zero violations. Both audits MUST run in CI against every PR that touches `src/server/web/`.

**Acceptance criteria:**

- Given the web UI source in `src/server/web/`, when the WAVE CLI is run against a built version, then the report MUST contain zero errors and zero contrast errors.
- Given the same source, when `axe-core` is run against a built version via the `@axe-core/playwright` test harness, then the report MUST contain zero violations.
- The CI workflow `.github/workflows/ci.yml` MUST include a `a11y` job that runs both WAVE and axe-core and MUST fail the build on any violation.

### [[R-027]] Keyboard-only navigation

**Problem:** [[P-012]] — Accessibility — screen reader support for web UI.
**Decision:** [[BP-ADR-012]].
**Priority:** nice.

The boGDan configuration web UI MUST support keyboard-only navigation for all actions. The Tab key MUST move focus forward; Shift-Tab MUST move focus backward; Enter MUST activate a focused control; Escape MUST close any open dialog or menu. No action MUST require a mouse. Focus order MUST follow visual order. Focus MUST be visible (a focus indicator with at least 3:1 contrast against the background).

**Acceptance criteria:**

- Given the web UI loaded in a browser with the mouse unplugged, when the user presses Tab repeatedly, then focus MUST visit every interactive control in visual order, MUST wrap from the last control back to the first, and MUST show a visible focus indicator at each stop.
- Given a dialog (e.g. the "Add Tor bridge" dialog) is open, when the user presses Escape, then the dialog MUST close and focus MUST return to the control that opened it.
- The pre-release manual a11y test plan MUST include a "keyboard-only navigation smoke test" and the test MUST pass before v1 release.

### [[R-028]] High-contrast mode

**Problem:** [[P-012]] — Accessibility — screen reader support for web UI.
**Decision:** [[BP-ADR-012]].
**Priority:** nice.

The boGDan configuration web UI MUST provide a high-contrast mode toggle in the UI header. High-contrast mode MUST be implemented as a CSS variable theme that achieves at least 7:1 contrast between foreground and background. The user's choice MUST persist across sessions via `localStorage`.

**Acceptance criteria:**

- Given the web UI loaded, when the user clicks the high-contrast toggle in the header, then all text and interactive controls MUST switch to a high-contrast palette achieving ≥ 7:1 contrast (measured by the WAVE contrast check).
- Given the user has enabled high-contrast mode, when they reload the page, then high-contrast mode MUST remain enabled (persisted via `localStorage`).
- Given the web UI source, when inspected, then the colour values MUST be defined as CSS custom properties (variables) and MUST NOT be hardcoded in selectors.

## Requirement-to-Test Traceability

| Requirement | Primary test | Test type | Cadence |
|---|---|---|---|
| [[R-001]] | `verify-network-isolation.sh` | integration + hw | every PR + nightly |
| [[R-002]] | `tests/integration_circuit_isolation.rs` | integration | every PR |
| [[R-003]] | CI grep `rg 'getaddrinfo'` + `tcpdump port 53` | static + integration | every PR |
| [[R-004]] | `pgrep` + systemd unit inspection | hw | nightly |
| [[R-005]] | `tests/hw_1080p60.rs` | hw | nightly |
| [[R-006]] | `tests/hw_1080p60.rs` + `v4l2-ctl` | hw | nightly |
| [[R-007]] | `tests/hw_zero_copy.rs` | hw | nightly |
| [[R-008]] | `tests/conformance/http/` | conformance | every PR |
| [[R-009]] | `tests/conformance/ws/` | conformance | every PR |
| [[R-010]] | `tests/conformance/dlna/` | conformance | every PR |
| [[R-011]] | manual smoke test matrix | manual | pre-release |
| [[R-012]] | `tests/integration_circuit_rotation.rs` | integration | every PR |
| [[R-013]] | `tests/integration_circuit_failure.rs` | integration | every PR |
| [[R-014]] | `tests/hw_youtube_cast.rs` | hw | nightly |
| [[R-015]] | `tests/integration_multi_source.rs` | integration | every PR |
| [[R-016]] | `tests/extension/chrome.spec.ts` | extension | every PR |
| [[R-017]] | `tests/extension/firefox.spec.ts` | extension | every PR |
| [[R-018]] | `tests/installer/fresh_pi_os.sh` | hw | pre-release |
| [[R-019]] | `tests/integration_web_ui.rs` | integration | every PR |
| [[R-020]] | `tests/integration_mdns.rs` | integration | nightly |
| [[R-021]] | `tests/integration_ssdp.rs` | integration | every PR |
| [[R-022]] | `tests/integration_dlna_minidlna.rs` + manual Plex | integration + manual | every PR + pre-release |
| [[R-023]] | `tests/integration_thermal_throttle.rs` | integration | every PR |
| [[R-024]] | `tests/integration_thermal_bitrate.rs` | integration | every PR |
| [[R-025]] | doc review only | static | pre-release |
| [[R-026]] | WAVE + axe-core in CI | automated | every PR |
| [[R-027]] | keyboard smoke (manual) + automated focus check | manual + automated | pre-release + every PR |
| [[R-028]] | WAVE contrast check (automated) | automated | every PR |

## Requirement-to-Problem Coverage

Every problem [[P-001]]..[[P-012]] is addressed by at least one requirement. Coverage matrix:

| Problem | Requirements |
|---|---|
| [[P-001]] | [[R-001]], [[R-002]], [[R-003]] |
| [[P-002]] | [[R-004]], [[R-005]] |
| [[P-003]] | [[R-006]], [[R-007]] |
| [[P-004]] | [[R-008]], [[R-009]], [[R-010]], [[R-011]] |
| [[P-005]] | [[R-012]], [[R-013]] |
| [[P-006]] | [[R-014]], [[R-015]] |
| [[P-007]] | [[R-016]], [[R-017]] |
| [[P-008]] | [[R-018]], [[R-019]], [[R-020]] |
| [[P-009]] | [[R-021]], [[R-022]] |
| [[P-010]] | [[R-023]], [[R-024]] |
| [[P-011]] | [[R-025]] |
| [[P-012]] | [[R-026]], [[R-027]], [[R-028]] |

## Open Questions for the Implementation Phase

These are requirements whose acceptance criteria depend on resolving an open question from the fine draft ([[T-001]]..[[T-008]]):

- [[R-013]] depends on [[T-001]] (SOCKS5 forwarder implementation).
- [[R-024]] depends on [[T-007]] (thermal supervisor / resolver contract for `NoLowerVariant`).
- [[R-016]] / [[R-017]] depend on [[T-008]] (browser extension MV3 storage strategy).
- [[R-019]] depends on [[T-002]] (web UI framework choice).
- [[R-018]] depends on [[T-006]] (mDNS fallback strategy).
