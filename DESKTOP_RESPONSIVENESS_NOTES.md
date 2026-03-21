# Desktop Responsiveness Notes

This note separates future desktop-feel work from the narrower RT watchdog-stall follow-up.

## Why This Is Separate

The watchdog issue and the "desktop feels laggy under full saturation" issue are related, but they are not the same bug.

- The watchdog issue is about robustness under RT-heavy pressure.
- The desktop-lag issue is about how quickly interactive work gets to the CPU when the machine is fully saturated.

## Promising Design Directions

- sleep-to-run or sleep-gap heuristics for interactive tasks
- burst-oriented penalties for long-running CPU hogs
- stronger bypass lanes for clearly interactive wakeups
- re-evaluating how much of that logic belongs in BPF versus optional userspace policy

## Ground Rules

- Any future responsiveness work should be measured against repeated local benchmarks, not one-off impressions.
- It should not be rushed into the current upstream PR as a speculative fix for the watchdog issue.
- The current Inspirations and References section already gives a reasonable base for this direction; adding more heuristic-heavy policy should stay explicit about what changed and why.

## Practical Goal

If Cognis grows stronger desktop heuristics, the target is not abstract fairness. The target is:

- fast wake responsiveness
- good frame pacing under saturation
- low risk of trapping bursty interactive work behind long-running CPU hogs
