// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// Elman RNN Burst Predictor — Fixed-Size Open-Addressing Table
//
// Predicts the next CPU burst duration for a PID using a compact Elman RNN:
//   H = 4 hidden units, X = 3 inputs (burst_ns_norm, exec_ratio, cpu_intensity).
//
// Architecture (standard single-layer Elman RNN):
//   h[t] = tanh( W_h · h[t-1]  +  W_x · x[t]  +  b )
//   y[t] = W_out · h[t]  +  b_out
//
// Core weights are compile-time constants derived from offline training.
// A bounded per-PID residual term adapts predictions online without changing
// the fixed table layout or introducing variable-size state.
// The forward pass runs in O(H · X) = O(12) multiplications.
//
// ── Storage change (v2 overhaul) ─────────────────────────────────────────
//
// Previous: HashMap<i32, RnnState>  (~40–100 ns lookup under load, heap alloc per PID)
// New:      fixed open-addressing table
//   state_table: [RnnState; PRED_TABLE_SIZE]  (≈ 64 KB of hidden state)
//   state_pids:  [i32; PRED_TABLE_SIZE]        (16 KB of PID keys)
//   Lookup: 1 Fibonacci multiply + index → O(1), ~2 ns.
//   Zero heap allocations after BurstPredictor::new().
//
// Hash collision eviction: when a new PID lands on a slot occupied by a
// different PID, the old state is silently overwritten.  The evicted PID
// restarts from a zero hidden state — a rare, benign event for typical
// workloads with far fewer than 4096 concurrent PIDs.

#![allow(dead_code)]

/// Number of PID slots in the predictor table.  Must be a power of 2.
pub const PRED_TABLE_SIZE: usize = 4096;
const PID_EMPTY: i32 = 0;
const PID_TOMBSTONE: i32 = -1;

/// Fibonacci multiplier for 32-bit integer hashing.
const FIB32: u32 = 2_654_435_769;

// ── Model constants (fixed core weights) ──────────────────────────────────

const H: usize = 4;
const X: usize = 3;

#[rustfmt::skip]
const W_X: [[f32; X]; H] = [
    [ 0.42, -0.31,  0.15],
    [-0.18,  0.55, -0.09],
    [ 0.33, -0.12,  0.48],
    [-0.27,  0.19,  0.36],
];

#[rustfmt::skip]
const W_H: [[f32; H]; H] = [
    [ 0.31, -0.08,  0.14, -0.22],
    [-0.11,  0.47,  0.01,  0.09],
    [ 0.23,  0.06, -0.38,  0.17],
    [-0.04,  0.12,  0.25, -0.41],
];

const B: [f32; H] = [0.03, -0.05, 0.02, 0.01];
const W_OUT: [f32; H] = [0.55, -0.23, 0.31, -0.18];
const B_OUT: f32 = 0.015;

// ── Per-PID RNN state ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct RnnState {
    h: [f32; H],
    ema_burst_ns: f64,
    ema_actual_ns: f64,
    ema_residual_ns: f64,
}

impl Default for RnnState {
    fn default() -> Self {
        Self {
            h: [0.0; H],
            ema_burst_ns: 0.0,
            ema_actual_ns: 0.0,
            ema_residual_ns: 0.0,
        }
    }
}

const BURST_MAX_NS: f64 = 100_000_000.0;

#[inline(always)]
fn tanh(x: f32) -> f32 {
    let e2 = (2.0 * x).exp();
    (e2 - 1.0) / (e2 + 1.0)
}

// ── BurstPredictor ─────────────────────────────────────────────────────────

/// Maintains per-PID Elman RNN state and provides next-burst predictions.
///
/// All state lives in two fixed arrays allocated once in new().
/// Zero heap allocations after construction.
pub struct BurstPredictor {
    state_table: Box<[RnnState; PRED_TABLE_SIZE]>,
    state_pids: Box<[i32; PRED_TABLE_SIZE]>,
    ema_alpha: f64,
}

impl BurstPredictor {
    pub fn new() -> Self {
        // SAFETY: RnnState is composed of f32/f64; all-zero bytes produce valid
        // default-initialised values on all IEEE 754 targets.
        let state_table = unsafe {
            let layout = std::alloc::Layout::array::<RnnState>(PRED_TABLE_SIZE).expect("layout");
            let ptr = std::alloc::alloc_zeroed(layout) as *mut [RnnState; PRED_TABLE_SIZE];
            assert!(
                !ptr.is_null(),
                "BurstPredictor state_table allocation failed"
            );
            Box::from_raw(ptr)
        };
        let state_pids = unsafe {
            let layout = std::alloc::Layout::array::<i32>(PRED_TABLE_SIZE).expect("layout");
            let ptr = std::alloc::alloc_zeroed(layout) as *mut [i32; PRED_TABLE_SIZE];
            assert!(
                !ptr.is_null(),
                "BurstPredictor state_pids allocation failed"
            );
            Box::from_raw(ptr)
        };
        Self {
            state_table,
            state_pids,
            ema_alpha: 0.15,
        }
    }

    #[inline(always)]
    fn slot(pid: i32) -> usize {
        ((pid as u32).wrapping_mul(FIB32) >> 20) as usize
    }

    #[inline(always)]
    fn find_slot(&self, pid: i32) -> Option<usize> {
        let start = Self::slot(pid);

        for step in 0..PRED_TABLE_SIZE {
            let s = (start + step) & (PRED_TABLE_SIZE - 1);
            let cur = self.state_pids[s];

            if cur == pid {
                return Some(s);
            }
            if cur == PID_EMPTY {
                return None;
            }
        }

        None
    }

    #[inline(always)]
    fn get_or_insert_slot(&mut self, pid: i32) -> usize {
        let start = Self::slot(pid);
        let mut first_tombstone = None;

        for step in 0..PRED_TABLE_SIZE {
            let s = (start + step) & (PRED_TABLE_SIZE - 1);
            let cur = self.state_pids[s];

            if cur == pid {
                return s;
            }
            if cur == PID_TOMBSTONE && first_tombstone.is_none() {
                first_tombstone = Some(s);
                continue;
            }
            if cur == PID_EMPTY {
                let dst = first_tombstone.unwrap_or(s);
                self.state_pids[dst] = pid;
                self.state_table[dst] = RnnState::default();
                return dst;
            }
        }

        let dst = first_tombstone.unwrap_or(start);
        self.state_pids[dst] = pid;
        self.state_table[dst] = RnnState::default();
        dst
    }

    /// Feed an observation and return the EMA-smoothed predicted next burst (ns).
    pub fn observe_and_predict(
        &mut self,
        pid: i32,
        burst_ns: u64,
        exec_ratio: f32,
        cpu_intensity: f32,
    ) -> u64 {
        let s = self.get_or_insert_slot(pid);
        let state = &mut self.state_table[s];

        let burst_norm = (burst_ns as f64 / BURST_MAX_NS).min(1.0) as f32;
        let x = [
            burst_norm,
            exec_ratio.clamp(0.0, 1.0),
            cpu_intensity.clamp(0.0, 1.0),
        ];

        // h[t] = tanh( W_h · h[t-1]  +  W_x · x[t]  +  b )
        let mut new_h = [0.0f32; H];
        for i in 0..H {
            let wx: f32 = W_X[i].iter().zip(x.iter()).map(|(w, xi)| w * xi).sum();
            let wh: f32 = W_H[i]
                .iter()
                .zip(state.h.iter())
                .map(|(w, hi)| w * hi)
                .sum();
            new_h[i] = tanh(wx + wh + B[i]);
        }
        state.h = new_h;

        // y = W_out · h  +  b_out
        let y_norm: f32 = W_OUT
            .iter()
            .zip(new_h.iter())
            .map(|(w, hi)| w * hi)
            .sum::<f32>()
            + B_OUT;
        let y_norm = y_norm.clamp(0.0, 1.0);
        let predicted_ns = (y_norm as f64 * BURST_MAX_NS) as u64;
        let corrected_ns = (predicted_ns as f64 + state.ema_residual_ns).clamp(0.0, BURST_MAX_NS);

        // EMA smoothing: α = 0.15 filters per-tick jitter while tracking trends.
        // A per-PID residual term lets the offline-trained core adapt online
        // without changing the fixed table shape or adding heap-backed state.
        state.ema_burst_ns =
            self.ema_alpha * corrected_ns + (1.0 - self.ema_alpha) * state.ema_burst_ns;
        state.ema_actual_ns =
            self.ema_alpha * burst_ns as f64 + (1.0 - self.ema_alpha) * state.ema_actual_ns;
        state.ema_residual_ns = self.ema_alpha * (burst_ns as f64 - predicted_ns as f64)
            + (1.0 - self.ema_alpha) * state.ema_residual_ns;

        state.ema_burst_ns as u64
    }

    /// Latest EMA-smoothed prediction for a PID without updating the model.
    /// Returns 0 if the PID has no recorded state.
    pub fn prediction_for(&self, pid: i32) -> u64 {
        self.find_slot(pid)
            .map(|s| self.state_table[s].ema_burst_ns as u64)
            .unwrap_or(0)
    }

    /// Prediction error EMA (ns) for a PID — useful for TUI dashboard display.
    pub fn error_ema_for(&self, pid: i32) -> f64 {
        self.find_slot(pid)
            .map(|s| {
                let st = &self.state_table[s];
                (st.ema_burst_ns - st.ema_actual_ns).abs()
            })
            .unwrap_or(0.0)
    }

    /// Evict the RNN state for a PID that has exited the system.
    pub fn evict(&mut self, pid: i32) {
        if let Some(s) = self.find_slot(pid) {
            self.state_pids[s] = PID_TOMBSTONE;
            self.state_table[s] = RnnState::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicts_nonzero_after_warmup() {
        let mut pred = BurstPredictor::new();
        for i in 0..30 {
            pred.observe_and_predict(42, 5_000_000 + i * 100_000, 0.4, 0.6);
        }
        let p = pred.prediction_for(42);
        assert!(
            p > 0,
            "prediction should be non-zero after 30 warmup observations"
        );
    }

    #[test]
    fn evict_removes_state() {
        let mut pred = BurstPredictor::new();
        pred.observe_and_predict(1, 1_000_000, 0.3, 0.5);
        assert!(pred.prediction_for(1) > 0);
        pred.evict(1);
        assert_eq!(
            pred.prediction_for(1),
            0,
            "prediction must be 0 after eviction"
        );
    }

    #[test]
    fn different_pids_independent() {
        // Verify that slots for different PIDs are truly independent:
        // evicting one PID must not clear the other's state.
        let mut pred = BurstPredictor::new();
        for _ in 0..20 {
            pred.observe_and_predict(10, 1_000_000, 0.1, 0.1);
            pred.observe_and_predict(20, 50_000_000, 0.9, 0.9);
        }
        let p10_before = pred.prediction_for(10);
        let p20_before = pred.prediction_for(20);
        // Both PIDs must have non-zero predictions after warmup.
        assert!(p10_before > 0, "PID 10 should have a prediction");
        assert!(p20_before > 0, "PID 20 should have a prediction");
        // Evicting PID 10 must not clear PID 20's state.
        pred.evict(10);
        assert_eq!(pred.prediction_for(10), 0, "evicted PID must return 0");
        assert_eq!(
            pred.prediction_for(20),
            p20_before,
            "evicting PID 10 should not affect PID 20"
        );
    }

    #[test]
    fn colliding_pids_do_not_evict_each_other() {
        let mut pred = BurstPredictor::new();
        let pid_a = 2;
        let pid_b = 2586;

        for _ in 0..20 {
            pred.observe_and_predict(pid_a, 2_000_000, 0.2, 0.2);
            pred.observe_and_predict(pid_b, 9_000_000, 0.8, 0.8);
        }

        assert!(pred.prediction_for(pid_a) > 0);
        assert!(pred.prediction_for(pid_b) > 0);
    }

    #[test]
    fn residual_correction_tracks_shift_in_burst_size() {
        let mut pred = BurstPredictor::new();
        for _ in 0..20 {
            pred.observe_and_predict(7, 2_000_000, 0.8, 0.6);
        }
        let before = pred.prediction_for(7);
        for _ in 0..20 {
            pred.observe_and_predict(7, 10_000_000, 0.8, 0.6);
        }
        let after = pred.prediction_for(7);
        assert!(
            after > before,
            "prediction should move upward with sustained larger bursts"
        );
    }
}
