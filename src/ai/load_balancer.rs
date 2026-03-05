// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// A* Load Balancer
//
// Uses an A*-inspired heuristic search over per-CPU cost nodes to find the
// "path of least resistance" when placing a task.  Cost accounts for:
//   - Current utilisation (primary cost)
//   - NUMA distance from the task's previous CPU
//   - Core type mismatch penalty (P-core vs E-core vs Atom)
//   - Thermal throttle penalty
//   - Anti-cheat quarantine (flagged tasks → restricted cores only)
//
// Runs inside `ops.select_cpu` — must return within a handful of µs.

#![allow(dead_code)]

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use crate::ai::classifier::TaskLabel;

/// Core types present on modern hybrid Intel™ / AMD CPUs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreType {
    Performance, // Big/P-core
    Efficient,   // Little/E-core
    Unknown,
}

/// Snapshot of one CPU's current state.
#[derive(Debug, Clone)]
pub struct CpuState {
    pub cpu_id: i32,
    pub core_type: CoreType,
    /// Current utilisation 0..1.
    pub utilisation: f32,
    /// NUMA node.
    pub numa_node: u32,
    /// True if this CPU is being throttled by the thermal governor.
    pub throttled: bool,
    /// True if this CPU should only accept quarantined tasks.
    pub restricted: bool,
}

/// A node in the A* search graph.
#[derive(Debug, Clone)]
struct SearchNode {
    cpu_id: i32,
    /// g(n): cost so far (actual).
    g_cost: f32,
    /// f(n) = g(n) + h(n): estimated total cost.
    f_cost: f32,
}

impl PartialEq for SearchNode {
    fn eq(&self, other: &Self) -> bool {
        self.cpu_id == other.cpu_id
    }
}
impl Eq for SearchNode {}

// Min-heap by f_cost.
impl Ord for SearchNode {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .f_cost
            .partial_cmp(&self.f_cost)
            .unwrap_or(Ordering::Equal)
    }
}
impl PartialOrd for SearchNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Cost weights — tunable at runtime.
pub struct CostWeights {
    pub utilisation: f32,   // weight on utilisation cost
    pub numa_distance: f32, // penalty per NUMA hop
    pub core_mismatch: f32, // penalty for wrong core type
    pub thermal: f32,       // penalty for throttled CPU
}

impl Default for CostWeights {
    fn default() -> Self {
        Self {
            utilisation: 1.0,
            numa_distance: 0.3,
            core_mismatch: 0.2,
            thermal: 0.5,
        }
    }
}

/// The load balancer.
pub struct AStarLoadBalancer {
    /// Topology: cpu_id → state.
    pub cpus: HashMap<i32, CpuState>,
    pub weights: CostWeights,
    /// EWMA of per-task performance criticality observed system-wide.
    ///
    /// Tasks with `perf_cri > avg_perf_cri` are routed to P-cores (if available).
    /// Tasks below average are routed to E-cores.  Updated periodically by the
    /// policy tick via `update_avg_perf_cri()`.
    ///
    /// Starts at 0.5 (the centre of the [0, 1] range) so the system is
    /// symmetrical until real observations arrive.
    pub avg_perf_cri: f32,
}

impl AStarLoadBalancer {
    pub fn new() -> Self {
        Self {
            cpus: HashMap::new(),
            weights: CostWeights::default(),
            avg_perf_cri: 0.5,
        }
    }

    /// Update or insert a CPU state.
    pub fn update_cpu(&mut self, state: CpuState) {
        self.cpus.insert(state.cpu_id, state);
    }

    /// Initialise the topology from a flat list of CPUs.
    pub fn init_topology(&mut self, cpus: Vec<CpuState>) {
        self.cpus.clear();
        for c in cpus {
            self.cpus.insert(c.cpu_id, c);
        }
    }

    /// Update the system-wide average performance criticality score.
    ///
    /// Called from `tick_policy()` with the EWMA of per-task `perf_cri` values
    /// observed in the most recent scheduling window.  Uses a gentle EWMA (α=0.15)
    /// to avoid over-reacting to short-lived bursts.
    pub fn update_avg_perf_cri(&mut self, new_avg: f32) {
        self.avg_perf_cri = self.avg_perf_cri * 0.85 + new_avg * 0.15;
    }

    /// Select the best CPU for a task.
    ///
    /// * `prev_cpu`   — the CPU the task last ran on.
    /// * `label`      — the task label from the heuristic classifier.
    /// * `quarantine` — whether this task is flagged by the anti-cheat engine.
    /// * `perf_cri`   — the task's performance criticality score (0..1).  Tasks
    ///                  above `self.avg_perf_cri` are routed to P-cores; tasks
    ///                  below are routed to E-cores.  This replaces the static
    ///                  label→core-type mapping and adapts to the actual workload.
    ///
    /// Returns the selected `cpu_id`, or `RL_CPU_ANY` (-1) as a fallback.
    pub fn select_cpu(&self, prev_cpu: i32, label: TaskLabel, quarantine: bool, perf_cri: f32) -> i32 {
        if self.cpus.is_empty() {
            return -1; // RL_CPU_ANY
        }

        let preferred_type = self.preferred_core_type(label, perf_cri);
        let prev_numa = self.cpus.get(&prev_cpu).map(|c| c.numa_node).unwrap_or(0);

        // A* search.  The "graph" is flat (every CPU is reachable), so we just
        // enumerate all candidates and pick the minimum-cost one.  This is
        // O(n_cpus) but n_cpus is typically < 256, well within our latency budget.
        let mut heap = BinaryHeap::with_capacity(self.cpus.len());

        for cpu in self.cpus.values() {
            // Quarantine logic: flagged tasks must run on restricted CPUs only.
            if quarantine && !cpu.restricted {
                continue;
            }
            // Non-flagged tasks must not run on restricted-only CPUs.
            if !quarantine && cpu.restricted {
                continue;
            }

            let g = self.node_cost(cpu, prev_numa, preferred_type);
            // Heuristic h(n): nudge idle CPUs (utilisation == 0) towards zero cost.
            let h = if cpu.utilisation < 0.01 {
                0.0
            } else {
                cpu.utilisation * 0.1
            };
            let f = g + h;

            heap.push(SearchNode {
                cpu_id: cpu.cpu_id,
                g_cost: g,
                f_cost: f,
            });
        }

        heap.pop().map(|n| n.cpu_id).unwrap_or(-1)
    }

    /// Compute the cost of placing a task on `cpu`.
    fn node_cost(&self, cpu: &CpuState, prev_numa: u32, preferred: CoreType) -> f32 {
        let w = &self.weights;

        let util_cost = cpu.utilisation * w.utilisation;
        let numa_cost = if cpu.numa_node != prev_numa {
            w.numa_distance
        } else {
            0.0
        };
        let type_cost = if preferred != CoreType::Unknown && cpu.core_type != preferred {
            w.core_mismatch
        } else {
            0.0
        };
        let therm_cost = if cpu.throttled { w.thermal } else { 0.0 };

        util_cost + numa_cost + type_cost + therm_cost
    }

    /// Determine the preferred core type for a task based on its performance
    /// criticality score relative to the system-wide average.
    ///
    /// Unlike the old static `label → CoreType` mapping, this method dynamically
    /// adjusts to the actual workload: if all tasks have high perf_cri (e.g. on
    /// a pure gaming machine), all of them compete for P-cores on merit.  If
    /// the system is mostly idle (all perf_cri near 0.5 = avg), the P/E split
    /// reflects actual need rather than a hardcoded category.
    ///
    /// RealTime tasks are always routed to P-cores regardless of score.
    fn preferred_core_type(&self, label: TaskLabel, perf_cri: f32) -> CoreType {
        // RealTime tasks unconditionally require the fastest available core.
        if matches!(label, TaskLabel::RealTime) {
            return CoreType::Performance;
        }
        // On non-hybrid systems every CPU is Performance — the comparison is
        // trivially true and every task "prefers" Performance, which is correct.
        if perf_cri >= self.avg_perf_cri {
            CoreType::Performance
        } else {
            CoreType::Efficient
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cpu(id: i32, util: f32, ctype: CoreType) -> CpuState {
        CpuState {
            cpu_id: id,
            core_type: ctype,
            utilisation: util,
            numa_node: 0,
            throttled: false,
            restricted: false,
        }
    }

    #[test]
    fn selects_idle_cpu() {
        let mut lb = AStarLoadBalancer::new();
        lb.update_cpu(make_cpu(0, 0.9, CoreType::Performance));
        lb.update_cpu(make_cpu(1, 0.0, CoreType::Performance)); // idle
        lb.update_cpu(make_cpu(2, 0.8, CoreType::Performance));

        // perf_cri = 0.8 > avg_perf_cri = 0.5  →  prefers Performance core; idle wins.
        let best = lb.select_cpu(0, TaskLabel::Interactive, false, 0.8);
        assert_eq!(best, 1);
    }

    #[test]
    fn quarantine_only_restricted() {
        let mut lb = AStarLoadBalancer::new();
        lb.update_cpu(CpuState {
            cpu_id: 0,
            restricted: false,
            utilisation: 0.0,
            ..make_cpu(0, 0.0, CoreType::Efficient)
        });
        lb.update_cpu(CpuState {
            cpu_id: 1,
            restricted: true,
            utilisation: 0.5,
            ..make_cpu(1, 0.5, CoreType::Efficient)
        });

        // Quarantined task must land on cpu 1.
        let best = lb.select_cpu(0, TaskLabel::Compute, true, 0.5);
        assert_eq!(best, 1);
    }
}
