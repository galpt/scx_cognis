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
    /// Last-level cache domain identifier for this CPU.
    pub llc_id: u16,
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
/// Replaces `AStarLoadBalancer`.  All placement state lives in four u64 fields:
/// - `p_mask`:          static P-core membership, set once at init.
/// - `restricted_mask`: static quarantine-only CPUs, set once at init.
/// - `all_mask`:        static online CPU set, set once at init.
/// - `idle_mask`:       **dynamic** — which CPUs are available this dispatch window.
///
/// `idle_mask` is reset to `all_mask` at the start of every `schedule()` call
/// and cleared bit-by-bit as tasks are dispatched to specific CPUs.  This turns
/// the selection into effective round-robin distribution across CPUs within each
/// scheduling window, preventing the "always CPU-0" monopoly that caused
/// kworker stalls when the original idle-mask was never wired up.
pub struct CpuSelector {
    /// Bit i set = CPU i is a Performance (big/P) core.
    /// On non-hybrid CPUs (AMD, homogeneous Intel, VMs) every bit is set,
    /// so all CPUs are treated as Performance-class — no change in behaviour.
    p_mask: u64,
    /// Bit i set = CPU i is reserved exclusively for quarantined tasks.
    restricted_mask: u64,
    /// Bit i set = CPU i is online.
    all_mask: u64,
    /// Bit i set = CPU i is available for dispatch in the current scheduling window.
    ///
    /// Reset to `all_mask` at the start of each `schedule()` call via
    /// `reset_idle()`.  Cleared by `mark_busy(cpu)` after each successful
    /// dispatch to a specific CPU.  When empty, `select_cpu` falls back to
    /// `prev_cpu` (respecting CPU affinity) before returning `RL_CPU_ANY`.
    idle_mask: u64,
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
    /// LLC identifier indexed by CPU id. `u16::MAX` means unknown / untracked.
    cpu_llc_ids: [u16; MAX_CPUS],
    /// Number of distinct LLC ids present in `cpu_llc_ids`.
    nr_llcs: u16,
}

impl CpuSelector {
    /// Create an empty selector (no CPUs registered yet).
    pub fn new() -> Self {
        Self {
            p_mask: 0,
            restricted_mask: 0,
            all_mask: 0,
            idle_mask: 0,
            nr_cpus: 0,
            avg_perf_cri: 0.5,
            cpu_llc_ids: [u16::MAX; MAX_CPUS],
            nr_llcs: 0,
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
        self.idle_mask = self.all_mask; // keep idle_mask in sync during init
        self.nr_cpus = self.all_mask.count_ones();
        self.cpu_llc_ids[state.cpu_id as usize] = state.llc_id;
        let mut llc_count = 0u16;
        for idx in 0..MAX_CPUS {
            let llc_id = self.cpu_llc_ids[idx];
            if llc_id == u16::MAX {
                continue;
            }

            let seen_before = self.cpu_llc_ids[..idx].contains(&llc_id);
            if !seen_before {
                llc_count = llc_count.saturating_add(1);
            }
        }
        self.nr_llcs = llc_count.max(1);

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

    // ── Dynamic idle-mask management ──────────────────────────────────────────

    /// Reset the idle mask to all online CPUs at the start of a dispatch window.
    ///
    /// Called by `Scheduler::schedule()` once before the per-task dispatch loop.
    /// CPUs are then marked busy one-by-one as tasks are dispatched, giving
    /// effective round-robin distribution within each scheduling cycle.
    #[inline(always)]
    pub fn reset_idle(&mut self) {
        self.idle_mask = self.all_mask;
    }

    /// Mark CPU `cpu` as busy (dispatched to this cycle).
    ///
    /// Called after a successful `bpf.dispatch_task()` that targeted a specific
    /// CPU.  Does nothing for `RL_CPU_ANY` dispatches.
    #[inline(always)]
    pub fn mark_busy(&mut self, cpu: i32) {
        self.idle_mask &= !cpu_bit(cpu);
    }

    /// Mark CPU `cpu` as idle (available for the next task).
    ///
    /// Optional: can be called from a BPF idle callback if available.
    #[inline(always)]
    pub fn mark_idle(&mut self, cpu: i32) {
        self.idle_mask |= cpu_bit(cpu) & self.all_mask;
    }

    // ── Periodic policy update ─────────────────────────────────────────────────

    /// Update the system-wide average performance criticality score.
    ///
    /// Called from the periodic slice-control tick with the EWMA of per-task `perf_cri` values
    /// observed in the most recent scheduling window.  α = 0.15 provides a
    /// gentle adaptation that avoids over-reacting to short workload bursts.
    pub fn update_avg_perf_cri(&mut self, new_avg: f32) {
        // α = 0.15: matches the old A* `update_avg_perf_cri` coefficient.
        self.avg_perf_cri = self.avg_perf_cri * 0.85 + new_avg * 0.15;
    }

    /// Returns true when the topology contains both preferred and efficient cores.
    #[inline(always)]
    pub fn has_little_cores(&self) -> bool {
        self.all_mask != 0 && self.p_mask != 0 && self.p_mask != self.all_mask
    }

    /// Returns true when multiple LLC domains are visible in the topology.
    #[inline(always)]
    pub fn has_multiple_llcs(&self) -> bool {
        self.nr_llcs > 1
    }

    #[inline(always)]
    fn llc_id(&self, cpu: i32) -> Option<u16> {
        let bit = cpu_bit(cpu);
        if bit == 0 {
            return None;
        }
        let llc_id = self.cpu_llc_ids[cpu as usize];
        (llc_id != u16::MAX).then_some(llc_id)
    }

    #[inline(always)]
    pub fn shares_llc(&self, cpu_a: i32, cpu_b: i32) -> bool {
        match (self.llc_id(cpu_a), self.llc_id(cpu_b)) {
            (Some(llc_a), Some(llc_b)) => llc_a == llc_b,
            _ => true,
        }
    }

    /// Returns whether `cpu` is an eligible CPU of the preferred core class.
    ///
    /// This is intended for schedulers that still rely on the kernel's idle-CPU
    /// selector but want to reject placements that land latency-sensitive work
    /// on the wrong core type on hybrid systems.
    #[inline(always)]
    pub fn prefers_cpu(&self, cpu: i32, label: TaskLabel, quarantine: bool, perf_cri: f32) -> bool {
        let bit = cpu_bit(cpu);
        if bit == 0 {
            return false;
        }

        let pool = if quarantine {
            self.restricted_mask
        } else {
            self.all_mask & !self.restricted_mask
        };
        if (pool & bit) == 0 {
            return false;
        }

        if !self.has_little_cores() {
            return true;
        }

        let want_p = matches!(label, TaskLabel::RealTime) || perf_cri >= self.avg_perf_cri;
        if want_p {
            (self.p_mask & bit) != 0
        } else {
            (self.p_mask & bit) == 0
        }
    }

    /// Returns whether an explicit idle CPU should be accepted for a wakeup.
    ///
    /// For latency-sensitive tasks on multi-LLC systems, require the selected
    /// idle CPU to stay within the task's previous LLC when that information is
    /// available. This preserves cache warmth without forcing a busy per-CPU
    /// dispatch when the locality target is not idle.
    #[inline(always)]
    pub fn accepts_idle_cpu(
        &self,
        cpu: i32,
        prev_cpu: i32,
        label: TaskLabel,
        quarantine: bool,
        perf_cri: f32,
        prefer_same_llc: bool,
    ) -> bool {
        if !self.prefers_cpu(cpu, label, quarantine, perf_cri) {
            return false;
        }

        if !prefer_same_llc || !self.has_multiple_llcs() || prev_cpu < 0 {
            return true;
        }

        self.shares_llc(cpu, prev_cpu)
    }

    /// Select the best CPU for a task.
    ///
    /// Cost: 4–8 bit operations + TZCNT = O(1), approximately 2–3 ns.
    ///
    /// # Parameters
    /// * `prev_cpu`   — the CPU the task last ran on.  Used as affinity hint:
    ///   if no idle CPU of the preferred type exists, `prev_cpu` is returned
    ///   (respecting CPU affinity for kworkers and affinity-pinned threads).
    /// * `label`      — task label from the heuristic classifier.
    /// * `quarantine` — task is anomaly-flagged and must run on restricted CPUs.
    /// * `perf_cri`   — task's performance criticality score ∈ [0, 1].
    ///   Above `avg_perf_cri` → routed to P-core.
    ///   Below                → routed to E-core.
    ///
    /// # Returns
    /// The selected `cpu_id`, or `-1` (`RL_CPU_ANY`) if no eligible CPU exists.
    ///
    /// # Selection priority
    /// 1. Idle CPU of the preferred core type (P or E) — best placement.
    /// 2. Any idle CPU in the eligible pool — good enough.
    /// 3. `prev_cpu` if it is in the eligible pool — preserves CPU affinity and
    ///    cache warmth; critical for kworkers that must run on a specific CPU.
    /// 4. Any CPU in the pool by lowest bit — last resort (rare; pool non-empty
    ///    but no idle CPUs and prev_cpu ineligible).
    pub fn select_cpu(
        &self,
        prev_cpu: i32,
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

        // ── Priority 1: idle CPU of the preferred core type. ──────────────────
        let idle_pool = pool & self.idle_mask;
        let preferred_idle = if want_p {
            idle_pool & self.p_mask
        } else {
            idle_pool & !self.p_mask // E-core preferred
        };
        if preferred_idle != 0 {
            return preferred_idle.trailing_zeros() as i32;
        }

        // ── Priority 2: any idle CPU in the eligible pool. ────────────────────
        if idle_pool != 0 {
            return idle_pool.trailing_zeros() as i32;
        }

        // ── Priority 3: prev_cpu if eligible (affinity / cache warmth). ───────
        // This is the critical path for kworkers and CPU-affine threads: when
        // all CPUs in the pool are already busy this cycle, fall back to the
        // task's home CPU rather than always picking bit-0 (CPU 0).
        if prev_cpu >= 0 && (pool & cpu_bit(prev_cpu)) != 0 {
            return prev_cpu;
        }

        // ── Priority 4: any eligible CPU — last resort. ───────────────────────
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
            llc_id: 0,
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
        assert!(
            chosen >= 0,
            "must fall back to any eligible cpu, got {chosen}"
        );
    }

    #[test]
    fn rejects_idle_cpu_from_other_llc_when_locality_requested() {
        let mut sel = CpuSelector::new();
        sel.update_cpu(CpuState {
            cpu_id: 0,
            core_type: CoreType::Performance,
            numa_node: 0,
            llc_id: 0,
            restricted: false,
        });
        sel.update_cpu(CpuState {
            cpu_id: 1,
            core_type: CoreType::Performance,
            numa_node: 0,
            llc_id: 1,
            restricted: false,
        });

        assert!(!sel.accepts_idle_cpu(1, 0, TaskLabel::Interactive, false, 0.9, true));
        assert!(sel.accepts_idle_cpu(0, 0, TaskLabel::Interactive, false, 0.9, true));
    }
}
