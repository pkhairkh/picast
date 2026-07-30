# boGDan Conformance Test Suites

This directory contains protocol conformance tests for boGDan's three network-facing APIs:

- **HTTP REST API** (`http/`) — tests all REST endpoints via curl
- **WebSocket API** (`ws/`) — tests event streaming via wscat/python
- **DLNA/UPnP** (`dlna/`) — tests MediaRenderer discovery and control

## Running

Each suite is a standalone shell script that requires a running boGDan instance.

```bash
# HTTP conformance (requires boGDan on http://localhost:8585)
./tests/conformance/http/run.sh

# WebSocket conformance (requires boGDan on ws://localhost:8586)
./tests/conformance/ws/run.sh

# DLNA conformance (requires boGDan on port 49152, SSDP on 1900)
./tests/conformance/dlna/run.sh
```

## Prerequisites

- `curl` (HTTP suite)
- `python3` with `websockets` package (WS suite)
- `gupnp-tools` or `gmediarender` (DLNA suite)
- A running boGDan instance (see `scripts/setup.sh`)

## Test Results

Each script exits 0 on success, 1 on failure. Output is human-readable with PASS/FAIL markers.
