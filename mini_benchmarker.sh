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
MODE="desktop"
RUNS=1
DROP_CACHES=0
MINI_BENCHMARKER_CMD="${MINI_BENCHMARKER_CMD:-}"
PLOTTER="$SCRIPT_DIR/mini_benchmarker_plot.py"
SCX_BIN=""
SCX_LAUNCHED=0
INITIAL_COGNIS_ACTIVE=0
INITIAL_SERVICE_ACTIVE=0

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
  -h, --help             Show this help

Environment overrides:
  MINI_BENCHMARKER_CMD   Same as --mini-cmd
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
        sudo "$@"
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

find_mini_benchmarker() {
    if [ -n "$MINI_BENCHMARKER_CMD" ]; then
        [ -x "$MINI_BENCHMARKER_CMD" ] || {
            err "Mini Benchmarker command '$MINI_BENCHMARKER_CMD' is not executable."
            exit 1
        }
        return
    fi

    for candidate in mini-benchmarker.sh mini-benchmarker; do
        if command -v "$candidate" >/dev/null 2>&1; then
            MINI_BENCHMARKER_CMD=$(command -v "$candidate")
            return
        fi
    done

    err "mini-benchmarker.sh was not found in PATH."
    say "Install the external Mini Benchmarker tool first, then rerun this script."
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
    command -v python3 >/dev/null 2>&1 || {
        err "python3 is required for chart generation."
        exit 1
    }
    [ -f "$PLOTTER" ] || {
        err "Missing plot helper: $PLOTTER"
        exit 1
    }
    python3 - <<'PY'
import matplotlib  # noqa: F401
PY
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

    python3 - "$source_log" "$tagged_log" "$label" "$variant_slug" <<'PY'
from pathlib import Path
import re
import sys

source = Path(sys.argv[1])
target = Path(sys.argv[2])
label = sys.argv[3]
variant = sys.argv[4]
text = source.read_text(encoding="utf-8", errors="replace")
match = re.search(r"Kernel:\s+(\S+)", text)
if not match:
    raise SystemExit(f"Could not find Kernel: line in {source}")
kernel = match.group(1)
tagged = f"Kernel: {kernel}__{variant}"
text = re.sub(r"Kernel:\s+\S+", tagged, text, count=1)
text += f"\nBenchmark label: {label}\nOriginal kernel: {kernel}\nBenchmark variant: {variant}\n"
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
    tag_log_copy "$raw_log" "$tagged_log" "$label" "$variant_slug"
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
    mkdir -p "$WORKDIR" "$RESULTS_DIR/raw" "$RESULTS_DIR/tagged" "$RESULTS_DIR/console"

    find_mini_benchmarker
    find_scx_binary
    check_plot_deps
    ensure_supported_scheduler_state

    if cognis_is_active; then
        INITIAL_COGNIS_ACTIVE=1
    fi
    if service_is_active; then
        INITIAL_SERVICE_ACTIVE=1
    fi

    say "Mini Benchmarker command : $MINI_BENCHMARKER_CMD"
    say "scx_cognis binary        : $SCX_BIN"
    say "Work directory           : $WORKDIR"
    say "Results directory        : $RESULTS_DIR"
    say "Cognis benchmark mode    : $MODE"
    say "Runs per variant         : $RUNS"

    run_variant "baseline" "Baseline (kernel default scheduler)" baseline
    run_variant "cognis-${MODE}" "Cognis (${MODE})" cognis

    python3 "$PLOTTER" "$RESULTS_DIR/tagged" \
        --title "Mini Benchmarker Comparison (${MODE} mode)"

    restore_initial_state

    ok "Mini Benchmarker comparison complete."
    say "Chart: $RESULTS_DIR/tagged/mini_benchmarker_comparison.png"
    say "Chart: $RESULTS_DIR/tagged/mini_benchmarker_comparison.svg"
    say "CSV  : $RESULTS_DIR/tagged/mini_benchmarker_summary.csv"
}

main "$@"
