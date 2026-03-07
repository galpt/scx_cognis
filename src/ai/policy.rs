// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// AI Module: Q-learning Policy Controller
//
// Implements a tabular Q-learning reinforcement learning controller that
// dynamically adjusts the global time-slice parameter to maximise a
// composite reward signal:
//
//   R = interactive_frac * load_norm * 0.7  −  congestion * 0.2  −  latency * 0.1
//
// This is standard off-policy Q-learning with a discrete (state, action) table
// and the Bellman update rule.  It is NOT PPO (Proximal Policy Optimization) —
// PPO is an on-policy policy-gradient algorithm.  Q-learning and PPO are
// distinct RL paradigms; we use Q-learning because it is simpler, stationary,
// and has a bounded update cost suitable for a scheduling hot-path adjunct.
//
// State:  [load_avg, interactive_fraction, compute_fraction, latency_ema_ns]
// Action: {shrink_slice, keep_slice, grow_slice}
//
// The controller runs asynchronously (not on the hot scheduling path) and
// updates a shared atomic that the main scheduler reads on `ops.dispatch`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

// ── Auto-slice constants ──────────────────────────────────────────────────────

/// Targeted scheduling latency: the time window in which all runnable tasks
/// should be served at least once. Mirrors LAVD's targeted_latency concept,
/// but tuned for a 120 Hz desktop frame budget instead of throughput-first
/// batch execution. Actual slice = TARGETED_LATENCY_NS / max(nr_runnable_per_cpu, 1).
const TARGETED_LATENCY_NS: u64 = 6_000_000; // 6 ms

/// Absolute minimum slice regardless of how many tasks are running.
/// Below this value, context-switch overhead starts dominating.
const AUTO_SLICE_MIN_NS: u64 = 250_000; // 250 µs

/// Absolute maximum slice even under very low load.
/// Caps how long a task can monopolize a CPU even when the system is nearly idle.
const AUTO_SLICE_MAX_NS: u64 = 8_000_000; // 8 ms

// ── Action space ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Shrink = 0,
    Keep = 1,
    Grow = 2,
}

const N_ACTIONS: usize = 3;

// Adjustment ratios per action (applied to current slice within [auto_base/4, auto_base]).
// Q-learning can only CONTRACT the slice for interactive-heavy phases or RESTORE
// it when the system is I/O-bound — it cannot inflate above the auto-computed base.
const ACTION_RATIO: [f64; N_ACTIONS] = [0.75, 1.00, 1.08];

// ── State discretisation ─────────────────────────────────────────────────────

// Quantise a 0..1 float into one of N_BINS buckets.
const N_BINS: usize = 5;

fn quantise(v: f64) -> usize {
    ((v * N_BINS as f64) as usize).min(N_BINS - 1)
}

// State tuple index: (load_bin, interactive_bin, compute_bin, latency_bin).
fn state_index(load: f64, interactive: f64, compute: f64, latency_norm: f64) -> usize {
    let b0 = quantise(load);
    let b1 = quantise(interactive);
    let b2 = quantise(compute);
    let b3 = quantise(latency_norm);
    b0 * N_BINS.pow(3) + b1 * N_BINS.pow(2) + b2 * N_BINS + b3
}

const TABLE_SIZE: usize = N_BINS * N_BINS * N_BINS * N_BINS; // 625 entries

// ── Value function ────────────────────────────────────────────────────────────

/// Q-value table: Q[state][action].
///
/// Initialised to zero (neutral), then updated online via:
///   Q(s, a) ← Q(s, a) + α * ( r + γ·max_a' Q(s', a')  −  Q(s, a) )
struct QTable {
    data: Box<[[f64; N_ACTIONS]; TABLE_SIZE]>,
}

impl QTable {
    fn new() -> Self {
        Self {
            data: vec![[0.0; N_ACTIONS]; TABLE_SIZE]
                .try_into()
                .unwrap_or_else(|_| {
                    // Fallback for platforms where const array conversion fails.
                    unsafe {
                        let ptr = std::alloc::alloc_zeroed(
                            std::alloc::Layout::array::<[f64; N_ACTIONS]>(TABLE_SIZE).unwrap(),
                        ) as *mut [[f64; N_ACTIONS]; TABLE_SIZE];
                        Box::from_raw(ptr)
                    }
                }),
        }
    }

    fn best_action(&self, state: usize) -> Action {
        let row = &self.data[state];
        let best_idx = row
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
            .unwrap_or(1); // default: keep slice
        match best_idx {
            0 => Action::Shrink,
            2 => Action::Grow,
            _ => Action::Keep,
        }
    }

    fn update(
        &mut self,
        state: usize,
        action: Action,
        reward: f64,
        next_state: usize,
        alpha: f64,
        gamma: f64,
    ) {
        let a = action as usize;
        let next_max = self.data[next_state]
            .iter()
            .cloned()
            .fold(f64::NEG_INFINITY, f64::max);
        let old = self.data[state][a];
        self.data[state][a] = old + alpha * (reward + gamma * next_max - old);
    }
}

// ── Observable signal from the scheduler ─────────────────────────────────────

/// Snapshot of observable metrics passed to the policy on each update.
#[derive(Debug, Default, Clone)]
pub struct SchedulerSignal {
    /// System load average (0..n_cpus) normalised to 0..1.
    pub load_norm: f64,
    /// Fraction of cumulative classification events labelled Interactive.
    pub interactive_frac: f64,
    /// Fraction of cumulative classification events labelled Compute.
    pub compute_frac: f64,
    /// P99 scheduling latency EMA (ns), normalised to [0, 1] by dividing by 10 ms.
    pub latency_p99_norm: f64,
    /// Number of congestion events per second.
    pub congestion_rate: f64,
}

// ── Policy Controller ─────────────────────────────────────────────────────────

/// Tabular Q-learning policy controller.
///
/// Runs in a background thread; updates `shared_slice_ns` which the main
/// scheduling loop reads atomically.
pub struct PolicyController {
    q: QTable,
    /// ε-greedy exploration rate (decays over time).
    epsilon: f64,
    /// Learning rate.
    alpha: f64,
    /// Discount factor.
    gamma: f64,
    /// Previous state index.
    prev_state: usize,
    /// Previous action taken.
    prev_action: Action,
    /// Target slice value (nanoseconds) — written to shared atomic.
    pub current_slice_ns: u64,
    /// User-configured base slice (acts as ceiling override if > 0; 0 = auto-only).
    base_slice_ns: u64,
    /// Auto-computed slice ceiling: `TARGETED_LATENCY_NS / nr_runnable_per_cpu`,
    /// clamped to `[AUTO_SLICE_MIN_NS, AUTO_SLICE_MAX_NS]`.  Updated by `update_load()`.
    /// This is the dynamic ceiling for Q-learning to operate within — no manual
    /// tuning required.  The effective ceiling is `min(auto_base_ns, base_slice_ns)`
    /// so a manual `--slice-us` override is always respected.
    pub auto_base_ns: u64,
    /// EMA of recent rewards (for TUI display).
    pub reward_ema: f64,
    /// Shared atomic that the scheduler hot-loop reads.
    pub shared_slice_ns: Arc<AtomicU64>,
}

impl PolicyController {
    pub fn new(base_slice_ns: u64) -> Self {
        // Start the auto-base at a reasonable default.  It will be updated
        // immediately once the first tick_policy() fires with real load data.
        let initial_auto = base_slice_ns.clamp(AUTO_SLICE_MIN_NS, AUTO_SLICE_MAX_NS);
        let shared = Arc::new(AtomicU64::new(initial_auto));
        Self {
            q: QTable::new(),
            epsilon: 0.15,
            alpha: 0.01,
            gamma: 0.95,
            prev_state: 0,
            prev_action: Action::Keep,
            current_slice_ns: initial_auto,
            base_slice_ns,
            auto_base_ns: initial_auto,
            reward_ema: 0.0,
            shared_slice_ns: shared,
        }
    }

    /// Update the auto-computed slice ceiling based on current system load.
    ///
    /// Formula: `TARGETED_LATENCY_NS / max(tasks_per_cpu, 1)`,
    /// clamped to `[AUTO_SLICE_MIN_NS, AUTO_SLICE_MAX_NS]`.
    ///
    /// This mirrors LAVD's `slice = targeted_latency / nr_runnable` formula and
    /// eliminates the need for a human to tune `--slice-us` for their workload:
    /// - Lightly loaded system (2 tasks on 8 cores → 0.25 tasks/cpu): 20 ms slice.
    /// - Moderately loaded (16 tasks on 8 cores → 2 tasks/cpu): 7.5 ms slice.
    /// - Heavily loaded (80 tasks on 8 cores → 10 tasks/cpu): 1.5 ms slice.
    ///
    /// Should be called once per policy tick (every 250 ms) with fresh counters.
    pub fn update_load(&mut self, nr_runnable: u64, nr_cpus: u64) {
        if nr_cpus == 0 {
            return;
        }
        let tasks_per_cpu = (nr_runnable as f64 / nr_cpus as f64).max(1.0);
        let computed = (TARGETED_LATENCY_NS as f64 / tasks_per_cpu) as u64;
        let new_auto = computed.clamp(AUTO_SLICE_MIN_NS, AUTO_SLICE_MAX_NS);

        // Smooth the update with a gentle EWMA to avoid abrupt slice changes
        // when the runqueue depth fluctuates rapidly (e.g. burst of fork+exec).
        let smoothed = ((self.auto_base_ns as f64 * 0.7) + (new_auto as f64 * 0.3)) as u64;
        self.auto_base_ns = smoothed.clamp(AUTO_SLICE_MIN_NS, AUTO_SLICE_MAX_NS);
    }

    /// Effective slice ceiling: the lesser of the user's manual `--slice-us`
    /// override (if non-zero) and the auto-computed load-based ceiling.
    fn effective_base_ns(&self) -> u64 {
        if self.base_slice_ns > 0 {
            self.auto_base_ns.min(self.base_slice_ns)
        } else {
            self.auto_base_ns
        }
    }

    /// Update the policy with a new scheduler signal.
    ///
    /// Returns the new recommended time-slice in nanoseconds.
    pub fn update(&mut self, sig: &SchedulerSignal) -> u64 {
        let state = state_index(
            sig.load_norm,
            sig.interactive_frac,
            sig.compute_frac,
            sig.latency_p99_norm,
        );

        // Compute reward signal.
        let reward = self.compute_reward(sig);
        self.reward_ema = 0.9 * self.reward_ema + 0.1 * reward;

        // Q-table update.
        self.q.update(
            self.prev_state,
            self.prev_action,
            reward,
            state,
            self.alpha,
            self.gamma,
        );

        // Decay exploration.
        self.epsilon = (self.epsilon * 0.9995).max(0.02);

        // ε-greedy action selection.
        let action = if rand_f64() < self.epsilon {
            // Explore: random action.
            let r = (rand_f64() * N_ACTIONS as f64) as usize;
            match r {
                0 => Action::Shrink,
                2 => Action::Grow,
                _ => Action::Keep,
            }
        } else {
            self.q.best_action(state)
        };

        // Apply action to current slice.
        let ratio = ACTION_RATIO[action as usize];
        let effective_max = self.effective_base_ns();
        let min_slice = effective_max / 4;
        // The effective_max is the lesser of the user's manual override and the
        // auto-computed load-based ceiling.  Q-learning can only contract the
        // slice within this window or restore it — it never inflates above the
        // auto-computed ceiling.  This means on a heavily-loaded system the max
        // slice automatically shrinks to give all tasks a fair turn.
        let new_slice =
            ((self.current_slice_ns as f64 * ratio) as u64).clamp(min_slice, effective_max);

        self.current_slice_ns = new_slice;
        self.prev_state = state;
        self.prev_action = action;

        // Publish to shared atomic.
        self.shared_slice_ns.store(new_slice, Ordering::Relaxed);
        new_slice
    }

    /// Compute the reward signal from the current scheduler state.
    ///
    /// # Design rationale
    ///
    /// The original reward `dispatch_rate / 1_000_000 × 0.4` produced values
    /// of ~0.0002 (573 dispatches/s × 0.4/1M), which is so close to zero that
    /// Q-table updates had no gradient and the policy random-walked to the
    /// max-slice corner.  The fix: reward interactivity directly.
    ///
    /// Reward signal (range ≈ −0.3 … +0.7):
    ///
    /// - `+interactive_fraction × load_norm` — primary goal: keep interactive tasks running under load
    /// - `-congestion_rate / 100` — penalise dispatch backlog
    /// - `-latency_p99_norm` — penalise inference overhead
    fn compute_reward(&self, sig: &SchedulerSignal) -> f64 {
        // Primary term: are interactive tasks actually getting CPU time?
        // Multiplied by load_norm so an idle system (load≈0) contributes ~0
        // — the policy should not try to optimise when nothing is running.
        let interactivity = sig.interactive_frac * sig.load_norm.min(1.0);

        // Congestion: high queue depth means scheduler is falling behind.
        let congestion = (sig.congestion_rate / 100.0).min(1.0);

        // Latency: scheduling overhead eating into the slice budget.
        let latency = sig.latency_p99_norm;

        (interactivity * 0.7 - congestion * 0.2 - latency * 0.1).clamp(-1.0, 1.0)
    }

    /// Read the current slice recommendation from the shared atomic.
    pub fn read_slice_ns(&self) -> u64 {
        self.shared_slice_ns.load(Ordering::Relaxed)
    }
}

/// Tiny LCG pseudo-random; avoids a full RNG dependency on the hot path.
fn rand_f64() -> f64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0xDEAD_BEEF_CAFE_1234);
    let mut s = STATE.load(Ordering::Relaxed);
    s = s
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    STATE.store(s, Ordering::Relaxed);
    (s >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_stays_in_bounds() {
        let mut ctrl = PolicyController::new(20_000_000); // 20 ms base
                                                          // Simulate a load of 4 tasks across 2 CPUs → 2 tasks/cpu → 7.5 ms auto-base.
        ctrl.update_load(4, 2);
        let sig = SchedulerSignal {
            load_norm: 0.8,
            interactive_frac: 0.3,
            compute_frac: 0.5,
            latency_p99_norm: 0.6,
            congestion_rate: 100.0,
        };
        for _ in 0..1000 {
            ctrl.update(&sig);
        }
        let slice = ctrl.read_slice_ns();
        let effective_max = ctrl.effective_base_ns();
        let effective_min = effective_max / 4;
        // Slice must be within [effective_max / 4, effective_max].
        assert!(
            slice >= effective_min,
            "slice {slice} below min {effective_min}"
        );
        assert!(
            slice <= effective_max,
            "slice {slice} above max {effective_max}"
        );
    }
}
