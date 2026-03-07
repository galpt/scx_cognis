// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// Deterministic slice controller.
//
// Cognis previously used a tabular Q-learning loop to adjust one global slice
// value. That added smoothing, exploration, and delayed feedback on a path
// where desktop frame pacing needs direct, predictable behaviour. The current
// controller is intentionally simpler: derive the slice from current runnable
// load, clamp it to a tight desktop-oriented window, and publish that value
// immediately.

/// Targeted scheduling latency: the time window in which all runnable tasks
/// should be served at least once under nominal desktop load.
const TARGETED_LATENCY_NS: u64 = 6_000_000; // 6 ms

/// Absolute minimum slice regardless of load.
const AUTO_SLICE_MIN_NS: u64 = 250_000; // 250 us

/// Absolute maximum slice even when the machine is mostly idle.
const AUTO_SLICE_MAX_NS: u64 = 8_000_000; // 8 ms

/// Load-driven deterministic slice controller.
pub struct SliceController {
    /// Current slice recommendation (nanoseconds).
    current_slice_ns: u64,
    /// User-configured base slice ceiling (0 = auto mode only).
    base_slice_ns: u64,
    /// Auto-computed slice ceiling derived from runnable load.
    pub auto_base_ns: u64,
}

impl SliceController {
    pub fn new(base_slice_ns: u64) -> Self {
        let initial_auto = if base_slice_ns > 0 {
            base_slice_ns
        } else {
            TARGETED_LATENCY_NS
        }
        .clamp(AUTO_SLICE_MIN_NS, AUTO_SLICE_MAX_NS);

        Self {
            current_slice_ns: initial_auto,
            base_slice_ns,
            auto_base_ns: initial_auto,
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
        self.current_slice_ns = self.effective_base_ns();
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
