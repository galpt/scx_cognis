# scx_cognis

`scx_cognis` is a Rust userspace `sched_ext` scheduler aimed at interactive Linux desktops and workstations.

The project focuses on keeping scheduler policy inspectable and bounded: fixed-capacity queues, fixed-size per-PID state, deterministic classification, load-driven slice control, trust tracking, burst prediction, and lightweight monitoring/TUI support.

## Table of Contents

- [Status](#status)
- [What Cognis Does](#what-cognis-does)
- [Current Design](#current-design)
- [Build and Run](#build-and-run)
- [Installation and Removal](#installation-and-removal)
- [Benchmark Helper](#benchmark-helper)
- [Limitations](#limitations)
- [Contributing](#contributing)
- [License](#license)

## Status

- Runtime model: Rust 2021 userspace scheduler on top of `sched_ext`
- Core scheduler backend: `scx_rustland_core = 2.4.10`
- Local verification on this branch: `cargo fmt --check`, `cargo check`, and `cargo test` pass
- Unit tests: `44`
- CI currently covers Ubuntu, Arch Linux, and CachyOS build/test paths

This repository is still best treated as an experimental scheduler under active hardening, not a broadly validated drop-in replacement for mature upstream schedulers. Manual testing on the target machine still matters.

## What Cognis Does

Cognis currently does the following:

- Classifies work into `RealTime`, `Interactive`, `IoWait`, and `Compute` buckets with a deterministic heuristic.
- Uses fixed-capacity queues and bounded per-PID tables so the scheduler path does not grow unbounded under load.
- Adjusts slices from runnable pressure instead of using a single static desktop slice.
- Applies bounded interactive wake boosting and keeps lightly loaded wakeups on a BPF-managed idle/previous CPU path.
- Tracks per-task trust and burst history and cleans up per-PID state when tasks exit.
- Exports live stats and can render a ratatui dashboard.

The implementation is intentionally hybrid:

- BPF handles the kernel-facing `sched_ext` hooks.
- Rust owns the higher-level scheduling policy and observability.

## Current Design

The main scheduling policy lives in [src/main.rs](src/main.rs), with smaller policy modules under [src/ai](src/ai).

At a high level, the current design is:

1. `ops.enqueue` keeps a BPF-side fast path for kthreads and lightly loaded tasks, while passing backlog-heavy work into userspace.
2. Cognis computes a compact feature set, classifies the task, reads trust and burst state, and assigns a slice.
3. Tasks are queued in fixed-capacity FIFOs by label.
4. `ops.dispatch` drains those queues in priority order and hands work back to BPF through the shared fallback, unless the task is explicitly per-CPU.
5. Periodic housekeeping updates the slice controller, trust bookkeeping, stats, and TUI state outside the hot path.

Important implementation details:

- Below saturation and without a userspace backlog, BPF keeps ordinary work on an idle or previous CPU to preserve locality without a Rust round-trip.
- Under pressure, Cognis falls back to Rust classification plus shared-DSQ dispatch to preserve forward progress.
- Kernel workers always stay on their explicit/previous CPU. `--percpu-local` forces explicit per-CPU dispatch for userspace-managed tasks instead of the normal shared fallback.
- Interactive wake boosts are bounded and temporary; they are not a blanket priority override.
- Per-PID state is collision-safe inside fixed-size tables rather than silently overwriting on hash collisions.
- Task exit notifications from BPF are consumed in userspace so trust, lifetime, and burst state can be evicted promptly.

Observability surfaces:

- `--monitor <secs>` prints scheduler stats without launching a new scheduler instance.
- `--stats <secs>` runs the scheduler and monitoring output together.
- `--tui` launches the ratatui dashboard.
- `--help-stats` prints descriptions for exported metrics.

## Build and Run

### Requirements

To run Cognis you need:

- Linux with `sched_ext` support
- a Rust toolchain
- the usual BPF build dependencies such as `clang`, `llvm`, `libbpf`, `libelf`, `zlib`, `libseccomp`, and `pkg-config`

You can check whether the running kernel exposes `sched_ext` with:

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

### Run with the TUI

```bash
sudo ./target/release/scx_cognis --tui
```

Only one `sched_ext` scheduler instance should be active at a time. If you installed Cognis as a service, stop that service before launching a foreground or TUI instance.

> [!NOTE]
> The TUI is best used for short diagnostic sessions. For 24/7 use, run Cognis headless and treat `--tui` as an interactive troubleshooting tool rather than the normal production mode.

### Monitor a running instance

```bash
scx_cognis --monitor 1.0
```

### Selected command-line options

| Option | Current behavior |
|:--|:--|
| `-s, --slice-us <N>` | Sets the slice ceiling in microseconds; `0` keeps automatic sizing enabled |
| `-S, --slice-us-min <N>` | Sets the minimum slice duration in microseconds |
| `-l, --percpu-local` | Forces userspace-managed tasks to explicit per-CPU dispatch to each task's previous CPU instead of the normal shared fallback |
| `-p, --partial` | Only manages tasks already using `SCHED_EXT` |
| `-v, --verbose` | Enables verbose output |
| `-t, --tui` | Launches the TUI dashboard |
| `--stats <secs>` | Runs the scheduler and periodic stats output together |
| `--monitor <secs>` | Monitor-only mode; does not launch the scheduler |
| `--help-stats` | Prints descriptions for exported statistics |
| `-V, --version` | Prints the Cognis version and `scx_rustland_core` version |

## Installation and Removal

The repository includes helper scripts for service-based installation and cleanup.

### Install

[install.sh](install.sh) can:

- download a GitHub release for `x86_64`, or build locally with `--build-from-source`
- detect CachyOS, Arch, Ubuntu, Debian, and fall back to a generic systemd path
- check for `sched_ext` support and warn when it cannot confirm it
- write or reuse `scx.service`
- manage `/etc/default/scx`
- enable and restart the scheduler service

Common examples:

```bash
sudo sh install.sh
sudo sh install.sh --dry-run
sudo sh install.sh --build-from-source
sudo sh install.sh --version vX.Y.Z
sudo sh install.sh --flags "--verbose"
```

On CachyOS and Arch, the installer will use `scx-manager` when available and fall back to its own service setup when needed.

### Uninstall

[uninstall.sh](uninstall.sh) can:

- stop and disable `scx.service`
- restore or clean up `/etc/default/scx`
- remove `/usr/bin/scx_cognis`
- optionally purge the service file when it looks installer-owned

Common examples:

```bash
sudo sh uninstall.sh
sudo sh uninstall.sh --dry-run
sudo sh uninstall.sh --purge
sudo sh uninstall.sh --force
```

## Benchmark Helper

The repository also includes [cognis_benchmark.sh](cognis_benchmark.sh).

That script is a local comparison helper, not a source of authoritative benchmark claims. It:

- opens the WebGL Aquarium benchmark
- runs three `stress-ng` phases
- asks you to compare throughput, frame pacing, and visible jank between baseline and Cognis

Current phase layout:

1. CPU stress
2. I/O stress
3. Mixed CPU + VM pressure

If you want numbers you can trust on your hardware, keep the environment fixed, run both modes multiple times, and compare repeated local runs rather than relying on old README tables.

## Limitations

- Cognis is still not as battle-tested as mature schedulers from the main `sched_ext` ecosystem.
- Runtime behavior depends heavily on kernel version, topology, firmware, GPU/compositor stack, browser workload, and the exact desktop load mix.
- CI can prove build and test health, but it cannot prove compositor stability, gaming smoothness, or watchdog safety on GitHub-hosted runners.
- The scheduler policy is intentionally bounded and simple in some places; that makes it easier to inspect, but it also means there is still room for policy improvement.
- Any real claim about "better" behavior should come from repeated testing on the target machine.

## Contributing

Changes are most useful when they keep the scheduler path understandable and bounded.

Before sending a change, it is a good idea to run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

If a change alters CLI behavior, exported stats, install scripts, or scheduler behavior that is documented here, update this README in the same patch.

## License

This project is licensed under `GPL-2.0-only`. See [LICENSE](LICENSE).
