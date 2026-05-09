# ADR-006: UPnP/DLNA MediaRenderer

| Field        | Value          |
|--------------|----------------|
| **ID**       | ADR-006        |
| **Status**   | ACCEPTED       |
| **Date**     | 2025-01-19     |
| **Supersedes** | —            |
| **Superseded by** | —         |

## Context

boGDan needs a discovery and control protocol that allows users on the local network to send media URLs to the Pi for playback. The protocol must:

1. **Be discoverable** — Users should see boGDan as an available device without manual IP entry
2. **Work with existing apps** — VLC, DLNA controller apps (BubbleUPnP, Hi-Fi Cast), and Home Assistant should be able to send media to boGDan
3. **Support direct URL playback** — Send a media URL; boGDan downloads and plays it
4. **Be open and standard** — No proprietary certificates or vendor-controlled infrastructure

DLNA (Digital Living Network Alliance) built on UPnP (Universal Plug and Play) is the most widely supported open standard for media rendering on local networks. It provides:

- **SSDP discovery** — Devices announce themselves on the local network via UDP multicast
- **UPnP MediaRenderer device type** — Standard device profile for receiving media URLs
- **AVTransport service** — Control protocol for play, pause, stop, seek, and URI setting
- **RenderingControl service** — Volume control and mute

### gmediarender

[gmediarender](https://github.com/hzeller/gmediarender) is a lightweight, open-source DLNA MediaRenderer implementation. Key features:

- **Custom GStreamer pipeline** — gmediarender allows specifying a custom GStreamer pipeline via the `GSTREAMER_PIPELINE` environment variable, enabling boGDan to inject its V4L2 M2M + kmssink pipeline
- **Small footprint** — ~2 MB binary, ~5 MB RAM; negligible compared to the GStreamer pipeline it hosts
- **SSDP + UPnP stack** — Built-in SSDP announcement and UPnP SOAP service handling
- **Battle-tested** — Used in numerous Pi-based media projects (Volumio, Max2Play, etc.)

## Decision

boGDan uses gmediarender as its DLNA MediaRenderer, configured with a custom GStreamer pipeline that integrates boGDan's V4L2 M2M hardware decode and Tor SOCKS5 routing.

The `bogdan-protocols` crate manages gmediarender as a subprocess with the following configuration:

```
GSTREAMER_PIPELINE="souphttpsrc location=%U proxy-id=<tor-socks5> ! \
  queue2 max-size-bytes=10485760 use-buffering=true ! \
  parsebin ! v4l2h264dec capture-io-mode=dmabuf ! \
  kmssink plane-id=0 can-attach-static=true"

gmediarender \
  --friendly-name "boGDan" \
  --uuid <generated-uuid> \
  --port 49152 \
  --gst-initial-volume 80
```

The `%U` placeholder is replaced by gmediarender with the URI received from the DLNA controller.

The `bogdan-protocols` crate also:
- Manages SSDP announcement (re-announce every 1800 seconds per UPnP spec)
- Provides an HTTP endpoint on port 49152 for UPnP device description
- Monitors gmediarender subprocess health and restarts on crash
- Forwards AVTransport events to the boGDan session manager

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ Works with VLC | VLC's "Render" menu discovers boGDan via SSDP and can send media URLs |
| ✅ Works with DLNA apps | BubbleUPnP, Hi-Fi Cast, and other DLNA controllers can discover and control boGDan |
| ✅ Works with Home Assistant | HA's DLNA integration can cast media to boGDan as a MediaRenderer entity |
| ✅ SSDP auto-discovery | No manual IP configuration; boGDan appears on the network automatically |
| ✅ Custom GStreamer pipeline | gmediarender's `GSTREAMER_PIPELINE` env var allows injecting V4L2 M2M + Tor routing |
| ✅ Small footprint | gmediarender adds only ~5 MB RAM to the system |
| ❌ Direct URLs only | DLNA MediaRenderer receives URLs, not JavaScript-rendered pages; cannot play sites requiring JS execution |
| ❌ No adaptive bitrate from DLNA sender | DLNA sends a single URL; quality is fixed at send time. ABR must be handled by boGDan's `queue2` buffer management (graceful degradation, not quality switching) |
| ❌ UPnP security concerns | SSDP/UPnP on the local network could be exploited by malicious LAN devices; boGDan mitigates with iptables rules restricting UPnP traffic to the LAN interface |
| ❌ gmediarender is a subprocess | Another process to manage; crashes in gmediarender's UPnP stack don't affect boGDan core but require monitoring and restart |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **Custom DLNA implementation** | Writing a full UPnP SSDP + SOAP + AVTransport stack from scratch is ~3000+ lines of non-trivial protocol code; gmediarender already implements this correctly and allows custom GStreamer pipelines; reinventing it would take weeks and introduce protocol compliance bugs |
| **Rygel** | Rygel is a GNOME-based DLNA server/renderer; heavier than gmediarender (~15 MB RAM); requires GLib main loop integration; designed as a media server, not a lightweight renderer; would require patching to use boGDan's custom GStreamer pipeline |
