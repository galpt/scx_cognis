// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// scx_cognis — Adaptive CPU Scheduler
//
// Built on scx_rustland_core (sched_ext), this scheduler combines deterministic
// heuristics and statistical/RL-based components in a multi-stage pipeline:
//
//   ┌─────────────────────────────────────────────────────────────────────┐
//   │  ops.enqueue  → Heuristic classifier + Trust check                  │
//   │  ops.dispatch → Q-learning policy (adaptive time slice)              │
//   │  ops.select_cpu → O(1) bitmask CPU selector (P/E-core, quarantine)  │
//   │  ops.tick     → Trust-based anomaly detection (behavioural, zero-alloc) │
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
mod tui;

use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::io;
use std::mem::MaybeUninit;

use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use clap::Parser;
use libbpf_rs::OpenObject;
use log::{info, warn};
use procfs::process::Process;

use scx_stats::prelude::*;
use scx_utils::build_id;
use scx_utils::libbpf_clap_opts::LibbpfOpts;
use scx_utils::UserExitInfo;

use ai::{
    BurstPredictor, CoreType, CpuSelector, CpuState, ExitObservation, HeuristicClassifier,
    PolicyController, SchedulerSignal, TaskFeatures, TaskLabel, TrustTable,
};
use stats::Metrics;
use tui::SharedState;

const SCHEDULER_NAME: &str = "Cognis";
const NSEC_PER_USEC: u64 = 1_000;
const NSEC_PER_SEC: u64 = 1_000_000_000;

// ── CLI Options ────────────────────────────────────────────────────────────

/// scx_cognis: an adaptive CPU scheduler combining heuristics, statistical models, and RL.
///
/// Scheduling pipeline: a deterministic heuristic task classifier, O(1) bitmask CPU selector
/// (P/E-core and quarantine aware), Elman RNN burst prediction (fixed offline-trained weights),
/// a combined trust/anomaly table for reputation tracking, and a tabular Q-learning policy
/// controller — all targeting sub-10µs per-event latency with zero hot-path heap allocations.
#[derive(Debug, Parser)]
struct Opts {
    /// Base scheduling slice duration in microseconds.
    ///
    /// Set to 0 (default) to let the scheduler auto-compute the optimal slice
    /// from system load: `targeted_latency (15 ms) / nr_runnable_tasks_per_cpu`.
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

    /// Number of restricted CPUs reserved for quarantined tasks (0 = disable quarantine).
    #[clap(long, default_value = "1")]
    restricted_cpus: usize,

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

/// A task in the user-space scheduler queue.
///
/// Ordering: RealTime > Interactive > IoWait > Unknown > Compute (by label
/// priority), then by earliest deadline, then by arrival timestamp.  This
/// ensures latency-sensitive tasks always run before batch/compute work while
/// the vtime deadline provides fairness within each priority band.
#[derive(Debug, PartialEq, Eq, Clone)]
struct Task {
    qtask: QueuedTask,
    deadline: u64,
    timestamp: u64,
    label: TaskLabel,
    slice_ns: u64,
    /// Performance criticality score stored as fixed-point u16 (perf_cri × 1000).
    /// Range 0..=1000, representing 0.0..=1.0 with 0.1% resolution.
    /// Stored as integer so Task remains fully Eq-comparable.
    /// Converted back to f32 when passed to the CPU selector.
    perf_cri_fp: u16,
}

// ── Per-task lifetime tracking (for trust updates on exit) ─────────

#[derive(Debug, Default, Clone)]
struct TaskLifetime {
    slice_assigned_ns: u64,
    slice_used_ns: u64,
    preempted: bool,
    cheat_flagged: bool,
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

/// Maximum depth of each per-label VecDeque task bucket.
/// Allocated with_capacity() once at init; no growth happens after that.
const QUEUE_DEPTH: usize = 512;

/// Lifetime table size — must be a power of 2.  Sized to hold all concurrent
/// PIDs on even the largest NUMA servers with comfortable headroom.
const LIFETIME_TABLE_SIZE: usize = 4096;

/// Fibonacci multiplier for i32 → table-slot hashing.
const FIB32_MAIN: u32 = 2_654_435_769;

struct Scheduler<'a> {
    bpf: BpfScheduler<'a>,
    opts: &'a Opts,
    stats_server: StatsServer<(), Metrics>,

    // Per-label task queues (priority order: RT > Interactive > IoWait > Unknown > Compute).
    // Each bucket is pre-allocated to QUEUE_DEPTH and never grows after init.
    rt_queue: VecDeque<Task>,
    interactive_queue: VecDeque<Task>,
    iowait_queue: VecDeque<Task>,
    unknown_queue: VecDeque<Task>,
    compute_queue: VecDeque<Task>,

    // Time tracking.
    vruntime_now: u64,
    init_page_faults: u64,
    base_slice_ns: u64,
    slice_ns_min: u64,

    // Scheduling policy components.
    classifier: HeuristicClassifier,
    cpu_sel: CpuSelector,
    burst_pred: BurstPredictor,
    trust: Box<TrustTable>,
    policy: PolicyController,

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
    last_policy_tick: Instant,
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
    /// Exponential moving average of user-space scheduling latency
    /// (enqueue → dispatch), in nanoseconds. Updated on every successful
    /// dispatch. Used as `latency_p99_norm` in the Q-learning reward signal.
    sched_latency_ema_ns: f64,
    /// EWMA of per-task performance criticality scores observed in the current
    /// scheduling window.  Fed into the CPU selector periodically so the
    /// P/E-core routing threshold adapts to the actual workload composition.
    perf_cri_ema: f32,
}

impl<'a> Scheduler<'a> {
    fn init(opts: &'a Opts, open_object: &'a mut MaybeUninit<OpenObject>) -> Result<Self> {
        let stats_server = StatsServer::new(stats::server_data()).launch()?;

        // When --slice-us is 0 (the default), the auto-slice mode is active:
        // PolicyController.update_load() computes the slice from system load and
        // targeted_latency.  base_slice_ns = 0 signals to PolicyController that
        // the user has not pinned a ceiling, so it uses auto_base_ns exclusively.
        //
        // When --slice-us > 0, the user has chosen an explicit ceiling; we use
        // it as the reference for vruntime fairness AND as the PolicyController
        // ceiling override.
        let base_slice_ns = opts.slice_us * NSEC_PER_USEC; // 0 when auto mode
        let slice_ns_min = opts.slice_us_min * NSEC_PER_USEC;

        let policy = PolicyController::new(base_slice_ns);

        let mut bpf = BpfScheduler::init(
            open_object,
            opts.libbpf.clone().into_bpf_open_opts(),
            opts.exit_dump_len,
            opts.partial,
            opts.verbose,
            true, // built-in idle CPU selection
            slice_ns_min,
            "cognis",
        )?;

        // Build initial CPU topology from real sysfs data.
        let mut cpu_sel = CpuSelector::new();
        {
            let nr_cpus = *bpf.nr_online_cpus_mut() as i32;
            let restricted = opts.restricted_cpus;

            // Read P/E-core and NUMA assignments once at startup.
            let core_type_map = build_core_type_map(nr_cpus);
            let numa_map = build_numa_map();

            for cpu_id in 0..nr_cpus {
                // The last `restricted_cpus` CPUs are reserved for quarantined tasks.
                let is_restricted = cpu_id >= nr_cpus - restricted as i32;
                let core_type = core_type_map
                    .get(&cpu_id)
                    .copied()
                    .unwrap_or(CoreType::Performance);
                let numa_node = numa_map.get(&cpu_id).copied().unwrap_or(0);

                cpu_sel.update_cpu(CpuState {
                    cpu_id,
                    core_type,
                    numa_node,
                    restricted: is_restricted,
                });
            }
        }

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

        info!(
            "{} version {} — scx_rustland_core {}",
            SCHEDULER_NAME,
            build_id::full_version(env!("CARGO_PKG_VERSION")),
            scx_rustland_core::VERSION
        );

        Ok(Self {
            bpf,
            opts,
            stats_server,
            rt_queue: VecDeque::with_capacity(QUEUE_DEPTH),
            interactive_queue: VecDeque::with_capacity(QUEUE_DEPTH),
            iowait_queue: VecDeque::with_capacity(QUEUE_DEPTH),
            unknown_queue: VecDeque::with_capacity(QUEUE_DEPTH),
            compute_queue: VecDeque::with_capacity(QUEUE_DEPTH),
            vruntime_now: 0,
            init_page_faults: 0,
            base_slice_ns,
            slice_ns_min,
            classifier: HeuristicClassifier::new(),
            cpu_sel,
            burst_pred: BurstPredictor::new(),
            trust: TrustTable::new(),
            policy,
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
            last_policy_tick: Instant::now(),
            last_trust_flush: Instant::now(),
            stats_channel_failed: false,
            label_counts: [0; 5],
            total_inference_ns: 0,
            inference_samples: 0,
            sched_latency_ema_ns: 0.0,
            perf_cri_ema: 0.5, // start at midpoint; adapts after first policy tick
        })
    }

    // ── Helpers ────────────────────────────────────────────────────────────

    fn now_ns() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
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

    fn scale_by_weight_inverse(task: &QueuedTask, value: u64) -> u64 {
        let weight = task.weight.max(1);
        value.saturating_mul(100) / weight
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

    // ── Per-label VecDeque task queue helpers ──────────────────────────

    /// Route a task to the correct per-label bucket.
    ///
    /// If the bucket is full (len == QUEUE_DEPTH), the oldest (front) task
    /// is silently dropped to make room — this is a back-pressure signal
    /// that the system is overloaded and very old tasks have missed their
    /// deadline anyway.
    #[inline(always)]
    fn push_task(&mut self, task: Task) {
        let q = match task.label {
            TaskLabel::RealTime => &mut self.rt_queue,
            TaskLabel::Interactive => &mut self.interactive_queue,
            TaskLabel::IoWait => &mut self.iowait_queue,
            TaskLabel::Unknown => &mut self.unknown_queue,
            TaskLabel::Compute => &mut self.compute_queue,
        };
        if q.len() >= QUEUE_DEPTH {
            q.pop_front(); // drop oldest under back-pressure
        }
        q.push_back(task);
    }

    /// Pop the highest-priority task available across all five buckets,
    /// with anti-starvation promotion.
    ///
    /// Normal priority order: RealTime > Interactive > IoWait > Unknown > Compute.
    /// Within a bucket, tasks are served FIFO (insertion order).
    ///
    /// # Anti-starvation
    ///
    /// Kernel workers (kworker/N:M, ksoftirqd/N, etc.) are force-classified
    /// as `RealTime` in `ai_classify_and_enqueue` and therefore land in the
    /// highest-priority bucket by construction — no starvation rescue needed
    /// for them.
    ///
    /// The starvation threshold here is a defence-in-depth for user tasks
    /// that might accumulate unexpected wait time in lower-priority buckets
    /// under sustained high-priority load.  Any task waiting longer than
    /// `STARVATION_NS` (100 ms = 50× below the 5 s watchdog) is promoted
    /// immediately regardless of its label.  Buckets are scanned from
    /// lowest to highest so the most-starved task wins when multiple
    /// buckets breach the threshold simultaneously.
    ///
    /// Cost: one `now_ns()` vDSO call + four saturating-subtract comparisons
    /// per dispatch — O(1), < 50 ns total.
    #[inline(always)]
    fn pop_highest_priority_task(&mut self) -> Option<Task> {
        // 100 ms — 50× safety margin below the 5 s sched_ext watchdog.
        // Reduced from 500 ms since kernel workers are now force-classified as
        // RealTime and should not reach lower-priority buckets.  The 100 ms
        // threshold acts as a defence-in-depth for any unexpected user task.
        const STARVATION_NS: u64 = 100_000_000;
        let now = Self::now_ns();

        // Check buckets from lowest → highest priority so the most-starved
        // bucket wins when multiple buckets are simultaneously over threshold.
        if self
            .compute_queue
            .front()
            .map_or(false, |t| now.saturating_sub(t.timestamp) >= STARVATION_NS)
        {
            return self.compute_queue.pop_front();
        }
        if self
            .unknown_queue
            .front()
            .map_or(false, |t| now.saturating_sub(t.timestamp) >= STARVATION_NS)
        {
            return self.unknown_queue.pop_front();
        }
        if self
            .iowait_queue
            .front()
            .map_or(false, |t| now.saturating_sub(t.timestamp) >= STARVATION_NS)
        {
            return self.iowait_queue.pop_front();
        }
        if self
            .interactive_queue
            .front()
            .map_or(false, |t| now.saturating_sub(t.timestamp) >= STARVATION_NS)
        {
            return self.interactive_queue.pop_front();
        }

        // Normal priority order.
        if let Some(t) = self.rt_queue.pop_front() {
            return Some(t);
        }
        if let Some(t) = self.interactive_queue.pop_front() {
            return Some(t);
        }
        if let Some(t) = self.iowait_queue.pop_front() {
            return Some(t);
        }
        if let Some(t) = self.unknown_queue.pop_front() {
            return Some(t);
        }
        self.compute_queue.pop_front()
    }

    /// True when all five task buckets are empty.
    #[inline(always)]
    fn tasks_empty(&self) -> bool {
        self.rt_queue.is_empty()
            && self.interactive_queue.is_empty()
            && self.iowait_queue.is_empty()
            && self.unknown_queue.is_empty()
            && self.compute_queue.is_empty()
    }

    /// Total number of tasks across all five buckets.
    #[inline(always)]
    fn tasks_len(&self) -> usize {
        self.rt_queue.len()
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

        // Performance criticality: how much would this task benefit from a
        // fast P-core vs an efficient E-core?
        //
        // Approximation from observable burst statistics:
        //   - Tasks that use lots of CPU (high cpu_intensity) AND spend little
        //     time sleeping (low exec_ratio) are the most performance-sensitive:
        //     compilers, video encoders, physics simulations.
        //   - RealTime tasks always get max perf_cri.
        //   - I/O-bound tasks (low cpu_intensity) score low: CPU speed barely
        //     matters when the task is blocked on disk/network most of the time.
        //
        // Formula: blend cpu_intensity weight (0.7) with a non-sleep penalty
        // (0.3) so tasks that are CPU-heavy AND never sleep get a high score
        // while tasks that are CPU-heavy BUT frequently sleep (e.g. a 120fps
        // game render thread syncing to vsync) get a moderate score.
        //
        // Range: [0, 1].  System-wide average tracked in perf_cri_ema and fed
        // into the O(1) CPU selector for dynamic P/E routing.
        let weight_norm = (task.weight as f32 / 10000.0).clamp(0.0, 1.0);
        let non_sleep = 1.0 - exec_ratio.min(1.0) * 0.5;
        let perf_cri = if weight_norm > 0.95 {
            1.0f32 // RealTime tasks unconditionally need the fastest core.
        } else {
            (cpu_intensity * 0.7 + non_sleep * 0.3).clamp(0.0, 1.0)
        };

        TaskFeatures {
            runnable_ratio,
            cpu_intensity,
            exec_ratio,
            weight_norm,
            cpu_affinity: (task.nr_cpus_allowed as f32 / (nr_cpus as f32).max(1.0)).clamp(0.0, 1.0),
            perf_cri,
        }
    }

    fn ai_classify_and_enqueue(
        &mut self,
        task: &mut QueuedTask,
    ) -> (u64, u64, TaskLabel, f32, TaskFeatures) {
        let t0 = Self::now_ns();

        let nr_cpus = (*self.bpf.nr_online_cpus_mut()).max(1) as i32;

        // Use the slice assigned to this PID in the previous cycle as the
        // denominator for cpu_intensity.  This gives the unambiguous
        // "slice-usage fraction" without any global-constant normalisation
        // artefacts.  On the very first event for a new PID, fall back to
        // base_slice_ns so the value is at least reasonable.
        let prev_slice_ns = self
            .lifetime_get(task.pid)
            .filter(|lt| lt.last_slice_ns > 0)
            .map(|lt| lt.last_slice_ns)
            .unwrap_or(self.base_slice_ns);

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
        self.label_counts[label as usize] += 1;

        // Trust-based slice factor.
        let rep_factor = self.trust.slice_factor(task.pid);
        let quarantined = self.trust.is_quarantined(task.pid);

        // Burst predictor — read prediction for this PID (updated on exit path).
        let predicted_burst = self.burst_pred.prediction_for(task.pid);

        // Q-learning-adjusted base slice.
        let ai_slice = self.policy.read_slice_ns();

        // Final time-slice:
        //   base = Q-learning policy slice × label_multiplier × (weight / 100)
        //   clamped to [slice_ns_min .. base_slice * 8]
        let mut slice = (ai_slice as f64
            * label.slice_multiplier()
            * rep_factor
            * (task.weight as f64 / 100.0)) as u64;

        // Headroom hint: if burst predictor says next burst will be short,
        // give a shorter slice to reduce wasted CPU.
        if predicted_burst > 0 && predicted_burst < slice {
            slice = slice.min(predicted_burst * 2);
        }

        // Ensure clamp min ≤ max even if user passes large --slice-us-min.
        // When in auto mode (base_slice_ns == 0), use the policy's current
        // auto_base_ns as the ceiling reference so clamp_max is meaningful.
        let ref_base = if self.base_slice_ns > 0 {
            self.base_slice_ns
        } else {
            self.policy.auto_base_ns
        };
        let clamp_max = (ref_base * 8).max(self.slice_ns_min);
        slice = slice.clamp(self.slice_ns_min, clamp_max);
        if quarantined {
            slice = self.slice_ns_min;
        }

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
        let slice_ns_actual = task.stop_ts.saturating_sub(task.start_ts);
        let vslice = Self::scale_by_weight_inverse(task, slice_ns_actual);
        task.vtime = task.vtime.saturating_add(vslice);
        // Advance the virtual clock to the new task vtime front.
        self.vruntime_now = self.vruntime_now.max(task.vtime);

        // Compute tasks must not accumulate an exec_runtime deadline penalty.
        // CPU-bound workers never sleep, so exec_runtime would instantly hit
        // the cap and bury them behind every Interactive task.
        // Schedule Compute tasks by vruntime fairness alone.
        //
        // For all other labels, cap at 100 × slice_ns_min (≈ 100 ms at
        // default --slice-us-min 1000).  The old cap of 100 × base_slice_ns
        // (≈ 2000 ms) meant any non-Compute task that didn't sleep frequently
        // accumulated a 2-second deadline penalty and was treated as Compute
        // regardless of its label — breaking interactivity under 100% CPU
        // load.  The tighter 100 ms cap matches scx_rustland's behaviour.
        let exec_cap = if matches!(label, TaskLabel::Compute) {
            0
        } else {
            self.slice_ns_min.saturating_mul(100)
        };
        let deadline = task.vtime.saturating_add(task.exec_runtime.min(exec_cap));

        // Track inference latency.
        let elapsed = Self::now_ns().saturating_sub(t0);
        self.total_inference_ns += elapsed;
        self.inference_samples += 1;

        // Kernel workers need the fastest available CPU (P-core) to complete
        // their bounded kernel operations with minimal latency.
        let ret_perf_cri = if is_kworker {
            1.0f32
        } else {
            features.perf_cri
        };
        (deadline, slice, label, ret_perf_cri, features)
    }

    // ── Drain queued tasks (runs scheduling pipeline per task) ──────────────

    fn drain_queued_tasks(&mut self) {
        loop {
            match self.bpf.dequeue_task() {
                Ok(Some(mut task)) => {
                    let (deadline, slice_ns, label, perf_cri, features) =
                        self.ai_classify_and_enqueue(&mut task);
                    let timestamp = Self::now_ns();

                    // Update per-task perf_cri EWMA for the load balancer threshold.
                    // α = 0.05: tracks the running average without being dominated
                    // by any single burst task in a batch.
                    self.perf_cri_ema = self.perf_cri_ema * 0.95 + perf_cri * 0.05;

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
                        slice_ns,
                        perf_cri_fp: (perf_cri * 1000.0).clamp(0.0, 1000.0) as u16,
                        qtask: task,
                    });
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

        // Measure real user-space scheduling latency: the time this task waited
        // in the queues between enqueue (drain_queued_tasks) and now (dispatch).
        // This is a true measure of scheduler responsiveness.
        let wait_ns = Self::now_ns().saturating_sub(task.timestamp);
        // α = 0.05: smooth out per-task jitter while tracking trends over ~20 dispatches.
        self.sched_latency_ema_ns = self.sched_latency_ema_ns * 0.95 + wait_ns as f64 * 0.05;

        let quarantined =
            self.trust.is_quarantined(task.qtask.pid) || self.trust.is_flagged(task.qtask.pid);

        let mut dispatched = DispatchedTask::new(&task.qtask);
        dispatched.slice_ns = task.slice_ns;
        dispatched.vtime = task.deadline;

        // CPU selection: O(1) bitmask selector, percpu_local shortcut, or fast-path.
        //
        // Under full CPU saturation (perf_cri_ema > 0.85 — all runnable tasks are
        // compute-bound) the bitmask select_cpu still runs in O(1) but we skip
        // it entirely and let the BPF kernel's O(1) idle-CPU scan handle placement,
        // which avoids any user-kernel round-trips when every core is occupied.
        // The threshold floats via the perf_cri EWMA, so the selector re-engages
        // automatically when interactive tasks return.
        dispatched.cpu = if self.opts.percpu_local {
            task.qtask.cpu
        } else if self.perf_cri_ema > 0.85 {
            RL_CPU_ANY
        } else {
            let cpu = self.cpu_sel.select_cpu(
                task.qtask.cpu,
                task.label,
                quarantined,
                task.perf_cri_fp as f32 / 1000.0,
            );
            if cpu >= 0 {
                cpu
            } else {
                RL_CPU_ANY
            }
        };

        if self.bpf.dispatch_task(&dispatched).is_err() {
            self.push_task(task);
            return false;
        }
        // Mark the target CPU busy so the next select_cpu() call in this
        // dispatch window won't pick the same CPU again (round-robin effect).
        // Skipped for RL_CPU_ANY dispatches — BPF distributes those itself.
        if dispatched.cpu >= 0 {
            self.cpu_sel.mark_busy(dispatched.cpu);
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
        // through the TUI wall-of-shame instead.
    }

    /// Q-learning policy update (every 250 ms).
    fn tick_policy(&mut self) {
        if self.last_policy_tick.elapsed() < Duration::from_millis(250) {
            return;
        }
        self.last_policy_tick = Instant::now();

        let nr_cpus = (*self.bpf.nr_online_cpus_mut()).max(1);
        let nr_running = *self.bpf.nr_running_mut();
        let total_labeled = self.label_counts.iter().sum::<u64>().max(1) as f64;
        let interactive_frac =
            self.label_counts[TaskLabel::Interactive as usize] as f64 / total_labeled;
        let compute_frac = self.label_counts[TaskLabel::Compute as usize] as f64 / total_labeled;

        // Update the auto-computed slice ceiling based on current load.
        // This mirrors LAVD's slice = targeted_latency / nr_tasks approach and
        // removes the need for a human to tune --slice-us for their workload.
        self.policy.update_load(nr_running, nr_cpus);

        // Update the P/E-core routing threshold in the CPU selector.
        // Uses the per-task perf_cri EWMA accumulated in drain_queued_tasks().
        self.cpu_sel.update_avg_perf_cri(self.perf_cri_ema);

        let sig = SchedulerSignal {
            load_norm: (nr_running as f64 / nr_cpus as f64).min(1.0),
            interactive_frac,
            compute_frac,
            // Real enqueue→dispatch scheduling latency, normalised by 10 ms.
            // Typical: < 100 µs (0.01). Overloaded: 1–5 ms (0.1–0.5).
            latency_p99_norm: (self.sched_latency_ema_ns / 10_000_000.0).min(1.0),
            congestion_rate: *self.bpf.nr_sched_congested_mut() as f64,
        };
        self.policy.update(&sig);
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
            nr_interactive: self.label_counts[TaskLabel::Interactive as usize],
            nr_compute: self.label_counts[TaskLabel::Compute as usize],
            nr_iowait: self.label_counts[TaskLabel::IoWait as usize],
            nr_realtime: self.label_counts[TaskLabel::RealTime as usize],
            nr_unknown: self.label_counts[TaskLabel::Unknown as usize],
            nr_quarantined: quarantined_count,
            nr_flagged: self.trust.flagged_count(),
            ai_slice_us: self.policy.read_slice_ns() / NSEC_PER_USEC,
            ai_inference_us: avg_inference_us as u64,
            reward_ema_x100: (self.policy.reward_ema * 100.0) as i64,
        }
    }

    // ── Main scheduling loop ───────────────────────────────────────────────

    fn schedule(&mut self) {
        // 1. Drain queued tasks (heuristic classify + enqueue).
        self.drain_queued_tasks();

        // 2. Batch-dispatch up to nr_cpus tasks in a single schedule() call.
        //
        // Previously only ONE task was dispatched per cycle.  With 16+ workers
        // all runnable simultaneously, the remaining 15 had to wait for the
        // next BPF dispatch callback.  Rather than waiting, the BPF fell back
        // to its kernel-side fallback path, producing the k >> d→u symptom
        // (kernel dispatches >> user-space dispatches) and leaving CPUs idle.
        //
        // By filling the BPF dispatch list with up to nr_cpus tasks at once,
        // every runnable CPU gets a task in a single round-trip.
        let nr_cpus = (*self.bpf.nr_online_cpus_mut()).max(1) as usize;
        // Reset the idle-CPU bitmask so that each dispatch window starts with
        // all CPUs considered available.  As tasks are dispatched to specific
        // CPUs, those CPUs are marked busy one-by-one, producing round-robin
        // distribution across the eligible pool within each schedule() call.
        // Without this reset, select_cpu() always returns CPU 0 (trailing_zeros
        // of a static all_mask), pinning every task to the same CPU and causing
        // kworker affinity stalls that trip the sched_ext watchdog.
        self.cpu_sel.reset_idle();
        // Dispatch up to 2× nr_cpus per cycle.  When nr_cpus tasks wake
        // simultaneously (common under burst or compute-saturated workloads),
        // a 1× budget forces an extra schedule() round-trip for the overflow.
        // 2× absorbs typical burst spikes without over-committing the BPF
        // dispatch ring, keeping all cores fed in a single pass.
        let dispatch_budget = nr_cpus.saturating_mul(2);
        for _ in 0..dispatch_budget {
            if self.tasks_empty() {
                break;
            }
            if !self.dispatch_task() {
                break;
            }
        }

        // 3. Notify BPF dispatcher of remaining pending work.
        self.bpf.notify_complete(self.tasks_len() as u64);
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
        self.tick_policy();
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
        let shame: Vec<tui::WallEntry> = actors[..n_actors]
            .iter()
            .map(|e| tui::WallEntry {
                pid: e.pid,
                comm: e.comm_str().to_string(),
                trust: e.trust as f64,
                is_flagged: e.flagged,
            })
            .collect();

        if let Ok(mut s) = state.lock() {
            s.metrics = metrics.clone();
            s.inference_us = avg_us;
            s.wall_of_shame = shame;
        }
    }

    fn run(&mut self) -> Result<UserExitInfo> {
        // Elevate this thread to SCHED_FIFO so the userspace scheduler can
        // always preempt ordinary tasks when dispatch decisions are needed.
        //
        // Without this, 100%-CPU workloads starve the scheduler thread: the BPF
        // kernel fallback takes over (k >> d→u), cores go idle waiting for
        // userspace to catch up, and desktop interactivity collapses under load.
        //
        // Priority 1 is the minimum FIFO level — it beats SCHED_NORMAL but
        // yields to any higher-priority RT thread (e.g. audio daemons at
        // SCHED_FIFO 80+), so we don't interfere with real latency-critical work.
        unsafe {
            let param = libc::sched_param { sched_priority: 1 };
            if libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) != 0 {
                warn!(
                    "Could not set SCHED_FIFO (errno {}); continuing with \
                     SCHED_NORMAL — performance may degrade under CPU saturation",
                    *libc::__errno_location()
                );
            }
        }

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

            // Background housekeeping (trust engine tick, Q-learning policy update, trust flush).
            // Runs outside schedule() so the BPF dispatch path is never
            // delayed by periodic work.  50 ms outer gate plus each
            // function's inner timer ensures at most one unit of work
            // executes between two consecutive schedule() calls.
            if last_housekeeping.elapsed() >= Duration::from_millis(50) {
                last_housekeeping = Instant::now();
                self.housekeeping();
            }

            // Inline TUI rendering (no separate thread — avoids EPERM under sudo).
            if self.tui_term.is_some() {
                let should_render = self.last_tui_render.elapsed() >= Duration::from_millis(50);
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
                        if tui::tick_tui(state, term, &mut self.last_tui_hist) {
                            self.tui_quit = true;
                        }
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

// ── CPU topology helpers ──────────────────────────────────────────────────

// ONLINE_CPUS removed – nr_online_cpus is queried live from the BPF scheduler.

// ── Topology helpers (sysfs-based, no heuristics) ─────────────────────────

/// Parse a Linux cpulist string such as "0-3,6,8-11" into a set of CPU IDs.
fn parse_cpulist(s: &str) -> HashSet<i32> {
    let mut set = HashSet::new();
    for part in s.trim().split(',') {
        let part = part.trim();
        if let Some((lo, hi)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (lo.trim().parse::<i32>(), hi.trim().parse::<i32>()) {
                for id in a..=b {
                    set.insert(id);
                }
            }
        } else if let Ok(id) = part.parse::<i32>() {
            set.insert(id);
        }
    }
    set
}

/// Read a sysfs cpulist file and return the set of CPU IDs it contains.
fn read_cpulist_file(path: &str) -> HashSet<i32> {
    std::fs::read_to_string(path)
        .map(|s| parse_cpulist(&s))
        .unwrap_or_default()
}

/// Build a map from `cpu_id → NUMA node` by reading
/// `/sys/devices/system/node/nodeN/cpulist` for every node present.
/// Returns an empty map on single-socket or non-NUMA systems (all CPUs
/// will default to node 0 in the caller).
fn build_numa_map() -> HashMap<i32, u32> {
    let mut map = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/sys/devices/system/node") else {
        return map;
    };
    for entry in entries.flatten() {
        let raw = entry.file_name();
        let name = raw.to_string_lossy();
        if !name.starts_with("node") {
            continue;
        }
        let node_id: u32 = match name["node".len()..].parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let cpulist_path = format!("{}/cpulist", entry.path().display());
        for cpu_id in read_cpulist_file(&cpulist_path) {
            map.insert(cpu_id, node_id);
        }
    }
    map
}

/// Build a map from `cpu_id → CoreType` using the kernel's hybrid-topology
/// sysfs entries:
///
/// - `/sys/devices/cpu_atom/cpus`  — Intel Atom / E-cores (Efficient)
/// - `/sys/devices/cpu_core/cpus`  — Intel Core / P-cores (Performance)
///
/// On AMD, pure-Intel, or VM systems where these entries do not exist the
/// file reads return empty sets and every CPU is treated as Performance
/// (homogeneous topology).
fn build_core_type_map(nr_cpus: i32) -> HashMap<i32, CoreType> {
    let atom_cpus = read_cpulist_file("/sys/devices/cpu_atom/cpus");
    let core_cpus = read_cpulist_file("/sys/devices/cpu_core/cpus");
    let hybrid = !atom_cpus.is_empty() || !core_cpus.is_empty();

    let mut map = HashMap::new();
    for cpu_id in 0..nr_cpus {
        let ct = if hybrid {
            if atom_cpus.contains(&cpu_id) {
                CoreType::Efficient
            } else {
                // Listed in cpu_core set, or not listed in either (treat as P-core).
                CoreType::Performance
            }
        } else {
            // Non-hybrid topology (AMD, homogeneous Intel, VMs).
            CoreType::Performance
        };
        map.insert(cpu_id, ct);
    }
    map
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

    // Main scheduler loop with restart support.
    let mut open_object = MaybeUninit::uninit();
    loop {
        let mut sched = Scheduler::init(&opts, &mut open_object)?;
        if !sched.run()?.should_restart() {
            break;
        }
    }

    Ok(())
}
