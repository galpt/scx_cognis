// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// scx_cognis — An Attempt at an Intelligent CPU Scheduler
//
// Built on scx_rustland_core (sched_ext), this scheduler replaces static
// heuristics with a live AI inference pipeline:
//
//   ┌─────────────────────────────────────────────────────────────────┐
//   │  ops.enqueue  → Heuristic classifier + Reputation check          │
//   │  ops.dispatch → Q-learning policy (adaptive time slice)           │
//   │  ops.select_cpu → A* load balancer (P/E-core aware)             │
//   │  ops.tick     → Isolation Forest anti-cheat                     │
//   │  ops.exit     → Bayesian reputation update                      │
//   │  ops.update_idle → LSTM-lite burst predictor (headroom signal)  │
//   └─────────────────────────────────────────────────────────────────┘
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

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
    AStarLoadBalancer, AntiCheatEngine, BurstPredictor, CoreType, CpuState, ExitObservation,
    HeuristicClassifier, PolicyController, ReputationEngine, SchedulerSignal, TaskFeatures,
    TaskLabel,
};
use stats::Metrics;
use tui::SharedState;

const SCHEDULER_NAME: &str = "Cognis";
const NSEC_PER_USEC: u64 = 1_000;
const NSEC_PER_SEC: u64 = 1_000_000_000;

// ── CLI Options ────────────────────────────────────────────────────────────

/// scx_cognis: an intelligent, AI-driven CPU scheduler.
///
/// scx_cognis uses an ensemble of AI algorithms to make scheduling decisions:
/// heuristic task classification, Isolation Forest anti-cheat, A* CPU placement,
/// Elman-RNN burst prediction, Bayesian reputation tracking, and a tabular Q-learning
/// policy controller — all with a sub-10µs inference latency target.
#[derive(Debug, Parser)]
struct Opts {
    /// Base scheduling slice duration in microseconds (Q-learning policy adjusts this dynamically).
    #[clap(short = 's', long, default_value = "5000")]
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
}

impl Ord for Task {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // RealTime and Interactive tasks get priority over Compute tasks.
        let self_prio = label_priority(self.label);
        let other_prio = label_priority(other.label);

        other_prio
            .cmp(&self_prio) // higher label priority first
            .then_with(|| self.deadline.cmp(&other.deadline))
            .then_with(|| self.timestamp.cmp(&other.timestamp))
            .then_with(|| self.qtask.pid.cmp(&other.qtask.pid))
    }
}

impl PartialOrd for Task {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn label_priority(l: TaskLabel) -> u8 {
    match l {
        TaskLabel::RealTime => 4,
        TaskLabel::Interactive => 3,
        TaskLabel::IoWait => 2,
        TaskLabel::Unknown => 1,
        TaskLabel::Compute => 0,
    }
}

// ── Per-task lifetime tracking (for reputation updates on exit) ─────────

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
    /// the reputation / KNN eviction only fires for tasks that have actually
    /// left, not for still-active tasks on every scheduling loop.
    last_seen_ns: u64,
}

// ── Main Scheduler Struct ──────────────────────────────────────────────────

struct Scheduler<'a> {
    bpf: BpfScheduler<'a>,
    opts: &'a Opts,
    stats_server: StatsServer<(), Metrics>,

    // Task queue (ordered by priority + deadline).
    tasks: BTreeSet<Task>,

    // Time tracking.
    vruntime_now: u64,
    init_page_faults: u64,
    base_slice_ns: u64,
    slice_ns_min: u64,

    // AI components.
    classifier: HeuristicClassifier,
    anti_cheat: AntiCheatEngine,
    load_bal: AStarLoadBalancer,
    burst_pred: BurstPredictor,
    reputation: ReputationEngine,
    policy: PolicyController,

    // Per-PID lifetime tracking for reputation updates.
    lifetimes: HashMap<i32, TaskLifetime>,

    // TUI shared state (None if TUI not requested).
    tui_state: Option<SharedState>,
    tui_shutdown: Arc<AtomicBool>,

    // Periodic tick timers.
    last_anticheat_tick: Instant,
    last_policy_tick: Instant,
    /// Rate-limiter for [`flush_reputation_updates`]: only runs once per second
    /// and only evicts PIDs that have not been seen for ≥ 2 s.
    last_reputation_flush: Instant,
    /// True once stats response channel fails (e.g. broken pipe). Scheduling
    /// must continue regardless of stats client lifecycle.
    stats_channel_failed: bool,

    // Running counters for AImetrics.
    label_counts: [u64; 5],
    total_inference_ns: u64,
    inference_samples: u64,
}

impl<'a> Scheduler<'a> {
    fn init(opts: &'a Opts, open_object: &'a mut MaybeUninit<OpenObject>) -> Result<Self> {
        let stats_server = StatsServer::new(stats::server_data()).launch()?;

        let base_slice_ns = opts.slice_us * NSEC_PER_USEC;
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
        let mut load_bal = AStarLoadBalancer::new();
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

                load_bal.update_cpu(CpuState {
                    cpu_id,
                    core_type,
                    utilisation: 0.0,
                    numa_node,
                    throttled: false,
                    restricted: is_restricted,
                });
            }
        }

        let tui_state = if opts.tui {
            Some(tui::new_shared_state())
        } else {
            None
        };
        let tui_shutdown = Arc::new(AtomicBool::new(false));

        // Launch TUI thread.
        if let Some(ref state) = tui_state {
            let state_clone = Arc::clone(state);
            let shutdown_clone = Arc::clone(&tui_shutdown);
            std::thread::spawn(move || tui::run_tui(state_clone, shutdown_clone));
        }

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
            tasks: BTreeSet::new(),
            vruntime_now: 0,
            init_page_faults: 0,
            base_slice_ns,
            slice_ns_min,
            classifier: HeuristicClassifier::new(),
            anti_cheat: AntiCheatEngine::new(),
            load_bal,
            burst_pred: BurstPredictor::new(),
            reputation: ReputationEngine::new(),
            policy,
            lifetimes: HashMap::with_capacity(1024),
            tui_state,
            tui_shutdown,
            last_anticheat_tick: Instant::now(),
            last_policy_tick: Instant::now(),
            last_reputation_flush: Instant::now(),
            stats_channel_failed: false,
            label_counts: [0; 5],
            total_inference_ns: 0,
            inference_samples: 0,
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

    // ── AI inference pipeline (ops.enqueue) ───────────────────────────────

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

        TaskFeatures {
            runnable_ratio,
            cpu_intensity,
            exec_ratio,
            weight_norm: (task.weight as f32 / 10000.0).clamp(0.0, 1.0),
            cpu_affinity: (task.nr_cpus_allowed as f32 / (nr_cpus as f32).max(1.0)).clamp(0.0, 1.0),
        }
    }

    fn ai_classify_and_enqueue(&mut self, task: &mut QueuedTask) -> (u64, u64, TaskLabel) {
        let t0 = Self::now_ns();

        let nr_cpus = (*self.bpf.nr_online_cpus_mut()).max(1) as i32;

        // Use the slice assigned to this PID in the previous cycle as the
        // denominator for cpu_intensity.  This gives the unambiguous
        // "slice-usage fraction" without any global-constant normalisation
        // artefacts.  On the very first event for a new PID, fall back to
        // base_slice_ns so the value is at least reasonable.
        let prev_slice_ns = self
            .lifetimes
            .get(&task.pid)
            .filter(|lt| lt.last_slice_ns > 0)
            .map(|lt| lt.last_slice_ns)
            .unwrap_or(self.base_slice_ns);

        // Build features.
        let features = Self::compute_features(task, self.base_slice_ns, prev_slice_ns, nr_cpus);

        // Classify using the deterministic heuristic only.
        // Stateless, O(1), no feedback loop — see src/ai/classifier.rs.
        let label = self.classifier.classify(&features);
        self.label_counts[label as usize] += 1;

        // Reputation-based slice factor.
        let rep_factor = self.reputation.slice_factor(task.pid);
        let quarantined = self.reputation.is_quarantined(task.pid);

        // Burst predictor — read prediction for this PID (updated on exit path).
        let predicted_burst = self.burst_pred.prediction_for(task.pid);

        // Q-learning-adjusted base slice.
        let ai_slice = self.policy.read_slice_ns();

        // Final time-slice:
        //   base = AI-adjusted slice × label_multiplier × (weight / 100)
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
        let clamp_max = (self.base_slice_ns * 8).max(self.slice_ns_min);
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
            // Sleeping tasks gain at most one base_slice of credit.
            let vruntime_min = self.vruntime_now.saturating_sub(self.base_slice_ns);
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

        (deadline, slice, label)
    }

    // ── Drain queued tasks (calls AI pipeline per task) ───────────────────

    fn drain_queued_tasks(&mut self) {
        loop {
            match self.bpf.dequeue_task() {
                Ok(Some(mut task)) => {
                    let (deadline, slice_ns, label) = self.ai_classify_and_enqueue(&mut task);
                    let timestamp = Self::now_ns();

                    // Track lifetime for reputation updates.
                    let e = self.lifetimes.entry(task.pid).or_default();
                    e.slice_assigned_ns = slice_ns;
                    // Store the assigned slice so the next scheduling event
                    // for this PID can compute cpu_intensity = burst / last_slice.
                    e.last_slice_ns = slice_ns;
                    e.slice_used_ns = task.stop_ts.saturating_sub(task.start_ts);
                    e.preempted = e.slice_used_ns >= slice_ns.saturating_sub(slice_ns / 8);
                    e.cheat_flagged = self.anti_cheat.is_flagged(task.pid);
                    e.last_seen_ns = Self::now_ns();

                    // Update burst predictor with observed burst.
                    let burst_ns = task.stop_ts.saturating_sub(task.start_ts);
                    let nr_cpus_exit = (*self.bpf.nr_online_cpus_mut()).max(1) as i32;
                    // Use the newly-assigned slice_ns as prev_slice for the
                    // burst predictor features (acceptable approximation here
                    // since the predictor uses exec_ratio, not cpu_intensity).
                    let features =
                        Self::compute_features(&task, self.base_slice_ns, slice_ns, nr_cpus_exit);
                    self.burst_pred.observe_and_predict(
                        task.pid,
                        burst_ns,
                        features.exec_ratio,
                        features.cpu_intensity,
                    );

                    self.tasks.insert(Task {
                        deadline,
                        timestamp,
                        label,
                        slice_ns,
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
        let Some(task) = self.tasks.pop_first() else {
            return true;
        };

        let quarantined = self.reputation.is_quarantined(task.qtask.pid)
            || self.anti_cheat.is_flagged(task.qtask.pid);

        let mut dispatched = DispatchedTask::new(&task.qtask);
        dispatched.slice_ns = task.slice_ns;
        dispatched.vtime = task.deadline;

        // CPU selection: A* or percpu_local shortcut.
        dispatched.cpu = if self.opts.percpu_local {
            task.qtask.cpu
        } else {
            let cpu = self
                .load_bal
                .select_cpu(task.qtask.cpu, task.label, quarantined);
            if cpu >= 0 {
                cpu
            } else {
                RL_CPU_ANY
            }
        };

        if self.bpf.dispatch_task(&dispatched).is_err() {
            self.tasks.insert(task);
            return false;
        }
        true
    }

    // ── Periodic AI housekeeping ───────────────────────────────────────────

    /// Anti-cheat tick (every 100 ms).
    fn tick_anti_cheat(&mut self) {
        if self.last_anticheat_tick.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.last_anticheat_tick = Instant::now();
        let now = Self::now_ns();
        let newly_flagged = self.anti_cheat.tick(now);
        for tgid in newly_flagged {
            warn!("Anti-cheat: flagged TGID {tgid} as anomalous");
        }
    }

    /// Q-learning policy update (every 250 ms).
    fn tick_policy(&mut self) {
        if self.last_policy_tick.elapsed() < Duration::from_millis(250) {
            return;
        }
        self.last_policy_tick = Instant::now();

        let nr_cpus = (*self.bpf.nr_online_cpus_mut()).max(1) as f64;
        let nr_running = *self.bpf.nr_running_mut() as f64;
        let total_labeled = self.label_counts.iter().sum::<u64>().max(1) as f64;
        let interactive_frac =
            self.label_counts[TaskLabel::Interactive as usize] as f64 / total_labeled;
        let compute_frac = self.label_counts[TaskLabel::Compute as usize] as f64 / total_labeled;

        let avg_inference_us = if self.inference_samples > 0 {
            self.total_inference_ns as f64 / self.inference_samples as f64 / 1000.0
        } else {
            0.0
        };

        let sig = SchedulerSignal {
            load_norm: (nr_running / nr_cpus).min(1.0),
            interactive_frac,
            compute_frac,
            // Normalise by 10 ms.
            latency_p99_norm: (avg_inference_us / 10_000.0).min(1.0),
            congestion_rate: *self.bpf.nr_sched_congested_mut() as f64,
        };
        self.policy.update(&sig);
    }

    /// Emit reputation updates for finished tasks (ops.exit approximation).
    ///
    /// Rate-limited to once per second. A task is considered departed only when
    /// its `last_seen_ns` timestamp is more than 2 seconds old — this prevents
    /// the previous bug where every still-active task was evicted on every loop,
    /// wiping the KNN pid_cache constantly and corrupting reputation scores.
    fn flush_reputation_updates(&mut self) {
        // Run at most once per second.
        if self.last_reputation_flush.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_reputation_flush = Instant::now();

        // Tasks not seen for >2 seconds are assumed to have exited.
        let stale_threshold_ns = 2 * NSEC_PER_SEC;
        let now = Self::now_ns();

        // Hard cap: if the lifetimes map grows beyond 8192 entries (e.g., from
        // a heavy benchmark with many short-lived browser sub-processes), evict
        // stale entries.  Use HashMap::retain() in cascading time-window passes
        // (each O(N)) rather than sorting the full map (O(N log N)).  We stop
        // as soon as the map fits within the cap.
        const LIFETIMES_MAX: usize = 8192;
        if self.lifetimes.len() > LIFETIMES_MAX {
            // Pass 1: keep entries seen within the last 10 s.
            let cutoff = now.saturating_sub(10 * NSEC_PER_SEC);
            self.lifetimes.retain(|_, lt| lt.last_seen_ns >= cutoff);
            // Pass 2: tighten to 5 s if still over cap.
            if self.lifetimes.len() > LIFETIMES_MAX {
                let cutoff = now.saturating_sub(5 * NSEC_PER_SEC);
                self.lifetimes.retain(|_, lt| lt.last_seen_ns >= cutoff);
            }
            // Pass 3: final tighten to 2 s (aligns with the stale-eviction window).
            if self.lifetimes.len() > LIFETIMES_MAX {
                let cutoff = now.saturating_sub(2 * NSEC_PER_SEC);
                self.lifetimes.retain(|_, lt| lt.last_seen_ns >= cutoff);
            }
        }

        let stale: Vec<i32> = self
            .lifetimes
            .iter()
            .filter(|(_, lt)| {
                lt.last_seen_ns > 0 && now.saturating_sub(lt.last_seen_ns) >= stale_threshold_ns
            })
            .map(|(&pid, _)| pid)
            .collect();

        for pid in stale {
            if let Some(lt) = self.lifetimes.remove(&pid) {
                let obs = ExitObservation {
                    slice_underrun: lt.slice_used_ns < lt.slice_assigned_ns / 2,
                    preempted: lt.preempted,
                    clean_exit: !lt.cheat_flagged,
                    cheat_flagged: lt.cheat_flagged,
                    fork_count: 0,
                    involuntary_ctx_sw: 0,
                };
                self.reputation.update_on_exit(pid, pid, &obs, "");
                self.burst_pred.evict(pid);
                self.anti_cheat.evict(pid);
            }
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

        let quarantined_count = self
            .reputation
            .all_scores()
            .filter(|(_, score, _)| *score < ai::TRUST_THRESHOLD)
            .count() as u64;

        Metrics {
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
            nr_flagged: self.anti_cheat.wall_of_shame().len() as u64,
            ai_slice_us: self.policy.read_slice_ns() / NSEC_PER_USEC,
            ai_inference_us: avg_inference_us as u64,
            reward_ema_x100: (self.policy.reward_ema * 100.0) as i64,
        }
    }

    // ── Main scheduling loop ───────────────────────────────────────────────

    fn schedule(&mut self) {
        // 1. Drain queued tasks (AI classify + enqueue).
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
        for _ in 0..nr_cpus {
            if self.tasks.is_empty() {
                break;
            }
            if !self.dispatch_task() {
                break;
            }
        }

        // 3. Notify BPF dispatcher of remaining pending work.
        self.bpf.notify_complete(self.tasks.len() as u64);
    }

    // ── Background AI housekeeping ─────────────────────────────────────────────
    //
    // Kept OFF the schedule() critical path.  schedule() is called in a
    // tight BPF dispatch loop; any stall there risks the sched_ext watchdog
    // (which fires if ops.dispatch is not called for several seconds).
    //
    // All three inner functions carry their own rate-limit timers, so
    // calling housekeeping() every ~50 ms from run() is safe: each will
    // no-op immediately if its own timer has not elapsed.
    fn housekeeping(&mut self) {
        self.tick_anti_cheat();
        self.tick_policy();
        self.flush_reputation_updates();
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

        let shame: Vec<tui::WallEntry> = {
            let anticheat_flagged: HashSet<i32> =
                self.anti_cheat.wall_of_shame().keys().cloned().collect();
            self.reputation
                .wall_of_shame(20)
                .iter()
                .map(|(pid, trust, comm)| tui::WallEntry {
                    pid: *pid,
                    comm: comm.to_string(),
                    trust: *trust,
                    is_flagged: anticheat_flagged.contains(pid),
                })
                .collect()
        };

        if let Ok(mut s) = state.lock() {
            s.metrics = metrics.clone();
            s.inference_us = avg_us;
            s.wall_of_shame = shame;
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

            // Background AI housekeeping (anti-cheat, policy, reputation).
            // Runs outside schedule() so the BPF dispatch path is never
            // delayed by periodic work.  50 ms outer gate plus each
            // function's inner timer ensures at most one unit of work
            // executes between two consecutive schedule() calls.
            if last_housekeeping.elapsed() >= Duration::from_millis(50) {
                last_housekeeping = Instant::now();
                self.housekeeping();
            }

            // TUI requested shutdown.
            if self.tui_shutdown.load(Ordering::Relaxed) {
                break;
            }
        }

        self.bpf.shutdown_and_report()
    }
}

impl Drop for Scheduler<'_> {
    fn drop(&mut self) {
        self.tui_shutdown.store(true, Ordering::Relaxed);
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
