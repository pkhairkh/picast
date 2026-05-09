# DLNA MediaRenderer Specification

boGDan implements a UPnP AV MediaRenderer device so that standard DLNA controller apps (BubbleUPnP, Windows Media Player, VLC, Home Assistant) can cast media without a custom sender app. This document specifies the SSDP discovery protocol, device description XML, AVTransport service actions, RenderingControl service actions, and the mapping between UPnP transport states and boGDan's internal session states.

## Overview

```
┌──────────────┐    SSDP (1900)     ┌──────────────┐
│  DLNA        │◀───────────────────│  boGDan      │
│  Controller  │    M-SEARCH/NOTIFY │  MediaRenderer│
│  (Phone/PC)  │                    │              │
│              │    HTTP (49152)   │              │
│              │──────────────────▶│              │
│              │  GET /desc.xml    │              │
│              │  POST /ctl/*      │              │
└──────────────┘                    └──────────────┘
```

The DLNA renderer is implemented via gmediarender running on port 49152 for HTTP (device description, SOAP control) and uses UDP port 1900 for SSDP discovery. boGDan manages the gmediarender subprocess and synchronizes its state with the session manager, ensuring DLNA and HTTP/WebSocket clients see a consistent playback state.

## SSDP Discovery

### Announcement (NOTIFY)

boGDan sends NOTIFY messages every 30 seconds to the SSDP multicast address `239.255.255.250:1900`. Four separate NOTIFY messages are sent, one for each service type, to ensure compatibility with all DLNA controllers:

```
NOTIFY * HTTP/1.1
HOST: 239.255.255.250:1900
CACHE-CONTROL: max-age=1800
LOCATION: http://<pi-ip>:49152/description.xml
NT: upnp:rootdevice
NTS: ssdp:alive
SERVER: Linux/5.15 UPnP/1.1 boGDan/0.1
USN: uuid:bogdan-001::upnp:rootdevice
```

```
NOTIFY * HTTP/1.1
HOST: 239.255.255.250:1900
CACHE-CONTROL: max-age=1800
LOCATION: http://<pi-ip>:49152/description.xml
NT: urn:schemas-upnp-org:device:MediaRenderer:1
NTS: ssdp:alive
SERVER: Linux/5.15 UPnP/1.1 boGDan/0.1
USN: uuid:bogdan-001::urn:schemas-upnp-org:device:MediaRenderer:1
```

```
NOTIFY * HTTP/1.1
HOST: 239.255.255.250:1900
CACHE-CONTROL: max-age=1800
LOCATION: http://<pi-ip>:49152/description.xml
NT: urn:schemas-upnp-org:service:AVTransport:1
NTS: ssdp:alive
SERVER: Linux/5.15 UPnP/1.1 boGDan/0.1
USN: uuid:bogdan-001::urn:schemas-upnp-org:service:AVTransport:1
```

```
NOTIFY * HTTP/1.1
HOST: 239.255.255.250:1900
CACHE-CONTROL: max-age=1800
LOCATION: http://<pi-ip>:49152/description.xml
NT: urn:schemas-upnp-org:service:RenderingControl:1
NTS: ssdp:alive
SERVER: Linux/5.15 UPnP/1.1 boGDan/0.1
USN: uuid:bogdan-001::urn:schemas-upnp-org:service:RenderingControl:1
```

### M-SEARCH Response

When a controller sends an M-SEARCH query, boGDan responds within a random delay of 0–MX seconds (to avoid response storms when multiple devices are present on the network):

```
HTTP/1.1 200 OK
CACHE-CONTROL: max-age=1800
DATE: Mon, 15 Jan 2024 10:30:00 GMT
EXT:
LOCATION: http://<pi-ip>:49152/description.xml
SERVER: Linux/5.15 UPnP/1.1 boGDan/0.1
ST: urn:schemas-upnp-org:device:MediaRenderer:1
USN: uuid:bogdan-001::urn:schemas-upnp-org:device:MediaRenderer:1
```

### Shutdown (byebye)

On graceful shutdown, boGDan sends NOTIFY with `NTS: ssdp:byebye` for each service type:

```
NOTIFY * HTTP/1.1
HOST: 239.255.255.250:1900
NT: urn:schemas-upnp-org:device:MediaRenderer:1
NTS: ssdp:byebye
USN: uuid:bogdan-001::urn:schemas-upnp-org:device:MediaRenderer:1
```

## Device Description XML

Served at `GET /description.xml` on port 49152. This XML describes the boGDan device, its services, and their control/event URLs.

```xml
<?xml version="1.0" encoding="utf-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0"
      xmlns:dlna="urn:schemas-dlna-org:device-1-0">
  <specVersion>
    <major>1</major>
    <minor>1</minor>
  </specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>boGDan</friendlyName>
    <manufacturer>boGDan Project</manufacturer>
    <manufacturerURL>https://github.com/bogdan/bogdan</manufacturerURL>
    <modelDescription>boGDan Media Renderer</modelDescription>
    <modelName>boGDan</modelName>
    <modelNumber>0.1</modelNumber>
    <modelURL>https://github.com/bogdan/bogdan</modelURL>
    <serialNumber>000000001</serialNumber>
    <UDN>uuid:bogdan-001</UDN>
    <dlna:X_DLNACAP>playcontainer-0-0,mrl-1-0</dlna:X_DLNACAP>
    <dlna:X_DLNADOC>DMR-1.50</dlna:X_DLNADOC>

    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <controlURL>/ctl/avtransport</controlURL>
        <eventSubURL>/evt/avtransport</eventSubURL>
        <SCPDURL>/scpd/avtransport.xml</SCPDURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>
        <controlURL>/ctl/renderingcontrol</controlURL>
        <eventSubURL>/evt/renderingcontrol</eventSubURL>
        <SCPDURL>/scpd/renderingcontrol.xml</SCPDURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>
        <controlURL>/ctl/connectionmanager</controlURL>
        <eventSubURL>/evt/connectionmanager</eventSubURL>
        <SCPDURL>/scpd/connectionmanager.xml</SCPDURL>
      </service>
    </serviceList>
  </device>
</root>
```

## AVTransport Service

Control URL: `POST /ctl/avtransport`

### SetAVTransportURI

Set the media URL to play. Maps to `SessionManager::load(url)`. The URL may be a direct media URL, an HLS manifest, or a web page URL — boGDan will classify and resolve it through the same pipeline used by the HTTP API.

```xml
<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"
            s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
      <InstanceID>0</InstanceID>
      <CurrentURI>https://example.com/video.mp4</CurrentURI>
      <CurrentURIMetaData>
        &lt;DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"&gt;
          &lt;item id="0" parentID="-1" restricted="1"&gt;
            &lt;res protocolInfo="http-get:*:video/mp4:*"&gt;
              https://example.com/video.mp4
            &lt;/res&gt;
            &lt;dc:title xmlns:dc="http://purl.org/dc/elements/1.1/"&gt;Video Title&lt;/dc:title&gt;
          &lt;/item&gt;
        &lt;/DIDL-Lite&gt;
      </CurrentURIMetaData>
    </u:SetAVTransportURI>
  </s:Body>
</s:Envelope>
```

### Play

Start playback. Maps to `SessionManager::play()`.

```xml
<u:Play xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
  <InstanceID>0</InstanceID>
  <Speed>1</Speed>
</u:Play>
```

### Pause

Pause playback. Maps to `SessionManager::pause()`.

```xml
<u:Pause xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
  <InstanceID>0</InstanceID>
</u:Pause>
```

### Stop

Stop playback. Maps to `SessionManager::stop()`.

```xml
<u:Stop xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
  <InstanceID>0</InstanceID>
</u:Stop>
```

### Seek

Seek to a position. Maps to `SessionManager::seek()`.

```xml
<u:Seek xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
  <InstanceID>0</InstanceID>
  <Unit>REL_TIME</Unit>
  <Target>00:02:15</Target>
</u:Seek>
```

**Target format**: `HH:MM:SS` or `HH:MM:SS.fraction`. boGDan converts this to seconds and calls `seek(position_secs)`.

### GetPositionInfo

Return current position and duration.

```xml
<u:GetPositionInfoResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
  <Track>1</Track>
  <TrackDuration>0:03:32.000</TrackDuration>
  <TrackMetaData>...</TrackMetaData>
  <TrackURI>https://example.com/video.mp4</TrackURI>
  <RelTime>0:01:27.500</RelTime>
  <AbsTime>0:01:27.500</AbsTime>
  <RelCount>2147483647</RelCount>
  <AbsCount>2147483647</AbsCount>
</u:GetPositionInfoResponse>
```

### GetTransportInfo

Return current transport state.

```xml
<u:GetTransportInfoResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
  <CurrentTransportState>PLAYING</CurrentTransportState>
  <CurrentTransportStatus>OK</CurrentTransportStatus>
  <CurrentSpeed>1</CurrentSpeed>
</u:GetTransportInfoResponse>
```

### boGDan State to UPnP Transport State Mapping

| boGDan State | UPnP Transport State | Notes |
|-------------|---------------------|-------|
| Idle | NO_MEDIA_PRESENT | No media loaded |
| Resolving | TRANSITIONING | URL being resolved through yt-dlp/Tor |
| Loading | TRANSITIONING | GStreamer pipeline being constructed |
| Playing | PLAYING | Normal playback |
| Paused | PAUSED_PLAYBACK | Playback paused |
| Buffering | PLAYING | Buffer underrun; display buffering indicator. Some controllers show this as "stalled" |
| Error | TRANSITIONING | Error state; controller may retry |

## RenderingControl Service

Control URL: `POST /ctl/renderingcontrol`

### SetVolume

Set the playback volume (0–100 integer, matching boGDan's internal 0–100 range).

```xml
<u:SetVolume xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1">
  <InstanceID>0</InstanceID>
  <Channel>Master</Channel>
  <DesiredVolume>75</DesiredVolume>
</u:SetVolume>
```

### GetVolume

Return current volume level.

```xml
<u:GetVolumeResponse xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1">
  <CurrentVolume>75</CurrentVolume>
</u:GetVolumeResponse>
```

### SetMute / GetMute

Mute/unmute the audio output. Maps to `volume=0` / restore previous volume.

```xml
<u:SetMute xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1">
  <InstanceID>0</InstanceID>
  <Channel>Master</Channel>
  <DesiredMute>1</DesiredMute>
</u:SetMute>
```

## ConnectionManager Service

Control URL: `POST /ctl/connectionmanager`

### GetProtocolInfo

Return supported media protocols. This tells DLNA controllers which media types boGDan can play.

```xml
<u:GetProtocolInfoResponse xmlns:u="urn:schemas-upnp-org:service:ConnectionManager:1">
  <Source></Source>
  <Sink>
    http-get:*:video/mp4:*,
    http-get:*:video/x-matroska:*,
    http-get:*:video/webm:*,
    http-get:*:video/quicktime:*,
    http-get:*:application/x-mpegURL:*,
    http-get:*:application/dash+xml:*,
    http-get:*:audio/mpeg:*,
    http-get:*:audio/ogg:*,
    http-get:*:audio/flac:*
  </Sink>
</u:GetProtocolInfoResponse>
```

## Error Handling

UPnP SOAP errors use fault codes with UPnP-specific error numbers:

```xml
<s:Body>
  <s:Fault>
    <faultcode>s:Client</faultcode>
    <faultstring>UPnPError</faultstring>
    <detail>
      <UPnPError xmlns="urn:schemas-upnp-org:control-1-0">
        <errorCode>701</errorCode>
        <errorDescription>No such object</errorDescription>
      </UPnPError>
    </detail>
  </s:Fault>
</s:Body>
```

| Error Code | Description | boGDan Scenario |
|-----------|-------------|-----------------|
| 402 | Invalid arguments | Missing InstanceID or required parameter |
| 501 | Action failed | Internal error (GStreamer, Tor, DRM) |
| 701 | No such object | Invalid session or track reference |
| 706 | Argument invalid | Negative seek target, invalid volume |
| 714 | Illegal seek target | Seek beyond duration |
| 716 | Illegal transport state | Play when no URI is set |
| 718 | Content format mismatch | Unsupported codec or container |

## DLNA Limitations

- **No site resolution feedback**: DLNA controllers expect `SetAVTransportURI` to return immediately. If the URL requires yt-dlp resolution (5–15 seconds), the SOAP response returns success and the transport state is set to `TRANSITIONING`. The controller must poll `GetTransportInfo` to detect when playback actually starts.
- **No Tor indication**: DLNA has no mechanism to indicate that traffic is being routed through Tor. The controller sees a standard MediaRenderer.
- **Single instance**: All AVTransport and RenderingControl actions use `InstanceID=0`. boGDan does not support multiple simultaneous media streams.
