# WebSocket Protocol Reference

boGDan exposes a WebSocket server on port **8586** (configurable via `BOGDAN_WS_ADDR` or `bogdan.toml`) for real-time bidirectional communication with sender applications. The WebSocket provides push-based state updates so clients do not need to poll `GET /api/status`. Multiple clients may connect simultaneously (up to 32 concurrent connections) — boGDan broadcasts state changes to all connected clients.

## Connection

```
ws://<pi-ip-address>:8586/ws
```

When TLS is enabled:

```
wss://<pi-ip-address>:8586/ws
```

- **Protocol**: RFC 6455 (standard WebSocket)
- **Subprotocol**: None (no `Sec-WebSocket-Protocol` negotiation)
- **Ping/Pong**: Server sends WebSocket-level ping every 30 seconds; clients that do not respond within 10 seconds are disconnected
- **Connection limit**: Maximum 32 concurrent clients; connections beyond this limit receive an `ERROR` event and are closed
- **Max frame size**: 1 MiB for both incoming and outgoing messages

## Message Format

All messages are JSON text frames with a `type` field using SCREAMING_SNAKE_CASE:

```json
{
  "type": "<MESSAGE_TYPE>",
  ...type-specific fields...
}
```

---

## Client → Server Messages (Commands)

### `CAST`

Cast a URL. Equivalent to `POST /api/cast`.

```json
{
  "type": "CAST",
  "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `url` | string | **yes** | URL to cast. Only `http://` and `https://` schemes are allowed. |

---

### `STOP`

Stop the current playback session. Equivalent to `POST /api/stop`.

```json
{
  "type": "STOP"
}
```

---

### `PAUSE`

Pause the current playback. Equivalent to `POST /api/pause`.

```json
{
  "type": "PAUSE"
}
```

---

### `RESUME`

Resume playback from paused state. Equivalent to `POST /api/resume`.

```json
{
  "type": "RESUME"
}
```

---

### `SEEK`

Seek to a position within the current media. Equivalent to `POST /api/seek`.

```json
{
  "type": "SEEK",
  "position_ms": 120000
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `position_ms` | integer | **yes** | Target position in milliseconds from start |

---

### `VOLUME`

Set the playback volume. Equivalent to `POST /api/volume`.

```json
{
  "type": "VOLUME",
  "volume": 75
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `volume` | integer | **yes** | Volume level 0–100. Values above 100 are clamped. |

---

### `PING`

Application-level keep-alive. The server responds with a `PONG` event. This is distinct from WebSocket protocol-level ping/pong frames — browser extensions cannot send WS-level pings and need this application-level equivalent.

```json
{
  "type": "PING"
}
```

---

## Server → Client Messages (Events)

### `CONNECTED`

Sent immediately upon successful WebSocket connection. Confirms the connection is established.

```json
{
  "type": "CONNECTED"
}
```

---

### `MEDIA_STATUS`

Sent whenever the playback state changes or the position updates (approximately every 2 seconds during playback). This is the primary mechanism for clients to track playback state without polling.

```json
{
  "type": "MEDIA_STATUS",
  "state": "playing",
  "position_ms": 47300,
  "duration_ms": 212000,
  "volume": 75,
  "source_url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
  "title": "Rick Astley - Never Gonna Give You Up"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `state` | string | Current session state: `"idle"`, `"resolving"`, `"buffering"`, `"playing"`, `"paused"`, `"seeking"`, `"error"` |
| `position_ms` | integer | Current playback position in milliseconds |
| `duration_ms` | integer? | Total duration in milliseconds, or `null` if unknown |
| `volume` | integer | Volume level 0–100 |
| `source_url` | string? | Original URL cast by the user |
| `title` | string? | Display title from yt-dlp metadata |

Sent on: play, pause, stop, seek, volume change, position update (every 2 seconds during playback).

---

### `RESOLVE_PROGRESS`

Sent during URL resolution and buffering to provide progress indication.

```json
{
  "type": "RESOLVE_PROGRESS",
  "percent": 50
}
```

| Field | Type | Description |
|-------|------|-------------|
| `percent` | integer | Progress percentage 0–100 (buffering fill level) |

Also sent as a placeholder (percent: 0) when the session transitions through `Created`, `Resolving`, or `Resolved` states.

---

### `ERROR`

Sent when an error occurs — resolution failure, pipeline error, CDN rejection, or invalid client command.

```json
{
  "type": "ERROR",
  "message": "CDN rejected request (403 Forbidden) — Tor exit IP mismatch, re-resolving…"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `message` | string | Human-readable error description |

Common error scenarios:
- URL resolution failure (yt-dlp error, unsupported URL)
- CDN 403 Forbidden (Tor exit IP mismatch — triggers automatic re-resolution)
- Invalid client command (malformed JSON, unknown type)
- Playback engine failure (GStreamer pipeline error)
- Too many connections (when the 32-client limit is exceeded)

---

### `PONG`

Response to a client `PING` — application-level keep-alive.

```json
{
  "type": "PONG"
}
```

---

## Connection Lifecycle

```
Client                                Server
  │                                     │
  │──── ws://<pi>:8586/ws ────────────▶│  Upgrade to WebSocket
  │◀─── 101 Switching Protocols ───────│
  │                                     │
  │◀─── CONNECTED ─────────────────────│  Initial connection confirmation
  │                                     │
  │──── CAST(url) ────────────────────▶│
  │◀─── RESOLVE_PROGRESS(percent:0) ──│  Resolution started
  │◀─── MEDIA_STATUS(state:resolving)─│
  │◀─── RESOLVE_PROGRESS(percent:50) ─│  Buffering at 50%
  │◀─── MEDIA_STATUS(state:playing) ──│  Playback began
  │◀─── MEDIA_STATUS(position:47300) ─│  Position update (~2s interval)
  │◀─── MEDIA_STATUS(position:49300) ─│
  │                                     │
  │──── PAUSE ────────────────────────▶│
  │◀─── MEDIA_STATUS(state:paused) ───│
  │                                     │
  │──── RESUME ───────────────────────▶│
  │◀─── MEDIA_STATUS(state:playing) ──│
  │                                     │
  │──── STOP ─────────────────────────▶│
  │◀─── MEDIA_STATUS(state:idle) ─────│
  │                                     │
  │──── Close frame ──────────────────▶│
  │◀─── Close frame ───────────────────│
```

## Keep-Alive

- Server sends WebSocket-level ping frames every 30 seconds.
- Clients that do not respond with a pong within 10 seconds are disconnected.
- Browser extensions that cannot send WS-level pings should send application-level `PING` messages at a similar interval and wait for `PONG` responses.

## Reconnection

If the WebSocket connection drops, the client should:

1. Wait 1 second, then attempt to reconnect.
2. On failure, wait 2 seconds, then attempt again.
3. Double the wait time up to a maximum of 30 seconds (exponential backoff).
4. On reconnection, the server sends a `CONNECTED` event immediately. The client should then query `GET /api/status` to obtain the current playback state.

## Multiple Connections

The WebSocket server supports up to 32 simultaneous connections. All connected clients receive the same outgoing events (media status, resolve progress, errors). Commands from any client are accepted on a first-come-first-served basis — there is no locking or session ownership. Connections beyond the limit receive an `ERROR` event with a "too many connections" message and are immediately closed.
