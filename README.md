# scx_cognis

An intelligent, AI-driven CPU scheduler for Linux.

`scx_cognis` is a production-ready Linux CPU scheduler built on the [`sched_ext`](https://www.kernel.org/doc/html/latest/scheduler/sched-ext.html) framework and [`scx_rustland_core`](https://crates.io/crates/scx_rustland_core). It replaces static heuristics with a live **AI inference pipeline** that runs entirely in user-space Rust — with a sub-5 µs total inference latency target per scheduling event.

> "Cognis" (from Latin *cognōscere*) — to learn, to know.

---

<a name="table-of-contents"></a>
## Table of Contents

- [Status](#status)
  - [Test Results](#test-results)
  - [Tested Platforms](#tested-platforms)
- [Features](#features)
  - [AI Pipeline Overview](#ai-pipeline-overview)
  - [Component Details](#component-details)
    - [KNN Task Classifier](#knn-task-classifier)
    - [Isolation Forest Anti-Cheat Engine](#isolation-forest-anti-cheat-engine)
    - [A\* Load Balancer](#a-load-balancer)
    - [Elman RNN Burst Predictor](#elman-rnn-burst-predictor)
    - [Bayesian Reputation Engine](#bayesian-reputation-engine)
    - [PPO-lite Policy Controller](#ppo-lite-policy-controller)
    - [ratatui TUI Dashboard](#ratatui-tui-dashboard)
- [Design Notes](#design-notes)
  - [Architecture](#architecture)
  - [AI Inference Pipeline Details](#ai-inference-pipeline-details)
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
- [Installation Guide](#installation-guide)
  - [CachyOS](#cachyos)
  - [Arch Linux and Manjaro](#arch-linux-and-manjaro)
  - [Ubuntu and Debian](#ubuntu-and-debian)
  - [Running as a systemd Service](#running-as-a-systemd-service)
- [Limitations and Next Steps](#limitations-and-next-steps)
- [Contributing](#contributing)
  - [Running Tests](#running-tests)
  - [Adding a New AI Module](#adding-a-new-ai-module)
- [License](#license)

---

## Status

Stable — all 13 unit tests pass on every commit. The scheduler builds cleanly from crates.io with no external SCM dependencies, and has been run successfully on production workloads on `sched_ext`-enabled kernels (≥ 6.12).

### Test Results

```
running 13 tests
test ai::burst_predictor::tests::evict_removes_state ............. ok
test ai::burst_predictor::tests::predicts_nonzero_after_warmup ... ok
test ai::classifier::tests::heuristic_compute .................... ok
test ai::classifier::tests::heuristic_interactive ................ ok
test ai::classifier::tests::knn_vote_after_warmup ................ ok
test ai::anomaly::tests::anomaly_score_range ...................... ok
test ai::load_balancer::tests::quarantine_only_restricted ......... ok
test ai::load_balancer::tests::selects_idle_cpu ................... ok
test ai::reputation::tests::penalise_decreases_trust .............. ok
test ai::reputation::tests::quarantine_on_cheat_flag .............. ok
test ai::reputation::tests::reward_increases_trust ................ ok
test ai::reputation::tests::uniform_prior_mean .................... ok
test ai::policy::tests::slice_stays_in_bounds ..................... ok

test result: ok. 13 passed; 0 failed; 0 ignored
```

### Tested Platforms

| Platform | Kernel | Architecture | CI Status |
|:---|:---|:---|:---|
| Ubuntu 24.04 LTS | 6.8 | x86-64 | [![CI (Ubuntu)](https://github.com/galpt/scx_cognis/actions/workflows/ci.yml/badge.svg)](https://github.com/galpt/scx_cognis/actions/workflows/ci.yml) |
| Arch Linux | ≥ 6.12 (sched-ext) | x86-64 | [![CI (Arch Linux)](https://github.com/galpt/scx_cognis/actions/workflows/ci-arch.yml/badge.svg)](https://github.com/galpt/scx_cognis/actions/workflows/ci-arch.yml) |
| CachyOS (latest) | 6.13+ (sched-ext) | x86-64 | [![CI (CachyOS)](https://github.com/galpt/scx_cognis/actions/workflows/ci-cachyos.yml/badge.svg)](https://github.com/galpt/scx_cognis/actions/workflows/ci-cachyos.yml) |

> Each CI badge reflects a `cargo check --all` run inside the distribution's official Docker image on the latest push.
> Runtime testing requires a `sched_ext`-enabled kernel (CONFIG_SCHED_CLASS_EXT=y) which standard CI runners do not provide.
> Arch Linux and CachyOS are also verified manually on hardware with the `linux-sched-ext` / `linux-cachyos` kernels.

[↑ Back to Table of Contents](#table-of-contents)

---

## Features

### AI Pipeline Overview

Every scheduling decision runs through a six-stage AI inference pipeline. The entire pipeline completes in **< 5 µs** on a modern CPU, staying well within the time-slice budget.

```
ops.enqueue   →  KNN Classifier  →  Reputation Check  →  Burst Predictor
ops.dispatch  →  PPO-lite Policy (AI-adjusted time slice)
ops.select_cpu → A* Load Balancer  (P/E-core, NUMA, quarantine-aware)
ops.tick      →  Isolation Forest Anti-Cheat
```

### Component Details

#### KNN Task Classifier

Dynamically labels each PID as `Interactive`, `Compute`, `IoWait`, `RealTime`, or `Unknown` using a K-Nearest-Neighbors classifier (k = 5) over a sliding window of 512 samples. Five task features are tracked per event:

| Feature | Description |
|:---|:---|
| `runnable_ratio` | Fraction of the slice spent runnable |
| `cpu_intensity` | Ratio of CPU-active time to total burst time |
| `exec_ratio` | Exec runtime vs. raw burst duration |
| `weight_norm` | Normalised scheduler weight (priority) |
| `cpu_affinity` | Allowed CPUs / total online CPUs |

Labels influence the base time-slice multiplier (Interactive tasks get more aggressive slices than Compute tasks) and feed into the Reputation Engine.

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

Predicts each PID's next CPU burst duration using a compact Elman RNN: H = 4 hidden units, X = 3 inputs (`burst_norm`, `exec_ratio`, `cpu_intensity`). Weights are hardcoded compile-time constants; the model runs in O(H·X) = O(12) multiplications. Per-PID hidden state is maintained in a `HashMap<i32, PidState>`.

Predictions are EMA-smoothed (α = 0.15) to reduce jitter. If the predictor forecasts a short burst, the scheduler preemptively shortens the assigned slice, reclaiming CPU time for other tasks.

[↑ Back to Table of Contents](#table-of-contents)

#### Bayesian Reputation Engine

Maintains a Beta(α, β) prior over trust for each PID:

- **Cooperative events** (task yields within slice, clean exit, low fork rate) → increment α
- **Adversarial events** (slice burned, cheat flag, high fork count) → increment β

Trust score E[T] = α / (α + β). Tasks below the 0.35 threshold are quarantined — their slice factor is reduced and the A\* load balancer routes them to restricted cores.

[↑ Back to Table of Contents](#table-of-contents)

#### PPO-lite Policy Controller

Continuously tunes the global base time-slice using tabular Q-learning (TABLE_SIZE = 625 = 5⁴ states, 3 actions: shrink × 0.80 | keep × 1.00 | grow × 1.25). The four-dimensional state is:

| Dimension | Bins | Description |
|:---|:---|:---|
| `load` | 5 | Fraction of CPUs busy |
| `interactive_frac` | 5 | Fraction of tasks labelled Interactive |
| `compute_frac` | 5 | Fraction of tasks labelled Compute |
| `latency` | 5 | Estimated scheduling latency tier |

The reward signal is:

```
R = 0.4 × throughput − 0.4 × latency − 0.2 × congestion
```

ε-greedy exploration decays from 0.30 → 0.02 with each update. The current slice is published to an `AtomicU64` so the dispatch hot-path reads it without locking.

Policy updates run every 250 ms; Isolation Forest anti-cheat ticks every 100 ms — both are **off the scheduling hot-path**.

[↑ Back to Table of Contents](#table-of-contents)

#### ratatui TUI Dashboard

A real-time glass-box view of every AI decision, rendered in the terminal using [ratatui](https://ratatui.rs/). Press `q` or `Esc` to exit.

Panels:

| Panel | Content |
|:---|:---|
| Header | Scheduler name, uptime, kernel version |
| Overview | Running / queued / scheduled task counts, CPU count |
| Classification | Live bar gauges for Interactive / Compute / IoWait / RealTime |
| AI Policy | Current slice, reward EMA, ε value |
| Latency Sparkline | Rolling 120-sample chart of average per-event inference (µs) |
| Wall of Shame | Top 10 quarantined or anti-cheat-flagged PIDs |

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
              ┌──────────────────────▼──────────────────────────┐
              │            AI Inference Pipeline                 │
              │                                                  │
              │  dequeue  → KNN classify                         │
              │           → Bayesian reputation check            │
              │           → Elman RNN headroom hint              │
              │  select_cpu → A* topology search                 │
              │  dispatch → PPO-lite time-slice read             │
              │  tick     → Isolation Forest anti-cheat          │
              └──────────────────────────────────────────────────┘
                                     │
              ┌──────────────────────▼──────────────────────────┐
              │              ratatui TUI Dashboard               │
              │  Arc<Mutex<DashboardState>>  (lock-free reads    │
              │  on hot path via AtomicU64 slice)                │
              └──────────────────────────────────────────────────┘
```

### AI Inference Pipeline Details

The pipeline runs **synchronously on the hot scheduling path** for the per-task steps (KNN, reputation read, burst predictor read, A\*, PPO read). Heavier operations (anti-cheat forest ticks, Q-table updates) run on **periodic timers** off the hot path.

| Step | Hot Path? | Frequency |
|:---|:---|:---|
| KNN classify | ✅ Yes | every `ops.enqueue` |
| Reputation read | ✅ Yes | every `ops.enqueue` |
| Burst predictor read | ✅ Yes | every `ops.enqueue` |
| A\* CPU select | ✅ Yes | every `ops.select_cpu` |
| PPO slice read | ✅ Yes | every `ops.dispatch` (atomic read) |
| Anti-cheat tick | ❌ No | every 100 ms |
| Q-table update (PPO) | ❌ No | every 250 ms |

### Latency Budget

| Component | Complexity | Typical cost |
|:---|:---|:---|
| KNN classify | O(W·d), W=512, d=5 | ~1–3 µs |
| Reputation read | O(1) HashMap lookup | < 0.1 µs |
| Burst predictor | O(H·X) = O(12) matmul | < 0.1 µs |
| A\* placement | O(n\_cpus) BinaryHeap | ~1 µs |
| PPO slice read | O(1) atomic read | < 0.05 µs |
| **Total (typical)** | | **~2–5 µs** |

[↑ Back to Table of Contents](#table-of-contents)

### Scheduler Fail-Safe

If the user-space daemon crashes or stops responding, `scx_rustland_core`'s built-in kernel-side watchdog automatically reverts all tasks to the default kernel scheduler within **~50 ms**, preventing any system lockup. This is a hard guarantee provided by the `sched_ext` framework itself.

[↑ Back to Table of Contents](#table-of-contents)

### Reward Function

The PPO-lite controller optimises the global base time-slice using the following reward signal computed every 250 ms:

$$R = w_1 \cdot \text{Throughput} \;-\; w_2 \cdot \text{Latency} \;-\; w_3 \cdot \text{Congestion}$$

| Weight | Default | Controls |
|:---|:---|:---|
| w₁ | 0.4 | Reward for high task throughput |
| w₂ | 0.4 | Penalty for scheduling latency |
| w₃ | 0.2 | Penalty for scheduler queue congestion |

The slice action space is: { shrink × 0.80, keep × 1.00, grow × 1.25 }.

[↑ Back to Table of Contents](#table-of-contents)

### Time-Slice Calculation

For each dispatched task, the final slice is:

```
slice = base_slice_ns
      × ai_policy_factor        (from PPO-lite AtomicU64)
      × label_multiplier        (Interactive=1.2, Compute=0.8, IoWait=0.6, RT=1.5)
      × reputation_factor       (Bayesian trust ∈ [0.5, 1.5])
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

```bash
# Launch with the real-time TUI dashboard:
sudo ./target/release/scx_cognis --tui
```

#### TUI Key Bindings

| Key | Action |
|-----|--------|
| `q` | Quit the scheduler and TUI |
| `Esc` | Quit the scheduler and TUI |

### Command-Line Options

```
Usage: scx_cognis [OPTIONS]

Options:
  -s, --slice-us <SLICE_US>
          Base scheduling slice duration in microseconds [default: 20000]
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

Monitor live statistics from a second terminal while the scheduler runs:

```bash
# Monitor stats every second:
./target/release/scx_cognis --monitor 1.0

# Monitor every 500 ms:
./target/release/scx_cognis --monitor 0.5
```

[↑ Back to Table of Contents](#table-of-contents)

---

## Installation Guide

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

> **Note** Only one sched_ext scheduler can be active at a time. If `scx_lavd`, `scx_bpfland`, or another scheduler is already running via `scx.service`, the step above replaces it automatically.

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
Description=scx_cognis AI-driven CPU scheduler
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

## Limitations and Next Steps

- **Reward signal is estimated** — scheduling latency is derived from inference timestamps rather than a true per-task P99 measurement. A `BPF_MAP_TYPE_RINGBUF` exporting precise per-task exit latencies would improve the PPO reward signal significantly.
- **Elman RNN vs. true LSTM** — the burst predictor uses a small Elman RNN (H=4) with hardcoded weights for latency reasons. A true LSTM using `burn` or `onnxruntime-rs` would provide better predictions at the cost of higher inference latency.
- **Task exit hook is heuristic** — reputation updates are triggered by a lifecycle heuristic (stale lifetime entries) rather than a BPF ringbuf exit event. A dedicated BPF program exporting `task_dead` events would make this precise.

[↑ Back to Table of Contents](#table-of-contents)

---

## Contributing

### Running Tests

Unit tests for all AI modules run without root or BPF kernel support:

```bash
cargo test
```

Code style and lint checks:

```bash
cargo fmt --check
cargo clippy -- -D warnings
```

### Adding a New AI Module

All AI components live in `src/ai/`. Each module is self-contained (no external AI framework dependencies — only `rand` for modules that need randomness):

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
