# scx_cognis

`scx_cognis` is a BPF-first `sched_ext` scheduler for Linux desktops, workstations, and servers.

Cognis v2 keeps the normal scheduling path in BPF. Rust remains in the process for loading the scheduler, exporting stats, handling restart/reporting, driving the optional TUI, and servicing a narrow compatibility fallback when work intentionally crosses into userspace.

## Table of Contents

- [Status](#status)
- [Design](#design)
- [Profiles](#profiles)
- [Build and Run](#build-and-run)
- [Install and Remove](#install-and-remove)
- [Observability](#observability)
- [Benchmark Helper](#benchmark-helper)
- [Limitations](#limitations)
- [Contributing](#contributing)
- [License](#license)

## Status

- Runtime model: BPF-first `sched_ext` scheduler with a Rust control plane
- Current scaffold: still loaded through `scx_rustland_core`, but the policy shape is closer to `bpfland`/`lavd` than to an always-userspace scheduler
- Default install profile: `desktop`
- Optional profile: `server`
- Userspace fallback exists, but it is intended to be the exception rather than the normal path
- Local verification on this branch: `cargo fmt --all -- --check`, `cargo check --locked`, `cargo test --locked`, `sh -n install.sh`, and `sh -n uninstall.sh`
- CI covers Ubuntu format/test/build plus Arch and CachyOS compile checks

This repository is still experimental scheduler work. Build health and unit tests are good signals, but they are not a substitute for repeated real-machine testing on the target kernel, desktop stack, and workload mix.

## Design

The kernel-facing policy lives in [main.bpf.c](main.bpf.c). The Rust control plane lives in [src/main.rs](src/main.rs) and [src/bpf.rs](src/bpf.rs).

At a high level, Cognis v2 works like this:

1. `ops.select_cpu` and `ops.enqueue` try to keep ordinary work in BPF.
2. The BPF side uses per-CPU local DSQs plus a shared overflow DSQ.
3. Dispatch ordering is deadline-based and bounded by profile slice and wake-credit knobs.
4. Short-burst locality can stay sticky in `desktop` mode.
5. `server` mode prefers broader balancing and shared overflow sooner.
6. Rust stays available for restart control, stats, TUI, and the compatibility fallback path.

Important implementation details:

- The common case is meant to avoid a Rust round-trip.
- `nr_queued`, `nr_scheduled`, and `nr_user_dispatches` are compatibility-fallback signals. If they keep rising under a workload, that means work is escaping the intended BPF fast path.
- The Rust loop is no longer meant to spin continuously when BPF is handling the workload.
- Scheduler-owned structures are fixed-capacity and allocated once at startup.
- The TUI and monitor are observability tools, not the scheduling engine itself.

## Profiles

Cognis currently exposes two profiles:

| Profile | Default slice ceiling | Default min slice | Wake behavior | Overflow behavior |
|:--|:--|:--|:--|:--|
| `desktop` | `6000 µs` | `500 µs` | stronger wake responsiveness | keeps overflow local when possible |
| `server` | `8000 µs` | `1000 µs` | less wake-sync bias | prefers shared overflow sooner |

The active profile is selected with:

```bash
scx_cognis --mode desktop
scx_cognis --mode server
```

`install.sh` writes `--mode desktop` by default unless you override it with `--flags`.

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
sudo ./target/release/scx_cognis --mode desktop
```

### Run in `server` mode

```bash
sudo ./target/release/scx_cognis --mode server
```

### Selected command-line options

| Option | Current behavior |
|:--|:--|
| `--mode <desktop\|server>` | Selects the active BPF profile |
| `-s, --slice-us <N>` | Overrides the profile slice ceiling in microseconds |
| `-S, --slice-us-min <N>` | Overrides the profile minimum slice in microseconds |
| `-l, --percpu-local` | Forces explicit per-CPU dispatch for userspace-fallback tasks |
| `-p, --partial` | Only manages tasks already using `SCHED_EXT` |
| `-v, --verbose` | Enables verbose output |
| `-t, --tui` | Launches the TUI dashboard |
| `--stats <secs>` | Runs the scheduler and periodic stats output together |
| `--monitor <secs>` | Monitor-only mode; does not launch a scheduler |
| `--help-stats` | Prints descriptions for exported statistics |
| `-V, --version` | Prints the Cognis version and `scx_rustland_core` version |

Only one `sched_ext` scheduler instance should be active at a time. If you installed Cognis as a service, stop that service before launching a foreground or TUI instance.

> [!NOTE]
> The TUI is for diagnostics, not for unattended 24/7 operation. For long-running use, run Cognis headless and treat `--tui` as a short interactive inspection tool.

## Install and Remove

The repository includes helper scripts for service-based installation and cleanup.

### Install

[install.sh](install.sh) can:

- download a GitHub release for `x86_64`, or build locally with `--build-from-source`
- detect CachyOS, Arch, Ubuntu, Debian, and fall back to a generic systemd path
- check for `sched_ext` support and warn when it cannot confirm it
- write or reuse `scx.service`
- manage `/etc/default/scx`
- default the installed service to `--mode desktop`
- enable and restart the scheduler service

Common examples:

```bash
sudo sh install.sh
sudo sh install.sh --dry-run
sudo sh install.sh --build-from-source
sudo sh install.sh --version vX.Y.Z
sudo sh install.sh --flags "--mode server --verbose"
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

## Observability

Cognis exports live stats and can render a ratatui dashboard.

Available surfaces:

- `scx_cognis --monitor 1.0`
- `scx_cognis --stats 1.0`
- `scx_cognis --tui`
- `scx_cognis --help-stats`

What the main counters mean:

- `nr_kernel_dispatches`: tasks handled directly by the BPF scheduler
- `nr_user_dispatches`: tasks that crossed into the userspace compatibility fallback
- `nr_queued` / `nr_scheduled`: current compatibility-fallback backlog
- `sched_p50/p95/p99`: userspace fallback latency percentiles, not full-system frame-time metrics

If `nr_user_dispatches`, `nr_queued`, or `nr_scheduled` stay elevated during a workload that should fit the BPF fast path, that is a signal to investigate the BPF policy rather than a sign that the userspace path is “working as intended.”

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

If you want benchmark numbers you can trust on your hardware, keep the environment fixed, run both schedulers multiple times, and compare repeated local runs rather than relying on one-off impressions.

## Limitations

- Cognis v2 is BPF-first, but it is not yet a pure single-language BPF scheduler with no Rust control process.
- The current implementation still uses `scx_rustland_core` as its userspace scaffold.
- CI cannot prove compositor stability, gaming smoothness, or watchdog safety on GitHub-hosted runners.
- Runtime behavior still depends heavily on kernel version, topology, firmware, browser workload, GPU/compositor stack, and desktop/server load mix.
- Any claim of “better” behavior should come from repeated testing on the target machine.

## Contributing

Changes are most useful when they keep the BPF policy understandable, bounded, and easy to benchmark.

Before sending a change, it is a good idea to run:

```bash
cargo fmt --all -- --check
cargo check --locked
cargo test --locked
sh -n install.sh
sh -n uninstall.sh
```

If a change alters CLI behavior, profiles, exported stats, install scripts, workflows, or scheduler behavior documented here, update this README in the same patch.

## License

This project is licensed under `GPL-2.0-only`. See [LICENSE](LICENSE).
