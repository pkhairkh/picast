# WebSocket Protocol Specification

PiCast exposes a WebSocket server on port **8081** for real-time bidirectional communication with sender applications. The WebSocket provides push-based state updates so clients don't need to poll `GET /api/v1/status`. Multiple clients may connect simultaneously — PiCast broadcasts state changes to all connected clients.

## Connection

```
ws://<pi-ip-address>:8081/ws
```

- **Protocol**: RFC 6455 (standard WebSocket)
- **Subprotocol**: None (no `Sec-WebSocket-Protocol` negotiation)
- **Ping/Pong**: Server sends ping every 30 seconds; clients that don't respond within 10 seconds are disconnected

## Message Format

All messages are JSON text frames with a `type` field for dispatch:

```json
{
  "type": "<message_type>",
  ...type-specific fields...
}
```

---

## Incoming Messages (Sender → PiCast)

### cast

Cast a URL. Equivalent to `POST /api/v1/cast`.

```json
{
  "type": "cast",
  "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
  "quality": "720p"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `url` | string | yes | URL to cast |
| `quality` | string | no | Preferred quality tier: "360p", "480p", "720p", "1080p", "best" |

---

### control

Playback control. Equivalent to `POST /api/v1/control`.

```json
{
  "type": "control",
  "action": "pause"
}
```

```json
{
  "type": "control",
  "action": "seek",
  "param": 45.0
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `action` | string | yes | "play", "pause", "stop", "seek" |
| `param` | number | conditional | Required for "seek" (position in seconds) |

---

### queue_remove

Remove an item from the queue. Equivalent to `DELETE /api/v1/queue/:id`.

```json
{
  "type": "queue_remove",
  "id": "550e8400-e29b-41d4-a716-446655440000"
}
```

---

### volume

Set the playback volume.

```json
{
  "type": "volume",
  "level": 0.75
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `level` | number | yes | Volume level 0.0–1.0 |

---

### subtitle

Select or disable subtitle track.

```json
{
  "type": "subtitle",
  "track": "en",
  "enabled": true
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `track` | string | yes | Language code from available subtitles, or "none" to disable |
| `enabled` | boolean | no | true to enable, false to disable. Default: true |

---

### ping

Keep-alive ping. The server responds with `pong`.

```json
{
  "type": "ping"
}
```

---

## Outgoing Messages (PiCast → Sender)

### state_change

Sent whenever the session state transitions. This is the primary mechanism for clients to track playback progress without polling.

```json
{
  "type": "state_change",
  "from": "resolving",
  "to": "loading",
  "timestamp": "2024-01-15T10:30:00Z"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `from` | string | Previous state: "idle", "resolving", "loading", "playing", "paused", "buffering", "error" |
| `to` | string | New state (same enumeration) |
| `timestamp` | string | ISO 8601 timestamp of the transition |

---

### progress

Sent approximately once per second during playback. Provides continuous position and buffer health updates.

```json
{
  "type": "progress",
  "position_secs": 127.5,
  "duration_secs": 212.0,
  "buffer_fill": 0.85
}
```

| Field | Type | Description |
|-------|------|-------------|
| `position_secs` | number | Current playback position in seconds |
| `duration_secs` | number | Total duration in seconds (-1.0 if unknown/live) |
| `buffer_fill` | number | Buffer fill ratio 0.0–1.0 (from GStreamer queue2) |

---

### queue_change

Sent whenever the queue contents change (item added or removed).

```json
{
  "type": "queue_change",
  "action": "added",
  "item": {
    "id": "550e8400-e29b-41d4-a716-446655440000",
    "url": "https://www.youtube.com/watch?v=abc123",
    "title": "Cool Video",
    "duration_secs": 300.0
  }
}
```

```json
{
  "type": "queue_change",
  "action": "removed",
  "id": "550e8400-e29b-41d4-a716-446655440000"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `action` | string | "added" or "removed" |
| `item` | object? | Full queue item (present for "added") |
| `id` | string? | Item UUID (present for "removed") |

---

### error

Sent when an asynchronous error occurs (resolver failure, pipeline error, Tor disconnection, buffer underrun).

```json
{
  "type": "error",
  "message": "yt-dlp failed: HTTP Error 403: Forbidden",
  "code": "RESOLVER_ERROR",
  "recoverable": true
}
```

| Field | Type | Description |
|-------|------|-------------|
| `message` | string | Human-readable error description |
| `code` | string | Machine-readable error category (see below) |
| `recoverable` | boolean | Whether PiCast can continue without user intervention |

### Error Codes

| Code | Description | Recoverable |
|------|-------------|-------------|
| `RESOLVER_ERROR` | yt-dlp or URL resolution failed | Maybe (retry with different quality) |
| `PIPELINE_ERROR` | GStreamer pipeline construction or runtime error | No (requires new cast) |
| `TOR_ERROR` | Tor daemon or SOCKS5 connection error | Yes (auto-restart) |
| `DRM_ERROR` | DRM/KMS display error | No (requires process restart) |
| `NETWORK_ERROR` | Network connectivity issue | Yes (ABR will adapt) |
| `BUFFER_UNDERRUN` | Buffer ran empty (ABR will react) | Yes (buffering state) |

---

### abr_tier_change

Sent when the ABR controller switches quality tier in response to buffer conditions.

```json
{
  "type": "abr_tier_change",
  "from": "1080p",
  "to": "720p",
  "reason": "buffer_low",
  "buffer_fill": 0.15
}
```

| Field | Type | Description |
|-------|------|-------------|
| `from` | string | Previous quality tier |
| `to` | string | New quality tier |
| `reason` | string | "buffer_low" (downshift) or "buffer_high" (upshift) |
| `buffer_fill` | number | Buffer fill at time of switch decision |

---

### pong

Response to a client `ping`.

```json
{
  "type": "pong"
}
```

---

## Connection Lifecycle

```
Client                              Server
  │                                   │
  │──── ws://<pi>:8081/ws ──────────▶│  Upgrade to WebSocket
  │◀─── 101 Switching Protocols ─────│
  │                                   │
  │◀─── state_change(idle) ──────────│  Initial state push on connect
  │                                   │
  │──── cast(url) ──────────────────▶│
  │◀─── state_change(resolving) ─────│
  │◀─── state_change(loading) ───────│
  │◀─── state_change(playing) ───────│
  │◀─── progress(127.5, 212.0, 0.85)─│  (1 Hz during playback)
  │◀─── progress(128.5, 212.0, 0.82)─│
  │                                   │
  │──── control(pause) ─────────────▶│
  │◀─── state_change(paused) ────────│
  │                                   │
  │──── control(stop) ──────────────▶│
  │◀─── state_change(idle) ──────────│
  │                                   │
  │──── Close frame ────────────────▶│
  │◀─── Close frame ─────────────────│
```

## Keep-Alive

- Client should send `ping` every 30 seconds.
- Server responds with `pong` immediately.
- If no `pong` is received within 10 seconds, the client should reconnect.
- Server sends WebSocket-level ping frames every 30 seconds as well.
- If no response within 10 seconds, the server closes the connection.

## Reconnection

If the WebSocket connection drops, the client should:

1. Wait 1 second, then attempt to reconnect.
2. On failure, wait 2 seconds, then attempt again.
3. Double the wait time up to a maximum of 30 seconds (exponential backoff).
4. On reconnection, the server pushes the current `state_change` message immediately so the client doesn't miss any state.

## Frame Size Limits

- Maximum incoming frame size: **1 MiB**
- Maximum outgoing frame size: **64 KiB**
- If a client sends a frame larger than 1 MiB, the server closes the connection with code 1009 (Message Too Big).

## Multiple Connections

The WebSocket server supports multiple simultaneous connections. All connected clients receive the same outgoing messages (state changes, progress, errors). Commands from any client are accepted on a first-come-first-served basis — there is no locking or session ownership.
