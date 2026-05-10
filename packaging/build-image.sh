#!/usr/bin/env bash
# ╔══════════════════════════════════════════════════════════════════╗
# ║  boGDan SD Card Image Builder                                   ║
# ║  Creates a pre-built Raspberry Pi OS Lite image with boGDan     ║
# ║  pre-installed. Flash-and-boot: insert SD card, power on,       ║
# ║  boGDan is running.                                             ║
# ╚══════════════════════════════════════════════════════════════════╝
set -euo pipefail

# ─── Configuration ──────────────────────────────────────────────────
BOGDAN_VERSION="0.1.0"
IMAGE_NAME="bogdan-${BOGDAN_VERSION}-pi4-arm64"
IMAGE_SIZE_MB=2048          # 2 GB image (will be shrunk later)
PI_OS_URL="https://downloads.raspberrypi.com/raspios_lite_arm64/images/raspios_lite_arm64-2024-07-04/2024-07-04-raspios-bookworm-arm64-lite.img.xz"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
BUILD_DIR="${SCRIPT_DIR}/build"
IMAGE_DIR="${BUILD_DIR}/${IMAGE_NAME}"
DEB_FILE=""                  # Will be set from args or auto-detected
OUTPUT_DIR="${BUILD_DIR}"

# ─── Colors ─────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log()  { echo -e "${GREEN}[IMAGE]${NC} $*"; }
err()  { echo -e "${RED}[ERROR]${NC} $*" >&2; exit 1; }
info() { echo -e "${BLUE}[INFO]${NC} $*"; }
warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }

# ─── Usage ──────────────────────────────────────────────────────────
usage() {
    cat <<EOF
boGDan SD Card Image Builder v${BOGDAN_VERSION}

Usage: $(basename "$0") [OPTIONS]

Options:
  --deb <path>       Path to pre-built .deb file (required if not building)
  --skip-deb-build   Skip building the .deb (use existing)
  --output <dir>     Output directory (default: packaging/build/)
  --compress         Compress output with xz (slower but smaller)
  --shrink           Shrink filesystem to minimum size (requires pi-gen tools)
  -h, --help         Show this help message

Examples:
  # Full build: deb + image
  sudo bash packaging/build-image.sh

  # Use pre-built deb
  sudo bash packaging/build-image.sh --deb packaging/build/bogdan_0.1.0_arm64.deb

  # Build and compress
  sudo bash packaging/build-image.sh --compress
EOF
    exit 0
}

# ─── Argument Parsing ───────────────────────────────────────────────
SKIP_DEB_BUILD=false
COMPRESS=false
SHRINK=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --deb)          DEB_FILE="$2"; shift 2 ;;
        --skip-deb-build) SKIP_DEB_BUILD=true; shift ;;
        --output)       OUTPUT_DIR="$2"; shift 2 ;;
        --compress)     COMPRESS=true; shift ;;
        --shrink)       SHRINK=true; shift ;;
        -h|--help)      usage ;;
        *)              err "Unknown option: $1" ;;
    esac
done

# ─── Pre-flight Checks ─────────────────────────────────────────────
info "boGDan SD Card Image Builder v${BOGDAN_VERSION}"

if [ "$(id -u)" -ne 0 ]; then
    err "This script must be run as root (uses losetup, mount, chroot)"
fi

for cmd in losetup mount chroot parted mkfs.ext4; do
    if ! command -v "$cmd" &>/dev/null; then
        err "Required command not found: $cmd"
    fi
done

# ─── Step 1: Build or locate the .deb package ──────────────────────
if [ -z "$DEB_FILE" ]; then
    if [ "$SKIP_DEB_BUILD" = true ]; then
        # Try to find existing deb
        for f in "${BUILD_DIR}"/bogdan_*_arm64.deb; do
            if [ -f "$f" ]; then
                DEB_FILE="$f"
                break
            fi
        done
        if [ -z "$DEB_FILE" ]; then
            err "No .deb file found. Build with: bash packaging/build-deb.sh"
        fi
    else
        log "Building .deb package..."
        bash "${SCRIPT_DIR}/build-deb.sh"
        DEB_FILE="${BUILD_DIR}/bogdan_${BOGDAN_VERSION}_arm64.deb"
    fi
fi

if [ ! -f "$DEB_FILE" ]; then
    err "Deb file not found: ${DEB_FILE}"
fi
log "Using deb: ${DEB_FILE}"

# ─── Step 2: Download Pi OS Lite base image ────────────────────────
PI_OS_IMG="${BUILD_DIR}/pi-os-lite.img"
PI_OS_XZ="${BUILD_DIR}/pi-os-lite.img.xz"

if [ ! -f "$PI_OS_IMG" ]; then
    log "Downloading Raspberry Pi OS Lite..."
    mkdir -p "$BUILD_DIR"
    if [ ! -f "$PI_OS_XZ" ]; then
        curl -L -o "$PI_OS_XZ" "$PI_OS_URL"
    fi
    log "Decompressing base image..."
    xzcat "$PI_OS_XZ" > "$PI_OS_IMG"
else
    info "Base image already exists: ${PI_OS_IMG}"
fi

# ─── Step 3: Create working copy ───────────────────────────────────
WORK_IMG="${BUILD_DIR}/${IMAGE_NAME}.img"
log "Creating working copy of base image..."
cp "$PI_OS_IMG" "$WORK_IMG"

# ─── Step 4: Mount the image and customize ─────────────────────────
log "Mounting image for customization..."

# Find the root partition offset
PART_INFO=$(fdisk -l "$WORK_IMG" | grep -A1 "Device" | tail -1)
PART_START_SECTORS=$(echo "$PART_INFO" | awk '{print $2}')
PART_START_BYTES=$((PART_START_SECTORS * 512))

# Set up loop device
LOOP_DEV=$(losetup --show -f -o "$PART_START_BYTES" "$WORK_IMG")
info "Loop device: ${LOOP_DEV}"

# Mount
MOUNT_POINT="${BUILD_DIR}/mnt"
mkdir -p "$MOUNT_POINT"
mount "$LOOP_DEV" "$MOUNT_POINT"

# Also mount boot partition
BOOT_PART_INFO=$(fdisk -l "$WORK_IMG" | grep -A1 "Device" | head -2 | tail -1)
BOOT_START_SECTORS=$(echo "$BOOT_PART_INFO" | awk '{print $2}')
BOOT_START_BYTES=$((BOOT_START_SECTORS * 512))

BOOT_LOOP_DEV=$(losetup --show -f -o "$BOOT_START_BYTES" "$WORK_IMG")
mkdir -p "${MOUNT_POINT}/boot"
mount "$BOOT_LOOP_DEV" "${MOUNT_POINT}/boot"

log "Image mounted at ${MOUNT_POINT}"

# ─── Step 5: Customize the image ───────────────────────────────────
log "Customizing image..."

# Enable SSH (create ssh file in boot)
touch "${MOUNT_POINT}/boot/ssh"
info "  SSH enabled"

# Configure kernel overlays
if ! grep -q "vc4-kms-v3d" "${MOUNT_POINT}/boot/config.txt" 2>/dev/null; then
    cat >> "${MOUNT_POINT}/boot/config.txt" << 'EOF'

# boGDan: Enable DRM/KMS with V3D GPU
dtoverlay=vc4-kms-v3d

# boGDan: Disable WiFi and Bluetooth (reduce attack surface)
dtoverlay=disable-wifi
dtoverlay=disable-bt

# boGDan: GPU memory allocation for video decode
gpu_mem=256

# boGDan: Enable 4Kp60 HDMI (for future HEVC support)
hdmi_enable_4kp60=1
EOF
    info "  Kernel overlays configured"
fi

# Set hostname
echo "bogdan" > "${MOUNT_POINT}/etc/hostname"
sed -i 's/127.0.1.1.*/127.0.1.1\tbogdan/' "${MOUNT_POINT}/etc/hosts"
info "  Hostname set to 'bogdan'"

# ─── Step 6: Install boGDan via chroot ─────────────────────────────
log "Installing boGDan in chroot..."

# Copy the deb into the image
cp "$DEB_FILE" "${MOUNT_POINT}/tmp/bogdan.deb"
info "  Deb copied to /tmp/bogdan.deb"

# Mount necessary filesystems for chroot
mount --bind /dev "${MOUNT_POINT}/dev"
mount --bind /dev/pts "${MOUNT_POINT}/dev/pts"
mount --bind /proc "${MOUNT_POINT}/proc"
mount --bind /sys "${MOUNT_POINT}/sys"

# Install boGDan inside the chroot
chroot "$MOUNT_POINT" /bin/bash -c "
    set -e
    export DEBIAN_FRONTEND=noninteractive

    # Update package lists
    apt-get update -y

    # Install the boGDan deb (this also installs dependencies)
    dpkg -i /tmp/bogdan.deb || apt-get install -f -y

    # Configure iptables-persistent non-interactively
    echo 'iptables-persistent iptables-persistent/autosave_v4 boolean true' | debconf-set-selections
    echo 'iptables-persistent iptables-persistent/autosave_v6 boolean true' | debconf-set-selections

    # Enable boGDan service
    systemctl enable bogdan

    # Start Tor
    systemctl enable tor

    # Clean up
    rm -f /tmp/bogdan.deb
    apt-get clean
    rm -rf /var/lib/apt/lists/*
"
info "  boGDan installed and services enabled"

# Unmount chroot filesystems
umount "${MOUNT_POINT}/sys" 2>/dev/null || true
umount "${MOUNT_POINT}/proc" 2>/dev/null || true
umount "${MOUNT_POINT}/dev/pts" 2>/dev/null || true
umount "${MOUNT_POINT}/dev" 2>/dev/null || true

# ─── Step 7: Shrink filesystem (optional) ──────────────────────────
if [ "$SHRINK" = true ]; then
    log "Shrinking filesystem to minimum size..."
    # Unmount first
    umount "${MOUNT_POINT}/boot"
    umount "$MOUNT_POINT"

    # Run e2fsck and resize
    e2fsck -f "$LOOP_DEV"
    MIN_SIZE=$(resize2fs -P "$LOOP_DEV" 2>/dev/null | awk '{print $NF}')
    resize2fs "$LOOP_DEV" "${MIN_SIZE}"

    # Detach loop device
    losetup -d "$LOOP_DEV"
    losetup -d "$BOOT_LOOP_DEV"

    # TODO: Truncate image file to actual size (requires recalculating partition)
    # For now, the image stays at the original size
    warn "  Shrinking is partial — image size not yet reduced (requires partition recalculation)"
else
    # Unmount
    umount "${MOUNT_POINT}/boot"
    umount "$MOUNT_POINT"
    losetup -d "$LOOP_DEV"
    losetup -d "$BOOT_LOOP_DEV"
fi

log "Image unmounted"

# ─── Step 8: Compute SHA-256 checksum ──────────────────────────────
log "Computing SHA-256 checksum..."
CHECKSUM_FILE="${OUTPUT_DIR}/${IMAGE_NAME}.img.sha256"
sha256sum "$WORK_IMG" > "$CHECKSUM_FILE"
info "  Checksum: $(cat "$CHECKSUM_FILE")"

# ─── Step 9: Compress (optional) ───────────────────────────────────
FINAL_IMG="$WORK_IMG"
if [ "$COMPRESS" = true ]; then
    log "Compressing image with xz (this may take 10-20 minutes)..."
    xz -T0 -6 "$WORK_IMG"
    FINAL_IMG="${WORK_IMG}.xz"

    # Checksum for compressed file
    COMPRESSED_CHECKSUM="${OUTPUT_DIR}/${IMAGE_NAME}.img.xz.sha256"
    sha256sum "$FINAL_IMG" > "$COMPRESSED_CHECKSUM"
    info "  Compressed checksum: $(cat "$COMPRESSED_CHECKSUM")"
fi

# ─── Done ───────────────────────────────────────────────────────────
echo ""
log "${GREEN}════════════════════════════════════════════${NC}"
log "${GREEN}  SD Card Image built successfully!${NC}"
log "${GREEN}════════════════════════════════════════════${NC}"
echo ""
info "Image:    ${FINAL_IMG}"
info "Size:     $(du -h "$FINAL_IMG" | cut -f1)"
info "Checksum: ${CHECKSUM_FILE}"
echo ""
info "Flash to SD card:"
info "  # Using Raspberry Pi Imager (recommended)"
info "  # Or with dd:"
if [ "$COMPRESS" = true ]; then
    info "  xzcat ${FINAL_IMG} | sudo dd bs=4M of=/dev/sdX status=progress conv=fsync"
else
    info "  sudo dd bs=4M if=${FINAL_IMG} of=/dev/sdX status=progress conv=fsync"
fi
echo ""
info "After boot:"
info "  1. Find the Pi's IP: ping bogdan.local or check your router"
info "  2. SSH: ssh pi@<ip> (default password: raspberry — CHANGE THIS!)"
info "  3. Verify boGDan: curl http://<ip>:8585/api/health"
info "  4. Install the browser extension from /usr/share/bogdan/extension-chrome/"
echo ""
