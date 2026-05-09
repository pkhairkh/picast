# mDNS and SSDP Discovery

boGDan advertises itself on the local network using both mDNS (DNS-SD) and SSDP (UPnP) so that sender applications can discover it without manual IP configuration. The dual-discovery strategy ensures compatibility with both custom boGDan clients (which use mDNS) and standard DLNA controllers (which use SSDP).

## mDNS / DNS-SD

### Service Type

```
_bogcast._tcp
```

boGDan registers this custom service type on the local network. The browser extension and native apps discover boGDan instances by browsing for `_bogcast._tcp`. This custom type avoids collision with other media services and allows boGDan-specific metadata to be included in TXT records.

### Registration

```
Service Name:  boGDan (<hostname>)
Service Type:  _bogcast._tcp
Domain:        local.
Port:          8080
```

### TXT Records

TXT records carry boGDan-specific metadata that clients use to determine capabilities and connection parameters without making additional network requests.

| Key | Value | Description |
|-----|-------|-------------|
| `ver` | `0.1.0` | boGDan software version (semver) |
| `ws` | `8081` | WebSocket port for real-time status |
| `dlna` | `8200` | DLNA HTTP port for UPnP control |
| `hw` | `pi4` | Hardware identifier (always `pi4` for Pi 4) |
| `id` | `bogdan-001` | Unique device ID (derived from `/etc/machine-id`) |
| `tor` | `1` | Tor routing enabled (1=yes, 0=no) |
| `maxres` | `1080p` | Maximum supported resolution |

### Example mDNS Response

```
_bogcast._tcp.local. PTR boGDan\032\(raspberrypi\)._bogcast._tcp.local.
boGDan\032\(raspberrypi\)._bogcast._tcp.local. SRV 0 0 8080 raspberrypi.local.
boGDan\032\(raspberrypi\)._bogcast._tcp.local. TXT "ver=0.1.0" "ws=8081" "dlna=8200" "hw=pi4" "id=bogdan-001" "tor=1" "maxres=1080p"
raspberrypi.local. A 192.168.1.100
```

### Browser Extension Discovery

The boGDan browser extension discovers boGDan instances using the mDNS API available in Manifest V3 extensions. The discovery flow is:

1. Extension sends a PTR query for `_bogcast._tcp.local`.
2. boGDan responds with its service instance name.
3. Extension resolves the SRV record to get the hostname and port.
4. Extension resolves the A record to get the IP address.
5. Extension reads TXT records for WebSocket port and device ID.
6. Extension connects to the HTTP API on port 8080 and/or WebSocket on port 8081.

```javascript
// Simplified discovery in the browser extension
const query = { name: '_bogcast._tcp.local', type: 'PTR' };
// Result parsing: extract SRV, A, and TXT records
```

### Implementation Requirements

boGDan's mDNS responder must:

1. Listen on UDP port 5353 on all interfaces.
2. Join the mDNS multicast group `224.0.0.251` (IPv4) or `ff02::fb` (IPv6).
3. Respond to PTR queries for `_bogcast._tcp.local`.
4. Include SRV and TXT records in the response (per RFC 6763 §12).
5. Send proactive announcements on startup and every 60 seconds thereafter.
6. Send a goodbye (TTL=0) announcement on shutdown.

---

## SSDP (UPnP Discovery)

SSDP is part of the UPnP device architecture. boGDan uses it primarily for DLNA controller discovery. The full SSDP message format is specified in `docs/protocols/dlna.md`; this document describes the service types, intervals, and network behavior.

### Service Types Advertised

| Service Type | Purpose | Target Audience |
|-------------|---------|-----------------|
| `upnp:rootdevice` | Root UPnP device | All UPnP control points |
| `urn:schemas-upnp-org:device:MediaRenderer:1` | DLNA MediaRenderer | VLC, BubbleUPnP, Windows Media Player |
| `urn:schemas-upnp-org:service:AVTransport:1` | Transport control | Controllers seeking playback control |
| `urn:schemas-upnp-org:service:RenderingControl:1` | Volume control | Controllers seeking volume/mute control |
| `urn:schemas-upnp-org:service:ConnectionManager:1` | Protocol info | Controllers checking media compatibility |

### Announcement Intervals

| Event | Interval | Cache TTL | Notes |
|-------|----------|-----------|-------|
| NOTIFY (alive) | 30 seconds | 1800 (30 min) | Sent for each service type (4 NOTIFY messages per interval) |
| NOTIFY (byebye) | On shutdown | — | Sent once per service type on graceful shutdown |
| M-SEARCH response | On demand | 1800 | Random delay 0–MX seconds to avoid storms |

### M-SEARCH Handling

boGDan listens for M-SEARCH queries on UDP port 1900 at the multicast address `239.255.255.250`. It responds only to queries that match its advertised service types:

```
M-SEARCH * HTTP/1.1
HOST: 239.255.255.250:1900
MAN: "ssdp:discover"
MX: 3
ST: urn:schemas-upnp-org:device:MediaRenderer:1
```

The `MX` header specifies the maximum wait time in seconds. boGDan responds with a random delay between 0 and MX seconds to prevent response storms when multiple devices are present on the network.

---

## Dual Discovery Strategy

boGDan advertises through both mDNS and SSDP because they serve different audiences:

| Protocol | Used by | Advantages | Disadvantages |
|----------|---------|------------|---------------|
| mDNS / DNS-SD | boGDan browser extension, native apps | Custom TXT records, lower overhead, boGDan-specific metadata | Not understood by DLNA controllers |
| SSDP | DLNA controllers (VLC, BubbleUPnP) | Standard UPnP interoperability, no custom code needed | Higher overhead, slower discovery, no custom metadata |

### Network Interface Selection

On the Pi, the primary network interface is typically:

| Interface | Type | Priority | Notes |
|-----------|------|----------|-------|
| `eth0` | Ethernet | Primary | Recommended for boGDan (stable, low latency) |
| `wlan0` | Wi-Fi | Secondary | Works but may have higher latency and packet loss |

boGDan sends announcements on **all** interfaces that have an IPv4 address. It does not announce on `lo` (loopback). The responder enumerates interfaces at startup and re-enumerates on network configuration changes.

### Firewall Considerations

The following ports must be open in iptables for discovery to work:

| Port | Protocol | Direction | Purpose |
|------|----------|-----------|---------|
| 5353 | UDP | In/Out | mDNS multicast |
| 1900 | UDP | In/Out | SSDP multicast |
| 8080 | TCP | In | HTTP REST API |
| 8081 | TCP | In | WebSocket |
| 8200 | TCP | In | DLNA HTTP (device description + SOAP) |

See `config/iptables.rules` for the complete firewall configuration. The iptables rules must allow multicast traffic on ports 5353 and 1900, and must NOT block the `239.255.255.250` and `224.0.0.251` multicast addresses.

## Troubleshooting Discovery

| Problem | Cause | Fix |
|---------|-------|-----|
| VLC doesn't show boGDan | SSDP not responding | Check `iptables -L` allows UDP 1900; check boGDan is running |
| Browser extension can't find boGDan | mDNS not responding | Check UDP 5353 is open; try `avahi-browse -r _bogcast._tcp` |
| Discovery works on Wi-Fi but not Ethernet | Wrong interface | Check `ip addr` for active interfaces; boGDan should announce on all |
| Multiple boGDan instances appear | Stale mDNS cache | Wait 60 seconds for cache expiry; send goodbye on shutdown |
| SSDP responses are slow | MX delay + network latency | Normal; controllers may take 3–5 seconds to discover |
