// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// AI Module: KNN Task Classifier
//
// Dynamically labels tasks based on their runtime signatures.
// Labels are: Interactive, Compute, IoWait, RealTime, Unknown.
//
// Runs inside `ops.enqueue` — must not allocate on the hot path.

#![allow(dead_code)]

use std::collections::HashMap;

/// Labels assigned to tasks after classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskLabel {
    /// Latency-sensitive interactive tasks (games, HID, audio).
    Interactive,
    /// CPU-bound background tasks (compilers, encoders).
    Compute,
    /// Tasks blocked on I/O most of the time.
    IoWait,
    /// Realtime or near-realtime tasks (audio daemons, JACK).
    RealTime,
    /// Not yet classified.
    Unknown,
}

impl TaskLabel {
    /// Returns the time-slice multiplier hint for this label (relative to base slice).
    ///
    /// Values < 1.0 mean shorter slices (better for interactive).
    /// Values > 1.0 mean longer slices (better for throughput).
    pub fn slice_multiplier(self) -> f64 {
        match self {
            TaskLabel::Interactive => 0.5,
            TaskLabel::RealTime => 0.25,
            TaskLabel::IoWait => 0.75,
            TaskLabel::Compute => 2.0,
            TaskLabel::Unknown => 1.0,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TaskLabel::Interactive => "Interactive",
            TaskLabel::Compute => "Compute",
            TaskLabel::IoWait => "I/O Wait",
            TaskLabel::RealTime => "RealTime",
            TaskLabel::Unknown => "Unknown",
        }
    }
}

/// A compact feature vector for one observed task scheduling event.
#[derive(Debug, Clone, Copy)]
pub struct TaskFeatures {
    /// Fraction of time the task was runnable but not running (0..1).
    pub runnable_ratio: f32,
    /// CPU time consumed per scheduling event (normalised, 0..1).
    pub cpu_intensity: f32,
    /// Ratio of exec_runtime to total observed lifetime (0..1).
    pub exec_ratio: f32,
    /// Priority weight normalised to 0..1 (weight / 10000).
    pub weight_norm: f32,
    /// Number of CPUs allowed (normalised by total online CPUs).
    pub cpu_affinity: f32,
}

impl TaskFeatures {
    /// Euclidean distance in feature space.
    pub fn distance(&self, other: &TaskFeatures) -> f32 {
        let d0 = self.runnable_ratio - other.runnable_ratio;
        let d1 = self.cpu_intensity - other.cpu_intensity;
        let d2 = self.exec_ratio - other.exec_ratio;
        let d3 = self.weight_norm - other.weight_norm;
        let d4 = self.cpu_affinity - other.cpu_affinity;
        (d0 * d0 + d1 * d1 + d2 * d2 + d3 * d3 + d4 * d4).sqrt()
    }
}

/// A labelled sample in the training window.
#[derive(Debug, Clone, Copy)]
struct Sample {
    features: TaskFeatures,
    label: TaskLabel,
}

/// Online K-Nearest Neighbours task classifier.
///
/// Keeps a bounded sliding window of `WINDOW_SIZE` labelled samples.
/// Self-labels new observations via rule-based heuristics, then refines
/// future classifications via KNN voting.
///
/// Window replacement strategy: oldest sample is evicted (ring buffer).
pub struct KnnClassifier {
    window: Box<[Option<Sample>]>,
    head: usize,
    capacity: usize,
    k: usize,
    /// Per-PID cached label to avoid re-classifying every enqueue.
    pid_cache: HashMap<i32, TaskLabel>,
}

const WINDOW_SIZE: usize = 512;
const K_NEIGHBOURS: usize = 5;

impl KnnClassifier {
    pub fn new() -> Self {
        Self {
            window: vec![None; WINDOW_SIZE].into_boxed_slice(),
            head: 0,
            capacity: WINDOW_SIZE,
            k: K_NEIGHBOURS,
            pid_cache: HashMap::with_capacity(1024),
        }
    }

    /// Classify a task given its feature vector.
    ///
    /// First performs KNN vote over current window;
    /// falls back to heuristic rule if window is sparse.
    pub fn classify(&self, features: &TaskFeatures) -> TaskLabel {
        let filled: Vec<&Sample> = self.window.iter().flatten().collect();

        if filled.len() < self.k {
            return self.heuristic_classify(features);
        }

        // Compute distances and collect k nearest.
        let mut distances: Vec<(f32, TaskLabel)> = filled
            .iter()
            .map(|s| (s.features.distance(features), s.label))
            .collect();

        distances.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        // Majority vote among k nearest neighbours.
        let mut votes: HashMap<u8, usize> = HashMap::new();
        for (_, label) in distances.iter().take(self.k) {
            *votes.entry(label_to_u8(*label)).or_insert(0) += 1;
        }

        votes
            .into_iter()
            .max_by_key(|(_, v)| *v)
            .map(|(l, _)| u8_to_label(l))
            .unwrap_or(TaskLabel::Unknown)
    }

    /// Insert a new labelled observation into the sliding window.
    pub fn feed(&mut self, features: TaskFeatures, label: TaskLabel) {
        self.window[self.head] = Some(Sample { features, label });
        self.head = (self.head + 1) % self.capacity;
    }

    /// Classify and immediately feed the result back into the window.
    pub fn classify_and_learn(&mut self, pid: i32, features: TaskFeatures) -> TaskLabel {
        let label = self.classify(&features);
        self.feed(features, label);
        self.pid_cache.insert(pid, label);
        label
    }

    /// Retrieve a previously computed label for a PID (fast path).
    pub fn cached_label(&self, pid: i32) -> TaskLabel {
        self.pid_cache
            .get(&pid)
            .copied()
            .unwrap_or(TaskLabel::Unknown)
    }

    /// Remove a PID from the cache (called on task exit).
    pub fn evict(&mut self, pid: i32) {
        self.pid_cache.remove(&pid);
    }

    /// Heuristic rule-based fallback classifer.
    fn heuristic_classify(&self, f: &TaskFeatures) -> TaskLabel {
        // Real-time: high-priority weight (e.g., SCHED_FIFO tasks have weight > 0.95).
        if f.weight_norm > 0.95 {
            return TaskLabel::RealTime;
        }
        // Compute: burning through the full assigned slice AND has been running
        // continuously without sleeping for many slices (high run streak).
        if f.cpu_intensity > 0.85 && f.runnable_ratio > 0.6 {
            return TaskLabel::Compute;
        }
        // I/O-bound: very low CPU consumption per scheduling event.
        if f.cpu_intensity < 0.15 {
            return TaskLabel::IoWait;
        }
        // Interactive: used only a small-to-moderate fraction of its assigned slice,
        // or just woke from a sleep (high freshness ratio → exec_runtime ≈ burst_ns).
        if f.cpu_intensity < 0.5 || f.exec_ratio > 0.4 {
            return TaskLabel::Interactive;
        }
        TaskLabel::Unknown
    }
}

fn label_to_u8(l: TaskLabel) -> u8 {
    match l {
        TaskLabel::Interactive => 0,
        TaskLabel::Compute => 1,
        TaskLabel::IoWait => 2,
        TaskLabel::RealTime => 3,
        TaskLabel::Unknown => 4,
    }
}

fn u8_to_label(v: u8) -> TaskLabel {
    match v {
        0 => TaskLabel::Interactive,
        1 => TaskLabel::Compute,
        2 => TaskLabel::IoWait,
        3 => TaskLabel::RealTime,
        _ => TaskLabel::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feat(cpu: f32, io: f32, exec: f32) -> TaskFeatures {
        TaskFeatures {
            runnable_ratio: io,
            cpu_intensity: cpu,
            exec_ratio: exec,
            weight_norm: 0.01,
            cpu_affinity: 1.0,
        }
    }

    #[test]
    fn heuristic_compute() {
        let clf = KnnClassifier::new();
        // cpu_intensity=0.9 (burns full slice), runnable_ratio=0.8 (long run streak),
        // exec_ratio≈0 (burst << exec_runtime → spinning without sleeping)
        let f = feat(0.9, 0.8, 0.001);
        assert_eq!(clf.classify(&f), TaskLabel::Compute);
    }

    #[test]
    fn heuristic_interactive() {
        let clf = KnnClassifier::new();
        // cpu_intensity=0.3 (used only 30% of slice), exec_ratio=0.95 (just woke from sleep)
        let f = feat(0.3, 0.02, 0.95);
        assert_eq!(clf.classify(&f), TaskLabel::Interactive);
    }

    #[test]
    fn knn_vote_after_warmup() {
        let mut clf = KnnClassifier::new();
        // Feed compute samples using feature values consistent with new semantics.
        for _ in 0..20 {
            clf.feed(feat(0.9, 0.8, 0.001), TaskLabel::Compute);
        }
        let result = clf.classify(&feat(0.88, 0.75, 0.002));
        assert_eq!(result, TaskLabel::Compute);
    }
}
