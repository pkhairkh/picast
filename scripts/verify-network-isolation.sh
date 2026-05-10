#!/bin/bash
# ──────────────────────────────────────────────────────────────────────
# boGDan Network Isolation Verification — S6.3
# ──────────────────────────────────────────────────────────────────────
#
# Verifies that ALL outbound traffic from boGDan goes through Tor.
# This is an ACTIVE test — it sets up iptables rules, starts casting,
# monitors for leaks, and tests that disabling Tor causes all requests
# to fail (no fallback to direct connections).
#
# What this script does:
#   1. Save existing iptables rules
#   2. Install test iptables rules:
#      - Allow ESTABLISHED connections
#      - Allow loopback (lo)
#      - Allow Tor SOCKS (127.0.0.1:9050)
#      - REJECT everything else (with counter)
#   3. Start boGDan and cast URLs
#   4. Monitor REJECT counter — should stay at 0
#   5. Verify no UDP port 53 traffic from bogdan process (no DNS leaks)
#   6. Test: disable Tor → all requests should fail (no fallback)
#   7. Clean up iptables rules on exit (trap EXIT)
#
# Prerequisites:
#   - Root access (required for iptables)
#   - boGDan server installed
#   - Tor daemon running with SOCKS5 on 127.0.0.1:9050
#   - tcpdump or ss for traffic monitoring
#
# Usage:
#   sudo bash scripts/verify-network-isolation.sh
#
# Exit codes:
#   0 — all isolation checks pass
#   1 — one or more isolation checks fail
#   2 — setup failure
#
# ──────────────────────────────────────────────────────────────────────

set -euo pipefail

# ── Pre-flight: must run as root ──────────────────────────────────────

if [[ "$(id -u)" -ne 0 ]]; then
    echo "ERROR: This script must be run as root (sudo)." >&2
    echo "  sudo bash scripts/verify-network-isolation.sh" >&2
    exit 2
fi

# ── Configurable Variables ─────────────────────────────────────────────

BOGDAN_HOST="${BOGDAN_HOST:-localhost}"
BOGDAN_PORT="${BOGDAN_PORT:-8585}"
TOR_SOCKS_PORT="${TOR_SOCKS_PORT:-9050}"
TOR_CONTROL_PORT="${TOR_CONTROL_PORT:-9052}"
BASE_URL="http://${BOGDAN_HOST}:${BOGDAN_PORT}"

# iptables chain name for our test rules (to avoid conflicts)
CHAIN_NAME="BOGDAN_TEST"

# URL to cast during the test
TEST_URL="${TEST_URL:-https://upload.wikimedia.org/wikipedia/commons/transcoded/c/c0/Big_Buck_Bunny_4K.webm/Big_Buck_Bunny_4K.webm.480p.vp9.webm}"

# How long to monitor for leaks during active casting (seconds)
MONITOR_DURATION="${MONITOR_DURATION:-60}"

# ── Colours ────────────────────────────────────────────────────────────

if [[ -t 1 ]]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    CYAN='\033[0;36m'
    BOLD='\033[1m'
    NC='\033[0m'
else
    RED='' GREEN='' YELLOW='' CYAN='' BOLD='' NC=''
fi

# ── Counters ───────────────────────────────────────────────────────────

PASS=0
FAIL=0
SKIP=0

# ── Helpers ────────────────────────────────────────────────────────────

pass() { PASS=$((PASS + 1)); echo -e "  ${GREEN}[PASS]${NC} $1"; }
fail() { FAIL=$((FAIL + 1)); echo -e "  ${RED}[FAIL]${NC} $1"; }
skip() { SKIP=$((SKIP + 1)); echo -e "  ${YELLOW}[SKIP]${NC} $1"; }
info() { echo -e "  ${CYAN}[INFO]${NC} $1"; }
section() { echo ""; echo -e "${BOLD}━━━ $1 ━━━${NC}"; }

# Get a value from a JSON response (jq → python3 → grep fallback)
json_value() {
    local url="$1" key="$2"
    local body
    body=$(curl -sf --max-time 10 "$url" 2>/dev/null) || return 1
    if command -v jq &>/dev/null; then
        printf '%s' "$body" | jq -r "$key" 2>/dev/null
    elif command -v python3 &>/dev/null; then
        printf '%s' "$body" | python3 -c "import sys,json; print(json.load(sys.stdin)${key})" 2>/dev/null
    else
        printf '%s' "$body" | grep -oP "\"${key#*.}\"\s*:\s*\K[^\s,}\"']+" | head -1
    fi
}

# Get the boGDan process ID
get_bogdan_pid() {
    local pid
    pid=$(pgrep -xf "bogdan" 2>/dev/null || true)
    if [[ -z "$pid" ]]; then
        pid=$(pgrep -xf "bogdan-server" 2>/dev/null || true)
    fi
    if [[ -z "$pid" ]]; then
        pid=$(pgrep -f "bogdan" 2>/dev/null | head -1 || true)
    fi
    echo "${pid}"
}

# ── Cleanup ────────────────────────────────────────────────────────────
# CRITICAL: Always restore iptables rules on exit, even on error or
# Ctrl+C. This prevents leaving the Pi with broken network access.

cleanup() {
    echo ""
    info "Restoring iptables rules..."

    # Delete our test chain rules from OUTPUT
    iptables -D OUTPUT -j "$CHAIN_NAME" 2>/dev/null || true

    # Flush and delete our test chain
    iptables -F "$CHAIN_NAME" 2>/dev/null || true
    iptables -X "$CHAIN_NAME" 2>/dev/null || true

    # Restore saved rules if we saved them
    if [[ -f "/tmp/bogdan-iptables-backup-$$.rules" ]]; then
        iptables-restore < "/tmp/bogdan-iptables-backup-$$.rules" 2>/dev/null || true
        rm -f "/tmp/bogdan-iptables-backup-$$.rules"
        info "Previous iptables rules restored"
    fi

    # Stop any active cast
    curl -sf --max-time 5 -X POST "${BASE_URL}/api/stop" &>/dev/null || true

    # Restart Tor if we stopped it
    if [[ -f "/tmp/bogdan-tor-was-running-$$" ]]; then
        info "Restarting Tor daemon..."
        systemctl start tor 2>/dev/null || true
        rm -f "/tmp/bogdan-tor-was-running-$$"
    fi

    # Kill tcpdump if we started it
    if [[ -n "${TCPDUMP_PID:-}" ]]; then
        kill "$TCPDUMP_PID" 2>/dev/null || true
    fi

    info "Cleanup complete"
}
trap cleanup EXIT

# ── Banner ─────────────────────────────────────────────────────────────

echo ""
echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}║     boGDan Network Isolation Verification (S6.3)           ║${NC}"
echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${NC}"
echo ""
echo -e "  Date:       $(date -u '+%Y-%m-%d %H:%M:%S UTC')"
echo -e "  Host:       $(hostname)"
echo -e "  User:       $(whoami) (UID $(id -u))"
echo -e "  Target:     ${BASE_URL}"
echo -e "  Tor SOCKS:  127.0.0.1:${TOR_SOCKS_PORT}"
echo ""

# ═══════════════════════════════════════════════════════════════════════
# PHASE 1: Pre-flight checks
# ═══════════════════════════════════════════════════════════════════════

section "Phase 1: Pre-flight Checks"

# Check iptables is available
if ! command -v iptables &>/dev/null; then
    fail "iptables is not installed"
    exit 2
fi
pass "iptables is available"

# Check Tor is running
if systemctl is-active --quiet tor 2>/dev/null || pgrep -x tor &>/dev/null; then
    pass "Tor daemon is running"
    touch "/tmp/bogdan-tor-was-running-$$"
else
    fail "Tor daemon is not running — cannot test network isolation"
    exit 2
fi

# Check Tor SOCKS port is listening
if ss -tlnp 2>/dev/null | grep -qE ":${TOR_SOCKS_PORT}\b"; then
    pass "Tor SOCKS port ${TOR_SOCKS_PORT} is listening"
else
    fail "Tor SOCKS port ${TOR_SOCKS_PORT} is not listening"
    exit 2
fi

# Check boGDan is available (try to start if not running)
BOGDAN_PID=$(get_bogdan_pid)
if [[ -z "$BOGDAN_PID" ]]; then
    info "boGDan not detected — attempting to start..."
    if systemctl is-active --quiet bogdan 2>/dev/null; then
        pass "boGDan started via systemctl"
    elif command -v systemctl &>/dev/null; then
        systemctl start bogdan 2>/dev/null && pass "Started bogdan.service" || {
            if command -v bogdan-server &>/dev/null; then
                sudo -u bogdan bogdan-server &>/tmp/bogdan-net-test.log &
                pass "Started bogdan-server"
            else
                fail "Cannot find or start boGDan"
                exit 2
            fi
        }
    fi
    # Wait for server
    for i in $(seq 1 30); do
        curl -sf --max-time 2 "${BASE_URL}/api/health" &>/dev/null && break
        sleep 2
    done
fi

BOGDAN_PID=$(get_bogdan_pid)
if [[ -z "$BOGDAN_PID" ]]; then
    fail "Cannot find boGDan process"
    exit 2
fi
pass "boGDan is running (PID: ${BOGDAN_PID})"

# Verify server health
if curl -sf --max-time 5 "${BASE_URL}/api/health" &>/dev/null; then
    pass "boGDan health check OK"
else
    fail "boGDan health check failed"
    exit 2
fi

# ═══════════════════════════════════════════════════════════════════════
# PHASE 2: Set up iptables test rules
# ═══════════════════════════════════════════════════════════════════════

section "Phase 2: Install Test iptables Rules"

# Backup existing rules
iptables-save > "/tmp/bogdan-iptables-backup-$$.rules" 2>/dev/null
info "Existing iptables rules backed up"

# Create a dedicated chain for our test rules
# This avoids interfering with existing rules and makes cleanup easy
iptables -N "$CHAIN_NAME" 2>/dev/null || iptables -F "$CHAIN_NAME" 2>/dev/null
info "Created test chain: ${CHAIN_NAME}"

# Rule 1: Allow ESTABLISHED connections (for responses to our outbound)
iptables -A "$CHAIN_NAME" -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
info "Rule: Allow ESTABLISHED,RELATED connections"

# Rule 2: Allow loopback
iptables -A "$CHAIN_NAME" -i lo -j ACCEPT
info "Rule: Allow loopback (lo)"

# Rule 3: Allow connections to Tor SOCKS proxy (localhost only)
iptables -A "$CHAIN_NAME" -d 127.0.0.1/32 -p tcp --dport "$TOR_SOCKS_PORT" -j ACCEPT
info "Rule: Allow Tor SOCKS (127.0.0.1:${TOR_SOCKS_PORT})"

# Rule 4: Allow Tor control port (localhost only)
iptables -A "$CHAIN_NAME" -d 127.0.0.1/32 -p tcp --dport "$TOR_CONTROL_PORT" -j ACCEPT
info "Rule: Allow Tor Control (127.0.0.1:${TOR_CONTROL_PORT})"

# Rule 5: Allow LAN traffic (DLNA, mDNS, HTTP API responses)
iptables -A "$CHAIN_NAME" -d 192.168.0.0/16 -j ACCEPT
iptables -A "$CHAIN_NAME" -d 10.0.0.0/8 -j ACCEPT
iptables -A "$CHAIN_NAME" -d 172.16.0.0/12 -j ACCEPT
info "Rule: Allow LAN traffic (192.168/16, 10/8, 172.16/12)"

# Rule 6: Allow DNS only to localhost (Tor DNSPort or dnsmasq stub)
iptables -A "$CHAIN_NAME" -d 127.0.0.1/32 -p udp --dport 53 -j ACCEPT
info "Rule: Allow DNS only to 127.0.0.1:53"

# Rule 7: Allow Tor daemon itself to reach the internet
# (Tor needs to connect to relay nodes on dynamic ports)
TOR_UID=$(id -u debian-tor 2>/dev/null || echo "")
if [[ -n "$TOR_UID" ]]; then
    iptables -A "$CHAIN_NAME" -m owner --uid-owner "$TOR_UID" -j ACCEPT
    info "Rule: Allow Tor daemon (UID ${TOR_UID}) outbound"
else
    info "Rule: Could not determine Tor UID — skipping Tor daemon rule"
fi

# Rule 8: REJECT everything else — with a counter we can check
iptables -A "$CHAIN_NAME" -j REJECT --reject-with icmp-port-unreachable
info "Rule: REJECT all other outbound traffic"

# Jump to our chain from OUTPUT
iptables -I OUTPUT 1 -j "$CHAIN_NAME"
info "Test chain inserted into OUTPUT chain (position 1)"

pass "Test iptables rules installed"
info ""
info "Active rules in ${CHAIN_NAME}:"
iptables -L "$CHAIN_NAME" -v -n 2>/dev/null | while read -r line; do
    info "  $line"
done

# ═══════════════════════════════════════════════════════════════════════
# PHASE 3: Verify direct connections are blocked
# ═══════════════════════════════════════════════════════════════════════

section "Phase 3: Verify Direct Connections Are Blocked"

# Test 3a: Direct HTTP should fail
info "Testing direct HTTP (should be blocked)..."
if curl --noproxy '*' --connect-timeout 5 --max-time 5 \
     http://example.com &>/dev/null; then
    fail "Direct HTTP succeeded — traffic is NOT blocked through iptables!"
else
    pass "Direct HTTP blocked — iptables REJECT rule is working"
fi

# Test 3b: Direct HTTPS should fail
info "Testing direct HTTPS (should be blocked)..."
if curl --noproxy '*' --connect-timeout 5 --max-time 5 \
     https://example.com &>/dev/null; then
    fail "Direct HTTPS succeeded — traffic is NOT blocked through iptables!"
else
    pass "Direct HTTPS blocked — iptables REJECT rule is working"
fi

# Test 3c: External DNS should be blocked
info "Testing direct DNS (should be blocked)..."
if command -v dig &>/dev/null; then
    if dig +short +timeout=3 @8.8.8.8 example.com &>/dev/null; then
        fail "Direct DNS to 8.8.8.8 succeeded — DNS is NOT blocked!"
    else
        pass "Direct DNS to 8.8.8.8 blocked — no DNS leak possible"
    fi
else
    skip "dig not available for DNS leak test"
fi

# ═══════════════════════════════════════════════════════════════════════
# PHASE 4: Verify Tor-routed connections work
# ═══════════════════════════════════════════════════════════════════════

section "Phase 4: Verify Tor-Routed Connections Work"

info "Testing via Tor SOCKS proxy..."
if curl --socks5-hostname 127.0.0.1:"${TOR_SOCKS_PORT}" \
     --connect-timeout 15 --max-time 30 \
     https://check.torproject.org/api/ip 2>/dev/null | grep -q '"IsTor"'; then
    pass "Tor SOCKS proxy is routing traffic correctly"
else
    # Try a simpler test
    if curl --socks5-hostname 127.0.0.1:"${TOR_SOCKS_PORT}" \
         --connect-timeout 15 --max-time 30 \
         https://example.com &>/dev/null; then
        pass "Tor SOCKS proxy connectivity works (response received)"
    else
        fail "Tor SOCKS proxy is not routing traffic"
    fi
fi

# ═══════════════════════════════════════════════════════════════════════
# PHASE 5: Cast URLs and monitor REJECT counter
# ═══════════════════════════════════════════════════════════════════════

section "Phase 5: Cast URLs and Monitor for Leaks"

# Record the REJECT counter before casting
REJECT_BEFORE=$(iptables -L "$CHAIN_NAME" -v -n 2>/dev/null | grep "REJECT" | awk '{print $1}' | head -1 || echo "0")
info "REJECT counter before cast: ${REJECT_BEFORE}"

# Start a background tcpdump to capture any DNS leaks from bogdan process
DNS_CAPTURE_FILE="/tmp/bogdan-dns-capture-$$.pcap"
TCPDUMP_PID=""
if command -v tcpdump &>/dev/null; then
    # Capture UDP port 53 traffic that is NOT to 127.0.0.1
    tcpdump -i any -n -w "$DNS_CAPTURE_FILE" \
        "udp port 53 and not dst host 127.0.0.1" \
        &>/dev/null &
    TCPDUMP_PID=$!
    info "DNS leak capture started (PID: ${TCPDUMP_PID})"
else
    info "tcpdump not available — DNS capture skipped"
fi

# Cast a URL
info "Casting test URL..."
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" --max-time 30 \
    -X POST "${BASE_URL}/api/cast" \
    -H "Content-Type: application/json" \
    -d "{\"url\": \"${TEST_URL}\"}" 2>/dev/null || echo "000")

if [[ "$HTTP_CODE" == "202" ]]; then
    pass "Cast accepted (HTTP 202)"
else
    info "Cast returned HTTP ${HTTP_CODE} — continuing monitoring"
fi

# Wait for playback to start
info "Waiting for playback to start..."
PLAYBACK_STARTED=false
for i in $(seq 1 30); do
    STATE=$(json_value "${BASE_URL}/api/status" ".state" 2>/dev/null || echo "")
    if [[ "$STATE" == "playing" ]]; then
        PLAYBACK_STARTED=true
        break
    fi
    sleep 2
done

if $PLAYBACK_STARTED; then
    pass "Playback started — monitoring for ${MONITOR_DURATION}s"
else
    STATE=$(json_value "${BASE_URL}/api/status" ".state" 2>/dev/null || echo "unknown")
    info "Playback state: ${STATE} — monitoring anyway"
fi

# Monitor for the specified duration
info "Monitoring REJECT counter during active playback..."
SAMPLE_INTERVAL=10
SAMPLES=$((MONITOR_DURATION / SAMPLE_INTERVAL))
LEAK_DETECTED=false

for i in $(seq 1 "$SAMPLES"); do
    sleep "$SAMPLE_INTERVAL"

    REJECT_NOW=$(iptables -L "$CHAIN_NAME" -v -n 2>/dev/null | grep "REJECT" | awk '{print $1}' | head -1 || echo "0")

    if [[ "$REJECT_NOW" -gt "$REJECT_BEFORE" ]]; then
        LEAK_DETECTED=true
        LEAK_COUNT=$((REJECT_NOW - REJECT_BEFORE))
        fail "REJECT counter increased: ${REJECT_BEFORE} → ${REJECT_NOW} (${LEAK_COUNT} leaked packets)"
    else
        info "Sample ${i}/${SAMPLES}: REJECT counter stable at ${REJECT_NOW}"
    fi
done

if ! $LEAK_DETECTED; then
    pass "No packets hit REJECT rule during playback — all traffic goes through Tor"
fi

# Stop playback
curl -sf --max-time 5 -X POST "${BASE_URL}/api/stop" &>/dev/null || true

# ═══════════════════════════════════════════════════════════════════════
# PHASE 6: DNS leak verification
# ═══════════════════════════════════════════════════════════════════════

section "Phase 6: DNS Leak Verification"

# Stop tcpdump and analyze capture
if [[ -n "$TCPDUMP_PID" ]]; then
    kill "$TCPDUMP_PID" 2>/dev/null || true
    wait "$TCPDUMP_PID" 2>/dev/null || true
    TCPDUMP_PID=""

    if [[ -f "$DNS_CAPTURE_FILE" ]]; then
        # Count captured packets
        DNS_PACKET_COUNT=$(tcpdump -r "$DNS_CAPTURE_FILE" 2>/dev/null | wc -l || echo "0")
        if [[ "$DNS_PACKET_COUNT" -eq 0 ]]; then
            pass "No DNS leaks: zero UDP port 53 packets to non-localhost detected"
        else
            fail "DNS leak detected: ${DNS_PACKET_COUNT} DNS packet(s) to non-localhost!"
            info "Captured DNS packets:"
            tcpdump -r "$DNS_CAPTURE_FILE" -n 2>/dev/null | head -20
        fi
        rm -f "$DNS_CAPTURE_FILE"
    else
        info "No DNS capture file (tcpdump may have failed to start)"
    fi
else
    # Alternative: check /proc/net/udp for DNS connections from bogdan
    info "Checking for DNS sockets from bogdan process..."
    BOGDAN_PID=$(get_bogdan_pid)
    if [[ -n "$BOGDAN_PID" ]]; then
        # Look for UDP connections to port 53 from bogdan
        DNS_SOCKETS=$(ls -la /proc/"${BOGDAN_PID}"/fd 2>/dev/null | grep -c "socket" || echo "0")
        info "Process has ${DNS_SOCKETS} socket FDs (check manually for DNS leaks)"
        skip "Detailed DNS leak analysis (tcpdump not available)"
    else
        skip "DNS leak analysis (tcpdump not available, bogdan PID unknown)"
    fi
fi

# Also verify via iptables: check if any packets matched the DNS-allowed rule
DNS_ALLOW_PACKETS=$(iptables -L "$CHAIN_NAME" -v -n 2>/dev/null | grep "dpt:53" | awk '{print $1}' || echo "0")
info "Packets matching DNS-allow rule (to 127.0.0.1:53 only): ${DNS_ALLOW_PACKETS}"

# ═══════════════════════════════════════════════════════════════════════
# PHASE 7: Tor failover test — disabling Tor should break everything
# ═══════════════════════════════════════════════════════════════════════

section "Phase 7: Tor Failover Test (No Fallback)"

info "Stopping Tor daemon to verify no fallback to direct connections..."
systemctl stop tor 2>/dev/null || kill "$(pgrep -x tor)" 2>/dev/null || true
sleep 3

# Verify Tor is actually stopped
TOR_STOPPED=false
if ! ss -tlnp 2>/dev/null | grep -qE ":${TOR_SOCKS_PORT}\b"; then
    TOR_STOPPED=true
    info "Tor SOCKS port ${TOR_SOCKS_PORT} is no longer listening"
else
    fail "Tor SOCKS port is still listening — could not stop Tor"
fi

if $TOR_STOPPED; then
    # Try to cast — this should FAIL because boGDan routes through Tor
    info "Attempting to cast with Tor disabled (should fail)..."
    HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" --max-time 30 \
        -X POST "${BASE_URL}/api/cast" \
        -H "Content-Type: application/json" \
        -d "{\"url\": \"${TEST_URL}\"}" 2>/dev/null || echo "000")

    # The cast request itself may succeed (HTTP 202) but playback should fail
    # because the resolver can't reach the internet without Tor.
    sleep 10
    STATE=$(json_value "${BASE_URL}/api/status" ".state" 2>/dev/null || echo "")

    if [[ "$STATE" == "playing" ]]; then
        fail "Playback started without Tor — traffic is falling back to direct connection!"
    else
        pass "Playback did NOT start without Tor — no fallback to direct connections"
        info "State with Tor disabled: ${STATE}"
    fi

    # Also test direct HTTP access from the Pi
    info "Testing direct HTTP with Tor disabled (should fail)..."
    if curl --noproxy '*' --connect-timeout 5 --max-time 5 \
         http://example.com &>/dev/null; then
        fail "Direct HTTP works even with Tor disabled — iptables not blocking!"
    else
        pass "Direct HTTP blocked even with Tor disabled — iptables isolation holds"
    fi
else
    skip "Tor failover test (could not stop Tor)"
fi

# Restart Tor
info "Restarting Tor daemon..."
systemctl start tor 2>/dev/null || true
sleep 5

# Verify Tor is back
if ss -tlnp 2>/dev/null | grep -qE ":${TOR_SOCKS_PORT}\b"; then
    pass "Tor daemon restarted successfully"
else
    info "Waiting for Tor to come back..."
    for i in $(seq 1 30); do
        if ss -tlnp 2>/dev/null | grep -qE ":${TOR_SOCKS_PORT}\b"; then
            pass "Tor daemon is back online"
            break
        fi
        sleep 2
    done
fi

# ═══════════════════════════════════════════════════════════════════════
# PHASE 8: Stream isolation verification
# ═══════════════════════════════════════════════════════════════════════

section "Phase 8: Stream Isolation Verification"

# Check torrc for IsolateSOCKSAuth
TORRC_PATHS=("/etc/tor/torrc" "/etc/bogdan/torrc")
torrc_found=false

for torrc in "${TORRC_PATHS[@]}"; do
    if [[ -f "$torrc" ]]; then
        torrc_found=true
        info "Checking torrc: ${torrc}"

        if grep -qE 'SocksPort.*IsolateSOCKSAuth' "$torrc" 2>/dev/null; then
            pass "SocksPort has IsolateSOCKSAuth — per-domain circuit isolation enabled"
        else
            fail "SocksPort does NOT have IsolateSOCKSAuth — all sites share circuits"
        fi

        if grep -qE 'ExitRelay\s+0' "$torrc" 2>/dev/null; then
            pass "ExitRelay is set to 0 (client only)"
        else
            fail "ExitRelay not set to 0 — may act as exit relay"
        fi

        break
    fi
done

if [[ "$torrc_found" == "false" ]]; then
    skip "No torrc found for stream isolation check"
fi

# ═══════════════════════════════════════════════════════════════════════
# PHASE 9: Process privilege verification
# ═══════════════════════════════════════════════════════════════════════

section "Phase 9: Process Privilege Verification"

BOGDAN_PID=$(get_bogdan_pid)
if [[ -n "$BOGDAN_PID" ]]; then
    bogdan_user=$(ps -o user= -p "$BOGDAN_PID" 2>/dev/null | head -1 || true)

    if [[ "$bogdan_user" == "root" ]]; then
        fail "boGDan is running as root — should run as 'bogdan' user"
    elif [[ "$bogdan_user" == "bogdan" ]]; then
        pass "boGDan runs as 'bogdan' user (not root)"
    else
        info "boGDan runs as '${bogdan_user}' (expected 'bogdan')"
        if [[ "$bogdan_user" != "root" ]]; then
            pass "boGDan is not running as root"
        fi
    fi

    # Check group memberships
    bogdan_groups=$(id -Gn "$bogdan_user" 2>/dev/null || true)
    if echo "$bogdan_groups" | grep -qw "video"; then
        pass "Process user has 'video' group (DRM/KMS access)"
    else
        info "Process user lacks 'video' group (needed for DRM)"
    fi
else
    skip "boGDan process not found for privilege check"
fi

# ═══════════════════════════════════════════════════════════════════════
# Report
# ═══════════════════════════════════════════════════════════════════════

echo ""
echo -e "${BOLD}══════════════════════════════════════════════════════════════${NC}"
echo -e "${BOLD}  NETWORK ISOLATION VERIFICATION REPORT${NC}"
echo -e "${BOLD}══════════════════════════════════════════════════════════════${NC}"
echo ""
echo -e "  ${GREEN}PASSED${NC}: ${PASS}"
echo -e "  ${RED}FAILED${NC}: ${FAIL}"
echo -e "  ${YELLOW}SKIPPED${NC}: ${SKIP}"
echo ""

if [[ "$FAIL" -gt 0 ]]; then
    echo -e "  ${RED}${BOLD}RESULT: FAIL — ${FAIL} check(s) did not pass${NC}"
    echo ""
    echo "  Network isolation is compromised. Review the failures above."
    echo "  Critical actions:"
    echo "    1. Verify iptables rules are correct: iptables -S OUTPUT -v -n"
    echo "    2. Check for DNS leaks: tcpdump -i any -n 'udp port 53 and not dst host 127.0.0.1'"
    echo "    3. Ensure boGDan uses socks5h:// (not socks5://) for remote DNS"
    echo "    4. See docs/SECURITY.md for hardening guide"
    echo ""
    exit 1
else
    echo -e "  ${GREEN}${BOLD}RESULT: PASS — all network isolation checks passed${NC}"
    echo ""
    echo "  All outbound traffic is correctly routed through Tor."
    echo "  No DNS leaks detected. No fallback to direct connections."
    echo ""
    exit 0
fi
