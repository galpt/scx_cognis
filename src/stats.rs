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
    #[stat(desc = "Number of online CPUs")]
    pub nr_cpus: u64,
    #[stat(desc = "Tasks currently running")]
    pub nr_running: u64,
    #[stat(desc = "Tasks queued to user-space scheduler")]
    pub nr_queued: u64,
    #[stat(desc = "Tasks waiting in user-space to be dispatched")]
    pub nr_scheduled: u64,
    #[stat(desc = "User-space scheduler page faults (should be 0)")]
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

    // ── AI-specific metrics ───────────────────────────────────────────────
    #[stat(desc = "Tasks classified as Interactive")]
    pub nr_interactive: u64,
    #[stat(desc = "Tasks classified as Compute")]
    pub nr_compute: u64,
    #[stat(desc = "Tasks classified as I/O Wait")]
    pub nr_iowait: u64,
    #[stat(desc = "Tasks classified as RealTime")]
    pub nr_realtime: u64,
    #[stat(desc = "PIDs currently quarantined by reputation engine")]
    pub nr_quarantined: u64,
    #[stat(desc = "TGIDs flagged by anti-cheat isolation forest")]
    pub nr_flagged: u64,
    #[stat(desc = "Current AI-adjusted time slice (µs)")]
    pub ai_slice_us: u64,
    #[stat(desc = "Average AI inference latency per task (µs)")]
    pub ai_inference_us: u64,
    #[stat(desc = "Policy controller reward EMA (×100 for integer display)")]
    pub reward_ema_x100: i64,
}

impl Metrics {
    pub fn format<W: Write>(&self, w: &mut W) -> Result<()> {
        writeln!(
            w,
            "[cognis] r:{:>3}/{:<3} q:{:<3}/{:<3} | pf:{:<4} | d→u:{:<6} k:{:<4} c:{:<4} b:{:<4} f:{:<4} | cong:{:<4} | \
             🧠 Interactive:{:<4} Compute:{:<4} IOwait:{:<4} RT:{:<4} | quarantine:{} flagged:{} | slice:{}µs reward:{:.2}",
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
            self.nr_sched_congested,
            self.nr_interactive,
            self.nr_compute,
            self.nr_iowait,
            self.nr_realtime,
            self.nr_quarantined,
            self.nr_flagged,
            self.ai_slice_us,
            self.reward_ema_x100 as f64 / 100.0,
        )?;
        Ok(())
    }

    pub fn delta(&self, rhs: &Self) -> Self {
        Self {
            nr_user_dispatches: self.nr_user_dispatches - rhs.nr_user_dispatches,
            nr_kernel_dispatches: self.nr_kernel_dispatches - rhs.nr_kernel_dispatches,
            nr_cancel_dispatches: self.nr_cancel_dispatches - rhs.nr_cancel_dispatches,
            nr_bounce_dispatches: self.nr_bounce_dispatches - rhs.nr_bounce_dispatches,
            nr_failed_dispatches: self.nr_failed_dispatches - rhs.nr_failed_dispatches,
            nr_sched_congested: self.nr_sched_congested - rhs.nr_sched_congested,
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
