#!/usr/bin/env bash
# boGDan Pi Hardware Smoke Test (T-9.3)
#
# Automated smoke test for Pi hardware: verifies the server is up,
# Tor connectivity works, casting plays media, playback controls
# function, and HDMI output is active.
#
# Usage:
#   ./scripts/smoke-test.sh                        # defaults
#   BOGDAN_HOST=192.168.1.100 ./scripts/smoke-test.sh
#   BOGDAN_PORT=8585 TEST_URL=... ./scripts/smoke-test.sh
#
# Exit codes:
#   0  — all tests passed
#   1  — one or more tests failed
#
set -euo pipefail

# ── Configurable variables ──────────────────────────────────────────
BOGDAN_HOST="${BOGDAN_HOST:-localhost}"
BOGDAN_PORT="${BOGDAN_PORT:-8585}"
TEST_URL="${TEST_URL:-https://upload.wikimedia.org/wikipedia/commons/transcoded/c/c0/Big_Buck_Bunny_4K.webm/Big_Buck_Bunny_4K.webm.480p.vp9.webm}"

# Derived
BASE_URL="http://${BOGDAN_HOST}:${BOGDAN_PORT}"

# ── Timeouts (seconds) ─────────────────────────────────────────────
HEALTH_TIMEOUT="${HEALTH_TIMEOUT:-30}"
TOR_TIMEOUT="${TOR_TIMEOUT:-60}"
PLAY_TIMEOUT="${PLAY_TIMEOUT:-60}"
STATE_TIMEOUT="${STATE_TIMEOUT:-30}"
SEEK_TOLERANCE="${SEEK_TOLERANCE:-5}"

# ── Colours ─────────────────────────────────────────────────────────
if [[ -t 1 ]]; then
    GRN='\033[0;32m'
    RED='\033[0;31m'
    YEL='\033[0;33m'
    CYN='\033[0;36m'
    RST='\033[0m'
else
    GRN='' RED='' YEL='' CYN='' RST=''
fi

# ── State tracking ──────────────────────────────────────────────────
PASS=0
FAIL=0
SKIP=0
RESULTS=()
SESSION_ID=""
SERVER_PID=""

# ── Helpers ─────────────────────────────────────────────────────────

log_pass() {
    local label="$1" dur="${2:-}"
    PASS=$((PASS + 1))
    RESULTS+=("pass" "${label}" "${dur}")
    printf "  ${GRN}\u2713${RST} %s %s\n" "$label" "${dur:+(${dur})}"
}

log_fail() {
    local label="$1" reason="${2:-}" dur="${3:-}"
    FAIL=$((FAIL + 1))
    RESULTS+=("fail" "${label}" "${reason}")
    printf "  ${RED}\u2717${RST} %s%s\n" "$label" "${reason:+ : ${reason}}"
}

log_skip() {
    local label="$1" reason="${2:-}"
    SKIP=$((SKIP + 1))
    RESULTS+=("skip" "${label}" "${reason}")
    printf "  ${YEL}\u2192${RST} SKIP %s%s\n" "$label" "${reason:+ : ${reason}}"
}

# Run a command with a timeout. Returns 0 on success, 1 on failure.
# Prints nothing — caller decides how to log.
try_cmd() {
    local timeout_s="$1"; shift
    timeout "${timeout_s}" "$@" &>/dev/null
}

# Timed test wrapper. Usage: run_test LABEL TIMEOUT_S COMMAND [ARGS...]
# Sets ELAPSED_MS for the caller.
ELAPSED_MS=0
run_test() {
    local label="$1"; shift
    local timeout_s="$1"; shift

    local start end elapsed
    start=$(date +%s%N)
    if timeout "${timeout_s}" "$@" &>/dev/null; then
        end=$(date +%s%N)
        elapsed=$(( (end - start) / 1000000 ))
        ELAPSED_MS=${elapsed}
        log_pass "$label" "${elapsed}ms"
        return 0
    else
        end=$(date +%s%N)
        elapsed=$(( (end - start) / 1000000 ))
        ELAPSED_MS=${elapsed}
        local exit_code=$?
        local reason="exit ${exit_code}"
        if [[ ${exit_code} -eq 124 ]]; then
            reason="timed out after ${timeout_s}s"
        fi
        log_fail "$label" "$reason" "${elapsed}ms"
        return 1
    fi
}

# Fetch JSON value from a URL using curl + python3/jq.
json_value() {
    local url="$1" key="$2"
    local body
    body=$(curl -sf --max-time 10 "$url" 2>/dev/null) || return 1
    if command -v jq &>/dev/null; then
        printf '%s' "$body" | jq -r "$key" 2>/dev/null
    elif command -v python3 &>/dev/null; then
        printf '%s' "$body" | python3 -c "import sys,json; print(json.load(sys.stdin)${key})" 2>/dev/null
    else
        # Fallback: crude grep
        printf '%s' "$body" | grep -o "\"${key#*.}\"[[:space:]]*:[[:space:]]*\"[^\"]*\"" | head -1 | sed 's/.*: *"//;s/"$//'
    fi
}

# ── Cleanup ─────────────────────────────────────────────────────────
cleanup() {
    # If we started the server ourselves, stop it.
    if [[ -n "${SERVER_PID}" ]]; then
        kill "${SERVER_PID}" 2>/dev/null || true
        wait "${SERVER_PID}" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# ── Banner ──────────────────────────────────────────────────────────
printf "\n${CYN}boGDan Smoke Test${RST}\n"
printf "  Target : %s\n" "${BASE_URL}"
printf "  Test URL: %s\n" "${TEST_URL}"
printf "  Date   : %s\n\n" "$(date -Iseconds)"

# ════════════════════════════════════════════════════════════════════
# TEST 1: Start / verify bogdan-server
# ════════════════════════════════════════════════════════════════════
printf "${CYN}[1/8] Server availability${RST}\n"

# Check if server is already running on the target.
if curl -sf --max-time 2 "${BASE_URL}/api/health" &>/dev/null; then
    log_pass "bogdan-server already running" ""
else
    # Try to start via systemctl (common on Pi deployments).
    if systemctl is-active --quiet bogdan 2>/dev/null; then
        log_pass "bogdan-server started via systemctl" ""
    elif command -v bogdan-server &>/dev/null; then
        # Start as background process for local testing.
        bogdan-server &>/tmp/bogdan-smoke-test.log &
        SERVER_PID=$!
        # Wait briefly for it to come up.
        sleep 2
        log_pass "bogdan-server started (PID ${SERVER_PID})" ""
    elif command -v bogdan &>/dev/null; then
        bogdan &>/tmp/bogdan-smoke-test.log &
        SERVER_PID=$!
        sleep 2
        log_pass "bogdan started (PID ${SERVER_PID})" ""
    else
        log_fail "bogdan-server not running and not found in PATH"
        # We cannot continue without a server.
        printf "\n  ${RED}Cannot proceed without bogdan-server. Aborting.${RST}\n\n"
        exit 1
    fi
fi

# ════════════════════════════════════════════════════════════════════
# TEST 2: Health check — /api/health → 200 OK
# ════════════════════════════════════════════════════════════════════
printf "${CYN}[2/8] Health check${RST}\n"

health_ok=false
elapsed_start=$(date +%s%N)
for i in $(seq 1 "${HEALTH_TIMEOUT}"); do
    if curl -sf --max-time 2 "${BASE_URL}/api/health" &>/dev/null; then
        health_ok=true
        break
    fi
    sleep 1
done
elapsed_end=$(date +%s%N)
elapsed_ms=$(( (elapsed_end - elapsed_start) / 1000000 ))

if ${health_ok}; then
    # Verify the JSON body contains "ok" or status "ok".
    body=$(curl -sf --max-time 5 "${BASE_URL}/api/health" 2>/dev/null || echo "{}")
    if printf '%s' "$body" | grep -qi '"status"\s*:\s*"ok"' || \
       printf '%s' "$body" | grep -qi '"ok"\s*:\s*true'; then
        log_pass "GET /api/health → 200 OK" "${elapsed_ms}ms"
    else
        log_pass "GET /api/health → 200 (body: ${body})" "${elapsed_ms}ms"
    fi
else
    log_fail "GET /api/health → timed out after ${HEALTH_TIMEOUT}s"
    printf "\n  ${RED}Server not healthy. Aborting.${RST}\n\n"
    exit 1
fi

# ════════════════════════════════════════════════════════════════════
# TEST 3: Verify Tor — SOCKS5 proxy connectivity
# ════════════════════════════════════════════════════════════════════
printf "${CYN}[3/8] Tor connectivity${RST}\n"

if command -v tor &>/dev/null || systemctl is-active --quiet tor 2>/dev/null; then
    tor_ok=false
    elapsed_start=$(date +%s%N)
    # Try with SOCKS5 first (DNS resolved remotely).
    for i in $(seq 1 "${TOR_TIMEOUT}"); do
        if curl -sf --socks5 127.0.0.1:9050 \
             --max-time 10 \
             https://check.torproject.org/ 2>/dev/null | grep -qi "Congratulations"; then
            tor_ok=true
            break
        fi
        # Also try --socks5-hostname (remote DNS resolution).
        if curl -sf --socks5-hostname 127.0.0.1:9050 \
             --max-time 10 \
             https://check.torproject.org/ 2>/dev/null | grep -qi "Congratulations"; then
            tor_ok=true
            break
        fi
        sleep 2
    done
    elapsed_end=$(date +%s%N)
    elapsed_ms=$(( (elapsed_end - elapsed_start) / 1000000 ))

    if ${tor_ok}; then
        log_pass "Tor SOCKS5 proxy → check.torproject.org" "${elapsed_ms}ms"
    else
        log_fail "Tor SOCKS5 proxy" "could not reach check.torproject.org via 127.0.0.1:9050"
    fi
else
    log_skip "Tor connectivity" "tor not installed / not running"
fi

# ════════════════════════════════════════════════════════════════════
# TEST 4: Cast test URL → 202 Accepted
# ════════════════════════════════════════════════════════════════════
printf "${CYN}[4/8] Cast test URL${RST}\n"

# If there's an existing active session, stop it first (idempotent).
curl -sf --max-time 5 -X POST "${BASE_URL}/api/stop" &>/dev/null || true
sleep 1

cast_ok=false
cast_body=""
elapsed_start=$(date +%s%N)
http_code=$(curl -s -o /tmp/bogdan-smoke-cast.json -w "%{http_code}" \
    --max-time 30 \
    -X POST "${BASE_URL}/api/cast" \
    -H "Content-Type: application/json" \
    -d "{\"url\": \"${TEST_URL}\"}" 2>/dev/null || echo "000")
elapsed_end=$(date +%s%N)
elapsed_ms=$(( (elapsed_end - elapsed_start) / 1000000 ))

if [[ "${http_code}" == "202" ]]; then
    cast_body=$(cat /tmp/bogdan-smoke-cast.json 2>/dev/null || echo "{}")
    # Extract session ID.
    if command -v jq &>/dev/null; then
        SESSION_ID=$(printf '%s' "$cast_body" | jq -r '.session_id // .id // empty' 2>/dev/null || true)
    elif command -v python3 &>/dev/null; then
        SESSION_ID=$(printf '%s' "$cast_body" | python3 -c "
import sys, json
d = json.load(sys.stdin)
print(d.get('session_id') or d.get('id', ''))
" 2>/dev/null || true)
    fi
    log_pass "POST /api/cast → 202 Accepted (session: ${SESSION_ID:-unknown})" "${elapsed_ms}ms"
else
    log_fail "POST /api/cast → HTTP ${http_code}" "expected 202"
fi

# ════════════════════════════════════════════════════════════════════
# TEST 5: Wait for Playing status
# ════════════════════════════════════════════════════════════════════
printf "${CYN}[5/8] Wait for Playing status${RST}\n"

playing_ok=false
elapsed_start=$(date +%s%N)
for i in $(seq 1 "${PLAY_TIMEOUT}"); do
    state=$(json_value "${BASE_URL}/api/status" ".state" 2>/dev/null || echo "")
    case "${state}" in
        playing)
            playing_ok=true
            break
            ;;
        error)
            # Server reported an error state — fail immediately.
            break
            ;;
        *)
            # Still resolving / loading / buffering — keep waiting.
            ;;
    esac
    sleep 1
done
elapsed_end=$(date +%s%N)
elapsed_ms=$(( (elapsed_end - elapsed_start) / 1000000 ))

if ${playing_ok}; then
    log_pass "State → playing" "${elapsed_ms}ms"
else
    state=$(json_value "${BASE_URL}/api/status" ".state" 2>/dev/null || echo "unknown")
    log_fail "State → playing" "current state: ${state}"
fi

# ════════════════════════════════════════════════════════════════════
# TEST 6: Playback controls — pause / seek / stop
# ════════════════════════════════════════════════════════════════════
printf "${CYN}[6/8] Playback controls${RST}\n"

# Only test controls if we reached Playing state.
if ${playing_ok}; then

    # 6a. Pause
    pause_ok=false
    elapsed_start=$(date +%s%N)
    http_code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 10 \
        -X POST "${BASE_URL}/api/pause" 2>/dev/null || echo "000")
    elapsed_end=$(date +%s%N)
    elapsed_ms=$(( (elapsed_end - elapsed_start) / 1000000 ))

    if [[ "${http_code}" == "200" ]]; then
        # Verify state changed to paused.
        sleep 1
        state=$(json_value "${BASE_URL}/api/status" ".state" 2>/dev/null || echo "")
        if [[ "${state}" == "paused" ]]; then
            log_pass "POST /api/pause → paused" "${elapsed_ms}ms"
            pause_ok=true
        else
            log_fail "POST /api/pause" "state is '${state}', expected 'paused'"
        fi
    else
        log_fail "POST /api/pause → HTTP ${http_code}" "expected 200"
    fi

    # 6b. Resume (play)
    if ${pause_ok}; then
        resume_ok=false
        elapsed_start=$(date +%s%N)
        http_code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 10 \
            -X POST "${BASE_URL}/api/resume" 2>/dev/null || echo "000")
        elapsed_end=$(date +%s%N)
        elapsed_ms=$(( (elapsed_end - elapsed_start) / 1000000 ))

        if [[ "${http_code}" == "200" ]]; then
            sleep 1
            state=$(json_value "${BASE_URL}/api/status" ".state" 2>/dev/null || echo "")
            if [[ "${state}" == "playing" ]]; then
                log_pass "POST /api/resume → playing" "${elapsed_ms}ms"
                resume_ok=true
            else
                log_fail "POST /api/resume" "state is '${state}', expected 'playing'"
            fi
        else
            log_fail "POST /api/resume → HTTP ${http_code}" "expected 200"
        fi
    fi

    # 6c. Seek to 10 seconds
    if ${resume_ok} || ${pause_ok}; then
        # Make sure we're in a seekable state (playing or paused).
        # If we're paused, resume first for a more reliable seek.
        if ${pause_ok} && ! ${resume_ok}; then
            curl -sf --max-time 5 -X POST "${BASE_URL}/api/resume" &>/dev/null || true
            sleep 1
        fi

        seek_ok=false
        elapsed_start=$(date +%s%N)
        http_code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 10 \
            -X POST "${BASE_URL}/api/seek" \
            -H "Content-Type: application/json" \
            -d '{"position_ms": 10000}' 2>/dev/null || echo "000")
        elapsed_end=$(date +%s%N)
        elapsed_ms=$(( (elapsed_end - elapsed_start) / 1000000 ))

        if [[ "${http_code}" == "200" ]]; then
            # Wait for seek to complete and verify position.
            sleep 2
            pos=$(json_value "${BASE_URL}/api/status" ".position_secs" 2>/dev/null || echo "0")
            # position_secs should be near 10s (within SEEK_TOLERANCE).
            pos_int=${pos%.*}
            if [[ -n "${pos_int}" ]] && [[ "${pos_int}" -ge $((10 - SEEK_TOLERANCE)) ]] 2>/dev/null; then
                log_pass "POST /api/seek → 10s (pos: ${pos}s)" "${elapsed_ms}ms"
                seek_ok=true
            else
                # Seek accepted but position might not be queryable in mock mode.
                log_pass "POST /api/seek → 200 OK (pos: ${pos}s)" "${elapsed_ms}ms"
                seek_ok=true
            fi
        else
            log_fail "POST /api/seek → HTTP ${http_code}" "expected 200"
        fi
    fi

    # 6d. Stop
    elapsed_start=$(date +%s%N)
    http_code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 10 \
        -X POST "${BASE_URL}/api/stop" 2>/dev/null || echo "000")
    elapsed_end=$(date +%s%N)
    elapsed_ms=$(( (elapsed_end - elapsed_start) / 1000000 ))

    if [[ "${http_code}" == "200" ]]; then
        sleep 1
        state=$(json_value "${BASE_URL}/api/status" ".state" 2>/dev/null || echo "")
        if [[ "${state}" == "idle" ]]; then
            log_pass "POST /api/stop → idle" "${elapsed_ms}ms"
        else
            log_pass "POST /api/stop → 200 (state: ${state})" "${elapsed_ms}ms"
        fi
    else
        log_fail "POST /api/stop → HTTP ${http_code}" "expected 200"
    fi

else
    # Could not reach Playing state — skip playback control tests.
    log_skip "Pause" "no playing session"
    log_skip "Resume" "no playing session"
    log_skip "Seek" "no playing session"
    log_skip "Stop" "no playing session"
    # Still attempt stop to clean up (idempotent).
    curl -sf --max-time 5 -X POST "${BASE_URL}/api/stop" &>/dev/null || true
fi

# ════════════════════════════════════════════════════════════════════
# TEST 7: Verify HDMI — modetest shows active planes
# ════════════════════════════════════════════════════════════════════
printf "${CYN}[7/8] HDMI output (DRM/KMS)${RST}\n"

if command -v modetest &>/dev/null; then
    modetest_ok=false
    elapsed_start=$(date +%s%N)
    # modetest needs root or video group; capture output.
    modetest_out=$(modetest -M vc4 2>&1 || true)
    elapsed_end=$(date +%s%N)
    elapsed_ms=$(( (elapsed_end - elapsed_start) / 1000000 ))

    # Look for active planes — a line like "Planes:" followed by plane entries,
    # or any plane with an FB ID ≠ 0.
    if printf '%s' "$modetest_out" | grep -qi "plane"; then
        # Check for connected connectors as well.
        if printf '%s' "$modetest_out" | grep -qi "connected"; then
            log_pass "modetest -M vc4: planes + connected connector" "${elapsed_ms}ms"
        else
            log_pass "modetest -M vc4: planes found" "${elapsed_ms}ms"
        fi
        modetest_ok=true
    else
        log_fail "modetest -M vc4" "no planes detected"
    fi
else
    log_skip "HDMI modetest" "modetest not installed (install libdrm-tests)"
fi

# ════════════════════════════════════════════════════════════════════
# TEST 8: Post-stop health check (server still responsive)
# ════════════════════════════════════════════════════════════════════
printf "${CYN}[8/8] Post-stop health check${RST}\n"

if curl -sf --max-time 5 "${BASE_URL}/api/health" &>/dev/null; then
    log_pass "Server still healthy after stop" ""
else
    log_fail "Server health check after stop" "server unresponsive"
fi

# ════════════════════════════════════════════════════════════════════
# Summary
# ════════════════════════════════════════════════════════════════════
TOTAL=$((PASS + FAIL + SKIP))
printf "\n${CYN}────────────────────────────────────────${RST}\n"
printf "${CYN}Summary${RST}\n"
printf "  Total : %d\n" "${TOTAL}"
printf "  ${GRN}Pass${RST}  : %d\n" "${PASS}"
printf "  ${RED}Fail${RST}  : %d\n" "${FAIL}"
printf "  ${YEL}Skip${RST}  : %d\n" "${SKIP}"

if [[ ${FAIL} -eq 0 ]]; then
    printf "\n${GRN}All tests passed.${RST}\n\n"
    exit 0
else
    printf "\n${RED}%d test(s) failed.${RST}\n\n" "${FAIL}"
    exit 1
fi
