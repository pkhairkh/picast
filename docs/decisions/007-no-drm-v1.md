# ADR-007: DRM Out of Scope for v1

| Field        | Value          |
|--------------|----------------|
| **ID**       | ADR-007        |
| **Status**   | ACCEPTED       |
| **Date**     | 2025-01-20     |
| **Supersedes** | —            |
| **Superseded by** | —         |

## Context

Digital Rights Management (DRM) is used by streaming services like Netflix, Disney+, Amazon Prime Video, and HBO Max to protect copyrighted content. The industry-standard DRM system for web browsers is Google's Widevine, which operates at two levels:

- **Widevine L1 (Level 1)**: Hardware-verified decryption. The media decryption keys are processed in a Trusted Execution Environment (TEE); decrypted video frames never touch main memory. This is required for HD/4K playback on most services.
- **Widevine L3 (Level 3)**: Software-only decryption. Decryption happens in a software Content Decryption Module (CDM). Decrypted frames are in main memory and can theoretically be captured. Most services limit L3 playback to SD (480p) quality.

### Widevine on Raspberry Pi 4

The Raspberry Pi 4B+ does **not** have Widevine L1 support. There is no TEE on BCM2711 that meets Widevine L1 requirements. Widevine L3 is theoretically available via the `libwidevinecdm.so` binary, but:

- **Unreliable on ARM**: The Widevine L3 CDM for ARM has a history of compatibility issues. Google provides the CDM binary for x86_64 and limited ARM platforms. The ARM build available for Pi 4 is not officially supported and breaks across Chrome/Chromium version updates.
- **Software decryption overhead**: L3 decryption on Pi 4's Cortex-A72 cores adds ~15–20% CPU overhead for 1080p content. Combined with the ~30% CPU needed for H.264 decoding (when hardware decode is available, which it isn't inside the Widevine CDM), the Pi 4 cannot sustain 1080p DRM playback.
- **No hardware decode in CDM**: The Widevine CDM performs decryption and decoding internally. On Pi 4, it uses software decode because V4L2 M2M is not accessible from within the CDM sandbox. This means the hardware H.264 decoder sits idle while the CPU struggles with software decode.
- **Proprietary CDM blobs**: Including `libwidevinecdm.so` in the boGDan image means distributing a proprietary binary blob. This conflicts with boGDan's goal of being a fully auditable, open-source appliance. The CDM binary is opaque — it cannot be audited for security vulnerabilities or privacy violations.

### Impact on boGDan

Without reliable DRM, boGDan cannot play:
- Netflix
- Disney+
- Amazon Prime Video
- HBO Max
- Hulu
- Any other Widevine-protected service

boGDan **can** play:
- YouTube (no DRM on most content)
- Direct media URLs (MP4, WebM, MKV)
- Media servers (Plex, Jellyfin for non-DRM content)
- Live streams (Twitch, HTTP Live Streaming)
- Any site supported by yt-dlp that doesn't require DRM

## Decision

DRM playback is out of scope for boGDan v1.0. No Widevine CDM will be included in the boGDan image. The `bogdan-resolver` crate will classify DRM-protected URLs and return a clear error message to the user: "This content requires DRM which is not supported on boGDan v1."

This decision will be re-evaluated when one or more of the following conditions are met:

1. **Widevine CDM improves on ARM**: Google provides an officially supported, stable Widevine L3 CDM for ARM64 with hardware decode passthrough.
2. **Alternative DRM emerges**: An open-source or permissive-license DRM system gains adoption by major streaming services.
3. **Pi 5+ provides L1**: A future Raspberry Pi model includes a TEE that meets Widevine L1 requirements.

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ No proprietary blobs | boGDan image is 100% open-source and auditable; no opaque CDM binary |
| ✅ Reduced attack surface | No Widevine CDM sandbox to maintain; no CDM update chain |
| ✅ Simpler compliance | No DRM license agreements; no CDM redistribution restrictions |
| ✅ Predictable performance | All playback uses V4L2 M2M hardware decode; no software decode fallback path |
| ❌ Cannot play Netflix/Disney+ | Major streaming services are inaccessible; this is the single most common user complaint expected |
| ❌ Perceived as incomplete | Users comparing boGDan to Chromecast will notice the DRM gap immediately |
| ❌ yt-dlp DRM errors | Some yt-dlp-supported sites return DRM errors; boGDan must handle these gracefully with clear user messaging |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **Widevine L3 CDM blob** | Unreliable on ARM; Google does not officially support Pi 4; breaks across Chrome updates; software decode cannot sustain 1080p; proprietary blob violates boGDan's open-source auditing goal |
