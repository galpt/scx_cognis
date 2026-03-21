# Cognis v3 Phase 1 Hot-Path Map

This note translates the v3 base decision into current-file, current-function
terms so the first refactor does not drift.

The target is a small, `cake`-style immediate path plus a `beerland`-style
stealable deferred tier.

## Current Files In Scope

- `main.bpf.c`
- `src/main.rs`
- `src/bpf.rs`

Phase 1 should avoid broad work outside those files until the hot path is
smaller and locally testable again.

## BPF Hot Path

### `cognis_select_cpu()`

Current role:

- validate `prev_cpu`
- skip work for the userspace scheduler task
- use `pick_idle_cpu()`
- direct-dispatch immediate idle placements

Phase 1 target:

- keep the current "pick an obvious idle CPU and direct-dispatch it" idea
- keep `direct_local_dispatch()`
- remove userspace-scheduler-specific branching from the common path once the
  default runtime no longer depends on it
- keep the callback focused on immediate placement only

Meaning:

- `select_cpu` should not decide wider hierarchy behavior
- if no clear immediate placement exists, return the chosen CPU and let
  `enqueue` place into the single deferred tier

### `cognis_enqueue()`

Current role:

- special-case userspace scheduler task
- special-case kthreads
- apply sticky-task logic
- attempt idle migration
- otherwise route into:
  - per-CPU deferred DSQ
  - LLC DSQ
  - node DSQ
  - shared DSQ

Phase 1 target:

- keep direct handling for immediate placements and simple kthread safety
- remove deep default overflow routing
- replace the current `overflow_dsq()` hierarchy choice with one scheduler-owned
  deferred tier
- keep the enqueue rule understandable enough to explain in one short paragraph

Meaning:

- ordinary busy-path tasks should not be sent through `CPU -> LLC -> node ->
  shared` in phase 1
- phase 1 enqueue should answer only:
  - can this task run immediately?
  - if not, which stealable deferred queue should own it?

### `cognis_dispatch()`

Current role:

- drain user ring buffer
- dispatch userspace scheduler task if pending
- compare:
  - local deferred CPU DSQ
  - LLC DSQ
  - node DSQ
  - shared DSQ
- attempt remote CPU / LLC / node steals
- optionally refill `prev`

Phase 1 target:

- remove deep multi-tier comparison from the default path
- first try:
  - local deferred queue
  - then remote eligible deferred queues
- keep refill behavior only if it remains simple and does not hide starvation
- keep this callback close to the `beerland` mental model:
  - local consume
  - remote drain
  - then let the CPU idle or continue safely

Meaning:

- `dispatch` should not be a mini scheduler hierarchy in phase 1
- it should primarily drain the one deferred model we selected

## BPF Support Helpers

### Keep For Phase 1

- `direct_local_dispatch()`
- `pick_idle_cpu()`
- basic `task_slice()` logic
- basic `task_dl()` only if still needed by the chosen deferred queue ordering

### Candidates To Remove Or Bypass In Phase 1

- `overflow_dsq()`
- `llc_spill_threshold()`
- `node_spill_threshold()`
- deep `steal_remote_llc()` / `steal_remote_node()` logic
- dispatch-progress guard as a correctness story

These can come back later only if the small model survives local stress first.

## Rust Control Plane

### `src/main.rs`

Phase 1 target:

- keep loading, lifecycle handling, and fail-open reporting
- keep optional observability surfaces available, but do not let them shape the
  default scheduler runtime
- treat userspace fallback as non-essential to the base branch

First reduction targets:

- stop describing the branch as if the current hierarchy is the intended base
- keep monitor/TUI/stats as optional tooling, not as part of the scheduler's
  correctness story

### `src/bpf.rs`

Phase 1 target:

- keep only the topology and stats exports the smaller queue model still needs
- avoid preserving counters just because they existed in the old hierarchy

First reduction targets:

- LLC/node/shared counters that no longer match the phase 1 model
- comments and helper names that assume the old hierarchy is still the base

## Concrete Refactor Order

1. simplify `cognis_dispatch()` to the chosen deferred model
2. simplify `cognis_enqueue()` to feed that model
3. remove unused deeper-tier helpers and counters
4. simplify Rust docs and stats to match the reduced BPF path
5. rerun local benchmark repros before reintroducing any Cognis-specific policy

## Definition Of Done For Phase 1

Phase 1 is done only when:

- the hot path can be explained without referring to the old deep hierarchy
- code comments match the reduced path exactly
- repeated local repros no longer hit the current watchdog stall
- only then do we start adding Cognis-specific locality or desktop heuristics
