# Cognis v3 Base Selection

This note records the first concrete v3 implementation decision: what the new
base scheduler shape should be before any Cognis-specific policy is added back.

## Decision

The v3 base should be:

- `scx_cake`-like for the common immediate path and quiet runtime model
- `scx_beerland`-like for the first deferred stealable queue tier
- `scx_lavd`-like later for anti-stranding and pressure-aware widening

In shorter form:

- start from a small `cake`-style fast path
- use a `beerland`-style stealable deferred tier
- defer `lavd`-style preemption and anti-stranding logic until the smaller base
  survives repeated local stress first

## Why This Is The Right Base

### `scx_cake`

`cake` gave the strongest signal for the shape of a robust common path:

- aggressive direct local dispatch when placement is obvious
- per-LLC deferred scheduling instead of a large custom hierarchy on the fast
  path
- quiet userspace behavior during steady-state scheduling

The main lesson from `cake` is not its exact fairness logic. The lesson is that
the common path should be small enough that liveness is easy to reason about.

### `scx_beerland`

`beerland` gave the strongest signal for the first deferred tier:

- runnable work parked in scheduler-owned per-CPU queues
- explicit remote draining with `dispatch_from_any_cpu()`
- simple local-vs-remote dispatch logic under pressure

The important lesson from `beerland` is that deferred work should stay in
stealable queues. Cognis v2 repeatedly showed that queue ownership assumptions
break down badly once a CPU is monopolized by RT or IRQ-heavy work.

### `scx_lavd`

`lavd` gave the strongest signal for the next layer, but not for the first
implementation pass:

- anti-stranding behavior
- pressure-aware widening
- more advanced preemption and victim-kick logic

Those ideas are valuable, but they are not the smallest stable base. They
should come after the stripped-down runtime survives local repros.

## What v3 Should Not Keep As The Base

The base should not start by carrying forward all of the current Cognis
structure.

Not for phase 1:

- a deep `CPU -> LLC -> node -> shared` hierarchy in the common path
- a busy Rust-side scheduling or fallback loop
- a correctness story that depends on a dispatch-progress guard firing before
  the kernel watchdog
- desktop heuristics that have not yet been proven safe under load

## Selected Phase 1 Shape

Phase 1 should aim for this runtime:

1. `select_cpu`
   - pick an obvious target quickly
   - direct-dispatch to `SCX_DSQ_LOCAL_ON | cpu` when that is clearly correct

2. `enqueue`
   - if the task cannot be placed immediately, queue it into a scheduler-owned
     stealable deferred tier
   - do not force a deeper locality hierarchy yet

3. `dispatch`
   - consume local deferred work first
   - if local is empty or the system is pressured, allow remote draining from
     eligible CPU queues

4. Rust runtime
   - quiet by default
   - no default userspace fallback
   - no default monitor-serving behavior

## Concrete Borrowing Plan

### From `scx_cake`

Keep the spirit of:

- direct local dispatch for obvious immediate placements
- small fast path
- quiet runtime expectations

Do not copy blindly:

- `cake`'s exact telemetry structure
- `cake`'s exact gaming-specific policy
- `cake`'s full EEVDF-inspired weighting logic

### From `scx_beerland`

Keep the spirit of:

- scheduler-owned per-CPU deferred queues
- explicit remote draining on busy systems
- simple fallback from local to remote dispatch

Do not copy blindly:

- `beerland`'s entire task deadline model
- `beerland`'s exact sticky-task assumptions

### From `scx_lavd`

Borrow later, after phase 1 is stable:

- anti-stranding escape behavior
- pressure-aware widening
- victim kick / preemption logic where justified

Do not start with:

- all of `lavd`'s complexity at once
- advanced preemption before the basic queue model is already robust

## What This Means For Cognis Identity

This does not mean v3 stops being Cognis.

It means Cognis-specific behavior comes back in a stricter order:

1. prove the small base survives
2. add bounded locality policy carefully
3. add profile differences carefully
4. add desktop heuristics carefully

That is the right tradeoff if the bar is "do not embarrass ourselves in front of
reviewers with another design that still stalls under local repro."
