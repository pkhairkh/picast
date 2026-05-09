# HTTP API Reference

boGDan exposes a RESTful JSON API on port **8585** (configurable via `BOGDAN_HTTP_ADDR` or `bogdan.toml`) for sender applications — browser extension, curl, scripts, and home automation integrations. All endpoints accept and return `application/json`. TLS is supported: when `tls_cert_path` and `tls_key_path` are configured in `bogdan.toml`, the server serves HTTPS instead of plain HTTP.

## Base URL

```
http://<pi-ip-address>:8585
```

When TLS is enabled:

```
https://<pi-ip-address>:8585
```

## Content Type

All request and response bodies use `application/json`.

## CORS

All responses include CORS headers to allow browser extension and local web app cross-origin requests:

```
Access-Control-Allow-Origin: *
Access-Control-Allow-Methods: GET, POST, OPTIONS
Access-Control-Allow-Headers: Content-Type
```

All endpoints respond to `OPTIONS` preflight requests with `204 No Content` and the above headers.

---

## Endpoints

### `POST /api/cast`

Load and play a media URL. The URL is classified and resolved through the resolver pipeline (custom resolvers or yt-dlp), then playback begins.

#### Request

```json
{
  "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `url` | string | **yes** | The URL to cast. May be a direct media URL (`.mp4`, `.webm`), an adaptive manifest (`.m3u8`, `.mpd`), or a web page URL. Only `http://` and `https://` schemes are allowed — `file://`, `data:`, and `javascript:` are rejected. |

#### Response: `202 Accepted`

```json
{
  "session_id": "550e8400-e29b-41d4-a716-446655440000",
  "status": "resolving"
}
```

The `202 Accepted` status indicates the URL has been accepted for resolution but playback has not yet begun. Use `GET /api/status` or the WebSocket to track state transitions through `resolving` → `buffering` → `playing`.

#### Error Responses

| Status | When |
|--------|------|
| `400 Bad Request` | Missing or invalid `url` field, or unsafe URL scheme |
| `409 Conflict` | A session is already active (boGDan supports one session at a time) |
| `422 Unprocessable Entity` | URL resolution failed (yt-dlp error, no streams found) |
| `500 Internal Server Error` | Playback engine unavailable or other internal error |

#### Example: Cast a YouTube video

```bash
curl -X POST http://192.168.1.100:8585/api/cast \
  -H "Content-Type: application/json" \
  -d '{"url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ"}'
```

#### Example: Cast a direct MP4

```bash
curl -X POST http://192.168.1.100:8585/api/cast \
  -H "Content-Type: application/json" \
  -d '{"url": "https://example.com/video.mp4"}'
```

Direct media URLs skip yt-dlp resolution and begin playback immediately.

---

### `POST /api/stop`

Stop the current playback session and release GStreamer pipeline resources.

#### Request

No request body required.

#### Response: `200 OK`

```json
{
  "session_id": null,
  "state": "idle",
  "source_url": null,
  "resolved_url": null,
  "position_ms": 0,
  "duration_ms": null,
  "volume": 100,
  "title": null
}
```

#### Error Responses

| Status | When |
|--------|------|
| `409 Conflict` | No active session to stop |

---

### `POST /api/pause`

Pause the current playback.

#### Request

No request body required.

#### Response: `200 OK`

```json
{
  "status": "paused"
}
```

#### Error Responses

| Status | When |
|--------|------|
| `409 Conflict` | No active session |

---

### `POST /api/resume`

Resume playback from paused state.

#### Request

No request body required.

#### Response: `200 OK`

```json
{
  "status": "playing"
}
```

#### Error Responses

| Status | When |
|--------|------|
| `409 Conflict` | No active session |

---

### `POST /api/seek`

Seek to a position within the current media.

#### Request

```json
{
  "position_seconds": 120.0
}
```

Alternative (milliseconds):

```json
{
  "position_ms": 120000
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `position_ms` | integer | no | Target position in milliseconds from start. Used if provided. |
| `position_seconds` | float | no | Target position in seconds from start. Used as fallback if `position_ms` is not provided. |

Both fields are optional but at least one should be provided. If neither is given, seeks to position 0.

#### Response: `200 OK`

```json
{
  "position_ms": 120000
}
```

#### Error Responses

| Status | When |
|--------|------|
| `409 Conflict` | No active session |

---

### `POST /api/volume`

Set the playback volume level.

#### Request

```json
{
  "volume": 75
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `volume` | integer | **yes** | Volume level 0–100. Values above 100 are clamped to 100. |

#### Response: `200 OK`

```json
{
  "volume": 75
}
```

#### Error Responses

| Status | When |
|--------|------|
| `409 Conflict` | No active session |
| `500 Internal Server Error` | Failed to set volume |

---

### `GET /api/status`

Get the current playback status and session metadata.

#### Request

No request body required.

#### Response: `200 OK`

```json
{
  "session_id": "550e8400-e29b-41d4-a716-446655440000",
  "state": "playing",
  "source_url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
  "resolved_url": "https://cdn.example.com/video.mp4",
  "position_ms": 47300,
  "duration_ms": 212000,
  "volume": 75,
  "title": "Rick Astley - Never Gonna Give You Up"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | string? | Session UUID, or `null` if idle |
| `state` | string | Current session state: `"idle"`, `"resolving"`, `"buffering"`, `"playing"`, `"paused"`, `"error"` |
| `source_url` | string? | Original URL cast by the user |
| `resolved_url` | string? | Direct CDN URL resolved by yt-dlp or custom resolver |
| `position_ms` | integer | Current playback position in milliseconds (0 if idle) |
| `duration_ms` | integer? | Total duration in milliseconds, or `null` if unknown |
| `volume` | integer | Volume level 0–100 |
| `title` | string? | Display title from yt-dlp metadata or sender |

When no session is active:

```json
{
  "session_id": null,
  "state": "idle",
  "source_url": null,
  "resolved_url": null,
  "position_ms": 0,
  "duration_ms": null,
  "volume": 100,
  "title": null
}
```

#### Example

```bash
curl http://192.168.1.100:8585/api/status
```

---

### `GET /api/health`

Health check endpoint. Returns a simple status object.

#### Response: `200 OK`

```json
{
  "status": "ok"
}
```

---

### `GET /api/audio-devices`

List available ALSA playback devices detected on the system. Includes ALSA hardware devices, PulseAudio sinks (if PulseAudio is running), and BlueALSA Bluetooth devices (if BlueALSA is running without PulseAudio).

#### Response: `200 OK`

```json
[
  {
    "device": "default",
    "card_name": "ALSA Default",
    "card_index": 0,
    "device_index": 0,
    "sink_type": "alsasink"
  },
  {
    "device": "plughw:1,0",
    "card_name": "vc4-hdmi",
    "card_index": 1,
    "device_index": 0,
    "sink_type": "alsasink"
  },
  {
    "device": "pulse",
    "card_name": "PulseAudio (auto Bluetooth)",
    "card_index": 99,
    "device_index": 0,
    "sink_type": "pulsesink"
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `device` | string | ALSA device string (e.g. `"plughw:1,0"`, `"pulse"`, `"bluealsa:DEV=XX:XX:XX:XX:XX:XX,PROFILE=a2dp"`) |
| `card_name` | string | Human-readable card name (e.g. `"vc4-hdmi"`, `"PulseAudio (auto Bluetooth)"`) |
| `card_index` | integer | ALSA card index number |
| `device_index` | integer | ALSA device index number |
| `sink_type` | string | GStreamer sink element to use (`"alsasink"` or `"pulsesink"`) |

---

### `POST /api/audio-device`

Set the audio output device and sink type for playback. Takes effect on the next playback session.

#### Request

```json
{
  "device": "plughw:1,0",
  "sink_type": "alsasink"
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `device` | string | **yes** | — | ALSA device string (e.g. `"plughw:1,0"` for HDMI, `"pulse"` for PulseAudio, `"bluealsa:DEV=...,PROFILE=a2dp"` for Bluetooth). Empty string = ALSA default device. |
| `sink_type` | string | no | `"alsasink"` | GStreamer sink element: `"alsasink"` for ALSA, `"pulsesink"` for PulseAudio. |

#### Response: `200 OK`

```json
{
  "device": "plughw:1,0",
  "sink_type": "alsasink"
}
```

---

### `GET /api/audio-device`

Get the currently configured audio output device.

#### Response: `200 OK`

```json
{
  "device": "plughw:1,0"
}
```

---

## Authentication

No authentication. The API is only accessible on the LAN. If exposed on the internet, add a token-based authentication layer (e.g., `Authorization: Bearer <token>` header).

## Rate Limiting

No rate limiting is applied. The API is only exposed on the local network. If exposed on the internet, add rate limiting middleware.
