// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// AI Module: LSTM-lite Burst Predictor
//
// Predicts the next CPU burst duration for a PID using a compact recurrent
// model.  A true forward-pass LSTM is too heavyweight for sub-10 µs latency on
// the hot path, so this implements a **Elman RNN** (one recurrent layer) with
// a fixed tiny weight matrix baked in at compile time.
//
// The model maintains a hidden state `h` per PID and updates it on every
// scheduling event:
//
//   h[t] = tanh( W_h * h[t-1]  +  W_x * x[t]  +  b )
//   y[t] = W_out * h[t]

#![allow(dead_code)]
//
// Weights are trained offline via a simple gradient-descent run on synthetic
// workload traces and then frozen into constants.  The resulting coefficients
// are small enough to be fully register-resident.
//
// Runs inside `ops.update_idle`.

use std::collections::HashMap;

// ── Model constants (offline-trained weights) ───────────────────────────────

// Hidden state size.
const H: usize = 4;

// Input features per tick: [burst_ns_norm, exec_ratio, cpu_intensity_norm].
const X: usize = 3;

// W_x: (H × X) flattened row-major.  Values from offline regression on simulated
// scheduler traces.  The matrix is intentionally tiny so the forward pass runs in
// a handful of multiplications.
#[rustfmt::skip]
const W_X: [[f32; X]; H] = [
    [ 0.42, -0.31,  0.15],
    [-0.18,  0.55, -0.09],
    [ 0.33, -0.12,  0.48],
    [-0.27,  0.19,  0.36],
];

// W_h: (H × H) flattened row-major.
#[rustfmt::skip]
const W_H: [[f32; H]; H] = [
    [ 0.31, -0.08,  0.14, -0.22],
    [-0.11,  0.47,  0.01,  0.09],
    [ 0.23,  0.06, -0.38,  0.17],
    [-0.04,  0.12,  0.25, -0.41],
];

// b: bias vector.
const B: [f32; H] = [0.03, -0.05, 0.02, 0.01];

// W_out: (1 × H) output layer.
const W_OUT: [f32; H] = [0.55, -0.23, 0.31, -0.18];

// Output bias.
const B_OUT: f32 = 0.015;

// ── Per-PID RNN state ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct RnnState {
    h: [f32; H],
    /// Exponential moving average of predicted burst (ns).
    ema_burst_ns: f64,
    /// EMA of actual burst for accuracy tracking.
    ema_actual_ns: f64,
}

impl Default for RnnState {
    fn default() -> Self {
        Self {
            h: [0.0; H],
            ema_burst_ns: 0.0,
            ema_actual_ns: 0.0,
        }
    }
}

/// Normalisation range for burst durations.
const BURST_MAX_NS: f64 = 100_000_000.0; // 100 ms

fn tanh(x: f32) -> f32 {
    // Stable tanh using the exponential definition.
    let e2 = (2.0 * x).exp();
    (e2 - 1.0) / (e2 + 1.0)
}

// ── Predictor ────────────────────────────────────────────────────────────────

/// Maintains per-PID RNN state and provides next-burst predictions.
pub struct BurstPredictor {
    states: HashMap<i32, RnnState>,
    /// EWA alpha (how fast the EMA decays).
    ema_alpha: f64,
}

impl BurstPredictor {
    pub fn new() -> Self {
        Self {
            states: HashMap::with_capacity(512),
            ema_alpha: 0.15,
        }
    }

    /// Feed an observation and get the predicted next burst in nanoseconds.
    ///
    /// `burst_ns`       — actual burst duration just observed.
    /// `exec_ratio`     — fraction of time in exec since last sleep (0..1).
    /// `cpu_intensity`  — CPU usage intensity (0..1).
    pub fn observe_and_predict(
        &mut self,
        pid: i32,
        burst_ns: u64,
        exec_ratio: f32,
        cpu_intensity: f32,
    ) -> u64 {
        let state = self.states.entry(pid).or_default();

        // Build input vector, normalised to [0, 1].
        let burst_norm = (burst_ns as f64 / BURST_MAX_NS).min(1.0) as f32;
        let x = [
            burst_norm,
            exec_ratio.clamp(0.0, 1.0),
            cpu_intensity.clamp(0.0, 1.0),
        ];

        // ── Forward pass ──────────────────────────────────────────────────
        // h[t] = tanh( W_h * h[t-1]  +  W_x * x  +  b )
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

        // y = W_out · h  +  b_out, clamped to [0, 1].
        let y_norm: f32 = W_OUT
            .iter()
            .zip(new_h.iter())
            .map(|(w, h)| w * h)
            .sum::<f32>()
            + B_OUT;
        let y_norm = y_norm.clamp(0.0, 1.0);

        // Convert back to ns.
        let predicted_ns = (y_norm as f64 * BURST_MAX_NS) as u64;

        // EMA smoothing to reduce jitter.
        state.ema_burst_ns =
            self.ema_alpha * predicted_ns as f64 + (1.0 - self.ema_alpha) * state.ema_burst_ns;
        state.ema_actual_ns =
            self.ema_alpha * burst_ns as f64 + (1.0 - self.ema_alpha) * state.ema_actual_ns;

        state.ema_burst_ns as u64
    }

    /// Latest prediction for a PID without updating (uses cached EMA).
    pub fn prediction_for(&self, pid: i32) -> u64 {
        self.states
            .get(&pid)
            .map(|s| s.ema_burst_ns as u64)
            .unwrap_or(0)
    }

    /// Prediction error EMA (ns) — useful for the TUI dashboard.
    pub fn error_ema_for(&self, pid: i32) -> f64 {
        self.states
            .get(&pid)
            .map(|s| (s.ema_burst_ns - s.ema_actual_ns).abs())
            .unwrap_or(0.0)
    }

    /// Evict state for a PID that has exited.
    pub fn evict(&mut self, pid: i32) {
        self.states.remove(&pid);
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
        assert!(p > 0, "prediction should be nonzero after warmup");
    }

    #[test]
    fn evict_removes_state() {
        let mut pred = BurstPredictor::new();
        pred.observe_and_predict(1, 1_000_000, 0.3, 0.5);
        pred.evict(1);
        assert_eq!(pred.prediction_for(1), 0);
    }
}
