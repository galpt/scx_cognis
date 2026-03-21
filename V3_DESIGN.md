# Cognis v3 Design Draft

This document is a design target for a future Cognis rewrite. It is not a claim
that the current tree already implements everything described here.

The purpose of this draft is to define a stricter bar than the current
implementation:

- survive repeated hostile local benchmark runs without `sched_ext` watchdog
  exits
- preserve responsive desktop behavior under heavy CPU saturation
- keep the common path simple enough that liveness is easier to reason about
  than in the current design

## Why A v3 Exists

Local work on the current Cognis implementation proved a few useful things:

- fail-open recovery can be made reliable
- BPF-first scheduling is the right general direction
- a desktop/server profile split is still a useful model

Local work also proved what is still missing:

- repeated `cachyos-benchmarker` repros can still trigger
  `runnable task stall (watchdog failed to check in for 5.001s)`
- quieter Rust behavior alone is not enough
- disabling the default userspace fallback alone is not enough
- direct local dispatch alone is not enough
- several queue-shape changes inspired by `cake`, `beerland`, and `lavd`
  improved understanding, but did not eliminate the underlying stall class

That makes the next step a design reset, not another small patch series.

## Primary Goals

1. Liveness first.
   Cognis v3 should prefer guaranteed forward progress over clever locality or
   fairness refinements when those goals conflict.

2. Desktop responsiveness under saturation.
   Interactive wakeups should not get trapped behind long CPU-bound work when
   the machine is busy.

3. Production-oriented failure behavior.
   If Cognis cannot make safe progress, it should fail open to the kernel
   scheduler cleanly and early rather than waiting for the watchdog path.

4. Simple common path.
   The fast path should look closer to the proven `sched_ext` schedulers than
   to the current experimental Cognis hierarchy.

## Non-Goals

- claiming universal stability before repeated local validation exists
- reintroducing a busy Rust-side scheduling loop as part of the normal runtime
- maximizing uniqueness at the cost of robustness
- carrying old behavior forward just because it existed in the current tree

## Design Principles

### 1. Build On Proven Base Patterns

The starting point should be a production-oriented `sched_ext` shape that has
already shown better robustness locally than the current Cognis tree.

That does not mean "fork `cake` and rename it." It means:

- keep the common immediate path close to the proven upstream model
- make every Cognis-specific addition justify its liveness cost
- treat new policy layers as optional on top of a stable base, not as the base
  itself

The strongest practical references so far are:

- `scx_cake`: simple BPF-owned steady state and quiet userspace side
- `scx_beerland`: stealable per-CPU scheduler-owned queues
- `scx_lavd`: anti-stranding and pressure-aware widening behavior
- `scx_cosmos`: simpler local/shared pressure behavior

## 2. Keep The Fast Path Minimal

The common case should be:

- select a valid target CPU quickly
- dispatch directly to `SCX_DSQ_LOCAL_ON | cpu` when that placement is clearly
  correct
- avoid extra hierarchy hops unless there is a real saturation reason

In other words, Cognis v3 should not try to express all of its personality in
the hot path.

## 3. Make Deferred Work Explicitly Stealable

When direct placement is not possible, deferred runnable work should land in
queues that other CPUs can help drain.

The current Cognis experiments suggest that "more hierarchy" alone is not a
robust answer. v3 should assume that a target CPU may stop making useful
progress under RT/IRQ pressure and design for that explicitly.

## 4. Separate Robustness From Desktop Feel

Two local problem classes need different treatment:

- RT-heavy watchdog robustness
- desktop responsiveness under saturation

v3 should keep those concerns separate in the design:

- liveness mechanisms should work even if all desktop heuristics are disabled
- desktop heuristics should improve feel without becoming necessary for
  correctness

## Proposed Runtime Shape

### Core Runtime

- BPF owns the normal scheduling path
- Rust stays quiet by default
- userspace fallback, if it exists at all, should be diagnostics-oriented and
  opt-in

### Queue Model

The default queue model should be intentionally small:

1. immediate direct local dispatch when placement is obvious
2. one stealable deferred tier for busy-path work
3. wider spill only when pressure genuinely requires it

If Cognis wants locality-aware hierarchy beyond that, it should be added only
after the smaller model proves robust.

### Pressure Handling

Under RT-heavy or IRQ-heavy pressure, v3 should prefer:

- widening to stealable queues earlier
- avoiding same-CPU stickiness for ordinary migratable work
- avoiding queue ownership assumptions that depend on a single CPU making
  forward progress

### Desktop Heuristics

Desktop-specific logic should be added on top of the robust base. Promising
directions include:

- sleep-gap or wake-recent bias for interactive tasks
- burst-sensitive penalties for long CPU hogs
- more explicit foreground or latency-critical lanes

Those ideas should be treated as measured policy additions, not as a substitute
for a sound liveness model.

## Validation Bar

v3 should not go upstream until all of the following are true locally:

1. repeated `cachyos-benchmarker` runs do not trigger the current watchdog stall
2. the desktop remains usable during heavy CPU saturation
3. fail-open behavior still works correctly if the scheduler exits
4. repeated local benchmark results are stable enough to be worth discussing
5. the README can describe the design without hedging around unresolved
   watchdog behavior

## Candidate Implementation Plan

### Phase 1: Stable Base

- choose the smallest proven BPF runtime shape to build on
- keep the queue model intentionally simple
- verify watchdog robustness first

### Phase 2: Cognis Identity

- add locality-aware decisions that do not weaken the validated fast path
- add profile tuning for desktop vs server
- add bounded wake-credit / deadline behavior only where it does not make
  liveness harder to reason about

### Phase 3: Desktop Feel

- add measured interactive heuristics
- compare against real desktop lag scenarios, not just synthetic throughput

## Research Directions

The current references remain useful, but v3 should be more selective about
where they apply:

- `EEVDF` and `BVT` remain relevant for bounded latency and virtual-deadline
  thinking
- `lavd`, `cake`, `beerland`, and `cosmos` matter more as implementation
  references for a robust `sched_ext` hot path
- desktop-specific heuristic work should only be cited if it directly informs
  the measured v3 policy

## Current Honest Status

This repository does not have a finished v3 yet.

What exists today is:

- a current Cognis implementation
- local evidence about where it still fails under hostile load
- enough comparison work to define what a cleaner v3 should optimize for

That is exactly why this document exists.
