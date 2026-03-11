// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// scx_cognis — Adaptive CPU Scheduler
//
// Built on scx_rustland_core (sched_ext), this scheduler combines deterministic
// heuristics and bounded statistical helpers in a multi-stage pipeline:
//
//   ┌─────────────────────────────────────────────────────────────────────┐
//   │  ops.enqueue    → Heuristic classifier + trust lookup                │
//   │  ops.dispatch   → load-driven slice base + bounded wake urgency      │
//   │               → SHARED_DSQ (RL_CPU_ANY) for most user tasks          │
//   │  ops.select_cpu → kernel idle-CPU query (pick_idle_cpu, atomic)     │
//   │  housekeeping   → trust flush + slice-base refresh outside dispatch  │
//   └─────────────────────────────────────────────────────────────────────┘
//
// All hot-path data structures are fixed-size and allocated once at startup:
// no HashMap, BTreeSet, or per-event heap allocations on the scheduling path.
//
// The BPF dispatcher (provided by scx_rustland_core) is completely agnostic
// of this scheduling policy; only this Rust file implements the logic.

mod bpf_skel;
pub use bpf_skel::*;
pub mod bpf_intf;

#[rustfmt::skip]
mod bpf;
use bpf::*;

mod ai;
mod stats;
mod task_queue;
mod tui;

use std::io;
use std::mem::MaybeUninit;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use std::os::unix::process::CommandExt;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use libbpf_rs::OpenObject;
use log::{debug, info, warn};
use procfs::process::Process;

use scx_stats::prelude::*;
use scx_utils::build_id;
use scx_utils::libbpf_clap_opts::LibbpfOpts;
use scx_utils::CoreType as TopologyCoreType;
use scx_utils::Topology;
use scx_utils::UserExitInfo;

use ai::{
    Autopilot, BurstPredictor, CpuCoreType, CpuSelector, CpuState, ExitObservation,
    HeuristicClassifier, SliceController, TaskFeatures, TaskLabel, TrustTable, SHAME_MAX,
};
use stats::Metrics;
use task_queue::{QueuePush, TaskQueue};
use tui::SharedState;

const SCHEDULER_NAME: &str = "Cognis";
const NSEC_PER_USEC: u64 = 1_000;
const NSEC_PER_SEC: u64 = 1_000_000_000;
const NON_COMPUTE_EXEC_CAP_NS: u64 = 8_000_000;
const RESTART_BACKOFF: Duration = Duration::from_millis(250);
const RAPID_FAILURE_WINDOW: Duration = Duration::from_secs(30);
const RAPID_FAILURE_LIMIT: u32 = 20;

// ── CLI Options ────────────────────────────────────────────────────────────

/// scx_cognis: an adaptive CPU scheduler combining heuristics and bounded runtime models.
///
/// Scheduling pipeline: a deterministic heuristic task classifier, Elman RNN burst prediction
/// with fixed core weights plus bounded per-PID residual correction, a combined trust/anomaly
/// table for reputation tracking, and a load-driven slice base with bounded per-task interactive
/// renewal — all targeting sub-10µs per-event latency with zero
/// hot-path heap allocations. User tasks are dispatched to SHARED_DSQ (RL_CPU_ANY) so any
/// available CPU can pick them up, preventing the per-CPU DSQ stall that the previous
/// bpf.select_cpu() dispatch caused. Kernel workers are still pinned to their affinity CPU.
#[derive(Debug, Parser)]
struct Opts {
    /// Base scheduling slice duration in microseconds.
    ///
    /// Set to 0 (default) to let the scheduler auto-compute the slice ceiling
    /// from system load: `targeted_latency (6 ms) / nr_runnable_tasks_per_cpu`.
    /// This is the recommended mode — no manual tuning required.
    ///
    /// Set to a non-zero value to pin the maximum slice to that value, overriding
    /// the auto-computed ceiling.  Useful for tuning latency budgets on specific hardware.
    #[clap(short = 's', long, default_value = "0")]
    slice_us: u64,

    /// Minimum scheduling slice duration in microseconds.
    #[clap(short = 'S', long, default_value = "1000")]
    slice_us_min: u64,

    /// If set, per-CPU tasks are dispatched directly to their only eligible CPU.
    #[clap(short = 'l', long, action = clap::ArgAction::SetTrue)]
    percpu_local: bool,

    /// If set, only tasks with SCHED_EXT policy are managed.
    #[clap(short = 'p', long, action = clap::ArgAction::SetTrue)]
    partial: bool,

    /// Exit debug dump buffer length. 0 = default.
    #[clap(long, default_value = "0")]
    exit_dump_len: u32,

    /// Enable verbose output (BPF details + tracefs events).
    #[clap(short = 'v', long, action = clap::ArgAction::SetTrue)]
    verbose: bool,

    /// Launch the ratatui TUI dashboard.
    #[clap(short = 't', long, action = clap::ArgAction::SetTrue)]
    tui: bool,

    /// Enable stats monitoring with the specified interval (seconds).
    #[clap(long)]
    stats: Option<f64>,

    /// Run in stats monitoring mode only (scheduler not launched).
    #[clap(long)]
    monitor: Option<f64>,

    /// Show descriptions for statistics.
    #[clap(long)]
    help_stats: bool,

    /// Print scheduler version and exit.
    #[clap(short = 'V', long, action = clap::ArgAction::SetTrue)]
    version: bool,

    #[clap(flatten, next_help_heading = "Libbpf Options")]
    pub libbpf: LibbpfOpts,
}

// ── Task record ────────────────────────────────────────────────────────────

/// A task in the user-space scheduler queues.
///
/// Tasks are partitioned into per-label FIFO rings. `deadline` is retained for
/// dispatch-time vtime handoff to BPF, while queue ordering stays O(1):
/// RealTime > Interactive > IoWait > Unknown > Compute, FIFO within each band.
#[derive(Debug, PartialEq, Clone)]
struct Task {
    qtask: QueuedTask,
    deadline: u64,
    timestamp: u64,
    label: TaskLabel,
    wake_boosted: bool,
    wake_credit_ns: u64,
    latency_sensitive: bool,
    perf_cri: f32,
    /// Kernel worker threads should stay on their previously used CPU even
    /// when they reach Rust with a widened affinity mask on newer kernels.
    is_kernel_worker: bool,
    slice_ns: u64,
}

// ── Per-task lifetime tracking (for trust updates on exit) ─────────

#[derive(Debug, Default, Clone)]
struct TaskLifetime {
    slice_assigned_ns: u64,
    slice_used_ns: u64,
    preempted: bool,
    cheat_flagged: bool,
    /// Bounded per-task interactive slice credit. This acts like a small
    /// budget bank for wake-heavy desktop tasks so they do not restart every
    /// burst from the same purely global slice recommendation.
    interactive_slice_credit_ns: u64,
    /// Bounded per-PID additive renewal bias. Positive values add headroom to
    /// future latency-sensitive interactive slices; negative values trim that
    /// headroom back when the task stops using it.
    interactive_slice_bias_ns: i64,
    /// The time-slice (ns) that was assigned to this task on its most recent
    /// scheduling event.  Used in the next cycle as the denominator for
    /// `cpu_intensity = burst_ns / last_slice_ns`, which gives a reliable
    /// slice-usage fraction. Defaults to 0 (→ base_slice_ns is used instead).
    last_slice_ns: u64,
    /// Nanosecond timestamp from [`Scheduler::now_ns`] of the last time this
    /// PID was dequeued from BPF. Used to detect genuinely departed tasks so
    /// trust eviction only fires for tasks that have actually
    /// left, not for still-active tasks on every scheduling loop.
    last_seen_ns: u64,
}

// ── Main Scheduler Struct ──────────────────────────────────────────────────

/// Fixed capacity of each per-label task ring.
///
/// This is intentionally much larger than the upstream kernel→userspace ring
/// buffer depth so the userspace side can absorb bursts without reallocating.
/// One ring is allocated once per label at init and never grows afterwards.
const QUEUE_DEPTH: usize = 16_384;

/// Lifetime table size — must be a power of 2.  Sized to hold all concurrent
/// PIDs on even the largest NUMA servers with comfortable headroom.
const LIFETIME_TABLE_SIZE: usize = 4096;

/// Fibonacci multiplier for i32 → table-slot hashing.
const FIB32_MAIN: u32 = 2_654_435_769;

struct Scheduler<'a> {
    bpf: BpfScheduler<'a>,
    opts: &'a Opts,
    stats_server: StatsServer<(), Metrics>,

    // Per-label fixed-capacity task queues (priority order: RT > Interactive > IoWait > Unknown > Compute).
    // A separate boosted interactive lane gives short-lived wake-sensitive
    // tasks one temporary priority step above the normal interactive FIFO.
    boosted_interactive_queue: TaskQueue<Task>,
    // Each queue has one inline deferred slot so a single saturation event can
    // be absorbed without losing a runnable task.
    rt_queue: TaskQueue<Task>,
    interactive_queue: TaskQueue<Task>,
    iowait_queue: TaskQueue<Task>,
    unknown_queue: TaskQueue<Task>,
    compute_queue: TaskQueue<Task>,

    // Time tracking.
    vruntime_now: u64,
    init_page_faults: u64,
    base_slice_ns: u64,
    slice_ns_min: u64,

    // Scheduling policy components.
    classifier: HeuristicClassifier,
    burst_pred: BurstPredictor,
    trust: Box<TrustTable>,
    slice_controller: SliceController,
    cpu_selector: CpuSelector,
    autopilot: Autopilot,

    // Fixed-size per-PID lifetime table (Fibonacci hash, zero-alloc after init).
    lifetime_table: Box<[TaskLifetime; LIFETIME_TABLE_SIZE]>,
    lifetime_pids: Box<[i32; LIFETIME_TABLE_SIZE]>,

    // TUI shared state (None if TUI not requested).
    tui_state: Option<SharedState>,
    /// Inline TUI terminal handle — avoids spawning a thread (prevents EPERM
    /// from cgroup pids.max limits when running under sudo).
    tui_term: Option<tui::Term>,
    tui_quit: bool,
    last_tui_render: Instant,
    last_tui_hist: Instant,

    // Periodic tick timers.
    last_trust_tick: Instant,
    last_slice_tick: Instant,
    /// Rate-limiter for [`flush_trust_updates`]: only runs once per second
    /// and only evicts PIDs that have not been seen for ≥ 2 s.
    last_trust_flush: Instant,
    /// True once stats response channel fails (e.g. broken pipe). Scheduling
    /// must continue regardless of stats client lifecycle.
    stats_channel_failed: bool,

    // Running counters for scheduling policy metrics.
    label_counts: [u64; 5],
    total_inference_ns: u64,
    inference_samples: u64,
    /// Exponential moving average of the final slice assigned after all
    /// per-task adjustments. Exported so monitor/TUI can show what tasks have
    /// actually been receiving instead of only the global slice base.
    assigned_slice_ema_ns: u64,
    /// Number of higher-priority dispatches since the last compute rescue.
    ///
    /// This prevents aged compute tasks from taking over the queue as soon as
    /// they cross the starvation threshold, while still guaranteeing periodic
    /// progress under a sustained interactive load.
    compute_rescue_credit: u8,
    /// Smoothed placement criticality signal used to bias topology-aware CPU
    /// preference on hybrid systems.
    placement_perf_ema: f32,
}

impl<'a> Scheduler<'a> {
    fn init(
        opts: &'a Opts,
        open_object: &'a mut MaybeUninit<OpenObject>,
        shutdown: Arc<AtomicBool>,
    ) -> Result<Self> {
        let stats_server = StatsServer::new(stats::server_data()).launch()?;

        // When --slice-us is 0 (the default), auto mode derives the slice from
        // current runnable load and the targeted latency budget.
        //
        // When --slice-us > 0, the user has chosen an explicit ceiling; we use
        // it as the reference for vruntime fairness and as an upper bound on
        // the load-derived slice.
        let base_slice_ns = opts.slice_us * NSEC_PER_USEC; // 0 when auto mode
        let slice_ns_min = opts.slice_us_min * NSEC_PER_USEC;

        let slice_controller = SliceController::new(base_slice_ns);
        let initial_assigned_slice_ns = slice_controller.read_slice_ns();
        let cpu_selector = Self::build_cpu_selector();

        // Autopilot proposer (always-on, conservative-by-default).
        let autopilot = Autopilot::new(slice_controller.read_min(), slice_controller.read_max());

        let bpf = BpfScheduler::init(
            shutdown,
            open_object,
            opts.libbpf.clone().into_bpf_open_opts(),
            opts.exit_dump_len,
            opts.partial,
            opts.verbose,
            true,
            slice_ns_min,
            "cognis",
        )?;

        let tui_state = if opts.tui {
            Some(tui::new_shared_state())
        } else {
            None
        };
        // Set up TUI terminal inline — no thread spawned. The TUI is driven
        // from within the scheduler's main run() loop via tick_tui().
        let tui_term = if opts.tui {
            match tui::setup_terminal() {
                Ok(t) => Some(t),
                Err(e) => {
                    eprintln!("[WARN] TUI init failed: {e}; continuing without TUI");
                    None
                }
            }
        } else {
            None
        };

        debug!(
            "{} version {} — scx_rustland_core {}",
            SCHEDULER_NAME,
            build_id::full_version(env!("CARGO_PKG_VERSION")),
            scx_rustland_core::VERSION
        );

        Ok(Self {
            bpf,
            opts,
            stats_server,
            boosted_interactive_queue: TaskQueue::with_capacity(QUEUE_DEPTH),
            rt_queue: TaskQueue::with_capacity(QUEUE_DEPTH),
            interactive_queue: TaskQueue::with_capacity(QUEUE_DEPTH),
            iowait_queue: TaskQueue::with_capacity(QUEUE_DEPTH),
            unknown_queue: TaskQueue::with_capacity(QUEUE_DEPTH),
            compute_queue: TaskQueue::with_capacity(QUEUE_DEPTH),
            vruntime_now: 0,
            init_page_faults: 0,
            base_slice_ns,
            slice_ns_min,
            classifier: HeuristicClassifier::new(),
            burst_pred: BurstPredictor::new(),
            trust: TrustTable::new(),
            slice_controller,
            autopilot,
            cpu_selector,
            lifetime_table: {
                // SAFETY: TaskLifetime is a plain struct with u64/bool fields;
                // all-zero bytes produce valid TaskLifetime::default() values.
                unsafe {
                    let layout = std::alloc::Layout::array::<TaskLifetime>(LIFETIME_TABLE_SIZE)
                        .expect("lifetime_table layout");
                    let ptr = std::alloc::alloc_zeroed(layout)
                        as *mut [TaskLifetime; LIFETIME_TABLE_SIZE];
                    assert!(!ptr.is_null(), "lifetime_table allocation failed");
                    Box::from_raw(ptr)
                }
            },
            lifetime_pids: {
                unsafe {
                    let layout = std::alloc::Layout::array::<i32>(LIFETIME_TABLE_SIZE)
                        .expect("lifetime_pids layout");
                    let ptr = std::alloc::alloc_zeroed(layout) as *mut [i32; LIFETIME_TABLE_SIZE];
                    assert!(!ptr.is_null(), "lifetime_pids allocation failed");
                    Box::from_raw(ptr)
                }
            },
            tui_state,
            tui_term,
            tui_quit: false,
            last_tui_render: Instant::now(),
            last_tui_hist: Instant::now(),
            last_trust_tick: Instant::now(),
            last_slice_tick: Instant::now(),
            last_trust_flush: Instant::now(),
            stats_channel_failed: false,
            label_counts: [0; 5],
            total_inference_ns: 0,
            inference_samples: 0,
            assigned_slice_ema_ns: initial_assigned_slice_ns,
            compute_rescue_credit: 0,
            placement_perf_ema: 0.5,
        })
    }

    // ── Helpers ────────────────────────────────────────────────────────────

    fn now_ns() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
    }

    #[inline(always)]
    fn sample_assigned_slice(&mut self, slice_ns: u64) {
        if self.assigned_slice_ema_ns == 0 {
            self.assigned_slice_ema_ns = slice_ns;
        } else {
            self.assigned_slice_ema_ns =
                (self.assigned_slice_ema_ns.saturating_mul(7) + slice_ns) / 8;
        }
    }

    #[inline(always)]
    fn effective_slice_pressure(nr_running: u64, nr_queued: u64, nr_scheduled: u64) -> u64 {
        nr_running
            .saturating_add(nr_queued)
            .saturating_add(nr_scheduled)
    }

    fn get_page_faults() -> Result<u64, io::Error> {
        let me = Process::myself().map_err(io::Error::other)?;
        let st = me.stat().map_err(io::Error::other)?;
        // Only count *major* faults (requires disk I/O — genuine swap pressure).
        // Minor faults (minflt) are normal anonymous-memory / CoW events and
        // accumulate constantly during ordinary operation; including them would
        // produce a permanently non-zero pf counter and a bogus TLDR warning.
        Ok(st.majflt)
    }

    fn build_cpu_selector() -> CpuSelector {
        let mut selector = CpuSelector::new();

        match Topology::new() {
            Ok(topology) => {
                for cpu in topology.all_cpus.values() {
                    let core_type = match cpu.core_type {
                        TopologyCoreType::Little => CpuCoreType::Efficient,
                        TopologyCoreType::Big { .. } => CpuCoreType::Performance,
                    };
                    selector.update_cpu(CpuState {
                        cpu_id: cpu.id as i32,
                        core_type,
                        numa_node: cpu.node_id as u32,
                        llc_id: cpu.llc_id as u16,
                        restricted: false,
                    });
                }

                info!(
                    "topology-aware placement: {} CPUs across {} NUMA node(s), {} LLC domain(s){}",
                    selector.nr_cpus,
                    topology.nodes.len(),
                    topology.all_llcs.len(),
                    if selector.has_little_cores() {
                        ", hybrid cores detected"
                    } else {
                        ""
                    }
                );
            }
            Err(err) => warn!("topology init failed, falling back to shared placement: {err}"),
        }

        selector
    }

    fn scale_by_weight_inverse(task: &QueuedTask, value: u64) -> u64 {
        let weight = task.weight.max(1);
        value.saturating_mul(100) / weight
    }

    #[inline(always)]
    fn placement_perf_cri(
        label: TaskLabel,
        features: &TaskFeatures,
        latency_sensitive: bool,
        is_kernel_worker: bool,
    ) -> f32 {
        let mut perf_cri = match label {
            TaskLabel::RealTime => 1.0,
            TaskLabel::Interactive => {
                0.45 + features.exec_ratio * 0.25 + features.cpu_intensity * 0.20
            }
            TaskLabel::IoWait => 0.20 + features.exec_ratio * 0.20,
            TaskLabel::Unknown => 0.30 + features.exec_ratio * 0.15,
            TaskLabel::Compute => 0.10 + (1.0 - features.exec_ratio) * 0.10,
        };

        if latency_sensitive {
            perf_cri = perf_cri.max(0.80);
        }
        if is_kernel_worker {
            perf_cri = 1.0;
        }

        perf_cri.clamp(0.0, 1.0)
    }

    /// Returns true if the task's comm identifies a kernel worker thread.
    ///
    /// The upstream BPF backend fast-dispatches strictly per-CPU kthreads
    /// (`PF_KTHREAD && nr_cpus_allowed == 1`) before they reach Rust.  On
    /// Linux >= 6.13 the workqueue subsystem reworked per-CPU worker affinity:
    /// nominally per-CPU workers such as `kworker/N:M` may now carry
    /// `nr_cpus_allowed > 1` and fall through to the Rust scheduling loop.
    /// The heuristic classifier assigns them `Compute` (high cpu_intensity,
    /// low exec_ratio — they burst through slices without sleeping) — the
    /// lowest-priority bucket — where they starve behind Interactive and IoWait
    /// traffic until the 5 s sched_ext watchdog fires.
    ///
    /// This function detects such threads from their comm name and the caller
    /// forces them into the `RealTime` bucket so they are always dispatched
    /// before any user-space task.
    ///
    /// Zero allocation — operates directly on the fixed `[c_char; 16]` byte
    /// array.  Bounded by `TASK_COMM_LEN` (16 bytes) — O(1).
    #[inline(always)]
    fn is_kernel_worker(task: &QueuedTask) -> bool {
        // Reinterpret c_char (i8 on Linux) as bytes for ASCII comparison.
        let bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(task.comm.as_ptr() as *const u8, task.comm.len()) };
        let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let s = &bytes[..len];

        // Kernel threads that embed '/' in their comm name:
        //   kworker/N:M, kworker/uN:M, ksoftirqd/N, rcuop/N, rcuog/N,
        //   migration/N, irq/N-name, idle_inject/N, cpuhp/N, watchdog/N.
        if s.contains(&b'/') {
            return true;
        }

        // Kernel daemons whose comm name does not include '/'.
        const KPREFIXES: &[&[u8]] = &[
            b"kswapd",
            b"khugepaged",
            b"kcompactd",
            b"kthreadd",
            b"kdevtmpfs",
            b"kauditd",
            b"kcryptd",
            b"kblockd",
        ];
        for prefix in KPREFIXES {
            if s.starts_with(prefix) {
                return true;
            }
        }
        false
    }

    // ── Fixed-table lifetime helpers ───────────────────────────────────

    /// Fibonacci hash: map PID → lifetime table slot.
    #[inline(always)]
    fn lifetime_slot(pid: i32) -> usize {
        ((pid as u32).wrapping_mul(FIB32_MAIN) >> 20) as usize
    }

    /// Return shared reference to lifetime entry if this PID owns the slot.
    #[inline(always)]
    fn lifetime_get(&self, pid: i32) -> Option<&TaskLifetime> {
        let s = Self::lifetime_slot(pid);
        if self.lifetime_pids[s] == pid && pid != 0 {
            Some(&self.lifetime_table[s])
        } else {
            None
        }
    }

    /// Return mutable reference to lifetime entry, evicting stale PID if needed.
    #[inline(always)]
    fn lifetime_get_mut_or_default(&mut self, pid: i32) -> &mut TaskLifetime {
        let s = Self::lifetime_slot(pid);
        if self.lifetime_pids[s] != pid {
            self.lifetime_pids[s] = pid;
            self.lifetime_table[s] = TaskLifetime::default();
        }
        &mut self.lifetime_table[s]
    }

    /// Evict the lifetime entry for a PID.
    #[inline(always)]
    fn lifetime_evict(&mut self, pid: i32) {
        let s = Self::lifetime_slot(pid);
        if self.lifetime_pids[s] == pid {
            self.lifetime_pids[s] = 0;
            self.lifetime_table[s] = TaskLifetime::default();
        }
    }

    // ── Per-label fixed-capacity task queue helpers ────────────────────

    #[inline(always)]
    fn queue_is_starved(front: Option<&Task>, now: u64, threshold_ns: u64) -> bool {
        front.is_some_and(|t| now.saturating_sub(t.timestamp) >= threshold_ns)
    }

    #[inline(always)]
    fn has_deferred_tasks(&self) -> bool {
        self.boosted_interactive_queue.has_deferred()
            || self.rt_queue.has_deferred()
            || self.interactive_queue.has_deferred()
            || self.iowait_queue.has_deferred()
            || self.unknown_queue.has_deferred()
            || self.compute_queue.has_deferred()
    }

    #[inline(always)]
    fn should_wake_boost(
        latency_sensitive: bool,
        exec_ratio: f32,
        cpu_intensity: f32,
        sleep_ns: u64,
    ) -> bool {
        const WAKE_BOOST_MIN_SLEEP_NS: u64 = 750_000;
        const WAKE_BOOST_MAX_SLEEP_NS: u64 = 25_000_000;

        latency_sensitive
            && (WAKE_BOOST_MIN_SLEEP_NS..=WAKE_BOOST_MAX_SLEEP_NS).contains(&sleep_ns)
            && exec_ratio >= 0.60
            && cpu_intensity >= 0.45
    }

    #[inline(always)]
    fn should_wake_preempt(
        wake_boosted: bool,
        latency_sensitive: bool,
        wake_credit_ns: u64,
        queued_ns: u64,
    ) -> bool {
        const WAKE_PREEMPT_MIN_CREDIT_NS: u64 = 1_000_000;
        const WAKE_PREEMPT_MAX_QUEUE_NS: u64 = 6_000_000;

        wake_boosted
            && latency_sensitive
            && wake_credit_ns >= WAKE_PREEMPT_MIN_CREDIT_NS
            && queued_ns <= WAKE_PREEMPT_MAX_QUEUE_NS
    }

    #[inline(always)]
    fn wake_deadline_credit_ns(&self, task: &Task) -> u64 {
        const WAKE_DEADLINE_CREDIT_NS: u64 = 4_000_000;

        if task.wake_boosted {
            task.wake_credit_ns
                .max(task.slice_ns / 2)
                .min(WAKE_DEADLINE_CREDIT_NS)
        } else {
            0
        }
    }

    #[inline(always)]
    fn interactive_slice_credit_cap_ns(&self, ref_base: u64) -> u64 {
        ref_base
            .max(self.slice_ns_min)
            .saturating_mul(4)
            .min(NON_COMPUTE_EXEC_CAP_NS.saturating_mul(2))
    }

    #[inline(always)]
    fn interactive_slice_bias_cap_ns(&self, ref_base: u64) -> i64 {
        (self.interactive_slice_credit_cap_ns(ref_base) / 2) as i64
    }

    #[inline(always)]
    fn apply_slice_bias_ns(slice_ns: u64, bias_ns: i64) -> u64 {
        if bias_ns >= 0 {
            slice_ns.saturating_add(bias_ns as u64)
        } else {
            slice_ns.saturating_sub((-bias_ns) as u64)
        }
    }

    #[inline(always)]
    fn interactive_sleep_bonus_ns(&self, sleep_ns: u64, ref_base: u64) -> u64 {
        const WAKE_BOOST_MIN_SLEEP_NS: u64 = 750_000;
        const WAKE_BOOST_MAX_SLEEP_NS: u64 = 25_000_000;

        if !(WAKE_BOOST_MIN_SLEEP_NS..=WAKE_BOOST_MAX_SLEEP_NS).contains(&sleep_ns) {
            return 0;
        }

        (sleep_ns / 4).clamp(self.slice_ns_min / 2, ref_base.max(self.slice_ns_min))
    }

    #[inline(always)]
    fn next_interactive_slice_credit_ns(
        &self,
        prev_credit_ns: u64,
        last_slice_ns: u64,
        burst_ns: u64,
        sleep_ns: u64,
        label: TaskLabel,
        latency_sensitive: bool,
        exec_ratio: f32,
        cpu_intensity: f32,
        ref_base: u64,
    ) -> u64 {
        let cap = self.interactive_slice_credit_cap_ns(ref_base);

        if !matches!(label, TaskLabel::Interactive) || !latency_sensitive {
            return (prev_credit_ns / 2).min(cap);
        }

        let wake_pattern =
            Self::should_wake_boost(latency_sensitive, exec_ratio, cpu_intensity, sleep_ns);
        let used_enough = burst_ns.saturating_add(last_slice_ns / 5) >= last_slice_ns;

        if wake_pattern && used_enough {
            let earn = (last_slice_ns / 2).max(self.interactive_sleep_bonus_ns(sleep_ns, ref_base));
            prev_credit_ns.saturating_add(earn).min(cap)
        } else if burst_ns < last_slice_ns / 3 {
            (prev_credit_ns / 2).min(cap)
        } else {
            prev_credit_ns.saturating_sub(last_slice_ns / 4).min(cap)
        }
    }

    #[inline(always)]
    fn next_interactive_slice_bias_ns(
        &self,
        prev_bias_ns: i64,
        last_slice_ns: u64,
        burst_ns: u64,
        sleep_ns: u64,
        label: TaskLabel,
        latency_sensitive: bool,
        exec_ratio: f32,
        cpu_intensity: f32,
        ref_base: u64,
    ) -> i64 {
        let cap = self.interactive_slice_bias_cap_ns(ref_base);
        let decayed = prev_bias_ns * 3 / 4;

        if !matches!(label, TaskLabel::Interactive) || !latency_sensitive {
            return decayed.clamp(-cap, cap);
        }

        let wake_pattern =
            Self::should_wake_boost(latency_sensitive, exec_ratio, cpu_intensity, sleep_ns);
        let full_use_mark = last_slice_ns.saturating_sub(last_slice_ns / 8);
        let update = if wake_pattern && burst_ns >= full_use_mark {
            (last_slice_ns / 5) as i64
        } else if burst_ns <= last_slice_ns / 2 {
            -((last_slice_ns / 6) as i64)
        } else {
            0
        };

        decayed.saturating_add(update).clamp(-cap, cap)
    }

    /// Route a task to the correct per-label bucket.
    ///
    /// Buckets are allocated once at scheduler init and never resized. If a
    /// primary bucket is momentarily full, one inline deferred slot absorbs the
    /// extra task so the scheduler never drops runnable work on local queue
    /// saturation.
    #[inline(always)]
    fn push_task(&mut self, task: Task) {
        let label = task.label;
        let pid = task.qtask.pid;
        let (result, capacity) = {
            let q = match label {
                TaskLabel::RealTime => &mut self.rt_queue,
                TaskLabel::Interactive if task.wake_boosted => &mut self.boosted_interactive_queue,
                TaskLabel::Interactive => &mut self.interactive_queue,
                TaskLabel::IoWait => &mut self.iowait_queue,
                TaskLabel::Unknown => &mut self.unknown_queue,
                TaskLabel::Compute => &mut self.compute_queue,
            };
            (q.push_back(task), q.capacity())
        };

        match result {
            Ok(QueuePush::Primary) => {}
            Ok(QueuePush::Deferred) => {
                let congested = self.bpf.nr_sched_congested_mut();
                *congested = congested.saturating_add(1);
                warn!(
                    "userspace task queue saturated for pid {} (label {:?}, capacity {}); deferred intake without dropping runnable work",
                    pid,
                    label,
                    capacity
                );
            }
            Err(task) => {
                warn!(
                    "userspace task queue invariant violated for pid {} (label {:?}, capacity {}); no free deferred slot remained",
                    task.qtask.pid,
                    task.label,
                    capacity
                );
                debug_assert!(false, "deferred queue invariant violated");
            }
        }
    }

    /// Pop the highest-priority task available across all five buckets,
    /// with anti-starvation promotion for lower-priority buckets.
    ///
    /// Priority order: RealTime > Interactive > IoWait > Unknown > Compute.
    /// Within a bucket, tasks are served FIFO (insertion order).
    ///
    /// # Guarantee for RealTime tasks
    ///
    /// `rt_queue` is checked **unconditionally first**, before any starvation
    /// promotion logic.  This ensures kernel workers (kworker/N:M,
    /// ksoftirqd/N, etc.) and SCHED_FIFO/RR tasks are dispatched immediately
    /// even under compute-saturated load where every other bucket would
    /// otherwise continuously trigger the anti-starvation threshold.
    ///
    /// # Anti-starvation (lower-priority buckets only)
    ///
    /// Any user task that has waited longer than `STARVATION_NS` (100 ms —
    /// 50× below the 5 s sched_ext watchdog) is promoted immediately.
    /// Buckets are scanned from lowest to highest priority so the most-starved
    /// task wins when multiple buckets breach the threshold simultaneously.
    ///
    /// Cost: one `now_ns()` vDSO call + four saturating-subtract comparisons
    /// per dispatch — O(1), < 50 ns total.
    #[inline(always)]
    fn pop_highest_priority_task(&mut self) -> Option<Task> {
        const BOOSTED_INTERACTIVE_STARVATION_NS: u64 = 8_000_000;
        const INTERACTIVE_STARVATION_NS: u64 = 20_000_000;
        const IOWAIT_STARVATION_NS: u64 = 40_000_000;
        const UNKNOWN_STARVATION_NS: u64 = 100_000_000;
        const COMPUTE_STARVATION_NS: u64 = 250_000_000;
        const COMPUTE_RESCUE_INTERVAL: u8 = 8;

        // RealTime tasks (kernel workers, SCHED_FIFO/RR) are UNCONDITIONALLY
        // highest priority.  They must be checked BEFORE any anti-starvation
        // logic so that no starvation promotion for lower-priority buckets can
        // ever delay a kworker.  Under compute-saturated load the starvation
        // checks used to run first, continuously popping Compute tasks that
        // had crossed the 100 ms threshold, leaving kworkers in rt_queue
        // unserved for 5+ seconds until the kernel watchdog fired.
        if let Some(t) = self.rt_queue.pop_front() {
            self.compute_rescue_credit = self.compute_rescue_credit.saturating_add(1);
            return Some(t);
        }

        let now = Self::now_ns();

        if self.boosted_interactive_queue.front().is_some_and(|task| {
            Self::should_wake_preempt(
                task.wake_boosted,
                task.latency_sensitive,
                task.wake_credit_ns,
                now.saturating_sub(task.timestamp),
            )
        }) {
            self.compute_rescue_credit = self.compute_rescue_credit.saturating_add(1);
            return self.boosted_interactive_queue.pop_front();
        }

        if Self::queue_is_starved(
            self.boosted_interactive_queue.front(),
            now,
            BOOSTED_INTERACTIVE_STARVATION_NS,
        ) {
            self.compute_rescue_credit = self.compute_rescue_credit.saturating_add(1);
            return self.boosted_interactive_queue.pop_front();
        }

        // Anti-starvation promotion — only reached when rt_queue is empty.
        // Interactive and I/O-heavy tasks get tighter latency bounds. Compute
        // tasks are rescued in a bounded way so they make progress without
        // taking over every dispatch once the queue ages past its threshold.
        if Self::queue_is_starved(
            self.interactive_queue.front(),
            now,
            INTERACTIVE_STARVATION_NS,
        ) {
            self.compute_rescue_credit = self.compute_rescue_credit.saturating_add(1);
            return self.interactive_queue.pop_front();
        }
        if Self::queue_is_starved(self.iowait_queue.front(), now, IOWAIT_STARVATION_NS) {
            self.compute_rescue_credit = self.compute_rescue_credit.saturating_add(1);
            return self.iowait_queue.pop_front();
        }
        if Self::queue_is_starved(self.unknown_queue.front(), now, UNKNOWN_STARVATION_NS) {
            self.compute_rescue_credit = self.compute_rescue_credit.saturating_add(1);
            return self.unknown_queue.pop_front();
        }
        if self.compute_rescue_credit >= COMPUTE_RESCUE_INTERVAL
            && Self::queue_is_starved(self.compute_queue.front(), now, COMPUTE_STARVATION_NS)
        {
            self.compute_rescue_credit = 0;
            return self.compute_queue.pop_front();
        }

        // Normal priority order for non-starved tasks.
        if let Some(t) = self.boosted_interactive_queue.pop_front() {
            self.compute_rescue_credit = self.compute_rescue_credit.saturating_add(1);
            return Some(t);
        }
        if let Some(t) = self.interactive_queue.pop_front() {
            self.compute_rescue_credit = self.compute_rescue_credit.saturating_add(1);
            return Some(t);
        }
        if let Some(t) = self.iowait_queue.pop_front() {
            self.compute_rescue_credit = self.compute_rescue_credit.saturating_add(1);
            return Some(t);
        }
        if let Some(t) = self.unknown_queue.pop_front() {
            self.compute_rescue_credit = self.compute_rescue_credit.saturating_add(1);
            return Some(t);
        }
        let task = self.compute_queue.pop_front();
        if task.is_some() {
            self.compute_rescue_credit = 0;
        }
        task
    }

    /// True when all five task buckets are empty.
    #[inline(always)]
    fn tasks_empty(&self) -> bool {
        self.boosted_interactive_queue.is_empty()
            && self.rt_queue.is_empty()
            && self.interactive_queue.is_empty()
            && self.iowait_queue.is_empty()
            && self.unknown_queue.is_empty()
            && self.compute_queue.is_empty()
    }

    /// Total number of tasks across all five buckets.
    #[inline(always)]
    fn tasks_len(&self) -> usize {
        self.boosted_interactive_queue.len()
            + self.rt_queue.len()
            + self.interactive_queue.len()
            + self.iowait_queue.len()
            + self.unknown_queue.len()
            + self.compute_queue.len()
    }

    // ── Scheduling pipeline (ops.enqueue) ───────────────────────────────

    /// Compute task features from a QueuedTask.
    ///
    /// `prev_slice_ns` is the slice duration that was assigned to this task
    /// on its most recent scheduling event (read from `lifetimes`).  If no
    /// history is available yet, the caller passes `base_slice_ns` instead.
    ///
    /// The key feature is `cpu_intensity = burst_ns / prev_slice_ns`, i.e.
    /// "what fraction of its assigned slice did the task actually consume?".
    /// This is unambiguous and stable:
    ///   • ≈ 1.0  task ran to the end of its slice → CPU-bound (Compute)
    ///   • ≈ 0.0  task released the CPU long before the slice expired → I/O-bound
    ///   • ≈ 0.3–0.8  task yields regularly → Interactive
    ///
    /// No dependency on `exec_runtime` semantics, no normalisation against a
    /// global constant that can produce degenerate extreme values.
    fn compute_features(
        task: &QueuedTask,
        base_slice_ns: u64,
        prev_slice_ns: u64,
        nr_cpus: i32,
    ) -> TaskFeatures {
        let burst_ns = task.stop_ts.saturating_sub(task.start_ts);

        // Primary classification feature: slice-usage fraction.
        // prev_slice_ns is the slice assigned in the *previous* cycle for this PID.
        // On a task's very first scheduling event, base_slice_ns is used as a stand-in.
        let denominator = prev_slice_ns.max(1);
        let cpu_intensity = (burst_ns as f64 / denominator as f64).clamp(0.0, 1.0) as f32;

        // Secondary feature: burst relative to the *target* base slice.
        // Useful as an additional signal for burst predictor and future KNN use.
        let runnable_ratio = if base_slice_ns > 0 {
            (burst_ns as f64 / base_slice_ns as f64).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };

        // Freshness: how fresh is this burst relative to accumulated CPU time?
        // Near 1.0 → task just woke (interactive/IO); near 0.0 → never sleeps (compute).
        let exec_ratio = if task.exec_runtime > 0 {
            (burst_ns as f64 / task.exec_runtime as f64).clamp(0.0, 1.0) as f32
        } else {
            1.0
        };

        let weight_norm = (task.weight as f32 / 10000.0).clamp(0.0, 1.0);

        TaskFeatures {
            runnable_ratio,
            cpu_intensity,
            exec_ratio,
            weight_norm,
            cpu_affinity: (task.nr_cpus_allowed as f32 / (nr_cpus as f32).max(1.0)).clamp(0.0, 1.0),
        }
    }

    #[inline(always)]
    fn task_comm_matches(task: &QueuedTask, names: &[&[u8]]) -> bool {
        let bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(task.comm.as_ptr() as *const u8, task.comm.len()) };
        let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let comm = &bytes[..len];

        names.iter().any(|name| comm.starts_with(name))
    }

    #[inline(always)]
    fn is_latency_sensitive(task: &QueuedTask, label: TaskLabel, features: &TaskFeatures) -> bool {
        if !matches!(label, TaskLabel::Interactive) {
            return false;
        }

        if Self::task_comm_matches(
            task,
            &[
                b"kwin_wayland",
                b"kwin_x11",
                b"Xwayland",
                b"firefox",
                b"chrome",
                b"chromium",
                b"brave",
                b"Web Content",
                b"GPU Process",
                b"plasmashell",
                b"gnome-shell",
                b"pipewire",
                b"wireplumber",
            ],
        ) {
            return true;
        }

        features.exec_ratio >= 0.70 && features.cpu_intensity >= 0.55
    }

    fn ai_classify_and_enqueue(
        &mut self,
        task: &mut QueuedTask,
    ) -> (
        u64,
        u64,
        TaskLabel,
        TaskFeatures,
        bool,
        bool,
        f32,
        bool,
        u64,
        i64,
        u64,
    ) {
        let t0 = Self::now_ns();

        let nr_cpus = (*self.bpf.nr_online_cpus_mut()).max(1) as i32;

        // Use the slice assigned to this PID in the previous cycle as the
        // denominator for cpu_intensity (= burst_ns / prev_slice_ns).
        //
        // On the very first event for a new PID there is no lifetime entry yet.
        // Fallback selection:
        //   • manual mode (base_slice_ns > 0): use the user-configured ceiling.
        //   • auto mode   (base_slice_ns == 0): use the policy's current auto
        //     slice (at least 1 ms).  Using 0 as denominator makes
        //     cpu_intensity = clamp(burst_ns / 1, 0, 1) ≈ 1.0 for every
        //     long-running process on its first event, causing the classifier
        //     to label them all as Compute (cpu_intensity > 0.85, exec_ratio
        //     driven low by accumulated exec_runtime).  kwin_wayland, browsers,
        //     and other high-value interactive tasks then start life in the
        //     lowest-priority bucket, compounding any existing per-CPU DSQ lag.
        let prev_slice_ns = self
            .lifetime_get(task.pid)
            .filter(|lt| lt.last_slice_ns > 0)
            .map(|lt| lt.last_slice_ns)
            .unwrap_or_else(|| {
                if self.base_slice_ns > 0 {
                    self.base_slice_ns
                } else {
                    // auto mode: clamp to at least 1 ms so cpu_intensity is meaningful.
                    self.slice_controller
                        .auto_base_ns
                        .max(NSEC_PER_USEC * 1_000)
                }
            });
        let recent_sleep_ns = self
            .lifetime_get(task.pid)
            .map(|lt| Self::now_ns().saturating_sub(lt.last_seen_ns))
            .unwrap_or(0);
        let prev_interactive_credit_ns = self
            .lifetime_get(task.pid)
            .map(|lt| lt.interactive_slice_credit_ns)
            .unwrap_or(0);
        let prev_interactive_bias_ns = self
            .lifetime_get(task.pid)
            .map(|lt| lt.interactive_slice_bias_ns)
            .unwrap_or(0);

        // Build features.
        let features = Self::compute_features(task, self.base_slice_ns, prev_slice_ns, nr_cpus);

        // Detect kernel workers before the label is computed.
        // Unbound kthreads (nr_cpus_allowed > 1 on Linux >= 6.13) are not
        // caught by the BPF per-CPU fast-dispatch guard and reach Rust, where
        // the heuristic misclassifies them as Compute (CPU-intensive burst,
        // low exec_ratio).  Forcing them into RealTime ensures they are always
        // dispatched before user tasks and never trip the sched_ext watchdog.
        let is_kworker = Self::is_kernel_worker(task);

        // Classify using the deterministic heuristic only.
        // Stateless, O(1), no feedback loop — see src/ai/classifier.rs.
        // Kernel workers bypass classification and are always RealTime.
        let label = if is_kworker {
            TaskLabel::RealTime
        } else {
            self.classifier.classify(&features)
        };
        let latency_sensitive = Self::is_latency_sensitive(task, label, &features);
        let perf_cri = Self::placement_perf_cri(label, &features, latency_sensitive, is_kworker);
        self.label_counts[label as usize] += 1;
        self.placement_perf_ema = self.placement_perf_ema * 0.90 + perf_cri * 0.10;

        // Trust-based slice factor.
        let rep_factor = self.trust.slice_factor(task.pid);
        let quarantined = self.trust.is_quarantined(task.pid);

        // Burst predictor — read prediction for this PID (updated on exit path).
        let predicted_burst = self.burst_pred.prediction_for(task.pid);

        // Load-adjusted deterministic base slice.
        let ai_slice = self.slice_controller.read_slice_ns();
        let ref_base = if self.base_slice_ns > 0 {
            self.base_slice_ns
        } else {
            self.slice_controller.auto_base_ns
        };

        // Final time-slice:
        //   base = deterministic slice × label_multiplier × (weight / 100)
        //   clamped to [slice_ns_min .. base_slice * 8]
        let mut slice = (ai_slice as f64
            * label.slice_multiplier()
            * rep_factor
            * (task.weight as f64 / 100.0)) as u64;

        // Render-like interactive tasks wake frequently, burn most of the
        // slice they are given, and need enough uninterrupted CPU time to
        // complete a frame stage before the next vblank. Give them modest
        // extra headroom instead of treating them like generic short-slice UI
        // work.
        if matches!(label, TaskLabel::Interactive)
            && features.cpu_intensity > 0.80
            && features.exec_ratio > 0.60
        {
            slice = ((slice as f64) * 1.25) as u64;
        }

        // Headroom hint: if burst predictor says next burst will be short,
        // give a shorter slice to reduce wasted CPU.
        if predicted_burst > 0 && predicted_burst < slice {
            slice = slice.min(predicted_burst * 2);
        }

        let wake_boosted = Self::should_wake_boost(
            latency_sensitive,
            features.exec_ratio,
            features.cpu_intensity,
            recent_sleep_ns,
        );
        if matches!(label, TaskLabel::Interactive) && latency_sensitive {
            let carry_bonus_ns =
                prev_interactive_credit_ns.min(self.interactive_slice_credit_cap_ns(ref_base)) / 2;
            let sleep_bonus_ns = self.interactive_sleep_bonus_ns(recent_sleep_ns, ref_base);
            slice = slice
                .saturating_add(carry_bonus_ns)
                .saturating_add(sleep_bonus_ns);
            slice = Self::apply_slice_bias_ns(slice, prev_interactive_bias_ns / 2);
        }

        // Ensure clamp min ≤ max even if user passes large --slice-us-min.
        // When in auto mode (base_slice_ns == 0), use the policy's current
        // auto_base_ns as the ceiling reference so clamp_max is meaningful.
        let clamp_max = (ref_base * 8).max(self.slice_ns_min);
        slice = slice.clamp(self.slice_ns_min, clamp_max);
        if quarantined {
            slice = self.slice_ns_min;
        }
        self.sample_assigned_slice(slice);

        let burst_ns = task.stop_ts.saturating_sub(task.start_ts);
        let renewed_slice_credit_ns = self.next_interactive_slice_credit_ns(
            prev_interactive_credit_ns,
            prev_slice_ns,
            burst_ns,
            recent_sleep_ns,
            label,
            latency_sensitive,
            features.exec_ratio,
            features.cpu_intensity,
            ref_base,
        );
        let renewed_slice_bias_ns = self.next_interactive_slice_bias_ns(
            prev_interactive_bias_ns,
            prev_slice_ns,
            burst_ns,
            recent_sleep_ns,
            label,
            latency_sensitive,
            features.exec_ratio,
            features.cpu_intensity,
            ref_base,
        );
        let wake_credit_ns = if wake_boosted {
            self.interactive_sleep_bonus_ns(recent_sleep_ns, ref_base)
                .saturating_add(renewed_slice_credit_ns / 2)
                .saturating_add(renewed_slice_bias_ns.max(0) as u64 / 2)
                .max(self.slice_ns_min)
                .min(4_000_000)
        } else {
            0
        };

        // Update vruntime / deadline.
        //
        // vruntime_now tracks the MAXIMUM observed task vtime — the "virtual
        // clock front".  This matches the scx_rustland reference pattern:
        //   1. New tasks (vtime == 0) start exactly at the current front so
        //      they enter the BTreeSet at the end of the queue, not at the
        //      very beginning (which would give them spurious burst priority).
        //   2. Sleeping tasks can reclaim at most one base_slice of credit,
        //      preventing any preemption cascade when they wake up.
        //   3. Using max() instead of a leaky ÷8 additive keeps vruntime_now
        //      aligned with the true task-vtime front regardless of how many
        //      tasks drain_queued_tasks() processes in a single batch.
        task.vtime = if task.vtime == 0 {
            self.vruntime_now
        } else {
            // Sleeping tasks gain at most one auto/base-slice of credit.
            // Use `ref_base` (policy auto_base or user override) as the cap so
            // the credit stays meaningful even in auto-slice mode.
            let vruntime_min = self.vruntime_now.saturating_sub(ref_base);
            task.vtime.max(vruntime_min)
        };
        let slice_ns_actual = burst_ns;
        let vslice = Self::scale_by_weight_inverse(task, slice_ns_actual);
        task.vtime = task.vtime.saturating_add(vslice);
        // Advance the virtual clock to the new task vtime front.
        self.vruntime_now = self.vruntime_now.max(task.vtime);

        // Compute tasks must not accumulate an exec_runtime deadline penalty.
        // CPU-bound workers never sleep, so exec_runtime would instantly hit
        // the cap and bury them behind every Interactive task.
        // Schedule Compute tasks by vruntime fairness alone.
        //
        // For all other labels, keep the exec-runtime deadline penalty within
        // roughly one 120 Hz frame budget. A render or compositor thread that
        // misses one wakeup should not spend the next 100 ms buried behind CPU
        // hogs. Keeping this cap tight preserves frame pacing under load while
        // still allowing fair vtime-based ordering among interactive peers.
        let exec_cap = if matches!(label, TaskLabel::Compute) {
            0
        } else {
            NON_COMPUTE_EXEC_CAP_NS.max(self.slice_ns_min.saturating_mul(2))
        };
        let deadline = task.vtime.saturating_add(task.exec_runtime.min(exec_cap));

        // Track inference latency.
        let elapsed = Self::now_ns().saturating_sub(t0);
        self.total_inference_ns += elapsed;
        self.inference_samples += 1;

        (
            deadline,
            slice,
            label,
            features,
            is_kworker,
            latency_sensitive,
            perf_cri,
            wake_boosted,
            wake_credit_ns,
            renewed_slice_bias_ns,
            renewed_slice_credit_ns,
        )
    }

    // ── Drain queued tasks (runs scheduling pipeline per task) ──────────────

    fn drain_queued_tasks(&mut self, max_batch: usize) {
        let mut drained = 0usize;

        while drained < max_batch {
            // Once any queue has a deferred task, stop pulling more work from
            // BPF until the dispatch phase folds that deferred task back into
            // its primary FIFO. This prevents any runnable task from being
            // removed from the kernel ring without a guaranteed local slot.
            if self.has_deferred_tasks() {
                break;
            }

            // NOTE: the two early-break guards that existed here previously
            // ("stop if rt_queue non-empty" and "break on first RT task")
            // were removed because they caused a slow-drain loop of 1 task
            // per schedule() call under SCHED_FIFO load.  Priority ordering
            // is maintained by pop_highest_priority_task() in the dispatch
            // phase, which always dequeues RT tasks first regardless of drain
            // order.

            match self.bpf.dequeue_task() {
                Ok(Some(mut task)) => {
                    let (
                        deadline,
                        slice_ns,
                        label,
                        features,
                        is_kernel_worker,
                        latency_sensitive,
                        perf_cri,
                        wake_boosted,
                        wake_credit_ns,
                        renewed_slice_bias_ns,
                        renewed_slice_credit_ns,
                    ) = self.ai_classify_and_enqueue(&mut task);
                    let timestamp = Self::now_ns();

                    // Check trust flag before borrowing lifetime table mutably.
                    let cheat_flagged = self.trust.is_flagged(task.pid);

                    // Track lifetime for trust updates.
                    let e = self.lifetime_get_mut_or_default(task.pid);
                    e.slice_assigned_ns = slice_ns;
                    // Store the assigned slice so the next scheduling event
                    // for this PID can compute cpu_intensity = burst / last_slice.
                    e.last_slice_ns = slice_ns;
                    e.slice_used_ns = task.stop_ts.saturating_sub(task.start_ts);
                    e.preempted = e.slice_used_ns >= slice_ns.saturating_sub(slice_ns / 8);
                    e.cheat_flagged = cheat_flagged;
                    e.interactive_slice_bias_ns = renewed_slice_bias_ns;
                    e.interactive_slice_credit_ns = renewed_slice_credit_ns;
                    e.last_seen_ns = Self::now_ns();

                    // Update burst predictor — reuse the features already
                    // computed during classification to avoid a redundant second
                    // compute_features() call on the hot dispatch path.
                    let burst_ns = task.stop_ts.saturating_sub(task.start_ts);
                    self.burst_pred.observe_and_predict(
                        task.pid,
                        burst_ns,
                        features.exec_ratio,
                        features.cpu_intensity,
                    );

                    self.push_task(Task {
                        deadline,
                        timestamp,
                        label,
                        wake_boosted,
                        wake_credit_ns,
                        latency_sensitive,
                        perf_cri,
                        is_kernel_worker,
                        slice_ns,
                        qtask: task,
                    });

                    drained += 1;
                }
                Ok(None) => break,
                Err(err) => {
                    warn!("dequeue_task error: {err}");
                    break;
                }
            }
        }
    }

    // ── Dispatch one task (ops.dispatch) ──────────────────────────────────

    fn dispatch_task(&mut self) -> bool {
        let Some(task) = self.pop_highest_priority_task() else {
            return true;
        };

        // Quarantine status reduces task slice (enforced in ai_classify_and_enqueue);
        // CPU-level isolation is applied via SHARED_DSQ: quarantined tasks still receive
        // reduced slices; the BPF idle-kick mechanism ensures them fair access to any CPU.
        let _quarantined =
            self.trust.is_quarantined(task.qtask.pid) || self.trust.is_flagged(task.qtask.pid);

        let mut dispatched = DispatchedTask::new(&task.qtask);
        dispatched.slice_ns = task.slice_ns;
        // RealTime tasks (kworkers, SCHED_FIFO/RR) get vtime = 0 so they are
        // inserted at the front of BPF's vtime-ordered SHARED_DSQ.  Without
        // this, a kworker with a large exec_runtime would sort *behind* regular
        // tasks whose vtimes are near the global minimum, causing BPF-level
        // starvation even after the Rust scheduler dispatches them first.
        dispatched.vtime = if matches!(task.label, TaskLabel::RealTime) {
            0
        } else {
            task.deadline
                .saturating_sub(self.wake_deadline_credit_ns(&task))
        };

        // CPU selection: most user tasks still go to SHARED_DSQ (RL_CPU_ANY).
        //
        // Why not per-CPU DSQs?  The BPF ops.dispatch callback picks tasks in this order:
        //   1. SCHED_DSQ  (userspace scheduler process, if pending work exists)
        //   2. cpu_to_dsq(current_cpu)  (per-CPU tasks)
        //   3. SHARED_DSQ
        //
        // As long as notify_complete(N > 0) keeps usersched_has_pending_tasks() true, step 1
        // wins on every ops.dispatch invocation for the CPU that hosts the scheduler.  Any user
        // task pinned to cpu_to_dsq(X) when CPU X is that hosting CPU sits unserved until
        // the userspace scheduler's slice expires and step 1 fails for CPU X — which can easily
        // take > 5 s under sustained load, tripping the sched_ext BPF watchdog with the
        // "runnable task stall" error seen in production (kwin_wayla:cs0, sudo, etc.).
        //
        // SHARED_DSQ is consumed at step 3 by any of the N-1 CPUs that did NOT win the
        // SCHED_DSQ race, guaranteeing forward progress for user tasks regardless of which
        // CPU is currently hosting the scheduler.  An idle-CPU kick is still issued by the
        // BPF dispatch_task() helper (kick_task_cpu → scx_bpf_kick_cpu) so a sleeping CPU
        // wakes up and drains SHARED_DSQ promptly.
        //
        // Latency-sensitive interactive tasks are the exception: they may ask
        // the BPF idle-CPU selector for an explicit target CPU when one is
        // available. This preserves the shared-queue safety path for the bulk
        // of user work while giving render/compositor/browser wakeups a chance
        // to land on an idle core with better cache locality.
        //
        // Kernel workers and --percpu-local tasks are still dispatched to their affined CPU:
        // kthreads must stay on interrupt-affined CPUs; percpu_local is an explicit user request.
        dispatched.cpu = if self.opts.percpu_local || task.is_kernel_worker {
            task.qtask.cpu
        } else if task.latency_sensitive {
            let idle_cpu = self
                .bpf
                .select_cpu(task.qtask.pid, task.qtask.cpu, task.qtask.flags);

            if idle_cpu >= 0
                && self.cpu_selector.accepts_idle_cpu(
                    idle_cpu,
                    task.qtask.cpu,
                    task.label,
                    _quarantined,
                    task.perf_cri,
                    true,
                )
            {
                idle_cpu
            } else {
                RL_CPU_ANY
            }
        } else {
            RL_CPU_ANY
        };

        if self.bpf.dispatch_task(&dispatched).is_err() {
            self.push_task(task);
            return false;
        }
        true
    }

    // ── Periodic housekeeping ───────────────────────────────────────────

    /// Trust/anomaly tick (every 100 ms).
    ///
    /// trust.tick() is an intentional no-op: the TrustTable is updated
    /// synchronously on each task exit (flush_trust_updates), so no
    /// periodic batch scan is needed.  The call is preserved for API symmetry
    /// and to leave a clear hook if periodic decay is added in the future.
    fn tick_trust(&mut self) {
        if self.last_trust_tick.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.last_trust_tick = Instant::now();
        let now = Self::now_ns();
        let (_flagged, _n) = self.trust.tick(now);
        // No per-TGID warning needed; trust.worst_actors() exposes bad actors
        // through the TUI trust watchlist instead.
    }

    /// Deterministic slice update (every 50 ms).
    fn tick_slice_controller(&mut self) {
        if self.last_slice_tick.elapsed() < Duration::from_millis(50) {
            return;
        }
        self.last_slice_tick = Instant::now();

        let nr_cpus = (*self.bpf.nr_online_cpus_mut()).max(1);
        let nr_running = *self.bpf.nr_running_mut();
        let nr_queued = *self.bpf.nr_queued_mut();
        let nr_scheduled = *self.bpf.nr_scheduled_mut();
        let effective_pressure =
            Self::effective_slice_pressure(nr_running, nr_queued, nr_scheduled);

        // Mirror the targeted-latency load formula directly with no delayed
        // learning step or exploration. Feed the controller with running work
        // plus both kinds of userspace queue pressure so the base slice can
        // tighten before backlog fully turns into on-CPU execution.
        self.slice_controller.update(effective_pressure, nr_cpus);
        self.cpu_selector
            .update_avg_perf_cri(self.placement_perf_ema);
        // Autopilot: propose bounded min/max adjustments periodically.
        if let Some((min_ns, max_ns)) = self.autopilot.propose(
            &self.slice_controller,
            self.assigned_slice_ema_ns,
            self.slice_controller.read_auto_base_ns(),
            nr_running,
            nr_queued,
        ) {
            let prev_min = self.slice_controller.read_min();
            let prev_max = self.slice_controller.read_max();
            if min_ns != prev_min || max_ns != prev_max {
                // Use debug level for frequent autopilot adjustments so the
                // TUI's info/log panel isn't flooded with periodic messages.
                // Significant regressions/alerts are still logged at higher
                // levels elsewhere.
                debug!(
                    "Autopilot: applying adaptive caps min={}µs max={}µs",
                    min_ns / NSEC_PER_USEC,
                    max_ns / NSEC_PER_USEC
                );
            }
            // Apply both bounds via a single safe API to avoid transient
            // `min > max` panics if the autopilot ever proposes inconsistent
            // values. `write_min_max` will adjust and warn if needed.
            self.slice_controller.write_min_max(min_ns, max_ns);
        }
    }

    /// Emit trust updates for finished tasks.
    ///
    /// Uses a staleness heuristic: any PID not seen for > 2 seconds is
    /// assumed to have exited. Called once per second.
    ///
    /// Zero heap allocations: stale PIDs are collected into a stack-allocated
    /// fixed array instead of a Vec.
    fn flush_trust_updates(&mut self) {
        // Run at most once per second.
        if self.last_trust_flush.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_trust_flush = Instant::now();

        // Staleness-based exit detection: any PID not seen for > 2 seconds
        // is assumed to have exited.  Robust across all kernel versions —
        // no custom BPF ring buffer required.
        let now = Self::now_ns();
        const STALE_THRESHOLD_NS: u64 = 2 * NSEC_PER_SEC;

        // Collect stale PIDs into a fixed stack array (no heap allocation).
        let mut stale = [0i32; 256];
        let mut stale_n = 0usize;

        for s in 0..LIFETIME_TABLE_SIZE {
            let pid = self.lifetime_pids[s];
            if pid == 0 {
                continue;
            }
            let lt = &self.lifetime_table[s];
            if lt.last_seen_ns > 0
                && now.saturating_sub(lt.last_seen_ns) >= STALE_THRESHOLD_NS
                && stale_n < stale.len()
            {
                stale[stale_n] = pid;
                stale_n += 1;
            }
        }

        for &pid in &stale[..stale_n] {
            // Snapshot the lifetime entry before evicting the slot.
            let lt = {
                let s = Self::lifetime_slot(pid);
                if self.lifetime_pids[s] == pid {
                    self.lifetime_table[s].clone()
                } else {
                    continue;
                }
            };
            self.lifetime_evict(pid);

            let obs = ExitObservation {
                slice_underrun: lt.slice_used_ns < lt.slice_assigned_ns / 2,
                preempted: lt.preempted,
                clean_exit: !lt.cheat_flagged,
                cheat_flagged: lt.cheat_flagged,
                fork_count: 0,
                involuntary_ctx_sw: 0,
            };
            self.trust.update_on_exit(pid, pid, &obs, "");
            self.burst_pred.evict(pid);
        }
    }

    // ── Metrics snapshot ───────────────────────────────────────────────────

    fn get_metrics(&mut self) -> Metrics {
        let page_faults = Self::get_page_faults().unwrap_or_default();
        if self.init_page_faults == 0 {
            self.init_page_faults = page_faults;
        }

        let _total_labeled = self.label_counts.iter().sum::<u64>().max(1);

        let avg_inference_us = if self.inference_samples > 0 {
            self.total_inference_ns as f64 / self.inference_samples as f64 / 1000.0
        } else {
            0.0
        };

        let quarantined_count = self.trust.quarantined_count();

        // Scheduling latency percentiles (ns → µs)
        let (p50_ns, p95_ns, p99_ns) = self.slice_controller.compute_sched_percentiles();
        let p50_us = p50_ns / NSEC_PER_USEC;
        let p95_us = p95_ns / NSEC_PER_USEC;
        let p99_us = p99_ns / NSEC_PER_USEC;

        Metrics {
            version: env!("CARGO_PKG_VERSION").to_string(),
            nr_running: *self.bpf.nr_running_mut(),
            nr_cpus: *self.bpf.nr_online_cpus_mut(),
            nr_queued: *self.bpf.nr_queued_mut(),
            nr_scheduled: *self.bpf.nr_scheduled_mut(),
            nr_page_faults: page_faults.saturating_sub(self.init_page_faults),
            nr_user_dispatches: *self.bpf.nr_user_dispatches_mut(),
            nr_kernel_dispatches: *self.bpf.nr_kernel_dispatches_mut(),
            nr_cancel_dispatches: *self.bpf.nr_cancel_dispatches_mut(),
            nr_bounce_dispatches: *self.bpf.nr_bounce_dispatches_mut(),
            nr_failed_dispatches: *self.bpf.nr_failed_dispatches_mut(),
            nr_sched_congested: *self.bpf.nr_sched_congested_mut(),
            nr_bpf_ewma_updates: *self.bpf.nr_bpf_ewma_updates_mut(),
            nr_kernel_boosts: *self.bpf.nr_kernel_boosts_mut(),
            nr_interactive: self.label_counts[TaskLabel::Interactive as usize],
            nr_compute: self.label_counts[TaskLabel::Compute as usize],
            nr_iowait: self.label_counts[TaskLabel::IoWait as usize],
            nr_realtime: self.label_counts[TaskLabel::RealTime as usize],
            nr_unknown: self.label_counts[TaskLabel::Unknown as usize],
            nr_quarantined: quarantined_count,
            nr_flagged: self.trust.flagged_count(),
            base_slice_us: self.slice_controller.read_slice_ns() / NSEC_PER_USEC,
            assigned_slice_us: self.assigned_slice_ema_ns / NSEC_PER_USEC,
            autopilot_min_us: self.slice_controller.read_min() / NSEC_PER_USEC,
            autopilot_max_us: self.slice_controller.read_max() / NSEC_PER_USEC,
            inference_us: avg_inference_us as u64,
            sched_p50_us: p50_us,
            sched_p95_us: p95_us,
            sched_p99_us: p99_us,
        }
    }

    // ── Main scheduling loop ───────────────────────────────────────────────

    fn schedule(&mut self) {
        // Measure the scheduling pipeline latency for autopilot overhead checks.
        let sched_t0 = Self::now_ns();

        // 1. Drain queued tasks in a bounded batch.
        //
        // Bound the batch so each schedule() call reaches the dispatch phase
        // quickly. 4× nr_cpus is enough to absorb normal bursts; any tasks
        // left in the BPF ring buffer trigger a re-invocation via
        // usersched_has_pending_tasks() automatically.
        let nr_cpus = (*self.bpf.nr_online_cpus_mut()).max(1) as usize;
        let drain_budget = nr_cpus.saturating_mul(4).max(16);
        self.drain_queued_tasks(drain_budget);

        // 2. Dispatch ALL queued tasks — no per-cycle cap.
        //
        // This is the critical fix for runnable-task stall crashes:
        //
        //   notify_complete(N > 0) sets nr_scheduled > 0, which makes
        //   usersched_has_pending_tasks() return true in BPF.  ops.dispatch
        //   then prioritises running the cognis kthread (SCHED_DSQ) over
        //   cpu_to_dsq(X) on every CPU X.  Kworkers dispatched by BPF to
        //   cpu_to_dsq(X) — the CPU currently hosting cognis — can only be
        //   consumed once that CPU stops picking cognis from SCHED_DSQ; if
        //   nr_scheduled stays > 0 for 5+ seconds the sched_ext watchdog fires.
        //
        //   By dispatching every internally-queued task before returning,
        //   tasks_len() == 0 → notify_complete(0) → nr_scheduled = 0 →
        //   usersched_has_pending_tasks() returns false → every CPU's
        //   ops.dispatch falls through to its local per-CPU DSQ and drains
        //   kworkers normally.
        //
        // All user tasks go to SHARED_DSQ (RL_CPU_ANY); dispatch order is
        // maintained by pop_highest_priority_task() (RT first).
        while !self.tasks_empty() {
            if !self.dispatch_task() {
                break;
            }
        }

        // 3. Notify BPF dispatcher of remaining pending work.
        self.bpf.notify_complete(self.tasks_len() as u64);

        // Record the end-to-end schedule() latency (cheap, lock-free ring write).
        let sched_elapsed = Self::now_ns().saturating_sub(sched_t0);
        self.slice_controller
            .record_sched_event_latency(sched_elapsed);
    }

    // ── Background housekeeping ─────────────────────────────────────────────
    //
    // Kept OFF the schedule() critical path.  schedule() is called in a
    // tight BPF dispatch loop; any stall there risks the sched_ext watchdog
    // (which fires if ops.dispatch is not called for several seconds).
    //
    // All three inner functions carry their own rate-limit timers, so
    // calling housekeeping() every ~50 ms from run() is safe: each will
    // no-op immediately if its own timer has not elapsed.
    fn housekeeping(&mut self) {
        self.tick_trust();
        self.tick_slice_controller();
        self.flush_trust_updates();
    }

    // ── TUI state refresh ─────────────────────────────────────────────────

    fn update_tui(&mut self, metrics: &Metrics) {
        let Some(ref state) = self.tui_state else {
            return;
        };
        let avg_us = if self.inference_samples > 0 {
            self.total_inference_ns as f64 / self.inference_samples as f64 / 1_000.0
        } else {
            0.0
        };

        let (actors, n_actors) = self.trust.worst_actors();
        let mut watchlist = [tui::WallEntry::ZERO; SHAME_MAX];
        for (dst, src) in watchlist.iter_mut().zip(actors.iter()).take(n_actors) {
            *dst = tui::WallEntry {
                pid: src.pid,
                comm: src.comm,
                trust: src.trust as f64,
                is_flagged: src.flagged,
            };
        }

        if let Ok(mut s) = state.lock() {
            s.metrics = metrics.clone();
            s.inference_us = avg_us;
            s.set_watchlist(&watchlist, n_actors);
        }
    }

    fn run(&mut self) -> Result<UserExitInfo> {
        let (res_ch, req_ch) = self.stats_server.channels();
        let mut last_housekeeping = Instant::now();

        while !self.bpf.exited() {
            // Core dispatch: classify tasks, fill BPF dispatch ring, notify.
            // Must never stall — sched_ext watchdog fires if too slow.
            self.schedule();

            // Stats: non-blocking try_recv so a disconnected client can't
            // block or crash the scheduler.
            if !self.stats_channel_failed && req_ch.try_recv().is_ok() {
                let m = self.get_metrics();
                self.update_tui(&m);
                if let Err(err) = res_ch.send(m) {
                    warn!(
                        "Stats response channel failed ({err}); continuing scheduler without stats responses"
                    );
                    self.stats_channel_failed = true;
                }
            }

            // Background housekeeping (trust engine tick, deterministic slice update, trust flush).
            // Runs outside schedule() so the BPF dispatch path is never
            // delayed by periodic work.  50 ms outer gate plus each
            // function's inner timer ensures at most one unit of work
            // executes between two consecutive schedule() calls.
            if last_housekeeping.elapsed() >= Duration::from_millis(50) {
                last_housekeeping = Instant::now();
                self.housekeeping();
            }

            // Inline TUI handling (no separate thread — avoids EPERM under sudo).
            // Input is polled every loop with a zero-timeout non-blocking poll
            // so 'q' / Esc remain responsive even under load. Rendering is
            // rate-limited to 10 FPS to keep terminal I/O bounded without
            // waiting for the scheduler to become completely idle.
            if self.tui_term.is_some() {
                if tui::poll_tui_quit() {
                    self.tui_quit = true;
                }

                let should_render = self.last_tui_render.elapsed() >= Duration::from_millis(100);
                if should_render {
                    self.last_tui_render = Instant::now();
                    // Feed fresh metrics to TUI state regardless of whether a
                    // stats client is connected (update_tui is normally only
                    // called when req_ch delivers a client request).
                    let m = self.get_metrics();
                    self.update_tui(&m);
                    if let (Some(ref state), Some(ref mut term)) =
                        (&self.tui_state, &mut self.tui_term)
                    {
                        tui::tick_tui(state, term, &mut self.last_tui_hist);
                    }
                }
            }
            if self.tui_quit {
                break;
            }
        }

        self.bpf.shutdown_and_report()
    }
}

impl Drop for Scheduler<'_> {
    fn drop(&mut self) {
        if let Some(ref mut term) = self.tui_term {
            let _ = tui::restore_terminal(term);
        }
        info!("Unregistered {SCHEDULER_NAME} scheduler");
    }
}

fn elevate_scheduler_thread() {
    // Keep the userspace scheduler responsive without turning it into a
    // permanent real-time thread.
    //
    // The previous implementation promoted the main loop to SCHED_FIFO(1).
    // That diverged from Andrea Righi's stable reference scheduler and was
    // unsafe here because Cognis runs a mostly non-blocking control loop
    // (schedule → housekeeping → optional TUI draw). Under SCHED_FIFO, Linux
    // runs the thread until it blocks, is preempted by a higher-priority RT
    // thread, or yields to an equal-priority peer; normal-priority kworkers on
    // the same CPU can therefore be starved long enough to trip the sched_ext
    // watchdog. See sched(7): https://man7.org/linux/man-pages/man7/sched.7.html
    //
    // A best-effort nice(-20) boost keeps the scheduler favored under CFS
    // while still preserving fair progress for kernel workers and the rest of
    // the system.
    unsafe {
        if libc::setpriority(libc::PRIO_PROCESS, 0, -20) != 0 {
            warn!(
                "Could not raise nice priority to -20 (errno {}); continuing \
                 with default CFS priority",
                *libc::__errno_location()
            );
        }
    }
}

fn panic_payload_to_string(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(msg) = payload.downcast_ref::<&'static str>() {
        (*msg).to_string()
    } else if let Some(msg) = payload.downcast_ref::<String>() {
        msg.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

fn is_runtime_exit_error(err: &anyhow::Error) -> bool {
    err.to_string().starts_with("EXIT:")
}

fn log_cognis_failure(reason: &str) {
    tui::emergency_restore_terminal();
    eprintln!();
    eprintln!("\x1b[31;1m╬══════════════════════════════════════════════════════════════╬\x1b[0m");
    eprintln!("\x1b[31;1m║  COGNIS SCHEDULER — PERMANENT FAILURE                        ║\x1b[0m");
    eprintln!("\x1b[31;1m╟──────────────────────────────────────────────────────────────╢\x1b[0m");
    eprintln!("\x1b[31;1m║  Cognis could not recover.  Your system has automatically     ║\x1b[0m");
    eprintln!("\x1b[31;1m║  fallen back to the kernel EEVDF scheduler.                  ║\x1b[0m");
    eprintln!("\x1b[31;1m╬══════════════════════════════════════════════════════════════╬\x1b[0m");
    eprintln!("  Reason  : {}", reason);
    eprintln!("  Recovery: sudo systemctl restart scx");
    eprintln!("            or: sudo scx_cognis --tui");
    eprintln!("  Report  : https://github.com/galpt/scx_cognis/issues");
    eprintln!();
    log::error!(
        "COGNIS PERMANENT FAILURE — system fell back to kernel EEVDF: {}",
        reason
    );
}

fn install_terminal_panic_hook() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        tui::emergency_restore_terminal();
        default_hook(panic_info);
    }));
}

// ── Entry point ────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let opts = Opts::parse();

    if opts.version {
        println!(
            "{} version {} — scx_rustland_core {}",
            SCHEDULER_NAME,
            build_id::full_version(env!("CARGO_PKG_VERSION")),
            scx_rustland_core::VERSION
        );
        return Ok(());
    }

    if opts.help_stats {
        stats::server_data().describe_meta(&mut std::io::stdout(), None)?;
        return Ok(());
    }

    // Logger.
    let mut lcfg = simplelog::ConfigBuilder::new();
    if lcfg.set_time_offset_to_local().is_err() {
        eprintln!("[WARN] Failed to set local time offset");
    }
    lcfg.set_time_level(simplelog::LevelFilter::Error)
        .set_location_level(simplelog::LevelFilter::Off)
        .set_target_level(simplelog::LevelFilter::Off)
        .set_thread_level(simplelog::LevelFilter::Off);
    simplelog::TermLogger::init(
        simplelog::LevelFilter::Info,
        lcfg.build(),
        simplelog::TerminalMode::Stderr,
        simplelog::ColorChoice::Auto,
    )?;
    install_terminal_panic_hook();

    // Stats monitor mode.
    if let Some(intv) = opts.monitor.or(opts.stats) {
        let jh = std::thread::spawn(move || {
            if let Err(err) = stats::monitor(Duration::from_secs_f64(intv)) {
                eprintln!("[WARN] stats monitor exited: {err}");
            }
        });
        if opts.monitor.is_some() {
            let _ = jh.join();
            return Ok(());
        }
    }

    // Shared shutdown flag and ctrlc/SIGTERM handler — registered ONCE for the
    // entire process lifetime.  The same Arc is passed into every
    // Scheduler::init() call (including after restarts), so a SIGTERM received
    // at any point — including the restart backoff window between two run()
    // iterations — is always observed and stops the outer restart loop.
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let sd = shutdown.clone();
        ctrlc::set_handler(move || {
            sd.store(true, Ordering::Relaxed);
        })
        .context("Error setting Ctrl-C / SIGTERM handler")?;
    }

    // Main scheduler loop with restart support.
    let mut open_object = MaybeUninit::uninit();
    let mut rapid_failures = 0u32;
    let mut last_failure_at: Option<Instant> = None;

    loop {
        // A SIGTERM received during the restart backoff (or while init is
        // still in progress) must not be silently dropped — check the flag
        // before starting a new instance.
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        elevate_scheduler_thread();

        let loop_result = panic::catch_unwind(AssertUnwindSafe(|| -> Result<bool> {
            let mut sched = Scheduler::init(&opts, &mut open_object, shutdown.clone())?;
            Ok(sched.run()?.should_restart())
        }));

        match loop_result {
            Ok(Ok(true)) => continue,
            Ok(Ok(false)) => break,
            Ok(Err(err)) if is_runtime_exit_error(&err) => {
                tui::emergency_restore_terminal();
                let now = Instant::now();
                rapid_failures = if last_failure_at
                    .is_some_and(|prev| now.duration_since(prev) <= RAPID_FAILURE_WINDOW)
                {
                    rapid_failures.saturating_add(1)
                } else {
                    1
                };
                last_failure_at = Some(now);

                if rapid_failures > RAPID_FAILURE_LIMIT {
                    log_cognis_failure(&format!(
                        "exceeded {} restart attempts in {:?}: {}",
                        RAPID_FAILURE_LIMIT, RAPID_FAILURE_WINDOW, err
                    ));
                    std::process::exit(1);
                }

                warn!(
                    "runtime failure detected (attempt {}/{} in {:?}): {}; re-executing for \
                     clean restart in {:?}",
                    rapid_failures, RAPID_FAILURE_LIMIT, RAPID_FAILURE_WINDOW, err, RESTART_BACKOFF
                );
                std::thread::sleep(RESTART_BACKOFF);

                // Re-exec this binary for a completely clean OS state.
                //
                // In-process restart fails after a sched_ext watchdog event because
                // the post-crash kernel state leaves sigaltstack(2) broken (EPERM),
                // which then aborts the process when StatsServer::launch() tries to
                // spawn its stats thread.  exec() replaces the process image in-place
                // (same PID so systemd keeps tracking it) and resets all OS state.
                let exe = std::env::current_exe()
                    .unwrap_or_else(|_| std::path::PathBuf::from("/proc/self/exe"));
                let exec_err = std::process::Command::new(&exe)
                    .args(std::env::args_os().skip(1))
                    .exec();
                // exec() only returns on error — fall back to in-process restart.
                warn!(
                    "re-exec failed ({}); falling back to in-process restart",
                    exec_err
                );
            }
            Ok(Err(err)) => {
                tui::emergency_restore_terminal();
                return Err(err);
            }
            Err(payload) => {
                tui::emergency_restore_terminal();
                let now = Instant::now();
                rapid_failures = if last_failure_at
                    .is_some_and(|prev| now.duration_since(prev) <= RAPID_FAILURE_WINDOW)
                {
                    rapid_failures.saturating_add(1)
                } else {
                    1
                };
                last_failure_at = Some(now);

                if rapid_failures > RAPID_FAILURE_LIMIT {
                    log_cognis_failure(&format!(
                        "exceeded {} restart attempts in {:?}: {}",
                        RAPID_FAILURE_LIMIT,
                        RAPID_FAILURE_WINDOW,
                        panic_payload_to_string(payload.as_ref())
                    ));
                    std::process::exit(1);
                }

                warn!(
                    "scheduler panic detected (attempt {}/{} in {:?}): {}; re-executing for \
                     clean restart in {:?}",
                    rapid_failures,
                    RAPID_FAILURE_LIMIT,
                    RAPID_FAILURE_WINDOW,
                    panic_payload_to_string(payload.as_ref()),
                    RESTART_BACKOFF
                );
                std::thread::sleep(RESTART_BACKOFF);

                // Re-exec for clean OS state (see runtime failure branch above).
                let exe = std::env::current_exe()
                    .unwrap_or_else(|_| std::path::PathBuf::from("/proc/self/exe"));
                let exec_err = std::process::Command::new(&exe)
                    .args(std::env::args_os().skip(1))
                    .exec();
                warn!(
                    "re-exec failed ({}); falling back to in-process restart",
                    exec_err
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Scheduler;

    #[test]
    fn wake_boost_requires_latency_sensitive_task() {
        assert!(!Scheduler::should_wake_boost(true, 0.80, 0.70, 200_000));
        assert!(!Scheduler::should_wake_boost(false, 0.80, 0.70, 8_000_000));
    }

    #[test]
    fn wake_boost_accepts_frame_sized_sleep_gap() {
        assert!(Scheduler::should_wake_boost(true, 0.92, 0.78, 8_000_000));
    }

    #[test]
    fn wake_boost_rejects_long_sleepers() {
        assert!(!Scheduler::should_wake_boost(true, 0.92, 0.78, 40_000_000));
    }

    #[test]
    fn wake_preempt_requires_recent_creditful_task() {
        assert!(!Scheduler::should_wake_preempt(
            true, true, 500_000, 1_000_000
        ));
        assert!(!Scheduler::should_wake_preempt(
            true, true, 2_000_000, 10_000_000
        ));
        assert!(!Scheduler::should_wake_preempt(
            false, true, 2_000_000, 1_000_000
        ));
    }

    #[test]
    fn wake_preempt_accepts_fresh_high_credit_wakeup() {
        assert!(Scheduler::should_wake_preempt(
            true, true, 2_000_000, 1_500_000
        ));
    }

    #[test]
    fn effective_slice_pressure_counts_queued_work() {
        assert_eq!(Scheduler::effective_slice_pressure(15, 2, 0), 17);
        assert_eq!(Scheduler::effective_slice_pressure(15, 2, 3), 20);
    }

    #[test]
    fn effective_slice_pressure_saturates() {
        assert_eq!(
            Scheduler::effective_slice_pressure(u64::MAX, 1, 1),
            u64::MAX
        );
    }
}
