#!/usr/bin/env bash
# mini_benchmarker.sh — Automated baseline vs Cognis comparison using torvic9's
# Mini Benchmarker. The external benchmark tool is required separately.

set -euo pipefail

RED=$(printf '\033[0;31m')
GRN=$(printf '\033[0;32m')
YLW=$(printf '\033[1;33m')
CYN=$(printf '\033[0;36m')
BLD=$(printf '\033[1m')
RST=$(printf '\033[0m')

say()  { printf "${BLD}${CYN}[mini-bench]${RST} %s\n" "$1"; }
ok()   { printf "${BLD}${GRN}[  OK  ]${RST} %s\n" "$1"; }
warn() { printf "${BLD}${YLW}[ WARN ]${RST} %s\n" "$1"; }
err()  { printf "${BLD}${RED}[ERROR ]${RST} %s\n" "$1" >&2; }

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
RESULTS_DIR="$SCRIPT_DIR/benchmark-results/mini-benchmarker-$(date +%Y%m%d-%H%M%S)"
WORKDIR="${XDG_CACHE_HOME:-$HOME/.cache}/scx_cognis/mini-benchmarker-workdir"
MINI_LOCAL_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/scx_cognis/mini-benchmarker"
MINI_LOCAL_SCRIPT="$MINI_LOCAL_DIR/mini-benchmarker.sh"
MODE="desktop"
RUNS=1
DROP_CACHES=0
MINI_BENCHMARKER_CMD="${MINI_BENCHMARKER_CMD:-}"
PLOTTER="$SCRIPT_DIR/mini_benchmarker_plot.py"
PLOTTER_PYTHON="${PLOTTER_PYTHON:-python3}"
BOOTSTRAP_PLOTTER=0
CHECK_DEPS_ONLY=0
SCX_BIN=""
SCX_LAUNCHED=0
INITIAL_COGNIS_ACTIVE=0
INITIAL_SERVICE_ACTIVE=0
RESTORE_NEEDED=0
SUDO_KEEPALIVE_PID=""
BASELINE_LABEL=""
POWER_PROFILE="unknown"

usage() {
    cat <<EOF
Usage: ./mini_benchmarker.sh [options]

Automate Mini Benchmarker runs for:
  1. Baseline (no scx_cognis)
  2. Cognis (--mode desktop or --mode server)

Options:
  --workdir DIR          Mini Benchmarker asset/work directory
  --results-dir DIR      Directory for copied logs, chart, and CSV summary
  --mode desktop|server  Cognis profile for the scheduler run (default: desktop)
  --runs N               Number of repeated runs per variant (default: 1)
  --drop-caches          Answer "yes" to Mini Benchmarker page-cache prompt
  --mini-cmd PATH        Path to mini-benchmarker.sh
  --bootstrap-plotter    Create a local venv with matplotlib if needed
  --check-deps           Report benchmark prerequisites and exit
  -h, --help             Show this help

Environment overrides:
  MINI_BENCHMARKER_CMD   Same as --mini-cmd
  PLOTTER_PYTHON         Python interpreter used for chart generation
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --workdir)
            WORKDIR="$2"
            shift 2
            ;;
        --results-dir)
            RESULTS_DIR="$2"
            shift 2
            ;;
        --mode)
            MODE="$2"
            shift 2
            ;;
        --runs)
            RUNS="$2"
            shift 2
            ;;
        --drop-caches)
            DROP_CACHES=1
            shift
            ;;
        --mini-cmd)
            MINI_BENCHMARKER_CMD="$2"
            shift 2
            ;;
        --bootstrap-plotter)
            BOOTSTRAP_PLOTTER=1
            shift
            ;;
        --check-deps)
            CHECK_DEPS_ONLY=1
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

case "$MODE" in
    desktop|server) ;;
    *)
        err "Unsupported mode '$MODE'. Expected desktop or server."
        exit 1
        ;;
esac

case "$RUNS" in
    ''|*[!0-9]*|0)
        err "--runs must be a positive integer"
        exit 1
        ;;
esac

run_privileged() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        sudo -n "$@"
    fi
}

ensure_sudo_ready() {
    if [ "$(id -u)" -eq 0 ]; then
        return
    fi
    command -v sudo >/dev/null 2>&1 || {
        err "sudo is required to stop/start Cognis when running as a non-root user."
        exit 1
    }
    say "Refreshing sudo credentials for scheduler stop/start"
    sudo -v
}

start_sudo_keepalive() {
    if [ "$(id -u)" -eq 0 ]; then
        return
    fi

    if [ -n "$SUDO_KEEPALIVE_PID" ] && kill -0 "$SUDO_KEEPALIVE_PID" >/dev/null 2>&1; then
        return
    fi

    (
        while true; do
            sudo -n true >/dev/null 2>&1 || exit 0
            sleep 60
        done
    ) &
    SUDO_KEEPALIVE_PID=$!
}

stop_sudo_keepalive() {
    if [ -n "$SUDO_KEEPALIVE_PID" ] && kill -0 "$SUDO_KEEPALIVE_PID" >/dev/null 2>&1; then
        kill "$SUDO_KEEPALIVE_PID" >/dev/null 2>&1 || true
        wait "$SUDO_KEEPALIVE_PID" 2>/dev/null || true
    fi
    SUDO_KEEPALIVE_PID=""
}

detect_baseline_label() {
    local kernel_name

    kernel_name=$(uname -sr 2>/dev/null || true)
    if [ -n "$kernel_name" ]; then
        BASELINE_LABEL="$kernel_name"
    else
        BASELINE_LABEL="Baseline"
    fi
}

detect_power_profile() {
    local profile=""

    if command -v powerprofilesctl >/dev/null 2>&1; then
        profile=$(powerprofilesctl get 2>/dev/null || true)
    elif command -v tuned-adm >/dev/null 2>&1; then
        profile=$(tuned-adm active 2>/dev/null | sed 's/^Current active profile: //')
    fi

    if [ -n "$profile" ]; then
        POWER_PROFILE="$profile"
    fi
}

warn_if_running_as_root() {
    if [ "$(id -u)" -eq 0 ]; then
        warn "Running the whole benchmark as root changes HOME and benchmark cache paths."
        warn "Prefer running ./mini_benchmarker.sh as your normal user and let it prompt for sudo when needed."
    fi
}

current_sched_ext_ops() {
    if [ -r /sys/kernel/sched_ext/root/ops ]; then
        cat /sys/kernel/sched_ext/root/ops 2>/dev/null || true
    fi
}

cognis_is_active() {
    case "$(current_sched_ext_ops)" in
        *cognis*) return 0 ;;
    esac
    pgrep -x scx_cognis >/dev/null 2>&1
}

service_exists() {
    command -v systemctl >/dev/null 2>&1 && systemctl cat scx.service >/dev/null 2>&1
}

service_is_active() {
    service_exists && systemctl is-active --quiet scx.service
}

patch_local_mini_benchmarker_compat() {
    [ -f "$MINI_LOCAL_SCRIPT" ] || return 0

    if grep -q 'MB_TIME_BIN=' "$MINI_LOCAL_SCRIPT"; then
        return 0
    fi

    say "Patching local Mini Benchmarker copy for portable GNU time lookup"
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
    ok "Updated local Mini Benchmarker copy."
}

find_mini_benchmarker() {
    if [ -n "$MINI_BENCHMARKER_CMD" ]; then
        [ -x "$MINI_BENCHMARKER_CMD" ] || {
            err "Mini Benchmarker command '$MINI_BENCHMARKER_CMD' is not executable."
            exit 1
        }
        if [ "$MINI_BENCHMARKER_CMD" = "$MINI_LOCAL_SCRIPT" ]; then
            patch_local_mini_benchmarker_compat
        fi
        return
    fi

    for candidate in \
        "$MINI_LOCAL_SCRIPT" \
        mini-benchmarker.sh \
        mini-benchmarker
    do
        if command -v "$candidate" >/dev/null 2>&1; then
            MINI_BENCHMARKER_CMD=$(command -v "$candidate")
            if [ "$MINI_BENCHMARKER_CMD" = "$MINI_LOCAL_SCRIPT" ]; then
                patch_local_mini_benchmarker_compat
            fi
            return
        elif [ -x "$candidate" ]; then
            MINI_BENCHMARKER_CMD="$candidate"
            if [ "$MINI_BENCHMARKER_CMD" = "$MINI_LOCAL_SCRIPT" ]; then
                patch_local_mini_benchmarker_compat
            fi
            return
        fi
    done

    err "mini-benchmarker.sh was not found in PATH."
    say "Install it with ./install_benchmark_deps.sh --mini-benchmarker or set --mini-cmd."
    exit 1
}

find_scx_binary() {
    for candidate in \
        scx_cognis \
        "$SCRIPT_DIR/target/release/scx_cognis" \
        /usr/bin/scx_cognis \
        /usr/local/bin/scx_cognis
    do
        if [ -x "$candidate" ]; then
            SCX_BIN="$candidate"
            return
        fi
    done

    err "Could not find an executable scx_cognis binary."
    say "Build or install scx_cognis first."
    exit 1
}

check_plot_deps() {
    command -v "$PLOTTER_PYTHON" >/dev/null 2>&1 || {
        err "Python interpreter '$PLOTTER_PYTHON' was not found."
        exit 1
    }
    [ -f "$PLOTTER" ] || {
        err "Missing plot helper: $PLOTTER"
        exit 1
    }
    if "$PLOTTER_PYTHON" - <<'PY'
import matplotlib  # noqa: F401
PY
    then
        return
    fi

    if [ "$BOOTSTRAP_PLOTTER" -eq 1 ]; then
        bootstrap_plotter_venv
        "$PLOTTER_PYTHON" - <<'PY'
import matplotlib  # noqa: F401
PY
        return
    fi

    err "matplotlib is required for chart generation."
    say "Re-run with --bootstrap-plotter to install it in a local virtualenv."
    exit 1
}

ensure_results_path_writable() {
    local parent
    parent=$(dirname "$RESULTS_DIR")

    mkdir -p "$parent" 2>/dev/null || {
        err "Cannot create benchmark results parent directory: $parent"
        say "Run the benchmark from a writable checkout, or pass --results-dir to a writable location."
        exit 1
    }

    if [ ! -w "$parent" ]; then
        err "Benchmark results parent directory is not writable: $parent"
        say "Run the benchmark as your normal user from a writable checkout, or pass --results-dir to a writable location."
        exit 1
    fi
}

bootstrap_plotter_venv() {
    local venv_dir="${XDG_CACHE_HOME:-$HOME/.cache}/scx_cognis/mini-benchmarker-venv"
    command -v python3 >/dev/null 2>&1 || {
        err "python3 is required to bootstrap the plotter environment."
        exit 1
    }
    say "Bootstrapping local matplotlib venv at $venv_dir"
    python3 -m venv "$venv_dir"
    # shellcheck disable=SC1090
    . "$venv_dir/bin/activate"
    pip install --quiet matplotlib
    PLOTTER_PYTHON="$venv_dir/bin/python"
    ok "Plotter environment ready."
}

print_install_hints() {
    cat <<'EOF'
Install hints:
  CachyOS / Arch:
    - benchmark bootstrap helper: ./install_benchmark_deps.sh --mini-benchmarker --plotter
    - local fetched copy is searched at ~/.local/share/scx_cognis/mini-benchmarker/mini-benchmarker.sh
  Debian / Ubuntu:
    - common packages: ./install_benchmark_deps.sh --mini-benchmarker --plotter
    - local fetched copy is searched at ~/.local/share/scx_cognis/mini-benchmarker/mini-benchmarker.sh
EOF
}

check_runtime_command() {
    local label="$1"
    shift
    local candidate

    for candidate in "$@"; do
        if command -v "$candidate" >/dev/null 2>&1; then
            ok "$label available: $(command -v "$candidate")"
            return 0
        fi
    done

    err "$label missing"
    return 1
}

check_gnu_time_binary() {
    local candidate
    for candidate in /usr/bin/time /bin/time /usr/local/bin/time /opt/homebrew/bin/gtime /usr/local/bin/gtime; do
        if [ -x "$candidate" ]; then
            ok "GNU time executable available: $candidate"
            return 0
        fi
    done

    err "GNU time executable missing"
    return 1
}

check_mini_runtime_prereqs() {
    local missing=0

    check_gnu_time_binary || missing=1
    check_runtime_command "stress-ng" stress-ng || missing=1
    check_runtime_command "perf" perf || missing=1
    check_runtime_command "blender" blender || missing=1
    check_runtime_command "primesieve" primesieve || missing=1
    check_runtime_command "argon2" argon2 || missing=1
    check_runtime_command "x265" x265 || missing=1
    check_runtime_command "7z" 7z || missing=1
    check_runtime_command "wget" wget || missing=1
    check_runtime_command "tar" tar || missing=1
    check_runtime_command "xz" xz || missing=1
    check_runtime_command "make" make || missing=1
    check_runtime_command "cmake" cmake || missing=1
    check_runtime_command "nasm" nasm || missing=1
    check_runtime_command "C compiler" cc gcc clang || missing=1
    check_runtime_command "python shim for Mini Benchmarker" python || missing=1
    check_runtime_command "inxi" inxi || missing=1

    return "$missing"
}

check_dependency_status() {
    local missing=0
    local detected_mini=""
    local detected_scx=""

    say "Checking benchmark prerequisites"

    if command -v python3 >/dev/null 2>&1; then
        ok "python3 available"
    else
        err "python3 missing"
        missing=1
    fi

    if [ -f "$PLOTTER" ]; then
        ok "plot helper present: $(basename "$PLOTTER")"
    else
        err "plot helper missing: $PLOTTER"
        missing=1
    fi

    if command -v "$PLOTTER_PYTHON" >/dev/null 2>&1 && \
       "$PLOTTER_PYTHON" - <<'PY' >/dev/null 2>&1
import matplotlib  # noqa: F401
PY
    then
        ok "matplotlib import works"
    else
        warn "matplotlib not available for $PLOTTER_PYTHON"
    fi

    for candidate in "$MINI_LOCAL_SCRIPT" mini-benchmarker.sh mini-benchmarker; do
        if command -v "$candidate" >/dev/null 2>&1; then
            detected_mini=$(command -v "$candidate")
            break
        elif [ -x "$candidate" ]; then
            detected_mini="$candidate"
            break
        fi
    done
    if [ -n "$detected_mini" ]; then
        if [ "$detected_mini" = "$MINI_LOCAL_SCRIPT" ]; then
            patch_local_mini_benchmarker_compat
        fi
        ok "Mini Benchmarker found: $detected_mini"
    else
        err "Mini Benchmarker not found in PATH"
        missing=1
    fi

    for candidate in \
        scx_cognis \
        "$SCRIPT_DIR/target/release/scx_cognis" \
        /usr/bin/scx_cognis \
        /usr/local/bin/scx_cognis
    do
        if [ -x "$candidate" ]; then
            detected_scx="$candidate"
            break
        fi
    done
    if [ -n "$detected_scx" ]; then
        ok "scx_cognis found: $detected_scx"
    else
        err "scx_cognis binary not found"
        missing=1
    fi

    if [ -r /sys/kernel/sched_ext/root/ops ]; then
        ok "sched_ext sysfs present"
    else
        warn "sched_ext sysfs not visible; benchmarking may not work on this kernel"
    fi

    if [ "$(id -u)" -eq 0 ]; then
        ok "running as root; sudo ticket not required"
    elif command -v sudo >/dev/null 2>&1; then
        if sudo -n true >/dev/null 2>&1; then
            ok "sudo ticket already valid"
        else
            warn "sudo ticket not cached; the runner will prompt before starting benchmark runs"
        fi
    else
        err "sudo missing; non-root benchmark orchestration cannot stop/start Cognis"
        missing=1
    fi

    say "Checking Mini Benchmarker runtime tools"
    check_mini_runtime_prereqs || missing=1

    print_install_hints
    return "$missing"
}

ensure_supported_scheduler_state() {
    local ops
    ops=$(current_sched_ext_ops || true)
    if [ -n "$ops" ] && ! printf '%s' "$ops" | grep -qi 'cognis'; then
        err "Another sched_ext scheduler is active: $ops"
        say "Disable it first, then rerun mini_benchmarker.sh."
        exit 1
    fi
}

wait_for_cognis_state() {
    local want="$1"
    local attempt
    for attempt in 1 2 3 4 5 6 7 8 9 10; do
        if [ "$want" = "active" ] && cognis_is_active; then
            return 0
        fi
        if [ "$want" = "inactive" ] && ! cognis_is_active; then
            return 0
        fi
        sleep 1
    done
    return 1
}

stop_cognis() {
    if service_is_active; then
        say "Stopping scx.service"
        run_privileged systemctl stop scx.service
    fi
    if pgrep -x scx_cognis >/dev/null 2>&1; then
        say "Stopping running scx_cognis processes"
        run_privileged pkill -x scx_cognis || true
    fi
    wait_for_cognis_state inactive || {
        err "Cognis did not stop cleanly."
        exit 1
    }
}

start_cognis_manual() {
    local runtime_log="$RESULTS_DIR/console/cognis-${MODE}.log"
    say "Starting scx_cognis in ${MODE} mode"
    run_privileged env RUST_LOG=info "$SCX_BIN" --mode "$MODE" >"$runtime_log" 2>&1 &
    SCX_LAUNCHED=1
    wait_for_cognis_state active || {
        err "Cognis did not become active."
        exit 1
    }
}

cleanup_exit() {
    local status="$1"

    trap - EXIT

    stop_sudo_keepalive

    if [ "$RESTORE_NEEDED" -eq 1 ]; then
        restore_initial_state || true
    fi

    exit "$status"
}

restore_initial_state() {
    if [ "$INITIAL_SERVICE_ACTIVE" -eq 1 ]; then
        stop_cognis || true
        say "Restoring scx.service"
        run_privileged systemctl start scx.service || true
        return
    fi

    if [ "$INITIAL_COGNIS_ACTIVE" -eq 0 ]; then
        stop_cognis || true
        return
    fi

    warn "Cognis was initially active outside scx.service."
    warn "The script cannot safely recover the original manual flags."
    warn "Leaving Cognis running in benchmark mode: --mode $MODE"
}

tag_log_copy() {
    local source_log="$1"
    local tagged_log="$2"
    local label="$3"
    local variant_slug="$4"
    local power_profile="$5"

    "$PLOTTER_PYTHON" - "$source_log" "$tagged_log" "$label" "$variant_slug" "$power_profile" <<'PY'
from pathlib import Path
import re
import sys

source = Path(sys.argv[1])
target = Path(sys.argv[2])
label = sys.argv[3]
variant = sys.argv[4]
power_profile = sys.argv[5]
text = source.read_text(encoding="utf-8", errors="replace")
match = re.search(r"Kernel:\s+(\S+)", text)
if not match:
    raise SystemExit(f"Could not find Kernel: line in {source}")
kernel = match.group(1)
tagged = f"Kernel: {kernel}__{variant}"
text = re.sub(r"Kernel:\s+\S+", tagged, text, count=1)
text += (
    f"\nBenchmark label: {label}\n"
    f"Original kernel: {kernel}\n"
    f"Benchmark variant: {variant}\n"
    f"Power profile: {power_profile}\n"
)
target.write_text(text, encoding="utf-8")
PY
}

run_one_benchmark() {
    local variant_slug="$1"
    local label="$2"
    local run_index="$3"
    local run_name
    local cache_answer
    local raw_log
    local tagged_log

    run_name="${variant_slug}_run$(printf '%02d' "$run_index")"
    cache_answer="n"
    if [ "$DROP_CACHES" -eq 1 ]; then
        cache_answer="y"
    fi

    say "Running Mini Benchmarker: ${label} (run ${run_index}/${RUNS})"
    printf '%s\n%s\n' "$cache_answer" "$run_name" | \
        "$MINI_BENCHMARKER_CMD" "$WORKDIR" | tee "$RESULTS_DIR/console/${run_name}.out"

    raw_log=$(find "$WORKDIR" -maxdepth 1 -type f -name "benchie_${run_name}_*.log" | sort | tail -n 1)
    [ -n "$raw_log" ] || {
        err "Could not locate Mini Benchmarker log for ${run_name}"
        exit 1
    }

    cp "$raw_log" "$RESULTS_DIR/raw/"
    tagged_log="$RESULTS_DIR/tagged/$(basename "$raw_log")"
    tag_log_copy "$raw_log" "$tagged_log" "$label" "$variant_slug" "$POWER_PROFILE"
    ok "Saved $(basename "$raw_log")"
}

run_variant() {
    local variant_slug="$1"
    local label="$2"
    local action="$3"
    local run_index

    case "$action" in
        baseline)
            stop_cognis
            ;;
        cognis)
            stop_cognis
            start_cognis_manual
            ;;
        *)
            err "Unsupported run action: $action"
            exit 1
            ;;
    esac

    for run_index in $(seq 1 "$RUNS"); do
        run_one_benchmark "$variant_slug" "$label" "$run_index"
    done
}

main() {
    warn_if_running_as_root
    ensure_results_path_writable
    mkdir -p "$WORKDIR" "$RESULTS_DIR/raw" "$RESULTS_DIR/tagged" "$RESULTS_DIR/console"
    trap 'cleanup_exit $?' EXIT

    if [ "$CHECK_DEPS_ONLY" -eq 1 ]; then
        check_dependency_status
        exit 0
    fi

    find_mini_benchmarker
    find_scx_binary
    check_plot_deps
    check_mini_runtime_prereqs || {
        err "Mini Benchmarker runtime prerequisites are incomplete."
        say "Run ./mini_benchmarker.sh --check-deps or ./install_benchmark_deps.sh --mini-benchmarker --plotter first."
        exit 1
    }
    detect_baseline_label
    detect_power_profile
    ensure_sudo_ready
    start_sudo_keepalive
    ensure_supported_scheduler_state

    if cognis_is_active; then
        INITIAL_COGNIS_ACTIVE=1
    fi
    if service_is_active; then
        INITIAL_SERVICE_ACTIVE=1
    fi
    RESTORE_NEEDED=1

    say "Mini Benchmarker command : $MINI_BENCHMARKER_CMD"
    say "scx_cognis binary        : $SCX_BIN"
    say "Work directory           : $WORKDIR"
    say "Results directory        : $RESULTS_DIR"
    say "Cognis benchmark mode    : $MODE"
    say "Runs per variant         : $RUNS"
    say "Power profile            : $POWER_PROFILE"

    run_variant "baseline" "$BASELINE_LABEL" baseline
    run_variant "cognis-${MODE}" "Cognis (${MODE})" cognis

    "$PLOTTER_PYTHON" "$PLOTTER" "$RESULTS_DIR/tagged" \
        --title "Mini Benchmarker Comparison (${MODE} mode)"

    restore_initial_state
    RESTORE_NEEDED=0

    ok "Mini Benchmarker comparison complete."
    say "Chart: $RESULTS_DIR/tagged/mini_benchmarker_comparison.png"
    say "Chart: $RESULTS_DIR/tagged/mini_benchmarker_comparison.svg"
    say "CSV  : $RESULTS_DIR/tagged/mini_benchmarker_summary.csv"
}

main "$@"
