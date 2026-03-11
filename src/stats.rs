// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// Metrics and statistics for scx_cognis.

use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use scx_stats::prelude::*;
use scx_stats_derive::stat_doc;
use scx_stats_derive::Stats;
use serde::Deserialize;
use serde::Serialize;

/// Top-level metrics snapshot exported via scx_stats.
#[stat_doc]
#[derive(Clone, Debug, Default, Serialize, Deserialize, Stats)]
#[stat(top)]
pub struct Metrics {
    #[stat(desc = "Scheduler version (matches the release tag)")]
    pub version: String,
    #[stat(desc = "Elapsed uptime (e.g. 1y2m3d 12h:30m:14s)")]
    pub elapsed: String,
    #[stat(desc = "Number of online CPUs")]
    pub nr_cpus: u64,
    #[stat(desc = "Tasks currently running")]
    pub nr_running: u64,
    #[stat(desc = "Tasks queued to user-space scheduler")]
    pub nr_queued: u64,
    #[stat(desc = "Tasks waiting in user-space to be dispatched")]
    pub nr_scheduled: u64,
    #[stat(desc = "Major page faults in the scheduler process (non-zero = swap pressure)")]
    pub nr_page_faults: u64,
    #[stat(desc = "Tasks dispatched by user-space scheduler")]
    pub nr_user_dispatches: u64,
    #[stat(desc = "Tasks dispatched directly by the kernel")]
    pub nr_kernel_dispatches: u64,
    #[stat(desc = "Cancelled dispatches")]
    pub nr_cancel_dispatches: u64,
    #[stat(desc = "Dispatches bounced to another DSQ")]
    pub nr_bounce_dispatches: u64,
    #[stat(desc = "Failed dispatches")]
    pub nr_failed_dispatches: u64,
    #[stat(desc = "Scheduler congestion events")]
    pub nr_sched_congested: u64,
    #[stat(desc = "BPF-side per-pid EWMA updates (PoC)")]
    pub nr_bpf_ewma_updates: u64,
    #[stat(desc = "BPF-side kernel boost applications (PoC)")]
    pub nr_kernel_boosts: u64,

    // ── Scheduling policy metrics ──────────────────────────────────────────
    #[stat(desc = "Tasks classified as Interactive")]
    pub nr_interactive: u64,
    #[stat(desc = "Tasks classified as Compute")]
    pub nr_compute: u64,
    #[stat(desc = "Tasks classified as I/O Wait")]
    pub nr_iowait: u64,
    #[stat(desc = "Tasks classified as RealTime")]
    pub nr_realtime: u64,
    #[stat(
        desc = "Tasks classified as Unknown (reserved bucket; normally zero with the current heuristic)"
    )]
    pub nr_unknown: u64,
    #[stat(desc = "PIDs currently quarantined by the trust engine")]
    pub nr_quarantined: u64,
    #[stat(desc = "PIDs flagged by the anomaly detection system")]
    pub nr_flagged: u64,
    #[stat(desc = "Current global slice-base from the load-driven controller (µs)")]
    pub base_slice_us: u64,
    #[stat(desc = "Recent EMA of final per-task assigned slices after adjustments (µs)")]
    pub assigned_slice_us: u64,
    #[stat(desc = "Autopilot adaptive minimum cap (µs)")]
    pub autopilot_min_us: u64,
    #[stat(desc = "Autopilot adaptive maximum cap (µs)")]
    pub autopilot_max_us: u64,
    #[stat(desc = "Average per-event scheduling pipeline latency (µs)")]
    pub inference_us: u64,
    #[stat(desc = "Scheduling pipeline latency p50 (µs)")]
    pub sched_p50_us: u64,
    #[stat(desc = "Scheduling pipeline latency p95 (µs)")]
    pub sched_p95_us: u64,
    #[stat(desc = "Scheduling pipeline latency p99 (µs)")]
    pub sched_p99_us: u64,
}

impl Metrics {
    /// Derive a human-readable one-liner summarising current system health.
    /// Scenarios are checked highest-severity first so the most pressing issue
    /// always surfaces in the output.
    pub fn tldr(&self) -> &'static str {
        let cpus = self.nr_cpus.max(1);
        let load = self.nr_running as f64 / cpus as f64;
        let classified = self.nr_interactive + self.nr_compute + self.nr_iowait + self.nr_realtime;
        let compute_heavy = classified > 0 && self.nr_compute > classified / 2;
        let interactive_heavy = classified > 0 && self.nr_interactive > classified / 2;

        // ── Worst ──────────────────────────────────────────────────────────
        // Scheduler itself is being paged out — immediate latency spike.
        if self.nr_page_faults > 0 {
            return "I'm being swapped out! Latency will spike — check available RAM!";
        }
        // Any dispatch failure means a kernel/BPF-level error.
        if self.nr_failed_dispatches > 0 {
            return "Dispatch failures detected! Something unexpected went wrong — check dmesg.";
        }
        // Many rule-breakers caught simultaneously.
        if self.nr_flagged > 5 && self.nr_quarantined > 5 {
            return "Under siege! Multiple rule-breakers caught and caged — enforcing order.";
        }
        // Anti-cheat engine fired.
        if self.nr_flagged > 0 {
            return "Suspicious behaviour detected! Isolating troublemakers — your system is protected.";
        }
        // Multiple greedy tasks throttled by reputation engine.
        if self.nr_quarantined > 3 {
            return "Several greedy tasks are throttled — keeping them from hogging your CPU.";
        }
        // At least one greedy task quarantined.
        if self.nr_quarantined > 0 {
            return "Caught a greedy task! Putting it on a leash so other tasks can breathe.";
        }
        // Heavy congestion in the scheduler queue.
        if self.nr_sched_congested > 10 {
            return "Oh boy! Things are getting really busy. Tightening the reins...";
        }
        // Mild congestion.
        if self.nr_sched_congested > 0 {
            return "Getting a little crowded in here, but I've got it handled.";
        }
        // ── Middle ground ──────────────────────────────────────────────────
        // High load, compute-dominated.
        if load >= 0.85 && compute_heavy {
            return "Your CPU is at full throttle! Giving compute tasks the runway they need.";
        }
        // High load, interactive-dominated.
        if load >= 0.85 && interactive_heavy {
            return "Busy but responsive! Juggling lots of interactive tasks like a pro.";
        }
        // High overall load, mixed workload.
        if load >= 0.85 {
            return "Running hot! Balancing a heavy mixed workload across all cores.";
        }
        // Moderate-high load.
        if load >= 0.65 {
            return "A solid workload — distributing tasks evenly and keeping things smooth.";
        }

        // ── Best ───────────────────────────────────────────────────────────
        // Mostly interactive, low-to-moderate load.
        if interactive_heavy && load < 0.5 {
            return "Rest assured! I'm keeping your system responsive.";
        }
        // Compute in progress but not overloaded.
        if compute_heavy && load < 0.65 {
            return "Compute tasks are in full swing — prioritising steady progress while preserving responsiveness where possible.";
        }
        // Balanced mix under control.
        if load < 0.5 {
            return "Balancing work steadily — nothing to worry about.";
        }
        // System mostly idle (cold start or light desktop).
        if load < 0.1 {
            return "System is mostly idle. Just here waiting to help!";
        }
        // Default: nominal operation.
        "Keeping an eye on things — all nominal."
    }

    pub fn format<W: Write>(&self, w: &mut W) -> Result<()> {
        writeln!(
            w,
            "[cognis v{}] elapsed: {:<22} tldr: {:<55} | r:{:>3}/{:<3} q:{:<3}/{:<3} | pf:{:<4} | d→u:{:<6} k:{:<4} c:{:<4} b:{:<4} f:{:<4} ewma:{:<6} kb:{:<4} sched:{:<5}/{:<5}/{:<5} | cong:{:<4} | \
             🧠 Interactive:{:<4} Compute:{:<4} IOwait:{:<4} RT:{:<4} Unknown:{:<4} | quarantine:{} flagged:{} | slice(base/assigned):{}/{}µs",
            self.version,
            self.elapsed,
            self.tldr(),
            self.nr_running,
            self.nr_cpus,
            self.nr_queued,
            self.nr_scheduled,
            self.nr_page_faults,
            self.nr_user_dispatches,
            self.nr_kernel_dispatches,
            self.nr_cancel_dispatches,
            self.nr_bounce_dispatches,
            self.nr_failed_dispatches,
            self.nr_bpf_ewma_updates,
            self.nr_kernel_boosts,
            self.sched_p50_us,
            self.sched_p95_us,
            self.sched_p99_us,
            self.nr_sched_congested,
            self.nr_interactive,
            self.nr_compute,
            self.nr_iowait,
            self.nr_realtime,
            self.nr_unknown,
            self.nr_quarantined,
            self.nr_flagged,
            self.base_slice_us,
            self.assigned_slice_us,
        )?;
        Ok(())
    }

    pub fn delta(&self, rhs: &Self) -> Self {
        Self {
            // Dispatch counters — per-interval deltas.
            nr_user_dispatches: self
                .nr_user_dispatches
                .saturating_sub(rhs.nr_user_dispatches),
            nr_kernel_dispatches: self
                .nr_kernel_dispatches
                .saturating_sub(rhs.nr_kernel_dispatches),
            nr_cancel_dispatches: self
                .nr_cancel_dispatches
                .saturating_sub(rhs.nr_cancel_dispatches),
            nr_bounce_dispatches: self
                .nr_bounce_dispatches
                .saturating_sub(rhs.nr_bounce_dispatches),
            nr_failed_dispatches: self
                .nr_failed_dispatches
                .saturating_sub(rhs.nr_failed_dispatches),
            nr_sched_congested: self
                .nr_sched_congested
                .saturating_sub(rhs.nr_sched_congested),
            nr_bpf_ewma_updates: self
                .nr_bpf_ewma_updates
                .saturating_sub(rhs.nr_bpf_ewma_updates),
            nr_kernel_boosts: self.nr_kernel_boosts.saturating_sub(rhs.nr_kernel_boosts),
            // Major page faults — per-interval delta so --monitor shows faults/sec.
            // (nr_page_faults is already baseline-subtracted in get_metrics(), but
            // delta() must subtract again so each --monitor line shows only the faults
            // that occurred during *that* interval, not the lifetime total.)
            nr_page_faults: self.nr_page_faults.saturating_sub(rhs.nr_page_faults),
            // Classification counters — per-interval deltas so --monitor shows
            // events-per-interval instead of ever-growing cumulative totals.
            nr_interactive: self.nr_interactive.saturating_sub(rhs.nr_interactive),
            nr_compute: self.nr_compute.saturating_sub(rhs.nr_compute),
            nr_iowait: self.nr_iowait.saturating_sub(rhs.nr_iowait),
            nr_realtime: self.nr_realtime.saturating_sub(rhs.nr_realtime),
            nr_unknown: self.nr_unknown.saturating_sub(rhs.nr_unknown),
            ..self.clone()
        }
    }
}

pub fn server_data() -> StatsServerData<(), Metrics> {
    let open: Box<dyn StatsOpener<(), Metrics>> = Box::new(move |(req_ch, res_ch)| {
        req_ch.send(())?;
        let mut prev = res_ch.recv()?;

        let read: Box<dyn StatsReader<(), Metrics>> = Box::new(move |_args, (req_ch, res_ch)| {
            req_ch.send(())?;
            let cur = res_ch.recv()?;
            let delta = cur.delta(&prev);
            prev = cur;
            delta.to_json()
        });

        Ok(read)
    });

    StatsServerData::new()
        .add_meta(Metrics::meta())
        .add_ops("top", StatsOps { open, close: None })
}

pub fn monitor(intv: Duration) -> Result<()> {
    scx_utils::monitor_stats::<Metrics>(
        &[],
        intv,
        || false,
        |metrics| metrics.format(&mut std::io::stdout()),
    )
}
