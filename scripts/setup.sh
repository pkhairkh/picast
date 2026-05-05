#!/usr/bin/env bash
# PiCast Setup Script — Raspberry Pi OS Lite 64-bit (bookworm)
# Run as: sudo bash scripts/setup.sh
set -euo pipefail

echo "=== PiCast Setup ==="

# ─── 1. System Update ────────────────────────────────────────
echo "[1/8] Updating system packages..."
apt update && apt upgrade -y

# ─── 2. Install Dependencies ─────────────────────────────────
echo "[2/8] Installing dependencies..."
apt install -y \
    tor \
    gstreamer1.0-tools \
    gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly \
    gstreamer1.0-libav \
    gmediarender \
    yt-dlp \
    python3-pip \
    build-essential \
    pkg-config \
    libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev \
    libgstreamer-plugins-bad1.0-dev \
    libdrm-dev \
    libgbm-dev \
    libegl-dev \
    libgles2-dev \
    libsqlite3-dev \
    libssl-dev \
    iptables \
    dnsmasq \
    avahi-daemon \
    git \
    curl

# ─── 3. Install Rust ─────────────────────────────────────────
echo "[3/8] Installing Rust toolchain..."
if ! command -v cargo &>/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
fi

# ─── 4. Configure Kernel Overlays ────────────────────────────
echo "[4/8] Configuring kernel overlays..."
OVERLAYS_ALREADY_SET=$(grep -c "vc4-kms-v3d" /boot/config.txt || true)
if [ "$OVERLAYS_ALREADY_SET" -eq 0 ]; then
    cat >> /boot/config.txt << 'EOF'

# PiCast: Enable DRM/KMS with V3D GPU
dtoverlay=vc4-kms-v3d

# PiCast: Enable HEVC V4L2 decoder (for v2)
# dtoverlay=rpivid-v4l2

# PiCast: Disable WiFi and Bluetooth (reduce attack surface, save power)
# dtoverlay=disable-wifi
# dtoverlay=disable-bt
EOF
    echo "  Added vc4-kms-v3d overlay. Reboot required."
else
    echo "  vc4-kms-v3d overlay already configured."
fi

# ─── 5. Configure Tor ────────────────────────────────────────
echo "[5/8] Configuring Tor..."
cp config/torrc /etc/tor/torrc
systemctl restart tor
systemctl enable tor

# ─── 6. Configure Firewall ───────────────────────────────────
echo "[6/8] Configuring firewall..."
iptables-restore < config/iptables.rules
apt install -y iptables-persistent
netfilter-persistent save

# ─── 7. Build PiCast ─────────────────────────────────────────
echo "[7/8] Building PiCast..."
cargo build --release

# ─── 8. Install PiCast ───────────────────────────────────────
echo "[8/8] Installing PiCast service..."

# Create picast user if it doesn't exist
if ! id -u picast &>/dev/null; then
    useradd -r -m -s /usr/sbin/nologin picast
    usermod -aG video,render,audio picast
fi

# Install binary
cp target/release/picast /usr/local/bin/picast

# Create data directory
mkdir -p /var/lib/picast
chown picast:picast /var/lib/picast

# Create temp directory
mkdir -p /tmp/picast/subs
chown picast:picast /tmp/picast

# Install systemd service
cp config/picast.service /etc/systemd/system/picast.service
systemctl daemon-reload
systemctl enable picast

echo ""
echo "=== PiCast Setup Complete ==="
echo ""
echo "Next steps:"
echo "  1. Reboot to apply kernel overlay: sudo reboot"
echo "  2. Start PiCast: sudo systemctl start picast"
echo "  3. Check status: sudo systemctl status picast"
echo "  4. View logs: journalctl -u picast -f"
echo ""
echo "  Cast from browser: Install the extension from src/extension/"
echo "  Cast from VLC: Playback → Renderer → PiCast"
echo "  Cast via API: curl -X POST http://$(hostname -I | cut -d' ' -f1):8585/api/cast -H 'Content-Type: application/json' -d '{\"url\": \"https://www.youtube.com/watch?v=dQw4w9WgXcQ\"}'"
