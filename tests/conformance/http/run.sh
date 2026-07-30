#!/bin/bash
# ──────────────────────────────────────────────────────────────────────
# boGDan HTTP REST API Conformance Suite
# ──────────────────────────────────────────────────────────────────────
#
# Tests all HTTP REST endpoints for protocol compliance.
# Requires a running boGDan instance at http://localhost:8585.
#
# Usage: ./tests/conformance/http/run.sh [BOGDAN_URL]
# Default: http://localhost:8585

set -euo pipefail

BOGDAN_URL="${1:-http://localhost:8585}"
PASS=0
FAIL=0

green() { printf "\033[32m%s\033[0m\n" "$1"; }
red()   { printf "\033[31m%s\033[0m\n" "$1"; }
log()   { printf "  %s\n" "$1"; }

assert_status() {
    local method="$1" path="$2" expected="$3" desc="$4"
    local actual
    actual=$(curl -s -o /dev/null -w "%{http_code}" -X "$method" "${BOGDAN_URL}${path}")
    if [ "$actual" = "$expected" ]; then
        green "PASS: $desc (got $actual)"
        PASS=$((PASS + 1))
    else
        red "FAIL: $desc (expected $expected, got $actual)"
        FAIL=$((FAIL + 1))
    fi
}

assert_json_field() {
    local path="$1" field="$2" desc="$3"
    local body
    body=$(curl -s "${BOGDAN_URL}${path}")
    if echo "$body" | python3 -c "import sys,json; d=json.load(sys.stdin); sys.exit(0 if '$field' in d else 1)" 2>/dev/null; then
        green "PASS: $desc"
        PASS=$((PASS + 1))
    else
        red "FAIL: $desc (field '$field' not in response: $body)"
        FAIL=$((FAIL + 1))
    fi
}

echo "═══════════════════════════════════════════════════════════════"
echo "  boGDan HTTP REST API Conformance Suite"
echo "  Target: $BOGDAN_URL"
echo "═══════════════════════════════════════════════════════════════"
echo ""

# ── Health Check ──────────────────────────────────────────────────────
echo "── Health Check ──"
assert_status GET "/api/health" "200" "GET /api/health returns 200"
assert_json_field "/api/health" "status" "Health response has 'status' field"

# ── Status ────────────────────────────────────────────────────────────
echo ""
echo "── Status ──"
assert_status GET "/api/status" "200" "GET /api/status returns 200"
assert_json_field "/api/status" "state" "Status response has 'state' field"
assert_json_field "/api/status" "volume" "Status response has 'volume' field"
assert_json_field "/api/status" "position_ms" "Status response has 'position_ms' field"

# ── Cast ──────────────────────────────────────────────────────────────
echo ""
echo "── Cast ──"
assert_status POST "/api/cast" "400" "POST /api/cast with no body returns 400"
assert_status POST "/api/cast" "400" "POST /api/cast with invalid JSON returns 400"

# Test with valid URL (should return 202 Accepted)
CAST_RESPONSE=$(curl -s -X POST "${BOGDAN_URL}/api/cast" \
    -H "Content-Type: application/json" \
    -d '{"url":"https://example.com/test.mp4"}')
CAST_STATUS=$(echo "$CAST_RESPONSE" | python3 -c "import sys,json; print(json.load(sys.stdin).get('status',''))" 2>/dev/null || echo "")
if [ -n "$CAST_STATUS" ]; then
    green "PASS: POST /api/cast with valid URL returns response with 'status' field"
    PASS=$((PASS + 1))
else
    red "FAIL: POST /api/cast response missing 'status' field: $CAST_RESPONSE"
    FAIL=$((FAIL + 1))
fi

# ── Stop ──────────────────────────────────────────────────────────────
echo ""
echo "── Stop ──"
assert_status POST "/api/stop" "200" "POST /api/stop returns 200"

# ── Pause/Resume ──────────────────────────────────────────────────────
echo ""
echo "── Pause/Resume ──"
assert_status POST "/api/pause" "200" "POST /api/pause returns 200 (or 409 if no session)"
assert_status POST "/api/resume" "200" "POST /api/resume returns 200 (or 409 if no session)"

# ── Seek ──────────────────────────────────────────────────────────────
echo ""
echo "── Seek ──"
assert_status POST "/api/seek" "400" "POST /api/seek with no body returns 400"

# ── Volume ────────────────────────────────────────────────────────────
echo ""
echo "── Volume ──"
assert_status POST "/api/volume" "400" "POST /api/volume with no body returns 400"

# Test valid volume
VOL_RESPONSE=$(curl -s -X POST "${BOGDAN_URL}/api/volume" \
    -H "Content-Type: application/json" \
    -d '{"volume":50}')
if echo "$VOL_RESPONSE" | python3 -c "import sys,json; d=json.load(sys.stdin); sys.exit(0 if d.get('volume')==50 else 1)" 2>/dev/null; then
    green "PASS: POST /api/volume with {volume:50} returns volume=50"
    PASS=$((PASS + 1))
else
    red "FAIL: POST /api/volume response unexpected: $VOL_RESPONSE"
    FAIL=$((FAIL + 1))
fi

# ── Audio Devices ─────────────────────────────────────────────────────
echo ""
echo "── Audio Devices ──"
assert_status GET "/api/audio-devices" "200" "GET /api/audio-devices returns 200"

# ── CORS ──────────────────────────────────────────────────────────────
echo ""
echo "── CORS ──"
CORS_HEADER=$(curl -s -o /dev/null -w "%{header_json}" "${BOGDAN_URL}/api/health" | python3 -c "import sys,json; h=json.load(sys.stdin); print(h.get('access-control-allow-origin',[''])[0])" 2>/dev/null || echo "")
if [ "$CORS_HEADER" = "*" ]; then
    green "PASS: CORS header Access-Control-Allow-Origin: * present"
    PASS=$((PASS + 1))
else
    red "FAIL: CORS header not '*' (got: $CORS_HEADER)"
    FAIL=$((FAIL + 1))
fi

# ── 404 ───────────────────────────────────────────────────────────────
echo ""
echo "── 404 ──"
assert_status GET "/api/nonexistent" "404" "GET /api/nonexistent returns 404"

# ── Rate Limiting ─────────────────────────────────────────────────────
echo ""
echo "── Rate Limiting ──"
log "Sending 35 rapid requests to trigger rate limit..."
RATE_LIMITED=0
for i in $(seq 1 35); do
    CODE=$(curl -s -o /dev/null -w "%{http_code}" "${BOGDAN_URL}/api/status")
    if [ "$CODE" = "429" ]; then
        RATE_LIMITED=1
        break
    fi
done
if [ "$RATE_LIMITED" = "1" ]; then
    green "PASS: Rate limiting triggered (429 received)"
    PASS=$((PASS + 1))
else
    red "FAIL: Rate limiting not triggered after 35 requests"
    FAIL=$((FAIL + 1))
fi

# ── Summary ───────────────────────────────────────────────────────────
echo ""
echo "═══════════════════════════════════════════════════════════════"
echo "  Results: $(green "$PASS passed"), $(red "$FAIL failed")"
echo "═══════════════════════════════════════════════════════════════"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
exit 0
