# Tor Stream Isolation

PiCast uses Tor's `IsolateSOCKSAuth` feature to ensure that different websites use independent Tor circuits, preventing cross-site correlation attacks. This document describes the isolation mechanism, the SOCKS5 username hashing scheme, circuit lifecycle management, and the security properties it provides.

## Why Stream Isolation Matters

Without stream isolation, all of PiCast's traffic — YouTube, Vimeo, Twitch, and every other site — would share the same Tor exit relay. An observer controlling the exit relay (or performing traffic analysis on the exit relay's upstream) could correlate visits to different sites and build a profile of the user's viewing habits. Stream isolation prevents this by ensuring each site's traffic exits through a different circuit.

### Threat Model Without Isolation

```
User watches: youtube.com/video1, vimeo.com/video2, twitch.tv/video3

Without isolation (same circuit):
  Exit relay sees: requests for youtube.com, vimeo.com, twitch.tv
  → Exit relay operator can correlate all three sites to one user
  → Traffic analyst on exit relay's upstream can do the same

With isolation (separate circuits):
  Circuit A exits via relay X → youtube.com only
  Circuit B exits via relay Y → vimeo.com only
  Circuit C exits via relay Z → twitch.tv only
  → No single exit relay sees traffic to multiple sites
  → Correlation requires controlling multiple exit relays simultaneously
```

## How IsolateSOCKSAuth Works

Tor's `IsolateSOCKSAuth` flag is set on the SOCKS port in the torrc:

```torrc
SocksPort 9050 IsolateSOCKSAuth
```

When this flag is enabled, Tor maps SOCKS5 connections with different username/password combinations to separate circuits. Connections with the same username/password share a circuit. This is specified in the Tor manual as: "Isolate SOCKS authentication based on the username and password. Different SOCKS authentication values will result in different circuits."

The key insight is that the SOCKS5 username/password fields are used purely for circuit routing — they are not transmitted to the destination server. They are visible only on the loopback interface between PiCast and the local Tor daemon. This means we can encode site identity in the username field without any privacy risk.

## SOCKS5 Username Hashing Scheme

PiCast derives a unique SOCKS5 username from each site's hostname using SHA-256 hashing. The same hostname always produces the same username, ensuring consistent circuit assignment. Different hostnames produce different usernames, ensuring separate circuits.

### Algorithm

```
1. Take the destination hostname (e.g., "youtube.com")
2. Compute SHA-256 hash of the hostname bytes
3. Take the first 8 bytes (64 bits) of the hash
4. Hex-encode to produce a 16-character username string
5. Use the constant password "picast-isolation"
```

### Implementation

```rust
use sha2::{Sha256, Digest};
use hex;

fn socks5_credentials(site_host: &str) -> (String, String) {
    let mut hasher = Sha256::new();
    hasher.update(site_host.as_bytes());
    let hash = hasher.finalize();

    // First 8 bytes → 16 hex characters
    let username = hex::encode(&hash[..8]);
    let password = "picast-isolation".to_string();

    (username, password)
}
```

### Examples

| Site Host | SHA-256 (first 16 hex) | SOCKS5 Username | Password | Circuit |
|-----------|------------------------|-----------------|----------|---------|
| youtube.com | `7d2d3e1f4a5b6c8d...` | `7d2d3e1f4a5b6c8d` | `picast-isolation` | Circuit A |
| vimeo.com | `9e8f7a6b5c4d3e2f...` | `9e8f7a6b5c4d3e2f` | `picast-isolation` | Circuit B |
| twitch.tv | `a1b2c3d4e5f6a7b8...` | `a1b2c3d4e5f6a7b8` | `picast-isolation` | Circuit C |
| youtube.com | `7d2d3e1f4a5b6c8d...` | `7d2d3e1f4a5b6c8d` | `picast-isolation` | Circuit A (reused!) |

### Properties of the Hashing Scheme

1. **Deterministic**: The same hostname always produces the same username. This means all requests to the same site consistently use the same circuit, which is beneficial because CDN caches (like Google's edge servers) are per-exit-node — reusing the circuit means the CDN cache is warm.

2. **Collision-resistant**: SHA-256 truncated to 64 bits has a collision probability of approximately 1 in 4.3 billion (birthday bound). In practice, with ~1,800 media sites supported by yt-dlp, the probability of two sites sharing a circuit is negligible.

3. **No side-channel leakage**: The username is derived from the hostname but does not reveal the hostname. An observer who sees the username `7d2d3e1f4a5b6c8d` on the loopback interface cannot determine that it corresponds to `youtube.com` without brute-forcing the hash. Since the username is only visible on the loopback interface (between PiCast and the local Tor daemon), there is no network-side leakage.

4. **Subdomain handling**: PiCast uses the registered domain (e.g., `youtube.com` not `www.youtube.com` or `m.youtube.com`) for hashing. This ensures all subdomains of the same site share a circuit. The `url` crate's `domain()` method provides the registered domain via the public suffix list.

## SOCKS5 Handshake with Authentication

When PiCast establishes a connection through the Tor SOCKS5 proxy with stream isolation, it performs the full SOCKS5 authentication handshake as specified in RFC 1928 and RFC 1929:

```
1. Client sends greeting:
   +----+----------+----------+
   |VER | NMETHODS | METHODS  |
   +----+----------+----------+
   | 05 |    02    | 00 | 02  |    ← No auth (0x00) and Username/Password (0x02)
   +----+----------+----------+

2. Server selects method:
   +----+--------+
   |VER | METHOD |
   +----+--------+
   | 05 |   02   |              ← Server selects Username/Password (0x02)
   +----+--------+

3. Client sends credentials:
   +----+------+----------+------+----------+
   |VER | ULEN |  UNAME   | PLEN |  PASSWD  |
   +----+------+----------+------+----------+
   | 01 |  16  | 7d2d3e...|  16  | picast...|
   +----+------+----------+------+----------+

4. Server responds:
   +----+--------+
   |VER | STATUS |
   +----+--------+
   | 01 |   00   |          ← 0x00 = success
   +----+--------+

5. Client sends connect request:
   +----+-----+-------+------+----------+----------+
   |VER | CMD |  RSV  | ATYP | DST.ADDR | DST.PORT |
   +----+-----+-------+------+----------+----------+
   | 05 |  01 |  00   |  03  | hostname |   443    |
   +----+-----+-------+------+----------+----------+
   ← ATYP=0x03 means domain name (not IP address)
   ← This causes Tor to resolve the hostname (prevents DNS leaks)

6. Server responds with bound address:
   +----+-----+-------+------+----------+----------+
   |VER | REP |  RSV  | ATYP | BND.ADDR | BND.PORT |
   +----+-----+-------+------+----------+----------+
   | 05 |  00 |  00   |  01  |    IP    |   PORT   |
   +----+-----+-------+------+----------+----------+
   ← REP=0x00 means success
```

### Critical: ATYP=0x03 (Domain Name)

Step 5 uses ATYP=0x03 (domain name) instead of ATYP=0x01 (IPv4 address). This tells the Tor SOCKS5 proxy to resolve the hostname itself (through the Tor network), rather than having PiCast resolve it locally. This is the DNS leak prevention mechanism — at no point does PiCast's system resolver ever see the destination hostname.

## Circuit Lifecycle

### Circuit Creation

When PiCast connects to a new site (e.g., `vimeo.com` for the first time), the SOCKS5 handshake with the site-specific username triggers Tor to build a new circuit:

1. Tor selects an entry guard (from the configured 3 long-lived guards)
2. Tor selects a middle relay (random)
3. Tor selects an exit relay that allows traffic to the destination port
4. The three-hop circuit is established: entry → middle → exit
5. Future connections to `vimeo.com` reuse this circuit (same username → same circuit)

Circuit build time: 1–5 seconds for a warm consensus (descriptors cached).

### Circuit Rotation

Tor rotates circuits every 10 minutes by default. When rotation occurs:

1. New connections to the same site will use a new circuit
2. Existing connections continue on the old circuit until they close
3. GStreamer's `souphttpsrc` maintains a persistent HTTP connection during streaming, so the video stream continues on the old circuit
4. If the old circuit is forcibly closed, the `queue2` buffer absorbs the brief interruption (~200ms) while a new circuit is built

### Circuit Failure

If a circuit fails (exit relay goes offline, network interruption):

1. Tor automatically builds a new circuit for the same SOCKS5 username
2. The new circuit may use a different exit relay
3. Existing HTTP connections break — `souphttpsrc` reconnects with the same SOCKS5 credentials, which now route through the new circuit
4. If the new circuit also fails, `souphttpsrc` retries with exponential backoff
5. After 3 failures, the playback engine reports a `TOR_ERROR` to the session manager

## Rate Limiting and Stream Isolation

YouTube and other sites may rate-limit IP addresses that make too many requests. With stream isolation, YouTube traffic always exits through the same circuit (same exit relay IP), so rate limits are per-site rather than global. This is beneficial because:

- If YouTube rate-limits the exit relay, only YouTube traffic is affected
- Vimeo and other sites continue working on their own circuits with different exit relay IPs
- If a site blocks the exit relay's IP, the resolver can retry with a different SOCKS5 username (e.g., appending a retry counter) to get a different circuit and exit relay

## Future: arti Migration

The C Tor daemon's `IsolateSOCKSAuth` feature is the reason PiCast uses C Tor instead of the Rust-based arti client (ADR-004). When arti adds equivalent per-username circuit isolation support, PiCast can migrate to arti for the following benefits:

- In-process Tor client (no child process management, no IPC)
- Tokio-native async API (no SOCKS5 handshake implementation needed)
- Rust memory safety (no C buffer overflow vulnerabilities)
- Smaller attack surface (no separate process with root privileges)

Until arti supports this feature, the C Tor daemon with `IsolateSOCKSAuth` remains the only viable option for PiCast's stream isolation requirements.
