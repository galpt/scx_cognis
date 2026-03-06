# tested on

           .-------------------------:                    galpt@galpt-laptop
          .+=========================.                    ------------------
         :++===++==================-       :++-           OS: CachyOS x86_64
        :*++====+++++=============-        .==:           Host: 82SB (IdeaPad Gaming 3 15ARH7)
       -*+++=====+***++==========:                        Kernel: Linux 6.19.5-3-cachyos
      =*++++========------------:                         Uptime: 5 hours, 19 mins
     =*+++++=====-                     ...                Packages: 1392 (pacman)
   .+*+++++=-===:                    .=+++=:              Shell: fish 4.5.0
  :++++=====-==:                     -*****+              Display (LEN9059): 1920x1080 @ 1.25x in 16", 120 Hz [Built-in]
 :++========-=.                      .=+**+.              DE: KDE Plasma 6.6.1
.+==========-.                          .                 WM: KWin (Wayland)
 :+++++++====-                                .--==-.     WM Theme: Breeze
  :++==========.                             :+++++++:    Theme: Breeze (Dark) [Qt], Breeze-Dark [GTK2], Breeze [GTK3]
   .-===========.                            =*****+*+    Icons: breeze-dark [Qt], breeze-dark [GTK2/3/4]
    .-===========:                           .+*****+:    Font: Noto Sans (10pt) [Qt], Noto Sans (10pt) [GTK2/3/4]
      -=======++++:::::::::::::::::::::::::-:  .---:      Cursor: breeze (24px)
       :======++++====+++******************=.             Terminal: konsole 25.12.2
        :=====+++==========++++++++++++++*-               CPU: AMD Ryzen 7 6800H (16) @ 4.79 GHz
         .====++==============++++++++++*-                GPU 1: NVIDIA GeForce RTX 3050 Mobile [Discrete]
          .===+==================+++++++:                 GPU 2: AMD Radeon 680M [Integrated]
           .-=======================+++:                  Memory: 6.32 GiB / 58.54 GiB (11%)
             ..........................                   Swap: 240.00 KiB / 58.54 GiB (0%)
                                                          Disk (/): 62.35 GiB / 472.61 GiB (13%) - xfs
                                                          Disk (/intel-drive): 69.40 GiB / 472.61 GiB (15%) - xfs
                                                          Local IP (wlan0): 10.0.10.147/8
                                                          Battery (PABAS0241231): 98% [AC Connected]
                                                          Locale: en_US.UTF-8

                                                                                  
                                                                                  
/intel-drive/scx_cognis main
❯ 




# without scx_cognis:

────────────────────────────────────────────────────────────────────────────
  scx_cognis — Interactive Benchmark Script
  Compare scheduler responsiveness with and without scx_cognis
────────────────────────────────────────────────────────────────────────────
[cognis-bench] 
[cognis-bench] This script will:
[cognis-bench]   • Open the WebGL Aquarium (visual responsiveness test)
[cognis-bench]   • Run a 3-phase stress-ng workload (CPU → I/O → Mixed, 60s each)
[cognis-bench]   • Print bogo-ops/s for each phase so you can compare results
[cognis-bench] 
[cognis-bench] Run it TWICE — once for each mode — and compare the Aquarium smoothness
[cognis-bench] and bogo-ops numbers between the two runs.
[cognis-bench] 
────────────────────────────────────────────────────────────────────────────
Select benchmark mode:

  1  Baseline — run without scx_cognis  (kernel default CFS/EEVDF)
  2  Cognis   — run with scx_cognis active
  q  Quit

Choice [1/2/q]: 1
────────────────────────────────────────────────────────────────────────────
[cognis-bench] Mode 1 — Baseline (default kernel scheduler, no scx_cognis)
────────────────────────────────────────────────────────────────────────────
────────────────────────────────────────────────────────────────────────────
[cognis-bench] Opening WebGL Aquarium in your browser...
[cognis-bench]   URL: https://webglsamples.org/aquarium/aquarium.html
[  OK  ] Launched with: xdg-open
[cognis-bench] 
[cognis-bench] While the benchmark runs, watch the Aquarium for:
[cognis-bench]   • Frame rate  — fish animation should stay smooth (≥ 30 fps)
[cognis-bench]   • Stutter     — pause or jank = scheduler struggling under load
[cognis-bench]   • Tab latency — click Fish Count slider: should respond instantly
[cognis-bench] 
[cognis-bench] Use the default 500 fish — it gives realistic load without overwhelming the system.
[cognis-bench] 
[cognis-bench] Aquarium is open. Leave the fish count at the default (500), let it settle for ~5s,
[cognis-bench] then press Enter here to start the stress workload.
  Press Enter to begin ... 
────────────────────────────────────────────────────────────────────────────
[cognis-bench] Starting stress-ng benchmark  [ Baseline — no scx_cognis ]
[cognis-bench] Total duration: 180s  (3 phases × 60s each)
[cognis-bench] 
[cognis-bench] Phase layout:
[cognis-bench]   1/3  CPU stress     — saturates all logical CPUs (compute-bound)
[cognis-bench]   2/3  I/O stress     — disk read/write latency (I/O-bound)
[cognis-bench]   3/3  Mixed stress   — CPU + VM pressure (realistic desktop load)
────────────────────────────────────────────────────────────────────────────
[cognis-bench] Phase 1/3 — CPU stress (60s) ...
dispatching hogs: 16 cpu
note: 16 cpus have scaling governors set to powersave and this may impact performance; setting /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor to 'performance' may improve performance
stressor       bogo ops real time  usr time  sys time   bogo ops/s     bogo ops/s
cpu             1321495     60.00    940.21      1.15     22024.86        1403.81
passed: 16: cpu (16)
[  OK  ] Phase 1 complete
[cognis-bench] Phase 2/3 — I/O stress (60s) ...
dispatching hogs: 4 iomix
iomix: using 256MB file system space per stressor instance (total 1GB of 403.21GB available file system space)
stressor       bogo ops real time  usr time  sys time   bogo ops/s     bogo ops/s
iomix          11053412     60.01     65.39    193.59    184206.49       42681.43
passed: 4: iomix (4)
[  OK  ] Phase 2 complete
[cognis-bench] Phase 3/3 — Mixed CPU + VM stress (60s) ...
dispatching hogs: 16 cpu, 2 vm
note: 16 cpus have scaling governors set to powersave and this may impact performance; setting /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor to 'performance' may improve performance
vm: using 128MB per stressor instance (total 256MB of 48.82GB available memory)
stressor       bogo ops real time  usr time  sys time   bogo ops/s     bogo ops/s
cpu             1179756     60.00    829.42      0.79     19662.59        1421.03
vm              1475058     60.03    101.58      3.64     24569.98       14019.12
passed: 18: cpu (16) vm (2)
[  OK  ] Phase 3 complete
────────────────────────────────────────────────────────────────────────────
[  OK  ] Benchmark complete  [ Baseline — no scx_cognis ]
[cognis-bench] 
[cognis-bench] Compare these results side-by-side:
[cognis-bench]   1. bogo-ops/s  — higher is better (raw throughput)
[cognis-bench]   2. Aquarium fps — higher is better (visual responsiveness)
[cognis-bench]   3. Aquarium jank — zero stutter is ideal
[cognis-bench] 
[cognis-bench] scx_cognis should deliver a smoother Aquarium experience under load
[cognis-bench] because Interactive tasks receive a 0.5x shorter time-slice, keeping
[cognis-bench] the browser frame-pacing responsive even while CPUs are saturated.
────────────────────────────────────────────────────────────────────────────

/intel-drive/scx_cognis main 3m 8s
❯ 



# with scx_cognis:

────────────────────────────────────────────────────────────────────────────
  scx_cognis — Interactive Benchmark Script
  Compare scheduler responsiveness with and without scx_cognis
────────────────────────────────────────────────────────────────────────────
[cognis-bench] 
[cognis-bench] This script will:
[cognis-bench]   • Open the WebGL Aquarium (visual responsiveness test)
[cognis-bench]   • Run a 3-phase stress-ng workload (CPU → I/O → Mixed, 60s each)
[cognis-bench]   • Print bogo-ops/s for each phase so you can compare results
[cognis-bench] 
[cognis-bench] Run it TWICE — once for each mode — and compare the Aquarium smoothness
[cognis-bench] and bogo-ops numbers between the two runs.
[cognis-bench] 
────────────────────────────────────────────────────────────────────────────
Select benchmark mode:

  1  Baseline — run without scx_cognis  (kernel default CFS/EEVDF)
  2  Cognis   — run with scx_cognis active
  q  Quit

Choice [1/2/q]: 2
────────────────────────────────────────────────────────────────────────────
[cognis-bench] Mode 2 — With scx_cognis active
────────────────────────────────────────────────────────────────────────────
[  OK  ] scx_cognis is active — ready to benchmark.
[cognis-bench] 
[cognis-bench] Tip: open a second terminal and run:
[cognis-bench]   scx_cognis --monitor 1.0
[cognis-bench] to watch the AI scheduler adapt in real-time during the test.
────────────────────────────────────────────────────────────────────────────
[cognis-bench] What to watch in  scx_cognis --monitor 1.0  output:
[cognis-bench] 
[cognis-bench]   tldr:         → Plain-English health summary. Should stay in
[cognis-bench]                   'Rest assured' / 'Busy but responsive' / 'Smooth sailing'
[cognis-bench]                   during the benchmark. Avoid 'SOS' or 'overwhelmed'.
[cognis-bench] 
[cognis-bench]   d→u  vs  k    → User dispatches (d→u) vs kernel fallback (k).
[cognis-bench]                   d→u should be non-trivial. A ratio of k >> d→u means
[cognis-bench]                   cognis isn't getting enough cycles to schedule.
[cognis-bench] 
[cognis-bench]   Interactive   → Should remain the dominant label (most desktop tasks).
[cognis-bench]   Compute       → Will rise during the stress-ng CPU phase — expected.
[cognis-bench] 
[cognis-bench]   cong          → Congestion events. Occasional spikes are fine.
[cognis-bench]                   Sustained high values = scheduler under pressure.
[cognis-bench] 
[cognis-bench]   slice         → AI-adjusted time-slice. Should shrink during
[cognis-bench]                   interactive-heavy phases and grow during compute phases.
[cognis-bench] 
[cognis-bench]   reward        → EMA reward score. Higher = better balance.
[cognis-bench]                   Aim for ≥ 0.3 during the full benchmark.
────────────────────────────────────────────────────────────────────────────
────────────────────────────────────────────────────────────────────────────
[cognis-bench] Opening WebGL Aquarium in your browser...
[cognis-bench]   URL: https://webglsamples.org/aquarium/aquarium.html
[  OK  ] Launched with: xdg-open
[cognis-bench] 
[cognis-bench] While the benchmark runs, watch the Aquarium for:
[cognis-bench]   • Frame rate  — fish animation should stay smooth (≥ 30 fps)
[cognis-bench]   • Stutter     — pause or jank = scheduler struggling under load
[cognis-bench]   • Tab latency — click Fish Count slider: should respond instantly
[cognis-bench] 
[cognis-bench] Use the default 500 fish — it gives realistic load without overwhelming the system.
[cognis-bench] 
[cognis-bench] Aquarium is open. Leave the fish count at the default (500), let it settle for ~5s,
[cognis-bench] then press Enter here to start the stress workload.
  Press Enter to begin ... 
────────────────────────────────────────────────────────────────────────────
[cognis-bench] Starting stress-ng benchmark  [ With scx_cognis ]
[cognis-bench] Total duration: 180s  (3 phases × 60s each)
[cognis-bench] 
[cognis-bench] Phase layout:
[cognis-bench]   1/3  CPU stress     — saturates all logical CPUs (compute-bound)
[cognis-bench]   2/3  I/O stress     — disk read/write latency (I/O-bound)
[cognis-bench]   3/3  Mixed stress   — CPU + VM pressure (realistic desktop load)
────────────────────────────────────────────────────────────────────────────
[cognis-bench] Phase 1/3 — CPU stress (60s) ...
dispatching hogs: 16 cpu
note: 16 cpus have scaling governors set to powersave and this may impact performance; setting /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor to 'performance' may improve performance
stressor       bogo ops real time  usr time  sys time   bogo ops/s     bogo ops/s
cpu              661167     60.00    361.83      0.25     11019.19        1826.00
passed: 16: cpu (16)
[  OK  ] Phase 1 complete
[cognis-bench] Phase 2/3 — I/O stress (60s) ...
dispatching hogs: 4 iomix
iomix: using 256MB file system space per stressor instance (total 1GB of 403.21GB available file system space)
stressor       bogo ops real time  usr time  sys time   bogo ops/s     bogo ops/s
iomix           5613008     60.80     33.73     96.51     92322.96       43098.01
passed: 4: iomix (4)
[  OK  ] Phase 2 complete
[cognis-bench] Phase 3/3 — Mixed CPU + VM stress (60s) ...
dispatching hogs: 16 cpu, 2 vm
note: 16 cpus have scaling governors set to powersave and this may impact performance; setting /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor to 'performance' may improve performance
vm: using 128MB per stressor instance (total 256MB of 48.97GB available memory)
stressor       bogo ops real time  usr time  sys time   bogo ops/s     bogo ops/s
cpu              706928     60.01    406.39      0.05     11780.62        1739.32
vm              1469004     60.03     61.11      3.03     24469.50       22904.19
passed: 18: cpu (16) vm (2)
[  OK  ] Phase 3 complete
────────────────────────────────────────────────────────────────────────────
[  OK  ] Benchmark complete  [ With scx_cognis ]
[cognis-bench] 
[cognis-bench] Compare these results side-by-side:
[cognis-bench]   1. bogo-ops/s  — higher is better (raw throughput)
[cognis-bench]   2. Aquarium fps — higher is better (visual responsiveness)
[cognis-bench]   3. Aquarium jank — zero stutter is ideal
[cognis-bench] 
[cognis-bench] scx_cognis should deliver a smoother Aquarium experience under load
[cognis-bench] because Interactive tasks receive a 0.5x shorter time-slice, keeping
[cognis-bench] the browser frame-pacing responsive even while CPUs are saturated.
────────────────────────────────────────────────────────────────────────────

/intel-drive/scx_cognis main 3m 11s
❯ 
