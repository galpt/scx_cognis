// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// Load-driven slice-base controller.
//
// This module computes the global slice ceiling from current runnable load.
// Task-specific adjustments, including bounded interactive renewal, are
// applied later in `main.rs` so this controller can stay simple and O(1).

/// Targeted scheduling latency: the time window in which all runnable tasks
/// should be served at least once under nominal desktop load.
const TARGETED_LATENCY_NS: u64 = 6_000_000; // 6 ms

/// Absolute minimum slice regardless of load.
const AUTO_SLICE_MIN_NS: u64 = 250_000; // 250 us

/// Absolute maximum slice even when the machine is mostly idle.
const AUTO_SLICE_MAX_NS: u64 = 8_000_000; // 8 ms

// Latency ring buffer size for scheduling pipeline samples. Must be power-of-two for mask.
const LAT_RING_CAP: usize = 2048;
const LAT_RING_MASK: usize = LAT_RING_CAP - 1;

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Load-driven slice-base controller.
pub struct SliceController {
    /// Current slice recommendation (nanoseconds).
    current_slice_ns: u64,
    /// User-configured base slice ceiling (0 = auto mode only).
    base_slice_ns: u64,
    /// Auto-computed slice ceiling derived from runnable load.
    pub auto_base_ns: u64,
    /// Current adaptive minimum and maximum caps (ns). These may be adjusted
    /// by the autopilot logic at runtime. Stored as plain u64 because the
    /// controller is owned by the scheduler main thread.
    min_ns: u64,
    max_ns: u64,
    /// Lock-free ring buffer of recent scheduling pipeline latencies (ns).
    lat_ring: Box<[AtomicU64]>,
    lat_idx: AtomicUsize,
    /// Last rollback threshold for p99 used by autopilot.
    last_p99_threshold: AtomicU64,
}

impl SliceController {
    pub fn new(base_slice_ns: u64) -> Self {
        let initial_auto = if base_slice_ns > 0 {
            base_slice_ns
        } else {
            TARGETED_LATENCY_NS
        }
        .clamp(AUTO_SLICE_MIN_NS, AUTO_SLICE_MAX_NS);
        // Initialize the lock-free latency ring buffer with zeros.
        let mut v = Vec::with_capacity(LAT_RING_CAP);
        for _ in 0..LAT_RING_CAP {
            v.push(AtomicU64::new(0));
        }

        Self {
            current_slice_ns: initial_auto,
            base_slice_ns,
            auto_base_ns: initial_auto,
            min_ns: AUTO_SLICE_MIN_NS,
            max_ns: AUTO_SLICE_MAX_NS,
            lat_ring: v.into_boxed_slice(),
            lat_idx: AtomicUsize::new(0),
            last_p99_threshold: AtomicU64::new(u64::MAX),
        }
    }

    /// Recompute the current slice from the runnable load.
    pub fn update(&mut self, nr_runnable: u64, nr_cpus: u64) -> u64 {
        if nr_cpus == 0 {
            return self.current_slice_ns;
        }

        let tasks_per_cpu = (nr_runnable as f64 / nr_cpus as f64).max(1.0);
        let computed = (TARGETED_LATENCY_NS as f64 / tasks_per_cpu) as u64;
        self.auto_base_ns = computed.clamp(AUTO_SLICE_MIN_NS, AUTO_SLICE_MAX_NS);
        // Recompute current base and respect runtime min/max bounds.
        let base = self.effective_base_ns();
        self.current_slice_ns = base.clamp(self.min_ns, self.max_ns);
        self.current_slice_ns
    }

    fn effective_base_ns(&self) -> u64 {
        if self.base_slice_ns > 0 {
            self.auto_base_ns.min(self.base_slice_ns)
        } else {
            self.auto_base_ns
        }
    }

    pub fn read_slice_ns(&self) -> u64 {
        self.current_slice_ns
    }

    /// Read adaptive min cap (ns).
    pub fn read_min(&self) -> u64 {
        self.min_ns
    }

    /// Read adaptive max cap (ns).
    pub fn read_max(&self) -> u64 {
        self.max_ns
    }

    /// Write adaptive min cap (ns). Caller must ensure safety bounds.
    pub fn write_min(&mut self, v: u64) {
        self.min_ns = v.max(1); // never zero
                                // keep current slice within bounds
        self.current_slice_ns = self.current_slice_ns.clamp(self.min_ns, self.max_ns);
    }

    /// Write adaptive max cap (ns). Caller must ensure safety bounds.
    pub fn write_max(&mut self, v: u64) {
        self.max_ns = v.max(1);
        self.current_slice_ns = self.current_slice_ns.clamp(self.min_ns, self.max_ns);
    }

    /// Record a scheduling pipeline latency sample (ns). This is designed to be
    /// cheap in the hot path: it simply stores the sample into a preallocated
    /// atomic slot using a wrapping index.
    pub fn record_sched_event_latency(&self, ns: u64) {
        let i = self.lat_idx.fetch_add(1, Ordering::Relaxed) & LAT_RING_MASK;
        self.lat_ring[i].store(ns, Ordering::Relaxed);
    }

    /// Compute simple percentiles (p50, p95, p99) from the ring buffer.
    /// This is intended to be called from background/monitor paths, not hot.
    pub fn compute_sched_percentiles(&self) -> (u64, u64, u64) {
        // Collect non-zero samples.
        let mut samples = Vec::with_capacity(LAT_RING_CAP);
        for s in self.lat_ring.iter() {
            let v = s.load(Ordering::Relaxed);
            if v > 0 {
                samples.push(v);
            }
        }
        if samples.is_empty() {
            return (0, 0, 0);
        }
        samples.sort_unstable();
        let n = samples.len();
        let p50 = samples[n / 2];
        let p95 = samples[(n * 95) / 100];
        let p99 = samples[(n * 99) / 100];
        (p50, p95, p99)
    }

    pub fn read_auto_base_ns(&self) -> u64 {
        self.auto_base_ns
    }

    pub fn read_last_p99_threshold(&self) -> u64 {
        self.last_p99_threshold.load(Ordering::Relaxed)
    }

    pub fn update_last_p99_threshold(&self, v: u64) {
        self.last_p99_threshold.store(v, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_stays_in_bounds() {
        let mut ctrl = SliceController::new(0);
        let slice = ctrl.update(80, 8);
        assert!(slice >= AUTO_SLICE_MIN_NS);
        assert!(slice <= AUTO_SLICE_MAX_NS);
    }

    #[test]
    fn load_increase_shrinks_slice_immediately() {
        let mut ctrl = SliceController::new(0);
        let light = ctrl.update(8, 8);
        let heavy = ctrl.update(64, 8);
        assert!(
            heavy < light,
            "heavy load slice {heavy} should be below light load slice {light}"
        );
    }

    #[test]
    fn manual_ceiling_is_respected() {
        let mut ctrl = SliceController::new(2_000_000);
        let slice = ctrl.update(1, 8);
        assert!(slice <= 2_000_000);
    }
}
