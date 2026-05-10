#!/usr/bin/env bash
# ╔══════════════════════════════════════════════════════════════════╗
# ║  boGDan Setup Script — Production-Grade One-Command Install     ║
# ║  Target: Raspberry Pi OS Lite 64-bit (bookworm) / Debian       ║
# ║  Run as: sudo bash scripts/setup.sh                            ║
# ╚══════════════════════════════════════════════════════════════════╝
set -euo pipefail

# ─── Version ────────────────────────────────────────────────────────
BOGDAN_SETUP_VERSION="0.1.0"

# ─── Paths (resolve repo root relative to this script) ──────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
CONFIG_DIR="${REPO_ROOT}/config"
LOG_FILE="/var/log/bogdan-setup.log"

# ─── Colors & Output ────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m' # No Color

log()       { echo -e "${GREEN}[BOGDAN]${NC} $*" | tee -a "$LOG_FILE"; }
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
        BACKUP_DIR="/var/backups/bogdan-setup-$(date +%Y%m%d-%H%M%S)"
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
${BOLD}boGDan Setup Script v${BOGDAN_SETUP_VERSION}${NC}

Usage: sudo bash scripts/setup.sh [OPTIONS]

Options:
  --skip-tor         Skip Tor configuration
  --skip-build       Skip building boGDan from source
  --skip-firewall    Skip firewall/iptables configuration
  --cross-compile    Cross-compile for aarch64 from x86_64 host
  --uninstall        Remove boGDan completely
  --help             Show this help message

Examples:
  sudo bash scripts/setup.sh                    # Full install
  sudo bash scripts/setup.sh --skip-build       # Install without building
  sudo bash scripts/setup.sh --cross-compile    # Cross-compile for Pi on x86
  sudo bash scripts/setup.sh --uninstall        # Remove boGDan
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
    echo "=== boGDan Setup Log — $(date) ===" > "$LOG_FILE"
    info "Logging to ${LOG_FILE}"
}

# ─── Uninstall ──────────────────────────────────────────────────────
do_uninstall() {
    echo ""
    log "${BOLD}╔══════════════════════════════════════════╗${NC}"
    log "${BOLD}║     boGDan Uninstall                    ║${NC}"
    log "${BOLD}╚══════════════════════════════════════════╝${NC}"
    echo ""

    # Stop services
    if systemctl is-active --quiet bogdan 2>/dev/null; then
        info "Stopping bogdan service..."
        systemctl stop bogdan || true
    fi
    if systemctl is-enabled --quiet bogdan 2>/dev/null; then
        info "Disabling bogdan service..."
        systemctl disable bogdan || true
    fi

    # Remove service file
    if [ -f /etc/systemd/system/bogdan.service ]; then
        rm -f /etc/systemd/system/bogdan.service
        systemctl daemon-reload
        info "Removed systemd service"
    fi

    # Remove binary
    if [ -f /usr/local/bin/bogdan-server ]; then
        rm -f /usr/local/bin/bogdan-server
        info "Removed /usr/local/bin/bogdan-server"
    fi

    # Remove user
    if id -u bogdan &>/dev/null; then
        userdel bogdan 2>/dev/null || true
        info "Removed bogdan user"
    fi

    # Remove data directory
    if [ -d /var/lib/bogdan ]; then
        rm -rf /var/lib/bogdan
        info "Removed /var/lib/bogdan"
    fi

    # Remove temp directory
    if [ -d /tmp/bogdan ]; then
        rm -rf /tmp/bogdan
        info "Removed /tmp/bogdan"
    fi

    # Optionally restore Tor config
    if [ -f /etc/tor/torrc ] && command -v tor &>/dev/null; then
        warn "Tor config at /etc/tor/torrc was modified by boGDan — review manually"
    fi

    echo ""
    log "${GREEN}boGDan has been uninstalled.${NC}"
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
        gstreamer1.0-alsa
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
        libgles-dev
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

# boGDan: Enable DRM/KMS with V3D GPU
dtoverlay=vc4-kms-v3d

# boGDan: Enable HEVC V4L2 decoder (for v2)
# dtoverlay=rpivid-v4l2

# boGDan: Disable WiFi and Bluetooth (reduce attack surface, save power)
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

# ─── Build boGDan ───────────────────────────────────────────────────
build_bogdan() {
    step_next
    step "Building boGDan..."

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

# ─── Install boGDan ─────────────────────────────────────────────────
install_bogdan() {
    step_next
    step "Installing boGDan..."

    # Determine binary path based on build target
    local binary_path
    if [ "$CROSS_COMPILE" = true ]; then
        binary_path="${REPO_ROOT}/target/aarch64-unknown-linux-gnu/release/bogdan"
    else
        binary_path="${REPO_ROOT}/target/release/bogdan"
    fi

    if [ "$SKIP_BUILD" = false ] && [ ! -f "$binary_path" ]; then
        error "Built binary not found at ${binary_path}"
        exit 1
    fi

    # Create bogdan user if it doesn't exist
    if ! id -u bogdan &>/dev/null; then
        useradd -r -m -s /usr/sbin/nologin bogdan
        info "Created bogdan system user"
    else
        info "bogdan user already exists"
    fi

    # Ensure user is in required groups (idempotent)
    usermod -aG video,render,audio bogdan
    info "bogdan user in groups: video, render, audio"

    # Install binary
    if [ "$SKIP_BUILD" = false ]; then
        cp "$binary_path" /usr/local/bin/bogdan-server
        chmod 755 /usr/local/bin/bogdan-server
        info "Installed binary to /usr/local/bin/bogdan-server"
    fi

    # Create data directory
    mkdir -p /var/lib/bogdan
    chown bogdan:bogdan /var/lib/bogdan
    info "Data directory: /var/lib/bogdan"

    # Create resolve cache directory (already under /var/lib/bogdan)
    # The SQLite cache file will be created automatically by the resolver.
    info "Resolve cache: /var/lib/bogdan/resolve-cache.db"

    # Generate self-signed TLS certificate for HTTPS/WSS
    local cert_dir="/etc/bogdan/tls"
    local cert_path="${cert_dir}/bogdan.pem"
    local key_path="${cert_dir}/bogdan-key.pem"
    if [ ! -f "$cert_path" ] || [ ! -f "$key_path" ]; then
        info "Generating self-signed TLS certificate..."
        mkdir -p "$cert_dir"
        # Get the Pi's hostname and IP for the SAN
        local hostname
        hostname="$(hostname 2>/dev/null || echo 'bogdan')" 
        local ip_addr
        ip_addr="$(hostname -I 2>/dev/null | cut -d' ' -f1 || echo '192.168.1.1')"
        openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 \
            -nodes -keyout "$key_path" -out "$cert_path" \
            -subj "/CN=boGDan/O=boGDan/C=US" \
            -addext "subjectAltName=DNS:${hostname}.local,DNS:${hostname},DNS:bogdan.local,IP:${ip_addr},IP:127.0.0.1" \
            2>/dev/null
        chmod 644 "$cert_path"
        chmod 600 "$key_path"
        chown bogdan:bogdan "$cert_path" "$key_path"
        info "TLS certificate generated: ${cert_path}"
        info "  SANs: ${hostname}.local, ${hostname}, bogdan.local, ${ip_addr}, 127.0.0.1"
    else
        info "TLS certificate already exists: ${cert_path}"
    fi

    # Create temp directory
    mkdir -p /tmp/bogdan/subs
    chown bogdan:bogdan /tmp/bogdan
    info "Temp directory: /tmp/bogdan"

    # Install provider configs
    mkdir -p /etc/bogdan/providers.d
    chown bogdan:bogdan /etc/bogdan/providers.d
    local providers_dir="${REPO_ROOT}/providers.d"
    if [ -d "$providers_dir" ]; then
        local provider_count=0
        for toml_file in "${providers_dir}"/*.toml; do
            if [ -f "$toml_file" ]; then
                local toml_name
                toml_name="$(basename "$toml_file")"
                if [ ! -f "/etc/bogdan/providers.d/${toml_name}" ]; then
                    cp "$toml_file" "/etc/bogdan/providers.d/${toml_name}"
                    chmod 644 "/etc/bogdan/providers.d/${toml_name}"
                    chown bogdan:bogdan "/etc/bogdan/providers.d/${toml_name}"
                    provider_count=$((provider_count + 1))
                    info "  Provider: ${toml_name}"
                else
                    info "  Provider: ${toml_name} (already exists, not overwriting)"
                    provider_count=$((provider_count + 1))
                fi
            fi
        done
        info "Installed ${provider_count} provider config(s) to /etc/bogdan/providers.d/"
    else
        warn "providers.d/ directory not found — provider configs not installed"
    fi

    # Install TOML config file
    mkdir -p /etc/bogdan
    # Prefer deploy/bogdan.toml (production Pi config) over bogdan.toml.example (generic)
    local toml_source=""
    if [ -f "${REPO_ROOT}/deploy/bogdan.toml" ]; then
        toml_source="${REPO_ROOT}/deploy/bogdan.toml"
    elif [ -f "${REPO_ROOT}/bogdan.toml.example" ]; then
        toml_source="${REPO_ROOT}/bogdan.toml.example"
    fi
    if [ -n "$toml_source" ]; then
        if [ ! -f /etc/bogdan/bogdan.toml ]; then
            cp "$toml_source" /etc/bogdan/bogdan.toml
            chown bogdan:bogdan /etc/bogdan/bogdan.toml
            chmod 644 /etc/bogdan/bogdan.toml
            # Add TLS paths to the config
            if ! grep -q "tls_cert_path" /etc/bogdan/bogdan.toml 2>/dev/null; then
                echo "" >> /etc/bogdan/bogdan.toml
                echo "# TLS certificate for HTTPS/WSS (self-signed by setup.sh)" >> /etc/bogdan/bogdan.toml
                echo "tls_cert_path = \"/etc/bogdan/tls/bogdan.pem\"" >> /etc/bogdan/bogdan.toml
                echo "tls_key_path = \"/etc/bogdan/tls/bogdan-key.pem\"" >> /etc/bogdan/bogdan.toml
            fi
            info "Installed config to /etc/bogdan/bogdan.toml (from $(basename "$toml_source"))"
        else
            info "Config already exists at /etc/bogdan/bogdan.toml — not overwriting"
            # Add TLS paths if missing from existing config
            if ! grep -q "tls_cert_path" /etc/bogdan/bogdan.toml 2>/dev/null; then
                echo "" >> /etc/bogdan/bogdan.toml
                echo "# TLS certificate for HTTPS/WSS (self-signed by setup.sh)" >> /etc/bogdan/bogdan.toml
                echo "tls_cert_path = \"/etc/bogdan/tls/bogdan.pem\"" >> /etc/bogdan/bogdan.toml
                echo "tls_key_path = \"/etc/bogdan/tls/bogdan-key.pem\"" >> /etc/bogdan/bogdan.toml
                info "Added TLS paths to existing config"
            fi
        fi
    else
        warn "No bogdan.toml found — skipping config installation"
    fi

    # Install systemd service (prefer deploy/ version with Pi-specific hardening)
    local service_source=""
    if [ -f "${REPO_ROOT}/deploy/bogdan.service" ]; then
        service_source="${REPO_ROOT}/deploy/bogdan.service"
    elif [ -f "${CONFIG_DIR}/bogdan.service" ]; then
        service_source="${CONFIG_DIR}/bogdan.service"
    fi
    if [ -n "$service_source" ]; then
        backup_file /etc/systemd/system/bogdan.service
        # Patch ExecStart to use bogdan-server
        sed 's|ExecStart=/usr/local/bin/bogdan|ExecStart=/usr/local/bin/bogdan-server|' \
            "$service_source" > /etc/systemd/system/bogdan.service
        chmod 644 /etc/systemd/system/bogdan.service
        systemctl daemon-reload
        systemctl enable bogdan
        info "Systemd service installed and enabled (from $(basename "$service_source"))"
    else
        warn "bogdan.service not found — skipping service installation"
    fi
}

# ─── Verification ───────────────────────────────────────────────────
verify_installation() {
    step_next
    step "Verifying installation..."

    local failed=0

    # Check binary
    if [ -x /usr/local/bin/bogdan-server ]; then
        info "  [OK] /usr/local/bin/bogdan-server exists and is executable"
    else
        error "  [FAIL] /usr/local/bin/bogdan-server not found or not executable"
        failed=1
    fi

    # Check that the binary can run (basic smoke test)
    if [ -x /usr/local/bin/bogdan-server ]; then
        if /usr/local/bin/bogdan-server --version &>/dev/null; then
            info "  [OK] bogdan-server --version succeeds"
        else
            # Binary might not support --version, try --help
            if /usr/local/bin/bogdan-server --help &>/dev/null; then
                info "  [OK] bogdan-server --help succeeds"
            else
                warn "  [WARN] Cannot verify bogdan-server runs (no --version/--help)"
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

    # Check boGDan service
    if systemctl is-enabled --quiet bogdan 2>/dev/null; then
        info "  [OK] bogdan service is enabled"
    else
        error "  [FAIL] bogdan service is not enabled"
        failed=1
    fi

    # Check user
    if id -u bogdan &>/dev/null; then
        info "  [OK] bogdan user exists"
    else
        error "  [FAIL] bogdan user does not exist"
        failed=1
    fi

    # Check data directory
    if [ -d /var/lib/bogdan ]; then
        info "  [OK] /var/lib/bogdan exists"
    else
        error "  [FAIL] /var/lib/bogdan does not exist"
        failed=1
    fi

    # Check firewall
    if [ "$SKIP_FIREWALL" = false ]; then
        if iptables -L INPUT 2>/dev/null | grep -q "8585"; then
            info "  [OK] Firewall rules include boGDan port 8585"
        else
            warn "  [WARN] Firewall rules may not include boGDan ports"
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
    echo -e "${CYAN}${BOLD}║   Setup v${BOGDAN_SETUP_VERSION}                                   ║${NC}"
    echo -e "${CYAN}${BOLD}║                                                   ║${NC}"
    echo -e "${CYAN}${BOLD}╚═══════════════════════════════════════════════════╝${NC}"
    echo ""
}

# ─── Print Summary ──────────────────────────────────────────────────
print_summary() {
    echo ""
    log "${GREEN}${BOLD}╔══════════════════════════════════════════╗${NC}"
    log "${GREEN}${BOLD}║   boGDan Setup Complete!                 ║${NC}"
    log "${GREEN}${BOLD}╚══════════════════════════════════════════╝${NC}"
    echo ""
    info "Next steps:"
    info "  1. Reboot to apply kernel overlay:  ${BOLD}sudo reboot${NC}"
    info "  2. Start boGDan:                    ${BOLD}sudo systemctl start bogdan${NC}"
    info "  3. Check status:                    ${BOLD}sudo systemctl status bogdan${NC}"
    info "  4. View logs:                       ${BOLD}journalctl -u bogdan -f${NC}"
    echo ""
    info "  Cast from browser: Install the extension from src/extension/"
    info "  Cast from VLC:     Playback → Renderer → boGDan"
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
    build_bogdan
    install_bogdan
    verify_installation
    print_summary
}

main "$@"
