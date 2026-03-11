# scx_cognis

Sometimes a Linux desktop feels fine right up until it doesn't. A browser starts rendering something heavy, a build kicks off in the background, a VM wakes up, and suddenly the part you actually care about, the window you are touching right now, stops feeling immediate.

`scx_cognis` is an experiment in pushing back on that failure mode.

It is a Rust userspace scheduler built on top of `sched_ext` and `scx_rustland_core`. Cognis combines a deterministic task classifier, a fixed-size trust table, a compact burst predictor with bounded online correction, and a load-driven slice controller with bounded per-task interactive renewal, per-PID calibration, and a wake-preempt proxy for fresh interactive wakeups. The goal is not to be magical. The goal is simpler: keep interactive work responsive under pressure without turning the scheduler itself into the problem.

That framing matters, because Cognis is still exactly what it says it is: **an attempt at an adaptive CPU scheduler for interactive desktops and workstations**. It currently builds, tests, benchmarks, and runs on `sched_ext`-enabled systems, but it is still an implementation-driven project rather than a broadly validated production scheduler.

---

## Table of Contents

- [Status](#status)
- [Setting the Stage](#setting-the-stage)
  - [Why this project exists](#why-this-project-exists)
  - [What Cognis does today](#what-cognis-does-today)
  - [Where Cognis fits among sched_ext schedulers](#where-cognis-fits-among-sched_ext-schedulers)
- [How Cognis Works](#how-cognis-works)
  - [The hot path in one pass](#the-hot-path-in-one-pass)
  - [Why user tasks go through SHARED_DSQ](#why-user-tasks-go-through-shared_dsq)
  - [How tasks are classified](#how-tasks-are-classified)
  - [How slices are adjusted](#how-slices-are-adjusted)
  - [Autopilot: always-on conservative autoslicing](#autopilot-always-on-conservative-autoslicing)
  - [Trust tracking and burst prediction](#trust-tracking-and-burst-prediction)
  - [Observability: monitor output and the TUI](#observability-monitor-output-and-the-tui)
- [Build and Run](#build-and-run)
  - [Requirements](#requirements)
  - [Build from source](#build-from-source)
  - [Run in the foreground](#run-in-the-foreground)
  - [Run with the TUI](#run-with-the-tui)
  - [Monitor a running instance](#monitor-a-running-instance)
  - [Selected command-line options](#selected-command-line-options)
- [Installation and Removal](#installation-and-removal)
  - [Using install.sh](#using-installsh)
  - [Using uninstall.sh](#using-uninstallsh)
  - [Distro notes](#distro-notes)
- [Benchmarks](#benchmarks)
  - [What the benchmark script does](#what-the-benchmark-script-does)
  - [Reference results](#reference-results)
  - [How to read those numbers](#how-to-read-those-numbers)
- [Limitations](#limitations)
- [Contributing](#contributing)
- [License](#license)

---

## Status

- Language/runtime model: Rust 2021 userspace scheduler on top of `sched_ext`
- Core dependencies include `scx_rustland_core = 2.4.10`, `libbpf-rs = 0.26.0-beta.1`, `ratatui = 0.26`, and `crossterm = 0.27`
- CI workflows currently cover Ubuntu, Arch Linux, and CachyOS compile/test paths
- Unit tests: 40

This README describes the scheduler behavior, scripts, and workflows that come with Cognis today.

### CI coverage at a glance

| Environment | What the workflow currently checks |
|:--|:--|
| Ubuntu | `cargo fmt --all -- --check`, `cargo test --all -- --nocapture`, `cargo build --release` |
| Arch Linux | `cargo check --all` inside `archlinux/archlinux:latest` |
| CachyOS | `cargo check --all` inside `cachyos/cachyos:latest` |

Those workflows do **not** prove runtime behavior on GitHub-hosted runners, because the runners do not provide a `sched_ext` kernel.

[↑ Back to Table of Contents](#table-of-contents)

---

## Setting the Stage

### Why this project exists

The project exists because “throughput is fine” and “the machine still feels good to use” are not the same statement.

When a system is under mixed load, a scheduler has to make a constant stream of small choices: which task looks interactive, which one is really just chewing CPU, whether a short slice or a longer one is the better trade, and how much work the scheduler can do before it becomes overhead itself. `scx_cognis` is a hands-on attempt to make those choices explicit in userspace, where the policy is easier to inspect and change.

### What Cognis does today

Today, Cognis does a few concrete things:

1. It classifies each scheduling event using a deterministic heuristic.
2. It dispatches normal user tasks to the shared dispatch queue so any eligible CPU can pick them up.
3. It keeps fixed-size, preallocated state for trust scoring and burst prediction.
4. It adjusts the effective slice with a deterministic load-driven controller that runs off the hot path.
5. It exports live metrics and can render them in a terminal dashboard.

Just as important are the things it does **not** currently do. It does not claim to be a general replacement for the kernel default on every machine. It does not run unconstrained online training in the scheduler path. It does not hide that some of its policy choices are still implementation-specific and benchmark-driven.

### Where Cognis fits among sched_ext schedulers

One of the easiest ways to misunderstand a scheduler is to assume they are all trying to win the same contest.

They are not.

That is especially true in the wider `sched_ext` ecosystem. Some schedulers are primarily there to teach or to provide a clean starting point. Some are trying to be highly tunable across architectures. Some are explicitly chasing interactive performance, but do most of their work in BPF. Cognis sits in a narrower lane than that.

If you want the short answer, it is this: **Cognis is for people who want an interactive-first userspace scheduler for a desktop or workstation, and who want the scheduling policy to stay inspectable and easy to modify in Rust.**

That puts it in a different place from several well-known `sched_ext` schedulers:

| Scheduler | What it is trying to be | When Cognis may fit better |
|:--|:--|:--|
| `scx_rust_scheduler` | A basic FIFO Rust scheduler template built to help developers learn and experiment | Consider Cognis if you want a built-in desktop-oriented policy with classification, trust tracking, slice control, monitoring, and a TUI instead of a minimal starting point |
| `scx_rustland` | A readable all-Rust userspace scheduler that prioritizes interactive workloads and doubles as a template | Consider Cognis if you want a similar userspace-Rust approach with per-event labels, trust-based throttling, burst prediction, and more scheduler-specific monitoring |
| `scx_bpfland` | An interactive-first scheduler similar in spirit to Rustland, but with the scheduling policy pushed fully into BPF for production use | Consider Cognis if you prefer to keep policy development in userspace Rust rather than move most of it into BPF |
| `scx_lavd` | A latency-criticality-aware scheduler aimed at interactive workloads like gaming while still targeting high throughput and low tail latency | Consider Cognis if you want a smaller userspace policy that is easier to inspect and modify, and you are comfortable with a more experimental scheduler |
| `scx_rusty` | A flexible multi-domain hybrid scheduler that can be tuned across architectures and workloads | Consider Cognis if your main problem is desktop responsiveness under mixed background work rather than broad topology-oriented tuning |
| `scx_layered` | A highly configurable scheduler for layer-based policies, service isolation, cgroup-driven CPU allocation, and workload-specific tuning | Consider Cognis if you want one built-in desktop/workstation policy rather than a layer-configuration framework |

That leads to a practical rule of thumb.

Reach for Cognis when your workload looks like this:

- gaming while builds, VMs, or stress workloads are active
- desktop interaction under mixed CPU and I/O pressure
- browser, compositor, audio, streaming, and shell activity competing with background compute
- experimentation where you want to change policy logic in Rust without dropping down into a larger BPF-first scheduler design

Cognis is probably **not** what you are looking for when your workload looks like this:

- you want the smallest possible starter scheduler to learn `sched_ext`
- you need a more clearly production-oriented scheduler from the main `scx` tree
- you need topology-heavy tuning across LLC, NUMA, or service layers
- you want a scheduler whose primary value is broad configurability rather than a built-in desktop/workstation policy

[↑ Back to Table of Contents](#table-of-contents)

---

## How Cognis Works

### The hot path in one pass

If you want the short version, here it is.

When the kernel hands a task to Cognis, the userspace scheduler computes a small feature set, classifies the task, looks up trust state, reads the current burst prediction, computes a slice, and queues the task in a fixed-capacity per-label FIFO. When it is time to dispatch, Cognis drains those queues in priority order, gives recent latency-sensitive interactive wakeups one temporary boosted lane ahead of the normal interactive FIFO, lets the freshest high-credit wakeups cut ahead of ordinary user-task starvation handling through a bounded wake-preempt proxy, gives those same wakeups a small temporary vtime credit when handing them back to BPF so the urgency survives the shared-queue handoff, selectively accepts explicit idle-CPU placement for those wakeups when that idle CPU matches the scheduler's topology preference and stays in the task's previous LLC when that locality information is available, and only performs bounded rescue dispatches for aged compute tasks before sending work back to BPF for execution.

The implementation is intentionally biased toward bounded work:

- fixed-size queues instead of growable containers on the scheduling path
- fixed-size predictor/trust tables instead of hash maps
- periodic policy updates outside the dispatch hot path
- non-blocking TUI input handling when the dashboard is enabled

At a high level, the pipeline in `src/main.rs` is:

```text
ops.enqueue    -> feature extraction -> heuristic classification -> trust lookup -> burst predictor read
ops.dispatch   -> slice computation + per-PID renewal calibration -> bounded wake-boost lane -> wake-preempt proxy -> wakeup vtime credit -> topology-aware idle-CPU and LLC-locality check -> queue pop in priority order -> BPF dispatch
ops.select_cpu -> idle-CPU hinting in BPF, while user-task placement still goes through SHARED_DSQ
```

### Why user tasks go through SHARED_DSQ

This is one of the most important behavioral details in the design.

The scheduler sends the bulk of user work to `RL_CPU_ANY`, which means the BPF side places it in the shared dispatch queue. Kernel workers and explicit `--percpu-local` tasks are the obvious exceptions, and there are two narrower interactive exceptions inside that shared-queue model: recent latency-sensitive wakeups can be moved into a temporary boosted FIFO ahead of the normal interactive lane, and those same wakeups may use a concrete idle CPU when BPF can hand one back and it matches Cognis's topology and LLC-locality rules.

That choice is there for a reason. The comments in `src/main.rs` document the failure mode plainly: pinning userspace-managed work to a CPU-specific DSQ could interact badly with the userspace scheduler thread itself and lead to stalls serious enough to trip the `sched_ext` watchdog. Sending regular user work through the shared queue avoids that class of stall by letting any available CPU drain it. The narrower wakeup boost and placement hints for latency-sensitive interactive tasks are deliberately opportunistic instead of universal: Cognis only gives the temporary queue boost to wakeups that look like recent frame-sized sleeps rather than generic backlog, and it only takes the explicit-CPU path when the BPF selector can identify an idle target right then, when the topology model says that target is a sensible fit for the task's current performance criticality, and when taking that hint does not throw the task onto a different LLC without a good reason.

### How tasks are classified

The current classifier lives in [src/ai/classifier.rs](src/ai/classifier.rs). It is intentionally simple and deterministic.

The main labels are:

- `Interactive`
- `Compute`
- `IoWait`
- `RealTime`
- `Unknown` (reserved, but not emitted by the current heuristic)

The primary signal is `cpu_intensity`, which is the fraction of the previously assigned slice the task actually used. The current rules are:

- `weight_norm > 0.95` -> `RealTime`
- `cpu_intensity > 0.85` **and** `exec_ratio < 0.30` -> `Compute`
- `cpu_intensity < 0.10` -> `IoWait`
- everything else -> `Interactive`

That extra `exec_ratio` guard matters. The code comments call out the reason directly: a task that wakes frequently, does meaningful work, and sleeps again can use a large fraction of its slice without behaving like a classic CPU hog. Without that guard, high-value interactive work can be mislabeled as compute-heavy background work.

There is another special case before the classifier even runs: `src/main.rs` checks `comm` names to detect kernel worker threads and force them into the `RealTime` bucket. That logic exists to avoid starving kernel workers that would otherwise look deceptively compute-like.

After labeling, Cognis also applies a narrower wakeup heuristic for latency-sensitive interactive work. The idea is simple: a compositor frame, browser renderer wakeup, or similar desktop-critical burst is more valuable when it gets one temporary step ahead of older interactive backlog, carries a small temporary urgency credit into the shared dispatch queue, and, when possible, lands on an idle CPU immediately. The current heuristic keeps that bounded by combining wakeup-heavy burst behavior with a small fixed set of well-known desktop process-name prefixes, then only granting the queue boost to wakeups that return after a recent frame-sized sleep gap. Cognis now also carries a bounded per-task interactive slice credit across those short sleep/wake cycles, calibrates a small per-PID renewal bias from recent slice pressure, and lets the freshest high-credit wakeups jump ahead of ordinary user-task starvation handling through a bounded wake-preempt proxy. Explicit idle-CPU placement is filtered through a lightweight topology model so hybrid systems can prefer the more suitable core type and multi-LLC systems can preserve cache locality instead of accepting any idle CPU blindly.

### How slices are adjusted

The slice story in Cognis is layered.

First, the deterministic slice controller in [src/ai/policy.rs](src/ai/policy.rs) computes an auto base from current load using a targeted latency model:

```text
TARGETED_LATENCY_NS / max(tasks_per_cpu, 1)
```

In the current implementation, `tasks_per_cpu` is fed from the combined pressure that Cognis can see at that moment: currently running tasks plus the queued BPF-to-userspace backlog and any userspace tasks still pending dispatch. That means the base slice can tighten before all of that pressure has already turned into actively running work.

  The result is normally clamped between 250 µs and 8 ms and applied directly. That is intentionally much tighter than a throughput-first desktop policy because Cognis is explicitly trying to stay inside a 120 Hz-style interaction budget under load. If the user passes `--slice-us N`, that value acts as a ceiling, not as a promise that every task will receive exactly `N` microseconds.

  Note: the deterministic auto base is the canonical source for most policy decisions, but the autopilot can safely request a temporary relaxation of the lower clamp under telemetry-guided conditions. When the adaptive `min` is lowered below the normal 250 µs auto floor, the slice controller will consult the raw (unclamped) computed base so effective slices can go below 250 µs for controlled experiments; such changes are still governed by the autopilot's overhead guard, smoothing, step cap, cooldown, and rollback checks. See the [Autopilot: always-on conservative autoslicing](#autopilot-always-on-conservative-autoslicing) section for details.

Then Cognis applies policy and label-specific adjustments:

- `Interactive` -> 1.0x
- `IoWait` -> 0.9x
- `Compute` -> 0.5x
- `RealTime` -> 0.75x
- `Unknown` -> 0.75x

There is one more interactive-specific guardrail on top of that. If a task keeps waking, burns most of the slice it was given, and then sleeps again, Cognis treats that as a latency-critical burst pattern and gives it a modest extra slice bump instead of forcing it down into the smallest possible desktop slice. It also renews a bounded per-task slice credit for that task and maintains a bounded per-PID renewal bias, so repeated frame-sized wakeups can preserve a little budget and urgency across bursts instead of paying the full cost of a global slice reset every time. When those wakeups are also fresh enough and carry enough recent urgency, Cognis lets them cut ahead of ordinary user-task starvation handling through the wake-preempt proxy. That is aimed squarely at render-thread and compositor-style behavior.

Burst prediction can still reduce the final slice further when the scheduler already has evidence that the next burst is likely to be short.

There is also a separate deadline-side guardrail now: non-compute tasks do not carry an `exec_runtime` penalty past roughly one 120 Hz frame budget. That keeps a browser, renderer, or compositor from missing a wakeup and then spending the next 100 ms paying for it.

The slice controller is deliberately direct. It re-runs the load formula on a short periodic tick and updates the global base slice immediately from the current running-plus-queued pressure signal. The point is not to be clever. The point is to make slice changes predictable enough that desktop-critical wakeups are not waiting on a slower control loop to notice that the machine just got busy.

### Autopilot: always-on conservative autoslicing

Cognis now includes a conservative, always-on autopilot proposer that adjusts the adaptive `min`/`max` slice caps at runtime. The autopilot runs entirely in userspace and is deliberately conservative by default: it is enabled by default and applies only bounded, rate-limited changes so a single noisy probe cannot destabilise latency.

Key safety features the autopilot implements:

- Overhead guard: the autopilot measures the scheduler's own per-cycle overhead (median / p50) and never proposes a `min` below 4× that median. This prevents short-slice modes from being dominated by scheduler overhead.
- Smoothing: targets are smoothed using an EMA to prevent step noise from driving rapid changes.
- Step cap: each update is limited to a small fractional change (default ~10%).
- Cooldown: writes are rate-limited (default 5 s between applied changes) so changes have time to settle and be validated against tail latency.
- Rollback on regression: the autopilot tracks a p99 threshold and will revert to the last-good caps if tail latency worsens beyond the stored threshold after a change.

Defaults and bounds used by the current conservative proposer:

- absolute hard floor: 10 µs
- absolute hard ceiling: 50 ms
- smoothing alpha: 0.25
- cooldown: 5 s
- step cap: 10%

The proposer takes its safety cues from the scheduling pipeline percentiles (p50/p95/p99) that Cognis now records and exports. See [Observability: monitor output and the TUI](#observability-monitor-output-and-the-tui) for how to read those values. The autopilot is intended as a safe, conservative assist — it is not doing aggressive exploration, and it will not push Caps beyond the hard bounds above.


### Trust tracking and burst prediction

Two other pieces round out the current policy.

The first is the burst predictor in [src/ai/burst_predictor.rs](src/ai/burst_predictor.rs). It is a compact Elman RNN with:

- 4 hidden units
- 3 inputs: burst normalization, `exec_ratio`, and `cpu_intensity`
- fixed compile-time weights plus a bounded per-PID residual correction term
- per-PID state stored in a fixed-size table of 4096 slots

The second is the trust table in [src/ai/trust.rs](src/ai/trust.rs). It tracks a trust score in `[-1.0, 1.0]`, quarantines tasks below the current threshold of `-0.35`, and can flag repeated bad actors for the TUI's trust watchlist. Neutral tasks are not pre-penalized: only negative trust pushes slices down, while neutral and positively scored tasks keep the full slice budget they would otherwise receive.

Both pieces are designed around fixed-size storage and bounded lookup/update cost. That theme shows up all over the project: if the scheduler wants to help during load, it cannot casually allocate its way into becoming extra load.

### Observability: monitor output and the TUI

Cognis exposes metrics through `scx_stats` and formats them in [src/stats.rs](src/stats.rs).


The line format starts like this:

```text
[cognis vx.y.z] elapsed: 1y2m3d 12h:30m:14s | tldr: ... | r:... | q:... | pf:... | d→u:... k:... c:... b:... f:... ewma:... kb:... sched:p50/p95/p99 | cong:... | 🧠 Interactive:... Compute:... IOwait:... RT:... Unknown:... | quarantine:... flagged:... | slice(base/assigned):.../...µs
```

The `tldr` message is not free-form prose; it comes from a fixed set of status messages selected from current metrics such as page faults, failures, congestion, load, and label mix. The exported slice pair now distinguishes the current global load-driven base slice from a recent EMA of the final per-task assigned slices after label, trust, burst, and interactive-renewal adjustments.

Example `--monitor` output
```text
[cognis v1.5.1] elapsed: 1y2m3d 12h:30m:14s | tldr: Balancing work steadily — nothing to worry about.       | r:  1/16  q:1  /0   | pf:0    | d→u:0      k:2002 c:0    b:0    f:0    ewma:0      kb:0    sched:3/12/30 | cong:0    |
             🧠 Interactive:0    Compute:0    IOwait:0    RT:0    Unknown:0    | quarantine:2009 flagged:0 | slice(base/assigned):6000/12128µs
```

#### BPF PoC: lightweight in-kernel counters and boost

As an incremental experiment, Cognis now ships a small BPF-side proof-of-concept (PoC) that moves tiny, deterministic pieces of telemetry and a global "kernel boost" multiplier into the BPF program to reduce userspace roundtrips for a very targeted set of observations.
- `kernel_ewma` — a bounded per-PID LRU map maintained in BPF that stores a fixed-point (Q16.16) EWMA of recent `exec_runtime` samples. It is cheap, bounded, and intended only as a hot-path hinting signal that userspace can read for parity checks.
- `kernel_boost` — a single-entry ARRAY map (plain `u64`, nanoseconds) that userspace can update to apply a vtime credit to kthreads at dispatch time. The BPF dispatch path subtracts the credit from a kthread's `dsq_vtime` before inserting it into the per-CPU DSQ, giving it higher priority. The userspace API exposes a helper `set_kernel_boost(credit_ns)` that writes the value into BPF. Default is `0` (no boost).
- New monitor counters: `nr_bpf_ewma_updates` and `nr_kernel_boosts` appear in `--monitor` output so you can observe PoC activity in real time (they show EWMA updates and times the boost path was exercised).

Why a PoC and not a full port? The PoC follows the hybrid principle: keep rich, adaptive, or floating-point-heavy logic (the RNN burst predictor and the `TrustTable`) in Rust, and only port small deterministic numeric helpers and counters to BPF using fixed-point math. This preserves safety and makes it much easier to validate parity between BPF and userspace before moving anything larger.

The PoC is intentionally conservative and labelled experimental. It is safe to leave enabled in production as a diagnostic, but if you tune `kernel_boost` be mindful that it applies globally. See [Limitations](#limitations) for details and guidance.

If you prefer a visual view, the TUI in [src/tui/dashboard.rs](src/tui/dashboard.rs) currently renders:

1. a header
2. an overview panel
3. a task-classification panel
4. a slice-control panel
5. an inference-latency chart
6. a trust watchlist

The TUI exits on `q` or `Esc`.

The header now includes scheduling pipeline percentiles (`sched:` p50/p95/p99 in µs) so you can observe median and tail scheduler overhead directly from the dashboard.
The Slice Control panel also shows the Autopilot adaptive caps in real time as `Autopilot: <min>µs min/<max>µs max`, so you can watch what the proposer is requesting while observing assigned slices and inference latency.

<p align="center">
	<img src="https://github.com/galpt/scx_cognis/blob/main/img/cognis-ratatui.png" alt="scx_cognis TUI" style="max-width:100%;height:auto;" />
	<br/>
  <em>The current ratatui dashboard shipped with Cognis.</em>
</p>

[↑ Back to Table of Contents](#table-of-contents)

---

## Build and Run

### Requirements

To run the scheduler itself, you need:

- Linux with `sched_ext` support enabled
- in practice, Cognis currently targets kernels with `sched_ext` support at `>= 6.12`
- a toolchain capable of building the Rust and BPF pieces

For a source build, the included scripts and workflows assume packages in the `clang`/`llvm`, `libbpf`, `libelf`, `zlib`, `libseccomp`, and `pkg-config` family.

You can check whether your kernel exposes `sched_ext` like this:

```bash
grep CONFIG_SCHED_CLASS_EXT /boot/config-$(uname -r)
```

### Build from source

```bash
git clone https://github.com/galpt/scx_cognis
cd scx_cognis
cargo build --release
```

The release binary will be at `target/release/scx_cognis`.

### Run in the foreground

```bash
sudo ./target/release/scx_cognis
```

The scheduler requires elevated privileges to become the active `sched_ext` scheduler.

### Run with the TUI

```bash
sudo ./target/release/scx_cognis --tui
```

If you are also using the provided `scx.service`, stop that service first. The installer itself prints this guidance because only one `sched_ext` scheduler instance can be active at a time:

```bash
sudo systemctl stop scx && sudo scx_cognis --tui
```

When you exit the TUI with `q` or `Esc`, that TUI-launched scheduler instance exits too.

### Monitor a running instance

If the scheduler is already running and exporting stats, you can watch it with:

```bash
scx_cognis --monitor 1.0
```

The installer and service configuration are set up so that, when installed through the provided service flow, the stats socket at `/run/scx/root/stats` is intended to be reachable by non-root users.

### Selected command-line options

The current CLI in `src/main.rs` includes these user-facing options:

| Option | What it does now |
|:--|:--|
| `-s, --slice-us <N>` | Sets the slice ceiling in microseconds; `0` keeps auto mode enabled |
| `-S, --slice-us-min <N>` | Sets the minimum slice duration in microseconds |
| `-l, --percpu-local` | Dispatches per-CPU tasks directly to their only eligible CPU |
| `-p, --partial` | Only manages tasks already using `SCHED_EXT` |
| `-v, --verbose` | Enables verbose output |
| `-t, --tui` | Launches the TUI dashboard |
| `--stats <secs>` | Starts stats monitoring while also running the scheduler |
| `--monitor <secs>` | Runs monitor mode only; does not launch the scheduler |
| `--help-stats` | Prints descriptions for exported statistics |
| `-V, --version` | Prints the Cognis version and `scx_rustland_core` version |

[↑ Back to Table of Contents](#table-of-contents)

---

## Installation and Removal

### Using install.sh

Cognis includes a root-level installer script in [install.sh](install.sh).

In its current form, that script can:

- download an `x86_64` release tarball from GitHub Releases, or build locally with `--build-from-source`
- detect CachyOS, Arch, Ubuntu, Debian, or fall back to a generic path
- check for `sched_ext` support and warn if it cannot confirm it
- write or reuse `scx.service`
- manage `/etc/default/scx`
- install a systemd drop-in so monitor mode works against the stats socket more conveniently
- enable and restart the `scx` service

Typical usage:

```bash
sudo sh install.sh
```

Useful current flags:

```bash
sudo sh install.sh --dry-run
sudo sh install.sh --build-from-source
sudo sh install.sh --version vX.Y.Z
sudo sh install.sh --flags "--verbose"
```

### Using uninstall.sh

Cognis also includes [uninstall.sh](uninstall.sh).

That script currently:

1. stops and disables `scx.service` if it exists
2. restores or cleans up `/etc/default/scx`
3. removes `/usr/bin/scx_cognis`
4. optionally purges the service file if it looks like the installer created it
5. reloads systemd

Typical usage:

```bash
sudo sh uninstall.sh
```

And for preview or cleanup variations:

```bash
sudo sh uninstall.sh --dry-run
sudo sh uninstall.sh --purge
sudo sh uninstall.sh --force
```

### Distro notes

- **CachyOS / Arch**: the installer will try to use `scx-manager` when appropriate, and otherwise falls back to writing the needed service file itself.
- **Ubuntu / Debian**: the installer writes a service file if one is not already present.
- **Other systemd-based distributions**: the installer may still work, but the generic path is less opinionated and less tested than the distro-specific paths above.

[↑ Back to Table of Contents](#table-of-contents)

---

## Benchmarks

### What the benchmark script does

The benchmark helper lives at [cognis_benchmark.sh](cognis_benchmark.sh).

It is intentionally simple. The script asks you to run one of two modes:

- baseline, without Cognis
- with Cognis already active

Then it opens the WebGL Aquarium and runs three 60-second `stress-ng` phases:

1. CPU
2. I/O (`iomix`)
3. mixed CPU + VM pressure

The script is focused on what to watch: not just `bogo ops/s`, but also frame pacing, jank, and whether desktop interaction still feels immediate during load.

### Reference results

The table below comes from one recorded comparison on a Lenovo IdeaPad Gaming 3 15ARH7 running CachyOS with a `6.19.5-3-cachyos` kernel, KDE Plasma on Wayland, and the panel's default 500-fish Aquarium workload. During both runs, `stress-ng` reported that the CPU scaling governors were still set to `powersave`, so treat this as a concrete reference point for this machine and setup rather than a promise that every later release or every other system will reproduce the same numbers.

| Phase | Metric | Baseline | Cognis | Delta |
|:--|:--|--:|--:|--:|
| CPU | bogo ops/s (real) | 20,588.69 | 21,497.25 | +4.4% |
| CPU | bogo ops/s (usr) | 1,373.80 | 1,399.02 | +1.8% |
| I/O | bogo ops/s (real) | 183,211.76 | 180,076.39 | -1.7% |
| I/O | bogo ops/s (usr) | 43,187.95 | 42,844.41 | -0.8% |
| Mixed CPU | bogo ops/s (real) | 18,402.63 | 18,748.51 | +1.9% |
| Mixed CPU | bogo ops/s (usr) | 1,394.70 | 1,401.14 | +0.5% |
| Mixed VM | bogo ops/s (real) | 24,523.72 | 24,540.00 | +0.1% |
| Mixed VM | bogo ops/s (usr) | 14,936.02 | 14,387.09 | -3.7% |

The two screenshots below show the comparison used in this benchmark section:

<p align="center">
	<img src="https://github.com/galpt/scx_cognis/blob/main/img/baseline-cpu-usage.png" alt="Benchmark baseline CPU usage" style="max-width:100%;height:auto;" />
	<br/>
	<em>Benchmark capture without Cognis active.</em>
</p>

<p align="center">
	<img src="https://github.com/galpt/scx_cognis/blob/main/img/cognis-cpu-usage.png" alt="Benchmark CPU usage with Cognis" style="max-width:100%;height:auto;" />
	<br/>
	<em>Benchmark capture with Cognis active.</em>
</p>

### How to read those numbers

The useful point of the benchmark is not “Cognis always produces a larger benchmark number.” The more honest reading is narrower than that.

On the recorded reference machine, this run improved CPU-heavy throughput modestly, stayed nearly even in the mixed phase, and gave back a little throughput in the I/O-heavy and VM-user-time numbers. That is useful context, but it is still only one machine, one governor configuration, and one set of runs.

If you want numbers you can trust on your own hardware, keep the governor fixed, run both modes multiple times, and compare medians. Then watch the Aquarium while you do it. The whole project makes a lot more sense when you look at throughput and interaction quality at the same time.

[↑ Back to Table of Contents](#table-of-contents)

---

## Limitations

The scheduler has clear limits.

- The burst predictor now includes a small online residual correction per PID, but it is still not a fully trained online model.
- The slice controller now combines a global load-driven base with bounded per-task interactive slice renewal, per-PID calibration, and a wake-preempt proxy, but it is still not the same thing as LF-BMQ's full per-task slice expiry and immediate wake-preempt model.
- Runtime behavior still depends heavily on kernel version, workload shape, CPU topology, and how `sched_ext` behaves on the target machine.
- CI can prove build/test health, but not real `sched_ext` runtime behavior.
- Some benchmark and policy conclusions in this README are still best read as evidence about the current implementation, not as universal scheduler laws.
- Experimental BPF PoC: the repository now contains a small BPF-side proof-of-concept that keeps a bounded per-PID EWMA (Q16.16 fixed-point, updated in `rustland_stopping()`) and a single global `kernel_boost` vtime credit (plain `u64` nanoseconds, applied in the kthread dispatch path). This is intentionally tiny — the design goal was to avoid floating-point or large verifier-unfriendly constructs in BPF. Treat these features as diagnostics: they are useful for parity checks and low-latency hints, but the canonical implementations of burst prediction and trust remain in userspace Rust.
- Kernel boost note: `kernel_boost` stores a vtime credit in nanoseconds (not a multiplier). To give kthreads roughly one 120 Hz frame budget of priority, set the credit to `8_333_333` (≈ 8.3 ms). The default is `0` (no boost). The EWMA alpha is still Q16.16 internally but is not exposed as a tunable.

[↑ Back to Table of Contents](#table-of-contents)

---

## Contributing

If you want to work on Cognis, start by treating the scheduler path with suspicion and respect in equal measure.

That means:

1. keep hot-path work bounded
2. avoid allocations in per-event logic
3. verify claims against the code, not against an earlier README paragraph
4. run the standard checks before sending a change

Current local commands worth running:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

If you add new exported stats, TUI state, or CLI behavior, update this README in the same change. The safest README is the one that has to survive contact with the code review for the feature it describes.

[↑ Back to Table of Contents](#table-of-contents)

---

## License

This project is licensed under `GPL-2.0-only`. See [LICENSE](LICENSE).

[↑ Back to Table of Contents](#table-of-contents)
