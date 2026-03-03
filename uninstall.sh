#!/bin/sh
# scx_cognis uninstallation script
#
# Cleanly stops the scx service, removes the scx_cognis binary, and (optionally)
# reverts /etc/default/scx to the previous scheduler so the system does not boot
# into a missing scheduler after removal.
#
# Usage: sudo sh uninstall.sh [options]
#
# Options:
#   --force       Skip all confirmation prompts
#   --dry-run     Print every action that would be taken; make no changes
#   --purge       Also remove the scx.service file (only safe if no other
#                 sched_ext scheduler is installed)
#   --help, -h    Print this help text and exit

set -e

# ─── Tunables ──────────────────────────────────────────────────────────────────
BINARY_NAME="scx_cognis"
SERVICE_NAME="scx"
BINARY_PATH="/usr/bin/${BINARY_NAME}"
SCX_DEFAULTS="/etc/default/scx"
SYSTEMD_SERVICE="/etc/systemd/system/${SERVICE_NAME}.service"

FORCE=""
DRY_RUN=""
PURGE=""

# ─── CLI parsing ───────────────────────────────────────────────────────────────
while [ "$#" -gt 0 ]; do
    case "$1" in
        --force)    FORCE="1";    shift ;;
        --dry-run)  DRY_RUN="1"; shift ;;
        --purge)    PURGE="1";   shift ;;
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

# ─── Service stop ─────────────────────────────────────────────────────────────
stop_scx_service() {
    if ! command -v systemctl >/dev/null 2>&1; then
        log_warn "systemctl not found — skipping service stop"
        return 0
    fi

    # Only act if the managed service unit actually exists
    if ! systemctl list-unit-files "${SERVICE_NAME}.service" 2>/dev/null | grep -q "${SERVICE_NAME}"; then
        log_info "scx service not found — nothing to stop"
        return 0
    fi

    log_info "Stopping and disabling ${SERVICE_NAME}.service ..."
    run "systemctl stop    '${SERVICE_NAME}' 2>/dev/null || true"
    run "systemctl disable '${SERVICE_NAME}' 2>/dev/null || true"
    log_ok "${SERVICE_NAME}.service stopped and disabled"
}

# ─── /etc/default/scx cleanup ────────────────────────────────────────────────
revert_scx_defaults() {
    if [ ! -f "$SCX_DEFAULTS" ]; then
        log_info "${SCX_DEFAULTS} not found — nothing to revert"
        return 0
    fi

    # Check if the file references scx_cognis at all
    if ! grep -q "SCX_SCHEDULER=${BINARY_NAME}" "$SCX_DEFAULTS" 2>/dev/null; then
        log_info "${SCX_DEFAULTS} does not reference ${BINARY_NAME} — leaving intact"
        return 0
    fi

    # Restore backup if the installer left one
    if [ -f "${SCX_DEFAULTS}.bak" ]; then
        log_info "Restoring ${SCX_DEFAULTS} from backup ..."
        run "cp '${SCX_DEFAULTS}.bak' '${SCX_DEFAULTS}'"
        run "rm -f '${SCX_DEFAULTS}.bak'"
        log_ok "${SCX_DEFAULTS} restored from backup"
        return 0
    fi

    # No backup — remove only the lines that scx_cognis owns
    log_info "Removing ${BINARY_NAME} entries from ${SCX_DEFAULTS} ..."
    if [ -n "$DRY_RUN" ]; then
        log_info "(dry-run) Would remove SCX_SCHEDULER/SCX_FLAGS lines referencing ${BINARY_NAME}"
        return 0
    fi

    _tmp=$(mktemp)
    grep -v "Managed by scx_cognis installer" "$SCX_DEFAULTS" \
        | grep -v "^SCX_SCHEDULER=${BINARY_NAME}" \
        | grep -v "^SCX_FLAGS="                   \
        > "$_tmp" || true
    cp "$_tmp" "$SCX_DEFAULTS"
    rm -f "$_tmp"
    log_ok "${SCX_DEFAULTS} cleaned up"
}

# ─── Binary and optional service file removal ─────────────────────────────────
remove_files() {
    _removed=0

    remove_if_exists() {
        if [ -f "$1" ] || [ -d "$1" ]; then
            log_info "Removing $1"
            run "rm -rf '$1'"
            _removed=$((_removed + 1))
        fi
    }

    remove_if_exists "$BINARY_PATH"

    if [ -n "$PURGE" ]; then
        # Only remove the service file if it was created by our installer
        # (detected by the Description line we wrote) to avoid destroying a
        # distro-managed scx-manager service file.
        if grep -q "scx_cognis installer\|galpt/scx_cognis" "$SYSTEMD_SERVICE" 2>/dev/null; then
            remove_if_exists "$SYSTEMD_SERVICE"
        else
            log_warn "--purge requested but ${SYSTEMD_SERVICE} was not created by this"
            log_warn "          installer (it may be owned by scx-manager). Leaving it intact."
        fi
    fi

    if [ "$_removed" -eq 0 ]; then
        log_warn "No files were found to remove (already uninstalled?)"
    fi
}

# ─── Post-removal systemd reload ─────────────────────────────────────────────
reload_systemd() {
    if command -v systemctl >/dev/null 2>&1; then
        run "systemctl daemon-reload 2>/dev/null || true"
    fi
}

# ─── Main ─────────────────────────────────────────────────────────────────────
main() {
    log_step "scx_cognis uninstaller"
    check_root

    [ -n "$DRY_RUN" ] && log_warn "DRY-RUN mode — no changes will be made"
    [ -n "$PURGE"   ] && log_warn "PURGE mode — service file will be removed if safe"

    confirm "This will stop the scx service and remove scx_cognis. Continue?" || {
        log_info "Aborted."
        exit 0
    }

    # ── 1. Stop the running scheduler ────────────────────────────────────────
    log_step "Stopping scx service"
    stop_scx_service

    # ── 2. Revert /etc/default/scx ───────────────────────────────────────────
    log_step "Reverting scheduler configuration"
    revert_scx_defaults

    # ── 3. Remove binary (and optionally service file) ────────────────────────
    log_step "Removing files"
    remove_files

    # ── 4. Reload systemd ─────────────────────────────────────────────────────
    reload_systemd

    # ── Summary ──────────────────────────────────────────────────────────────
    log_step "Done"
    log_ok "scx_cognis has been removed."
    echo ""
    log_info "The kernel has reverted to its default scheduler (CFS/EEVDF)."
    if [ -z "$PURGE" ] && [ -f "$SYSTEMD_SERVICE" ]; then
        log_info "The scx service file is still present at ${SYSTEMD_SERVICE}."
        log_info "To also remove it, re-run with:  sudo sh uninstall.sh --purge"
    fi
    echo ""
}

main "$@"
