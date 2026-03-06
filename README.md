# scx_cognis

Sometimes a Linux desktop feels fine right up until it doesn't. A browser starts rendering something heavy, a build kicks off in the background, a VM wakes up, and suddenly the part you actually care about, the window you are touching right now, stops feeling immediate.

`scx_cognis` is an experiment in pushing back on that failure mode.

It is a Rust userspace scheduler built on top of `sched_ext` and `scx_rustland_core`. The current code combines a deterministic task classifier, a fixed-size trust table, a compact burst predictor, and a tabular Q-learning slice controller. The goal is not to be magical. The goal is simpler: keep interactive work responsive under pressure without turning the scheduler itself into the problem.

That framing matters, because this repository is still exactly what its code says it is: **an attempt at an intelligent CPU scheduler**. It is stable enough to build, test, benchmark, and run on `sched_ext`-enabled systems, but it is still an implementation-driven project rather than a broadly validated production scheduler.

---

## Table of Contents

- [Status](#status)
- [Setting the Stage](#setting-the-stage)
  - [Why this project exists](#why-this-project-exists)
  - [What the repository contains today](#what-the-repository-contains-today)
- [How Cognis Works](#how-cognis-works)
  - [The hot path in one pass](#the-hot-path-in-one-pass)
  - [Why user tasks go through SHARED_DSQ](#why-user-tasks-go-through-shared_dsq)
  - [How tasks are classified](#how-tasks-are-classified)
  - [How slices are adjusted](#how-slices-are-adjusted)
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
- CI workflows in this repository currently cover Ubuntu, Arch Linux, and CachyOS compile/test paths
- Unit tests in the current tree: 29

At the time of this rewrite, the README has been aligned to the code in `src/`, the current scripts in the repository root, and the current workflow files under `.github/workflows/`.

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

### What the repository contains today

Today, the scheduler in this repository does a few concrete things:

1. It classifies each scheduling event using a deterministic heuristic.
2. It dispatches normal user tasks to the shared dispatch queue so any eligible CPU can pick them up.
3. It keeps fixed-size, preallocated state for trust scoring and burst prediction.
4. It adjusts the effective slice with a small tabular Q-learning controller that runs off the hot path.
5. It exports live metrics and can render them in a terminal dashboard.

Just as important are the things it does **not** currently do. It does not claim to be a general replacement for the kernel default on every machine. It does not train models online. It does not hide that some of its policy choices are still implementation-specific and benchmark-driven.

[↑ Back to Table of Contents](#table-of-contents)

---

## How Cognis Works

### The hot path in one pass

If you want the short version, here it is.

When the kernel hands a task to Cognis, the userspace scheduler computes a small feature set, classifies the task, looks up trust state, reads the current burst prediction, computes a slice, and queues the task in a fixed-capacity per-label FIFO. When it is time to dispatch, Cognis drains those queues in priority order and sends tasks back to BPF for execution.

The implementation is intentionally biased toward bounded work:

- fixed-size queues instead of growable containers on the scheduling path
- fixed-size predictor/trust tables instead of hash maps
- periodic policy updates outside the dispatch hot path
- non-blocking TUI input handling when the dashboard is enabled

At a high level, the pipeline in `src/main.rs` is:

```text
ops.enqueue    -> feature extraction -> heuristic classification -> trust lookup -> burst predictor read
ops.dispatch   -> slice computation -> queue pop in priority order -> BPF dispatch
ops.select_cpu -> idle-CPU hinting in BPF, while user-task placement still goes through SHARED_DSQ
```

### Why user tasks go through SHARED_DSQ

This is one of the most important behavioral details in the current design.

Older Cognis iterations tried to dispatch user tasks toward per-CPU DSQs. The current code does not. In `dispatch_task()`, user tasks are sent to `RL_CPU_ANY`, which means the BPF side places them in the shared dispatch queue. Kernel workers and explicit `--percpu-local` tasks are the main exceptions.

That choice is there for a reason. The comments in `src/main.rs` document the failure mode plainly: pinning userspace-managed work to a CPU-specific DSQ could interact badly with the userspace scheduler thread itself and lead to stalls serious enough to trip the `sched_ext` watchdog. Sending regular user work through the shared queue avoids that class of stall by letting any available CPU drain it.

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

### How slices are adjusted

The slice story in Cognis is layered.

First, the policy controller in [src/ai/policy.rs](src/ai/policy.rs) computes an auto base from current load using a targeted latency model:

```text
TARGETED_LATENCY_NS / max(tasks_per_cpu, 1)
```

The result is clamped between 500 µs and 20 ms, then smoothed. If the user passes `--slice-us N`, that value acts as a ceiling, not as a promise that every task will receive exactly `N` microseconds.

Then Cognis applies policy and label-specific adjustments:

- `Interactive` -> 0.5x
- `IoWait` -> 0.75x
- `Compute` -> 1.0x
- `RealTime` -> 0.25x
- `Unknown` -> 1.0x

Finally, trust state and burst prediction can reduce the final slice further.

The Q-learning controller itself is deliberately modest. It is a bounded tabular controller with 625 discrete states and three actions: shrink, keep, or grow. It runs periodically, not in the inner dispatch loop.

### Trust tracking and burst prediction

Two other pieces round out the current policy.

The first is the burst predictor in [src/ai/burst_predictor.rs](src/ai/burst_predictor.rs). It is a compact Elman RNN with:

- 4 hidden units
- 3 inputs: burst normalization, `exec_ratio`, and `cpu_intensity`
- fixed compile-time weights
- per-PID state stored in a fixed-size table of 4096 slots

The second is the trust table in [src/ai/trust.rs](src/ai/trust.rs). It tracks a trust score in `[-1.0, 1.0]`, quarantines tasks below the current threshold of `-0.35`, and can flag repeated bad actors for the TUI's wall-of-shame display.

Both pieces are designed around fixed-size storage and bounded lookup/update cost. That theme shows up all over the project: if the scheduler wants to help during load, it cannot casually allocate its way into becoming extra load.

### Observability: monitor output and the TUI

The repository currently exposes metrics through `scx_stats` and formats them in [src/stats.rs](src/stats.rs).

The line format starts like this:

```text
[cognis vx.y.z] tldr: ... | r:... | q:... | pf:... | d→u:... k:... c:... b:... f:... | cong:... | 🧠 Interactive:... Compute:... IOwait:... RT:... Unknown:... | quarantine:... flagged:... | slice:...µs reward:...
```

The `tldr` message is not free-form prose; it comes from a fixed set of status messages selected from current metrics such as page faults, failures, congestion, load, label mix, and reward EMA.

If you prefer a visual view, the TUI in [src/tui/dashboard.rs](src/tui/dashboard.rs) currently renders:

1. a header
2. an overview panel
3. a task-classification panel
4. a Q-learning policy panel
5. an inference-latency chart
6. a trust “wall of shame”

The TUI exits on `q` or `Esc`.

<p align="center">
	<img src="https://github.com/galpt/scx_cognis/blob/main/img/cognis-ratatui.png" alt="scx_cognis TUI" style="max-width:100%;height:auto;" />
	<br/>
	<em>The current ratatui dashboard shipped by this repository.</em>
</p>

[↑ Back to Table of Contents](#table-of-contents)

---

## Build and Run

### Requirements

To run the scheduler itself, you need:

- Linux with `sched_ext` support enabled
- in practice, this repository currently documents kernel support as `>= 6.12`
- a toolchain capable of building the Rust and BPF pieces

For a source build, the repository scripts and workflows currently assume packages in the `clang`/`llvm`, `libbpf`, `libelf`, `zlib`, `libseccomp`, and `pkg-config` family.

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

The installer and service configuration in this repository are set up so that, when installed through the provided service flow, the stats socket at `/run/scx/root/stats` is intended to be reachable by non-root users.

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

The repository ships a root-level installer script in [install.sh](install.sh).

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
sudo sh install.sh --version v0.1.5
sudo sh install.sh --flags "--verbose"
```

### Using uninstall.sh

The repository also ships [uninstall.sh](uninstall.sh).

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
- **Other systemd-based distributions**: the installer may still work, but the generic path is less opinionated and less tested by the scripts in this repository.

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

The script is opinionated about what to watch: not just `bogo ops/s`, but also frame pacing, jank, and whether desktop interaction still feels immediate during load.

### Reference results

The table below comes from one recorded comparison on a Lenovo IdeaPad Gaming 3 15ARH7 running CachyOS with a `6.19.5-3-cachyos` kernel. It reflects a `v1.3.6` benchmark run, so treat it as a concrete reference point for this machine and workload rather than a promise that every later release or every other system will reproduce the same numbers.

| Phase | Metric | Baseline | Cognis | Delta |
|:--|:--|--:|--:|--:|
| CPU | bogo ops/s (real) | 22,055.82 | 22,210.24 | +0.7% |
| CPU | bogo ops/s (usr) | 1,411.10 | 1,411.04 | 0.0% |
| I/O | bogo ops/s (real) | 178,202.08 | 180,383.24 | +1.2% |
| I/O | bogo ops/s (usr) | 40,448.66 | 40,180.41 | -0.7% |
| Mixed CPU | bogo ops/s (real) | 19,760.80 | 19,781.18 | +0.1% |
| Mixed CPU | bogo ops/s (usr) | 1,417.85 | 1,419.61 | +0.1% |
| Mixed VM | bogo ops/s (real) | 24,555.34 | 24,566.91 | +0.0% |
| Mixed VM | bogo ops/s (usr) | 14,789.09 | 14,021.45 | -5.2% |

The repository also includes the two comparison screenshots used in the benchmark section:

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

On the recorded reference machine, the benchmark script's latest checked-in numbers stayed close to baseline while Cognis was tuned for responsiveness-first behavior. That is encouraging, but it is still only one machine, one governor configuration, and one set of runs.

If you want numbers you can trust on your own hardware, keep the governor fixed, run both modes multiple times, and compare medians. Then watch the Aquarium while you do it. The whole project makes a lot more sense when you look at throughput and interaction quality at the same time.

[↑ Back to Table of Contents](#table-of-contents)

---

## Limitations

The current code is capable, but it is not shy about its limits.

- The burst predictor uses fixed compile-time weights rather than online learning.
- The Q-learning controller is deliberately small and bounded; it is a practical policy knob, not a grand adaptive intelligence layer.
- Runtime behavior still depends heavily on kernel version, workload shape, CPU topology, and how `sched_ext` behaves on the target machine.
- CI can prove build/test health, but not real `sched_ext` runtime behavior.
- Some benchmark and policy conclusions in this repository are still best read as evidence about the current implementation, not as universal scheduler laws.

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
