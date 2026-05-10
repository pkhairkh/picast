#!/bin/bash
# ──────────────────────────────────────────────────────────────────────
# boGDan Memory Leak Test — S6.1
# ──────────────────────────────────────────────────────────────────────
#
# Runs an 8-hour continuous playback session on Pi 4 and monitors:
#   - RSS (Resident Set Size) growth via `ps`
#   - Open file descriptor count via /proc/<pid>/fd
#   - GStreamer buffer pool statistics (if available via /api/status)
#
# Target: <10 MB/hour leak rate (i.e. <80 MB total over 8 hours).
#
# Output:
#   - CSV log:   /tmp/bogdan-mem-test-<timestamp>.csv
#   - Summary:   /tmp/bogdan-mem-test-<timestamp>-report.txt
#
# Prerequisites:
#   - boGDan server installed and configured (systemctl or in PATH)
#   - Tor daemon running (SOCKS5 on 127.0.0.1:9050)
#   - Network connectivity for media streaming
#   - jq recommended (falls back to python3/grep)
#
# Usage:
#   sudo bash scripts/mem-test.sh                    # default 8-hour test
#   DURATION_HOURS=2 bash scripts/mem-test.sh        # shorter run for dev
#   TEST_URL="https://..." bash scripts/mem-test.sh  # custom media URL
#
# Exit codes:
#   0 — test completed, no leak detected
#   1 — test completed, leak detected (exceeds threshold)
#   2 — setup failure (server won't start, etc.)
#
# ──────────────────────────────────────────────────────────────────────

set -euo pipefail

# ── Configurable Variables ─────────────────────────────────────────────

# Test duration in hours (default: 8)
DURATION_HOURS="${DURATION_HOURS:-8}"

# Interval between samples in minutes (default: 5)
SAMPLE_INTERVAL_MIN="${SAMPLE_INTERVAL_MIN:-5}"

# boGDan server connection
BOGDAN_HOST="${BOGDAN_HOST:-localhost}"
BOGDAN_PORT="${BOGDAN_PORT:-8585}"

# URL to cast for the duration test
# Big Buck Bunny — a reliable, long-form, royalty-free test video
TEST_URL="${TEST_URL:-https://upload.wikimedia.org/wikipedia/commons/transcoded/c/c0/Big_Buck_Bunny_4K.webm/Big_Buck_Bunny_4K.webm.480p.vp9.webm}"

# Leak threshold: maximum acceptable RSS growth rate in MB/hour
LEAK_THRESHOLD_MB_PER_HOUR="${LEAK_THRESHOLD_MB_PER_HOUR:-10}"

# Maximum acceptable FD growth over the entire test
FD_GROWTH_LIMIT="${FD_GROWTH_LIMIT:-50}"

# ── Derived Constants ──────────────────────────────────────────────────

BASE_URL="http://${BOGDAN_HOST}:${BOGDAN_PORT}"
SAMPLE_INTERVAL_SECS=$((SAMPLE_INTERVAL_MIN * 60))
TOTAL_SAMPLES=$(( (DURATION_HOURS * 60) / SAMPLE_INTERVAL_MIN ))
TIMESTAMP=$(date '+%Y%m%d-%H%M%S')
CSV_FILE="/tmp/bogdan-mem-test-${TIMESTAMP}.csv"
REPORT_FILE="/tmp/bogdan-mem-test-${TIMESTAMP}-report.txt"
BOGDAN_PID=""

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
    # Try pgrep first (most reliable)
    local pid
    pid=$(pgrep -xf "bogdan" 2>/dev/null || true)
    if [[ -z "$pid" ]]; then
        pid=$(pgrep -xf "bogdan-server" 2>/dev/null || true)
    fi
    if [[ -z "$pid" ]]; then
        # Broader match
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
    # Count entries in /proc/<pid>/fd
    if [[ -d "/proc/${pid}/fd" ]]; then
        ls /proc/"${pid}"/fd 2>/dev/null | wc -l || echo "0"
    else
        echo "0"
    fi
}

# Try to read GStreamer buffer pool stats from /api/status
get_gst_stats() {
    local stats
    stats=$(curl -sf --max-time 5 "${BASE_URL}/api/status" 2>/dev/null || echo "{}")
    if command -v jq &>/dev/null; then
        # Try to extract GStreamer-related fields if present
        local buffer_pool_size
        buffer_pool_size=$(printf '%s' "$stats" | jq -r '.gst_buffer_pool_size // .buffer_pool_size // "N/A"' 2>/dev/null || echo "N/A")
        local buffer_count
        buffer_count=$(printf '%s' "$stats" | jq -r '.gst_buffer_count // .buffer_count // "N/A"' 2>/dev/null || echo "N/A")
        echo "buffer_pool_size=${buffer_pool_size},buffer_count=${buffer_count}"
    else
        echo "buffer_pool_size=N/A,buffer_count=N/A"
    fi
}

# ── Cleanup ────────────────────────────────────────────────────────────

# We do NOT stop boGDan on exit — the user started it, they should stop it.
# But we do stop any active cast to free resources.
cleanup() {
    log_info "Cleaning up..."
    # Stop any active cast
    curl -sf --max-time 5 -X POST "${BASE_URL}/api/stop" &>/dev/null || true
    log_info "Cast stopped. CSV saved to: ${CSV_FILE}"
    log_info "Report saved to: ${REPORT_FILE}"
}
trap cleanup EXIT

# ── Banner ─────────────────────────────────────────────────────────────

echo ""
echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}║           boGDan Memory Leak Test (S6.1)                   ║${NC}"
echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${NC}"
echo ""
log_info "Configuration:"
log_info "  Duration:        ${DURATION_HOURS} hours"
log_info "  Sample interval: ${SAMPLE_INTERVAL_MIN} minutes"
log_info "  Total samples:   ${TOTAL_SAMPLES}"
log_info "  Leak threshold:  ${LEAK_THRESHOLD_MB_PER_HOUR} MB/hour"
log_info "  FD growth limit: ${FD_GROWTH_LIMIT} over test"
log_info "  Target:          ${BASE_URL}"
log_info "  Test URL:        ${TEST_URL}"
log_info "  CSV output:      ${CSV_FILE}"
log_info "  Report output:   ${REPORT_FILE}"
echo ""

# ── Step 1: Ensure boGDan is running ──────────────────────────────────

log_info "Step 1: Ensuring boGDan server is running..."

# Check if already running
BOGDAN_PID=$(get_bogdan_pid)

if [[ -z "$BOGDAN_PID" ]]; then
    log_info "boGDan not detected — attempting to start..."

    # Try systemctl first (production Pi deployment)
    if systemctl is-active --quiet bogdan 2>/dev/null; then
        log_ok "boGDan started via systemctl"
    elif command -v systemctl &>/dev/null; then
        sudo systemctl start bogdan 2>/dev/null && log_ok "Started bogdan.service" || {
            # Fallback: try starting the binary directly
            log_warn "systemctl failed — trying bogdan-server binary..."
            if command -v bogdan-server &>/dev/null; then
                bogdan-server &>/tmp/bogdan-mem-test-server.log &
                log_ok "Started bogdan-server (PID $!)"
            elif command -v bogdan &>/dev/null; then
                bogdan &>/tmp/bogdan-mem-test-server.log &
                log_ok "Started bogdan (PID $!)"
            else
                log_fail "Cannot find or start boGDan — aborting"
                exit 2
            fi
        }
    else
        # No systemctl — try direct binary
        if command -v bogdan-server &>/dev/null; then
            bogdan-server &>/tmp/bogdan-mem-test-server.log &
            log_ok "Started bogdan-server (PID $!)"
        elif command -v bogdan &>/dev/null; then
            bogdan &>/tmp/bogdan-mem-test-server.log &
            log_ok "Started bogdan (PID $!)"
        else
            log_fail "Cannot find boGDan binary — aborting"
            exit 2
        fi
    fi

    # Wait for server to come up
    log_info "Waiting for server to be healthy..."
    for i in $(seq 1 30); do
        if curl -sf --max-time 2 "${BASE_URL}/api/health" &>/dev/null; then
            log_ok "Server is healthy"
            break
        fi
        sleep 2
    done
fi

# Re-read PID after potential start
BOGDAN_PID=$(get_bogdan_pid)
if [[ -z "$BOGDAN_PID" ]]; then
    log_fail "Cannot find boGDan process ID — aborting"
    exit 2
fi
log_ok "boGDan PID: ${BOGDAN_PID}"

# ── Step 2: Start casting ─────────────────────────────────────────────

log_info "Step 2: Starting continuous cast..."

# Stop any existing session first (idempotent)
curl -sf --max-time 5 -X POST "${BASE_URL}/api/stop" &>/dev/null || true
sleep 2

# Cast the test URL
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" --max-time 30 \
    -X POST "${BASE_URL}/api/cast" \
    -H "Content-Type: application/json" \
    -d "{\"url\": \"${TEST_URL}\"}" 2>/dev/null || echo "000")

if [[ "$HTTP_CODE" == "202" ]]; then
    log_ok "Cast accepted (HTTP 202)"
else
    log_warn "Cast returned HTTP ${HTTP_CODE} (expected 202) — continuing anyway"
fi

# Wait for playback to start (up to 60s)
log_info "Waiting for playback to start..."
PLAYBACK_STARTED=false
for i in $(seq 1 60); do
    STATE=$(json_value "${BASE_URL}/api/status" ".state" 2>/dev/null || echo "")
    if [[ "$STATE" == "playing" ]]; then
        log_ok "Playback started"
        PLAYBACK_STARTED=true
        break
    fi
    sleep 1
done

if [[ "$PLAYBACK_STARTED" != "true" ]]; then
    STATE=$(json_value "${BASE_URL}/api/status" ".state" 2>/dev/null || echo "unknown")
    log_warn "Playback did not reach 'playing' state (current: ${STATE}) — monitoring anyway"
fi

# ── Step 3: Initialize CSV ────────────────────────────────────────────

log_info "Step 3: Initializing CSV log at ${CSV_FILE}"

# CSV header
echo "timestamp,elapsed_hours,rss_mb,fd_count,gst_stats,playback_state" > "$CSV_FILE"

# ── Step 4: Monitoring loop ───────────────────────────────────────────

log_info "Step 4: Starting ${DURATION_HOURS}-hour monitoring (sample every ${SAMPLE_INTERVAL_MIN} min)"
log_info "        Press Ctrl+C to stop early and generate report"
echo ""

# Record baseline
BASELINE_RSS_KB=$(get_rss_kb "$BOGDAN_PID")
BASELINE_FD=$(get_fd_count "$BOGDAN_PID")
BASELINE_RSS_MB=$((BASELINE_RSS_KB / 1024))

log_info "Baseline: RSS=${BASELINE_RSS_MB} MB, FDs=${BASELINE_FD}"

SAMPLE_NUM=0
PREV_RSS_KB=$BASELINE_RSS_KB

while [[ $SAMPLE_NUM -lt $TOTAL_SAMPLES ]]; do
    SAMPLE_NUM=$((SAMPLE_NUM + 1))
    ELAPSED_SECS=$((SAMPLE_NUM * SAMPLE_INTERVAL_SECS))
    ELAPSED_HOURS=$(awk "BEGIN {printf \"%.2f\", ${ELAPSED_SECS} / 3600}")

    # Re-read PID (process might have been restarted by systemd)
    CURRENT_PID=$(get_bogdan_pid)
    if [[ -n "$CURRENT_PID" ]] && [[ "$CURRENT_PID" != "$BOGDAN_PID" ]]; then
        log_warn "PID changed: ${BOGDAN_PID} → ${CURRENT_PID} (server may have restarted)"
        BOGDAN_PID=$CURRENT_PID
    fi

    # Collect metrics
    RSS_KB=$(get_rss_kb "$BOGDAN_PID")
    RSS_MB=$((RSS_KB / 1024))
    FD_COUNT=$(get_fd_count "$BOGDAN_PID")
    GST_STATS=$(get_gst_stats)
    PLAYBACK_STATE=$(json_value "${BASE_URL}/api/status" ".state" 2>/dev/null || echo "unknown")

    # If playback stopped (end of stream), re-cast
    if [[ "$PLAYBACK_STATE" == "idle" || "$PLAYBACK_STATE" == "error" ]]; then
        log_warn "Playback ended (state: ${PLAYBACK_STATE}) — re-casting..."
        curl -sf --max-time 30 -X POST "${BASE_URL}/api/cast" \
            -H "Content-Type: application/json" \
            -d "{\"url\": \"${TEST_URL}\"}" &>/dev/null || true
        sleep 5
        PLAYBACK_STATE=$(json_value "${BASE_URL}/api/status" ".state" 2>/dev/null || echo "unknown")
    fi

    # Calculate RSS delta from baseline
    RSS_DELTA_MB=$((RSS_MB - BASELINE_RSS_MB))

    # Log to console
    log_info "Sample ${SAMPLE_NUM}/${TOTAL_SAMPLES} | " \
             "Elapsed: ${ELAPSED_HOURS}h | " \
             "RSS: ${RSS_MB} MB (Δ${RSS_DELTA_MB} MB) | " \
             "FDs: ${FD_COUNT} | " \
             "State: ${PLAYBACK_STATE}"

    # Write to CSV
    echo "$(date -Iseconds),${ELAPSED_HOURS},${RSS_MB},${FD_COUNT},${GST_STATS},${PLAYBACK_STATE}" >> "$CSV_FILE"

    # Check if this is the last sample — don't sleep
    if [[ $SAMPLE_NUM -lt $TOTAL_SAMPLES ]]; then
        sleep "$SAMPLE_INTERVAL_SECS"
    fi
done

# ── Step 5: Stop cast ─────────────────────────────────────────────────

log_info "Stopping playback..."
curl -sf --max-time 5 -X POST "${BASE_URL}/api/stop" &>/dev/null || true

# ── Step 6: Generate report ───────────────────────────────────────────

log_info "Generating report..."

# Collect final metrics
FINAL_RSS_KB=$(get_rss_kb "$BOGDAN_PID")
FINAL_RSS_MB=$((FINAL_RSS_KB / 1024))
FINAL_FD=$(get_fd_count "$BOGDAN_PID")

RSS_TOTAL_GROWTH_MB=$((FINAL_RSS_MB - BASELINE_RSS_MB))
FD_TOTAL_GROWTH=$((FINAL_FD - BASELINE_FD))

# Calculate growth rate (MB/hour), using awk for floating point
if [[ "$DURATION_HOURS" -gt 0 ]]; then
    RSS_GROWTH_RATE=$(awk "BEGIN {printf \"%.2f\", ${RSS_TOTAL_GROWTH_MB} / ${DURATION_HOURS}}")
else
    RSS_GROWTH_RATE="0.00"
fi

# Determine pass/fail
LEAK_DETECTED=false
if awk "BEGIN {exit !(${RSS_GROWTH_RATE} > ${LEAK_THRESHOLD_MB_PER_HOUR})}"; then
    LEAK_DETECTED=true
fi

FD_LEAK_DETECTED=false
if [[ $FD_TOTAL_GROWTH -gt $FD_GROWTH_LIMIT ]]; then
    FD_LEAK_DETECTED=true
fi

# Find peak RSS from CSV
PEAK_RSS_MB=$(awk -F',' 'NR>1 {if ($3 > max) max=$3} END {print max+0}' "$CSV_FILE")
MIN_RSS_MB=$(awk -F',' 'NR>1 {if (NR==2 || $3 < min) min=$3} END {print min+0}' "$CSV_FILE")

# Compute hourly breakdown (average RSS per hour)
HOURLY_SUMMARY=""
for h in $(seq 0 $((DURATION_HOURS - 1))); do
    HOUR_START=$(awk "BEGIN {printf \"%.2f\", ${h}}")
    HOUR_END=$(awk "BEGIN {printf \"%.2f\", ${h} + 1}")
    HOUR_AVG=$(awk -F',' -v hs="$HOUR_START" -v he="$HOUR_END" \
        'NR>1 && $2>=hs+0 && $2<he+0 {sum+=$3; count++} END {if(count>0) printf "%.1f", sum/count; else print "N/A"}' "$CSV_FILE")
    HOURLY_SUMMARY+="  Hour ${h}: avg RSS = ${HOUR_AVG} MB\n"
done

# Write the report
cat > "$REPORT_FILE" <<EOF
═══════════════════════════════════════════════════════════════
  boGDan Memory Leak Test Report (S6.1)
═══════════════════════════════════════════════════════════════

Test Date:         $(date -u '+%Y-%m-%d %H:%M:%S UTC')
Duration:          ${DURATION_HOURS} hours
Sample Interval:   ${SAMPLE_INTERVAL_MIN} minutes
Total Samples:     ${TOTAL_SAMPLES}
Test URL:          ${TEST_URL}
boGDan PID:        ${BOGDAN_PID}

─── Baseline vs Final ────────────────────────────────────────

                    Baseline        Final           Delta
  RSS (MB):         ${BASELINE_RSS_MB}              ${FINAL_RSS_MB}              ${RSS_TOTAL_GROWTH_MB}
  File Descriptors: ${BASELINE_FD}              ${FINAL_FD}              ${FD_TOTAL_GROWTH}

─── Growth Analysis ──────────────────────────────────────────

  RSS Growth Rate:     ${RSS_GROWTH_RATE} MB/hour
  Leak Threshold:      ${LEAK_THRESHOLD_MB_PER_HOUR} MB/hour
  RSS Leak Detected:   ${LEAK_DETECTED}

  FD Growth:           ${FD_TOTAL_GROWTH}
  FD Limit:            ${FD_GROWTH_LIMIT}
  FD Leak Detected:    ${FD_LEAK_DETECTED}

  Peak RSS:            ${PEAK_RSS_MB} MB
  Min RSS:             ${MIN_RSS_MB} MB
  RSS Range:           $((PEAK_RSS_MB - MIN_RSS_MB)) MB

─── Hourly Breakdown ─────────────────────────────────────────

$(echo -e "$HOURLY_SUMMARY")

─── Verdict ──────────────────────────────────────────────────

EOF

if $LEAK_DETECTED; then
    echo "  *** LEAK DETECTED ***" >> "$REPORT_FILE"
    echo "" >> "$REPORT_FILE"
    echo "  RSS growth rate (${RSS_GROWTH_RATE} MB/hour) exceeds threshold (${LEAK_THRESHOLD_MB_PER_HOUR} MB/hour)." >> "$REPORT_FILE"
    echo "" >> "$REPORT_FILE"
    echo "  Recommended next steps:" >> "$REPORT_FILE"
    echo "    1. Run boGDan under Valgrind to identify the leak source:" >> "$REPORT_FILE"
    echo "       valgrind --leak-check=full --show-leak-kinds=all \\" >> "$REPORT_FILE"
    echo "         --track-origins=yes --suppressions=gst.supp \\" >> "$REPORT_FILE"
    echo "         bogdan-server 2>&1 | tee valgrind-report.txt" >> "$REPORT_FILE"
    echo "" >> "$REPORT_FILE"
    echo "    2. Check GStreamer element references:" >> "$REPORT_FILE"
    echo "       GST_DEBUG=GST_TRACER:7 bogdan-server" >> "$REPORT_FILE"
    echo "       (Look for buffer pool or element refcount leaks)" >> "$REPORT_FILE"
    echo "" >> "$REPORT_FILE"
    echo "    3. Profile heap allocation with heaptrack:" >> "$REPORT_FILE"
    echo "       heaptrack bogdan-server" >> "$REPORT_FILE"
    echo "" >> "$REPORT_FILE"
elif $FD_LEAK_DETECTED; then
    echo "  *** FILE DESCRIPTOR LEAK DETECTED ***" >> "$REPORT_FILE"
    echo "" >> "$REPORT_FILE"
    echo "  FD count grew by ${FD_TOTAL_GROWTH} (limit: ${FD_GROWTH_LIMIT})." >> "$REPORT_FILE"
    echo "  This suggests sockets, pipes, or files are not being closed properly." >> "$REPORT_FILE"
    echo "" >> "$REPORT_FILE"
    echo "  Recommended next steps:" >> "$REPORT_FILE"
    echo "    1. List open FDs for the process:" >> "$REPORT_FILE"
    echo "       ls -la /proc/\$(pgrep bogdan)/fd" >> "$REPORT_FILE"
    echo "    2. Check for unclosed sockets:" >> "$REPORT_FILE"
    echo "       ls -la /proc/\$(pgrep bogdan)/fd | grep socket" >> "$REPORT_FILE"
    echo "    3. Run under Valgrind with --track-fds=yes:" >> "$REPORT_FILE"
    echo "       valgrind --track-fds=yes bogdan-server" >> "$REPORT_FILE"
    echo "" >> "$REPORT_FILE"
else
    echo "  PASS — No memory leak detected." >> "$REPORT_FILE"
    echo "  RSS growth rate (${RSS_GROWTH_RATE} MB/hour) is within threshold." >> "$REPORT_FILE"
    echo "  FD growth (${FD_TOTAL_GROWTH}) is within limit." >> "$REPORT_FILE"
    echo "" >> "$REPORT_FILE"
fi

echo "─── Raw Data ────────────────────────────────────────────────" >> "$REPORT_FILE"
echo "" >> "$REPORT_FILE"
echo "  CSV file: ${CSV_FILE}" >> "$REPORT_FILE"
echo "" >> "$REPORT_FILE"
echo "═══════════════════════════════════════════════════════════════" >> "$REPORT_FILE"

# Print the report to console
echo ""
cat "$REPORT_FILE"

# ── Exit code ──────────────────────────────────────────────────────────

if $LEAK_DETECTED || $FD_LEAK_DETECTED; then
    exit 1
else
    exit 0
fi
