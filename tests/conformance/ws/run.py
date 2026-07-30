#!/usr/bin/env python3
"""
boGDan WebSocket API Conformance Suite

Tests the WebSocket event streaming API for protocol compliance.
Requires a running boGDan instance at ws://localhost:8586.

Usage: python3 tests/conformance/ws/run.py [WS_URL]
Default: ws://localhost:8586

Requires: pip install websockets
"""

import asyncio
import json
import sys
import time
from typing import Optional

try:
    import websockets
except ImportError:
    print("ERROR: websockets package not installed. Run: pip install websockets")
    sys.exit(2)

WS_URL = sys.argv[1] if len(sys.argv) > 1 else "ws://localhost:8586"

PASS = 0
FAIL = 0


def green(msg):
    print(f"\033[32m{msg}\033[0m")


def red(msg):
    print(f"\033[31m{msg}\033[0m")


def assert_eq(actual, expected, desc):
    global PASS, FAIL
    if actual == expected:
        green(f"PASS: {desc}")
        PASS += 1
    else:
        red(f"FAIL: {desc} (expected {expected!r}, got {actual!r})")
        FAIL += 1


def assert_in(item, collection, desc):
    global PASS, FAIL
    if item in collection:
        green(f"PASS: {desc}")
        PASS += 1
    else:
        red(f"FAIL: {desc} ({item!r} not in {collection!r})")
        FAIL += 1


async def connect_and_receive(ws_url: str, timeout: float = 5.0) -> Optional[dict]:
    """Connect to the WebSocket and return the first message."""
    try:
        async with websockets.connect(ws_url) as ws:
            msg = await asyncio.wait_for(ws.recv(), timeout=timeout)
            return json.loads(msg)
    except Exception as e:
        print(f"  Connection error: {e}")
        return None


async def send_command(ws_url: str, command: dict, timeout: float = 5.0) -> Optional[dict]:
    """Send a command and return the response (or next event)."""
    try:
        async with websockets.connect(ws_url) as ws:
            # Read the Connected event first
            await asyncio.wait_for(ws.recv(), timeout=timeout)
            # Send the command
            await ws.send(json.dumps(command))
            # Read the response
            msg = await asyncio.wait_for(ws.recv(), timeout=timeout)
            return json.loads(msg)
    except Exception as e:
        print(f"  Command error: {e}")
        return None


async def run_tests():
    global PASS, FAIL

    print("═" * 64)
    print("  boGDan WebSocket API Conformance Suite")
    print(f"  Target: {WS_URL}")
    print("═" * 64)
    print()

    # ── Connection Test ──────────────────────────────────────────────
    print("── Connection ──")
    result = await connect_and_receive(WS_URL)
    if result is not None:
        assert_in("type", result, "First message is JSON with 'type' field")
        if result.get("type") == "CONNECTED":
            green("PASS: First message is CONNECTED event")
            PASS += 1
        else:
            red(f"FAIL: Expected CONNECTED event, got {result.get('type')}")
            FAIL += 1
    else:
        red("FAIL: Could not connect to WebSocket")
        FAIL += 1
        return

    # ── STOP Command ─────────────────────────────────────────────────
    print()
    print("── STOP Command ──")
    result = await send_command(WS_URL, {"type": "STOP"})
    if result is not None:
        assert_in("type", result, "STOP response has 'type' field")
    else:
        red("FAIL: No response to STOP command")
        FAIL += 1

    # ── Invalid Command ──────────────────────────────────────────────
    print()
    print("── Invalid Command ──")
    result = await send_command(WS_URL, {"type": "INVALID_COMMAND"})
    if result is not None and result.get("type") == "ERROR":
        green("PASS: Invalid command returns ERROR event")
        PASS += 1
    else:
        red(f"FAIL: Invalid command should return ERROR, got {result}")
        FAIL += 1

    # ── Malformed JSON ───────────────────────────────────────────────
    print()
    print("── Malformed JSON ──")
    try:
        async with websockets.connect(WS_URL) as ws:
            await asyncio.wait_for(ws.recv(), timeout=5.0)  # Connected
            await ws.send("not valid json{{{")
            msg = await asyncio.wait_for(ws.recv(), timeout=5.0)
            result = json.loads(msg)
            if result.get("type") == "ERROR":
                green("PASS: Malformed JSON returns ERROR event")
                PASS += 1
            else:
                red(f"FAIL: Malformed JSON should return ERROR, got {result}")
                FAIL += 1
    except Exception as e:
        red(f"FAIL: Malformed JSON test error: {e}")
        FAIL += 1

    # ── VOLUME Command ───────────────────────────────────────────────
    print()
    print("── VOLUME Command ──")
    result = await send_command(WS_URL, {"type": "VOLUME", "volume": 50})
    if result is not None:
        assert_in("type", result, "VOLUME response has 'type' field")
    else:
        red("FAIL: No response to VOLUME command")
        FAIL += 1

    # ── PING/PONG ────────────────────────────────────────────────────
    print()
    print("── Application PING/PONG ──")
    result = await send_command(WS_URL, {"type": "PING"})
    if result is not None and result.get("type") == "PONG":
        green("PASS: PING command returns PONG event")
        PASS += 1
    else:
        red(f"FAIL: PING should return PONG, got {result}")
        FAIL += 1

    # ── CAST with invalid URL ────────────────────────────────────────
    print()
    print("── CAST with invalid URL ──")
    result = await send_command(WS_URL, {"type": "CAST", "url": "file:///etc/passwd"})
    if result is not None and result.get("type") == "ERROR":
        green("PASS: CAST with file:// URL returns ERROR")
        PASS += 1
    else:
        red(f"FAIL: CAST with file:// should return ERROR, got {result}")
        FAIL += 1

    # ── Summary ──────────────────────────────────────────────────────
    print()
    print("═" * 64)
    print(f"  Results: {PASS} passed, {FAIL} failed")
    print("═" * 64)

    return FAIL == 0


if __name__ == "__main__":
    success = asyncio.run(run_tests())
    sys.exit(0 if success else 1)
