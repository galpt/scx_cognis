// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// AI Module: PPO-lite Policy Controller
//
// Implements a simplified Proximal Policy Optimisation-inspired controller
// that dynamically adjusts the global time-slice parameter to maximise a
// composite reward signal:
//
//   R = w1 * fps_approx  +  w2 * throughput  −  w3 * latency_p99
//
// Because true PPO requires neural networks and gradient computation (which
// would blow our latency budget), we approximate the policy with a discrete
// action space and a value function implemented as an exponential moving
// average table over (state, action) pairs.
//
// State:  [load_avg, interactive_fraction, compute_fraction, latency_ema_ns]
// Action: {shrink_slice, keep_slice, grow_slice}
//
// The controller runs asynchronously (not on the hot scheduling path) and
// updates a shared atomic that the main scheduler reads on `ops.dispatch`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

// ── Action space ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    ShrinkSlice = 0,
    KeepSlice   = 1,
    GrowSlice   = 2,
}

const N_ACTIONS: usize = 3;

// Adjustment ratios per action (applied to current slice).
const ACTION_RATIO: [f64; N_ACTIONS] = [0.80, 1.00, 1.25];

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
            data: vec![[0.0; N_ACTIONS]; TABLE_SIZE].try_into()
                .unwrap_or_else(|_| {
                    // Fallback for platforms where const array conversion fails.
                    let boxed = unsafe {
                        let ptr = std::alloc::alloc_zeroed(
                            std::alloc::Layout::array::<[f64; N_ACTIONS]>(TABLE_SIZE).unwrap()
                        ) as *mut [[f64; N_ACTIONS]; TABLE_SIZE];
                        Box::from_raw(ptr)
                    };
                    boxed
                }),
        }
    }

    fn best_action(&self, state: usize) -> Action {
        let row = &self.data[state];
        let best_idx = row.iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
            .unwrap_or(1); // default: keep slice
        match best_idx {
            0 => Action::ShrinkSlice,
            2 => Action::GrowSlice,
            _ => Action::KeepSlice,
        }
    }

    fn update(&mut self, state: usize, action: Action, reward: f64, next_state: usize, alpha: f64, gamma: f64) {
        let a = action as usize;
        let next_max = self.data[next_state].iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let old = self.data[state][a];
        self.data[state][a] = old + alpha * (reward + gamma * next_max - old);
    }
}

// ── Reward weights ────────────────────────────────────────────────────────────

pub struct RewardWeights {
    /// How much we care about throughput (dispatch rate).
    pub w_throughput: f64,
    /// How much we penalise scheduling latency.
    pub w_latency:    f64,
    /// How much we reward low congestion.
    pub w_congestion: f64,
}

impl Default for RewardWeights {
    fn default() -> Self {
        Self {
            w_throughput: 0.4,
            w_latency:    0.4,
            w_congestion: 0.2,
        }
    }
}

// ── Observable signal from the scheduler ─────────────────────────────────────

/// Snapshot of observable metrics passed to the policy on each update.
#[derive(Debug, Default, Clone)]
pub struct SchedulerSignal {
    /// System load average (0..n_cpus) normalised to 0..1.
    pub load_norm: f64,
    /// Fraction of running tasks classified as Interactive.
    pub interactive_frac: f64,
    /// Fraction of running tasks classified as Compute.
    pub compute_frac: f64,
    /// P99 scheduling latency EMA (ns), normalised to [0, 1] by dividing by 10 ms.
    pub latency_p99_norm: f64,
    /// Number of user-space dispatches per second (raw).
    pub dispatch_rate: f64,
    /// Number of congestion events per second.
    pub congestion_rate: f64,
}

// ── Policy Controller ─────────────────────────────────────────────────────────

/// PPO-lite policy controller.
///
/// Runs in a background thread; updates `shared_slice_ns` which the main
/// scheduling loop reads atomically.
pub struct PolicyController {
    q:               QTable,
    weights:         RewardWeights,
    /// ε-greedy exploration rate (decays over time).
    epsilon:         f64,
    /// Learning rate.
    alpha:           f64,
    /// Discount factor.
    gamma:           f64,
    /// Previous state index.
    prev_state:      usize,
    /// Previous action taken.
    prev_action:     Action,
    /// Target slice value (nanoseconds) — written to shared atomic.
    pub current_slice_ns: u64,
    /// Base slice from user options (fixed reference point).
    base_slice_ns:   u64,
    /// EMA of recent rewards (for TUI display).
    pub reward_ema:  f64,
    /// Shared atomic that the scheduler hot-loop reads.
    pub shared_slice_ns: Arc<AtomicU64>,
}

impl PolicyController {
    pub fn new(base_slice_ns: u64) -> Self {
        let shared = Arc::new(AtomicU64::new(base_slice_ns));
        Self {
            q:                QTable::new(),
            weights:          RewardWeights::default(),
            epsilon:          0.15,
            alpha:            0.01,
            gamma:            0.95,
            prev_state:       0,
            prev_action:      Action::KeepSlice,
            current_slice_ns: base_slice_ns,
            base_slice_ns,
            reward_ema:       0.0,
            shared_slice_ns:  shared,
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
        self.q.update(self.prev_state, self.prev_action, reward, state, self.alpha, self.gamma);

        // Decay exploration.
        self.epsilon = (self.epsilon * 0.9995).max(0.02);

        // ε-greedy action selection.
        let action = if rand_f64() < self.epsilon {
            // Explore: random action.
            let r = (rand_f64() * N_ACTIONS as f64) as usize;
            match r { 0 => Action::ShrinkSlice, 2 => Action::GrowSlice, _ => Action::KeepSlice }
        } else {
            self.q.best_action(state)
        };

        // Apply action to current slice.
        let ratio = ACTION_RATIO[action as usize];
        let min_slice = self.base_slice_ns / 4;
        let max_slice = self.base_slice_ns * 4;
        let new_slice = ((self.current_slice_ns as f64 * ratio) as u64)
            .clamp(min_slice, max_slice);

        self.current_slice_ns = new_slice;
        self.prev_state  = state;
        self.prev_action = action;

        // Publish to shared atomic.
        self.shared_slice_ns.store(new_slice, Ordering::Relaxed);
        new_slice
    }

    /// Compute the reward signal from the current scheduler state.
    fn compute_reward(&self, sig: &SchedulerSignal) -> f64 {
        let w = &self.weights;
        // Throughput term: more dispatches = better.
        let throughput_reward = (sig.dispatch_rate / 1_000_000.0).min(1.0) * w.w_throughput;
        // Latency term: lower latency = better (invert).
        let latency_penalty = sig.latency_p99_norm * w.w_latency;
        // Congestion term: lower congestion = better.
        let congestion_penalty = (sig.congestion_rate / 1000.0).min(1.0) * w.w_congestion;

        throughput_reward - latency_penalty - congestion_penalty
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
    s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
    STATE.store(s, Ordering::Relaxed);
    (s >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_stays_in_bounds() {
        let mut ctrl = PolicyController::new(20_000_000); // 20 ms base
        let sig = SchedulerSignal {
            load_norm:       0.8,
            interactive_frac: 0.3,
            compute_frac:    0.5,
            latency_p99_norm: 0.6,
            dispatch_rate:   50_000.0,
            congestion_rate: 100.0,
        };
        for _ in 0..1000 {
            ctrl.update(&sig);
        }
        let slice = ctrl.read_slice_ns();
        assert!(slice >= 20_000_000 / 4);
        assert!(slice <= 20_000_000 * 4);
    }
}
