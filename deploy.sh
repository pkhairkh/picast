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
BINARY_NAME="picast"
# The systemd service references /usr/local/bin/picast-server,
# so we install under that name regardless of the Cargo bin name.
INSTALL_AS="picast-server"
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
    echo "[1/4] Building release binary with hw feature..."

    # Ensure rustc meets the MSRV (1.88).  Several transitive dependencies
    # (time 0.3.46+, cookie_store 0.22+, idna 1.x / ICU4X 2.x) require
    # rustc 1.88 or newer.  If rustup is available we install the
    # minimum required toolchain automatically.
    MIN_RUSTC_MAJOR=1
    MIN_RUSTC_MINOR=88
    RUSTC_VER=$(rustc --version 2>/dev/null | sed -n 's/rustc \([0-9]*\)\.\([0-9]*\).*/\1 \2/p')
    if [[ -z "$RUSTC_VER" ]]; then
        echo "      ERROR: rustc not found.  Install via https://rustup.rs"
        exit 1
    fi
    RUSTC_MAJOR=$(echo "$RUSTC_VER" | awk '{print $1}')
    RUSTC_MINOR=$(echo "$RUSTC_VER" | awk '{print $2}')
    if [[ "$RUSTC_MAJOR" -lt "$MIN_RUSTC_MAJOR" ]] || \
       [[ "$RUSTC_MAJOR" -eq "$MIN_RUSTC_MAJOR" && "$RUSTC_MINOR" -lt "$MIN_RUSTC_MINOR" ]]; then
        echo "      rustc $RUSTC_MAJOR.$RUSTC_MINOR is too old (need >= $MIN_RUSTC_MAJOR.$MIN_RUSTC_MINOR)"
        if command -v rustup &>/dev/null; then
            echo "      Installing rustc $MIN_RUSTC_MAJOR.$MIN_RUSTC_MINOR.0 via rustup …"
            rustup install "$MIN_RUSTC_MAJOR.$MIN_RUSTC_MINOR.0"
            rustup default "$MIN_RUSTC_MAJOR.$MIN_RUSTC_MINOR.0"
            echo "      Now using $(rustc --version)"
        else
            echo "      ERROR: Please upgrade rustc manually (https://rustup.rs)"
            exit 1
        fi
    fi

    (cd "$REPO_DIR" && cargo build --release --features hw)
    echo "      Build complete."
else
    echo "[1/4] Skipping build (--no-build)."
fi

BINARY_SRC="$REPO_DIR/target/release/$BINARY_NAME"
if [[ ! -f "$BINARY_SRC" ]]; then
    echo "ERROR: Binary not found at $BINARY_SRC"
    echo "       Run without --no-build, or build manually first."
    exit 1
fi

# ── Install binary ────────────────────────────────────────────────────
echo "[2/4] Installing binary to $INSTALL_DIR/..."
sudo cp "$BINARY_SRC" "$INSTALL_DIR/$INSTALL_AS"
sudo chmod 755 "$INSTALL_DIR/$INSTALL_AS"
echo "      Installed $INSTALL_AS $(stat -c %s "$INSTALL_DIR/$INSTALL_AS" 2>/dev/null || stat -f %z "$INSTALL_DIR/$INSTALL_AS") bytes"

# ── Install service ───────────────────────────────────────────────────
echo "[3/4] Installing systemd service..."
sudo cp "$SERVICE_SRC" "$SERVICE_DST"
sudo systemctl daemon-reload
echo "      Service unit installed and daemon reloaded."

# ── Restart service ───────────────────────────────────────────────────
echo "[4/4] Restarting picast service..."
sudo systemctl restart picast
sleep 1
sudo systemctl --no-pager status picast || true
echo ""
echo "=== Deploy complete ==="
echo "  Binary:  $INSTALL_DIR/$INSTALL_AS"
echo "  Service: $SERVICE_DST"
echo "  Status:  systemctl status picast"
echo "  Logs:    journalctl -u picast -f"
