# HTTP API Specification

PiCast exposes a RESTful JSON API on port **8080** for sender applications (browser extension, phone app, CLI tools, Home Assistant integrations). All endpoints accept and return `application/json`. The API is served over plain HTTP (no TLS) because PiCast is a LAN-only device — TLS on a local network adds complexity without meaningful security benefit in a trusted-network model.

## Base URL

```
http://<pi-ip-address>:8080/api/v1
```

## Content Type

All request and response bodies use `application/json`. Requests with incorrect `Content-Type` receive a `415 Unsupported Media Type` response.

## Response Envelope

Every response follows this consistent structure:

```json
{
  "ok": true,
  "data": { ... },
  "error": null
}
```

On error:

```json
{
  "ok": false,
  "data": null,
  "error": "descriptive error message"
}
```

The `ok` field is the primary indicator of success or failure. The `data` field is present only on success; the `error` field is present only on failure.

---

## Endpoints

### 1. POST /api/v1/cast

Cast a URL to the Pi. The resolver will classify and resolve it, then begin playback.

#### Request

```json
{
  "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
  "quality": "720p"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `url` | string | yes | The URL to cast. May be a direct media URL, HLS/DASH manifest, or a web page URL. |
| `quality` | string | no | Preferred quality: "360p", "480p", "720p", "1080p", "best". Default: "720p" (optimal for Tor). |

#### Response (200 OK)

```json
{
  "ok": true,
  "data": {
    "id": "550e8400-e29b-41d4-a716-446655440000",
    "state": "resolving",
    "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
    "quality": "720p"
  },
  "error": null
}
```

The `state` field will be `"resolving"` initially. Use `GET /api/v1/status` or the WebSocket to track state transitions through `loading` → `playing`.

#### Error Responses

| Status | When |
|--------|------|
| 400 | Missing or invalid `url` field |
| 409 | Already resolving or playing another URL (PiCast supports one session at a time) |
| 500 | Resolver failed (yt-dlp error, network timeout, unsupported URL) |

#### Example: Cast a direct MP4

```bash
curl -X POST http://192.168.1.100:8080/api/v1/cast \
  -H "Content-Type: application/json" \
  -d '{"url": "https://example.com/video.mp4"}'
```

Direct media URLs skip yt-dlp resolution and begin playback immediately (state goes directly to `loading`).

#### Example: Cast a YouTube video at 1080p

```bash
curl -X POST http://192.168.1.100:8080/api/v1/cast \
  -H "Content-Type: application/json" \
  -d '{"url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ", "quality": "1080p"}'
```

Note: 1080p over Tor may not stream smoothly due to bandwidth constraints. The ABR controller will downshift to 720p if the buffer runs low.

#### Example: Cast with curl and jq for pretty output

```bash
curl -s -X POST http://192.168.1.100:8080/api/v1/cast \
  -H "Content-Type: application/json" \
  -d '{"url": "https://vimeo.com/123456"}' | jq .
```

---

### 2. POST /api/v1/control

Send a playback control command to the active session.

#### Request

```json
{
  "action": "pause"
}
```

```json
{
  "action": "seek",
  "param": 45.0
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `action` | string | yes | One of: "play", "pause", "stop", "seek" |
| `param` | number | conditional | Required for "seek" (position in seconds from start). Ignored for other actions. |

#### Actions

| Action | Param | Effect |
|--------|-------|--------|
| `play` | — | Resume playback from paused state |
| `pause` | — | Pause playback |
| `stop` | — | Stop playback and return to idle state |
| `seek` | position (seconds, float) | Seek to absolute position from start |

#### Response (200 OK)

```json
{
  "ok": true,
  "data": {
    "state": "paused",
    "position_secs": 47.3
  },
  "error": null
}
```

#### Error Responses

| Status | When |
|--------|------|
| 400 | Invalid action or missing param for seek |
| 409 | No media is currently loaded |
| 500 | GStreamer seek failed or pipeline error |

#### Example: Pause playback

```bash
curl -X POST http://192.168.1.100:8080/api/v1/control \
  -H "Content-Type: application/json" \
  -d '{"action": "pause"}'
```

#### Example: Seek to 2 minutes

```bash
curl -X POST http://192.168.1.100:8080/api/v1/control \
  -H "Content-Type: application/json" \
  -d '{"action": "seek", "param": 120.0}'
```

#### Example: Stop playback

```bash
curl -X POST http://192.168.1.100:8080/api/v1/control \
  -H "Content-Type: application/json" \
  -d '{"action": "stop"}'
```

---

### 3. GET /api/v1/status

Get the current playback status. This is a read-only endpoint with no request body.

#### Response (200 OK)

```json
{
  "ok": true,
  "data": {
    "state": "playing",
    "position_secs": 127.5,
    "duration_secs": 212.0,
    "current_url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
    "current_title": "Rick Astley - Never Gonna Give You Up",
    "abr_tier": "720p",
    "buffer_fill": 0.85,
    "volume": 0.75
  },
  "error": null
}
```

| Field | Type | Description |
|-------|------|-------------|
| `state` | string | Current session state: "idle", "resolving", "loading", "playing", "paused", "buffering", "error" |
| `position_secs` | number | Current playback position in seconds (0.0 if idle) |
| `duration_secs` | number | Total duration in seconds (-1.0 if unknown/live) |
| `current_url` | string? | Original URL cast by the user |
| `current_title` | string? | Title extracted from yt-dlp metadata or provided by sender |
| `abr_tier` | string? | Current ABR quality tier: "360p", "480p", "720p", "1080p" |
| `buffer_fill` | number? | Buffer fill ratio 0.0–1.0 (from GStreamer queue2) |
| `volume` | number? | Volume level 0.0–1.0 |

When no session is active:

```json
{
  "ok": true,
  "data": {
    "state": "idle",
    "position_secs": 0,
    "duration_secs": 0,
    "current_url": null,
    "current_title": null,
    "abr_tier": null,
    "buffer_fill": null,
    "volume": null
  },
  "error": null
}
```

#### Example

```bash
curl http://192.168.1.100:8080/api/v1/status | jq .
```

---

### 4. GET /api/v1/queue

Get the current playback queue. PiCast v1 supports single-item playback; the queue is defined for forward compatibility.

#### Response (200 OK)

```json
{
  "ok": true,
  "data": {
    "items": [
      {
        "id": "550e8400-e29b-41d4-a716-446655440000",
        "url": "https://www.youtube.com/watch?v=abc123",
        "resolved_url": "https://cdn.example.com/video1.mp4",
        "title": "First Video",
        "duration_secs": 300.0,
        "thumbnail": "https://i.ytimg.com/vi/abc123/hqdefault.jpg",
        "quality": "720p"
      }
    ],
    "count": 1
  },
  "error": null
}
```

#### Example

```bash
curl http://192.168.1.100:8080/api/v1/queue | jq .
```

---

### 5. DELETE /api/v1/queue/:id

Remove an item from the playback queue.

#### Request

No request body. The `:id` path parameter is the queue item's UUID.

#### Response (200 OK)

```json
{
  "ok": true,
  "data": {
    "removed": "550e8400-e29b-41d4-a716-446655440000"
  },
  "error": null
}
```

#### Error Responses

| Status | When |
|--------|------|
| 404 | Queue item not found |

#### Example

```bash
curl -X DELETE http://192.168.1.100:8080/api/v1/queue/550e8400-e29b-41d4-a716-446655440000
```

---

## CORS

The HTTP API sets the following CORS headers to allow browser extensions and local web apps to make cross-origin requests:

```
Access-Control-Allow-Origin: *
Access-Control-Allow-Methods: GET, POST, DELETE, OPTIONS
Access-Control-Allow-Headers: Content-Type
```

All endpoints respond to `OPTIONS` preflight requests with `204 No Content` and the above headers.

## Rate Limiting

No rate limiting is applied. The API is only exposed on the local network (LAN). If exposed on the internet, add rate limiting middleware (e.g., 10 requests per second per IP).

## Authentication

No authentication. The API is only accessible on the LAN. If exposed on the internet, add a token-based authentication layer (e.g., `Authorization: Bearer <token>` header).
