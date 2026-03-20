# Mini Benchmarker Example

This directory contains one real local Mini Benchmarker comparison run captured with:

- date: `2026-03-20`
- baseline label: `Linux 6.19.7-1-cachyos`
- Cognis label: `Cognis (desktop)`
- runs per variant: `1`

Machine used:

- CPU: `AMD Ryzen 7 6800H with Radeon Graphics`
- RAM: `64 GiB DDR5-4800`
- distro / desktop: `CachyOS` with `KDE Plasma 6.6.3`

Power profile note:

- the benchmark script did not record the power profile for this historical run
- the committed example therefore does not claim `performance` or `balanced` mode for that run
- future tagged logs now include a `Power profile:` line

Result summary:

- baseline total time: `533.14 s`
- Cognis desktop total time: `547.25 s`
- baseline total score: `72.12`
- Cognis desktop total score: `74.39`

This is a single-machine, single-run example. It is useful as a concrete output sample and a local data point, not as a universal performance claim.
