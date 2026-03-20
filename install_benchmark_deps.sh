#!/usr/bin/env bash
# install_benchmark_deps.sh — Best-effort bootstrap for Cognis benchmark helpers.

set -euo pipefail

RED=$(printf '\033[0;31m')
GRN=$(printf '\033[0;32m')
YLW=$(printf '\033[1;33m')
CYN=$(printf '\033[0;36m')
BLD=$(printf '\033[1m')
RST=$(printf '\033[0m')

say()  { printf "${BLD}${CYN}[bench-deps]${RST} %s\n" "$1"; }
ok()   { printf "${BLD}${GRN}[  OK  ]${RST} %s\n" "$1"; }
warn() { printf "${BLD}${YLW}[ WARN ]${RST} %s\n" "$1"; }
err()  { printf "${BLD}${RED}[ERROR ]${RST} %s\n" "$1" >&2; }

INSTALL_MINI=0
INSTALL_PLOTTER=0
REMOVE_MINI=0
REMOVE_PLOTTER=0
REMOVE_WORKDIR=0
REMOVE_RESULTS=0
MINI_LOCAL_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/scx_cognis/mini-benchmarker"
MINI_LOCAL_SCRIPT="$MINI_LOCAL_DIR/mini-benchmarker.sh"
MINI_SOURCE_URL="https://gitlab.com/torvic9/mini-benchmarker/-/raw/master/mini-benchmarker.sh"
PLOTTER_VENV_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/scx_cognis/mini-benchmarker-venv"
MINI_WORKDIR="${XDG_CACHE_HOME:-$HOME/.cache}/scx_cognis/mini-benchmarker-workdir"
RESULTS_ROOT="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/benchmark-results"

usage() {
    cat <<'EOF'
Usage: ./install_benchmark_deps.sh [options]

Best-effort bootstrap for benchmark helper dependencies.

Options:
  --mini-benchmarker   Try to install Mini Benchmarker when a supported path exists
  --plotter            Install Python matplotlib dependencies for chart rendering
  --remove-mini-benchmarker
                       Remove the fetched local Mini Benchmarker script
  --remove-plotter     Remove the local matplotlib virtualenv
  --remove-workdir     Remove the Mini Benchmarker asset/work directory
  --remove-results     Remove generated mini-benchmarker result directories
  --remove-all         Remove all benchmark helper leftovers above
  -h, --help           Show this help
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --mini-benchmarker)
            INSTALL_MINI=1
            shift
            ;;
        --plotter)
            INSTALL_PLOTTER=1
            shift
            ;;
        --remove-mini-benchmarker)
            REMOVE_MINI=1
            shift
            ;;
        --remove-plotter)
            REMOVE_PLOTTER=1
            shift
            ;;
        --remove-workdir)
            REMOVE_WORKDIR=1
            shift
            ;;
        --remove-results)
            REMOVE_RESULTS=1
            shift
            ;;
        --remove-all)
            REMOVE_MINI=1
            REMOVE_PLOTTER=1
            REMOVE_WORKDIR=1
            REMOVE_RESULTS=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            err "Unknown option: $1"
            usage
            exit 1
            ;;
    esac
done

if [ "$INSTALL_MINI" -eq 0 ] && [ "$INSTALL_PLOTTER" -eq 0 ] && \
   [ "$REMOVE_MINI" -eq 0 ] && [ "$REMOVE_PLOTTER" -eq 0 ] && \
   [ "$REMOVE_WORKDIR" -eq 0 ] && [ "$REMOVE_RESULTS" -eq 0 ]; then
    usage
    exit 0
fi

run_privileged() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        sudo "$@"
    fi
}

detect_distro() {
    if [ -r /etc/os-release ]; then
        . /etc/os-release
        printf '%s\n' "${ID:-unknown}"
        return
    fi
    printf '%s\n' unknown
}

install_plotter() {
    command -v python3 >/dev/null 2>&1 || {
        err "python3 is required to install plotter dependencies."
        exit 1
    }
    say "Installing matplotlib into $PLOTTER_VENV_DIR"
    python3 -m venv "$PLOTTER_VENV_DIR"
    # shellcheck disable=SC1090
    . "$PLOTTER_VENV_DIR/bin/activate"
    pip install --quiet matplotlib
    ok "Plotter environment ready at $PLOTTER_VENV_DIR"
}

patch_mini_benchmarker_script() {
    [ -f "$MINI_LOCAL_SCRIPT" ] || return 0

    if grep -q 'MB_TIME_BIN=' "$MINI_LOCAL_SCRIPT"; then
        ok "Mini Benchmarker compatibility patch already present"
        return 0
    fi

    say "Patching Mini Benchmarker for portable GNU time lookup"
    sed -i '/^TMP="\/tmp"$/a\
MB_TIME_BIN=""\
for candidate in /usr/bin/time /bin/time /usr/local/bin/time /opt/homebrew/bin/gtime /usr/local/bin/gtime; do\
\tif [ -x "$candidate" ]; then\
\t\tMB_TIME_BIN="$candidate"\
\t\tbreak\
\tfi\
done\
[[ -z "$MB_TIME_BIN" ]] && echo "GNU time executable not found. Please install the time package." && exit 3\
' "$MINI_LOCAL_SCRIPT"
    sed -i 's#/usr/bin/time #$MB_TIME_BIN #g' "$MINI_LOCAL_SCRIPT"
    ok "Applied local compatibility patch to $MINI_LOCAL_SCRIPT"
}

fetch_mini_benchmarker_script() {
    mkdir -p "$MINI_LOCAL_DIR"
    if command -v curl >/dev/null 2>&1; then
        say "Fetching Mini Benchmarker from $MINI_SOURCE_URL"
        curl -L --fail --silent --show-error "$MINI_SOURCE_URL" -o "$MINI_LOCAL_SCRIPT"
    elif command -v wget >/dev/null 2>&1; then
        say "Fetching Mini Benchmarker from $MINI_SOURCE_URL"
        wget -qO "$MINI_LOCAL_SCRIPT" "$MINI_SOURCE_URL"
    else
        err "Need curl or wget to fetch Mini Benchmarker."
        exit 1
    fi
    chmod +x "$MINI_LOCAL_SCRIPT"
    patch_mini_benchmarker_script
    ok "Installed Mini Benchmarker to $MINI_LOCAL_SCRIPT"
}

install_mini_benchmarker() {
    local distro
    distro=$(detect_distro)

    case "$distro" in
        cachyos|arch)
            if command -v pacman >/dev/null 2>&1; then
                warn "Mini Benchmarker is not guaranteed to be in the standard repos."
                warn "Preferred path on Arch-derived systems is an AUR helper or the local fetched copy."
                say "Trying common benchmark dependencies from pacman first."
                run_privileged pacman -S --needed --noconfirm \
                    python python-pip python-matplotlib stress-ng perf blender x265 argon2 \
                    wget git p7zip primesieve inxi bc unzip xz gcc make cmake nasm time || true
                fetch_mini_benchmarker_script
                return
            fi
            ;;
        ubuntu|debian)
            if command -v apt-get >/dev/null 2>&1; then
                say "Installing common benchmark dependencies via apt"
                run_privileged apt-get update -qq
                run_privileged apt-get install -y --no-install-recommends \
                    python3 python3-venv python3-pip python3-matplotlib stress-ng linux-perf \
                    blender xz-utils wget git p7zip-full build-essential cmake nasm bc unzip \
                    time inxi || true
                fetch_mini_benchmarker_script
                return
            fi
            ;;
    esac

    warn "No supported automatic package install path for this distro."
    fetch_mini_benchmarker_script
}

remove_tree() {
    local path="$1"
    if [ -e "$path" ]; then
        rm -rf -- "$path"
        ok "Removed $path"
    else
        warn "Nothing to remove at $path"
    fi
}

remove_results() {
    local found=0
    local result_dir
    if [ -d "$RESULTS_ROOT" ]; then
        for result_dir in "$RESULTS_ROOT"/mini-benchmarker-*; do
            if [ -e "$result_dir" ]; then
                found=1
                rm -rf -- "$result_dir"
                ok "Removed $result_dir"
            fi
        done
    fi
    if [ "$found" -eq 0 ]; then
        warn "No mini-benchmarker result directories found under $RESULTS_ROOT"
    fi
}

if [ "$INSTALL_PLOTTER" -eq 1 ]; then
    install_plotter
fi

if [ "$INSTALL_MINI" -eq 1 ]; then
    install_mini_benchmarker
fi

if [ "$REMOVE_MINI" -eq 1 ]; then
    remove_tree "$MINI_LOCAL_DIR"
fi

if [ "$REMOVE_PLOTTER" -eq 1 ]; then
    remove_tree "$PLOTTER_VENV_DIR"
fi

if [ "$REMOVE_WORKDIR" -eq 1 ]; then
    remove_tree "$MINI_WORKDIR"
fi

if [ "$REMOVE_RESULTS" -eq 1 ]; then
    remove_results
fi
