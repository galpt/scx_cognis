// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// Scheduling support modules — aggregates all sub-modules:
//   - Heuristic task classifier (deterministic rules)
//   - O(1) bitmask CPU selector (topology model; dispatch delegates to kernel via bpf.select_cpu)
//   - Elman RNN burst predictor (fixed-weight recurrent model, zero-alloc table)
//   - Trust table (combined reputation + anomaly detection, zero-alloc)
//   - Deterministic slice controller (load-driven, zero-alloc)

pub mod burst_predictor;
pub mod classifier;
pub mod cpu_selector;
pub mod policy;
pub mod slice_autopilot;
pub mod trust;

// Re-export the most commonly used types for convenience.
pub use burst_predictor::BurstPredictor;
pub use classifier::{HeuristicClassifier, TaskFeatures, TaskLabel};
pub use cpu_selector::{CoreType as CpuCoreType, CpuSelector, CpuState};
pub use policy::SliceController;
pub use slice_autopilot::Autopilot;
pub use trust::{ExitObservation, TrustTable, SHAME_MAX};
