#!/usr/bin/env bash
# ╔══════════════════════════════════════════════════════════════════╗
# ║  boGDan Debian Package Builder                                  ║
# ║  Cross-compiles for aarch64 and builds a .deb package           ║
# ╚══════════════════════════════════════════════════════════════════╝
set -euo pipefail

# ─── Configuration ──────────────────────────────────────────────────
PACKAGE_NAME="bogdan"
PACKAGE_VERSION="0.1.0"
PACKAGE_ARCH="arm64"
TARGET_TRIPLE="aarch64-unknown-linux-gnu"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
CONFIG_DIR="${REPO_ROOT}/config"
DEBIAN_DIR="${SCRIPT_DIR}/debian"

# Build output
BUILD_DIR="${SCRIPT_DIR}/build"
DEB_ROOT="${BUILD_DIR}/${PACKAGE_NAME}_${PACKAGE_VERSION}_${PACKAGE_ARCH}"
DEB_FILE="${BUILD_DIR}/${PACKAGE_NAME}_${PACKAGE_VERSION}_${PACKAGE_ARCH}.deb"

# ─── Colors ─────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log()  { echo -e "${GREEN}[DEB]${NC} $*"; }
err()  { echo -e "${RED}[ERROR]${NC} $*" >&2; exit 1; }
info() { echo -e "${BLUE}[INFO]${NC} $*"; }

# ─── Pre-flight ─────────────────────────────────────────────────────
info "boGDan Debian Package Builder v${PACKAGE_VERSION}"

# Check for required tools
for cmd in cargo dpkg-deb; do
    if ! command -v "$cmd" &>/dev/null; then
        err "Required command not found: $cmd"
    fi
done

# ─── Step 1: Install cross-compilation toolchain ────────────────────
log "Checking cross-compilation setup..."

if ! rustup target list --installed | grep -q "$TARGET_TRIPLE"; then
    info "Adding Rust target: ${TARGET_TRIPLE}"
    rustup target add "$TARGET_TRIPLE"
fi

# Ensure cargo config has the correct linker
CARGO_CONFIG_DIR="${REPO_ROOT}/.cargo"
CARGO_CONFIG="${CARGO_CONFIG_DIR}/config.toml"
mkdir -p "$CARGO_CONFIG_DIR"

if [ ! -f "$CARGO_CONFIG" ] || ! grep -q "$TARGET_TRIPLE" "$CARGO_CONFIG" 2>/dev/null; then
    info "Configuring cross-compilation linker..."
    cat > "$CARGO_CONFIG" <<CARGO_CFG
[target.aarch64-unknown-linux-gnu]
linker = "aarch64-linux-gnu-gcc"
CARGO_CFG
fi

# Check for cross-compiler
if ! command -v aarch64-linux-gnu-gcc &>/dev/null; then
    info "Installing cross-compilation toolchain..."
    sudo apt-get update -y
    sudo apt-get install -y gcc-aarch64-linux-gnu binutils-aarch64-linux-gnu
fi

# ─── Step 2: Cross-compile ─────────────────────────────────────────
log "Cross-compiling boGDan for ${TARGET_TRIPLE}..."

cd "${REPO_ROOT}"
# shellcheck source=/dev/null
source "${HOME}/.cargo/env" 2>/dev/null || true

cargo build --release --target "$TARGET_TRIPLE"

BINARY_PATH="${REPO_ROOT}/target/${TARGET_TRIPLE}/release/bogdan"
if [ ! -f "$BINARY_PATH" ]; then
    err "Binary not found at ${BINARY_PATH}"
fi
log "Build successful: ${BINARY_PATH}"

# ─── Step 3: Create debian directory structure ──────────────────────
log "Creating package directory structure..."

# Clean previous build
rm -rf "$BUILD_DIR"
mkdir -p "$DEB_ROOT/DEBIAN"
mkdir -p "$DEB_ROOT/usr/local/bin"
mkdir -p "$DEB_ROOT/etc/bogdan"
mkdir -p "$DEB_ROOT/etc/bogdan/providers.d"
mkdir -p "$DEB_ROOT/etc/systemd/system"
mkdir -p "$DEB_ROOT/var/lib/bogdan"
mkdir -p "$DEB_ROOT/usr/share/doc/bogdan"
mkdir -p "$DEB_ROOT/usr/share/bogdan/extension-chrome"
mkdir -p "$DEB_ROOT/usr/share/bogdan/extension-firefox"
mkdir -p "$DEB_ROOT/usr/share/bogdan/scripts"

# ─── Step 4: Copy files ────────────────────────────────────────────
log "Installing files into package..."

# Binary
cp "$BINARY_PATH" "${DEB_ROOT}/usr/local/bin/bogdan-server"
chmod 755 "${DEB_ROOT}/usr/local/bin/bogdan-server"

# Config files
cp "${CONFIG_DIR}/torrc" "${DEB_ROOT}/etc/bogdan/torrc"
chmod 644 "${DEB_ROOT}/etc/bogdan/torrc"

cp "${CONFIG_DIR}/iptables.rules" "${DEB_ROOT}/etc/bogdan/iptables.rules"
chmod 644 "${DEB_ROOT}/etc/bogdan/iptables.rules"

# Example TOML config (installed as the live config if not already present)
if [ -f "${REPO_ROOT}/bogdan.toml.example" ]; then
    cp "${REPO_ROOT}/bogdan.toml.example" "${DEB_ROOT}/etc/bogdan/bogdan.toml"
    chmod 644 "${DEB_ROOT}/etc/bogdan/bogdan.toml"
fi

# Systemd service (patch ExecStart to use bogdan-server)
sed 's|ExecStart=/usr/local/bin/bogdan|ExecStart=/usr/local/bin/bogdan-server|' \
    "${CONFIG_DIR}/bogdan.service" > "${DEB_ROOT}/etc/systemd/system/bogdan.service"
chmod 644 "${DEB_ROOT}/etc/systemd/system/bogdan.service"

# Provider configs
log "Installing provider configs..."
PROVIDERS_DIR="${REPO_ROOT}/providers.d"
if [ -d "$PROVIDERS_DIR" ]; then
    for toml_file in "${PROVIDERS_DIR}"/*.toml; do
        if [ -f "$toml_file" ]; then
            cp "$toml_file" "${DEB_ROOT}/etc/bogdan/providers.d/"
            chmod 644 "${DEB_ROOT}/etc/bogdan/providers.d/$(basename "$toml_file")"
            info "  Provider: $(basename "$toml_file")"
        fi
    done
else
    warn "providers.d/ directory not found — provider configs not included"
fi

# Browser extension files
log "Installing browser extension files..."
EXT_DIR="${REPO_ROOT}/src/extension"
if [ -d "$EXT_DIR" ]; then
    # Chrome extension
    for f in manifest-chrome.json popup/ options/ content/ background/ icons/ _locales/; do
        src_path="${EXT_DIR}/${f}"
        if [ -e "$src_path" ]; then
            cp -r "$src_path" "${DEB_ROOT}/usr/share/bogdan/extension-chrome/"
        fi
    done
    # Copy the default manifest as manifest.json for Chrome
    if [ -f "${EXT_DIR}/manifest-chrome.json" ]; then
        cp "${EXT_DIR}/manifest-chrome.json" "${DEB_ROOT}/usr/share/bogdan/extension-chrome/manifest.json"
    fi

    # Firefox extension
    for f in manifest-firefox.json popup/ options/ content/ background/ icons/ _locales/; do
        src_path="${EXT_DIR}/${f}"
        if [ -e "$src_path" ]; then
            cp -r "$src_path" "${DEB_ROOT}/usr/share/bogdan/extension-firefox/"
        fi
    done
    # Copy the Firefox manifest as manifest.json
    if [ -f "${EXT_DIR}/manifest-firefox.json" ]; then
        cp "${EXT_DIR}/manifest-firefox.json" "${DEB_ROOT}/usr/share/bogdan/extension-firefox/manifest.json"
    fi

    info "  Chrome extension: $(find "${DEB_ROOT}/usr/share/bogdan/extension-chrome" -type f | wc -l) files"
    info "  Firefox extension: $(find "${DEB_ROOT}/usr/share/bogdan/extension-firefox" -type f | wc -l) files"
else
    warn "src/extension/ directory not found — extension files not included"
fi

# Utility scripts
log "Installing utility scripts..."
SCRIPTS_DIR="${REPO_ROOT}/scripts"
if [ -d "$SCRIPTS_DIR" ]; then
    for script in smoke-test.sh verify-network-isolation.sh mem-test.sh soak-test.sh; do
        if [ -f "${SCRIPTS_DIR}/${script}" ]; then
            cp "${SCRIPTS_DIR}/${script}" "${DEB_ROOT}/usr/share/bogdan/scripts/"
            chmod 755 "${DEB_ROOT}/usr/share/bogdan/scripts/${script}"
            info "  Script: ${script}"
        fi
    done
fi

# Data directory ownership will be set by postinst
# Create a placeholder to ensure the directory is included
touch "${DEB_ROOT}/var/lib/bogdan/.gitkeep"

# Copyright / license
if [ -f "${REPO_ROOT}/LICENSE" ]; then
    cp "${REPO_ROOT}/LICENSE" "${DEB_ROOT}/usr/share/doc/bogdan/copyright"
fi

# Changelog (Debian format)
if [ -f "${REPO_ROOT}/CHANGELOG.md" ]; then
    cp "${REPO_ROOT}/CHANGELOG.md" "${DEB_ROOT}/usr/share/doc/bogdan/changelog"
fi

# ─── Step 5: Copy debian control files ──────────────────────────────
log "Creating debian control files..."

# control file (patch version)
sed "s/Version:.*/Version: ${PACKAGE_VERSION}/" \
    "${DEBIAN_DIR}/control" > "${DEB_ROOT}/DEBIAN/control"

# maintainer scripts
cp "${DEBIAN_DIR}/postinst" "${DEB_ROOT}/DEBIAN/postinst"
cp "${DEBIAN_DIR}/prerm" "${DEB_ROOT}/DEBIAN/prerm"
cp "${DEBIAN_DIR}/postrm" "${DEB_ROOT}/DEBIAN/postrm"

# conffiles
if [ -f "${DEBIAN_DIR}/conffiles" ]; then
    cp "${DEBIAN_DIR}/conffiles" "${DEB_ROOT}/DEBIAN/conffiles"
fi

# Make maintainer scripts executable
chmod 755 "${DEB_ROOT}/DEBIAN/postinst"
chmod 755 "${DEB_ROOT}/DEBIAN/prerm"
chmod 755 "${DEB_ROOT}/DEBIAN/postrm"

# ─── Step 6: Calculate installed size ───────────────────────────────
INSTALLED_SIZE=$(du -sk "$DEB_ROOT" | cut -f1)
info "Installed-Size: ${INSTALLED_SIZE} KB"

# Update control with actual installed size
sed -i "s/Installed-Size:.*/Installed-Size: ${INSTALLED_SIZE}/" "${DEB_ROOT}/DEBIAN/control"

# ─── Step 7: Build the .deb package ────────────────────────────────
log "Building .deb package..."
dpkg-deb --build --root-owner-group "$DEB_ROOT" "$DEB_FILE"

# ─── Step 8: Generate SHA-256 checksum ─────────────────────────────
log "Generating SHA-256 checksum..."

CHECKSUM_FILE="${BUILD_DIR}/${PACKAGE_NAME}_${PACKAGE_VERSION}_${PACKAGE_ARCH}.sha256"
sha256sum "$DEB_FILE" > "$CHECKSUM_FILE"
info "Checksum: $(cat "$CHECKSUM_FILE")"

# ─── Step 9: Verify the package ────────────────────────────────────
log "Verifying package structure..."

# Check that dpkg can read it
if dpkg-deb --info "$DEB_FILE" >/dev/null 2>&1; then
    info "  [OK] dpkg-deb --info succeeds"
else
    err "  [FAIL] dpkg-deb --info failed — package may be corrupt"
fi

# Check that all expected files are present
for expected in \
    /usr/local/bin/bogdan-server \
    /etc/bogdan/bogdan.toml \
    /etc/bogdan/torrc \
    /etc/bogdan/iptables.rules \
    /etc/systemd/system/bogdan.service; do
    if dpkg-deb --fsys-tarfile "$DEB_FILE" | tar -t "./$expected" >/dev/null 2>&1; then
        info "  [OK] ${expected}"
    else
        warn "  [MISSING] ${expected}"
    fi
done

# Check for provider configs
PROVIDER_COUNT=$(dpkg-deb --fsys-tarfile "$DEB_FILE" | tar -t 2>/dev/null | grep -c 'providers\.d/.*\.toml' || echo "0")
info "  Provider configs included: ${PROVIDER_COUNT}"

# ─── Done ───────────────────────────────────────────────────────────
echo ""
log "${GREEN}════════════════════════════════════════════${NC}"
log "${GREEN}  Package built successfully!${NC}"
log "${GREEN}════════════════════════════════════════════${NC}"
echo ""
info "Output:   ${DEB_FILE}"
info "Checksum: ${CHECKSUM_FILE}"
info "Size:     $(du -h "$DEB_FILE" | cut -f1)"
info ""
info "Install on target:"
info "  scp ${DEB_FILE} pi@<ip>:/tmp/"
info "  ssh pi@<ip> 'sudo dpkg -i /tmp/$(basename "$DEB_FILE")'"
echo ""
info "Verify integrity:"
info "  sha256sum -c $(basename "$CHECKSUM_FILE")"
echo ""
