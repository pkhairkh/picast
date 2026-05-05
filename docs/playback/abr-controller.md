# ABR Controller Algorithm

The Adaptive Bitrate (ABR) controller monitors GStreamer's buffer fill level and dynamically switches between quality tiers to avoid buffering interruptions on Tor's variable-bandwidth network path. This document specifies the algorithm, thresholds, gapless source switching procedure, and testing strategy.

## Overview

The ABR controller is a reactive, buffer-based quality adaptation algorithm. It polls GStreamer's `queue2` element every second for the current buffer fill percentage, applies hysteresis-based thresholds to decide whether to switch quality tiers, and enforces a minimum stability time between switches to prevent oscillation. The controller is intentionally simple — it does not estimate available bandwidth proactively but reacts to buffer conditions that already reflect bandwidth changes.

```
┌─────────────────────────────────────────────────────┐
│                  ABR Controller                      │
│                                                     │
│  ┌──────────────┐   every 1s   ┌──────────────┐   │
│  │ GStreamer    │─────────────▶│ Buffer       │   │
│  │ queue2       │   fill=0.85  │ Monitor      │   │
│  │ buffering    │              └──────┬───────┘   │
│  │ query        │                     │            │
│  └──────────────┘                     ▼            │
│                               ┌──────────────┐    │
│                               │ Decision     │    │
│                               │ Engine       │    │
│                               └──────┬───────┘    │
│                                      │             │
│                          ┌───────────┼──────────┐  │
│                          ▼           ▼          ▼  │
│                     Downshift    Stay       Upshift │
│                        │                     │     │
│                        ▼                     ▼     │
│                  ┌──────────────────────────────┐  │
│                  │   Resolver (new quality)     │  │
│                  │   → new media URL            │  │
│                  └──────────────┬───────────────┘  │
│                                 │                  │
│                                 ▼                  │
│                  ┌──────────────────────────────┐  │
│                  │   PlaybackEngine              │  │
│                  │   → gapless source switch     │  │
│                  └──────────────────────────────┘  │
└─────────────────────────────────────────────────────┘
```

## Quality Tiers

| Tier Index | Label | Target Resolution | Target Bitrate | Decoder | Tor Viability |
|-----------|-------|-------------------|---------------|---------|---------------|
| 0 | 360p | 640×360 | 800 Kbps | H.264 HW | Always works |
| 1 | 480p | 854×480 | 1.5 Mbps | H.264 HW | Works reliably |
| 2 | 720p | 1280×720 | 3 Mbps | H.264 HW | Works most of the time |
| 3 | 1080p | 1920×1080 | 5 Mbps | H.264 HW | Unreliable over Tor |

Default starting tier: **720p** (index 2). This is chosen because 720p H.264 at 3 Mbps is sustainable over most Tor exit relays, and 720p looks acceptable on most displays.

## Buffer Monitoring

### Data Source

The `PlaybackEngine::buffer_fill()` method returns the current buffer fill ratio (0.0–1.0) by querying GStreamer's `GstQueryBuffering` on the `queue2` element. This query returns a percentage (0–100) which is divided by 100 to produce a 0.0–1.0 ratio.

### Polling Interval

The ABR controller is invoked every **1 second** by a background task in `SessionManager`:

```rust
tokio::spawn(async move {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        interval.tick().await;
        let fill = playback.buffer_fill().await.unwrap_or(1.0);
        let now = current_time_secs();
        session.abr_update(fill, now).await;
    }
});
```

When `buffer_fill()` returns an error (e.g., pipeline is not in PLAYING state), a default value of 1.0 (100%) is used to avoid false downshifts.

### Buffer Fill Interpretation

| Fill Range | Status | Action |
|-----------|--------|--------|
| 0.80–1.00 | Healthy | No action. Consider upshift if stable. |
| 0.50–0.80 | Marginal | No action. Monitor closely. |
| 0.20–0.50 | Low | At risk. Next drop triggers downshift. |
| 0.00–0.20 | Critical | Downshift immediately (if stability timer allows). |

## Decision Engine

### Algorithm (Pseudocode)

```
function abr_update(fill: float, current_time: float):
    elapsed = current_time - stable_since

    // Guard: don't switch too quickly
    if elapsed < MIN_STABLE_TIME:
        return  // no decision this tick

    // Downshift: buffer is critically low
    if fill < DOWN_THRESHOLD and current_tier > 0:
        current_tier -= 1
        stable_since = current_time
        trigger_source_switch(current_tier)
        emit("abr_tier_change", from=current_tier+1, to=current_tier, reason="buffer_low")
        return

    // Upshift: buffer is very healthy
    if fill > UP_THRESHOLD and current_tier < MAX_TIER:
        current_tier += 1
        stable_since = current_time
        trigger_source_switch(current_tier)
        emit("abr_tier_change", from=current_tier-1, to=current_tier, reason="buffer_high")
        return

    // No change
```

### Threshold Table

| Parameter | Value | Rationale |
|-----------|-------|-----------|
| `DOWN_THRESHOLD` | 0.20 (20%) | Below 20%, playback will stall within seconds. Downshift to reduce bandwidth requirements. |
| `UP_THRESHOLD` | 0.80 (80%) | Above 80%, there is bandwidth headroom for higher quality. Upshift to improve visual quality. |
| `MIN_STABLE_TIME` | 10 seconds | Prevents oscillation on unstable Tor circuits. 10 seconds is long enough to distinguish sustained bandwidth changes from momentary fluctuations. |
| `MAX_TIER` | 3 (1080p) | Highest available quality tier. |
| `DEFAULT_TIER` | 2 (720p) | Starting tier for new sessions. Optimal for Tor bandwidth. |

### Transition Diagram

```
                DOWN_THRESHOLD (20%)
                    │
    ┌───────────────┼───────────────┐
    │               ▼               │
    │  ┌─────┐  ┌─────┐  ┌─────┐  │
    │  │360p │◀─│480p │◀─│720p │◀─│─ DOWN_THRESHOLD
    │  └──┬──┘  └──┬──┘  └──┬──┘  │
    │     │        │        │     │
    │     └────────┴───────┘┘     │
    │           │                  │
    │           ▼                  │
    │  ┌─────┐  ┌─────┐  ┌─────┐ │
    │  │480p │─▶│720p │─▶│1080p│ │─ UP_THRESHOLD
    │  └─────┘  └─────┘  └─────┘ │
    │               ▲              │
    └───────────────┼──────────────┘
                    │
                UP_THRESHOLD (80%)
```

The tier system is a simple linear progression. Downshifts move one tier at a time (no jumping from 1080p directly to 360p). This provides a smooth degradation experience and gives the ABR controller finer-grained control.

## Gapless Source Switch Procedure

When the ABR controller decides to switch tiers, the following sequence is executed. The goal is to minimize the visual disruption during the quality change.

### Step 1: Record Current Position

```rust
let (position, _) = playback.position().await?;
```

### Step 2: Re-resolve at New Quality

```rust
let new_quality = abr_config.tiers[new_tier].clone();
let resolved = resolver.resolve(&url, Some(&new_quality)).await?;
```

If the resolver returns a cached result (same URL, different quality tier was resolved earlier), this step takes 0–500ms. If yt-dlp must be re-invoked, it takes 2–10 seconds.

### Step 3: Rebuild Pipeline

```rust
playback.stop().await?;
playback.start(&resolved.media_url, resolved.media_type, None).await?;
```

The old pipeline is set to `State::Null` and dropped, releasing V4L2 decoder buffers and DRM framebuffers. The new pipeline is constructed with the new URL at the new quality tier.

### Step 4: Seek to Saved Position

```rust
playback.seek(position).await?;
```

The seek operation brings playback to the position where the quality switch was triggered, providing continuity.

### Timing Budget

| Step | Expected Time (Cached) | Expected Time (yt-dlp) |
|------|----------------------|----------------------|
| Record position | < 1 ms | < 1 ms |
| Re-resolve | 0–500 ms | 2–10 s |
| Pipeline teardown | ~50 ms | ~50 ms |
| Pipeline construction | ~100 ms | ~100 ms |
| Seek to position | ~200 ms | ~200 ms |
| **Total** | **~400 ms** | **~10.5 s** |

The 400ms cached case is barely noticeable. The 10.5s yt-dlp case causes a visible interruption (black frame + buffering indicator). The resolver should cache results aggressively to avoid the slow path.

## HLS vs Progressive ABR

### HLS (Preferred)

For HLS streams, the ABR controller can use `hlsdemux`'s built-in variant switching. Instead of rebuilding the pipeline, it sets the `bandwidth` property on the `hlsdemux` element:

```
hlsdemux bandwidth=<new_bandwidth>
```

This causes `hlsdemux` to switch to a different variant in the master playlist without a pipeline rebuild. This is much faster (~100ms) and truly gapless — the viewer sees no interruption.

### Progressive (Direct MP4)

For progressive (direct MP4) streams, there is no variant mechanism. The full pipeline teardown and rebuild is required. This is one of the reasons HLS is preferred over progressive download for PiCast.

## Network Quality Estimation (Future Enhancement)

A future enhancement could estimate available bandwidth independently of buffer fill, allowing the ABR controller to be proactive (switch down *before* the buffer runs low) rather than reactive:

```
estimated_bandwidth = bytes_downloaded / download_time
```

### Smoothing Filter

Raw bandwidth estimates are noisy (especially over Tor). Apply an exponential moving average:

```
smoothed_bw = α * measured_bw + (1 - α) * smoothed_bw
```

Where `α = 0.3` provides a balance between responsiveness to real changes and stability against noise.

### Proactive Downshift

With bandwidth estimation, the controller could downshift when `estimated_bandwidth < current_tier_bitrate * 1.2` (i.e., when the available bandwidth drops below 120% of the current tier's target bitrate), even if the buffer hasn't yet dropped below 20%. This would prevent buffer underruns entirely rather than reacting to them.

## Testing Strategy

### Unit Tests

1. Test `abr_update()` with synthetic fill sequences:
   - Monotonically decreasing fill (0.85, 0.70, 0.50, 0.30, 0.15) → triggers downshift at 0.15
   - Monotonically increasing fill (0.15, 0.30, 0.50, 0.70, 0.85) → triggers upshift at 0.85
   - Oscillating fill (0.15, 0.85, 0.15, 0.85) → no switch if MIN_STABLE_TIME has not elapsed
2. Test that tier changes stay within bounds (0–3). Downshift at tier 0 does nothing; upshift at tier 3 does nothing.
3. Test that the same fill value doesn't trigger multiple switches (idempotency).
4. Test MIN_STABLE_TIME guard: simulate fill < 20% followed by fill < 20% within 5 seconds → no second switch.

### Integration Tests

1. Stream a real HLS video with bandwidth throttling (`tc netem` to limit to 1 Mbps).
2. Verify that the ABR controller downshifts from 720p to 480p within 15 seconds.
3. Remove throttling and verify upshift from 480p to 720p within 20 seconds.
4. Measure the total switch time (position gap) for both cached and uncached resolution paths.

### Edge Cases

- **Resolver fails at new quality tier**: Stay at current tier, log error, retry in 30 seconds. Do not downshift further unless the buffer continues to drop.
- **New URL has different duration**: Re-seek by position (seconds), not by percentage. Different quality tiers may have slightly different durations due to encoder padding.
- **Switch during an ad**: Accept the brief glitch. Ad content is typically short (15–30 seconds) and does not warrant special handling.
- **Tor circuit rotation during switch**: The switch may fail if the Tor circuit rotates during re-resolution. The resolver's retry logic (with different stream isolation) handles this automatically.
