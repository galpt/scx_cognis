// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// Scheduling AI modules — aggregates all sub-modules:
//   - Heuristic task classifier (deterministic rules)
//   - O(1) bitmask CPU selector (replaces A* load balancer)
//   - Elman RNN burst predictor (fixed-weight recurrent model, zero-alloc table)
//   - Trust table (combined reputation + anomaly detection, zero-alloc)
//   - Q-learning policy controller (tabular reinforcement learning)

pub mod burst_predictor;
pub mod classifier;
pub mod cpu_selector;
pub mod policy;
pub mod trust;

// Re-export the most commonly used types for convenience.
pub use burst_predictor::BurstPredictor;
pub use classifier::{HeuristicClassifier, TaskFeatures, TaskLabel};
pub use cpu_selector::{CoreType, CpuSelector, CpuState};
pub use policy::{PolicyController, SchedulerSignal};
pub use trust::{ExitObservation, TrustTable, SHAME_MAX};
