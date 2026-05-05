# picast-protocols

Exposes PiCast on the local network through three protocol servers that sender applications (browser extension, phone app, DLNA controller) can use to discover and control the receiver. This crate is the **network-facing boundary** of PiCast — it translates external wire formats (HTTP JSON, WebSocket JSON, UPnP SOAP) into calls on `SessionManager`, ensuring that every protocol sees a consistent view of playback state.

## Purpose

The protocols crate implements all three control interfaces that external senders use to interact with PiCast: a RESTful HTTP API for programmatic control, a WebSocket channel for real-time bidirectional communication, and a UPnP/DLNA MediaRenderer for compatibility with VLC, Home Assistant, and Android DLNA apps. All three servers share a single `Arc<SessionManager>` and delegate every operation to it, ensuring that state changes from any protocol are immediately visible to clients on all protocols. The crate handles JSON serialization, CORS headers, WebSocket frame management, SSDP multicast, UPnP SOAP XML parsing and generation, and connection lifecycle management.

## Public API

| Struct | Role |
|--------|------|
| `HttpApiServer` | RESTful JSON API on port 8080 (hyper) |
| `WebSocketServer` | Real-time push + command channel on port 8081 (tokio-tungstenite) |
| `DlnaRenderer` | UPnP AVTransport + RenderingControl on port 8200, SSDP on 1900 |
| `ApiResponse<T>` | JSON envelope for HTTP responses with `ok`, `data`, `error` fields |
| `CastRequest` | Deserialized body of `POST /api/v1/cast` |
| `ControlRequest` | Deserialized body of `POST /api/v1/control` |
| `StatusResponse` | Payload of `GET /api/v1/status` |
| `WsMessage` | WebSocket JSON envelope (bidirectional, with `type` field for dispatch) |

### HTTP Endpoint Table

| Method | Endpoint | Handler | SessionManager Call | Description |
|--------|----------|---------|---------------------|-------------|
| POST | `/api/v1/cast` | `handle_cast` | `session.load(url)` | Submit URL for casting; triggers resolution and playback |
| POST | `/api/v1/control` | `handle_control` | `session.play/pause/stop/seek()` | Playback control: play, pause, stop, seek |
| GET | `/api/v1/status` | `handle_status` | `session.status()` | Current playback state, position, buffer fill, ABR tier |
| GET | `/api/v1/queue` | `handle_queue` | `session.queue()` | List items in the playback queue |
| DELETE | `/api/v1/queue/:id` | `handle_queue_remove` | `session.queue_remove(id)` | Remove a specific item from the queue |

### WebSocket Server→Client Message Types

| Type | Trigger | Payload Fields | Description |
|------|---------|----------------|-------------|
| `state_change` | Session state transition | `from`, `to`, `timestamp` | Notifies client of state machine transition (e.g., resolving→loading) |
| `progress` | Every 1 second during playback | `position_secs`, `duration_secs`, `buffer_fill` | Periodic playback position and buffer health update |
| `queue_change` | Queue mutation | `action` (added/removed), `item` or `id` | Queue item added or removed |
| `error` | Async error (resolver, pipeline, Tor) | `message`, `code`, `recoverable` | Error notification with category code |
| `abr_tier_change` | ABR quality switch | `from`, `to`, `reason`, `buffer_fill` | Quality tier changed due to buffer conditions |
| `pong` | Response to client `ping` | (none) | Keep-alive acknowledgment |

### WebSocket Client→Server Message Types

| Type | Payload Fields | Equivalent HTTP | Description |
|------|----------------|-----------------|-------------|
| `cast` | `url`, `quality` | `POST /api/v1/cast` | Cast a URL with optional quality preference |
| `control` | `action`, `param` | `POST /api/v1/control` | Playback control (play, pause, stop, seek) |
| `queue_remove` | `id` | `DELETE /api/v1/queue/:id` | Remove queue item by UUID |
| `volume` | `level` | (via control) | Set volume 0.0–1.0 |
| `ping` | (none) | N/A | Keep-alive; server responds with `pong` |

### DLNA Service Actions

| UPnP Service | Action | Maps To |
|--------------|--------|---------|
| AVTransport | `SetAVTransportURI` | `session.load(url)` |
| AVTransport | `Play` | `session.play()` |
| AVTransport | `Pause` | `session.pause()` |
| AVTransport | `Stop` | `session.stop()` |
| AVTransport | `Seek` | `session.seek()` |
| AVTransport | `GetTransportInfo` | `session.status()` → UPnP state mapping |
| AVTransport | `GetPositionInfo` | `session.status()` → HH:MM:SS format |
| RenderingControl | `SetVolume` | `session.set_volume()` (0–100 → 0.0–1.0) |
| RenderingControl | `GetVolume` | `session.status()` → volume (0.0–1.0 → 0–100) |
| RenderingControl | `SetMute` / `GetMute` | `session.set_volume(0)` / status check |
| ConnectionManager | `GetProtocolInfo` | Static list of supported MIME types |

## Dependencies

| Dependency | Why |
|------------|-----|
| `picast-session` | `Arc<SessionManager>` is the sole business-logic dependency; all commands are forwarded to it |
| `hyper` | HTTP server implementation with `service_fn` for per-connection handling |
| `tokio-tungstenite` | WebSocket server; upgrade from raw TCP, split into sender/receiver tasks |
| `roxmltree` | Parsing UPnP SOAP XML request bodies from DLNA controllers |
| `serde` / `serde_json` | JSON serialization for HTTP responses and WebSocket messages |
| `url` | URL validation on cast requests before forwarding to resolver |
| `tokio` | Async runtime, `net::TcpListener`, `sync::broadcast` for shutdown |

## Implementation Guide for AI Agents

### 1. HTTP API (`HttpApiServer`)

Implement the five endpoints defined in `docs/protocols/http-api.md`. Use `hyper::service::service_fn` for each connection. Route with a simple match on `(method, path)` — no framework needed. Return `ApiResponse<T>` as JSON with appropriate HTTP status codes (200, 400, 404, 409, 500). Add CORS headers (`Access-Control-Allow-Origin: *`) to every response for browser extension compatibility.

**Testing strategy**: Unit-test each handler with a mock `SessionManager` using the `ResolverTrait`, `PlaybackTrait`, `DisplayTrait`, `TorTrait` trait objects from `picast-session`. Use `hyper::Request::builder()` to construct test requests and assert on status code and response body.

### 2. WebSocket (`WebSocketServer`)

Accept TCP connections on port 8081, upgrade with `tokio-tungstenite`. Split each connection into a sender task and receiver task. The receiver task deserializes `WsMessage` and forwards to `SessionManager` (same operations as HTTP). The sender task subscribes to `SessionManager::subscribe()` and forwards state-change, progress, and error events. Implement ping/pong keep-alive (30-second interval, 10-second timeout). Limit incoming frame size to 1 MiB.

**Testing strategy**: Use `tokio-tungstenite::tungstenite::client` in integration tests to connect, send a `cast` message, and verify `state_change` messages arrive in the expected order.

### 3. DLNA Renderer (`DlnaRenderer`)

Three sub-components:

1. **SSDP advertiser** — Every 30 seconds, send `NOTIFY` to `239.255.255.250:1900` with `ST: upnp:rootdevice` and `ST: urn:schemas-upnp-org:device:MediaRenderer:1`. Also respond to `M-SEARCH` queries with a random delay of 0–MX seconds to avoid response storms.

2. **Device description** — Serve `/description.xml` on port 8200 with the full UPnP device template including AVTransport, RenderingControl, and ConnectionManager services (see `docs/protocols/dlna.md` for the complete XML).

3. **SOAP action handlers** — For AVTransport (`Play`, `Pause`, `Stop`, `Seek`, `SetAVTransportURI`) and RenderingControl (`SetVolume`, `GetVolume`). Translate each into `SessionManager` calls. Return UPnP-compliant SOAP XML responses.

**Testing strategy**: Use `curl` against the description URL and a minimal SSDP client to verify discovery. Send SOAP envelopes with `curl -X POST` and verify XML responses.

## Key Constraints

- **Single DRM master**: only one process can be DRM master at a time. The HTTP server must not attempt to open `/dev/dri/card0`; that belongs to `picast-display`. The protocols crate only interacts with the display through `SessionManager` → `DisplayTrait`.

- **Blocking calls**: `yt-dlp` invocations from the resolver are blocking and must not be called on the tokio runtime directly. Use `tokio::task::spawn_blocking` or ensure `SessionManager` already wraps resolver calls appropriately.

- **SSDP multicast**: the SSDP socket must bind to `0.0.0.0:1900` and join the multicast group on each network interface. On the Pi this is typically `wlan0` or `eth0`. Multiple services on the same host may conflict on port 1900 — use `SO_REUSEADDR` and `SO_REUSEPORT`.

- **WebSocket frame size**: limit incoming frames to 1 MiB to prevent memory exhaustion from misbehaving clients. Close connections exceeding this limit with code 1009 (Message Too Big).

- **DLNA timing**: UPnP controllers expect SOAP responses within 5 seconds. If the resolver is slow (yt-dlp takes 5–15 seconds), return a "TRANSITIONING" transport state immediately and push the final state over WebSocket when ready. Never block a SOAP handler on yt-dlp resolution.

- **Multiple connections**: the WebSocket server supports multiple simultaneous connections. All connected clients receive the same outgoing messages. Commands from any client are accepted on a first-come-first-served basis.

- **Error serialization**: all error responses must include a machine-readable `code` field and a human-readable `message` field. The WebSocket `error` message type uses the same error code taxonomy as the HTTP API.

## Reference

| Resource | Location |
|----------|----------|
| HTTP API full spec | `docs/protocols/http-api.md` |
| WebSocket protocol spec | `docs/protocols/websocket.md` |
| DLNA MediaRenderer spec | `docs/protocols/dlna.md` |
| mDNS/SSDP discovery | `docs/protocols/discovery.md` |
| Session state machine | `src/session/README.md` |
| SPECIFICATION.md API section | `SPECIFICATION.md` §2 |
| ADR-006: DLNA via gmediarender | `DECISIONS.md` |
| ADR-005: Cast V2 rejected | `DECISIONS.md` |
