# RT Watchdog Follow-Up

This note tracks the remaining RT-heavy robustness issue that showed up during local `cachyos-benchmarker` repro runs.

## What We Proved Locally

- Cognis can still hit `sched_ext`'s `runnable task stall (watchdog failed to check in for 5.001s)` exit under heavy RT-class pressure.
- The failure is reproducible locally with `cachyos-benchmarker`; it is not tied to a single process name.
- The service/runtime recovery path is now better:
  - watchdog exits fail open to the kernel scheduler
  - systemd no longer restarts Cognis immediately on exit status `86`

## What We Already Ruled Out As Sufficient Fixes

- quieter Rust control-loop behavior by itself
- userspace fallback disabled by default
- immediate direct local dispatch for the common fast path
- starting the deferred overflow path at LLC instead of a custom per-CPU DSQ
- the current experimental BPF-side dispatch-progress guard

## Current Safe Conclusion

The remaining issue is not "Rust panicked" or "the service restart loop made the machine worse." The unresolved part is the scheduler's robustness under RT-heavy pressure.

The strongest current hypothesis is that Cognis still needs better RT-aware behavior in the saturated/deferred path, or a clearer documented limitation for affinity-confined work when RT-class threads monopolize CPUs.

## Reviewer-Safe Scope

This note is intentionally narrow. It does not claim that Cognis is fundamentally broken, and it does not claim that the whole watchdog stall class is solved.

The professional current claim is:

- Cognis has a real RT-heavy watchdog edge case.
- Recovery behavior is improved and verified locally.
- The underlying stall is still an active follow-up item.
