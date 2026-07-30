#!/bin/bash
# ──────────────────────────────────────────────────────────────────────
# boGDan DLNA/UPnP MediaRenderer Conformance Suite
# ──────────────────────────────────────────────────────────────────────
#
# Tests DLNA MediaRenderer discovery and control.
# Requires a running boGDan instance with DLNA enabled (port 49152).
#
# Usage: ./tests/conformance/dlna/run.sh
#
# Prerequisites: gupnp-tools (gssdp-discover, gupnp-universal-cp)

set -euo pipefail

BOGDAN_HOST="${BOGDAN_HOST:-localhost}"
BOGDAN_DLNA_PORT="${BOGDAN_DLNA_PORT:-49152}"
PASS=0
FAIL=0

green() { printf "\033[32m%s\033[0m\n" "$1"; }
red()   { printf "\033[31m%s\033[0m\n" "$1"; }
log()   { printf "  %s\n" "$1"; }

echo "═══════════════════════════════════════════════════════════════"
echo "  boGDan DLNA MediaRenderer Conformance Suite"
echo "  Target: ${BOGDAN_HOST}:${BOGDAN_DLNA_PORT}"
echo "═══════════════════════════════════════════════════════════════"
echo ""

# ── SSDP Discovery ────────────────────────────────────────────────────
echo "── SSDP Discovery ──"
if command -v gssdp-discover &>/dev/null; then
    DISCOVERY=$(gssdp-discover --target=urn:schemas-upnp-org:device:MediaRenderer:1 --timeout=5 2>/dev/null || echo "")
    if echo "$DISCOVERY" | grep -qi "boGDan\|bogdan"; then
        green "PASS: boGDan discovered via SSDP as MediaRenderer"
        PASS=$((PASS + 1))
    else
        red "FAIL: boGDan not discovered via SSDP"
        FAIL=$((FAIL + 1))
    fi
else
    log "gssdp-discover not available — skipping SSDP discovery test"
    log "Install with: apt install gupnp-tools"
fi

# ── Device Description ────────────────────────────────────────────────
echo ""
echo "── Device Description ──"
DESC_URL="http://${BOGDAN_HOST}:${BOGDAN_DLNA_PORT}/rootDesc.xml"
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" "$DESC_URL" 2>/dev/null || echo "000")
if [ "$HTTP_CODE" = "200" ]; then
    green "PASS: Device description accessible at /rootDesc.xml (200)"
    PASS=$((PASS + 1))

    # Check for MediaRenderer device type
    DESC_BODY=$(curl -s "$DESC_URL" 2>/dev/null || echo "")
    if echo "$DESC_BODY" | grep -qi "MediaRenderer"; then
        green "PASS: Device type is MediaRenderer"
        PASS=$((PASS + 1))
    else
        red "FAIL: Device type not MediaRenderer"
        FAIL=$((FAIL + 1))
    fi

    # Check for AVTransport service
    if echo "$DESC_BODY" | grep -qi "AVTransport"; then
        green "PASS: AVTransport service advertised"
        PASS=$((PASS + 1))
    else
        red "FAIL: AVTransport service not found in device description"
        FAIL=$((FAIL + 1))
    fi

    # Check for RenderingControl service
    if echo "$DESC_BODY" | grep -qi "RenderingControl"; then
        green "PASS: RenderingControl service advertised"
        PASS=$((PASS + 1))
    else
        red "FAIL: RenderingControl service not found in device description"
        FAIL=$((FAIL + 1))
    fi
else
    red "FAIL: Device description not accessible (HTTP $HTTP_CODE)"
    FAIL=$((FAIL + 1))
fi

# ── AVTransport: GetTransportInfo ─────────────────────────────────────
echo ""
echo "── AVTransport: GetTransportInfo ──"
SOAP_BODY='<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:GetTransportInfo xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
      <InstanceID>0</InstanceID>
    </u:GetTransportInfo>
  </s:Body>
</s:Envelope>'

SOAP_RESPONSE=$(curl -s -X POST "http://${BOGDAN_HOST}:${BOGDAN_DLNA_PORT}/ctl/AVTransport" \
    -H "Content-Type: text/xml; charset=utf-8" \
    -H "SOAPAction: \"urn:schemas-upnp-org:service:AVTransport:1#GetTransportInfo\"" \
    -d "$SOAP_BODY" 2>/dev/null || echo "")

if echo "$SOAP_RESPONSE" | grep -qi "CurrentTransportState"; then
    green "PASS: GetTransportInfo returns CurrentTransportState"
    PASS=$((PASS + 1))
else
    red "FAIL: GetTransportInfo did not return CurrentTransportState"
    FAIL=$((FAIL + 1))
fi

# ── RenderingControl: GetVolume ───────────────────────────────────────
echo ""
echo "── RenderingControl: GetVolume ──"
SOAP_BODY='<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:GetVolume xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1">
      <InstanceID>0</InstanceID>
      <Channel>Master</Channel>
    </u:GetVolume>
  </s:Body>
</s:Envelope>'

SOAP_RESPONSE=$(curl -s -X POST "http://${BOGDAN_HOST}:${BOGDAN_DLNA_PORT}/ctl/RenderingControl" \
    -H "Content-Type: text/xml; charset=utf-8" \
    -H "SOAPAction: \"urn:schemas-upnp-org:service:RenderingControl:1#GetVolume\"" \
    -d "$SOAP_BODY" 2>/dev/null || echo "")

if echo "$SOAP_RESPONSE" | grep -qi "CurrentVolume"; then
    green "PASS: GetVolume returns CurrentVolume"
    PASS=$((PASS + 1))
else
    red "FAIL: GetVolume did not return CurrentVolume"
    FAIL=$((FAIL + 1))
fi

# ── SetAVTransportURI ─────────────────────────────────────────────────
echo ""
echo "── SetAVTransportURI ──"
SOAP_BODY='<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
      <InstanceID>0</InstanceID>
      <CurrentURI>https://example.com/test.mp4</CurrentURI>
      <CurrentURIMetaData></CurrentURIMetaData>
    </u:SetAVTransportURI>
  </s:Body>
</s:Envelope>'

SOAP_RESPONSE=$(curl -s -X POST "http://${BOGDAN_HOST}:${BOGDAN_DLNA_PORT}/ctl/AVTransport" \
    -H "Content-Type: text/xml; charset=utf-8" \
    -H "SOAPAction: \"urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI\"" \
    -d "$SOAP_BODY" 2>/dev/null || echo "")

if echo "$SOAP_RESPONSE" | grep -qi "SetAVTransportURIResponse"; then
    green "PASS: SetAVTransportURI accepted"
    PASS=$((PASS + 1))
else
    red "FAIL: SetAVTransportURI not accepted"
    FAIL=$((FAIL + 1))
fi

# ── Stop ──────────────────────────────────────────────────────────────
echo ""
echo "── Stop ──"
SOAP_BODY='<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:Stop xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
      <InstanceID>0</InstanceID>
    </u:Stop>
  </s:Body>
</s:Envelope>'

SOAP_RESPONSE=$(curl -s -X POST "http://${BOGDAN_HOST}:${BOGDAN_DLNA_PORT}/ctl/AVTransport" \
    -H "Content-Type: text/xml; charset=utf-8" \
    -H "SOAPAction: \"urn:schemas-upnp-org:service:AVTransport:1#Stop\"" \
    -d "$SOAP_BODY" 2>/dev/null || echo "")

if echo "$SOAP_RESPONSE" | grep -qi "StopResponse"; then
    green "PASS: Stop accepted"
    PASS=$((PASS + 1))
else
    red "FAIL: Stop not accepted"
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
