# boGDan Security Hardening Guide

**Audience:** DevOps engineers deploying boGDan on Raspberry Pi 4B+.
**Scope:** Network isolation, DNS leak prevention, circuit isolation, privilege
minimization, TLS, rate limiting, URL validation, and extension security.

---

## Table of Contents

1. [iptables Rules for Tor-Only Traffic](#1-iptables-rules-for-tor-only-traffic)
2. [Verifying No DNS Leaks](#2-verifying-no-dns-leaks)
3. [Verifying Circuit Isolation](#3-verifying-circuit-isolation)
4. [Privilege Minimization (DRM Master Only)](#4-privilege-minimization-drm-master-only)
5. [TLS Configuration (rustls, No TLS 1.0/1.1)](#5-tls-configuration-rustls-no-tls-1011)
6. [Rate Limiting (HTTP API)](#6-rate-limiting-http-api)
7. [URL Validation (No file://, data:, javascript:)](#7-url-validation-no-file-data-javascript)
8. [Extension Security Model](#8-extension-security-model)
9. [Automated Verification](#9-automated-verification)

---

## 1. iptables Rules for Tor-Only Traffic

All outbound internet traffic **must** be routed through the Tor SOCKS5 proxy.
Direct connections are prohibited by iptables rules with a default-DROP policy
on the OUTPUT chain.

### Reference Rules

The canonical rules file is `config/iptables.rules`. Apply with:

```bash
sudo iptables-restore < config/iptables.rules
sudo netfilter-persistent save
```

### OUTPUT Chain Design

The OUTPUT chain uses a **deny-by-default** model:

| Rule | Target | Purpose |
|------|--------|---------|
| `-A OUTPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT` | ACCEPT | Allow return traffic for established connections |
| `-A OUTPUT -o lo -j ACCEPT` | ACCEPT | Allow loopback (local IPC) |
| `-A OUTPUT -d 127.0.0.1/32 -p tcp --dport 9050 -j ACCEPT` | ACCEPT | Allow Tor SOCKS5 proxy |
| `-A OUTPUT -d 127.0.0.1/32 -p tcp --dport 9051 -j ACCEPT` | ACCEPT | Allow Tor control port |
| `-A OUTPUT -d 192.168.0.0/16 -j ACCEPT` | ACCEPT | Allow LAN traffic (DLNA, mDNS, SSH) |
| `-A OUTPUT -d 10.0.0.0/8 -j ACCEPT` | ACCEPT | Allow LAN traffic |
| `-A OUTPUT -d 172.16.0.0/12 -j ACCEPT` | ACCEPT | Allow LAN traffic |
| `-A OUTPUT -m owner --uid-owner debian-tor -j ACCEPT` | ACCEPT | Allow Tor daemon to reach relay nodes |
| `-A OUTPUT -d 127.0.0.1/32 -p udp --dport 53 -j ACCEPT` | ACCEPT | Allow DNS to localhost only (dnsmasq → Tor DNSPort) |
| `-A OUTPUT -j DROP` | DROP | Deny all other outbound traffic |

### Key Principles

1. **Default DROP**: The OUTPUT chain policy is DROP, so any traffic not
   explicitly allowed is silently discarded.

2. **Tor UID exemption**: Only the `debian-tor` user can make direct outbound
   connections. This allows the Tor daemon to reach relay nodes on dynamic ports
   while preventing all other processes from bypassing the proxy.

3. **No per-process exceptions**: boGDan must NOT have any direct outbound
   rules. All its traffic goes through `127.0.0.1:9050`.

4. **LAN traffic allowed**: DLNA discovery, mDNS, SSDP, and SSH need LAN
   access. These are not privacy-sensitive.

### Verification

```bash
# Check that default policy is DROP
sudo iptables -S OUTPUT | head -1
# Expected: -P OUTPUT DROP

# List all OUTPUT rules with packet counters
sudo iptables -L OUTPUT -v -n

# Run the automated verification script
sudo bash scripts/verify-network-isolation.sh
```

### Persisting Rules

```bash
# Method 1: netfilter-persistent (recommended for Debian/Raspbian)
sudo apt install iptables-persistent
sudo netfilter-persistent save

# Method 2: if-pre-up.d hook
sudo tee /etc/network/if-pre-up.d/iptables << 'EOF'
#!/bin/sh
/sbin/iptables-restore < /etc/iptables/rules.v4
EOF
sudo chmod +x /etc/network/if-pre-up.d/iptables
```

---

## 2. Verifying No DNS Leaks

DNS leaks are the most common privacy failure in Tor-routed systems. If DNS
queries go to the ISP's resolver instead of through Tor, the user's intent
is exposed even though HTTP traffic is routed correctly.

### How boGDan Prevents DNS Leaks

1. **SOCKS5h (remote DNS)**: boGDan uses `socks5h://` (not `socks5://`) for all
   proxy configurations. The `h` suffix forces DNS resolution through the Tor
   SOCKS proxy rather than locally.

2. **iptables blocks outbound DNS**: The only DNS rule in the OUTPUT chain
   allows UDP port 53 to `127.0.0.1` (the local dnsmasq stub). No external
   DNS is permitted.

3. **dnsmasq → Tor DNSPort**: The local DNS stub resolver forwards queries
   to Tor's DNSPort at `127.0.0.1:9053`, ensuring all DNS goes through Tor.

4. **yt-dlp uses `--proxy socks5h://`**: The resolver subprocess passes the
   `socks5h://` proxy URL to yt-dlp, which resolves hostnames remotely.

### Verification Steps

```bash
# 1. Verify iptables only allows localhost DNS
sudo iptables -S OUTPUT -v -n | grep 'dpt:53'
# Expected: only rules allowing DNS to 127.0.0.1

# 2. Verify Tor DNSPort is listening
ss -ulnp | grep ':9053'
# Expected: tor process on 127.0.0.1:9053

# 3. Test that external DNS is blocked
dig +short +timeout=3 @8.8.8.8 google.com
# Expected: timeout (no response)

# 4. Capture DNS traffic during playback (should show nothing)
sudo tcpdump -i eth0 -n 'udp port 53 and not dst host 127.0.0.1' -c 10
# Expected: no packets captured

# 5. Verify /etc/resolv.conf points to localhost
cat /etc/resolv.conf
# Expected: nameserver 127.0.0.1

# 6. Verify boGDan uses socks5h (not socks5)
grep -r 'socks5h' src/resolver/
# Should find proxy URLs using socks5h://
```

### dnsmasq Configuration

```bash
# /etc/dnsmasq.conf — forward to Tor DNSPort
server=127.0.0.1#9053
listen-address=127.0.0.1
bind-interfaces
```

### Troubleshooting

If DNS leaks are detected:

1. Check `/etc/resolv.conf` — it must point to `127.0.0.1`, not the router
2. Check that `dnsmasq` is running: `systemctl status dnsmasq`
3. Check Tor DNSPort: `grep DNSPort /etc/tor/torrc` — should have `DNSPort 9053`
4. Check boGDan's proxy URL — must be `socks5h://`, not `socks5://`

---

## 3. Verifying Circuit Isolation

Circuit isolation ensures that different websites use independent Tor circuits.
Without it, a single exit relay can see traffic to all sites, enabling
cross-site correlation attacks.

### How boGDan Implements Circuit Isolation

boGDan uses Tor's `IsolateSOCKSAuth` feature. Each unique SOCKS5 username
gets its own circuit. boGDan generates a deterministic username by hashing the
target hostname with SHA-256, so:

- `youtube.com` → circuit A
- `vimeo.com` → circuit B
- Same domain always uses the same circuit (deterministic)

### Tor Configuration

The `config/torrc` file configures stream isolation:

```
# Primary SOCKS5 port with per-site isolation
SocksPort 9050 IsolateSOCKSAuth

# Alternative port with per-IP isolation (for GStreamer)
SocksPort 9051 IsolateDestAddr

# boGDan-exclusive port with per-site isolation
SocksPort 29050 IsolateSOCKSAuth
```

### Verification Steps

```bash
# 1. Verify IsolateSOCKSAuth is configured
grep -E 'SocksPort.*IsolateSOCKSAuth' /etc/tor/torrc
# Expected: SocksPort 9050 IsolateSOCKSAuth

# 2. Verify boGDan generates per-domain SOCKS5 credentials
# Check the source code for the credential generation logic:
grep -r 'socks5_credentials\|isolation' src/tor/ src/resolver/
# Should find functions that hash domain names to usernames

# 3. Inspect active Tor circuits (requires ControlPort)
echo -e "AUTHENTICATE\r\nGETINFO circuit-status\r\nQUIT\r\n" | \
    nc 127.0.0.1 9052
# Look for circuits with different SOCKS usernames

# 4. Test with two different domains
# Cast YouTube → observe circuit
# Cast Vimeo → observe different circuit
```

### Circuit Rotation

- `MaxCircuitDirtiness 600` — circuits are rotated every 10 minutes (default
  is 10 minutes, which is fine for streaming but may be lowered for higher
  privacy)
- `NewCircuitPeriod 120` — new circuits are considered every 2 minutes
- `CircuitBuildTimeout 30` — circuits that take longer than 30s to build are
  abandoned

---

## 4. Privilege Minimization (DRM Master Only)

boGDan needs access to DRM/KMS for video rendering and ALSA for audio output.
These are granted through group membership, **not** root privileges.

### User and Group Setup

```bash
# Create the boGDan user (no login shell)
sudo useradd -r -m -s /usr/sbin/nologin bogdan

# Grant device access through group membership
sudo usermod -aG video,render,audio bogdan
```

| Group | Device | Purpose |
|-------|--------|---------|
| `video` | `/dev/dri/card0` | DRM/KMS master for video rendering |
| `render` | `/dev/dri/renderD128` | GPU rendering (V3D) |
| `audio` | `/dev/snd/*` | ALSA audio output |

### Systemd Service Hardening

The `config/bogdan.service` file includes security directives:

```ini
[Service]
User=bogdan
Group=bogdan
SupplementaryGroups=video render audio

# Prevent privilege escalation
NoNewPrivileges=true

# Filesystem restrictions
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/tmp/bogdan /var/lib/bogdan

# Kernel protections
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true

# Prevent namespace abuse
RestrictNamespaces=true

# Lock process personality and prevent W+X memory
LockPersonality=true
MemoryDenyWriteExecute=true

# Prevent realtime scheduling abuse
RestrictRealtime=true
```

### Verification

```bash
# Check process user
ps -o user,uid,groups -p $(pgrep bogdan)
# Expected: user=bogdan, uid>1000, groups includes video,render,audio

# Verify DRM device permissions
ls -la /dev/dri/card0
# Expected: crw-rw---- root video

# Check systemd security score
systemd-analyze security bogdan
# Expected: LOW exposure (good)

# Verify no X11/Wayland (DRM master must be boGDan only)
pgrep -la Xorg Xwayland weston mutter
# Expected: no results
```

### DRM Master

Only one process can hold DRM master. boGDan must be the sole DRM master:

```bash
# Check who holds DRM master
fuser /dev/dri/card0
# Expected: only the bogdan process
```

If X11 or Wayland is running, it holds DRM master and boGDan cannot render.
Use Raspberry Pi OS **Lite** (no desktop environment).

---

## 5. TLS Configuration (rustls, No TLS 1.0/1.1)

boGDan uses **rustls** for all TLS connections. openssl is banned by
`cargo-deny` to reduce C-library attack surface.

### Why rustls

- Pure Rust implementation — no C FFI, no buffer overflow risks
- No support for legacy protocols (TLS 1.0, TLS 1.1, SSLv3)
- No support for broken ciphers (RC4, DES, EXPORT)
- Default configuration follows Mozilla's server-side TLS guidelines
- Memory-safe: no use-after-free, no double-free, no buffer overflows

### Configuration

rustls is configured in `src/protocols/src/tls.rs` with:

```rust
// Only TLS 1.2 and 1.3 are supported
// TLS 1.0 and 1.1 are not implemented in rustls and cannot be enabled
let config = ClientConfig::builder()
    .with_safe_defaults()       // Uses safe cipher suite defaults
    .with_root_certificates(root_store)
    .with_no_client_auth();
```

### Cipher Suites

rustls supports only modern cipher suites:

| Protocol | Cipher Suites |
|----------|--------------|
| TLS 1.3 | `TLS_AES_256_GCM_SHA384`, `TLS_AES_128_GCM_SHA256`, `TLS_CHACHA20_POLY1305_SHA256` |
| TLS 1.2 | `TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384`, `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256`, `TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256`, `TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384`, `TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256`, `TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256` |

### Verification

```bash
# Verify openssl is not linked
ldd $(which bogdan) | grep -i ssl
# Expected: no output (no openssl linkage)

# Verify cargo-deny bans openssl
grep -A5 'ban' deny.toml | grep openssl
# Expected: openssl is in the ban list

# Check TLS version support
# TLS 1.0 and 1.1 are simply not implemented in rustls
# They cannot be accidentally enabled
```

### Certificate Verification

rustls uses the platform's root certificate store (via `rustls-native-certs`).
All certificates are verified against this store. Self-signed or expired
certificates are rejected.

---

## 6. Rate Limiting (HTTP API)

The boGDan HTTP API enforces per-IP rate limiting to prevent abuse.

### Configuration

Rate limiting is implemented in `src/protocols/src/http.rs`:

- **Limit**: 30 requests per 10 seconds per client IP
- **Response**: HTTP 429 Too Many Requests with `Retry-After` header
- **Scope**: All API endpoints (`/api/*`)
- **IP extraction**: `X-Forwarded-For` header (first IP) or direct socket IP

### Error Response Format

```json
{
  "error": "Rate limit exceeded",
  "code": "RATE_LIMITED",
  "status": 429,
  "retry_after_secs": 10
}
```

### Verification

```bash
# Send 31 rapid requests and verify the 31st gets 429
for i in $(seq 1 31); do
    CODE=$(curl -s -o /dev/null -w "%{http_code}" http://localhost:8585/api/health)
    echo "Request $i: HTTP $CODE"
done
# Expected: first 30 return 200, 31st returns 429

# Verify Retry-After header
curl -s -D- http://localhost:8585/api/health -H "X-Forwarded-For: 1.2.3.4"
# After rate limit: should include Retry-After header
```

### LAN-Only Access

In addition to rate limiting, the HTTP API should only be accessible from LAN
addresses. This is enforced by iptables:

```
-A INPUT -s 192.168.0.0/16 -p tcp --dport 8585 -j ACCEPT
-A INPUT -s 10.0.0.0/8     -p tcp --dport 8585 -j ACCEPT
```

No rule allows port 8585 from WAN addresses, so the API is not exposed
to the internet.

---

## 7. URL Validation (No file://, data:, javascript:)

boGDan validates all URLs before processing to prevent local file access,
code injection, and protocol smuggling.

### Blocked URL Schemes

| Scheme | Risk | Blocked |
|--------|------|---------|
| `file://` | Local file read (arbitrary file disclosure) | Yes |
| `data:` | Code injection via data URIs | Yes |
| `javascript:` | XSS / code execution | Yes |
| `ftp://` | Unencrypted protocol | Yes |
| `gopher://` | Protocol smuggling | Yes |
| `http://` | Unencrypted (downgraded from HTTPS) | Allowed with warning |
| `https://` | Encrypted — standard | Allowed |

### Validation Logic

URL validation is enforced at two levels:

1. **HTTP API layer** (`src/protocols/src/http.rs`): The `/api/cast` endpoint
   rejects URLs with blocked schemes before any processing begins.

2. **Resolver layer** (`src/resolver/src/lib.rs`): The resolver double-checks
   the URL scheme before passing it to yt-dlp or the direct media pipeline.

### Error Response

```json
{
  "error": "URL scheme 'file' is not allowed",
  "code": "INVALID_URL",
  "status": 400
}
```

### Verification

```bash
# Test blocked schemes — all should return 400
curl -X POST http://localhost:8585/api/cast \
    -H 'Content-Type: application/json' \
    -d '{"url": "file:///etc/passwd"}'
# Expected: 400 INVALID_URL

curl -X POST http://localhost:8585/api/cast \
    -H 'Content-Type: application/json' \
    -d '{"url": "data:text/html,<script>alert(1)</script>"}'
# Expected: 400 INVALID_URL

curl -X POST http://localhost:8585/api/cast \
    -H 'Content-Type: application/json' \
    -d '{"url": "javascript:alert(1)"}'
# Expected: 400 INVALID_URL

# Test allowed scheme — should return 202
curl -X POST http://localhost:8585/api/cast \
    -H 'Content-Type: application/json' \
    -d '{"url": "https://example.com/video.mp4"}'
# Expected: 202 Accepted
```

### Additional Validation

- **Malformed JSON**: Returns 400 `BAD_REQUEST`
- **Empty body**: Returns 400 `BAD_REQUEST`
- **Maximum body size**: 1 KB (larger requests are rejected)
- **Missing URL field**: Returns 400 `BAD_REQUEST`

---

## 8. Extension Security Model

The browser extension (Chrome/Firefox) sends URLs to boGDan for casting.
It must be hardened against XSS, CSP violations, and permission abuse.

### Content Security Policy

The extension enforces a strict CSP:

```
default-src 'self';
script-src 'self';
style-src 'self';
connect-src http://*:8585 http://*:8586 ws://*:8586;
img-src 'self' data:;
object-src 'none';
```

### Manifest Permissions

The extension uses **optional host permissions** to minimize scope:

```json
{
  "permissions": ["activeTab", "storage"],
  "optional_host_permissions": ["http://*/"]
}
```

Broad host permissions (`http://*/`) are optional and only requested when
the user explicitly enables the "detect media on all sites" feature.

### DOM Safety

The extension **never** uses `innerHTML` for rendering. All DOM manipulation
uses safe methods:

- `document.createElement()` + `textContent` for text content
- `element.classList` for CSS class manipulation
- `element.setAttribute()` for attributes

This prevents XSS through HTML injection.

### Message Validation

Messages between the content script, popup, and service worker are validated:

1. **Origin check**: Messages are only accepted from the extension's own origin
2. **Type validation**: Message type must be a known action
3. **Payload validation**: URL fields are validated before being sent to boGDan

### Verification

```bash
# Check CSP in manifest
grep -A5 'content_security_policy' src/extension/manifest-chrome.json

# Verify no innerHTML usage
grep -r 'innerHTML' src/extension/
# Expected: no results

# Check host permissions are optional
grep -A3 'optional_host_permissions' src/extension/manifest-chrome.json

# Verify URL validation in extension code
grep -r 'file://\|data:\|javascript:' src/extension/
# Expected: only in blocklists/validation code, not in user-facing paths
```

---

## 9. Automated Verification

Use the provided scripts to verify security posture on a deployed Pi:

### Network Isolation

```bash
# Full network isolation verification (requires root)
sudo bash scripts/verify-network-isolation.sh
```

This script:
- Sets up test iptables rules
- Verifies direct connections are blocked
- Monitors for leaks during active casting
- Checks DNS isolation
- Tests Tor failover (no fallback)
- Cleans up iptables on exit

### Memory Leak Detection

```bash
# 8-hour continuous playback with RSS/FD monitoring
bash scripts/mem-test.sh
```

### Resource Exhaustion

```bash
# 100 cast/stop cycles with resource tracking
bash scripts/soak-test.sh
```

### Security Audit Checklist

See `docs/SECURITY_AUDIT.md` for a comprehensive checklist with 11 items,
each including verification commands, expected results, and remediation steps.

---

## Quick Reference: Security Configuration Files

| File | Purpose |
|------|---------|
| `config/iptables.rules` | Firewall rules (Tor-only outbound) |
| `config/torrc` | Tor daemon configuration (stream isolation) |
| `config/bogdan.service` | Systemd service with hardening directives |
| `deny.toml` | Cargo deny rules (bans openssl/curl) |
| `src/protocols/src/tls.rs` | rustls TLS configuration |
| `src/protocols/src/http.rs` | HTTP API with rate limiting and URL validation |
| `src/extension/manifest-chrome.json` | Chrome extension CSP and permissions |
