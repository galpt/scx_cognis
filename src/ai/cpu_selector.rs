// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// CPU Selector — O(1) bitmask-based task placement
//
// Replaces the A*-inspired BinaryHeap load balancer with strictly O(1) bitmask
// operations on three 64-bit CPU availability masks:
//
//   p_mask:          bit i set = CPU i is a Performance (big/P) core.
//   restricted_mask: bit i set = CPU i is reserved for quarantined tasks only.
//   all_mask:        bit i set = CPU i exists and is online.
//
// CPU topology (P/E type, restricted set) is read from sysfs once at startup
// and stored in the bitmasks.  All subsequent placement calls (once per dispatch)
// cost 3–6 bit operations (AND + TZCNT) = absolute O(1), ~1 ns.
//
// Old A* cost: O(n_cpu log n_cpu) BinaryHeap on every task dispatch.
//   16 cores × log(16) = 64 comparisons + heap push/pop per dispatch.
//   32 dispatches per schedule() call = 2048 operations per cycle.
// New cost: 3 AND + 1 TZCNT = 4 operations per dispatch, regardless of nr_cpus.
//
// For systems with > 64 CPUs: extend to [u64; N] masks and loop over 64-bit
// words — still O(n/64) which is amortised O(1) for the common ≤ 64 CPU case.
//
// Runs inside `ops.dispatch` — must return within nanoseconds.

#![allow(dead_code)]

use crate::ai::classifier::TaskLabel;

/// Maximum number of CPUs supported by a single u64 mask.
const MAX_CPUS: usize = 64;

/// Core types present on modern hybrid Intel™ / AMD CPUs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreType {
    Performance, // Big/P-core (Intel P-core, AMD CCX primary, etc.)
    Efficient,   // Little/E-core (Intel Atom/E-core)
    Unknown,
}

/// Snapshot of one CPU's configuration, used during the initial topology build.
///
/// Only `cpu_id`, `core_type`, and `restricted` are used by the bitmask
/// selector.  `numa_node` is accepted for forward-compatibility but not yet
/// used to build per-NUMA masks (future extension).
#[derive(Debug, Clone)]
pub struct CpuState {
    pub cpu_id: i32,
    pub core_type: CoreType,
    /// NUMA node of this CPU (accepted but not yet used for NUMA mask routing).
    pub numa_node: u32,
    /// If `true`, this CPU is reserved exclusively for quarantined tasks.
    pub restricted: bool,
}

/// Return a u64 with the `cpu_id`-th bit set, or 0 if `cpu_id ≥ MAX_CPUS`.
#[inline(always)]
fn cpu_bit(cpu_id: i32) -> u64 {
    if cpu_id < 0 || cpu_id as usize >= MAX_CPUS {
        return 0;
    }
    1u64 << cpu_id as u32
}

/// O(1) bitmask CPU selector.
///
/// Replaces `AStarLoadBalancer`.  All placement state lives in three u64 fields
/// (`p_mask`, `restricted_mask`, `all_mask`) that never grow or shrink after
/// `Scheduler::init()`.
pub struct CpuSelector {
    /// Bit i set = CPU i is a Performance (big/P) core.
    /// On non-hybrid CPUs (AMD, homogeneous Intel, VMs) every bit is set,
    /// so all CPUs are treated as Performance-class — no change in behaviour.
    p_mask: u64,
    /// Bit i set = CPU i is reserved exclusively for quarantined tasks.
    restricted_mask: u64,
    /// Bit i set = CPU i is online.
    all_mask: u64,
    /// Number of online CPUs tracked in `all_mask`.
    pub nr_cpus: u32,
    /// EWMA of per-task performance criticality observed system-wide.
    ///
    /// Tasks with `perf_cri ≥ avg_perf_cri` are routed to P-cores (if any exist).
    /// Tasks below the average are routed to E-cores.  Updated once per policy
    /// tick (every 250 ms) via `update_avg_perf_cri()`.
    ///
    /// Initialised to 0.5 (the centre of [0, 1]) so the P/E split is symmetric
    /// until real observations drive it to reflect the actual workload.
    pub avg_perf_cri: f32,
}

impl CpuSelector {
    /// Create an empty selector (no CPUs registered yet).
    pub fn new() -> Self {
        Self {
            p_mask: 0,
            restricted_mask: 0,
            all_mask: 0,
            nr_cpus: 0,
            avg_perf_cri: 0.5,
        }
    }

    /// Register one CPU's type and quarantine role.
    ///
    /// Called once per CPU in `Scheduler::init()`.  After `init()` returns,
    /// `update_cpu` is never called again — the bitmasks are static.
    pub fn update_cpu(&mut self, state: CpuState) {
        let bit = cpu_bit(state.cpu_id);
        if bit == 0 {
            // cpu_id ≥ MAX_CPUS (64) — ignored.
            // Extend to [u64; N] masks if > 64 CPU systems need full support.
            return;
        }
        self.all_mask |= bit;
        self.nr_cpus = self.all_mask.count_ones();

        if matches!(state.core_type, CoreType::Performance) {
            self.p_mask |= bit;
        } else {
            self.p_mask &= !bit; // E-core or Unknown → clear P bit
        }

        if state.restricted {
            self.restricted_mask |= bit;
        } else {
            self.restricted_mask &= !bit; // normal CPU → clear restricted bit
        }
    }

    /// Update the system-wide average performance criticality score.
    ///
    /// Called from `tick_policy()` with the EWMA of per-task `perf_cri` values
    /// observed in the most recent scheduling window.  α = 0.15 provides a
    /// gentle adaptation that avoids over-reacting to short workload bursts.
    pub fn update_avg_perf_cri(&mut self, new_avg: f32) {
        // α = 0.15: matches the old A* `update_avg_perf_cri` coefficient.
        self.avg_perf_cri = self.avg_perf_cri * 0.85 + new_avg * 0.15;
    }

    /// Select the best CPU for a task.
    ///
    /// Cost: 3–6 bit operations + TZCNT = O(1), approximately 1–2 ns.
    ///
    /// # Parameters
    /// * `_prev_cpu`  — the CPU the task last ran on (unused; NUMA-distance
    ///   routing is a planned future extension via per-NUMA masks).
    /// * `label`      — task label from the heuristic classifier.
    /// * `quarantine` — task is anomaly-flagged and must run on restricted CPUs.
    /// * `perf_cri`   — task's performance criticality score ∈ [0, 1].
    ///   Above `avg_perf_cri` → routed to P-core.
    ///   Below                → routed to E-core.
    ///
    /// # Returns
    /// The selected `cpu_id` (lowest-numbered bit in the winner mask),
    /// or `-1` (`RL_CPU_ANY`) if no eligible CPU exists.
    pub fn select_cpu(
        &self,
        _prev_cpu: i32,
        label: TaskLabel,
        quarantine: bool,
        perf_cri: f32,
    ) -> i32 {
        if self.all_mask == 0 {
            return -1; // RL_CPU_ANY — topology not yet initialised
        }

        // Determine the eligible CPU pool based on quarantine status.
        // Quarantined tasks may ONLY land on restricted CPUs.
        // Normal tasks must NOT land on restricted CPUs.
        let pool = if quarantine {
            self.restricted_mask
        } else {
            self.all_mask & !self.restricted_mask
        };

        if pool == 0 {
            return -1; // RL_CPU_ANY — no eligible CPUs in this pool
        }

        // RealTime tasks always get a P-core regardless of perf_cri score.
        // All other tasks: compare perf_cri against the system-wide average.
        let want_p = matches!(label, TaskLabel::RealTime) || perf_cri >= self.avg_perf_cri;

        // First: try the preferred core type.
        let preferred = if want_p {
            pool & self.p_mask
        } else {
            pool & !self.p_mask // E-core preferred
        };

        if preferred != 0 {
            // trailing_zeros() = TZCNT = 1 instruction.  Picks the lowest-numbered
            // eligible CPU in the preferred type.
            return preferred.trailing_zeros() as i32;
        }

        // Fallback: relax core-type constraint — any eligible CPU in the pool.
        pool.trailing_zeros() as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cpu(id: i32, ctype: CoreType, restricted: bool) -> CpuState {
        CpuState {
            cpu_id: id,
            core_type: ctype,
            numa_node: 0,
            restricted,
        }
    }

    #[test]
    fn selects_p_core_for_high_perf_cri() {
        let mut sel = CpuSelector::new();
        // CPU 0: E-core. CPU 1: P-core. CPU 2: P-core.
        sel.update_cpu(make_cpu(0, CoreType::Efficient, false));
        sel.update_cpu(make_cpu(1, CoreType::Performance, false));
        sel.update_cpu(make_cpu(2, CoreType::Performance, false));
        // perf_cri 0.8 > avg 0.5 → wants P-core; expect CPU 1 (lowest P-core bit).
        let chosen = sel.select_cpu(0, TaskLabel::Interactive, false, 0.8);
        assert!(
            chosen == 1 || chosen == 2,
            "expected a P-core (1 or 2), got {chosen}"
        );
    }

    #[test]
    fn selects_e_core_for_low_perf_cri() {
        let mut sel = CpuSelector::new();
        sel.update_cpu(make_cpu(0, CoreType::Efficient, false));
        sel.update_cpu(make_cpu(1, CoreType::Performance, false));
        // perf_cri 0.2 < avg 0.5 → wants E-core.
        let chosen = sel.select_cpu(0, TaskLabel::Compute, false, 0.2);
        assert_eq!(chosen, 0, "expected E-core (cpu 0), got {chosen}");
    }

    #[test]
    fn quarantine_only_restricted() {
        let mut sel = CpuSelector::new();
        sel.update_cpu(make_cpu(0, CoreType::Performance, false)); // normal CPU
        sel.update_cpu(make_cpu(1, CoreType::Efficient, true)); // restricted CPU
        // Quarantined task must land on cpu 1 only.
        let chosen = sel.select_cpu(0, TaskLabel::Compute, true, 0.5);
        assert_eq!(chosen, 1, "quarantined task must go to restricted cpu 1");
    }

    #[test]
    fn non_quarantine_avoids_restricted() {
        let mut sel = CpuSelector::new();
        sel.update_cpu(make_cpu(0, CoreType::Performance, false)); // normal
        sel.update_cpu(make_cpu(1, CoreType::Performance, true)); // restricted only
        // Normal task must not land on restricted cpu 1.
        let chosen = sel.select_cpu(0, TaskLabel::Interactive, false, 0.8);
        assert_eq!(chosen, 0, "normal task must not go to restricted cpu 1");
    }

    #[test]
    fn realtime_always_p_core() {
        let mut sel = CpuSelector::new();
        sel.update_cpu(make_cpu(0, CoreType::Efficient, false));
        sel.update_cpu(make_cpu(1, CoreType::Performance, false));
        // RealTime always gets P-core regardless of perf_cri.
        let chosen = sel.select_cpu(0, TaskLabel::RealTime, false, 0.0);
        assert_eq!(chosen, 1, "RealTime must always be placed on a P-core");
    }

    #[test]
    fn fallback_to_any_when_preferred_unavailable() {
        let mut sel = CpuSelector::new();
        // All E-cores but we want P-core (high perf_cri).
        sel.update_cpu(make_cpu(0, CoreType::Efficient, false));
        sel.update_cpu(make_cpu(1, CoreType::Efficient, false));
        let chosen = sel.select_cpu(0, TaskLabel::Interactive, false, 0.9);
        // No P-cores available → falls back to any eligible CPU.
        assert!(chosen >= 0, "must fall back to any eligible cpu, got {chosen}");
    }
}
