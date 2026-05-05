# picast-session

Central state machine, playback queue, and adaptive bitrate controller. This is the "brain" of PiCast — every protocol server funnels commands through `SessionManager`. It owns the global playback state, coordinates the four subsystems via trait interfaces, persists the queue to SQLite, and monitors buffer health to drive ABR decisions.

## Purpose

The session crate owns and coordinates the global playback state, the FIFO queue of items to play, and the ABR logic that keeps playback smooth on the Pi's Tor-constrained network path. It depends on four trait interfaces (`ResolverTrait`, `PlaybackTrait`, `DisplayTrait`, `TorTrait`) so the concrete subsystems can be swapped or mocked. All protocol handlers (HTTP, WebSocket, DLNA) call into `SessionManager` and never touch subsystems directly, ensuring a single source of truth for playback state.

## Public API

| Item | Kind | Description |
|------|------|-------------|
| `SessionManager` | struct | Central coordinator; all commands go here. Wrap in `Arc` and share across protocol handlers. |
| `PlayerState` | enum | 6-state FSM: `Idle`, `Loaded`, `Playing`, `Paused`, `Buffering`, `Error` |
| `MediaSession` | struct | Persistent session record (id, source_url, resolved_url, state, position_ms, volume, timestamps) |
| `SessionError` | enum | Error variants: `NoActiveSession`, `NotFound(Uuid)`, `Database`, `Subsystem` |
| `QueueItem` | struct | A queue entry (id, url, title, duration, quality, position, created_at) |
| `AbrConfig` | struct | ABR thresholds and quality tiers (configurable) |
| `AbrState` | struct | Current ABR tier, stability timer, buffer fill history |
| `StatusSnapshot` | struct | Snapshot returned by `status()` — includes position, duration, buffer fill, ABR tier, volume |

### Trait Interfaces (defined in `interfaces.rs`)

| Trait | Methods | Description |
|-------|---------|-------------|
| `ResolverTrait` | `resolve(url)` → `Result<String>` | Resolve URL to direct media URL |
| `PlaybackTrait` | `play(url)`, `pause()`, `resume()`, `stop()`, `seek(pos_ms)`, `set_volume(vol)`, `position_ms()` | GStreamer pipeline control |
| `DisplayTrait` | `acquire()`, `release()`, `resolution()` | DRM/KMS plane management |
| `TorTrait` | `ensure_running()`, `socks_addr()`, `health_check()` | Tor daemon lifecycle |

### Key `SessionManager` Methods

| Method | Description |
|--------|-------------|
| `load(url)` | Create session → resolve URL → start playback |
| `play(session_id)` | Resume playback on the session |
| `pause(session_id)` | Pause playback |
| `stop(session_id)` | Stop playback, destroy session, release resources |
| `seek(session_id, position_ms)` | Seek to absolute position |
| `set_volume(session_id, volume)` | Set volume 0–100 |
| `status(session_id)` | Full state snapshot |
| `subscribe()` | Get a `watch::Receiver<PlayerState>` for real-time state push |
| `abr_update(fill, time)` | Called by buffer monitor every 1 second |

## Dependencies

| Dependency | Why |
|------------|-----|
| `picast-resolver` | Via `ResolverTrait` trait object — resolves URLs through yt-dlp and Tor |
| `picast-playback` | Via `PlaybackTrait` trait object — GStreamer pipeline construction and control |
| `picast-display` | Via `DisplayTrait` trait object — DRM/KMS plane acquisition and mode setting |
| `picast-tor` | Via `TorTrait` trait object — Tor daemon lifecycle and SOCKS5 proxy address |
| `rusqlite` | Queue and session persistence (SQLite at `/var/lib/picast/sessions.db`) |
| `tokio` | Async runtime, `watch` channel for state notifications, `spawn_blocking` for SQLite |
| `uuid` | Session and queue item identifiers (UUID v4) |
| `serde` / `serde_json` | JSON serialization for status snapshots and WebSocket messages |
| `chrono` | Timestamps for session creation and update times |
| `async-trait` | Trait objects for subsystem interfaces |
| `thiserror` | Structured error types |

## State Machine

```
                    ┌─────────────────────────────────────────────────────────┐
                    │                                                         │
      load(url)     │   resolve OK       pipeline READY      play() /         │
    ────────────▶  Loaded  ──────────▶  Playing  ◀──────────────────────┘   │
                    │    │                   │  ▲                              │
                    │    │                   │  │  resume()                    │
                    │    │ resolve error     │  │                              │
                    │    ▼                   │  │                              │
                    │  Error ◀───────     Paused                             │
                    │    │             │     ▲                                │
                    │    │             │     │  pause()                       │
                    │    │             ▼     │                                │
                    │    │          Buffering ──── buffer > 80% ──▶ Playing   │
                    │    │             ▲                                     │
                    │    │             │                                     │
                    │    │     buffer < 20%                                   │
                    │    │             │                                     │
                    │    └─── stop() ◀─┴─── stop() ◀─── stop()               │
                    │                                                         │
                    └─── Idle (initial state) ◀──── stop() from any state ──┘
```

### Full Transition Table

| # | From State | To State | Trigger | Side Effect | Guard Condition |
|---|-----------|----------|---------|-------------|-----------------|
| 1 | Idle | Loaded | `load(url)` called | Create `MediaSession`, invoke `ResolverTrait::resolve(url)`, set state to Loaded on success | No active session exists |
| 2 | Idle | Error | `load(url)` called, resolve fails | Log error, notify client via WebSocket `error` message | — |
| 3 | Loaded | Playing | Pipeline reports READY | Call `PlaybackTrait::play(resolved_url)`, start ABR monitoring timer | `resolved_url` is Some |
| 4 | Loaded | Error | Pipeline construction fails | Call `PlaybackTrait::stop()`, log error, transition to Error | — |
| 5 | Playing | Paused | `pause()` called | Call `PlaybackTrait::pause()`, stop ABR timer | — |
| 6 | Paused | Playing | `play()` / `resume()` called | Call `PlaybackTrait::resume()`, restart ABR timer | — |
| 7 | Playing | Buffering | Buffer fill drops below 20% | Show buffering OSD, continue ABR monitoring | ABR detects low buffer |
| 8 | Buffering | Playing | Buffer fill rises above 80% | Hide buffering OSD, resume normal ABR | ABR detects recovered buffer |
| 9 | Buffering | Error | Buffer stalls for > 30 seconds | Call `PlaybackTrait::stop()`, report network error | Timeout expired |
| 10 | Playing | Idle | `stop()` called | Call `PlaybackTrait::stop()`, destroy session, release resources | — |
| 11 | Paused | Idle | `stop()` called | Call `PlaybackTrait::stop()`, destroy session | — |
| 12 | Buffering | Idle | `stop()` called | Call `PlaybackTrait::stop()`, destroy session | — |
| 13 | Error | Idle | `stop()` called | Destroy session, clear error state | — |
| 14 | Playing | Playing | ABR tier change | Re-resolve at new quality, rebuild pipeline, seek to saved position | `MIN_STABLE_TIME` elapsed since last switch |
| 15 | Playing | Loaded | Source URL changed (ABR switch) | Stop old pipeline, start new pipeline with new URL | New URL obtained from resolver |

## Queue Persistence Schema

```sql
CREATE TABLE IF NOT EXISTS queue_items (
    id          TEXT PRIMARY KEY,   -- UUID v4
    url         TEXT NOT NULL,
    resolved_url TEXT,
    title       TEXT,
    duration_secs REAL,
    thumbnail   TEXT,
    quality     TEXT,
    position    INTEGER NOT NULL,   -- FIFO ordering
    created_at  TEXT NOT NULL        -- ISO 8601
);

CREATE INDEX idx_queue_position ON queue_items(position);
```

On startup, `SessionManager` loads items ordered by `position` into a `VecDeque<QueueItem>`. On every mutation (add/remove), the SQLite table is updated within a transaction to survive crashes. Use WAL mode (`PRAGMA journal_mode=WAL`) and batch writes rather than one transaction per mutation to reduce SD card wear.

## ABR Threshold Table

| Metric | Downshift | Upshift |
|--------|-----------|---------|
| Buffer fill | < 20% | > 80% |
| Min stable time | 10 s | 10 s |
| Default tier | — | 720p (index 2) |
| Tiers (low→high) | 360p → 480p → 720p → 1080p | |

When a tier change is triggered, `SessionManager` calls `ResolverTrait::resolve()` again with the new quality, then performs a **gapless source switch** via `PlaybackTrait::stop()` followed by `PlaybackTrait::play(new_url)` and `PlaybackTrait::seek(saved_position)`. The `MIN_STABLE_TIME` guard prevents rapid oscillation between tiers on unstable Tor circuits.

## Implementation Guide for AI Agents

1. **Implement state transitions first** — they are the core logic. Write unit tests for every entry in the transition table above (15 transitions). Each test should: set initial state, call the trigger method, assert the new state and side effects.

2. **Stub the traits** — create mock implementations of `ResolverTrait`, `PlaybackTrait`, `DisplayTrait`, and `TorTrait` that return `Ok(())` / empty data so you can test `SessionManager` in isolation. Use `#[automock]` from the `mockall` crate or hand-write simple stubs.

3. **Add queue persistence** — use `rusqlite` with the schema above. Test crash-recovery by inserting items, simulating a crash (drop the struct), creating a new `SessionManager`, and verifying the queue was reloaded correctly.

4. **ABR controller** — the `abr_update()` method is pure logic with no I/O. Test it with synthetic buffer fill sequences: monotonically decreasing (should downshift), monotonically increasing (should upshift), oscillating (should not switch due to `MIN_STABLE_TIME`).

5. **Wire up real subsystems** — last step, after `SessionManager` itself is solid and all state machine tests pass with mocks.

6. **Watch channel** — `SessionManager` owns a `tokio::sync::watch::Sender<PlayerState>`. Protocol servers call `subscribe()` to get a `Receiver`. Every state transition updates the watch, which triggers WebSocket push notifications. Only `SessionManager` writes to the sender — never the protocol handlers.

## Key Constraints

- **No blocking on the async runtime**: resolver calls (yt-dlp) must go through `tokio::task::spawn_blocking`. SQLite operations must also use `spawn_blocking` because `rusqlite` is synchronous. Never call `.await` on a blocking I/O function directly on the tokio runtime.

- **Watch channel is single-producer**: only `SessionManager` writes to `state_tx`. Protocol servers must only read from `subscribe()`. Violating this will cause `watch::Sender::send()` to fail silently.

- **Queue size limit**: cap the queue at 100 items to prevent unbounded memory growth. Return a `SessionError::Subsystem("queue full")` if the limit is exceeded.

- **ABR oscillation**: the `MIN_STABLE_TIME` (10 seconds) guard prevents rapid flipping between tiers on unstable networks. Do not bypass or reduce this value — Tor circuit rotation causes bandwidth oscillation that would trigger constant tier changes without this guard.

- **SQLite on SD card**: the Pi's SD card has limited write endurance (~10K–100K write cycles per block). Use WAL mode, batch writes, and avoid writing the full session state on every position update. Only persist on state transitions and queue mutations.

- **Error state is sticky**: once the state machine enters `Error`, the only valid transition is `Error → Idle` via `stop()`. Do not attempt to auto-recover from the Error state without user intervention — the error may indicate a fundamental problem (e.g., unsupported codec) that retrying will not solve.

- **Single active session**: PiCast supports one playback session at a time. If `load()` is called while a session is active, return `SessionError::Subsystem("session already active")` with the existing session ID.

## Reference

| Resource | Location |
|----------|----------|
| Trait interfaces | `src/session/src/interfaces.rs` |
| Session manager | `src/session/src/lib.rs` |
| ABR algorithm | `docs/playback/abr-controller.md` |
| State machine spec | `SPECIFICATION.md` §2.2 |
| Queue persistence | This file (Schema section) |
| Architecture overview | `ARCHITECTURE.md` §2.2 |
