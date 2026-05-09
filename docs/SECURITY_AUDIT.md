# boGDan Security Audit Checklist

**Task:** T-9.7 — Security audit checklist
**Depends on:** T-9.4 — Network isolation verification
**Last updated:** 2026-05-06

This document provides a comprehensive security audit checklist for boGDan deployments.
Each item includes a description, verification commands, expected results, risk levels,
and remediation steps.

> **Reference configuration files:**
> - `config/iptables.rules` — Firewall rules (applied via `iptables-restore`)
> - `config/bogdan.service` — Systemd unit with hardening directives
> - `config/torrc` — Tor daemon configuration

---

## Checklist Summary

| # | Check | Risk Level | Status |
|---|-------|-----------|--------|
| 1 | All outbound via Tor SOCKS | Critical | ☐ |
| 2 | DNS queries only to Tor DNSPort | Critical | ☐ |
| 3 | Stream isolation (per-domain circuits) | High | ☐ |
| 4 | DRM master is only boGDan (no X11/Wayland) | High | ☐ |
| 5 | Process runs as `bogdan` user, not root | High | ☐ |
| 6 | No unnecessary listening ports | High | ☐ |
| 7 | Systemd service hardening | Medium | ☐ |
| 8 | iptables default policies are DROP | Critical | ☐ |
| 9 | No SUID binaries in boGDan's path | Medium | ☐ |
| 10 | Tor is not running as relay/exit | Critical | ☐ |
| 11 | Physical security recommendations | Medium | ☐ |

---

## 1. All Outbound Traffic Routed via Tor SOCKS

**Risk Level:** Critical
**Description:** All outbound internet traffic must be routed through the Tor SOCKS5 proxy at `127.0.0.1:9050`. No direct internet connections should be possible. This is the foundational privacy guarantee of boGDan — if traffic can bypass Tor, the user's viewing activity is exposed to their ISP and network observers.

### How to Verify

**Method A: iptables rule inspection**
```bash
# List all OUTPUT chain rules and verify only the following are ACCEPT:
#   - ESTABLISHED,RELATED connections
#   - Loopback (lo)
#   - 127.0.0.1:9050 and 127.0.0.1:9051 (Tor SOCKS)
#   - LAN destinations (192.168.0.0/16, 10.0.0.0/8, 172.16.0.0/12)
#   - UID-owner debian-tor (Tor daemon itself)
#   - 127.0.0.1:53/udp (local DNS stub)
sudo iptables -S OUTPUT -v -n
```

**Method B: Active traffic capture with tcpdump**
```bash
# Capture all non-SOCKS outbound traffic during playback
# This should produce NO output if isolation is working
sudo tcpdump -i eth0 -n 'not (dst host 127.0.0.1 or dst net 192.168.0.0/16 or dst net 10.0.0.0/8 or dst net 172.16.0.0/12)' -c 100
```

**Method C: Direct HTTP/HTTPS should fail**
```bash
# These should timeout — proving direct internet access is blocked
curl --noproxy '*' --connect-timeout 5 http://example.com
curl --noproxy '*' --connect-timeout 5 https://example.com
```

**Method D: SOCKS5 proxy should work**
```bash
# This should succeed — proving Tor routing works
curl --socks5-hostname 127.0.0.1:9050 https://check.torproject.org/api/ip
# Should return JSON with "IsTor":true
```

### Expected Result

- `iptables -S OUTPUT` shows only ESTABLISHED, loopback, LAN, Tor-UID, and localhost:9050/9051 as ACCEPT targets
- `tcpdump` captures no non-SOCKS outbound packets during active playback
- Direct `curl --noproxy` requests timeout
- SOCKS5 `curl --socks5-hostname` requests succeed with `IsTor: true`

### Remediation

1. Apply the firewall rules: `sudo iptables-restore < config/iptables.rules`
2. Persist the rules: `sudo netfilter-persistent save`
3. Verify no other application or service is modifying iptables rules
4. Check for VPN or proxy services that might override iptables: `systemctl list-units --type=service --state=running | grep -E 'vpn|proxy|clash|v2ray'`

---

## 2. DNS Queries Only to Tor DNSPort

**Risk Level:** Critical
**Description:** All DNS resolution must go through Tor's DNSPort (127.0.0.1:9053) or the Tor SOCKS5 proxy's built-in remote DNS resolution. If DNS queries leak to the ISP's resolver, the user's browsing intent is exposed even though the actual HTTP traffic is routed through Tor.

### How to Verify

**Method A: iptables OUTPUT chain DNS rules**
```bash
# Verify only localhost DNS is allowed outbound
sudo iptables -S OUTPUT -v -n | grep 'dpt:53'
# Expected: only rules allowing DNS to 127.0.0.1
```

**Method B: Test external DNS is blocked**
```bash
# Should fail/timeout — external DNS is blocked by iptables
dig +short +timeout=3 @8.8.8.8 google.com
dig +short +timeout=3 @1.1.1.1 google.com
```

**Method C: Verify Tor DNSPort is listening**
```bash
ss -ulnp | grep ':9053'
# Expected: Tor process listening on 127.0.0.1:9053/udp
```

**Method D: Capture DNS traffic during playback**
```bash
# Should show NO outbound DNS queries to non-localhost
sudo tcpdump -i eth0 -n 'udp port 53 and not dst host 127.0.0.1' -c 10
```

**Method E: Verify yt-dlp uses SOCKS5h (remote DNS)**
```bash
# Check that the proxy URL uses socks5h (h = remote DNS resolution)
# In boGDan code, verify the proxy string format:
grep -r 'socks5h' src/resolver/
# Should find: --proxy socks5h://username@127.0.0.1:9050/
```

### Expected Result

- Only DNS to 127.0.0.1 is allowed in iptables OUTPUT chain
- External DNS queries (to 8.8.8.8, 1.1.1.1, etc.) timeout
- Tor DNSPort 9053/udp is listening on localhost
- No outbound DNS packets captured by tcpdump during playback
- `socks5h://` (not `socks5://`) is used in all proxy configurations

### Remediation

1. Ensure `config/iptables.rules` has the OUTPUT rule: `-A OUTPUT -d 127.0.0.1/32 -p udp --dport 53 -j ACCEPT`
2. Ensure there is no rule allowing outbound DNS to non-localhost addresses
3. Add `DNSPort 9053` to `/etc/tor/torrc` if missing
4. Configure `/etc/resolv.conf` to point to `127.0.0.1` (dnsmasq stub)
5. Configure dnsmasq to forward to Tor DNSPort:
   ```
   # /etc/dnsmasq.conf
   server=127.0.0.1#9053
   ```

---

## 3. Stream Isolation (Different Domains → Different Circuits)

**Risk Level:** High
**Description:** boGDan must use Tor's `IsolateSOCKSAuth` feature to ensure that different websites use independent Tor circuits. Without stream isolation, a single exit relay sees traffic to all sites, enabling cross-site correlation attacks (building a profile of the user's viewing habits).

### How to Verify

**Method A: Verify torrc has IsolateSOCKSAuth**
```bash
grep -E 'SocksPort.*IsolateSOCKSAuth' /etc/tor/torrc
# Expected: SocksPort 9050 IsolateSOCKSAuth
```

**Method B: Verify different SOCKS5 usernames produce different circuits**
```bash
# Generate stream isolation IDs for two different domains
# In boGDan's Rust code:
#   socks5_credentials("youtube.com") → username "7d2d3e1f4a5b6c8d"
#   socks5_credentials("vimeo.com")   → username "9e8f7a6b5c4d3e2f"
# Different usernames → different circuits (IsolateSOCKSAuth)
```

**Method C: Inspect Tor circuits (if ControlPort enabled)**
```bash
# Connect to Tor control port and check circuit isolation
echo -e "AUTHENTICATE\r\nGETINFO circuit-status\r\nQUIT\r\n" | nc 127.0.0.1 9051
# Look for circuits with different SOCKS usernames
# Each unique username should have its own circuit
```

**Method D: Integration test**
```bash
# Run the network isolation verification script
sudo bash scripts/verify-network-isolation.sh
# Section 5 should confirm SOCKS5 proxy is working
```

### Expected Result

- `SocksPort 9050 IsolateSOCKSAuth` is present in torrc
- Different domains produce different SOCKS5 usernames
- Tor circuit list shows separate circuits for different SOCKS usernames
- Same domain always produces the same username (deterministic hashing)

### Remediation

1. Add `SocksPort 9050 IsolateSOCKSAuth` to `/etc/tor/torrc`
2. Ensure boGDan's `TorManager` generates SOCKS5 credentials using SHA-256 of the domain
3. Verify `souphttpsrc` and `yt-dlp` both pass the stream isolation username
4. See `docs/tor/stream-isolation.md` for the full isolation design

---

## 4. DRM Master is Only boGDan (No X11/Wayland)

**Risk Level:** High
**Description:** boGDan uses DRM/KMS directly (no display server) to render video. Only one process can hold DRM master at a time. If X11 or Wayland is running, it holds DRM master, and boGDan cannot render video. Additionally, running a display server increases attack surface (X11 has a history of privilege escalation vulnerabilities).

### How to Verify

**Method A: Check for display server processes**
```bash
# None of these should be running
pgrep -la Xorg
pgrep -la Xwayland
pgrep -la weston
pgrep -la mutter
pgrep -la kwin_wayland
```

**Method B: Check DRM master holder**
```bash
# Check which process has DRM master
sudo fuser /dev/dri/card0
# Expected: only the bogdan process
```

**Method C: Check DISPLAY and WAYLAND_DISPLAY environment**
```bash
# In the boGDan service context, these should not point to a display server
systemctl show bogdan | grep Environment
# Expected: DISPLAY=:0 (set for DRM, not X11)
# Expected: no WAYLAND_DISPLAY variable
```

**Method D: Verify boot configuration**
```bash
# Ensure no auto-login to desktop
grep -E 'autologin|startx' /etc/systemd/system/getty.target.wants/* 2>/dev/null
# Should find nothing — Pi OS Lite has no desktop

# Check that Raspberry Pi OS Lite (not Desktop) is installed
dpkg -l | grep -E 'xserver|xorg|wayland|weston' 2>/dev/null
# Should return nothing or only libraries (not the compositor)
```

### Expected Result

- No X11 or Wayland compositor processes are running
- boGDan is the sole DRM master holder on `/dev/dri/card0`
- Raspberry Pi OS Lite is installed (not the Desktop variant)
- No display server packages are installed

### Remediation

1. Use Raspberry Pi OS Lite, not Desktop: `sudo apt remove --purge xserver-* xorg-* wayland-* weston-*`
2. Disable auto-login to desktop: `sudo raspi-config` → Boot Options → Console
3. Ensure boGDan service starts after boot, not a desktop session
4. See `docs/decisions/001-no-display-server.md` for rationale

---

## 5. Process Runs as `bogdan` User, Not Root

**Risk Level:** High
**Description:** The boGDan process must run as the `bogdan` user, not root. DRM/KMS access is granted through group membership (`video`, `render`), not root privileges. Running as root would mean any vulnerability in boGDan or its dependencies (GStreamer, yt-dlp subprocess) could lead to full system compromise.

### How to Verify

**Method A: Check running process user**
```bash
# Get boGDan PID and check user
pid=$(pgrep -x bogdan)
ps -o user,uid,groups -p "$pid"
# Expected: user=bogdan, uid>1000, groups include video,render,audio
```

**Method B: Verify systemd service configuration**
```bash
grep -E '^User=|^Group=|^SupplementaryGroups=' /etc/systemd/system/bogdan.service
# Expected:
#   User=bogdan
#   Group=bogdan
#   SupplementaryGroups=video render audio
```

**Method C: Verify bogdan user exists and has correct groups**
```bash
id bogdan
# Expected: uid=<non-zero> gid=<non-zero> groups=bogdan,video,render,audio
```

**Method D: Verify DRM device permissions**
```bash
ls -la /dev/dri/card0
# Expected: crw-rw---- root video
# The 'bogdan' user is in the 'video' group, so can read/write

ls -la /dev/dri/renderD128
# Expected: crw-rw---- root render
# The 'bogdan' user is in the 'render' group, so can read/write
```

### Expected Result

- boGDan process runs as UID > 0 (not root)
- `User=bogdan` in systemd service file
- `bogdan` user belongs to `video`, `render`, and `audio` groups
- `/dev/dri/card0` is accessible via `video` group membership

### Remediation

1. Create the `bogdan` user: `sudo useradd -r -m -s /usr/sbin/nologin bogdan`
2. Add to required groups: `sudo usermod -aG video,render,audio bogdan`
3. Set `User=bogdan` and `Group=bogdan` in `config/bogdan.service`
4. Add `SupplementaryGroups=video render audio` to the service file
5. Ensure data directories are owned by bogdan: `sudo chown bogdan:bogdan /var/lib/bogdan /tmp/bogdan`

---

## 6. No Unnecessary Listening Ports

**Risk Level:** High
**Description:** boGDan should only listen on the ports required for its functionality. Any additional listening ports increase the attack surface and may indicate a misconfigured or compromised service.

### How to Verify

**Method A: List all listening TCP/UDP ports**
```bash
# List all TCP listening ports with process info
sudo ss -tlnp
# List all UDP listening ports with process info
sudo ss -ulnp
```

**Method B: Identify which process owns each port**
```bash
sudo lsof -i -P -n | grep LISTEN
```

**Method C: Scan from another machine on the LAN**
```bash
# From another device on the same network
nmap -sS -sU -p- <bogdan-ip>
```

### Expected Listening Ports

| Port | Protocol | Process | Purpose | Binding |
|------|----------|---------|---------|---------|
| 8585 | TCP | bogdan | HTTP API | 0.0.0.0 (LAN) |
| 8586 | TCP | bogdan | WebSocket | 0.0.0.0 (LAN) |
| 49152 | TCP | bogdan | DLNA MediaRenderer | 0.0.0.0 (LAN) |
| 9050 | TCP | tor | SOCKS5 proxy | 127.0.0.1 (loopback) |
| 9051 | TCP | tor | Control port | 127.0.0.1 (loopback) |

### Acceptable System Ports (if present)

| Port | Protocol | Purpose |
|------|----------|---------|
| 22 | TCP | SSH (administration) |
| 53 | UDP | dnsmasq (local DNS stub → Tor DNSPort) |
| 5353 | UDP | mDNS / Avahi (discovery) |
| 1900 | UDP | SSDP (DLNA discovery) |
| 68 | UDP | DHCP client |

### Expected Result

- Only the ports listed above are listening
- boGDan ports (8585, 8586, 49152) are bound to LAN interfaces
- Tor ports (9050, 9051) are bound to 127.0.0.1 only
- No unexpected ports are open, especially not on WAN-facing interfaces

### Remediation

1. Disable unnecessary services: `sudo systemctl disable --now <service>`
2. Check for accidentally installed services: `sudo ss -tlnp | grep -v -E ':(8585|8586|49152|9050|9051|22|53|5353|1900)\b'`
3. Ensure iptables INPUT chain restricts boGDan ports to LAN:
   ```bash
   sudo iptables -S INPUT | grep -E '8585|8586|49152'
   # Should show rules with -s 192.168.0.0/16 and -s 10.0.0.0/8
   ```
4. If a port is needed temporarily, bind it to 127.0.0.1 only

---

## 7. Systemd Service Hardening

**Risk Level:** Medium
**Description:** The boGDan systemd service must use security hardening directives to limit the impact of a potential compromise. Even if an attacker gains code execution within the boGDan process, these directives restrict what they can do (e.g., no new privileges, no kernel module loading, read-only filesystem except explicit paths).

### How to Verify

**Method A: Inspect service file for hardening directives**
```bash
cat /etc/systemd/system/bogdan.service
```

**Method B: Check effective security properties**
```bash
# View the process's security context
pid=$(pgrep -x bogdan)
cat /proc/$pid/status | grep -E 'NoNewPrivs|Seccomp'
systemctl show bogdan | grep -E 'ProtectSystem|NoNewPrivileges|ProtectHome'
```

### Required Directives

| Directive | Value | Purpose |
|-----------|-------|---------|
| `NoNewPrivileges` | `true` | Prevents privilege escalation via setuid |
| `ProtectSystem` | `strict` | Makes filesystem read-only except explicit paths |
| `ProtectHome` | `true` | Hides /home, /root, /run/user |
| `ReadWritePaths` | `/tmp/bogdan /var/lib/bogdan` | Only paths boGDan needs to write to |
| `PrivateTmp` | `false` | Shared tmp needed for yt-dlp subtitle files |
| `ProtectKernelTunables` | `true` | Prevents modifying kernel sysctl parameters |
| `ProtectKernelModules` | `true` | Prevents loading kernel modules |
| `ProtectControlGroups` | `true` | Prevents modifying cgroup configuration |
| `RestrictNamespaces` | `true` | Prevents creating new namespaces |
| `LockPersonality` | `true` | Locks process personality (prevents ABI changes) |
| `MemoryDenyWriteExecute` | `true` | Prevents W+X memory pages (hardens against code injection) |
| `RestrictRealtime` | `true` | Prevents realtime scheduling (DoS prevention) |
| `User` | `bogdan` | Process runs as unprivileged user |
| `Group` | `bogdan` | Process group is bogdan |
| `SupplementaryGroups` | `video render audio` | DRM/GPU/audio device access |

### Expected Result

- All required directives are present in the service file
- The running process shows `NoNewPrivs: 1` in `/proc/$pid/status`
- No writable paths exist outside `/tmp/bogdan` and `/var/lib/bogdan`

### Remediation

1. Copy the hardened service file: `sudo cp config/bogdan.service /etc/systemd/system/`
2. Reload systemd: `sudo systemctl daemon-reload`
3. Restart boGDan: `sudo systemctl restart bogdan`
4. Verify with: `systemd-analyze security bogdan` (should show low exposure level)

---

## 8. iptables Default Policies are DROP

**Risk Level:** Critical
**Description:** The default policies for all three iptables chains (INPUT, FORWARD, OUTPUT) must be DROP. This means any traffic not explicitly allowed by a rule is silently dropped. If any chain defaults to ACCEPT, any traffic not specifically blocked will be allowed through, violating the deny-by-default security model.

### How to Verify

**Method A: Check default policies**
```bash
sudo iptables -S | grep -E '^-P'
# Expected:
#   -P INPUT DROP
#   -P FORWARD DROP
#   -P OUTPUT DROP
```

**Method B: Verify iptables.rules file**
```bash
head -10 config/iptables.rules
# Should contain:
#   :INPUT DROP [0:0]
#   :FORWARD DROP [0:0]
#   :OUTPUT DROP [0:0]
```

**Method C: Test with an unexpected packet**
```bash
# Send a packet to a port that has no ACCEPT rule
# Should be dropped (no response)
nc -z -w 3 <bogdan-ip> 12345
# Expected: connection refused or timeout (DROP policy)
```

### Expected Result

- All three chains (INPUT, FORWARD, OUTPUT) have default policy DROP
- `config/iptables.rules` starts with the DROP policies
- Any traffic not matching an explicit ACCEPT rule is dropped

### Remediation

1. Apply the correct rules: `sudo iptables-restore < config/iptables.rules`
2. Persist: `sudo netfilter-persistent save`
3. If using `ufw`, it may set policies to ACCEPT — prefer raw `iptables-restore`
4. Add a startup hook to re-apply rules after reboot:
   ```bash
   # /etc/network/if-pre-up.d/iptables
   #!/bin/sh
   /sbin/iptables-restore < /etc/iptables/rules.v4
   ```

---

## 9. No SUID Binaries in boGDan's Path

**Risk Level:** Medium
**Description:** SUID (Set User ID) binaries run with the privileges of their owner (often root). If boGDan or any of its subprocesses (yt-dlp, GStreamer) can execute SUID binaries, a vulnerability in any of them could be leveraged for privilege escalation. We must ensure no SUID binaries exist in directories accessible to the `bogdan` user that aren't explicitly required.

### How to Verify

**Method A: Find SUID binaries in boGDan's PATH**
```bash
# Determine bogdan user's PATH
bogdan_path=$(sudo -u bogdan bash -c 'echo $PATH' 2>/dev/null || echo "/usr/local/bin:/usr/bin:/bin")

# Find SUID binaries in PATH directories
echo "$bogdan_path" | tr ':' '\n' | while read -r dir; do
    if [[ -d "$dir" ]]; then
        find "$dir" -perm -4000 -type f 2>/dev/null
    fi
done
```

**Method B: Find ALL SUID binaries on the system**
```bash
sudo find / -perm -4000 -type f 2>/dev/null
```

**Method C: Check boGDan binary itself is not SUID**
```bash
ls -la /usr/local/bin/bogdan
# Expected: -rwxr-xr-x (no 's' bit)
```

**Method D: Check yt-dlp is not SUID**
```bash
ls -la "$(which yt-dlp)"
# Expected: -rwxr-xr-x (no 's' bit)
```

### Expected Result

- boGDan binary (`/usr/local/bin/bogdan`) is NOT SUID
- yt-dlp is NOT SUID
- No unexpected SUID binaries in the `bogdan` user's PATH
- Standard system SUID binaries (sudo, passwd, ping, su) are acceptable but should be audited
- `NoNewPrivileges=true` in systemd service prevents SUID escalation from boGDan's process

### Remediation

1. Remove SUID bit from unnecessary binaries: `sudo chmod u-s /path/to/binary`
2. Ensure `NoNewPrivileges=true` is set in the systemd service (this is the primary defense)
3. Restrict the `bogdan` user's PATH to only necessary directories
4. Use `mount` options to prevent SUID on filesystems:
   ```bash
   # /etc/fstab — add nosuid to tmpfs mounts
   tmpfs /tmp tmpfs defaults,nosuid 0 0
   ```

---

## 10. Tor is Not Running as a Relay or Exit

**Risk Level:** Critical
**Description:** boGDan must run Tor as a client only. If Tor is configured as a relay or exit node, the Pi's bandwidth will be consumed by other users' traffic, and law enforcement may investigate the Pi's IP address for traffic exiting through it. An exit node configuration is especially dangerous as it makes the Pi's IP address visible as the source of arbitrary internet traffic.

### How to Verify

**Method A: Check torrc for relay/exit configuration**
```bash
grep -E '^(ORPort|ExitRelay|PublishServerDescriptor|ExitPolicy|BridgeRelay|RelayBandwidthRate|ServerDNSResolvConf)' /etc/tor/torrc
# Expected:
#   ExitRelay 0
#   PublishServerDescriptor 0
#   No ORPort line (or ORPort 0 / ORPort auto commented out)
#   ExitPolicy reject *:* (if present)
```

**Method B: Check Tor process flags**
```bash
# Look at Tor's actual configuration (not just the torrc file)
sudo grep -E 'ORPort|ExitRelay|relay' /var/log/syslog 2>/dev/null | grep tor
```

**Method C: Verify no ORPort is listening**
```bash
# ORPort typically listens on port 443 or 9001
ss -tlnp | grep -E ':(443|9001)\b'
# Expected: no Tor process on these ports
```

**Method D: Check Tor's reported role**
```bash
# If control port is enabled
echo -e "AUTHENTICATE\r\nGETINFO config/ExitRelay\r\nGETINFO config/ORPort\r\nQUIT\r\n" | nc 127.0.0.1 9051
# Expected: ExitRelay=0, ORPort not set
```

### Expected Result

- `ExitRelay 0` is set in torrc
- `PublishServerDescriptor 0` is set in torrc
- No `ORPort` directive (or `ORPort 0`)
- No exit policy allowing traffic
- No ORPort listening on the system
- Tor is running as a client-only process

### Remediation

1. Ensure torrc has:
   ```
   ExitRelay 0
   PublishServerDescriptor 0
   ```
2. Remove or comment out any `ORPort` directive
3. Add `ExitPolicy reject *:*` as a safety net
4. Restart Tor: `sudo systemctl restart tor`
5. Verify with: `sudo journalctl -u tor --since "5 min ago" | grep -i relay`

---

## 11. Physical Security Recommendations

**Risk Level:** Medium
**Description:** Physical access to the Raspberry Pi enables various attacks that cannot be mitigated by software alone. An attacker with physical access can modify the SD card, access the serial console (UART), or use GPIO pins to interact with the hardware. These recommendations reduce the risk of physical tampering.

### Recommendations

#### 11a. SD Card Encryption

**Description:** Encrypt the root filesystem to prevent offline modification or data extraction from the SD card.

**How to Verify:**
```bash
# Check if root filesystem is encrypted
lsblk -f
# Expected: crypto_LUKS or dm-crypt layer

# Check if / is an encrypted device
mount | grep 'on / '
# If using LUKS, should show a mapper device like /dev/mapper/root
```

**How to Implement:**
1. Use `cryptsetup` to create an encrypted root partition
2. Configure `initramfs` to prompt for passphrase on boot
3. For headless operation, consider a key file on a USB token
4. Alternative: use `dm-verity` for read-only verification of the root filesystem

**Risk if Skipped:** An attacker can remove the SD card, modify boGDan's binary or configuration, and reinsert it. They can also read cached Tor state and session data.

#### 11b. UART Serial Console Disabled

**Description:** The Raspberry Pi's UART serial console provides root shell access without authentication. Disable it to prevent physical attacks via the GPIO header.

**How to Verify:**
```bash
# Check if serial console is enabled
grep -E 'console=serial|console=ttyAMA0|console=ttyS0' /boot/cmdline.txt
# Expected: no serial console entries

# Check if serial getty service is running
systemctl is-active serial-getty@ttyAMA0.service 2>/dev/null
# Expected: inactive or not found
```

**How to Implement:**
1. Remove `console=serial0,115200` from `/boot/cmdline.txt`
2. Disable serial getty: `sudo systemctl disable serial-getty@ttyAMA0.service`
3. In `raspi-config`: Interfacing Options → Serial → No to login shell
4. Add to `/boot/config.txt`:
   ```
   # Disable Bluetooth (frees UART)
   dtoverlay=disable-bt
   ```

**Risk if Skipped:** An attacker with physical access can connect a USB-to-serial adapter to the GPIO header and get a root shell without authentication.

#### 11c. GPIO Pins Locked Down

**Description:** GPIO pins should not expose debug interfaces or allow untrusted input that could compromise the system.

**How to Verify:**
```bash
# Check for JTAG/debug interfaces
grep -E 'jtag|debug' /boot/config.txt
# If JTAG is enabled, disable it

# Check GPIO overlay configuration
grep 'dtoverlay' /boot/config.txt
# Should not expose debug interfaces
```

**How to Implement:**
1. Disable JTAG: add `dtoverlay=disable-jtag` to `/boot/config.txt`
2. Disable WiFi and Bluetooth if not used:
   ```
   dtoverlay=disable-wifi
   dtoverlay=disable-bt
   ```
3. Set GPIO pin modes explicitly in the boGDan application
4. Consider physical GPIO header covers for deployed appliances

**Risk if Skipped:** JTAG or debug interfaces could allow firmware-level attacks or direct memory access.

#### 11d. Boot Configuration Hardening

**Description:** Prevent unauthorized changes to the boot configuration that could compromise the system.

**How to Verify:**
```bash
# Check for secure boot
vcgencmd otp_dump 2>/dev/null | grep -E 'boot_lock|program_jtag_lock'
# Expected: boot_lock=1, program_jtag_lock=1

# Verify boot order (SD card only, no USB/network boot)
vcgencmd otp_dump 2>/dev/null | grep -E 'boot_order'
```

**How to Implement:**
1. Lock OTP (One-Time Programmable) settings:
   ```bash
   # WARNING: These are irreversible!
   # Lock JTAG: program_jtag_lock
   # Lock boot: boot_lock
   ```
2. Disable USB/network boot if only SD card is used
3. Set `program_boot_mode` to SD-only in OTP
4. Write-protect the boot partition: `mount -o ro,remount /boot`

**Risk if Skipped:** An attacker can boot from a USB device with a malicious kernel or modify the boot partition to inject code.

---

## Automated Verification

Run the network isolation verification script to automatically check items 1, 2, 5, 6, 7, 8, and 10:

```bash
sudo bash scripts/verify-network-isolation.sh
```

Exit code 0 = all checks pass, exit code 1 = one or more failures detected.

---

## Audit Log Template

| Date | Auditor | Checklist Item | Result | Notes |
|------|---------|---------------|--------|-------|
| YYYY-MM-DD | | 1. Outbound via Tor | ☐ Pass / ☐ Fail | |
| YYYY-MM-DD | | 2. DNS leak prevention | ☐ Pass / ☐ Fail | |
| YYYY-MM-DD | | 3. Stream isolation | ☐ Pass / ☐ Fail | |
| YYYY-MM-DD | | 4. DRM master | ☐ Pass / ☐ Fail | |
| YYYY-MM-DD | | 5. Non-root process | ☐ Pass / ☐ Fail | |
| YYYY-MM-DD | | 6. Listening ports | ☐ Pass / ☐ Fail | |
| YYYY-MM-DD | | 7. Systemd hardening | ☐ Pass / ☐ Fail | |
| YYYY-MM-DD | | 8. DROP policies | ☐ Pass / ☐ Fail | |
| YYYY-MM-DD | | 9. No SUID binaries | ☐ Pass / ☐ Fail | |
| YYYY-MM-DD | | 10. Tor client-only | ☐ Pass / ☐ Fail | |
| YYYY-MM-DD | | 11. Physical security | ☐ Pass / ☐ Fail | |
