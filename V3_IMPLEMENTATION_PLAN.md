# Cognis v3 Implementation Plan

This file turns the v3 design draft into a concrete branch-local checklist.

## Phase 1: Pick The Base Shape

Target outcome:

- a small, understandable BPF fast path
- quiet Rust control plane
- clear liveness story before any Cognis-specific hierarchy is added

Selected base ingredients:

- `scx_cake`
  - common immediate path
  - quiet userspace runtime model
- `scx_beerland`
  - stealable scheduler-owned per-CPU queue behavior
- `scx_lavd`
  - anti-stranding ideas for a later phase
  - widening behavior for pressured paths after the small base is stable

Decision:

- phase 1 should use a `cake`-style immediate path plus a `beerland`-style
  stealable deferred tier
- `lavd`-style anti-stranding logic should be introduced only after that
  smaller base survives repeated local `cachyos-benchmarker` runs

## Phase 2: Reintroduce Cognis-Specific Behavior

Candidate additions after the base survives local stress:

- profile split:
  - `desktop`
  - `server`
- bounded wake-credit / deadline ordering
- locality-aware wider spill behavior
- desktop responsiveness heuristics

Each addition should have:

- a local benchmark or workload reason
- a rollback path if it regresses liveness

## Phase 3: Validation

Before any new upstream PR:

- repeated `cachyos-benchmarker` runs with no watchdog stall
- desktop remains usable under heavy saturation
- recovery path still fails open cleanly
- README wording matches what the branch actually proves

## Immediate Branch Tasks

- [x] compare `cake`, `beerland`, and `lavd` hot paths function by function
- [x] choose the smallest base runtime model
- [x] write the phase-1 target hot-path map in code-oriented terms
- [ ] strip the current branch down to that base
- [ ] rerun local benchmark repros before reintroducing any Cognis-specific
      policy
