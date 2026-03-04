# scx_cognis

"Cognis" (from Latin *cognōscere*) — to learn, to know — is a Linux CPU scheduler with adaptive scheduling policy, built on established statistical and machine-learning algorithms.

`scx_cognis` is a Linux CPU scheduler built on the [`sched_ext`](https://www.kernel.org/doc/html/latest/scheduler/sched-ext.html) framework and [`scx_rustland_core`](https://crates.io/crates/scx_rustland_core). It combines a deterministic task classifier, Bayesian reputation tracking, tabular Q-learning, Isolation Forest anomaly detection, and an Elman RNN burst predictor — all running in user-space Rust with a sub-5 µs per-event inference target.

---

<a name="table-of-contents"></a>
## Table of Contents

- [Status](#status)
  - [Test Results](#test-results)
  - [Tested Platforms](#tested-platforms)
- [Features](#features)
  - [Pipeline Overview](#pipeline-overview)
  - [Component Details](#component-details)
    - [Heuristic Task Classifier](#heuristic-task-classifier)
    - [Isolation Forest Anti-Cheat Engine](#isolation-forest-anti-cheat-engine)
    - [A\* Load Balancer](#a-load-balancer)
    - [Elman RNN Burst Predictor](#elman-rnn-burst-predictor)
    - [Bayesian Reputation Engine](#bayesian-reputation-engine)
    - [Q-learning Policy Controller](#q-learning-policy-controller)
    - [ratatui TUI Dashboard](#ratatui-tui-dashboard)
- [Design Notes](#design-notes)
  - [Architecture](#architecture)
  - [Pipeline Details](#pipeline-details)
  - [Latency Budget](#latency-budget)
  - [Scheduler Fail-Safe](#scheduler-fail-safe)
  - [Reward Function](#reward-function)
  - [Time-Slice Calculation](#time-slice-calculation)
- [Requirements](#requirements)
  - [Kernel Requirements](#kernel-requirements)
  - [Rust Toolchain](#rust-toolchain)
  - [System Libraries](#system-libraries)
- [Build](#build)
  - [Quick Build](#quick-build)
  - [Build from Source](#build-from-source)
- [Usage](#usage)
  - [Basic Usage](#basic-usage)
  - [TUI Dashboard](#tui-dashboard)
  - [Command-Line Options](#command-line-options)
  - [Stats Monitoring](#stats-monitoring)
  - [How to Read Cognis Statistics](#how-to-read-cognis-statistics)
    - [Full Output Format](#full-output-format)
    - [Column Reference](#column-reference)
    - [Classification Label Deep-Dive](#classification-label-deep-dive)
    - [TLDR Message Reference](#tldr-message-reference)
- [Installation Guide](#installation-guide)
  - [Using install.sh (Recommended)](#using-installsh-recommended)
  - [Using uninstall.sh](#using-uninstallsh)
  - [CachyOS](#cachyos)
    - [Step 1 — Install build dependencies](#step-1--install-build-dependencies)
    - [Step 2 — Install Rust (if not already present via rustup)](#step-2--install-rust-if-not-already-present-via-rustup)
    - [Step 3 — Clone and build](#step-3--clone-and-build)
    - [Step 4 — Run](#step-4--run)
    - [Step 5 — (Optional) Install system-wide](#step-5--optional-install-system-wide)
    - [Step 6 — Register as the system-default scheduler via CachyOS Hello](#step-6--register-as-the-system-default-scheduler-via-cachyos-hello)
  - [Arch Linux and Manjaro](#arch-linux-and-manjaro)
    - [Kernel requirement](#kernel-requirement)
    - [Build dependencies](#build-dependencies)
    - [Build and run](#build-and-run)
  - [Ubuntu and Debian](#ubuntu-and-debian)
    - [Kernel requirement](#kernel-requirement-1)
    - [Build dependencies](#build-dependencies-1)
    - [Build and run](#build-and-run-1)
  - [Running as a systemd Service](#running-as-a-systemd-service)
- [Benchmarks](#benchmarks)
  - [Running the Benchmark](#running-the-benchmark)
  - [What to Watch During the Benchmark](#what-to-watch-during-the-benchmark)
  - [Interpreting Results](#interpreting-results)
  - [Test Platform](#test-platform)
  - [Reference Benchmark Results](#reference-benchmark-results)
- [Limitations and Next Steps](#limitations-and-next-steps)
- [Contributing](#contributing)
  - [Running Tests](#running-tests)
  - [Adding a New Module](#adding-a-new-module)
- [License](#license)

---

## Status

Stable — all 17 unit tests pass on every commit. The scheduler builds cleanly from crates.io with no external SCM dependencies, and has been run successfully on production workloads on `sched_ext`-enabled kernels (≥ 6.12).

### Test Results

```
running 17 tests
test ai::burst_predictor::tests::evict_removes_state ............. ok
test ai::burst_predictor::tests::predicts_nonzero_after_warmup ... ok
test ai::classifier::tests::heuristic_boundary_compute_high ...... ok
test ai::classifier::tests::heuristic_boundary_interactive_low ... ok
test ai::classifier::tests::heuristic_compute .................... ok
test ai::classifier::tests::heuristic_interactive ................ ok
test ai::classifier::tests::heuristic_iowait ..................... ok
test ai::classifier::tests::heuristic_realtime ................... ok
test ai::classifier::tests::high_cpu_fresh_wakeup_is_interactive ... ok
test ai::anomaly::tests::anomaly_score_range ...................... ok
test ai::load_balancer::tests::quarantine_only_restricted ......... ok
test ai::load_balancer::tests::selects_idle_cpu ................... ok
test ai::reputation::tests::penalise_decreases_trust .............. ok
test ai::reputation::tests::quarantine_on_cheat_flag .............. ok
test ai::reputation::tests::reward_increases_trust ................ ok
test ai::reputation::tests::uniform_prior_mean .................... ok
test ai::policy::tests::slice_stays_in_bounds ..................... ok

test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
```

### Tested Platforms

| Platform | Kernel | Architecture | CI Status |
|:---|:---|:---|:---|
| Ubuntu 24.04 LTS | 6.8 | x86-64 | [![CI (Ubuntu)](https://github.com/galpt/scx_cognis/actions/workflows/ci.yml/badge.svg)](https://github.com/galpt/scx_cognis/actions/workflows/ci.yml) |
| Arch Linux | ≥ 6.12 (sched-ext) | x86-64 | [![CI (Arch Linux)](https://github.com/galpt/scx_cognis/actions/workflows/ci-arch.yml/badge.svg)](https://github.com/galpt/scx_cognis/actions/workflows/ci-arch.yml) |
| CachyOS (latest) | 6.13+ (sched-ext) | x86-64 | [![CI (CachyOS)](https://github.com/galpt/scx_cognis/actions/workflows/ci-cachyos.yml/badge.svg)](https://github.com/galpt/scx_cognis/actions/workflows/ci-cachyos.yml) |

> [!NOTE]
> 1. Each CI badge reflects a `cargo check --all` run inside the distribution's official Docker image on the latest push.
> 2. Runtime testing requires a `sched_ext`-enabled kernel (CONFIG_SCHED_CLASS_EXT=y) which standard CI runners do not provide.
> 3. Arch Linux and CachyOS are also verified manually on hardware with the `linux-sched-ext` / `linux-cachyos` kernels.

[↑ Back to Table of Contents](#table-of-contents)

---

## Features

> [!TIP]
> **What to expect from scx_cognis?**
>
> Cognis is designed to keep interactive tasks (UI rendering, audio, input) responsive even when the system is under heavy CPU or I/O load. It does this by classifying tasks as Interactive, Compute, IoWait, or RealTime on every scheduling event and giving short, high-frequency slices to Interactive tasks (0.5× base) so they are never queued behind long-running CPU-bound workers. Fair-share throughput for batch workloads is preserved — just not maximised. If your goal is raw CPU throughput (compilers, encoders, benchmarks), the default EEVDF scheduler wins; cognis's advantage is consistent low scheduling latency for tasks that wake up, do a little work, and sleep again.

### Pipeline Overview

Every scheduling decision passes through a multi-stage inference pipeline. The entire pipeline completes in **< 5 µs** on a modern CPU, staying well within the time-slice budget.

```
ops.enqueue   →  Heuristic Classifier  →  Reputation Check  →  Burst Predictor
ops.dispatch  →  Q-learning Policy (adaptive time slice)
ops.select_cpu → A* Load Balancer  (P/E-core, NUMA, quarantine-aware)
ops.tick      →  Isolation Forest Anti-Cheat
schedule loop →  Reputation Engine          (staleness-based, every 1 s)
```

### Component Details

#### Heuristic Task Classifier

Dynamically labels each PID as `Interactive`, `Compute`, `IoWait`, `RealTime`, or `Unknown` using a
deterministic heuristic evaluated on every `ops.enqueue` event. Five task features are tracked:

| Feature | Description |
|:---|:---|
| `runnable_ratio` | `burst_ns / base_slice_ns` — most-recent burst as a fraction of the target slice |
| `cpu_intensity` | `burst_ns / prev_assigned_slice_ns` — fraction of the *assigned* slice the task actually consumed |
| `exec_ratio` | `burst_ns / exec_runtime` — freshness: near 1.0 = just woke; near 0.0 = spinning without sleeping |
| `weight_norm` | Normalised scheduler weight (priority) |
| `cpu_affinity` | Allowed CPUs / total online CPUs |

The primary classification feature is `cpu_intensity`. `prev_assigned_slice_ns` is the slice allocated to this PID on its previous scheduling event (stored in `TaskLifetime`). On first event, `base_slice_ns` is used as fallback. This makes classification stable and loop-free from the second event onward:

| `cpu_intensity` range | `exec_ratio` condition | Label |
|:---|:---|:---|
| > 0.85 AND exec_ratio < 0.30 | Never sleeps between slices — CPU-bound | **Compute** |
| > 0.85 AND exec_ratio ≥ 0.30 | High burst but just woke (e.g. 120fps render) | **Interactive** |
| 0.10 – 0.85 (any exec_ratio) | Yields regularly — latency-sensitive | **Interactive** |
| < 0.10 (any exec_ratio) | Released CPU far before slice expired — I/O-blocked | **IoWait** |

> [!IMPORTANT]
> **The `exec_ratio` guard is critical**
>
> `exec_runtime` resets to 0 on every wakeup (`ops.runnable`), so a browser rendering WebGL frames at 120fps gets `exec_ratio ≈ 1.0` even though it consumes 100% of each slice. A true background Compute task (stress-ng, compiler) never sleeps, so `exec_runtime` accumulates for seconds and `exec_ratio → 0` within a few slices. Without this guard, the monitor would show `Interactive:1 Compute:77` for a browser workload — exactly the root cause of the [throughput regression seen during benchmarking](#reference-benchmark-results).

A weight-norm threshold (> 0.95) catches `SCHED_FIFO` / `SCHED_RR` tasks regardless of slice usage and labels them **RealTime**.

Labels influence the base time-slice multiplier and feed into the Reputation Engine and Q-learning Policy.

[↑ Back to Table of Contents](#table-of-contents)

#### Isolation Forest Anti-Cheat Engine

Detects scheduler-abusing processes (fork-bombers, yield-spinners, deadline-gaming) using an approximated Isolation Forest: 32 trees, sample size 128, max depth 8. Anomaly scores are averaged over all trees; tasks scoring above the 0.65 threshold are flagged and routed exclusively to restricted CPUs.

The forest is retrained every 500 ticks (~50 s at a 100 ms tick rate) to adapt to workload changes without stalling the hot path.

[↑ Back to Table of Contents](#table-of-contents)

#### A\* Load Balancer

Selects the optimal CPU for each task using an A\*-inspired heuristic traversal over a per-CPU cost graph. Placement cost accounts for:

- Current CPU utilisation (primary cost)
- NUMA node distance to the task's previous CPU
- Core-type mismatch penalty (Performance vs. Efficiency cores)
- Thermal throttle penalty
- Quarantine routing (flagged tasks may only land on restricted CPUs)

CPU topology is read from sysfs at scheduler startup:

| Topology data | Source |
|:---|:---|
| Performance / Efficiency core classification | `/sys/devices/cpu_core/cpus`, `/sys/devices/cpu_atom/cpus` (Intel hybrid) |
| NUMA node per CPU | `/sys/devices/system/node/nodeN/cpulist` |

On non-hybrid CPUs (AMD, homogeneous Intel, VMs) the sysfs entries are absent and all CPUs are treated as Performance-class.

Falls back to `RL_CPU_ANY` (kernel-side placement) when no CPU is a clear winner.

[↑ Back to Table of Contents](#table-of-contents)

#### Elman RNN Burst Predictor

Predicts each PID's next CPU burst duration using a compact Elman RNN: H = 4 hidden units, X = 3 inputs (`burst_norm`, `exec_ratio`, `cpu_intensity`). The architecture is a standard single-layer Elman RNN (`h[t] = tanh(W_h·h[t-1] + W_x·x[t] + b)`); weights are compile-time constants derived from offline analysis of synthetic scheduler traces and have not been validated on real production workloads. The forward pass runs in O(H·X) = O(12) multiplications with per-PID hidden state in a `HashMap<i32, PidState>`.

Predictions are EMA-smoothed (α = 0.15) to reduce jitter. If the predictor forecasts a short burst, the scheduler preemptively shortens the assigned slice, reclaiming CPU time for other tasks.

[↑ Back to Table of Contents](#table-of-contents)

#### Bayesian Reputation Engine

Maintains a Beta(α, β) prior over trust for each PID:

- **Cooperative events** (task yields within slice, clean exit, low fork rate) → increment α
- **Adversarial events** (slice burned, cheat flag, high fork count) → increment β

Trust score E[T] = α / (α + β). Tasks below the 0.35 threshold are quarantined — their slice factor is reduced and the A\* load balancer routes them to restricted cores.

[↑ Back to Table of Contents](#table-of-contents)

#### Q-learning Policy Controller

Continuously tunes the global base time-slice using tabular Q-learning (TABLE_SIZE = 625 = 5⁴ states, 3 actions: shrink × 0.80 | keep × 1.00 | grow × 1.15). The four-dimensional state is:

| Dimension | Bins | Description |
|:---|:---|:---|
| `load` | 5 | Fraction of CPUs busy |
| `interactive_frac` | 5 | Fraction of tasks labelled Interactive |
| `compute_frac` | 5 | Fraction of tasks labelled Compute |
| `latency` | 5 | Estimated scheduling latency tier |

The reward signal is:

```
R = (interactive_frac × load_norm) × 0.7 − congestion × 0.2 − latency × 0.1
  clamped to [−1.0, +1.0]
```

where `interactive_frac` is the fraction of currently-queued Interactive tasks and `load_norm` is CPU utilisation (0–1). This produces a meaningful gradient: the policy learns to **shrink** when Compute tasks dominate (low `interactive_frac`) and **keep/grow** only when Interactive tasks are well-served.

ε-greedy exploration decays from 0.30 → 0.02 with each update. The current slice is published to an `AtomicU64` so the dispatch hot-path reads it without locking.

Policy updates run every 250 ms; Isolation Forest anti-cheat ticks every 100 ms — both are **off the scheduling hot-path**.

[↑ Back to Table of Contents](#table-of-contents)

#### ratatui TUI Dashboard

A real-time glass-box view of every AI decision, rendered in the terminal using [ratatui](https://ratatui.rs/). Press `q` or `Esc` to exit.

Panels:

| Panel | Content |
|:---|:---|
| Header | Scheduler name, live CPU/running/queued counts, current time-slice |
| Overview | User/kernel/failed dispatch counts, congestion events, page faults, CPU load % |
| Classification | Live bar gauges for Interactive / Compute / IoWait / RealTime with quarantine/flagged counts |
| Q-learning Policy | Current time-slice, Q-learning reward EMA, average inference latency, latency budget check |
| Latency Chart | Rolling 120-sample line chart of average per-event inference (µs) |
| Wall of Shame | Top quarantined or anti-cheat-flagged PIDs with trust score and cheat flag |

[↑ Back to Table of Contents](#table-of-contents)

---

## Design Notes

### Architecture

```
┌───────────────────────────────────────────────────────────────────┐
│  Linux Kernel  (sched_ext BPF backend — scx_rustland_core)        │
│                                                                   │
│  ┌────────────────────────┐   ring buffer   ┌──────────────────┐  │
│  │ BPF dispatcher         │ ─────────────▶  │  User-space      │  │
│  │ (scx_rustland_core)    │ ◀─────────────  │  Scheduler       │  │
│  └────────────────────────┘   user ring     │  (scx_cognis)    │  │
│                                             └──────────────────┘  │
└───────────────────────────────────────────────────────────────────┘
                                     │
┌────────────────────────────────────▼──────────────────────────────┐
│                        Scheduling Pipeline                        │
│                                                                   │
│  dequeue     → heuristic classify                                 │
│              → Bayesian reputation check                          │
│              → Elman RNN headroom hint                            │
│  select_cpu  → A* topology search                                 │
│  dispatch    → Q-learning policy slice read                       │
│  tick        → Isolation Forest anti-cheat                        │
└───────────────────────────────────────────────────────────────────┘
                                     │
┌────────────────────────────────────▼──────────────────────────────┐
│                       ratatui TUI Dashboard                       │
│  Arc<Mutex<DashboardState>>  (lock-free reads                     │
│  on hot path via AtomicU64 slice)                                 │
└───────────────────────────────────────────────────────────────────┘
```

### Pipeline Details

The pipeline runs **synchronously on the hot scheduling path** for the per-task steps (heuristic classify, reputation read, burst predictor read, A\*, Q-learning slice read). Heavier operations (anti-cheat forest ticks, Q-table updates) run on **periodic timers** off the hot path.

| Step | Hot Path? | Frequency |
|:---|:---|:---|
| Heuristic classify | ✅ Yes | every `ops.enqueue` |
| Reputation read | ✅ Yes | every `ops.enqueue` |
| Burst predictor read | ✅ Yes | every `ops.enqueue` |
| A\* CPU select | ✅ Yes | every `ops.select_cpu` |
| Q-learning slice read | ✅ Yes | every `ops.dispatch` (atomic read) |
| Anti-cheat tick | ❌ No | every 100 ms |
| Q-table update | ❌ No | every 250 ms |
| Reputation update | ❌ No | every 1 s (staleness heuristic) |

### Latency Budget

| Component | Complexity | Typical cost |
|:---|:---|:---|
| Heuristic classify | O(1) — 4 comparisons | < 0.05 µs |
| Reputation read | O(1) HashMap lookup | < 0.1 µs |
| Burst predictor | O(H·X) = O(12) matmul | < 0.1 µs |
| A\* placement | O(n\_cpus) BinaryHeap | ~1 µs |
| Q-learning slice read | O(1) atomic read | < 0.05 µs |
| **Total (typical)** | | **~1–2 µs** |

[↑ Back to Table of Contents](#table-of-contents)

### Scheduler Fail-Safe

If the user-space daemon crashes or stops responding, `scx_rustland_core`'s built-in kernel-side watchdog automatically reverts all tasks to the default kernel scheduler within **~50 ms**, preventing any system lockup. This is a hard guarantee provided by the `sched_ext` framework itself.

[↑ Back to Table of Contents](#table-of-contents)

### Reward Function

The Q-learning controller optimises the global base time-slice using the following reward signal computed every 250 ms:

```
R = (interactive_frac × load_norm) × 0.7 − congestion × 0.2 − latency × 0.1
    clamped to [−1.0, +1.0]
```

| Term | Weight | Description |
|:---|:---|:---|
| `interactive_frac × load_norm` | **0.7** | Reward for serving Interactive tasks under load |
| `congestion` | 0.2 | Penalty for scheduler queue congestion |
| `latency` | 0.1 | Penalty for estimated scheduling latency |

The slice action space is: { shrink × 0.80, keep × 1.00, grow × **1.15** }.

The Q-learning controller's maximum output is capped at `base_slice_ns` (the user's `--slice-us` setting), preventing any inflation beyond the configured budget.

[↑ Back to Table of Contents](#table-of-contents)

### Time-Slice Calculation

For each dispatched task, the final slice is:

```
slice = base_slice_ns
      × ai_policy_factor        (from Q-learning policy AtomicU64)
      × label_multiplier        (Interactive=0.5, Compute=1.0, IoWait=0.75, RT=0.25)
      × reputation_factor       (Bayesian trust ∈ [0.25, 1.0])
      × (weight / 100)          (scheduler priority)
      clamped to [slice_ns_min, base_slice × 8]
```

If the Elman RNN predicts a short burst, the slice is further capped to `min(slice, predicted_burst × 2)`.

[↑ Back to Table of Contents](#table-of-contents)

---

## Requirements

### Kernel Requirements

- Linux kernel **≥ 6.12** with `sched_ext` support enabled (`CONFIG_SCHED_CLASS_EXT=y`).
- Kernels known to work out of the box: **CachyOS** (all editions), `linux-cachyos`, `linux-sched-ext` (AUR), upstream kernels ≥ 6.12 with the option enabled.

To verify your kernel supports sched_ext:
```bash
grep CONFIG_SCHED_CLASS_EXT /boot/config-$(uname -r)
# Expected: CONFIG_SCHED_CLASS_EXT=y
```

### Rust Toolchain

- Rust **stable ≥ 1.80** (the project builds with the current stable toolchain; Rust 2021 edition).

```bash
# Install or update Rust via rustup:
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup update stable
```

### System Libraries

- `clang` / `llvm` (BPF compilation)
- `libbpf` development headers
- `libelf` development headers
- `zlib` development headers
- `bpftool` (optional, for BPF object inspection)

[↑ Back to Table of Contents](#table-of-contents)

---

## Build

### Quick Build

All SCX ecosystem crates are published on [crates.io](https://crates.io). No external checkouts are required:

```bash
git clone https://github.com/galpt/scx_cognis
cd scx_cognis
cargo build --release
```

The resulting binary is at `target/release/scx_cognis`.

### Build from Source

```bash
# 1. Install Rust (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# 2. Clone the repository
git clone https://github.com/galpt/scx_cognis
cd scx_cognis

# 3. Build in release mode
cargo build --release

# 4. (Optional) Run unit tests — no root or BPF kernel support needed
cargo test

# 5. Run with debug output
RUST_LOG=debug cargo build && sudo ./target/debug/scx_cognis -v
```

[↑ Back to Table of Contents](#table-of-contents)

---

## Usage

### Basic Usage

```bash
# Launch the scheduler (requires root / CAP_SYS_ADMIN):
sudo ./target/release/scx_cognis

# Stop: press Ctrl+C in the terminal, or send SIGINT/SIGTERM.
```

### TUI Dashboard

`--tui` starts `scx_cognis` as the active scheduler **with** a live dashboard. Because only one sched_ext scheduler can run at a time, you must stop the systemd service first if it is running:

```bash
# Stop the background service, then launch with TUI:
sudo systemctl stop scx && sudo scx_cognis --tui
```

> [!NOTE]
> Pressing `q` or `Esc` exits the TUI and stops the scheduler. The `scx.service` will **not** restart automatically — run `sudo systemctl start scx` to bring it back.

For live stats while the service is running (no TUI, no scheduler restart needed):

```bash
scx_cognis --monitor 1.0
```

### Command-Line Options

```
Usage: scx_cognis [OPTIONS]

Options:
  -s, --slice-us <SLICE_US>
          Base scheduling slice duration in microseconds [default: 5000]
  -S, --slice-us-min <SLICE_US_MIN>
          Minimum scheduling slice duration in microseconds [default: 1000]
  -l, --percpu-local
          Dispatch per-CPU tasks directly to their only eligible CPU
  -p, --partial
          Only manage tasks with SCHED_EXT policy (partial mode)
      --exit-dump-len <EXIT_DUMP_LEN>
          Exit debug dump buffer length; 0 = default [default: 0]
  -v, --verbose
          Enable verbose output (BPF details and tracefs events)
      --restricted-cpus <RESTRICTED_CPUS>
          CPUs reserved for quarantined tasks (0 = disable quarantine) [default: 1]
  -t, --tui
          Launch the ratatui TUI dashboard
      --stats <STATS>
          Enable stats monitoring with the specified interval (seconds)
      --monitor <MONITOR>
          Run in stats monitoring mode only (scheduler not launched)
      --help-stats
          Show descriptions for statistics
  -V, --version
          Print scheduler version and exit
  -h, --help
          Print help
```

### Stats Monitoring

Monitor live statistics from a second terminal while the scheduler runs. `--monitor` connects to the stats socket at `/run/scx/root/stats` — no root required (the service file sets `UMask=0111` so the socket is world-accessible):

```bash
# Monitor stats every second:
scx_cognis --monitor 1.0

# Monitor every 500 ms:
scx_cognis --monitor 0.5
```

> [!NOTE]
> If the scheduler was started **without** the provided service file (e.g. a manually launched instance), the socket may be root-only. In that case prefix with `sudo`.

[↑ Back to Table of Contents](#table-of-contents)

---

### How to Read Cognis Statistics

Each line from `--monitor` is a snapshot of one polling interval. All counters labelled as **per-interval** show how many events happened since the last sample; all others are instantaneous readings.

#### Full Output Format

```
[cognis] tldr: Rest assured! I'm keeping your system responsive.        | r:  5/16  q:1 /0   | pf:0 | d→u:312   k:140 c:0  b:0  f:0  | cong:0 | 🧠 Interactive:18  Compute:3  IOwait:2  RT:0  Unknown:0 | quarantine:0 flagged:0 | slice:4000µs reward:0.42
```

#### Column Reference

| Column | Full Name | Type | Meaning |
|:---|:---|:---|:---|
| `tldr: ...` | human summary | computed | One-line plain-English summary of current system health. Changes every interval based on load, reward, congestion, and threat level. See the [TLDR message reference](#tldr-message-reference) below. |
| `r: 5/16` | running / online CPUs | instant | Tasks actively executing right now out of total online CPUs. High ratios (≥ 0.8) mean the system is busy. |
| `q:1 /0` | queued / scheduled | instant | `queued` = tasks handed by the kernel to userspace and waiting for a dispatch decision; `scheduled` = tasks that have been ordered but not yet sent back to BPF. Under normal load both stay near 0. |
| `pf:0` | major page faults | per-interval | **Major** page faults (hard faults requiring disk I/O) inside the scheduler process per interval. Non-zero means the scheduler binary itself was partially swapped to disk — this causes real latency spikes and indicates memory pressure. Minor faults (normal anonymous-memory mapping) are intentionally excluded. Should always be **0** on a healthy system. |
| `d→u:312` | user dispatches | per-interval | Tasks dispatched **by the Cognis userspace scheduler** in this interval. The primary work-done counter. |
| `k:140` | kernel dispatches | per-interval | Tasks dispatched **by the kernel fallback path** (e.g. idle tasks, kthreads). A high ratio of `k` to `d→u` is normal. |
| `c:0` | cancel dispatches | per-interval | Dispatches cancelled before execution (task exited or migrated away). Usually 0. |
| `b:0` | bounce dispatches | per-interval | Dispatches that had to be redirected to a different DSQ (CPU affinity conflict). Occasional bounces are fine; sustained high values suggest affinity misconfiguration. |
| `f:0` | failed dispatches | per-interval | Dispatches that errored out entirely. Should always be **0**. |
| `cong:0` | congestion events | per-interval | Times the scheduler's internal queue was full and had to drop or defer work. Sustained non-zero values indicate scheduler overload. |
| `Interactive:18` | interactive events | per-interval | Scheduling events classified as **Interactive** (latency-sensitive: games, HID, GUI). Gets a 0.5× time-slice to stay responsive. |
| `Compute:3` | compute events | per-interval | Events classified as **Compute** (CPU-bound: compilers, encoders). Gets a 1× baseline time-slice (CPU-bound; pre-empted less often by design). |
| `IOwait:2` | I/O-wait events | per-interval | Events classified as **I/O Wait** (blocked on disk/network most of the time). Gets a 0.75× time-slice. |
| `RT:0` | realtime events | per-interval | Events classified as **RealTime** (JACK, audio daemons, SCHED_FIFO tasks). Gets a 0.25× time-slice for minimum latency. |
| `Unknown:1` | unclassified events | per-interval | Events that did not match any heuristic rule. In practice this should never appear with the current 3-band heuristic (every task is classified as Compute, Interactive, or IoWait). Gets a 1× baseline time-slice. |
| `quarantine:0` | quarantined PIDs | instant | PIDs currently throttled by the **Reputation Engine** for consistently burning 100% of their assigned slice (monopolising behaviour). They receive the minimum time-slice until their reputation recovers. |
| `flagged:0` | flagged TGIDs | instant | Thread-groups detected as outliers by the **Isolation Forest Anti-Cheat Engine** (statistical anomaly in scheduling behaviour). Flagged tasks are isolated to prevent them from starving others. |
| `slice:4000µs` | policy time-slice | instant | The **Q-learning Policy Controller**'s current base time-slice in microseconds. The controller adjusts this every ~250 ms based on the reward signal — it shrinks when Interactive tasks are well-served and grows when the system is I/O-bound. |
| `reward:0.42` | reward EMA | instant | Exponential moving average of the scheduler's **reward function**: `R = (interactive_frac × load_norm) × 0.7 − congestion × 0.2 − latency × 0.1`. Values near **1.0** are ideal (Interactive tasks served under load); near **0** means mostly Compute tasks dominating; negative values indicate sustained high congestion. |

#### Classification Label Deep-Dive

The classifier uses a **deterministic 3-band heuristic** evaluated stateless on every scheduling event. There is no sliding window or voting — this prevents feedback-loop misclassification while maintaining O(1) latency.

The key feature is `cpu_intensity = burst_ns / prev_assigned_slice_ns` (slice-usage fraction):

| Label | Slice Multiplier | Heuristic Rule |
|:---|:---|:---|
| **RealTime** | 0.25× | priority weight > 95% of max (SCHED_FIFO / SCHED_RR tasks) |
| **Compute** | 1.0× | `cpu_intensity > 0.85` — task consumed > 85% of its assigned slice (CPU-bound) |
| **Interactive** | 0.5× | `0.10 ≤ cpu_intensity ≤ 0.85` — task yields regularly (latency-sensitive) |
| **IoWait** | 0.75× | `cpu_intensity < 0.10` — task released CPU far before slice expired (I/O-blocked) |
| **Unknown** | 1.0× | reserved; not emitted by the current heuristic |

> [!TIP]
> **Why does Interactive dominate?**
>
> Most desktop, service, and shell tasks yield before consuming their full slice, giving `cpu_intensity` values in the 0.1–0.85 range. Pure CPU workloads (compilers, encoders, stress-ng) consume their entire slice and jump to `cpu_intensity > 0.85` within one or two scheduling events.

#### TLDR Message Reference

Messages are evaluated each interval in **highest-severity-first** order. The first matching condition wins.

| Message | Condition | What to do |
|:---|:---|:---|
| `I'm being swapped out! Latency will spike — check available RAM!` | `pf > 0` per interval — scheduler had major (hard) page faults this second | Free memory; the scheduler process should never be swapped |
| `Dispatch failures detected! Something unexpected went wrong — check dmesg.` | `failed_dispatches > 0` | Run `sudo dmesg \| grep sched` and file a bug |
| `SOS! The system is overwhelmed. Hanging on by a thread here!` | `reward < −0.5` — deep, sustained congestion | Reduce workload or reboot; something is seriously wrong |
| `Under siege! Multiple rule-breakers caught and caged — enforcing order.` | `flagged > 5` **and** `quarantined > 5` | Normal if running untrusted workloads; cognis is handling it |
| `Suspicious behaviour detected! Isolating troublemakers — your system is protected.` | `flagged > 0` — anti-cheat engine fired | Inspect flagged processes with `ps aux` |
| `Several greedy tasks are throttled — keeping them from hogging your CPU.` | `quarantined > 3` | Some processes keep burning 100% of their slice; they are being rate-limited |
| `Caught a greedy task! Putting it on a leash so other tasks can breathe.` | `quarantined > 0` | A process exceeded its slice budget; reputation engine is throttling it |
| `Oh boy! Things are getting really busy. Tightening the reins...` | `congestion > 10` per interval | High burst of work; cognis is adapting — sustained = consider tuning `--slice-us` |
| `Getting a little crowded in here, but I've got it handled.` | `congestion > 0` | Transient queue build-up; normal under bursty load |
| `Working hard under pressure — might get bumpy. Stay with me!` | `reward < 0` (no explicit congestion) | Latency/throughput imbalance; usually self-corrects |
| `Your CPU is at full throttle! Giving compute tasks the runway they need.` | `load ≥ 85%`, compute-dominated | CPU-bound workload (compilation, encoding); expected behaviour |
| `Busy but responsive! Juggling lots of interactive tasks like a pro.` | `load ≥ 85%`, interactive-dominated | Heavy desktop/gaming load; cognis is prioritising responsiveness |
| `Running hot! Balancing a heavy mixed workload across all cores.` | `load ≥ 85%`, mixed | All cores busy with varied work |
| `A solid workload — distributing tasks evenly and keeping things smooth.` | `load 65–85%` | Normal moderately-loaded system |
| `Smooth sailing! Everything is running beautifully right now.` | `reward ≥ 0.7`, `load < 50%` | Ideal operating conditions |
| `Rest assured! I'm keeping your system responsive.` | `reward ≥ 0.4`, interactive-heavy | System healthy, desktop/UI is snappy |
| `Compute tasks are in full swing — throughput maximised, interactivity preserved.` | `reward ≥ 0.4`, compute-heavy | Background compute running efficiently without hurting interactivity |
| `Balancing work steadily — nothing to worry about.` | `reward ≥ 0.4`, balanced | Healthy mixed workload |
| `System is mostly idle. Just here waiting to help!` | `load < 10%` | Light load; cognis is in standby |
| `Keeping an eye on things — all nominal.` | fallback | Default: nothing notable to report |

[↑ Back to Table of Contents](#table-of-contents)

---

## Installation Guide

### Using install.sh (Recommended)

`install.sh` is a self-contained POSIX shell script that handles the full system-wide installation in one command. It requires root.

```bash
sudo sh install.sh
```

**What it does — in order:**

| Step | Action |
|:---|:---|
| 1 | Detects your CPU architecture (`x86_64` pre-built; other arches require `--build-from-source`) |
| 2 | Detects your distro (`CachyOS`, `Arch`, `Ubuntu`, `Debian`, or generic) |
| 3 | Checks that your running kernel has `CONFIG_SCHED_CLASS_EXT=y` (advisory — warns but does not abort) |
| 4 | **Downloads** the pre-built binary from GitHub Releases into `/usr/bin/scx_cognis` (or **compiles** it locally if `--build-from-source` is given) |
| 5 | On **Arch / CachyOS**: installs `scx-manager` (which provides `/etc/systemd/system/scx.service` and `/etc/default/scx`); falls back to writing its own service file if `scx-manager` is unavailable |
| 6 | On **Ubuntu / Debian**: writes `/etc/systemd/system/scx.service` (skipped if a service file is already present) |
| 7 | Backs up any existing `/etc/default/scx` to `/etc/default/scx.bak`, then writes `SCX_SCHEDULER=scx_cognis` (and any custom `SCX_FLAGS`) |
| 8 | Runs `systemctl daemon-reload`, enables, and starts `scx.service` |

**Options:**

| Flag | Description |
|:---|:---|
| `--version TAG` | Install a specific release tag (e.g. `v0.1.5`); default: `latest` |
| `--build-from-source` | Compile the binary locally instead of downloading a pre-built archive |
| `--dry-run` | Print every action that *would* be taken without making any changes |
| `--force` | Skip all interactive confirmation prompts |
| `--flags "..."` | Custom scheduler flags written to `SCX_FLAGS` in `/etc/default/scx` (e.g. `--restricted-cpus 2`) |

**Examples:**

```bash
# Install the latest release (default — asks for confirmation once):
sudo sh install.sh

# Preview every action without touching the system:
sudo sh install.sh --dry-run

# Install a specific version with custom flags, no prompts:
sudo sh install.sh --version v0.1.5 --flags "--restricted-cpus 2" --force

# Build and install from the local source tree:
sudo sh install.sh --build-from-source
```

> [!NOTE]
> After installation the scheduler starts automatically on every boot via `scx.service`. If the user-space daemon ever crashes, the kernel's sched_ext watchdog reverts all tasks to the default scheduler (CFS/EEVDF) within ~50 ms — there is no risk of a kernel panic or system lockup.
>
> **TUI and the systemd service cannot run at the same time.** `--tui` starts its own scheduler instance; if `scx.service` is already active you will get `Error: another sched_ext scheduler is already running`. To use the TUI, stop the service first:
> ```bash
> sudo systemctl stop scx && sudo scx_cognis --tui
> ```
> For live stats *without* stopping the service, use `--monitor` (the service file sets socket permissions so no root is needed):
> ```bash
> scx_cognis --monitor 1.0
> ```

[↑ Back to Table of Contents](#table-of-contents)

---

### Using uninstall.sh

`uninstall.sh` cleanly removes everything the installer put in place. It requires root.

```bash
sudo sh uninstall.sh
```

**What it does — in order:**

| Step | Action |
|:---|:---|
| 1 | Stops and disables `scx.service` via `systemctl` |
| 2 | Reverts `/etc/default/scx`: restores the `.bak` backup if the installer left one, otherwise surgically removes only the `SCX_SCHEDULER=scx_cognis` and `SCX_FLAGS` lines it owns |
| 3 | Deletes `/usr/bin/scx_cognis` |
| 4 | Runs `systemctl daemon-reload` |

The kernel reverts to its default scheduler (CFS/EEVDF) the moment `scx.service` is stopped in step 1.

**Options:**

| Flag | Description |
|:---|:---|
| `--dry-run` | Print every action that *would* be taken without making any changes |
| `--force` | Skip the confirmation prompt |
| `--purge` | Also remove `/etc/systemd/system/scx.service` — **only** if that file was originally created by this installer (detected by its content); distro-managed service files are left untouched |

**Examples:**

```bash
# Standard uninstall (asks for confirmation once):
sudo sh uninstall.sh

# Preview what would be removed without touching the system:
sudo sh uninstall.sh --dry-run

# Uninstall and also remove the scx service file (if the installer created it):
sudo sh uninstall.sh --purge

# Fully silent removal:
sudo sh uninstall.sh --force --purge
```

> [!NOTE]
> `--purge` is only needed if you want to completely remove the `scx.service` unit. On Arch/CachyOS where `scx-manager` owns that file, `--purge` will print a warning and leave the file alone to avoid breaking the package manager's state.

[↑ Back to Table of Contents](#table-of-contents)

---

### CachyOS

CachyOS ships a `sched_ext`-enabled kernel by default, making it the easiest platform to run `scx_cognis` on.

#### Step 1 — Install build dependencies

```bash
sudo pacman -S --needed clang llvm libbpf libelf zlib bpftool rust
```

#### Step 2 — Install Rust (if not already present via rustup)

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup update stable
```

#### Step 3 — Clone and build

```bash
git clone https://github.com/galpt/scx_cognis
cd scx_cognis
cargo build --release
```

#### Step 4 — Run

```bash
# Without TUI:
sudo ./target/release/scx_cognis

# With TUI dashboard:
sudo ./target/release/scx_cognis --tui

# With a shorter base slice (good for desktop / interactive workloads):
sudo ./target/release/scx_cognis --tui -s 10000
```

#### Step 5 — (Optional) Install system-wide

```bash
sudo install -m755 target/release/scx_cognis /usr/local/bin/scx_cognis
```

#### Step 6 — Register as the system-default scheduler via CachyOS Hello

CachyOS ships an `scx` systemd service and a GUI helper called **CachyOS Hello** that makes it easy to switch the active sched_ext scheduler.

**Option A — CachyOS Hello (recommended for new users)**

1. Open **CachyOS Hello** from the application launcher (it auto-starts on first boot and is also in the system menu).
2. Click the **Tweaks** tab at the top.
3. Under the **Scheduler** section, find the *sched_ext Scheduler* dropdown.
4. Select **scx_cognis** from the list (it will appear once the binary is installed system-wide from Step 5).
5. Click **Apply** — CachyOS Hello will write the choice to `/etc/default/scx`, reload the `scx` daemon, and activate `scx_cognis` immediately.

**Option B — Manual (terminal)**

```bash
# Edit the scx service configuration:
sudo nano /etc/default/scx

# Set the scheduler to scx_cognis:
SCX_SCHEDULER=scx_cognis
SCX_FLAGS="--restricted-cpus 1"

# Restart the scx service to apply:
sudo systemctl restart scx

# Verify it is running:
sudo systemctl status scx
# The log should show: "scx_cognis: started"
```

**Stop / switch back to default**

```bash
# Stop scx_cognis and return to the kernel's default CFS scheduler:
sudo systemctl stop scx

# Or open CachyOS Hello → Tweaks → Scheduler → select a different scheduler → Apply.
```

> [!NOTE]
> Only one sched_ext scheduler can be active at a time. If `scx_lavd`, `scx_bpfland`, or another scheduler is already running via `scx.service`, the step above replaces it automatically.

[↑ Back to Table of Contents](#table-of-contents)

### Arch Linux and Manjaro

#### Kernel requirement

Install a sched_ext-capable kernel:

```bash
# From the AUR (linux-sched-ext):
yay -S linux-sched-ext linux-sched-ext-headers

# Or use the CachyOS kernel (if on Arch):
# https://wiki.cachyos.org/en/home/Installation
```

#### Build dependencies

```bash
sudo pacman -S --needed clang llvm libbpf libelf zlib bpftool
```

#### Build and run

```bash
git clone https://github.com/galpt/scx_cognis
cd scx_cognis
cargo build --release
sudo ./target/release/scx_cognis --tui
```

[↑ Back to Table of Contents](#table-of-contents)

### Ubuntu and Debian

#### Kernel requirement

Ubuntu 24.04 LTS ships a kernel that can be upgraded to one with sched_ext support:

```bash
# Install the linux-generic-hwe kernel (>= 6.12):
sudo apt install linux-generic-hwe-24.04
sudo reboot

# Alternatively, use the mainline kernel PPA:
sudo add-apt-repository ppa:cappelikan/ppa
sudo apt install mainline
# Then use the mainline tool to install a sched_ext kernel >= 6.12
```

#### Build dependencies

```bash
sudo apt install -y \
    clang llvm \
    libbpf-dev libelf-dev zlib1g-dev \
    linux-tools-common linux-tools-$(uname -r) \
    pkg-config
```

#### Build and run

```bash
git clone https://github.com/galpt/scx_cognis
cd scx_cognis
cargo build --release
sudo ./target/release/scx_cognis --tui
```

[↑ Back to Table of Contents](#table-of-contents)

### Running as a systemd Service

Create `/etc/systemd/system/scx_cognis.service`:

```ini
[Unit]
Description=scx_cognis adaptive CPU scheduler
Documentation=https://github.com/galpt/scx_cognis
After=local-fs.target
ConditionKernelVersion=>=6.12

[Service]
Type=simple
ExecStart=/usr/local/bin/scx_cognis --restricted-cpus 1
Restart=on-failure
RestartSec=5
KillMode=process
OOMScoreAdjust=-900

[Install]
WantedBy=multi-user.target
```

Then enable and start it:

```bash
sudo install -m755 target/release/scx_cognis /usr/local/bin/scx_cognis
sudo systemctl daemon-reload
sudo systemctl enable --now scx_cognis
sudo systemctl status scx_cognis
```

[↑ Back to Table of Contents](#table-of-contents)

---

## Benchmarks

The easiest way to experience the difference scx_cognis makes is to run a CPU stress workload while the browser renders a heavy WebGL scene. `cognis_benchmark.sh` automates this for you.

### Running the Benchmark

```bash
chmod +x cognis_benchmark.sh
./cognis_benchmark.sh
```

The script presents two modes:

| Mode | What it does |
|:----:|:-------------|
| **1 — Baseline** | Runs the workload with the kernel's default scheduler (CFS/EEVDF). Use this to establish a performance baseline. |
| **2 — With scx_cognis** | Runs the same workload while scx_cognis is active. Start the scheduler before selecting this mode. |

Each mode runs three 60-second stress-ng phases (CPU → I/O → Mixed) while the [WebGL Aquarium](https://webglsamples.org/aquarium/aquarium.html) is open in your browser.

> [!TIP]
> Run the script **twice** — once per mode — and compare the Aquarium smoothness and `bogo-ops/s` values between runs.

### What to Watch During the Benchmark

**In the browser (WebGL Aquarium):**
- Use the **default 500 fish** — it provides realistic GPU + CPU load without overwhelming lower-end machines.
- Watch for smooth, consistent animation (≥ 30 fps).
- Click the Fish Count slider mid-test — it should respond instantly even under full load.

**In `scx_cognis --monitor 1.0` (open a second terminal when using Mode 2):**

| Field | What to look for |
|:------|:-----------------|
| `tldr:` | Should stay in *"Rest assured"*, *"Busy but responsive"*, or *"Smooth sailing"* |
| `d→u` vs `k` | User dispatches (`d→u`) should be non-trivial; `k >> d→u` means cognis isn't keeping up |
| `Interactive` | Should remain the dominant label — browser/compositor tasks stay prioritised |
| `Compute` | Will rise during the CPU phase — expected and correct |
| `cong` | Occasional spikes are fine; sustained high values = scheduler under pressure |
| `slice` | Should shrink during interactive phases and grow during compute phases |
| `reward` | Aim for ≥ 0.3 throughout; lower values mean the policy is compensating for a compute-heavy workload |

### Interpreting Results

scx_cognis prioritises **Interactive** tasks with a 0.5× shorter time-slice so the browser's compositing thread pre-empts compute-bound stress workers more frequently. You should observe:

- **Smoother Aquarium animation** under load compared to the baseline.
- **Lower raw bogo-ops/s for CPU-bound phases** — this is expected and by design. scx_cognis pre-empts compute workers more frequently to keep the browser compositor responsive. The `usr time` metric stays close to or above baseline, confirming no CPU cycles are wasted.
- **Lower perceived input latency** — the Fish Count slider and tab switching should feel snappy even while CPUs are saturated.

If the `reward` value stays low (near 0) during CPU-heavy phases, this is normal — `interactive_frac` is low when stress-ng workers dominate, which pulls the reward signal down. It does not indicate a problem.

### Test Platform

| Component | Details |
|:----------|:--------|
| Machine | Lenovo IdeaPad Gaming 3 15ARH7 |
| CPU | AMD Ryzen 7 6800H — 16 logical cores @ 4.79 GHz |
| RAM | 58.54 GiB |
| OS | CachyOS x86_64 |
| Kernel | Linux 6.19.5-3-cachyos |
| DE/WM | KDE Plasma 6.6.1, KWin (Wayland) |
| Display | 1920×1080 @ 120 Hz |

### Reference Benchmark Results

> [!NOTE]
> 500 fish (default Aquarium), 60 s per phase, `stress-ng` workload. CPU governor left at system default (`powersave`) for both runs.
>
> **Context:**
> These numbers reflect the current implementation. The real-time throughput gap for CPU-bound phases is expected and by design — scx_cognis pre-empts compute workers more frequently to keep interactive tasks (browser compositor, input handling) responsive. The `usr time` metric, which measures only the CPU time workers were actually executing, stays close to or above baseline, confirming that no CPU cycles are wasted. The tradeoff is intentional: lower raw compute throughput in exchange for sustained frame-rate smoothness under full CPU load.

**Phase 1 — CPU stress (16 × workers, 60 s):**

| Metric | Baseline (CFS) | scx_cognis | Δ |
|:-------|:--------------:|:----------:|:---:|
| bogo ops/s (real time) | 20,824 | 2,081 | −90.0% |
| bogo ops/s (usr time) | 1,384 | 2,077 | +50.1% |

The real-time drop reflects frequent pre-emption of compute workers by higher-priority interactive tasks. The usr-time gain shows that when workers do run, they execute more efficiently — no CPU cycles are wasted between scheduling events.

**Phase 2 — I/O stress (4 × `iomix` workers, 60 s):**

| Metric | Baseline (CFS) | scx_cognis | Δ |
|:-------|:--------------:|:----------:|:---:|
| bogo ops/s (real time) | 182,051 | 171,387 | −5.9% |
| bogo ops/s (usr time) | 41,935 | 42,999 | +2.5% |

I/O-bound workers sleep frequently on disk I/O, so they are rarely pre-empted by the Interactive path. Throughput stays within 6% of baseline in both metrics.

**Phase 3 — Mixed stress (16 × `cpu` + 2 × `vm` workers, 60 s):**

| Stressor | Metric | Baseline (CFS) | scx_cognis | Δ |
|:---------|:-------|:--------------:|:----------:|:---:|
| CPU ×16 | bogo ops/s (real) | 18,618 | 1,827 | −90.2% |
| CPU ×16 | bogo ops/s (usr) | 1,389 | 2,053 | +47.9% |
| VM ×2 | bogo ops/s (real) | 24,523 | 19,146 | −21.9% |
| VM ×2 | bogo ops/s (usr) | 14,949 | 160,034 | +970.7% |

The CPU stressor shows the same pattern as Phase 1. The VM stressor's exceptional usr-time gain reflects cognis giving short, frequent slices to tasks that yield quickly on memory allocation — the exact wakeup pattern the Interactive heuristic is designed to reward.

> [!TIP]
> If you run your own benchmark, keep the CPU governor fixed (`performance`) for both schedulers, run each mode at least 3 times, and compare median values. Single runs can swing significantly.

**Cognis vs. CachyOS Default Scheduler**

Both screenshots show CPU usage during the transition from Phase 2 to Phase 3 of the benchmark.

<p align="center">
	<img src="https://github.com/galpt/scx_cognis/blob/main/img/baseline-cpu-usage.png" alt="With Cognis Disabled" style="max-width:100%;height:auto;" />
	<br/>
	<em>With Cognis Disabled</em>
</p>

<p align="center">
	<img src="https://github.com/galpt/scx_cognis/blob/main/img/cognis-cpu-usage.png" alt="With Cognis Enabled" style="max-width:100%;height:auto;" />
	<br/>
	<em>With Cognis Enabled</em>
</p>

[↑ Back to Table of Contents](#table-of-contents)

---

## Limitations and Next Steps

- **Elman RNN weights are hardcoded** — the burst predictor uses a fixed H=4 Elman RNN with weights derived from offline synthetic-trace analysis. The EMA smoothing layer adapts predictions toward observed burst distributions at runtime, but weight updates require gradient computation incompatible with the <5 µs hot-path budget. Online or offline retraining remains a future step.

[↑ Back to Table of Contents](#table-of-contents)

---

## Contributing

### Running Tests

Unit tests for all modules run without root or BPF kernel support:

```bash
cargo test
```

Code style and lint checks:

```bash
cargo fmt --check
cargo clippy -- -D warnings
```

### Adding a New Module

All components live in `src/ai/`. Each module is self-contained (no external framework dependencies — only `rand` for modules that need randomness):

1. Create `src/ai/my_module.rs` with your struct and unit tests.
2. Add `pub mod my_module;` and a `pub use` re-export to `src/ai/mod.rs`.
3. Instantiate and wire the new component in `src/main.rs`.
4. Add the relevant metric(s) to `src/stats.rs` if you want them exposed via `--monitor`.

[↑ Back to Table of Contents](#table-of-contents)

---

## License

[GPL-2.0-only](LICENSE)

Compatible with Linux kernel symbols.

[↑ Back to Table of Contents](#table-of-contents)
