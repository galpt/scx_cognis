# scx_cognis

`scx_cognis` is a BPF-first `sched_ext` scheduler for Linux desktops and workstations.

The current design is intentionally simpler than earlier Cognis revisions: keep the common case in BPF, preserve locality below saturation, and use a small Rust fallback only when backlog builds up enough to justify extra policy work.

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

- Runtime model: BPF-first scheduler built on `scx_rustland_core`
- Current policy shape: local BPF fast path below saturation, weighted virtual-deadline fallback in Rust under backlog
- Behavioral labels remain for stats and TUI, but they are no longer the main scheduling hierarchy
- Local verification on this branch: `cargo fmt --check`, `cargo check`, `cargo test`, `sh -n install.sh`, and `sh -n uninstall.sh` pass
- Unit tests: `31`
- CI covers Ubuntu lint/test/build, Arch compile-check, CachyOS compile-check, and a locked release workflow

This repository is still best treated as experimental scheduler work under active benchmarking, not a proven drop-in replacement for mature upstream schedulers on every machine.

## What Cognis Does

Cognis currently does the following:

- Direct-dispatches kernel workers and lightly loaded wakeups in BPF.
- Prefers an idle CPU or the task's previous CPU while the machine still has spare capacity.
- Uses a load-driven slice controller instead of one fixed desktop slice.
- Applies a bounded wakeup credit to recent short sleepers in the Rust fallback path.
- Hands backlog-heavy user-space-managed work back to BPF through virtual deadlines rather than a deep class tree.
- Keeps scheduler-side queues and per-PID lifetime state fixed-capacity and allocation-free after init.
- Exports live stats and can render a ratatui dashboard.

## Current Design

The main scheduler logic lives in [src/main.rs](src/main.rs), with the kernel-facing backend in [main.bpf.c](main.bpf.c).

At a high level, the current design is:

1. `ops.enqueue` tries to keep the common case in BPF. When the system is lightly loaded and there is no meaningful userspace backlog, Cognis keeps work on an idle or previous CPU directly in BPF.
2. When runnable pressure or backlog increases, work is queued to the Rust side.
3. The Rust fallback computes a compact behavioral snapshot, assigns a load-driven slice, and derives a bounded wakeup credit for recent sleepers.
4. Userspace uses only three active lanes: RT, wake-boosted, and a general fair lane.
5. Fine-grained non-RT ordering is delegated back to BPF through virtual deadlines on the shared DSQ.
6. Background housekeeping refreshes the slice controller and cleans up per-PID observability state outside the hot path.

Important implementation details:

- The fast path is locality-first. The goal is to avoid a userspace round-trip for the desktop/common case whenever the system is not saturated.
- The Rust fallback is intentionally small. It does not try to run a large multi-stage policy stack.
- Behavioral labels are mostly observational now. They feed stats/TUI and a small amount of wake-sensitivity logic, but they are not the core scheduling hierarchy.
- The trust table is retained for observability and cleanup, not as the main basis for slice or placement decisions.
- `--percpu-local` only affects userspace-managed tasks. Kernel workers already stay on their explicit or previous CPU.

Observability surfaces:

- `--monitor <secs>` prints scheduler stats without launching a new scheduler instance.
- `--stats <secs>` runs the scheduler and periodic stats output together.
- `--tui` launches the ratatui dashboard.
- `--help-stats` prints descriptions for exported metrics.

## Build and Run

### Requirements

To run Cognis you need:

- Linux with `sched_ext` support
- a Rust toolchain
- common BPF build dependencies such as `clang`, `llvm`, `libbpf`, `libelf`, `zlib`, `libseccomp`, and `pkg-config`

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

If you want numbers you can trust on your hardware, keep the environment fixed, run both modes multiple times, and compare repeated local runs rather than relying on one-off impressions.

## Limitations

- Cognis is still built on `scx_rustland_core`, so it is not yet a fully standalone BPF scheduler.
- The current redesign intentionally favors simplicity over feature count. Some older Cognis subsystems are no longer central to scheduling decisions.
- CI can prove build and test health, but it cannot prove compositor stability, gaming smoothness, or watchdog safety on GitHub-hosted runners.
- Runtime behavior still depends heavily on kernel version, topology, firmware, browser workload, GPU/compositor stack, and desktop load mix.
- Any real claim about "better" behavior should come from repeated testing on the target machine.

## Contributing

Changes are most useful when they keep the scheduler path understandable, bounded, and benchmarkable.

Before sending a change, it is a good idea to run:

```bash
cargo fmt --all -- --check
cargo test
sh -n install.sh
sh -n uninstall.sh
```

If a change alters CLI behavior, exported stats, install scripts, workflows, or scheduler behavior documented here, update this README in the same patch.

## License

This project is licensed under `GPL-2.0-only`. See [LICENSE](LICENSE).
