#!/bin/bash
# ──────────────────────────────────────────────────────────────────────
# boGDan Soak Test — S6.2
# ──────────────────────────────────────────────────────────────────────
#
# Runs 100 cast/stop cycles with varying URL types to verify no
# resource exhaustion (memory, file descriptors, database bloat,
# zombie processes).
#
# After all cycles:
#   - RSS must be within 10 MB of the start value
#   - No zombie processes from bogdan
#   - SQLite database must be < 1 MB
#
# Output:
#   - CSV log:   /tmp/bogdan-soak-test-<timestamp>.csv
#   - Summary:   /tmp/bogdan-soak-test-<timestamp>-report.txt
#
# Prerequisites:
#   - boGDan server installed and configured (systemctl or in PATH)
#   - Tor daemon running (SOCKS5 on 127.0.0.1:9050)
#   - jq recommended (falls back to python3/grep)
#
# Usage:
#   bash scripts/soak-test.sh                          # 100 cycles
#   CYCLE_COUNT=10 bash scripts/soak-test.sh           # quick run
#   PLAY_DURATION=10 bash scripts/soak-test.sh         # shorter play time
#
# Exit codes:
#   0 — all checks pass (no resource exhaustion)
#   1 — one or more checks fail
#   2 — setup failure (server won't start)
#
# ──────────────────────────────────────────────────────────────────────

set -euo pipefail

# ── Configurable Variables ─────────────────────────────────────────────

# Number of cast/stop cycles
CYCLE_COUNT="${CYCLE_COUNT:-100}"

# Seconds to play before stopping each cycle
PLAY_DURATION="${PLAY_DURATION:-30}"

# boGDan server connection
BOGDAN_HOST="${BOGDAN_HOST:-localhost}"
BOGDAN_PORT="${BOGDAN_PORT:-8585}"

# RSS tolerance: maximum acceptable growth from baseline (MB)
RSS_TOLERANCE_MB="${RSS_TOLERANCE_MB:-10}"

# Maximum acceptable FD growth from baseline
FD_GROWTH_LIMIT="${FD_GROWTH_LIMIT:-20}"

# Maximum acceptable SQLite DB size (KB)
DB_SIZE_LIMIT_KB="${DB_SIZE_LIMIT_KB:-1024}"

# Seconds to wait for playback to start before declaring failure
PLAY_START_TIMEOUT="${PLAY_START_TIMEOUT:-30}"

# ── Mock URLs ─────────────────────────────────────────────────────────
# In production these would be YouTube/Voe/direct media URLs.
# For testing, we use a mix of publicly accessible media and
# URL patterns that exercise different resolver code paths.

MOCK_URLS=(
    # Direct media files (exercise souphttpsrc pipeline)
    "https://upload.wikimedia.org/wikipedia/commons/transcoded/c/c0/Big_Buck_Bunny_4K.webm/Big_Buck_Bunny_4K.webm.480p.vp9.webm"

    # Short test clip
    "https://www.w3schools.com/html/mov_bbb.mp4"

    # Ogg format (different container)
    "https://upload.wikimedia.org/wikipedia/commons/8/8f/Example.ogg"

    # WebM format (VP8)
    "https://upload.wikimedia.org/wikipedia/commons/transcoded/2/22/Volcano_Lava_Sample.webm/Volcano_Lava_Sample.webm.480p.vp9.webm"

    # Small MP4 (fast cycle)
    "https://test-videos.co.uk/vids/bigbuckbunny/mp4/h264/360/Big_Buck_Bunny_360_10s_1MB.mp4"

    # Different host (exercises DNS + circuit isolation)
    "https://sample-videos.com/video321/mp4/240/big_buck_bunny_240p_1mb.mp4"

    # Audio-only (exercises audio pipeline without video decode)
    "https://upload.wikimedia.org/wikipedia/commons/c/c8/Example_mml.ogg"

    # Another short video (different codec path)
    "https://www.w3schools.com/html/movie.mp4"
)

# ── Derived Constants ──────────────────────────────────────────────────

BASE_URL="http://${BOGDAN_HOST}:${BOGDAN_PORT}"
TIMESTAMP=$(date '+%Y%m%d-%H%M%S')
CSV_FILE="/tmp/bogdan-soak-test-${TIMESTAMP}.csv"
REPORT_FILE="/tmp/bogdan-soak-test-${TIMESTAMP}-report.txt"
BOGDAN_PID=""
BOGDAN_DB_PATH=""

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

CYCLE_PASS=0
CYCLE_FAIL=0
CYCLE_SKIP=0
FINAL_RESULT="PASS"

# ── Helpers ────────────────────────────────────────────────────────────

log()       { echo -e "[$(date '+%Y-%m-%d %H:%M:%S')] $*"; }
log_info()  { log "${CYAN}[INFO]${NC} $*"; }
log_ok()    { log "${GREEN}[OK]${NC} $*"; }
log_warn()  { log "${YELLOW}[WARN]${NC} $*"; }
log_fail()  { log "${RED}[FAIL]${NC} $*"; }

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

# Read RSS in KB from ps
get_rss_kb() {
    local pid="$1"
    if [[ -z "$pid" ]]; then
        echo "0"
        return
    fi
    ps -o rss= -p "$pid" 2>/dev/null | tr -d ' ' || echo "0"
}

# Read open file descriptor count
get_fd_count() {
    local pid="$1"
    if [[ -z "$pid" ]]; then
        echo "0"
        return
    fi
    if [[ -d "/proc/${pid}/fd" ]]; then
        ls /proc/"${pid}"/fd 2>/dev/null | wc -l || echo "0"
    else
        echo "0"
    fi
}

# Find the boGDan SQLite database file
find_db_path() {
    # Check common locations
    local paths=(
        "/var/lib/bogdan/bogdan.db"
        "/tmp/bogdan/bogdan.db"
        "bogdan.db"
        "./bogdan.db"
        "/home/bogdan/bogdan.db"
    )
    for p in "${paths[@]}"; do
        if [[ -f "$p" ]]; then
            echo "$p"
            return
        fi
    done
    # Try to find it from the process's working directory
    local pid
    pid=$(get_bogdan_pid)
    if [[ -n "$pid" ]] && [[ -d "/proc/${pid}/cwd" ]]; then
        local cwd
        cwd=$(readlink "/proc/${pid}/cwd" 2>/dev/null || true)
        if [[ -n "$cwd" ]] && [[ -f "${cwd}/bogdan.db" ]]; then
            echo "${cwd}/bogdan.db"
            return
        fi
    fi
    echo ""
}

# Get SQLite database size in KB
get_db_size_kb() {
    local db_path="$1"
    if [[ -z "$db_path" ]] || [[ ! -f "$db_path" ]]; then
        echo "0"
        return
    fi
    local size_bytes
    size_bytes=$(stat -c %s "$db_path" 2>/dev/null || echo "0")
    echo $(( size_bytes / 1024 ))
}

# Check for zombie processes from boGDan
count_zombies() {
    # Look for zombie (<defunct>) processes whose parent is bogdan
    local pid
    pid=$(get_bogdan_pid)
    if [[ -z "$pid" ]]; then
        echo "0"
        return
    fi
    # Count zombie children of the bogdan process
    local zombies
    zombies=$(ps -o pid=,ppid=,stat= -A 2>/dev/null | \
        awk -v ppid="$pid" '$2 == ppid && $3 ~ /Z/ {count++} END {print count+0}')
    echo "${zombies}"
}

# ── Cleanup ────────────────────────────────────────────────────────────

cleanup() {
    log_info "Cleaning up..."
    # Stop any active cast
    curl -sf --max-time 5 -X POST "${BASE_URL}/api/stop" &>/dev/null || true
    log_info "CSV saved to: ${CSV_FILE}"
    log_info "Report saved to: ${REPORT_FILE}"
}
trap cleanup EXIT

# ── Banner ─────────────────────────────────────────────────────────────

echo ""
echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}║           boGDan Soak Test (S6.2)                          ║${NC}"
echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${NC}"
echo ""
log_info "Configuration:"
log_info "  Cycle count:     ${CYCLE_COUNT}"
log_info "  Play duration:   ${PLAY_DURATION}s per cycle"
log_info "  RSS tolerance:   ${RSS_TOLERANCE_MB} MB"
log_info "  FD growth limit: ${FD_GROWTH_LIMIT}"
log_info "  DB size limit:   ${DB_SIZE_LIMIT_KB} KB"
log_info "  Target:          ${BASE_URL}"
log_info "  CSV output:      ${CSV_FILE}"
log_info "  Report output:   ${REPORT_FILE}"
echo ""

# ── Step 1: Ensure boGDan is running ──────────────────────────────────

log_info "Step 1: Ensuring boGDan server is running..."

BOGDAN_PID=$(get_bogdan_pid)

if [[ -z "$BOGDAN_PID" ]]; then
    log_info "boGDan not detected — attempting to start..."

    if systemctl is-active --quiet bogdan 2>/dev/null; then
        log_ok "boGDan started via systemctl"
    elif command -v systemctl &>/dev/null; then
        sudo systemctl start bogdan 2>/dev/null && log_ok "Started bogdan.service" || {
            log_warn "systemctl failed — trying bogdan-server binary..."
            if command -v bogdan-server &>/dev/null; then
                bogdan-server &>/tmp/bogdan-soak-test-server.log &
                log_ok "Started bogdan-server (PID $!)"
            elif command -v bogdan &>/dev/null; then
                bogdan &>/tmp/bogdan-soak-test-server.log &
                log_ok "Started bogdan (PID $!)"
            else
                log_fail "Cannot find or start boGDan — aborting"
                exit 2
            fi
        }
    else
        if command -v bogdan-server &>/dev/null; then
            bogdan-server &>/tmp/bogdan-soak-test-server.log &
            log_ok "Started bogdan-server (PID $!)"
        elif command -v bogdan &>/dev/null; then
            bogdan &>/tmp/bogdan-soak-test-server.log &
            log_ok "Started bogdan (PID $!)"
        else
            log_fail "Cannot find boGDan binary — aborting"
            exit 2
        fi
    fi

    log_info "Waiting for server to be healthy..."
    for i in $(seq 1 30); do
        if curl -sf --max-time 2 "${BASE_URL}/api/health" &>/dev/null; then
            log_ok "Server is healthy"
            break
        fi
        sleep 2
    done
fi

BOGDAN_PID=$(get_bogdan_pid)
if [[ -z "$BOGDAN_PID" ]]; then
    log_fail "Cannot find boGDan process ID — aborting"
    exit 2
fi
log_ok "boGDan PID: ${BOGDAN_PID}"

# ── Step 2: Record baseline ───────────────────────────────────────────

log_info "Step 2: Recording baseline metrics..."

BASELINE_RSS_KB=$(get_rss_kb "$BOGDAN_PID")
BASELINE_RSS_MB=$((BASELINE_RSS_KB / 1024))
BASELINE_FD=$(get_fd_count "$BOGDAN_PID")

BOGDAN_DB_PATH=$(find_db_path)
if [[ -n "$BOGDAN_DB_PATH" ]]; then
    BASELINE_DB_KB=$(get_db_size_kb "$BOGDAN_DB_PATH")
    log_ok "SQLite DB: ${BOGDAN_DB_PATH} (${BASELINE_DB_KB} KB)"
else
    BASELINE_DB_KB=0
    log_warn "SQLite DB not found — DB size checks will be skipped"
fi

BASELINE_ZOMBIES=$(count_zombies)

log_ok "Baseline: RSS=${BASELINE_RSS_MB} MB, FDs=${BASELINE_FD}, Zombies=${BASELINE_ZOMBIES}"

# ── Step 3: Initialize CSV ────────────────────────────────────────────

log_info "Step 3: Initializing CSV log at ${CSV_FILE}"

echo "cycle,timestamp,url,rss_mb,fd_count,db_size_kb,zombies,cast_result,play_state,stop_result" > "$CSV_FILE"

# ── Step 4: Main loop — cast/play/stop cycles ─────────────────────────

log_info "Step 4: Starting ${CYCLE_COUNT} cast/stop cycles..."
echo ""

CYCLE_NUM=0
MAX_RSS_MB=$BASELINE_RSS_MB
MIN_RSS_MB=$BASELINE_RSS_MB

while [[ $CYCLE_NUM -lt $CYCLE_COUNT ]]; do
    CYCLE_NUM=$((CYCLE_NUM + 1))

    # Pick URL (cycle through the array, also add variation)
    URL_INDEX=$(( (CYCLE_NUM - 1) % ${#MOCK_URLS[@]} ))
    CAST_URL="${MOCK_URLS[$URL_INDEX]}"

    # Progress indicator
    if [[ $((CYCLE_NUM % 10)) -eq 0 ]] || [[ "$CYCLE_NUM" -eq 1 ]]; then
        log_info "─── Cycle ${CYCLE_NUM}/${CYCLE_COUNT} ───"
    fi

    # --- Cast ---
    CAST_RESULT="ok"
    HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" --max-time 30 \
        -X POST "${BASE_URL}/api/cast" \
        -H "Content-Type: application/json" \
        -d "{\"url\": \"${CAST_URL}\"}" 2>/dev/null || echo "000")

    if [[ "$HTTP_CODE" != "202" && "$HTTP_CODE" != "200" ]]; then
        CAST_RESULT="fail_${HTTP_CODE}"
        log_warn "Cycle ${CYCLE_NUM}: Cast failed (HTTP ${HTTP_CODE})"
    fi

    # --- Wait for playback ---
    PLAY_STATE="unknown"
    if [[ "$CAST_RESULT" == "ok" ]]; then
        # Wait for playing state (or timeout)
        ELAPSED=0
        while [[ $ELAPSED -lt $PLAY_START_TIMEOUT ]]; do
            STATE=$(json_value "${BASE_URL}/api/status" ".state" 2>/dev/null || echo "")
            if [[ "$STATE" == "playing" ]]; then
                PLAY_STATE="playing"
                break
            elif [[ "$STATE" == "error" ]]; then
                PLAY_STATE="error"
                break
            fi
            sleep 1
            ELAPSED=$((ELAPSED + 1))
        done

        if [[ "$PLAY_STATE" != "playing" ]]; then
            PLAY_STATE=$(json_value "${BASE_URL}/api/status" ".state" 2>/dev/null || echo "timeout")
            log_warn "Cycle ${CYCLE_NUM}: Did not reach 'playing' state (got: ${PLAY_STATE})"
        fi

        # Play for the specified duration
        sleep "$PLAY_DURATION"
    else
        sleep 2  # Brief pause even on cast failure
        PLAY_STATE="skipped"
    fi

    # --- Stop ---
    STOP_RESULT="ok"
    HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" --max-time 10 \
        -X POST "${BASE_URL}/api/stop" 2>/dev/null || echo "000")

    if [[ "$HTTP_CODE" != "200" && "$HTTP_CODE" != "204" ]]; then
        STOP_RESULT="fail_${HTTP_CODE}"
        log_warn "Cycle ${CYCLE_NUM}: Stop returned HTTP ${HTTP_CODE}"
    fi

    # Brief pause after stop to let resources be released
    sleep 2

    # --- Collect metrics after cycle ---
    CURRENT_PID=$(get_bogdan_pid)
    if [[ -n "$CURRENT_PID" ]] && [[ "$CURRENT_PID" != "$BOGDAN_PID" ]]; then
        log_warn "PID changed: ${BOGDAN_PID} → ${CURRENT_PID}"
        BOGDAN_PID=$CURRENT_PID
    fi

    RSS_KB=$(get_rss_kb "$BOGDAN_PID")
    RSS_MB=$((RSS_KB / 1024))
    FD_COUNT=$(get_fd_count "$BOGDAN_PID")
    DB_SIZE_KB=$(get_db_size_kb "$BOGDAN_DB_PATH")
    ZOMBIES=$(count_zombies)

    # Track min/max RSS
    if [[ $RSS_MB -gt $MAX_RSS_MB ]]; then
        MAX_RSS_MB=$RSS_MB
    fi
    if [[ $RSS_MB -lt $MIN_RSS_MB ]]; then
        MIN_RSS_MB=$RSS_MB
    fi

    # Determine cycle result
    if [[ "$CAST_RESULT" == "ok" && "$PLAY_STATE" == "playing" && "$STOP_RESULT" == "ok" ]]; then
        CYCLE_PASS=$((CYCLE_PASS + 1))
    elif [[ "$PLAY_STATE" == "skipped" ]]; then
        CYCLE_SKIP=$((CYCLE_SKIP + 1))
    else
        CYCLE_FAIL=$((CYCLE_FAIL + 1))
    fi

    # Log to CSV
    echo "${CYCLE_NUM},$(date -Iseconds),${CAST_URL},${RSS_MB},${FD_COUNT},${DB_SIZE_KB},${ZOMBIES},${CAST_RESULT},${PLAY_STATE},${STOP_RESULT}" >> "$CSV_FILE"

    # Progress dot
    if [[ $((CYCLE_NUM % 10)) -ne 0 ]]; then
        printf "."
    else
        echo ""  # newline after progress dots
    fi

    # Alert on zombies
    if [[ "$ZOMBIES" -gt 0 ]]; then
        log_warn "Cycle ${CYCLE_NUM}: ${ZOMBIES} zombie process(es) detected!"
    fi
done

echo ""  # newline after progress dots

# ── Step 5: Final verification ────────────────────────────────────────

log_info "Step 5: Running final verification checks..."
echo ""

FINAL_PID=$(get_bogdan_pid)
FINAL_RSS_KB=$(get_rss_kb "$FINAL_PID")
FINAL_RSS_MB=$((FINAL_RSS_KB / 1024))
FINAL_FD=$(get_fd_count "$FINAL_PID")
FINAL_DB_KB=$(get_db_size_kb "$BOGDAN_DB_PATH")
FINAL_ZOMBIES=$(count_zombies)

RSS_DELTA_MB=$((FINAL_RSS_MB - BASELINE_RSS_MB))
FD_DELTA=$((FINAL_FD - BASELINE_FD))
DB_DELTA_KB=$((FINAL_DB_KB - BASELINE_DB_KB))

# Check 1: RSS within tolerance
RSS_CHECK="PASS"
if [[ $RSS_DELTA_MB -gt $RSS_TOLERANCE_MB ]]; then
    RSS_CHECK="FAIL"
    FINAL_RESULT="FAIL"
    log_fail "RSS check: ${FINAL_RSS_MB} MB (Δ${RSS_DELTA_MB} MB) exceeds tolerance of ${RSS_TOLERANCE_MB} MB"
else
    log_ok "RSS check: ${FINAL_RSS_MB} MB (Δ${RSS_DELTA_MB} MB) within tolerance of ${RSS_TOLERANCE_MB} MB"
fi

# Check 2: FD growth within limit
FD_CHECK="PASS"
if [[ $FD_DELTA -gt $FD_GROWTH_LIMIT ]]; then
    FD_CHECK="FAIL"
    FINAL_RESULT="FAIL"
    log_fail "FD check: ${FINAL_FD} (Δ${FD_DELTA}) exceeds limit of ${FD_GROWTH_LIMIT}"
else
    log_ok "FD check: ${FINAL_FD} (Δ${FD_DELTA}) within limit of ${FD_GROWTH_LIMIT}"
fi

# Check 3: No zombie processes
ZOMBIE_CHECK="PASS"
if [[ $FINAL_ZOMBIES -gt 0 ]]; then
    ZOMBIE_CHECK="FAIL"
    FINAL_RESULT="FAIL"
    log_fail "Zombie check: ${FINAL_ZOMBIES} zombie process(es) detected"
else
    log_ok "Zombie check: No zombie processes detected"
fi

# Check 4: SQLite DB size < 1 MB
DB_CHECK="PASS"
if [[ -n "$BOGDAN_DB_PATH" ]]; then
    if [[ $FINAL_DB_KB -gt $DB_SIZE_LIMIT_KB ]]; then
        DB_CHECK="FAIL"
        FINAL_RESULT="FAIL"
        log_fail "DB size check: ${FINAL_DB_KB} KB exceeds limit of ${DB_SIZE_LIMIT_KB} KB"
    else
        log_ok "DB size check: ${FINAL_DB_KB} KB within limit of ${DB_SIZE_LIMIT_KB} KB"
    fi
else
    DB_CHECK="SKIP"
    log_warn "DB size check: Skipped (DB file not found)"
fi

# Check 5: Server still healthy
HEALTH_CHECK="PASS"
if curl -sf --max-time 5 "${BASE_URL}/api/health" &>/dev/null; then
    log_ok "Health check: Server is still responsive"
else
    HEALTH_CHECK="FAIL"
    FINAL_RESULT="FAIL"
    log_fail "Health check: Server is unresponsive after ${CYCLE_COUNT} cycles"
fi

# ── Step 6: Generate report ───────────────────────────────────────────

log_info "Generating report..."

cat > "$REPORT_FILE" <<EOF
═══════════════════════════════════════════════════════════════
  boGDan Soak Test Report (S6.2)
═══════════════════════════════════════════════════════════════

Test Date:         $(date -u '+%Y-%m-%d %H:%M:%S UTC')
Cycle Count:       ${CYCLE_COUNT}
Play Duration:     ${PLAY_DURATION}s per cycle
boGDan PID:        ${FINAL_PID}
Target:            ${BASE_URL}

─── Cycle Results ─────────────────────────────────────────────

  Passed:    ${CYCLE_PASS}
  Failed:    ${CYCLE_FAIL}
  Skipped:   ${CYCLE_SKIP}
  Success:   $(awk "BEGIN {printf \"%.1f\", (${CYCLE_PASS} / ${CYCLE_COUNT}) * 100}")%

─── Baseline vs Final ────────────────────────────────────────

                    Baseline        Final           Delta
  RSS (MB):         ${BASELINE_RSS_MB}              ${FINAL_RSS_MB}              ${RSS_DELTA_MB}
  File Descriptors: ${BASELINE_FD}              ${FINAL_FD}              ${FD_DELTA}
  DB Size (KB):     ${BASELINE_DB_KB}              ${FINAL_DB_KB}              ${DB_DELTA_KB}
  Zombie Procs:     ${BASELINE_ZOMBIES}              ${FINAL_ZOMBIES}              $((FINAL_ZOMBIES - BASELINE_ZOMBIES))

  RSS Range:        ${MIN_RSS_MB} – ${MAX_RSS_MB} MB (span: $((MAX_RSS_MB - MIN_RSS_MB)) MB)

─── Verification Checks ──────────────────────────────────────

  RSS within ${RSS_TOLERANCE_MB} MB of start:   ${RSS_CHECK}
  FD growth within ${FD_GROWTH_LIMIT}:          ${FD_CHECK}
  No zombie processes:                ${ZOMBIE_CHECK}
  DB size < 1 MB:                     ${DB_CHECK}
  Server still healthy:               ${HEALTH_CHECK}

─── Verdict ──────────────────────────────────────────────────

EOF

if [[ "$FINAL_RESULT" == "PASS" ]]; then
    echo "  PASS — No resource exhaustion detected." >> "$REPORT_FILE"
    echo "" >> "$REPORT_FILE"
    echo "  All ${CYCLE_COUNT} cycles completed without exceeding resource limits." >> "$REPORT_FILE"
    echo "  RSS growth: ${RSS_DELTA_MB} MB (tolerance: ${RSS_TOLERANCE_MB} MB)" >> "$REPORT_FILE"
    echo "  FD growth: ${FD_DELTA} (limit: ${FD_GROWTH_LIMIT})" >> "$REPORT_FILE"
else
    echo "  *** FAIL — Resource exhaustion detected ***" >> "$REPORT_FILE"
    echo "" >> "$REPORT_FILE"
    echo "  Failed checks:" >> "$REPORT_FILE"
    [[ "$RSS_CHECK" == "FAIL" ]]    && echo "    - RSS grew by ${RSS_DELTA_MB} MB (limit: ${RSS_TOLERANCE_MB} MB)" >> "$REPORT_FILE"
    [[ "$FD_CHECK" == "FAIL" ]]     && echo "    - FD count grew by ${FD_DELTA} (limit: ${FD_GROWTH_LIMIT})" >> "$REPORT_FILE"
    [[ "$ZOMBIE_CHECK" == "FAIL" ]] && echo "    - ${FINAL_ZOMBIES} zombie process(es) detected" >> "$REPORT_FILE"
    [[ "$DB_CHECK" == "FAIL" ]]     && echo "    - DB size ${FINAL_DB_KB} KB exceeds ${DB_SIZE_LIMIT_KB} KB" >> "$REPORT_FILE"
    [[ "$HEALTH_CHECK" == "FAIL" ]] && echo "    - Server became unresponsive" >> "$REPORT_FILE"
    echo "" >> "$REPORT_FILE"
    echo "  Recommended next steps:" >> "$REPORT_FILE"
    echo "    1. Run mem-test.sh for long-duration profiling" >> "$REPORT_FILE"
    echo "    2. Check for unclosed resources with:" >> "$REPORT_FILE"
    echo "       ls -la /proc/\$(pgrep bogdan)/fd | sort -k11" >> "$REPORT_FILE"
    echo "    3. Inspect GStreamer element lifecycle with GST_DEBUG=GST_REFCOUNTING:6" >> "$REPORT_FILE"
    echo "    4. Run under Valgrind with --leak-check=full --track-fds=yes" >> "$REPORT_FILE"
fi

echo "" >> "$REPORT_FILE"
echo "─── Raw Data ────────────────────────────────────────────────" >> "$REPORT_FILE"
echo "" >> "$REPORT_FILE"
echo "  CSV file: ${CSV_FILE}" >> "$REPORT_FILE"
echo "" >> "$REPORT_FILE"
echo "═══════════════════════════════════════════════════════════════" >> "$REPORT_FILE"

# Print the report to console
echo ""
cat "$REPORT_FILE"

# ── Exit code ──────────────────────────────────────────────────────────

if [[ "$FINAL_RESULT" == "PASS" ]]; then
    exit 0
else
    exit 1
fi
