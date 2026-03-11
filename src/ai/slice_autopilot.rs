// Conservative, always-on slice autopilot skeleton for Cognis.
// - Runs in userspace as a background thread.
// - Reads available signals (assigned_slice_ema_ns, nr_running, nr_queued, burst predictor outputs).
// - Produces safe, smoothed adjustments to slice_min/slice_max via SliceController API.
// - Enforces hard bounds, cooldown, smoothing, and rollback triggers.

use std::time::{Duration, Instant};

use crate::ai::SliceController;

/// Conservative, always-on autopilot helper. This module exposes a small
/// budget-aware proposer that the single-threaded scheduler calls periodically.
/// The proposer is deliberately simple: it smooths targets, enforces hard
/// floors/caps and returns bounded step adjustments for `min` and `max` caps.
pub struct Autopilot {
    conservative: bool,
    smoothed_min: u64,
    smoothed_max: u64,
    last_apply: Instant,
    last_good_min: u64,
    last_good_max: u64,
}

impl Autopilot {
    pub fn new(current_min: u64, current_max: u64) -> Self {
        Autopilot {
            conservative: true,
            smoothed_min: current_min,
            smoothed_max: current_max,
            last_apply: Instant::now() - Duration::from_secs(60),
            last_good_min: current_min,
            last_good_max: current_max,
        }
    }

    /// Called periodically by the scheduler. Returns (min_ns, max_ns) if the
    /// autopilot wants the scheduler to apply new caps, otherwise None.
    pub fn propose(
        &mut self,
        sc: &SliceController,
        assigned_ema: u64,
        base: u64,
        nr_running: u64,
        nr_queued: u64,
    ) -> Option<(u64, u64)> {
        const MIN_CHANGE_INTERVAL: Duration = Duration::from_secs(5);
        const SMOOTH_ALPHA: f64 = 0.25; // smoothing for target adjustments
        const HARD_MIN_NS: u64 = 10_000; // 10 us absolute minimum floor
        const HARD_MAX_NS: u64 = 50_000_000; // 50 ms absolute max ceiling

        // Basic heuristic: when assigned EMA is small and load low, allow a
        // much smaller min. When load is high or assigned slices are large,
        // increase max to avoid harming throughput. Also bias decisions by
        // current runnable/queued pressure so the autopilot behaves sensibly
        // under sustained high load.
        let mut target_min = (assigned_ema / 4).max(HARD_MIN_NS);
        let mut target_max = (base.saturating_mul(4)).min(HARD_MAX_NS);

        // Use observed pressure to nudge max upward when backlog grows.
        let pressure = nr_running.saturating_add(nr_queued);
        if pressure > 64 {
            target_max = (target_max.saturating_mul(125) / 100).min(HARD_MAX_NS); // +25%
        } else if pressure > 16 {
            target_max = (target_max.saturating_mul(110) / 100).min(HARD_MAX_NS); // +10%
        }

        // Respect absolute conservative bounds.
        target_min = target_min.max(HARD_MIN_NS);
        target_max = target_max.min(HARD_MAX_NS);

        // Enforce overhead guard: do not set min below 4x measured scheduler overhead
        let (p50, p95, p99) = sc.compute_sched_percentiles();
        let sched_overhead = if p50 > 0 { p50 } else { 0 };
        if sched_overhead > 0 {
            let overhead_floor = sched_overhead.saturating_mul(4);
            target_min = target_min.max(overhead_floor);
        }

        // Also ensure min isn't below a mid-tail (p95) heuristic to avoid
        // pushing too small when the higher-percentile shows instability.
        if p95 > 0 {
            target_min = target_min.max(p95 / 2);
        }

        // Smooth changes
        self.smoothed_min = ((1.0 - SMOOTH_ALPHA) * (self.smoothed_min as f64)
            + SMOOTH_ALPHA * (target_min as f64)) as u64;
        self.smoothed_max = ((1.0 - SMOOTH_ALPHA) * (self.smoothed_max as f64)
            + SMOOTH_ALPHA * (target_max as f64)) as u64;

        // Rate limit actual writes
        if self.last_apply.elapsed() < MIN_CHANGE_INTERVAL {
            return None;
        }

        // Apply bounded step change. Use a smaller step when conservative.
        let max_frac = if self.conservative { 0.10 } else { 0.15 };
        let step_min = clamp_step(self.smoothed_min, sc.read_min(), max_frac);
        let step_max = clamp_step(self.smoothed_max, sc.read_max(), max_frac);

        // Rollback safety: if p99 exceeded threshold, revert to last good.
        let last_thr = sc.read_last_p99_threshold();
        if p99 > last_thr {
            // Revert immediately.
            self.last_apply = Instant::now();
            return Some((self.last_good_min, self.last_good_max));
        }

        // Accept new values.
        self.last_good_min = step_min;
        self.last_good_max = step_max;
        self.last_apply = Instant::now();
        sc.update_last_p99_threshold(p99.saturating_mul(2));

        Some((step_min, step_max))
    }
}

fn clamp_step(target: u64, current: u64, max_frac: f64) -> u64 {
    if target == current {
        return current;
    }
    let max_delta = ((current as f64) * max_frac) as i64;
    if max_delta <= 0 {
        return target;
    }
    let t = target as i64;
    let c = current as i64;
    if t > c + max_delta {
        (c + max_delta) as u64
    } else if t < c - max_delta {
        (c - max_delta) as u64
    } else {
        target
    }
}
