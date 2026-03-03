// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// AI Module: Isolation Forest Anti-Cheat Engine
//
// Detects scheduler-abusing processes using an approximated Isolation Forest.
// Anomalous processes (fork-bombers, yield-spinners, etc.) are flagged for
// quarantine to restricted cores.
//
// Runs inside `ops.tick` — bounded O(n_trees * depth) cost.

#![allow(dead_code)]

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use std::collections::HashMap;

/// Per-TGID behavioural fingerprint updated every tick.
#[derive(Debug, Default, Clone)]
pub struct ProcessStats {
    /// Total number of child processes spawned (fork/clone).
    pub fork_count: u64,
    /// Number of voluntary yields / sched_yield calls observed.
    pub yield_count: u64,
    /// Total CPU time consumed (ns).
    pub cpu_time_ns: u64,
    /// Observation window length (ns) since stats were last reset.
    pub window_ns: u64,
    /// Number of context switches.
    pub ctx_switches: u64,
}

impl ProcessStats {
    /// Derive a compact 4-dimensional feature vector from raw stats.
    ///
    /// Returns `[fork_rate, yield_rate, cpu_fraction, switch_rate]` all in [0, 1]
    /// capped at reasonable maximums to keep the space bounded.
    pub fn to_features(&self) -> [f32; 4] {
        let window_s = (self.window_ns.max(1) as f64) / 1e9;
        let fork_rate = ((self.fork_count as f64) / window_s).min(1000.0) as f32 / 1000.0;
        let yield_rate = ((self.yield_count as f64) / window_s).min(5000.0) as f32 / 5000.0;
        let cpu_fraction = (self.cpu_time_ns as f64 / self.window_ns.max(1) as f64).min(1.0) as f32;
        let switch_rate = ((self.ctx_switches as f64) / window_s).min(10000.0) as f32 / 10000.0;
        [fork_rate, yield_rate, cpu_fraction, switch_rate]
    }
}

// ── Isolation Tree ──────────────────────────────────────────────────────────

const FEATURE_DIM: usize = 4;

#[derive(Debug)]
enum ITreeNode {
    Internal {
        feature: usize,
        threshold: f32,
        left: Box<ITreeNode>,
        right: Box<ITreeNode>,
    },
    Leaf {
        size: usize,
    },
}

/// Build one random isolation tree on a sample of data.
fn build_tree(
    data: &[[f32; FEATURE_DIM]],
    depth: usize,
    max_depth: usize,
    rng: &mut SmallRng,
) -> ITreeNode {
    if data.len() <= 1 || depth >= max_depth {
        return ITreeNode::Leaf { size: data.len() };
    }

    // Pick a random feature and a random split within [min, max].
    let feat = rng.gen_range(0..FEATURE_DIM);

    let min_val = data.iter().map(|r| r[feat]).fold(f32::INFINITY, f32::min);
    let max_val = data
        .iter()
        .map(|r| r[feat])
        .fold(f32::NEG_INFINITY, f32::max);

    if (max_val - min_val).abs() < 1e-9 {
        return ITreeNode::Leaf { size: data.len() };
    }

    let threshold = rng.gen_range(min_val..max_val);

    let (left_data, right_data): (Vec<_>, Vec<_>) = data.iter().partition(|r| r[feat] <= threshold);

    ITreeNode::Internal {
        feature: feat,
        threshold,
        left: Box::new(build_tree(&left_data, depth + 1, max_depth, rng)),
        right: Box::new(build_tree(&right_data, depth + 1, max_depth, rng)),
    }
}

/// Compute path length for a single sample through one tree.
fn path_length(node: &ITreeNode, sample: &[f32; FEATURE_DIM], depth: usize) -> f32 {
    match node {
        ITreeNode::Leaf { size } => depth as f32 + adjustment(*size),
        ITreeNode::Internal {
            feature,
            threshold,
            left,
            right,
        } => {
            if sample[*feature] <= *threshold {
                path_length(left, sample, depth + 1)
            } else {
                path_length(right, sample, depth + 1)
            }
        }
    }
}

/// Expected path-length normalisation factor (Euler's constant approximation).
fn adjustment(n: usize) -> f32 {
    if n <= 1 {
        return 0.0;
    }
    let n = n as f32;
    2.0 * (n - 1.0).ln() + std::f32::consts::E - 2.0 * (n - 1.0) / n
}

// ── Isolation Forest ────────────────────────────────────────────────────────

const N_TREES: usize = 32;
const SAMPLE_SIZE: usize = 128;
const MAX_DEPTH: usize = 8; // ceil(log2(SAMPLE_SIZE)) ≈ 7

/// Lightweight Isolation Forest for real-time anomaly detection.
///
/// The anomaly score is in (0, 1).  Scores above `ANOMALY_THRESHOLD` indicate
/// likely scheduler abuse.
pub const ANOMALY_THRESHOLD: f32 = 0.65;

pub struct IsolationForest {
    trees: Vec<ITreeNode>,
    n_train: usize,
}

impl IsolationForest {
    pub fn new() -> Self {
        Self {
            trees: Vec::new(),
            n_train: 0,
        }
    }

    /// (Re-)train the forest on `samples`.  Call periodically (e.g. every 1 s).
    pub fn fit(&mut self, samples: &[[f32; FEATURE_DIM]]) {
        if samples.is_empty() {
            return;
        }

        let n = samples.len().min(SAMPLE_SIZE);
        self.n_train = n;

        let mut rng = SmallRng::seed_from_u64(0x00C0_FFEE_DEAD);
        let subset: Vec<[f32; FEATURE_DIM]> = samples[..n].to_vec();

        self.trees = (0..N_TREES)
            .map(|_| build_tree(&subset, 0, MAX_DEPTH, &mut rng))
            .collect();
    }

    /// Compute anomaly score for `sample` ∈ [0, 1].
    ///
    /// Higher score = more anomalous.  Returns 0.5 before first training pass.
    pub fn score(&self, sample: &[f32; FEATURE_DIM]) -> f32 {
        if self.trees.is_empty() || self.n_train == 0 {
            return 0.5;
        }

        let avg_path: f32 = self
            .trees
            .iter()
            .map(|t| path_length(t, sample, 0))
            .sum::<f32>()
            / self.trees.len() as f32;

        let c = adjustment(self.n_train);
        if c < 1e-9 {
            return 0.5;
        }

        // Anomaly score: 2^(-E[h(x)] / c)
        // Invert so higher score = more anomalous.
        // Score is close to 0.5 for typical points, closer to 1 for anomalies.
        2.0_f32.powf(-avg_path / c)
    }

    pub fn is_anomaly(&self, sample: &[f32; FEATURE_DIM]) -> bool {
        self.score(sample) > ANOMALY_THRESHOLD
    }
}

// ── Anti-Cheat Engine ───────────────────────────────────────────────────────

/// Manages per-TGID statistics and runs anomaly detection via `IsolationForest`.
pub struct AntiCheatEngine {
    forest: IsolationForest,
    stats: HashMap<i32, ProcessStats>,
    /// TGIDs currently flagged as cheaters.
    flagged: HashMap<i32, u64>,
    tick_count: u64,
    /// Training data buffer (ring buffer of recent samples).
    train_buf: Vec<[f32; FEATURE_DIM]>,
    train_max: usize,
    train_head: usize,
}

const TRAIN_BUF_SIZE: usize = 512;
const RETRAIN_EVERY: u64 = 500; // ticks

impl AntiCheatEngine {
    pub fn new() -> Self {
        Self {
            forest: IsolationForest::new(),
            stats: HashMap::with_capacity(256),
            flagged: HashMap::new(),
            tick_count: 0,
            train_buf: vec![[0.0; FEATURE_DIM]; TRAIN_BUF_SIZE],
            train_max: TRAIN_BUF_SIZE,
            train_head: 0,
        }
    }

    /// Update stats for a TGID.  Called every scheduler tick.
    pub fn update(
        &mut self,
        tgid: i32,
        forks: u64,
        yields: u64,
        cpu_ns: u64,
        ctx_sw: u64,
        window_ns: u64,
    ) {
        let entry = self.stats.entry(tgid).or_default();
        entry.fork_count = forks;
        entry.yield_count = yields;
        entry.cpu_time_ns = cpu_ns;
        entry.ctx_switches = ctx_sw;
        entry.window_ns = window_ns;
    }

    /// Tick — evaluate all tracked TGIDs.  Returns the list of newly-flagged TGIDs.
    pub fn tick(&mut self, now_ns: u64) -> Vec<i32> {
        self.tick_count += 1;

        // Collect feature vectors for all known TGIDs.
        let mut new_flags = Vec::new();
        let stats_snapshot: Vec<(i32, [f32; FEATURE_DIM])> = self
            .stats
            .iter()
            .map(|(&tgid, s)| (tgid, s.to_features()))
            .collect();

        // Possibly retrain.
        if self.tick_count.is_multiple_of(RETRAIN_EVERY) {
            let samples: Vec<[f32; FEATURE_DIM]> = stats_snapshot.iter().map(|(_, f)| *f).collect();
            // Also include historical buffer.
            let mut all = samples.clone();
            for s in &self.train_buf {
                if s.iter().any(|v| *v > 0.0) {
                    all.push(*s);
                }
            }
            self.forest.fit(&all);
        }

        for (tgid, feats) in stats_snapshot {
            // Push to training ring buffer.
            self.train_buf[self.train_head] = feats;
            self.train_head = (self.train_head + 1) % self.train_max;

            if self.forest.is_anomaly(&feats) {
                if let std::collections::hash_map::Entry::Vacant(e) = self.flagged.entry(tgid) {
                    e.insert(now_ns);
                    new_flags.push(tgid);
                }
            } else {
                // Clear flag if process normalised.
                self.flagged.remove(&tgid);
            }
        }

        new_flags
    }

    pub fn is_flagged(&self, tgid: i32) -> bool {
        self.flagged.contains_key(&tgid)
    }

    /// Anomaly score for a TGID (0..1, higher = worse).
    pub fn score_of(&self, tgid: i32) -> f32 {
        if let Some(s) = self.stats.get(&tgid) {
            self.forest.score(&s.to_features())
        } else {
            0.0
        }
    }

    /// All currently flagged TGIDs with their flagging timestamp.
    pub fn wall_of_shame(&self) -> &HashMap<i32, u64> {
        &self.flagged
    }

    /// Drop stats for a process that has exited.
    pub fn evict(&mut self, tgid: i32) {
        self.stats.remove(&tgid);
        self.flagged.remove(&tgid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anomaly_score_range() {
        let mut forest = IsolationForest::new();
        // Normal samples.
        let normals: Vec<[f32; 4]> = (0..100)
            .map(|i| {
                let v = (i as f32) / 100.0 * 0.3;
                [v, v * 0.5, v * 0.2, v * 0.1]
            })
            .collect();
        forest.fit(&normals);

        // Normal sample should have low score.
        let normal_score = forest.score(&[0.1, 0.05, 0.02, 0.01]);
        // Extreme outlier (fork-bomb pattern).
        let anomaly_score = forest.score(&[1.0, 0.9, 0.01, 0.8]);

        // Anomaly score should be higher than normal score.
        assert!(
            anomaly_score > normal_score,
            "anomaly={anomaly_score} normal={normal_score}"
        );
        // Both should be in (0, 1).
        assert!(normal_score > 0.0 && normal_score < 1.0);
        assert!(anomaly_score > 0.0 && anomaly_score < 1.0);
    }
}
