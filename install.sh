#!/bin/sh
# scx_cognis installation script
#
# Supported distros : CachyOS, Arch Linux, Ubuntu 24.04+, Debian 12+,
#                     and any other systemd-based Linux with a sched_ext kernel.
# Supported arches  : x86_64 (pre-built); use --build-from-source for others.
# Usage             : sudo sh install.sh [options]
#
# Options:
#   --version TAG         Install a specific release tag, e.g. v0.1.5
#                         (default: latest)
#   --build-from-source   Compile the binary locally instead of downloading
#   --dry-run             Print every action that would be taken; make no changes
#   --force               Skip all confirmation prompts
#   --flags "..."         Custom scheduler flags written to /etc/default/scx
#                         (default: empty — scheduler uses its own defaults)
#   --help, -h            Print this help text and exit

set -e

# ─── Tunables ──────────────────────────────────────────────────────────────────
BINARY_NAME="scx_cognis"
SERVICE_NAME="scx"
BINARY_PATH="/usr/bin/${BINARY_NAME}"
SCX_DEFAULTS="/etc/default/scx"
SYSTEMD_SERVICE="/etc/systemd/system/${SERVICE_NAME}.service"
REPO_URL="https://github.com/galpt/scx_cognis"
TARBALL_NAME="${BINARY_NAME}-linux-x86_64.tar.gz"

VERSION="latest"
BUILD_FROM_SOURCE=""
DRY_RUN=""
FORCE=""
SCX_FLAGS="${SCX_FLAGS:-}"

# ─── CLI parsing ───────────────────────────────────────────────────────────────
while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)            VERSION="$2";          shift 2 ;;
        --build-from-source)  BUILD_FROM_SOURCE="1"; shift   ;;
        --dry-run)            DRY_RUN="1";           shift   ;;
        --force)              FORCE="1";             shift   ;;
        --flags)              SCX_FLAGS="$2";        shift 2 ;;
        --help|-h)
            sed -n '/^# Options:/,/^[^#]/p' "$0" | grep '^#' | sed 's/^# \{0,2\}//'
            exit 0
            ;;
        *) printf '[ERR ] Unknown option: %s\n' "$1" >&2; exit 1 ;;
    esac
done

# ─── Colour helpers ────────────────────────────────────────────────────────────
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
BLUE='\033[0;34m'; CYAN='\033[0;36m'; BOLD='\033[1m'; NC='\033[0m'

log_info()  { printf "${BLUE}[INFO]${NC}  %s\n"          "$1"; }
log_ok()    { printf "${GREEN}[ OK ]${NC}  %s\n"          "$1"; }
log_warn()  { printf "${YELLOW}[WARN]${NC}  %s\n"         "$1"; }
log_error() { printf "${RED}[ERR ]${NC}  %s\n"            "$1" >&2; }
log_step()  { printf "\n${BOLD}${CYAN}──── %s ────${NC}\n" "$1"; }

# In dry-run mode, prefix every mutating command with a no-op echo.
run() {
    if [ -n "$DRY_RUN" ]; then
        printf "${YELLOW}[DRY ]${NC}  %s\n" "$*"
    else
        eval "$@"
    fi
}

# ─── Pre-flight ────────────────────────────────────────────────────────────────
check_root() {
    if [ "$(id -u)" -ne 0 ]; then
        log_error "Run as root:  sudo sh $0 $*"
        exit 1
    fi
}

confirm() {
    [ -n "$FORCE" ] && return 0
    printf "%s [y/N]: " "$1"
    read -r _ans
    case "$_ans" in y|Y) return 0 ;; *) return 1 ;; esac
}

# ─── Architecture detection ───────────────────────────────────────────────────
detect_arch() {
    _m=$(uname -m)
    case "$_m" in
        x86_64)
            echo "linux-x86_64"
            ;;
        *)
            if [ -n "$BUILD_FROM_SOURCE" ]; then
                echo "source"
            else
                log_error "Pre-built binaries are only available for x86_64."
                log_info  "Re-run with --build-from-source to compile for ${_m}."
                exit 1
            fi
            ;;
    esac
}

# ─── Distro detection ─────────────────────────────────────────────────────────
detect_distro() {
    if [ -f /etc/os-release ]; then
        # shellcheck source=/dev/null
        . /etc/os-release
        case "${ID:-}" in
            cachyos)             echo "cachyos";  return ;;
            arch)                echo "arch";     return ;;
            ubuntu)              echo "ubuntu";   return ;;
            debian)              echo "debian";   return ;;
        esac
        # Fall through to ID_LIKE for derivatives (Manjaro, EndeavourOS, etc.)
        case "${ID_LIKE:-}" in
            *arch*)              echo "arch";     return ;;
            *ubuntu*|*debian*)   echo "debian";   return ;;
        esac
    fi
    echo "generic"
}

# ─── Kernel sched_ext capability ──────────────────────────────────────────────
check_sched_ext_support() {
    # Method 1: /boot/config-<kernel> — present on most distros
    _cfg="/boot/config-$(uname -r)"
    if [ -f "$_cfg" ] && grep -q "CONFIG_SCHED_CLASS_EXT=y" "$_cfg" 2>/dev/null; then
        return 0
    fi
    # Method 2: /proc/config.gz — optional kernel feature
    if command -v zcat >/dev/null 2>&1 \
       && zcat /proc/config.gz 2>/dev/null | grep -q "CONFIG_SCHED_CLASS_EXT=y"; then
        return 0
    fi
    # Method 3: /sys/kernel/sched_ext exists once the subsystem is initialised
    if [ -d /sys/kernel/sched_ext ]; then
        return 0
    fi
    return 1
}

# ─── Dependency installation ──────────────────────────────────────────────────

install_deps_arch() {
    log_info "Installing build/runtime dependencies via pacman..."
    _pkgs="clang llvm libbpf libelf zlib libseccomp pkg-config"
    run "pacman -Syu --noconfirm"
    run "pacman -S --noconfirm --needed ${_pkgs}"
    log_ok "Dependencies installed"
}

install_deps_debian() {
    log_info "Installing build/runtime dependencies via apt..."
    run "apt-get update -qq"
    run "apt-get install -y --no-install-recommends \
        clang llvm libclang-dev libbpf-dev libelf-dev \
        zlib1g-dev libseccomp-dev pkg-config curl"
    log_ok "Dependencies installed"
}

install_rust_if_missing() {
    if command -v cargo >/dev/null 2>&1; then
        log_ok "Rust toolchain already present: $(cargo --version)"
        return 0
    fi
    log_info "Installing Rust via rustup..."
    run "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable"
    # shellcheck source=/dev/null
    run ". \"\$HOME/.cargo/env\""
    log_ok "Rust installed: $(cargo --version 2>/dev/null || echo '(reload shell)')"
}

# ─── Binary: download from GitHub Releases ────────────────────────────────────
download_binary() {
    _ver="$1"    # "latest" or "vX.Y.Z"

    if [ "$_ver" = "latest" ]; then
        _url="${REPO_URL}/releases/latest/download/${TARBALL_NAME}"
    else
        _url="${REPO_URL}/releases/download/${_ver}/${TARBALL_NAME}"
    fi

    log_info "Downloading ${TARBALL_NAME} from ${_url} ..."
    _tmp=$(mktemp -d)
    # shellcheck disable=SC2064
    trap "rm -rf '$_tmp'" EXIT INT TERM

    if command -v curl >/dev/null 2>&1; then
        run "curl -fsSL \"${_url}\" -o \"${_tmp}/${TARBALL_NAME}\""
    elif command -v wget >/dev/null 2>&1; then
        run "wget -q \"${_url}\" -O \"${_tmp}/${TARBALL_NAME}\""
    else
        log_error "Neither curl nor wget found. Install one and retry."
        exit 1
    fi

    [ -n "$DRY_RUN" ] && { log_ok "(dry-run) Skipping extract/install"; return; }

    tar -xzf "${_tmp}/${TARBALL_NAME}" -C "$_tmp" || {
        log_error "Failed to extract archive."
        exit 1
    }

    cp "${_tmp}/${BINARY_NAME}" "${BINARY_PATH}"
    chmod 755 "${BINARY_PATH}"
    log_ok "Binary installed: ${BINARY_PATH}"
}

# ─── Binary: build from source ────────────────────────────────────────────────
build_from_source() {
    _distro="$1"
    log_info "Building ${BINARY_NAME} from source..."

    case "$_distro" in
        cachyos|arch)   install_deps_arch ;;
        ubuntu|debian)  install_deps_debian ;;
        *)
            log_warn "Unknown distro. Attempting build without installing dependencies."
            ;;
    esac

    install_rust_if_missing

    # Ensure cargo is on PATH (rustup may have just added it)
    export PATH="${HOME}/.cargo/bin:${PATH}"

    _src=$(cd "$(dirname "$0")"; pwd)
    if [ ! -f "${_src}/Cargo.toml" ]; then
        log_error "Cargo.toml not found in ${_src}."
        log_info  "Run install.sh from the repository root, or use --version to download a pre-built binary."
        exit 1
    fi

    run "cargo build --release --manifest-path \"${_src}/Cargo.toml\""
    run "cp \"${_src}/target/release/${BINARY_NAME}\" \"${BINARY_PATH}\""
    run "chmod 755 \"${BINARY_PATH}\""
    log_ok "Built and installed: ${BINARY_PATH}"
}

# ─── scx-manager package (Arch/CachyOS) ──────────────────────────────────────
# scx-manager provides /etc/systemd/system/scx.service and /etc/default/scx.
ensure_scx_manager_arch() {
    if pacman -Q scx-manager >/dev/null 2>&1; then
        log_ok "scx-manager already installed"
        return 0
    fi
    log_info "Installing scx-manager (provides the scx systemd service)..."
    run "pacman -S --noconfirm --needed scx-manager" && return 0

    # Fallback: scx-manager may live in AUR on plain Arch
    log_warn "scx-manager not in official repos — checking AUR helpers..."
    for _aur in yay paru pikaur; do
        if command -v "$_aur" >/dev/null 2>&1; then
            log_info "Installing via ${_aur}..."
            run "${_aur} -S --noconfirm scx-manager"
            return 0
        fi
    done

    log_warn "Could not install scx-manager automatically (no AUR helper found)."
    log_warn "Install it manually (e.g. yay -S scx-manager) and then re-run this script,"
    log_warn "or the installer will create the service file for you now."
    return 1
}

# ─── scx.service (Ubuntu/Debian or fallback) ─────────────────────────────────
install_scx_service_file() {
    log_info "Writing ${SYSTEMD_SERVICE} ..."
    if [ -n "$DRY_RUN" ]; then
        log_info "(dry-run) Would write scx.service"
        return
    fi
    cat > "${SYSTEMD_SERVICE}" <<'SVCEOF'
[Unit]
Description=Start scx_cognis sched_ext scheduler
Documentation=https://github.com/galpt/scx_cognis
ConditionPathIsDirectory=/sys/kernel/sched_ext
After=multi-user.target
StartLimitIntervalSec=30
StartLimitBurst=2

[Service]
Type=simple
EnvironmentFile=/etc/default/scx
ExecStart=/bin/sh -c 'exec ${SCX_SCHEDULER_OVERRIDE:-$SCX_SCHEDULER} ${SCX_FLAGS_OVERRIDE:-$SCX_FLAGS}'
Restart=on-failure
RestartSec=5
StandardOutput=journal
StandardError=journal
SyslogIdentifier=scx

[Install]
WantedBy=multi-user.target
SVCEOF
    log_ok "Service file written: ${SYSTEMD_SERVICE}"
}

# ─── /etc/default/scx configuration ─────────────────────────────────────────
configure_scx_defaults() {
    _flags="${SCX_FLAGS:-}"
    log_info "Configuring ${SCX_DEFAULTS} ..."

    if [ -f "$SCX_DEFAULTS" ] && grep -q "SCX_SCHEDULER=${BINARY_NAME}" "$SCX_DEFAULTS" 2>/dev/null; then
        log_ok "${SCX_DEFAULTS} already points to ${BINARY_NAME}"
        return 0
    fi

    if [ -f "$SCX_DEFAULTS" ]; then
        log_info "Backing up existing ${SCX_DEFAULTS} → ${SCX_DEFAULTS}.bak"
        run "cp \"${SCX_DEFAULTS}\" \"${SCX_DEFAULTS}.bak\""
    fi

    if [ -n "$DRY_RUN" ]; then
        log_info "(dry-run) Would write SCX_SCHEDULER=${BINARY_NAME} to ${SCX_DEFAULTS}"
        return
    fi

    # Preserve any unrelated settings already in the file.
    _tmp_cfg=$(mktemp)
    if [ -f "$SCX_DEFAULTS" ]; then
        # Strip only the lines we are going to rewrite.
        grep -v "^SCX_SCHEDULER=" "$SCX_DEFAULTS" \
            | grep -v "^SCX_FLAGS="    \
            > "$_tmp_cfg" || true
    fi
    cat >> "$_tmp_cfg" <<EOF

# Managed by scx_cognis installer — do not edit these two lines manually.
SCX_SCHEDULER=${BINARY_NAME}
SCX_FLAGS='${_flags}'
EOF
    cp "$_tmp_cfg" "$SCX_DEFAULTS"
    rm -f "$_tmp_cfg"
    log_ok "${SCX_DEFAULTS} updated"
}

# ─── Enable and start the scx service ────────────────────────────────────────
enable_scx_service() {
    log_info "Reloading systemd and enabling scx.service ..."
    run "systemctl daemon-reload"
    run "systemctl enable ${SERVICE_NAME}"
    run "systemctl restart ${SERVICE_NAME}"
    log_ok "scx.service is enabled and running"

    if [ -z "$DRY_RUN" ] && ! systemctl is-active --quiet "${SERVICE_NAME}"; then
        log_warn "scx.service did not start cleanly; check: journalctl -u scx -e"
    fi
}

# ─── Main ─────────────────────────────────────────────────────────────────────
main() {
    log_step "scx_cognis installer"
    check_root

    _arch=$(detect_arch)
    _distro=$(detect_distro)

    log_info "Architecture : ${_arch}"
    log_info "Distribution : ${_distro}"
    log_info "Version      : ${VERSION}"
    [ -n "$DRY_RUN"            ] && log_warn "DRY-RUN mode — no changes will be made"
    [ -n "$BUILD_FROM_SOURCE"  ] && log_info "Will build from source"

    # ── sched_ext kernel check (advisory — warn, do not abort) ──────────────
    log_step "Checking kernel sched_ext support"
    if check_sched_ext_support; then
        log_ok "Kernel $(uname -r) reports CONFIG_SCHED_CLASS_EXT=y"
    else
        log_warn "Could not confirm CONFIG_SCHED_CLASS_EXT=y in kernel $(uname -r)."
        log_warn "scx_cognis will not run unless you boot a sched_ext-enabled kernel."
        log_warn "  • CachyOS  : all editions ship linux-cachyos (sched_ext enabled by default)"
        log_warn "  • Arch AUR : linux-sched-ext or linux-cachyos-bin"
        log_warn "  • Upstream : any kernel ≥ 6.12 built with CONFIG_SCHED_CLASS_EXT=y"
        confirm "Continue installation anyway?" || { log_info "Aborted."; exit 0; }
    fi

    # ── Binary install ───────────────────────────────────────────────────────
    log_step "Installing binary"
    if [ -n "$BUILD_FROM_SOURCE" ]; then
        build_from_source "$_distro"
    else
        download_binary "$VERSION"
    fi

    # ── scx service setup ────────────────────────────────────────────────────
    log_step "Configuring scx service"

    _service_installed=0
    case "$_distro" in
        cachyos|arch)
            if ensure_scx_manager_arch; then
                _service_installed=1
            else
                install_scx_service_file
                _service_installed=1
            fi
            ;;
        *)
            # On Ubuntu/Debian there is no distro package for the scx service.
            # We install our own service file unless the user already has one.
            if [ -f "$SYSTEMD_SERVICE" ]; then
                log_ok "Existing ${SYSTEMD_SERVICE} found — will not overwrite"
            else
                install_scx_service_file
            fi
            _service_installed=1
            ;;
    esac

    configure_scx_defaults
    [ "$_service_installed" -eq 1 ] && enable_scx_service

    # ── Summary ──────────────────────────────────────────────────────────────
    log_step "Done"
    log_ok "scx_cognis has been installed."
    echo ""
    log_info "Binary      : ${BINARY_PATH}"
    log_info "Defaults    : ${SCX_DEFAULTS}"
    log_info "Service     : systemctl status scx"
    log_info "Logs        : journalctl -u scx -f"
    log_info "Switch back : sudo systemctl stop scx    (reverts to default kernel scheduler)"
    echo ""
}

main "$@"
