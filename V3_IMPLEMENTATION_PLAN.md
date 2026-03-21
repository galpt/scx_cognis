# Cognis v3 Implementation Plan

This file turns the v3 design draft into a concrete branch-local checklist.

## Phase 1: Pick The Base Shape

Target outcome:

- a small, understandable BPF fast path
- quiet Rust control plane
- clear liveness story before any Cognis-specific hierarchy is added

Candidate base ingredients:

- `scx_cake`
  - common immediate path
  - quiet userspace runtime model
- `scx_beerland`
  - stealable scheduler-owned per-CPU queue behavior
- `scx_lavd`
  - anti-stranding ideas
  - widening behavior for pressured paths

Initial decision rule:

- prefer the smallest base that survives repeated local `cachyos-benchmarker`
  runs
- only add more policy after that smaller base is stable

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

- [ ] compare `cake`, `beerland`, and `lavd` hot paths function by function
- [ ] choose the smallest base runtime model
- [ ] strip the current branch down to that base
- [ ] rerun local benchmark repros before reintroducing any Cognis-specific
      policy
