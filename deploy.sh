#!/bin/bash
# PiCast deployment script
#
# Builds the release binary (with hw feature), copies it to
# /usr/local/bin, installs the systemd unit, and restarts the service.
#
# Usage:
#   ./deploy.sh           # build + deploy
#   ./deploy.sh --no-build # deploy only (skip cargo build)

set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")" && pwd)"
BINARY_NAME="picast-server"
INSTALL_DIR="/usr/local/bin"
SERVICE_SRC="$REPO_DIR/deploy/picast.service"
SERVICE_DST="/etc/systemd/system/picast.service"
CONFIG_DIR="/etc/picast"

SKIP_BUILD=false
if [[ "${1:-}" == "--no-build" ]]; then
    SKIP_BUILD=true
fi

echo "=== PiCast Deploy ==="
echo ""

# ── Build ──────────────────────────────────────────────────────────────
if [[ "$SKIP_BUILD" == false ]]; then
    echo "[1/5] Building release binary with hw feature..."
    (cd "$REPO_DIR" && cargo build --release --features hw)
    echo "      Build complete."
else
    echo "[1/5] Skipping build (--no-build)."
fi

BINARY_SRC="$REPO_DIR/target/release/$BINARY_NAME"
if [[ ! -f "$BINARY_SRC" ]]; then
    echo "ERROR: Binary not found at $BINARY_SRC"
    echo "       Run without --no-build, or build manually first."
    exit 1
fi

# ── Install binary ────────────────────────────────────────────────────
echo "[2/5] Installing binary to $INSTALL_DIR/..."
sudo cp "$BINARY_SRC" "$INSTALL_DIR/$BINARY_NAME"
sudo chmod 755 "$INSTALL_DIR/$BINARY_NAME"
echo "      Installed $BINARY_NAME $(stat -c %s "$INSTALL_DIR/$BINARY_NAME" 2>/dev/null || stat -f %z "$INSTALL_DIR/$BINARY_NAME") bytes"

# ── Install service ───────────────────────────────────────────────────
echo "[3/5] Installing systemd service..."
sudo cp "$SERVICE_SRC" "$SERVICE_DST"
sudo systemctl daemon-reload
echo "      Service unit installed and daemon reloaded."

# ── Ensure config directory exists ────────────────────────────────────
if [[ ! -d "$CONFIG_DIR" ]]; then
    echo "[4/5] Creating config directory $CONFIG_DIR..."
    sudo mkdir -p "$CONFIG_DIR"
fi
if [[ ! -f "$CONFIG_DIR/picast.toml" ]]; then
    echo "      WARNING: No config file at $CONFIG_DIR/picast.toml"
    echo "               Copy config/picast.toml there before starting."
fi

# ── Restart service ───────────────────────────────────────────────────
echo "[5/5] Restarting picast service..."
sudo systemctl restart picast
sleep 1
sudo systemctl --no-pager status picast || true
echo ""
echo "=== Deploy complete ==="
echo "  Binary:  $INSTALL_DIR/$BINARY_NAME"
echo "  Service: $SERVICE_DST"
echo "  Status:  systemctl status picast"
echo "  Logs:    journalctl -u picast -f"
