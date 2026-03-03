// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// AI Module: Bayesian Reputation Engine
//
// Maintains a Beta-distribution prior over the "trust" of each PID.
//
//   Trust ~ Beta(α, β)
//
//   α = 1 + #cooperative_events   (tasks that sleep, yield gracefully, finish in slice)
//   β = 1 + #adversarial_events   (tasks that burn full slices, fork-bomb, yield-spin)
//
// The posterior mean is used as the trust score:
//
//   E[Trust] = α / (α + β)

#![allow(dead_code)]
//
// Low-trust processes (score < TRUST_THRESHOLD) are quarantined to restricted
// cores and assigned shorter time slices.
//
// Runs inside `ops.exit` (update on exit), with reads on `ops.enqueue`.

use std::collections::HashMap;

/// Threshold below which a process is considered low-trust and quarantined.
pub const TRUST_THRESHOLD: f64 = 0.35;

/// A Beta(α, β) distribution tracking cooperative vs adversarial behaviour.
#[derive(Debug, Clone)]
pub struct TrustPrior {
    /// Pseudocount for cooperative events.
    pub alpha: f64,
    /// Pseudocount for adversarial events.
    pub beta:  f64,
    /// Human-readable name (comm) for display in TUI.
    pub comm:  String,
}

impl TrustPrior {
    pub fn new(comm: &str) -> Self {
        // Non-informative prior: Beta(1, 1) = Uniform.
        Self {
            alpha: 1.0,
            beta:  1.0,
            comm:  comm.to_owned(),
        }
    }

    /// Posterior mean trust score in (0, 1).
    #[inline]
    pub fn mean(&self) -> f64 {
        self.alpha / (self.alpha + self.beta)
    }

    /// 95th-percentile lower bound (Wilson-like approximation).
    ///
    /// This gives a conservative trust estimate used for quarantine decisions.
    #[inline]
    pub fn lower_bound(&self) -> f64 {
        let n = self.alpha + self.beta;
        let p = self.mean();
        let z = 1.645; // 90% one-sided z-score
        let lo = p - z * (p * (1.0 - p) / n).sqrt();
        lo.max(0.0)
    }

    /// Record a cooperative event (task cooperated with the scheduler).
    ///
    /// Examples: voluntarily slept, used less than its full slice, finished cleanly.
    #[inline]
    pub fn reward(&mut self, weight: f64) {
        self.alpha += weight;
    }

    /// Record an adversarial event (task was hostile to the scheduler).
    ///
    /// Examples: burned full slice repeatedly, forked excessively, yield-spun.
    #[inline]
    pub fn penalise(&mut self, weight: f64) {
        self.beta += weight;
    }
}

/// Events observed at task-exit time that inform the trust update.
#[derive(Debug, Default, Clone)]
pub struct ExitObservation {
    /// Task used less than 50% of its last assigned slice → cooperative.
    pub slice_underrun: bool,
    /// Task was forcibly preempted (burned full slice).
    pub preempted: bool,
    /// Task exited cleanly (no abnormal flags).
    pub clean_exit: bool,
    /// Task was flagged by anti-cheat during its lifetime.
    pub cheat_flagged: bool,
    /// Number of child forks spawned.
    pub fork_count: u64,
    /// Number of involuntary context switches.
    pub involuntary_ctx_sw: u64,
}

/// Manages trust scores for all observed PIDs.
pub struct ReputationEngine {
    /// PID → trust prior.
    priors: HashMap<i32, TrustPrior>,
    /// TGID → aggregated trust (for thread groups).
    tgid_trust: HashMap<i32, f64>,
}

impl ReputationEngine {
    pub fn new() -> Self {
        Self {
            priors:    HashMap::with_capacity(1024),
            tgid_trust: HashMap::with_capacity(256),
        }
    }

    /// Get or create the trust prior for a PID.
    pub fn get_or_create(&mut self, pid: i32, comm: &str) -> &mut TrustPrior {
        self.priors.entry(pid).or_insert_with(|| TrustPrior::new(comm))
    }

    /// Trust score for a PID in (0, 1).  Returns 1.0 (full trust) for unknown PIDs.
    pub fn trust_score(&self, pid: i32) -> f64 {
        self.priors.get(&pid).map(|p| p.mean()).unwrap_or(1.0)
    }

    /// Conservative lower-bound trust score (used for quarantine decisions).
    pub fn trust_lower_bound(&self, pid: i32) -> f64 {
        self.priors.get(&pid).map(|p| p.lower_bound()).unwrap_or(1.0)
    }

    /// Whether a PID should currently be quarantined.
    pub fn is_quarantined(&self, pid: i32) -> bool {
        self.trust_lower_bound(pid) < TRUST_THRESHOLD
    }

    /// Update the reputation of a PID based on its exit observation.
    ///
    /// Called from `ops.exit`.
    pub fn update_on_exit(&mut self, pid: i32, tgid: i32, obs: &ExitObservation, comm: &str) {
        let prior = self.priors.entry(pid).or_insert_with(|| TrustPrior::new(comm));

        // Cooperative signals.
        if obs.slice_underrun {
            prior.reward(0.5);
        }
        if obs.clean_exit && !obs.cheat_flagged {
            prior.reward(0.3);
        }

        // Adversarial signals.
        if obs.preempted {
            prior.penalise(0.4);
        }
        if obs.cheat_flagged {
            prior.penalise(2.0);
        }
        if obs.fork_count > 50 {
            // Excessive forking.
            let excess = ((obs.fork_count - 50) as f64 * 0.05).min(3.0);
            prior.penalise(excess);
        }
        if obs.involuntary_ctx_sw > 1000 {
            // Lots of involuntary switches suggests spinning.
            prior.penalise(0.5);
        }

        // Propagate to TGID aggregation (simple average of recent thread scores).
        let score = prior.mean();
        self.tgid_trust
            .entry(tgid)
            .and_modify(|v| *v = 0.8 * *v + 0.2 * score)
            .or_insert(score);
    }

    /// Time-slice multiplier derived from trust score.
    ///
    /// Trusted tasks get full or boosted slices.
    /// Quarantined tasks get shrunk slices.
    pub fn slice_factor(&self, pid: i32) -> f64 {
        let t = self.trust_score(pid);
        // Map trust (0..1) → slice_factor (0.25 .. 1.5).
        0.25 + t * 1.25
    }

    /// Evict a PID's prior (called after a process fully exits).
    pub fn evict(&mut self, pid: i32) {
        self.priors.remove(&pid);
    }

    /// Sorted (ascending trust) list of distrusted PIDs for the TUI wall-of-shame.
    pub fn wall_of_shame(&self, limit: usize) -> Vec<(i32, f64, &str)> {
        let mut entries: Vec<(i32, f64, &str)> = self.priors.iter()
            .map(|(&pid, p)| (pid, p.mean(), p.comm.as_str()))
            .filter(|(_, score, _)| *score < TRUST_THRESHOLD)
            .collect();
        entries.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        entries.truncate(limit);
        entries
    }

    /// All known PIDs and their trust scores (for TUI sparklines).
    pub fn all_scores(&self) -> impl Iterator<Item = (i32, f64, &str)> {
        self.priors.iter().map(|(&pid, p)| (pid, p.mean(), p.comm.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_prior_mean() {
        let p = TrustPrior::new("test");
        assert_eq!(p.mean(), 0.5);
    }

    #[test]
    fn reward_increases_trust() {
        let mut p = TrustPrior::new("test");
        p.reward(10.0);
        assert!(p.mean() > 0.5);
    }

    #[test]
    fn penalise_decreases_trust() {
        let mut p = TrustPrior::new("test");
        p.penalise(10.0);
        assert!(p.mean() < 0.5);
    }

    #[test]
    fn quarantine_on_cheat_flag() {
        let mut eng = ReputationEngine::new();
        let obs = ExitObservation {
            cheat_flagged: true,
            preempted:     true,
            ..Default::default()
        };
        // Apply multiple adversarial exits to cross the threshold.
        for _ in 0..15 {
            eng.update_on_exit(99, 99, &obs, "badproc");
        }
        assert!(eng.is_quarantined(99));
    }
}
