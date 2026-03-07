#!/bin/sh
# cognis_benchmark.sh — Side-by-side benchmark helper for scx_cognis
#
# Runs a reproducible stress-ng workload and opens the WebGL Aquarium so you
# can compare system responsiveness with and without scx_cognis active.
#
# Usage:
#   sh cognis_benchmark.sh
#
# No root is needed for mode 1 (baseline).
# Mode 2 (with scx_cognis) requires scx_cognis to already be running
# (either via systemd or manually with sudo).

set -e

# ── Colour helpers ────────────────────────────────────────────────────────────
# Use $(printf ...) so POSIX sh expands the ESC byte rather than storing the
# literal backslash-escaped string that single-quoted assignments would give.
RED=$(printf '\033[0;31m')
GRN=$(printf '\033[0;32m')
YLW=$(printf '\033[1;33m')
CYN=$(printf '\033[0;36m')
BLD=$(printf '\033[1m')
RST=$(printf '\033[0m')

say()  { printf "${BLD}${CYN}[cognis-bench]${RST} %s\n" "$1"; }
ok()   { printf "${BLD}${GRN}[  OK  ]${RST} %s\n" "$1"; }
warn() { printf "${BLD}${YLW}[ WARN ]${RST} %s\n" "$1"; }
err()  { printf "${BLD}${RED}[ERROR ]${RST} %s\n" "$1"; }
sep()  { printf "${CYN}%s${RST}\n" "────────────────────────────────────────────────────────────────────────────"; }

AQUARIUM_URL="https://webglsamples.org/aquarium/aquarium.html"
STRESS_DURATION=60          # seconds each stressor phase runs
STRESS_CPU_WORKERS=0        # 0 = one worker per logical CPU
STRESS_IO_WORKERS=4
STRESS_VM_WORKERS=2
STRESS_VM_BYTES="256M"

# ── Dependency check ──────────────────────────────────────────────────────────
check_deps() {
    MISSING=""
    for cmd in stress-ng; do
        if ! command -v "$cmd" > /dev/null 2>&1; then
            MISSING="$MISSING $cmd"
        fi
    done

    # Find a usable browser for the aquarium (non-fatal — user can open manually)
    BROWSER=""
    for b in xdg-open firefox chromium google-chrome brave-browser; do
        if command -v "$b" > /dev/null 2>&1; then
            BROWSER="$b"
            break
        fi
    done

    if [ -n "$MISSING" ]; then
        err "Missing required tools:$MISSING"
        say "Install them with your package manager, e.g.:"
        say "  Arch/CachyOS : sudo pacman -S stress-ng"
        say "  Ubuntu/Debian: sudo apt install stress-ng"
        exit 1
    fi
}

# ── Check whether scx_cognis is currently the active scheduler ────────────────
cognis_is_active() {
    # /sys/kernel/sched_ext/root/ops reports the current sched_ext scheduler name
    if [ -f /sys/kernel/sched_ext/root/ops ]; then
        OPS=$(cat /sys/kernel/sched_ext/root/ops 2>/dev/null || true)
        case "$OPS" in
            *cognis*) return 0 ;;
        esac
    fi
    # Fallback: check if a process named scx_cognis is running
    if pgrep -x scx_cognis > /dev/null 2>&1; then
        return 0
    fi
    return 1
}

# ── Open the WebGL Aquarium in a browser ─────────────────────────────────────
open_aquarium() {
    sep
    say "Opening WebGL Aquarium in your browser..."
    say "  URL: $AQUARIUM_URL"
    if [ -n "$BROWSER" ]; then
        "$BROWSER" "$AQUARIUM_URL" > /dev/null 2>&1 &
        ok "Launched with: $BROWSER"
    else
        warn "No browser found. Open this URL manually:"
        warn "  $AQUARIUM_URL"
    fi
    say ""
    say "While the benchmark runs, watch the Aquarium for:"
    say "  • Frame rate  — fish animation should stay smooth (≥ 30 fps)"
    say "  • Stutter     — pause or jank = scheduler struggling under load"
    say "  • Tab latency — click Fish Count slider: should respond instantly"
    say ""
    say "Use the default 500 fish — it gives realistic load without overwhelming the system."
}

# ── Print what to watch in the cognis monitor output ─────────────────────────
print_monitor_guide() {
    sep
    say "What to watch in  scx_cognis --monitor 1.0  output:"
    say ""
    say "  tldr:         → Plain-English health summary. Should stay in"
    say "                  'Rest assured' / 'Busy but responsive' / 'Smooth sailing'"
    say "                  during the benchmark. Avoid 'SOS' or 'overwhelmed'."
    say ""
    say "  d→u  vs  k    → User dispatches (d→u) vs kernel fallback (k)."
    say "                  d→u should be non-trivial. A ratio of k >> d→u means"
    say "                  cognis isn't getting enough cycles to schedule."
    say ""
    say "  Interactive   → Should remain the dominant label (most desktop tasks)."
    say "  Compute       → Will rise during the stress-ng CPU phase — expected."
    say ""
    say "  cong          → Congestion events. Occasional spikes are fine."
    say "                  Sustained high values = scheduler under pressure."
    say ""
    say "  slice         → AI-adjusted time-slice. Should shrink during"
    say "                  interactive-heavy phases and grow during compute phases."
    say ""
    say "  reward        → EMA reward score. Higher = better balance."
    say "                  Aim for ≥ 0.3 during the full benchmark."
    sep
}

# ── Run the stress-ng workload ────────────────────────────────────────────────
run_stress() {
    MODE_LABEL="$1"
    sep
    say "Starting stress-ng benchmark  [ $MODE_LABEL ]"
    say "Total duration: $((STRESS_DURATION * 3))s  (3 phases × ${STRESS_DURATION}s each)"
    say ""
    say "Phase layout:"
    say "  1/3  CPU stress     — saturates all logical CPUs (compute-bound)"
    say "  2/3  I/O stress     — disk read/write latency (I/O-bound)"
    say "  3/3  Mixed stress   — CPU + VM pressure (realistic desktop load)"
    sep

    # ── Phase 1: CPU ──────────────────────────────────────────────────────────
    say "Phase 1/3 — CPU stress (${STRESS_DURATION}s) ..."
    stress-ng --cpu "$STRESS_CPU_WORKERS" \
              --metrics-brief \
              --timeout "${STRESS_DURATION}s" \
              --log-brief 2>&1 | grep -E "stress-ng:|bogo|cpu" || true
    ok "Phase 1 complete"

    # ── Phase 2: I/O ─────────────────────────────────────────────────────────
    say "Phase 2/3 — I/O stress (${STRESS_DURATION}s) ..."
    stress-ng --iomix "$STRESS_IO_WORKERS" \
              --metrics-brief \
              --timeout "${STRESS_DURATION}s" \
              --log-brief 2>&1 | grep -E "stress-ng:|bogo|iomix" || true
    ok "Phase 2 complete"

    # ── Phase 3: Mixed (CPU + VM) ─────────────────────────────────────────────
    say "Phase 3/3 — Mixed CPU + VM stress (${STRESS_DURATION}s) ..."
    stress-ng --cpu "$STRESS_CPU_WORKERS" \
              --vm "$STRESS_VM_WORKERS" \
              --vm-bytes "$STRESS_VM_BYTES" \
              --metrics-brief \
              --timeout "${STRESS_DURATION}s" \
              --log-brief 2>&1 | grep -E "stress-ng:|bogo|cpu|vm" || true
    ok "Phase 3 complete"
}

# ── Summary ───────────────────────────────────────────────────────────────────
print_summary() {
    MODE_LABEL="$1"
    sep
    ok "Benchmark complete  [ $MODE_LABEL ]"
    say ""
    say "Compare these results side-by-side:"
    say "  1. bogo-ops/s  — higher is better (raw throughput)"
    say "  2. Aquarium fps — higher is better (visual responsiveness)"
    say "  3. Aquarium jank — zero stutter is ideal"
    say ""
    say "scx_cognis should deliver a smoother Aquarium experience under load"
    say "by shortening greedy background bursts while preserving enough slice"
    say "budget for render, browser, compositor, and other wakeup-heavy work."
    sep
}

# ── Mode 1: Baseline (no cognis) ─────────────────────────────────────────────
run_baseline() {
    sep
    say "${BLD}Mode 1 — Baseline (default kernel scheduler, no scx_cognis)${RST}"
    sep

    if cognis_is_active; then
        warn "scx_cognis appears to be running right now."
        warn "For a clean baseline, stop it first:"
        warn "  sudo systemctl stop scx        # if running as a service"
        warn "  sudo killall scx_cognis        # if started manually"
        say ""
        printf "Continue anyway? [y/N] "
        read -r REPLY
        case "$REPLY" in
            y|Y) ;;
            *) say "Aborted."; exit 0 ;;
        esac
    fi

    open_aquarium
    say ""
    say "Aquarium is open. Leave the fish count at the default (500), let it settle for ~5s,"
    say "then press Enter here to start the stress workload."
    printf "  Press Enter to begin ... "
    read -r _
    run_stress "Baseline — no scx_cognis"
    print_summary "Baseline — no scx_cognis"
}

# ── Mode 2: With cognis ───────────────────────────────────────────────────────
run_with_cognis() {
    sep
    say "${BLD}Mode 2 — With scx_cognis active${RST}"
    sep

    if ! cognis_is_active; then
        warn "scx_cognis does not appear to be running."
        warn "Start it first with one of:"
        warn "  sudo systemctl start scx       # if installed via install.sh"
        warn "  sudo scx_cognis                # manual foreground"
        warn "  sudo scx_cognis &              # manual background"
        say ""
        printf "Continue anyway? [y/N] "
        read -r REPLY
        case "$REPLY" in
            y|Y) ;;
            *) say "Aborted."; exit 0 ;;
        esac
    else
        ok "scx_cognis is active — ready to benchmark."
    fi

    say ""
    say "Tip: open a second terminal and run:"
    say "  scx_cognis --monitor 1.0"
    say "to watch the AI scheduler adapt in real-time during the test."
    print_monitor_guide

    open_aquarium
    say ""
    say "Aquarium is open. Leave the fish count at the default (500), let it settle for ~5s,"
    say "then press Enter here to start the stress workload."
    printf "  Press Enter to begin ... "
    read -r _
    run_stress "With scx_cognis"
    print_summary "With scx_cognis"
}

# ── Entry point ───────────────────────────────────────────────────────────────
main() {
    clear
    sep
    printf "${BLD}${CYN}  scx_cognis — Interactive Benchmark Script${RST}\n"
    printf "${CYN}  Compare scheduler responsiveness with and without scx_cognis${RST}\n"
    sep
    say ""
    say "This script will:"
    say "  • Open the WebGL Aquarium (visual responsiveness test)"
    say "  • Run a 3-phase stress-ng workload (CPU → I/O → Mixed, ${STRESS_DURATION}s each)"
    say "  • Print bogo-ops/s for each phase so you can compare results"
    say ""
    say "Run it TWICE — once for each mode — and compare the Aquarium smoothness"
    say "and bogo-ops numbers between the two runs."
    say ""

    check_deps

    sep
    printf "${BLD}Select benchmark mode:${RST}\n\n"
    printf "  ${BLD}1${RST}  Baseline — run without scx_cognis  (kernel default CFS/EEVDF)\n"
    printf "  ${BLD}2${RST}  Cognis   — run with scx_cognis active\n"
    printf "  ${BLD}q${RST}  Quit\n\n"
    printf "Choice [1/2/q]: "
    read -r CHOICE

    case "$CHOICE" in
        1) run_baseline    ;;
        2) run_with_cognis ;;
        q|Q) say "Bye!"; exit 0 ;;
        *) err "Invalid choice '$CHOICE'. Run the script again and enter 1, 2, or q."; exit 1 ;;
    esac
}

main "$@"
