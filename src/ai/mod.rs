// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// AI inference engine — aggregates all sub-modules.

pub mod anomaly;
pub mod burst_predictor;
pub mod classifier;
pub mod load_balancer;
pub mod policy;
pub mod reputation;

// Re-export the most commonly used types for convenience.
pub use anomaly::AntiCheatEngine;
pub use burst_predictor::BurstPredictor;
pub use classifier::{HeuristicClassifier, TaskFeatures, TaskLabel};
pub use load_balancer::{AStarLoadBalancer, CoreType, CpuState};
pub use policy::{PolicyController, SchedulerSignal};
pub use reputation::{ExitObservation, ReputationEngine, TRUST_THRESHOLD};
