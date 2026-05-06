#!/usr/bin/env bash
# ╔══════════════════════════════════════════════════════════════════╗
# ║  PiCast Setup Script — Production-Grade One-Command Install     ║
# ║  Target: Raspberry Pi OS Lite 64-bit (bookworm) / Debian       ║
# ║  Run as: sudo bash scripts/setup.sh                            ║
# ╚══════════════════════════════════════════════════════════════════╝
set -euo pipefail

# ─── Version ────────────────────────────────────────────────────────
PICAST_SETUP_VERSION="0.1.0"

# ─── Paths (resolve repo root relative to this script) ──────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
CONFIG_DIR="${REPO_ROOT}/config"
LOG_FILE="/var/log/picast-setup.log"

# ─── Colors & Output ────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m' # No Color

log()       { echo -e "${GREEN}[PICAST]${NC} $*" | tee -a "$LOG_FILE"; }
warn()      { echo -e "${YELLOW}[WARN]${NC}  $*" | tee -a "$LOG_FILE" >&2; }
error()     { echo -e "${RED}[ERROR]${NC} $*" | tee -a "$LOG_FILE" >&2; }
info()      { echo -e "${BLUE}[INFO]${NC}  $*" | tee -a "$LOG_FILE"; }
step()      { echo -e "${CYAN}${BOLD}[${CURRENT_STEP}/${TOTAL_STEPS}]${NC} $*" | tee -a "$LOG_FILE"; }

# ─── Progress tracking ──────────────────────────────────────────────
CURRENT_STEP=0
TOTAL_STEPS=9

step_next() {
    CURRENT_STEP=$((CURRENT_STEP + 1))
}

# ─── Flags ──────────────────────────────────────────────────────────
SKIP_TOR=false
SKIP_BUILD=false
SKIP_FIREWALL=false
CROSS_COMPILE=false
UNINSTALL=false

# ─── Cleanup trap ───────────────────────────────────────────────────
cleanup() {
    local exit_code=$?
    if [ $exit_code -ne 0 ]; then
        error "Setup failed at step ${CURRENT_STEP}/${TOTAL_STEPS} (exit code: ${exit_code})"
        error "Check log at ${LOG_FILE} for details"
        # Restore any backed-up configs if we failed mid-write
        if [ -d "${BACKUP_DIR:-}" ]; then
            warn "Backups preserved at ${BACKUP_DIR} — manual restore may be needed"
        fi
    fi
    exit $exit_code
}
trap cleanup EXIT

# ─── Backup helper ──────────────────────────────────────────────────
BACKUP_DIR=""
backup_file() {
    local src="$1"
    if [ ! -f "$src" ]; then
        return 0
    fi
    if [ -z "$BACKUP_DIR" ]; then
        BACKUP_DIR="/var/backups/picast-setup-$(date +%Y%m%d-%H%M%S)"
        mkdir -p "$BACKUP_DIR"
    fi
    local base
    base="$(basename "$src")"
    cp -a "$src" "${BACKUP_DIR}/${base}.bak"
    info "Backed up ${src} -> ${BACKUP_DIR}/${base}.bak"
}

# ─── Usage / Help ───────────────────────────────────────────────────
usage() {
    cat <<EOF
${BOLD}PiCast Setup Script v${PICAST_SETUP_VERSION}${NC}

Usage: sudo bash scripts/setup.sh [OPTIONS]

Options:
  --skip-tor         Skip Tor configuration
  --skip-build       Skip building PiCast from source
  --skip-firewall    Skip firewall/iptables configuration
  --cross-compile    Cross-compile for aarch64 from x86_64 host
  --uninstall        Remove PiCast completely
  --help             Show this help message

Examples:
  sudo bash scripts/setup.sh                    # Full install
  sudo bash scripts/setup.sh --skip-build       # Install without building
  sudo bash scripts/setup.sh --cross-compile    # Cross-compile for Pi on x86
  sudo bash scripts/setup.sh --uninstall        # Remove PiCast
EOF
    exit 0
}

# ─── Argument Parsing ───────────────────────────────────────────────
parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --skip-tor)       SKIP_TOR=true;       shift ;;
            --skip-build)     SKIP_BUILD=true;      shift ;;
            --skip-firewall)  SKIP_FIREWALL=true;   shift ;;
            --cross-compile)  CROSS_COMPILE=true;    shift ;;
            --uninstall)      UNINSTALL=true;        shift ;;
            --help|-h)        usage ;;
            *)
                error "Unknown option: $1"
                usage
                ;;
        esac
    done
}

# ─── Pre-flight Checks ──────────────────────────────────────────────
preflight_checks() {
    step_next
    step "Running pre-flight checks..."

    # Check root
    if [ "$(id -u)" -ne 0 ]; then
        error "This script must be run as root. Use: sudo bash scripts/setup.sh"
        exit 1
    fi
    info "Running as root: OK"

    # Check OS
    if [ ! -f /etc/os-release ]; then
        error "Cannot determine OS — /etc/os-release not found"
        exit 1
    fi
    # shellcheck source=/dev/null
    source /etc/os-release
    case "${ID:-}" in
        debian|ubuntu|raspbian)
            info "OS: ${PRETTY_NAME} — supported"
            ;;
        *)
            warn "OS '${ID}' is not officially supported. Proceeding anyway..."
            ;;
    esac

    # Check if running on Raspberry Pi (unless cross-compiling)
    if [ "$CROSS_COMPILE" = false ]; then
        if [ -f /proc/device-tree/model ]; then
            local model
            model="$(tr -d '\0' < /proc/device-tree/model)"
            info "Hardware: ${model}"
        else
            warn "Not running on Raspberry Pi hardware (no /proc/device-tree/model)"
            warn "Some features (GPU, V4L2) may not work"
        fi
    else
        info "Cross-compilation mode — skipping Pi hardware check"
    fi

    # Initialize log file
    mkdir -p "$(dirname "$LOG_FILE")"
    echo "=== PiCast Setup Log — $(date) ===" > "$LOG_FILE"
    info "Logging to ${LOG_FILE}"
}

# ─── Uninstall ──────────────────────────────────────────────────────
do_uninstall() {
    echo ""
    log "${BOLD}╔══════════════════════════════════════════╗${NC}"
    log "${BOLD}║     PiCast Uninstall                    ║${NC}"
    log "${BOLD}╚══════════════════════════════════════════╝${NC}"
    echo ""

    # Stop services
    if systemctl is-active --quiet picast 2>/dev/null; then
        info "Stopping picast service..."
        systemctl stop picast || true
    fi
    if systemctl is-enabled --quiet picast 2>/dev/null; then
        info "Disabling picast service..."
        systemctl disable picast || true
    fi

    # Remove service file
    if [ -f /etc/systemd/system/picast.service ]; then
        rm -f /etc/systemd/system/picast.service
        systemctl daemon-reload
        info "Removed systemd service"
    fi

    # Remove binary
    if [ -f /usr/local/bin/picast-server ]; then
        rm -f /usr/local/bin/picast-server
        info "Removed /usr/local/bin/picast-server"
    fi

    # Remove user
    if id -u picast &>/dev/null; then
        userdel picast 2>/dev/null || true
        info "Removed picast user"
    fi

    # Remove data directory
    if [ -d /var/lib/picast ]; then
        rm -rf /var/lib/picast
        info "Removed /var/lib/picast"
    fi

    # Remove temp directory
    if [ -d /tmp/picast ]; then
        rm -rf /tmp/picast
        info "Removed /tmp/picast"
    fi

    # Optionally restore Tor config
    if [ -f /etc/tor/torrc ] && command -v tor &>/dev/null; then
        warn "Tor config at /etc/tor/torrc was modified by PiCast — review manually"
    fi

    echo ""
    log "${GREEN}PiCast has been uninstalled.${NC}"
    exit 0
}

# ─── Install Dependencies ───────────────────────────────────────────
install_dependencies() {
    step_next
    step "Installing system dependencies..."

    local pkgs=(
        tor
        gstreamer1.0-tools
        gstreamer1.0-plugins-base
        gstreamer1.0-plugins-good
        gstreamer1.0-plugins-bad
        gstreamer1.0-plugins-ugly
        gstreamer1.0-libav
        gmediarender
        yt-dlp
        python3-pip
        build-essential
        pkg-config
        libgstreamer1.0-dev
        libgstreamer-plugins-base1.0-dev
        libgstreamer-plugins-bad1.0-dev
        libdrm-dev
        libgbm-dev
        libegl-dev
        libgles2-dev
        libsqlite3-dev
        libssl-dev
        iptables
        dnsmasq
        avahi-daemon
        git
        curl
    )

    # Cross-compilation needs additional packages
    if [ "$CROSS_COMPILE" = true ]; then
        pkgs+=(
            gcc-aarch64-linux-gnu
            binutils-aarch64-linux-gnu
        )
    fi

    # Filter out already-installed packages
    local to_install=()
    for pkg in "${pkgs[@]}"; do
        if dpkg -s "$pkg" &>/dev/null 2>&1; then
            info "  ${pkg} — already installed"
        else
            to_install+=("$pkg")
        fi
    done

    if [ ${#to_install[@]} -gt 0 ]; then
        info "Installing ${#to_install[@]} packages: ${to_install[*]}"
        apt-get update -y
        apt-get install -y "${to_install[@]}"
    else
        info "All dependencies already installed"
    fi
}

# ─── Install Rust ───────────────────────────────────────────────────
install_rust() {
    step_next
    step "Setting up Rust toolchain..."

    if [ "$SKIP_BUILD" = true ]; then
        info "Skipping — build disabled"
        return 0
    fi

    # Install rustup if not present
    if ! command -v cargo &>/dev/null; then
        info "Installing rustup..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        # shellcheck source=/dev/null
        source "${HOME}/.cargo/env"
    else
        info "Rust toolchain already installed: $(cargo --version)"
    fi

    # Add aarch64 target for cross-compilation
    if [ "$CROSS_COMPILE" = true ]; then
        info "Adding aarch64-unknown-linux-gnu target..."
        rustup target add aarch64-unknown-linux-gnu
        info "Configuring linker for aarch64..."
        # Ensure cargo config exists for cross-compilation
        mkdir -p "${REPO_ROOT}/.cargo"
        cat > "${REPO_ROOT}/.cargo/config.toml" <<'CARGO_CFG'
[target.aarch64-unknown-linux-gnu]
linker = "aarch64-linux-gnu-gcc"
CARGO_CFG
    fi
}

# ─── Configure Kernel Overlays ──────────────────────────────────────
configure_kernel() {
    step_next
    step "Configuring kernel overlays..."

    if [ "$CROSS_COMPILE" = true ]; then
        info "Cross-compilation mode — skipping kernel overlay configuration"
        return 0
    fi

    if [ ! -f /boot/config.txt ]; then
        warn "/boot/config.txt not found — skipping kernel overlay configuration"
        return 0
    fi

    if grep -q "vc4-kms-v3d" /boot/config.txt 2>/dev/null; then
        info "vc4-kms-v3d overlay already configured"
    else
        backup_file /boot/config.txt
        cat >> /boot/config.txt << 'EOF'

# PiCast: Enable DRM/KMS with V3D GPU
dtoverlay=vc4-kms-v3d

# PiCast: Enable HEVC V4L2 decoder (for v2)
# dtoverlay=rpivid-v4l2

# PiCast: Disable WiFi and Bluetooth (reduce attack surface, save power)
# dtoverlay=disable-wifi
# dtoverlay=disable-bt
EOF
        info "Added vc4-kms-v3d overlay — reboot required"
    fi
}

# ─── Configure Tor ──────────────────────────────────────────────────
configure_tor() {
    step_next
    step "Configuring Tor..."

    if [ "$SKIP_TOR" = true ]; then
        info "Skipping Tor configuration (--skip-tor)"
        return 0
    fi

    if [ ! -f "${CONFIG_DIR}/torrc" ]; then
        error "Tor config not found at ${CONFIG_DIR}/torrc"
        exit 1
    fi

    backup_file /etc/tor/torrc
    cp "${CONFIG_DIR}/torrc" /etc/tor/torrc
    chmod 644 /etc/tor/torrc

    # Validate the torrc before restarting Tor
    if command -v tor &>/dev/null; then
        if ! tor --verify-config 2>/dev/null; then
            warn "Tor configuration validation failed — check /etc/tor/torrc"
        fi
    fi

    if systemctl is-active --quiet tor 2>/dev/null; then
        systemctl restart tor
        info "Tor restarted"
    else
        systemctl start tor
        info "Tor started"
    fi
    systemctl enable tor
    info "Tor enabled on boot"
}

# ─── Configure Firewall ────────────────────────────────────────────
configure_firewall() {
    step_next
    step "Configuring firewall..."

    if [ "$SKIP_FIREWALL" = true ]; then
        info "Skipping firewall configuration (--skip-firewall)"
        return 0
    fi

    if [ ! -f "${CONFIG_DIR}/iptables.rules" ]; then
        error "Firewall rules not found at ${CONFIG_DIR}/iptables.rules"
        exit 1
    fi

    backup_file /etc/iptables/rules.v4
    iptables-restore < "${CONFIG_DIR}/iptables.rules"

    # Persist rules
    if ! dpkg -s iptables-persistent &>/dev/null 2>&1; then
        # Pre-seed debconf to avoid interactive prompt
        echo "iptables-persistent iptables-persistent/autosave_v4 boolean true" | debconf-set-selections
        echo "iptables-persistent iptables-persistent/autosave_v6 boolean true" | debconf-set-selections
        DEBIAN_FRONTEND=noninteractive apt-get install -y iptables-persistent
    fi
    netfilter-persistent save
    info "Firewall rules applied and persisted"
}

# ─── Build PiCast ───────────────────────────────────────────────────
build_picast() {
    step_next
    step "Building PiCast..."

    if [ "$SKIP_BUILD" = true ]; then
        info "Skipping build (--skip-build)"
        return 0
    fi

    # Ensure we're in the repo root for cargo
    cd "${REPO_ROOT}"

    local build_target=""
    if [ "$CROSS_COMPILE" = true ]; then
        build_target="--target aarch64-unknown-linux-gnu"
        info "Cross-compiling for aarch64-unknown-linux-gnu..."
    else
        info "Building for native target..."
    fi

    # shellcheck source=/dev/null
    source "${HOME}/.cargo/env" 2>/dev/null || true

    cargo build --release $build_target
    info "Build complete"
}

# ─── Install PiCast ─────────────────────────────────────────────────
install_picast() {
    step_next
    step "Installing PiCast..."

    # Determine binary path based on build target
    local binary_path
    if [ "$CROSS_COMPILE" = true ]; then
        binary_path="${REPO_ROOT}/target/aarch64-unknown-linux-gnu/release/picast"
    else
        binary_path="${REPO_ROOT}/target/release/picast"
    fi

    if [ "$SKIP_BUILD" = false ] && [ ! -f "$binary_path" ]; then
        error "Built binary not found at ${binary_path}"
        exit 1
    fi

    # Create picast user if it doesn't exist
    if ! id -u picast &>/dev/null; then
        useradd -r -m -s /usr/sbin/nologin picast
        info "Created picast system user"
    else
        info "picast user already exists"
    fi

    # Ensure user is in required groups (idempotent)
    usermod -aG video,render,audio picast
    info "picast user in groups: video, render, audio"

    # Install binary
    if [ "$SKIP_BUILD" = false ]; then
        cp "$binary_path" /usr/local/bin/picast-server
        chmod 755 /usr/local/bin/picast-server
        info "Installed binary to /usr/local/bin/picast-server"
    fi

    # Create data directory
    mkdir -p /var/lib/picast
    chown picast:picast /var/lib/picast
    info "Data directory: /var/lib/picast"

    # Create temp directory
    mkdir -p /tmp/picast/subs
    chown picast:picast /tmp/picast
    info "Temp directory: /tmp/picast"

    # Install TOML config file
    mkdir -p /etc/picast
    if [ -f "${REPO_ROOT}/picast.toml.example" ]; then
        if [ ! -f /etc/picast/picast.toml ]; then
            cp "${REPO_ROOT}/picast.toml.example" /etc/picast/picast.toml
            chown picast:picast /etc/picast/picast.toml
            chmod 644 /etc/picast/picast.toml
            info "Installed config to /etc/picast/picast.toml"
        else
            info "Config already exists at /etc/picast/picast.toml — not overwriting"
        fi
    else
        warn "picast.toml.example not found — skipping config installation"
    fi

    # Install systemd service (update ExecStart to match our binary name)
    if [ -f "${CONFIG_DIR}/picast.service" ]; then
        backup_file /etc/systemd/system/picast.service
        # Patch ExecStart to use picast-server
        sed 's|ExecStart=/usr/local/bin/picast|ExecStart=/usr/local/bin/picast-server|' \
            "${CONFIG_DIR}/picast.service" > /etc/systemd/system/picast.service
        chmod 644 /etc/systemd/system/picast.service
        systemctl daemon-reload
        systemctl enable picast
        info "Systemd service installed and enabled"
    else
        warn "picast.service not found in ${CONFIG_DIR}"
    fi
}

# ─── Verification ───────────────────────────────────────────────────
verify_installation() {
    step_next
    step "Verifying installation..."

    local failed=0

    # Check binary
    if [ -x /usr/local/bin/picast-server ]; then
        info "  [OK] /usr/local/bin/picast-server exists and is executable"
    else
        error "  [FAIL] /usr/local/bin/picast-server not found or not executable"
        failed=1
    fi

    # Check that the binary can run (basic smoke test)
    if [ -x /usr/local/bin/picast-server ]; then
        if /usr/local/bin/picast-server --version &>/dev/null; then
            info "  [OK] picast-server --version succeeds"
        else
            # Binary might not support --version, try --help
            if /usr/local/bin/picast-server --help &>/dev/null; then
                info "  [OK] picast-server --help succeeds"
            else
                warn "  [WARN] Cannot verify picast-server runs (no --version/--help)"
            fi
        fi
    fi

    # Check Tor
    if [ "$SKIP_TOR" = false ]; then
        if systemctl is-active --quiet tor 2>/dev/null; then
            info "  [OK] Tor service is running"
        else
            warn "  [WARN] Tor service is not running"
        fi
        if systemctl is-enabled --quiet tor 2>/dev/null; then
            info "  [OK] Tor is enabled on boot"
        else
            warn "  [WARN] Tor is not enabled on boot"
        fi
    fi

    # Check PiCast service
    if systemctl is-enabled --quiet picast 2>/dev/null; then
        info "  [OK] picast service is enabled"
    else
        error "  [FAIL] picast service is not enabled"
        failed=1
    fi

    # Check user
    if id -u picast &>/dev/null; then
        info "  [OK] picast user exists"
    else
        error "  [FAIL] picast user does not exist"
        failed=1
    fi

    # Check data directory
    if [ -d /var/lib/picast ]; then
        info "  [OK] /var/lib/picast exists"
    else
        error "  [FAIL] /var/lib/picast does not exist"
        failed=1
    fi

    # Check firewall
    if [ "$SKIP_FIREWALL" = false ]; then
        if iptables -L INPUT 2>/dev/null | grep -q "8585"; then
            info "  [OK] Firewall rules include PiCast port 8585"
        else
            warn "  [WARN] Firewall rules may not include PiCast ports"
        fi
    fi

    if [ $failed -ne 0 ]; then
        error "Verification failed — check output above"
        exit 1
    fi
}

# ─── Banner ─────────────────────────────────────────────────────────
print_banner() {
    echo ""
    echo -e "${CYAN}${BOLD}╔═══════════════════════════════════════════════════╗${NC}"
    echo -e "${CYAN}${BOLD}║                                                   ║${NC}"
    echo -e "${CYAN}${BOLD}║   ██████╗ █████╗  ██████╗██╗   ██╗███████╗       ║${NC}"
    echo -e "${CYAN}${BOLD}║   ██╔══██╗██╔══██╗██╔════╝╚██╗ ██╔╝██╔════╝       ║${NC}"
    echo -e "${CYAN}${BOLD}║   ██████╔╝███████║██║      ╚████╔╝ █████╗         ║${NC}"
    echo -e "${CYAN}${BOLD}║   ██╔══██╗██╔══██║██║       ╚██╔╝  ██╔══╝         ║${NC}"
    echo -e "${CYAN}${BOLD}║   ██████╔╝██║  ██║╚██████╗   ██║   ███████╗       ║${NC}"
    echo -e "${CYAN}${BOLD}║   ╚═════╝ ╚═╝  ╚═╝ ╚═════╝   ╚═╝   ╚══════╝       ║${NC}"
    echo -e "${CYAN}${BOLD}║                                                   ║${NC}"
    echo -e "${CYAN}${BOLD}║   Tor-routed media casting appliance              ║${NC}"
    echo -e "${CYAN}${BOLD}║   Setup v${PICAST_SETUP_VERSION}                                   ║${NC}"
    echo -e "${CYAN}${BOLD}║                                                   ║${NC}"
    echo -e "${CYAN}${BOLD}╚═══════════════════════════════════════════════════╝${NC}"
    echo ""
}

# ─── Print Summary ──────────────────────────────────────────────────
print_summary() {
    echo ""
    log "${GREEN}${BOLD}╔══════════════════════════════════════════╗${NC}"
    log "${GREEN}${BOLD}║   PiCast Setup Complete!                 ║${NC}"
    log "${GREEN}${BOLD}╚══════════════════════════════════════════╝${NC}"
    echo ""
    info "Next steps:"
    info "  1. Reboot to apply kernel overlay:  ${BOLD}sudo reboot${NC}"
    info "  2. Start PiCast:                    ${BOLD}sudo systemctl start picast${NC}"
    info "  3. Check status:                    ${BOLD}sudo systemctl status picast${NC}"
    info "  4. View logs:                       ${BOLD}journalctl -u picast -f${NC}"
    echo ""
    info "  Cast from browser: Install the extension from src/extension/"
    info "  Cast from VLC:     Playback → Renderer → PiCast"
    local ip_addr
    ip_addr="$(hostname -I 2>/dev/null | cut -d' ' -f1)" || ip_addr="<PI-IP>"
    info "  Cast via API:      curl -X POST http://${ip_addr}:8585/api/cast -H 'Content-Type: application/json' -d '{\"url\": \"https://www.youtube.com/watch?v=dQw4w9WgXcQ\"}'"
    echo ""
    if [ -n "${BACKUP_DIR:-}" ]; then
        info "Config backups saved to: ${BACKUP_DIR}"
    fi
    info "Setup log: ${LOG_FILE}"
    echo ""
}

# ═══════════════════════════════════════════════════════════════════
#  Main
# ═══════════════════════════════════════════════════════════════════
main() {
    parse_args "$@"

    print_banner

    if [ "$UNINSTALL" = true ]; then
        do_uninstall
    fi

    preflight_checks
    install_dependencies
    install_rust
    configure_kernel
    configure_tor
    configure_firewall
    build_picast
    install_picast
    verify_installation
    print_summary
}

main "$@"
