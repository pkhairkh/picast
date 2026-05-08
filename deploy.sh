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
    echo "[1/4] Building release binary with hw + hevc features..."

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

    # Verify V3D GPU is available on the target (MANDATORY for HEVC decode).
    # The V3D GPU is present on Raspberry Pi 4B+ and is required for the
    # hevc feature (HEVC/H.265 hardware decode with V3D SAND→NV12 conversion).
    # HEVC with GPU is mandatory — deployment will abort if V3D is not found.
    #
    # Detection checks (in order of reliability):
    # 1. /dev/dri/by-path/platform-v3d — udev symlink (may not exist if udev rules missing)
    # 2. /sys/class/misc/v3d — misc device class (older kernels, pre-6.x)
    # 3. /dev/dri/renderD128 — V3D render node (used by picast-v3d crate internally)
    # 4. lsmod v3d — kernel module loaded but no device node yet
    # 5. /sys/class/drm/card*/device/driver → v3d — DRI card backed by v3d driver
    V3D_FOUND=false

    # Check 1: udev by-path symlink
    if [[ -e /dev/dri/by-path/platform-v3d ]]; then
        echo "      V3D GPU: udev by-path device found (/dev/dri/by-path/platform-v3d)"
        V3D_FOUND=true
    # Check 2: misc device class (older kernels)
    elif [[ -d /sys/class/misc/v3d ]]; then
        echo "      V3D GPU: misc device class found (/sys/class/misc/v3d)"
        V3D_FOUND=true
    # Check 3: render node (used by V3D compute engine via EGL)
    elif [[ -c /dev/dri/renderD128 ]]; then
        # Verify it's actually V3D and not some other GPU
        if [[ -e /sys/class/drm/renderD128/device/driver ]]; then
            driver_link=$(readlink /sys/class/drm/renderD128/device/driver 2>/dev/null || true)
            if [[ "$driver_link" == *"/v3d" ]]; then
                echo "      V3D GPU: render node found (/dev/dri/renderD128, driver=v3d)"
                V3D_FOUND=true
            fi
        fi
        if [[ "$V3D_FOUND" == false ]]; then
            # renderD128 exists but might not be v3d — check other render nodes
            for render_node in /dev/dri/renderD*; do
                [[ -c "$render_node" ]] || continue
                base=$(basename "$render_node")
                if [[ -e "/sys/class/drm/${base}/device/driver" ]]; then
                    driver_link=$(readlink "/sys/class/drm/${base}/device/driver" 2>/dev/null || true)
                    if [[ "$driver_link" == *"/v3d" ]]; then
                        echo "      V3D GPU: render node found ($render_node, driver=v3d)"
                        V3D_FOUND=true
                        break
                    fi
                fi
            done
        fi
    fi

    # Check 4: kernel module loaded (even if no device node yet)
    if [[ "$V3D_FOUND" == false ]] && lsmod 2>/dev/null | grep -q '^v3d'; then
        echo "      V3D GPU: kernel module loaded (lsmod)"
        V3D_FOUND=true
    fi

    # Check 5: any DRI card backed by v3d driver
    if [[ "$V3D_FOUND" == false ]]; then
        for card in /sys/class/drm/card*/device/driver; do
            [[ -e "$card" ]] || continue
            driver_link=$(readlink "$card" 2>/dev/null || true)
            if [[ "$driver_link" == *"/v3d" ]]; then
                echo "      V3D GPU: DRI card found with v3d driver ($card)"
                V3D_FOUND=true
                break
            fi
        done
    fi

    # Check 6: device tree (vc4-kms-v3d overlay enabled)
    if [[ "$V3D_FOUND" == false ]] && [[ -d /proc/device-tree/v3d ]]; then
        echo "      V3D GPU: device tree node found (/proc/device-tree/v3d)"
        V3D_FOUND=true
    fi

    if [[ "$V3D_FOUND" == false ]]; then
        echo "      ERROR: V3D GPU not detected. HEVC/H.265 hardware decode is MANDATORY"
        echo "      and requires the V3D GPU found on Raspberry Pi 4B+."
        echo ""
        echo "      Troubleshooting:"
        echo "        1. Ensure 'dtoverlay=vc4-kms-v3d' is in /boot/config.txt (or /boot/firmware/config.txt)"
        echo "        2. Run 'lsmod | grep v3d' to check if the kernel module is loaded"
        echo "        3. Run 'dmesg | grep -i v3d' to check for V3D initialization messages"
        echo "        4. Reboot after adding the dtoverlay"
        echo ""
        echo "      Deployment aborted."
        exit 1
    fi

    # ── HEVC decoder prerequisites ──────────────────────────────────────
    # The v4l2slh265dec GStreamer element requires the rpivid V4L2 driver
    # to be loaded. On Raspberry Pi OS, this requires uncommenting or
    # adding 'dtoverlay=rpivid-v4l2' in /boot/config.txt (or
    # /boot/firmware/config.txt on some Pi OS versions).
    echo "      Checking HEVC decoder prerequisites..."

    if gst-inspect-1.0 v4l2slh265dec &>/dev/null; then
        echo "      v4l2slh265dec: available"
    elif gst-inspect-1.0 v4l2h265dec &>/dev/null; then
        echo "      v4l2h265dec: available (stateful)"
    else
        echo "      WARNING: No HEVC GStreamer decoder element found."
        echo "      HEVC streams will fall back to H.264 hardware decode."
        echo ""
        echo "      To enable HEVC hardware decode on Raspberry Pi 4:"
        echo "        1. Edit /boot/config.txt (or /boot/firmware/config.txt)"
        echo "        2. Uncomment or add:  dtoverlay=rpivid-v4l2"
        echo "        3. Reboot"
        echo "        4. Verify: gst-inspect-1.0 v4l2slh265dec"
    fi

    # ── EGL/GLES libraries for V3D compute ──────────────────────────
    # The V3D compute shader engine requires libEGL and libGLESv2 for
    # EGL context creation and OpenGL ES 3.1 compute shader dispatch.
    if [[ -f /usr/lib/aarch64-linux-gnu/libEGL.so.1 ]] || \
       [[ -f /usr/lib/arm-linux-gnueabihf/libEGL.so.1 ]]; then
        echo "      EGL/GLES: libraries found"
    else
        echo "      Installing EGL/GLES libraries for V3D compute engine..."
        sudo apt-get install -y libegl1-mesa-dev libgles2-mesa-dev 2>/dev/null || \
            sudo apt-get install -y libegl-dev libgles2-dev 2>/dev/null || \
            echo "      WARNING: Could not install EGL/GLES packages — V3D compute may not work"
    fi

    (cd "$REPO_DIR" && cargo build --release --features hw,hevc)
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
