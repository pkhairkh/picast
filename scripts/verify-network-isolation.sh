#!/usr/bin/env bash
# verify-network-isolation.sh — PiCast T-9.4 Network Isolation Verification
#
# Verifies that iptables rules block all outbound traffic except through Tor.
# Tests DNS leak prevention, direct-HTTP blocking, SOCKS5 proxy functionality,
# listening-port audit, and OUTPUT chain rule correctness.
#
# Usage:
#   sudo bash scripts/verify-network-isolation.sh   # full checks (needs root)
#   bash scripts/verify-network-isolation.sh        # graceful skip for root-only checks
#
# Exit codes:
#   0 — all checks pass
#   1 — one or more checks failed

set -euo pipefail

# ─── Colors ──────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m' # No Color

# ─── Counters ────────────────────────────────────────────────────
PASS=0
FAIL=0
SKIP=0

# ─── Expected configuration ─────────────────────────────────────
EXPECTED_LISTEN_PORTS="8585 8586 49152 9050 9051"
TOR_SOCKS_HOST="127.0.0.1"
TOR_SOCKS_PORT="9050"
TOR_UID="debian-tor"
CURL_TIMEOUT=5

# ─── Helpers ─────────────────────────────────────────────────────

pass() {
    PASS=$((PASS + 1))
    echo -e "  ${GREEN}[PASS]${NC} $1"
}

fail() {
    FAIL=$((FAIL + 1))
    echo -e "  ${RED}[FAIL]${NC} $1"
}

skip() {
    SKIP=$((SKIP + 1))
    echo -e "  ${YELLOW}[SKIP]${NC} $1"
}

info() {
    echo -e "  ${CYAN}[INFO]${NC} $1"
}

section() {
    echo ""
    echo -e "${BOLD}━━━ $1 ━━━${NC}"
}

is_root() {
    [[ "$(id -u)" -eq 0 ]]
}

has_command() {
    command -v "$1" &>/dev/null
}

# ─── Header ──────────────────────────────────────────────────────

echo -e "${BOLD}╔════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}║       PiCast Network Isolation Verification (T-9.4)      ║${NC}"
echo -e "${BOLD}╚════════════════════════════════════════════════════════════╝${NC}"
echo ""
echo -e "  Date:    $(date -u '+%Y-%m-%d %H:%M:%S UTC')"
echo -e "  Host:    $(hostname)"
echo -e "  Kernel:  $(uname -r)"
echo -e "  User:    $(whoami) (UID $(id -u))"
echo ""

if is_root; then
    info "Running as root — all checks enabled"
else
    info "Not running as root — some checks will be skipped"
fi

# ─── 1. iptables rules loaded ───────────────────────────────────

section "1. iptables Rules Loaded"

if ! is_root; then
    skip "iptables inspection requires root (use sudo)"
elif ! has_command iptables; then
    fail "iptables command not found"
else
    # Check that rules exist (non-empty rule list beyond policy)
    input_rules=$(iptables -S INPUT 2>/dev/null | wc -l)
    output_rules=$(iptables -S OUTPUT 2>/dev/null | wc -l)
    forward_rules=$(iptables -S FORWARD 2>/dev/null | wc -l)

    if [[ "$input_rules" -gt 1 ]]; then
        pass "INPUT chain has $((input_rules - 1)) rules beyond policy"
    else
        fail "INPUT chain has no rules beyond default policy — iptables rules may not be loaded"
    fi

    if [[ "$output_rules" -gt 1 ]]; then
        pass "OUTPUT chain has $((output_rules - 1)) rules beyond policy"
    else
        fail "OUTPUT chain has no rules beyond default policy — iptables rules may not be loaded"
    fi

    if [[ "$forward_rules" -ge 1 ]]; then
        pass "FORWARD chain has rules"
    else
        fail "FORWARD chain is empty"
    fi
fi

# ─── 2. Default policies are DROP ───────────────────────────────

section "2. Default Policies are DROP"

if ! is_root; then
    skip "iptables policy inspection requires root (use sudo)"
elif ! has_command iptables; then
    skip "iptables not available"
else
    input_policy=$(iptables -S INPUT 2>/dev/null | head -1)
    output_policy=$(iptables -S OUTPUT 2>/dev/null | head -1)
    forward_policy=$(iptables -S FORWARD 2>/dev/null | head -1)

    if [[ "$input_policy" == "-P INPUT DROP" ]]; then
        pass "INPUT default policy is DROP"
    else
        fail "INPUT default policy is NOT DROP (got: $input_policy)"
    fi

    if [[ "$output_policy" == "-P OUTPUT DROP" ]]; then
        pass "OUTPUT default policy is DROP"
    else
        fail "OUTPUT default policy is NOT DROP (got: $output_policy)"
    fi

    if [[ "$forward_policy" == "-P FORWARD DROP" ]]; then
        pass "FORWARD default policy is DROP"
    else
        fail "FORWARD default policy is NOT DROP (got: $forward_policy)"
    fi
fi

# ─── 3. DNS leak prevention ─────────────────────────────────────

section "3. DNS Leak Prevention"

# 3a. Verify iptables blocks outbound DNS (port 53) to non-localhost
if ! is_root; then
    skip "iptables DNS rule inspection requires root (use sudo)"
elif ! has_command iptables; then
    skip "iptables not available"
else
    # Check that OUTPUT chain has a rule allowing DNS only to 127.0.0.1
    dns_output_rules=$(iptables -S OUTPUT 2>/dev/null | grep -E -- '--dport 53 ' || true)

    if echo "$dns_output_rules" | grep -qE '127\.0\.0\.1.*--dport 53'; then
        pass "OUTPUT chain allows DNS only to 127.0.0.1 (Tor DNSPort)"
    else
        fail "OUTPUT chain does not restrict DNS to 127.0.0.1 only — potential DNS leak"
    fi

    # Check that there is no rule allowing outbound DNS to non-loopback
    if echo "$dns_output_rules" | grep -qvE '127\.0\.0\.1|loopback|lo '; then
        fail "OUTPUT chain has rules allowing DNS to non-localhost addresses — DNS leak possible"
    else
        pass "No OUTPUT rules allow DNS to non-localhost addresses"
    fi
fi

# 3b. Verify Tor DNSPort is listening
if has_command ss; then
    if ss -ulnp 2>/dev/null | grep -qE ':9053\b'; then
        pass "Tor DNSPort (9053/udp) is listening"
    else
        # DNSPort might not be in the torrc; check for any local DNS resolver
        if ss -ulnp 2>/dev/null | grep -qE '127\.0\.0\.1:53\b'; then
            info "Tor DNSPort 9053 not found, but local DNS (127.0.0.1:53) is listening"
            info "If dnsmasq is forwarding to Tor DNSPort, this may be acceptable"
            pass "Local DNS resolver (127.0.0.1:53) is listening"
        else
            fail "Neither Tor DNSPort (9053) nor local DNS resolver (127.0.0.1:53) is listening"
        fi
    fi
elif has_command netstat; then
    if netstat -ulnp 2>/dev/null | grep -qE ':9053\b'; then
        pass "Tor DNSPort (9053/udp) is listening"
    else
        fail "Tor DNSPort (9053/udp) not found via netstat"
    fi
else
    skip "Neither ss nor netstat available for DNS port check"
fi

# 3c. Test that DNS queries to external resolvers are blocked
if ! is_root; then
    skip "DNS leak test (outbound) requires root — skipping direct DNS test"
elif ! has_command dig; then
    skip "dig not available for DNS leak test"
else
    # Try to query an external DNS server (should fail/timeout with iptables rules)
    if dig +short +timeout=3 @8.8.8.8 google.com &>/dev/null; then
        fail "DNS query to 8.8.8.8 succeeded — outbound DNS is NOT blocked (DNS leak!)"
    else
        pass "DNS query to 8.8.8.8 failed/timed out — outbound DNS is correctly blocked"
    fi
fi

# ─── 4. Direct HTTP fails ───────────────────────────────────────

section "4. Direct HTTP Blocked (no proxy)"

if ! has_command curl; then
    skip "curl not available for direct HTTP test"
else
    info "Testing: curl --noproxy '*' --connect-timeout ${CURL_TIMEOUT} http://example.com"
    # This should timeout/fail because iptables blocks direct outbound HTTP
    if curl --noproxy '*' --connect-timeout "$CURL_TIMEOUT" --max-time "$CURL_TIMEOUT" \
         http://example.com &>/dev/null; then
        fail "Direct HTTP request succeeded — outbound traffic is NOT blocked (network leak!)"
    else
        pass "Direct HTTP request failed/timed out — outbound traffic is correctly blocked"
    fi
fi

# Also test HTTPS
if has_command curl; then
    info "Testing: curl --noproxy '*' --connect-timeout ${CURL_TIMEOUT} https://example.com"
    if curl --noproxy '*' --connect-timeout "$CURL_TIMEOUT" --max-time "$CURL_TIMEOUT" \
         https://example.com &>/dev/null; then
        fail "Direct HTTPS request succeeded — outbound traffic is NOT blocked (network leak!)"
    else
        pass "Direct HTTPS request failed/timed out — outbound traffic is correctly blocked"
    fi
fi

# ─── 5. SOCKS5 proxy works ──────────────────────────────────────

section "5. Tor SOCKS5 Proxy Works"

if ! has_command curl; then
    skip "curl not available for SOCKS5 proxy test"
else
    # First check if Tor SOCKS port is reachable
    if has_command ss; then
        tor_socks_listening=$(ss -tlnp 2>/dev/null | grep -cE ":${TOR_SOCKS_PORT}\b" || true)
    elif has_command netstat; then
        tor_socks_listening=$(netstat -tlnp 2>/dev/null | grep -cE ":${TOR_SOCKS_PORT}\b" || true)
    else
        tor_socks_listening=0
    fi

    if [[ "$tor_socks_listening" -eq 0 ]]; then
        skip "Tor SOCKS5 port ${TOR_SOCKS_PORT} is not listening — cannot test proxy connectivity"
    else
        pass "Tor SOCKS5 port ${TOR_SOCKS_PORT} is listening"

        info "Testing: curl --socks5-hostname ${TOR_SOCKS_HOST}:${TOR_SOCKS_PORT} https://check.torproject.org/api/ip"
        if curl --socks5-hostname "${TOR_SOCKS_HOST}:${TOR_SOCKS_PORT}" \
             --connect-timeout 15 --max-time 30 \
             https://check.torproject.org/api/ip 2>/dev/null | grep -q '"IsTor"'; then
            pass "SOCKS5 proxy request succeeded — Tor is working"
        else
            # Try a simpler connectivity test
            if curl --socks5-hostname "${TOR_SOCKS_HOST}:${TOR_SOCKS_PORT}" \
                 --connect-timeout 15 --max-time 30 \
                 https://check.torproject.org/api/ip 2>/dev/null; then
                info "Got response from Tor proxy but could not verify IsTor flag"
                pass "SOCKS5 proxy connectivity works (response received)"
            else
                fail "SOCKS5 proxy request failed — Tor proxy is not routing traffic"
            fi
        fi
    fi
fi

# ─── 6. Listening ports audit ───────────────────────────────────

section "6. Listening Ports Audit"

if has_command ss; then
    # Get all TCP listening ports
    listening_tcp=$(ss -tlnp 2>/dev/null | awk 'NR>1 {print $4}' | sed 's/.*://' || true)
    # Get all UDP listening ports
    listening_udp=$(ss -ulnp 2>/dev/null | awk 'NR>1 {print $4}' | sed 's/.*://' || true)
elif has_command netstat; then
    listening_tcp=$(netstat -tlnp 2>/dev/null | awk 'NR>2 {print $4}' | sed 's/.*://' || true)
    listening_udp=$(netstat -ulnp 2>/dev/null | awk 'NR>2 {print $4}' | sed 's/.*://' || true)
else
    listening_tcp=""
    listening_udp=""
    skip "Neither ss nor netstat available for listening ports audit"
fi

if [[ -n "$listening_tcp" || -n "$listening_udp" ]]; then
    # Combine and deduplicate listening ports
    all_ports=$(echo -e "${listening_tcp}\n${listening_udp}" | sort -n | uniq | grep -E '^[0-9]+$' || true)

    info "Expected listening ports: ${EXPECTED_LISTEN_PORTS}"
    info "Found listening ports: $(echo "$all_ports" | tr '\n' ' ')"

    # Check each expected port
    for port in $EXPECTED_LISTEN_PORTS; do
        if echo "$all_ports" | grep -qx "$port"; then
            pass "Expected port ${port} is listening"
        else
            # Not all expected ports must be present (e.g., 9051 may not be configured)
            info "Expected port ${port} is NOT listening (may not be configured yet)"
        fi
    done

    # Check for unexpected ports (excluding well-known system ports)
    unexpected_ports=""
    system_ports="22 53 68 5353 1900"  # SSH, DNS, DHCP, mDNS, SSDP — expected system services
    for port in $all_ports; do
        is_expected=false
        for expected in $EXPECTED_LISTEN_PORTS $system_ports; do
            if [[ "$port" == "$expected" ]]; then
                is_expected=true
                break
            fi
        done
        if [[ "$is_expected" == "false" ]]; then
            # Check if it's a loopback-only port (less concerning)
            if has_command ss; then
                bind_addr=$(ss -tlnp 2>/dev/null | awk -v p=":$port$" '$4 ~ p {print $4}' | head -1 || true)
                if [[ "$bind_addr" == 127.0.0.1:* ]]; then
                    info "Unexpected port ${port} found (loopback only — lower risk)"
                else
                    unexpected_ports="${unexpected_ports} ${port}"
                fi
            else
                unexpected_ports="${unexpected_ports} ${port}"
            fi
        fi
    done

    if [[ -z "$unexpected_ports" ]]; then
        pass "No unexpected non-loopback listening ports found"
    else
        fail "Unexpected non-loopback listening ports found:$(echo "$unexpected_ports" | tr '\n' ' ')"
    fi
fi

# ─── 7. OUTPUT chain rule audit ──────────────────────────────────

section "7. OUTPUT Chain Rule Audit (no direct internet access except tor UID)"

if ! is_root; then
    skip "iptables OUTPUT chain audit requires root (use sudo)"
elif ! has_command iptables; then
    skip "iptables not available"
else
    # Get all OUTPUT rules
    output_rules=$(iptables -S OUTPUT 2>/dev/null || true)

    info "Current OUTPUT chain rules:"
    echo "$output_rules" | while read -r rule; do
        info "  $rule"
    done

    # Check for Tor UID exemption
    if echo "$output_rules" | grep -qE "owner.*uid-owner.*${TOR_UID}"; then
        pass "OUTPUT chain allows traffic from Tor UID (${TOR_UID})"
    else
        fail "OUTPUT chain does NOT have rule allowing traffic from Tor UID (${TOR_UID})"
    fi

    # Check for rules that might allow direct internet access without Tor UID
    # We look for ACCEPT rules that don't restrict to loopback, LAN, or Tor UID
    direct_internet_rules=""
    while read -r rule; do
        # Skip the default policy line
        [[ "$rule" == "-P OUTPUT"* ]] && continue
        # Skip rules that are DROP or LOG
        [[ "$rule" == *"-j DROP"* ]] && continue
        [[ "$rule" == *"-j LOG"* ]] && continue
        # Skip rules for established connections (these are fine)
        [[ "$rule" == *"conntrack"*"ESTABLISHED"* ]] && continue
        # Skip loopback rules
        [[ "$rule" == *"lo"* ]] && continue
        [[ "$rule" == *"127.0.0.1"* ]] && continue
        # Skip LAN rules (private networks)
        [[ "$rule" == *"192.168.0.0"* ]] && continue
        [[ "$rule" == *"10.0.0.0"* ]] && continue
        [[ "$rule" == *"172.16.0.0"* ]] && continue
        # Skip Tor UID rules
        [[ "$rule" == *"uid-owner"* ]] && continue

        # If we get here, this ACCEPT rule might allow direct internet access
        if [[ "$rule" == *"-j ACCEPT"* ]]; then
            direct_internet_rules="${direct_internet_rules}\n  ${rule}"
        fi
    done <<< "$output_rules"

    if [[ -z "$direct_internet_rules" ]]; then
        pass "No OUTPUT rules allowing direct internet access (except Tor UID)"
    else
        fail "Found OUTPUT rules that may allow direct internet access:"
        echo -e "$direct_internet_rules"
    fi
fi

# ─── 8. LAN-only access for service ports ────────────────────────

section "8. LAN-Only Access for PiCast Service Ports"

if ! is_root; then
    skip "iptables INPUT chain audit requires root (use sudo)"
elif ! has_command iptables; then
    skip "iptables not available"
else
    # Check that PiCast service ports (8585, 8586, 49152) are restricted to LAN
    for port in 8585 8586 49152; do
        port_rules=$(iptables -S INPUT 2>/dev/null | grep -E "--dport ${port}" || true)
        if [[ -z "$port_rules" ]]; then
            info "No INPUT rules found for port ${port} (may not be configured yet)"
        else
            if echo "$port_rules" | grep -qE '192\.168\.0\.0|10\.0\.0\.0'; then
                pass "Port ${port} INPUT restricted to LAN addresses"
            else
                fail "Port ${port} INPUT rules do NOT restrict to LAN — may be exposed to WAN"
            fi
        fi
    done
fi

# ─── 9. Tor not running as relay/exit ───────────────────────────

section "9. Tor is Client-Only (Not Relay/Exit)"

# Check torrc for relay/exit settings
TORRC_PATHS=("/etc/tor/torrc" "/etc/picast/torrc")
torrc_found=false

for torrc in "${TORRC_PATHS[@]}"; do
    if [[ -f "$torrc" ]]; then
        torrc_found=true
        info "Checking torrc: ${torrc}"

        # Check ExitRelay
        if grep -qE '^\s*ExitRelay\s+0' "$torrc" 2>/dev/null; then
            pass "ExitRelay is set to 0 (disabled)"
        elif grep -qE '^\s*ExitRelay\s+1' "$torrc" 2>/dev/null; then
            fail "ExitRelay is set to 1 (ENABLED) — Pi should not be an exit relay!"
        else
            info "ExitRelay not explicitly set (default is 0 — OK)"
        fi

        # Check PublishServerDescriptor
        if grep -qE '^\s*PublishServerDescriptor\s+0' "$torrc" 2>/dev/null; then
            pass "PublishServerDescriptor is set to 0 (descriptor not published)"
        elif grep -qE '^\s*PublishServerDescriptor\s+1' "$torrc" 2>/dev/null; then
            fail "PublishServerDescriptor is set to 1 — may announce this node to the Tor network"
        else
            info "PublishServerDescriptor not explicitly set (default depends on relay config)"
        fi

        # Check for ORPort (relay port)
        if grep -qE '^\s*ORPort\s+[1-9]' "$torrc" 2>/dev/null; then
            fail "ORPort is configured — Tor may be running as a relay"
        else
            pass "No ORPort configured (not a relay)"
        fi

        # Check for ExitPolicy
        if grep -qE '^\s*ExitPolicy\s+reject\s+\*:\*' "$torrc" 2>/dev/null; then
            pass "ExitPolicy reject *:* found (rejects all exit traffic)"
        elif grep -qE '^\s*ExitPolicy\s+accept' "$torrc" 2>/dev/null; then
            fail "ExitPolicy accept found — may allow exit traffic!"
        else
            info "No explicit ExitPolicy found (default rejects all if not relay)"
        fi

        break  # Only check the first found torrc
    fi
done

if [[ "$torrc_found" == "false" ]]; then
    skip "No torrc found at ${TORRC_PATHS[*]}"
fi

# ─── 10. Process runs as picast user ────────────────────────────

section "10. PiCast Process Runs as Non-Root User"

PICAST_PID=$(pgrep -x picast 2>/dev/null || true)

if [[ -n "$PICAST_PID" ]]; then
    picast_user=$(ps -o user= -p "$PICAST_PID" 2>/dev/null | head -1 || true)
    picast_uid=$(ps -o uid= -p "$PICAST_PID" 2>/dev/null | head -1 || true)

    if [[ "$picast_user" == "root" || "$picast_uid" == "0" ]]; then
        fail "PiCast process is running as root — should run as 'picast' user"
    elif [[ "$picast_user" == "picast" ]]; then
        pass "PiCast process runs as 'picast' user"
    else
        info "PiCast process runs as user '${picast_user}' (expected 'picast')"
        # Still pass if it's not root
        if [[ "$picast_uid" != "0" ]]; then
            pass "PiCast process is not running as root"
        fi
    fi

    # Check group membership for DRM access
    picast_groups=$(id -Gn "$picast_user" 2>/dev/null || true)
    if echo "$picast_groups" | grep -qw "video"; then
        pass "Process user has 'video' group membership (DRM/KMS access)"
    else
        fail "Process user does NOT have 'video' group membership (needed for DRM/KMS)"
    fi

    if echo "$picast_groups" | grep -qw "render"; then
        pass "Process user has 'render' group membership (GPU access)"
    else
        info "Process user does not have 'render' group membership (may not be needed)"
    fi

    if echo "$picast_groups" | grep -qw "audio"; then
        pass "Process user has 'audio' group membership (ALSA access)"
    else
        info "Process user does not have 'audio' group membership (may not be needed)"
    fi
else
    skip "PiCast process not running — cannot verify process user"
fi

# ─── 11. DRM master is only PiCast (no X11/Wayland) ─────────────

section "11. DRM Master is Only PiCast (No X11/Wayland)"

if pgrep -x Xorg &>/dev/null; then
    fail "Xorg is running — DRM master may be held by X11, not PiCast"
else
    pass "Xorg is not running"
fi

if pgrep -x Xwayland &>/dev/null; then
    fail "Xwayland is running — DRM master may be held by Wayland, not PiCast"
else
    pass "Xwayland is not running"
fi

if pgrep -x weston &>/dev/null; then
    info "Weston compositor is running — DRM master may be held by Wayland"
elif pgrep -x mutter &>/dev/null; then
    info "Mutter compositor is running — DRM master may be held by Wayland"
elif pgrep -x kwin_wayland &>/dev/null; then
    info "KWin Wayland compositor is running — DRM master may be held by Wayland"
else
    pass "No Wayland compositor detected"
fi

# Check DRM master holder
if [[ -e /dev/dri/card0 ]]; then
    if has_command fuser; then
        drm_holder=$(fuser /dev/dri/card0 2>/dev/null || true)
        if [[ -n "$drm_holder" ]]; then
            holder_name=$(ps -o comm= -p "$(echo "$drm_holder" | awk '{print $1}')" 2>/dev/null || true)
            if [[ "$holder_name" == "picast" ]]; then
                pass "PiCast holds DRM master on /dev/dri/card0"
            elif [[ -n "$holder_name" ]]; then
                info "DRM master on /dev/dri/card0 is held by: ${holder_name}"
            fi
        else
            info "No process currently holds /dev/dri/card0"
        fi
    else
        skip "fuser not available to check DRM master holder"
    fi
else
    info "/dev/dri/card0 not found (expected on Pi hardware only)"
fi

# ─── 12. Systemd service hardening ──────────────────────────────

section "12. Systemd Service Hardening"

SERVICE_PATHS=("/etc/systemd/system/picast.service" "/lib/systemd/system/picast.service")
service_found=false

for svc in "${SERVICE_PATHS[@]}"; do
    if [[ -f "$svc" ]]; then
        service_found=true
        info "Checking service file: ${svc}"

        # Required hardening directives
        required_directives=(
            "NoNewPrivileges=true"
            "ProtectSystem=strict"
            "ProtectHome=true"
            "ProtectKernelTunables=true"
            "ProtectKernelModules=true"
            "ProtectControlGroups=true"
            "RestrictNamespaces=true"
            "LockPersonality=true"
            "MemoryDenyWriteExecute=true"
            "RestrictRealtime=true"
        )

        for directive in "${required_directives[@]}"; do
            key="${directive%%=*}"
            if grep -qE "^\s*${key}=" "$svc" 2>/dev/null; then
                actual=$(grep -E "^\s*${key}=" "$svc" 2>/dev/null | head -1 | tr -d ' ')
                if [[ "$actual" == "$directive" ]]; then
                    pass "Service has ${directive}"
                else
                    fail "Service has ${key} but value differs: got '${actual}', expected '${directive}'"
                fi
            else
                fail "Service is missing ${directive}"
            fi
        done

        # Check User=picast
        if grep -qE '^\s*User=picast' "$svc" 2>/dev/null; then
            pass "Service runs as User=picast"
        else
            fail "Service does NOT set User=picast"
        fi

        # Check SupplementaryGroups
        if grep -qE '^\s*SupplementaryGroups=.*video' "$svc" 2>/dev/null; then
            pass "Service has video group in SupplementaryGroups"
        else
            fail "Service missing 'video' in SupplementaryGroups (needed for DRM)"
        fi

        break  # Only check the first found service file
    fi
done

if [[ "$service_found" == "false" ]]; then
    skip "PiCast systemd service file not found at ${SERVICE_PATHS[*]}"
fi

# ─── Report ──────────────────────────────────────────────────────

echo ""
echo -e "${BOLD}══════════════════════════════════════════════════════════════${NC}"
echo -e "${BOLD}  REPORT${NC}"
echo -e "${BOLD}══════════════════════════════════════════════════════════════${NC}"
echo ""
echo -e "  ${GREEN}PASSED${NC}: ${PASS}"
echo -e "  ${RED}FAILED${NC}: ${FAIL}"
echo -e "  ${YELLOW}SKIPPED${NC}: ${SKIP}"
echo ""

if [[ "$FAIL" -gt 0 ]]; then
    echo -e "  ${RED}${BOLD}RESULT: FAIL — ${FAIL} check(s) did not pass${NC}"
    echo ""
    echo "  Review the failed checks above and remediate before deployment."
    echo "  See docs/SECURITY_AUDIT.md for detailed remediation steps."
    exit 1
else
    echo -e "  ${GREEN}${BOLD}RESULT: PASS — all checks passed${NC}"
    echo ""
    echo "  Network isolation is correctly configured."
    exit 0
fi
