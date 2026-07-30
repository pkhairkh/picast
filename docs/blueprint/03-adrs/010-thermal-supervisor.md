# BP-ADR-010: Thermal supervisor with bitrate fallback above 80C

| Field        | Value          |
|--------------|----------------|
| **ID**       | BP-ADR-010        |
| **Status**   | PROPOSED       |
| **Date**     | 2026-07-30     |


| **Related** | BP-ADR-003 (V4L2 zero-copy pipeline), BP-ADR-006 (layered resolvers — bitrate fallback depends on resolver) |

## Context

Problem [[P-010]] requires CPU temperature to stay below 75C during 1080p playback and decode quality to be automatically reduced if temperature exceeds 80C. The V4L2 hardware decoder generates significant heat; without thermal management the Pi 4 throttles, causing frame drops. This is a nice-to-have for v1, but unmanaged throttling degrades the user experience for users without active cooling — a population that includes most Pi 4 owners with passive coolers.

## Decision

The playback supervisor polls `/sys/class/thermal/thermal_zone0/temp` every 5 s. Above 75C it emits a warning to `/api/status`. Above 80C it requests a lower-bitrate variant from the resolver (preferring `itag=18` 360p over higher itags on YouTube; equivalent lower-bitrate selection on other sites via yt-dlp format strings) and stretches the buffer window. Above 85C it pauses the pipeline and surfaces a user-visible 'cooling down' state until temperature drops below 75C. `/api/status` exposes `thermal_throttled: bool` and `cpu_temp_celsius: f32`; an OSD indicator is shown when throttling is active.

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ Temperature stays below 75C in the common case | Active cooling is not required for 1080p playback; matches P-010 success metric |
| ✅ Automatic graceful degradation | Above 80C the system downshifts bitrate rather than dropping frames or crashing |
| ✅ Observable thermal state | `/api/status` exposes temperature and throttling flag so users can correlate dropouts and the team can spot failing hardware |
| ✅ User-visible OSD indicator | User understands why quality dropped, rather than blaming their network |
| ❌ Quality degradation may confuse users | Lower-bitrate fallback degrades user-visible quality and may be mistaken for a network problem; mitigated by OSD indicator and user-guide documentation |
| ❌ Lower-bitrate selection depends on resolver | Not all sources have a lower-bitrate variant; the resolver must gracefully report 'no lower variant available' so the supervisor falls back to pausing |
| ❌ Polling adds minor CPU load | 5 s poll is negligible but non-zero; could be replaced by a netlink-based thermal event subscription in v2 |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **CPU frequency scaling via cpufreq governors** | Rejected because the actual heat source is the V4L2 decode block, not the CPU cores; cpufreq scaling mainly affects the parser/audio path and would not meaningfully reduce SoC temperature |
| **Active fan control via PWM** | Out of scope for v1 because most Pi 4 cases ship with passive coolers and PWM fan wiring is hardware-specific; revisit in v2 with a curated fan-HAT compatibility list |
| **Hard fail above 85C (no graceful degradation)** | Rejected because hard-failing a movie mid-playback is materially worse UX than downshifting to 360p; graceful degradation is the right default |
