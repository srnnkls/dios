# dios-v1 measurements

Cross-architecture bench report at `feat/dios-v1` HEAD `246cf9e`
(all seven batches + the bench-host gate clearance), 2026-07-10.
This file is also T014's measurement home; the T014 sections at the
bottom are placeholders it fills under the pinned host protocol.

## Environments

| Id | Hardware | OS / kernel | Arch | Role |
|----|----------|-------------|------|------|
| `mac` | Apple M1 Pro, 16 GiB | macOS (Darwin 25.5) | arm64 | dev machine — advisory only |
| `vm` | Apple M1 Pro via OrbStack VM, 8 vCPU / 12 GiB | Linux 7.0.11-orbstack | arm64 | interim Linux functional gate (owner decision, batch 5) — advisory |
| `nix` | AMD Threadripper 3970X, 64 threads, NVMe 970 PRO | NixOS, Linux 6.6.64 | x86-64 | the pinned gate host (scope protocol) |

Protocol caveat: these are as-is runs. The full T014 host protocol
(governor, CCX pinning, cache drop, fio device floor) is T014's entry
gate and has not been executed; `nix` numbers here are indicative,
not gate-grade. `mac`/`vm` are advisory by scope rule.

## Microbench results

### smoke — `granule_copy_4k` (criterion, absolute wall time)

| Env | Time (mid estimate) | CI width |
|-----|---------------------|----------|
| `mac` | 70.6 ns | ±0.7 ns |
| `vm` | 69.9 ns | ±0.7 ns |
| `nix` | 32.6 ns | ±0.005 ns |

The 4 KiB granule copy is memory-subsystem-bound; the Threadripper's
result is both ~2.2× faster and two orders of magnitude tighter in
CI — the pinned host earns its role on variance alone. `mac` and
`vm` agree with each other (same silicon through the VM), which
validates the container as a faithful arm64 stand-in for functional
work.

### paired_smoke — harness self-check (identical closures, gate ≤ 1.25)

| Env | Ratio geomean | CI95 upper | Gate |
|-----|---------------|------------|------|
| `mac` (run 1) | 1.0494 | 1.2553 | FAIL (noise spike) |
| `mac` (run 2) | 1.0167 | 1.0737 | PASS |
| `vm` | 0.9308 | 1.0090 | PASS |
| `nix` | 0.9628 | 0.9779 | PASS |

The single `mac` failure with an immediate clean re-run is a
concrete instance of why macOS numbers are advisory: an interactive
laptop cannot hold the CI width the gate assumes. `nix`'s 0.015-wide
CI is the reference behaviour.

### overlap — 64 concurrent cold gets vs one cold miss (plan gate ≤ 2.0, BINDS AT T014)

| Env | Ratio geomean | CI95 upper | Status |
|-----|---------------|------------|--------|
| `mac` | 60.52 | 62.77 | advisory |
| `vm` | 65.96 | 68.21 | advisory |
| `nix` | 63.13 | 65.19 | advisory |

The ~60–68× ratio is consistent across every environment because the
bench currently drives `Pool<MockDriver>`: the mock completes reads
synchronously at poll, so 64 misses cost ~64 single misses — there
is no kernel-side overlap to measure. This is the expected and
recorded state: the 2.0 gate binds at T014 on the real ring, and the
real ring cannot serve `Pool<Driver>` reads until the T014 arena
unification (the recorded blocker: `share_frames` is a no-op on the
real `Driver`, `push_read` needs a destination-buffer offset). The
uniformity across archs is itself a useful null result — it confirms
the number measures mock serialization, not platform IO behaviour.

## Non-bench performance expectations (verified as tests)

| Expectation | Verdict | Evidence |
|-------------|---------|----------|
| INV-2 / DIO-G4 zero allocation on submit/poll (driver, both backends) | MET | `tests/zero_alloc.rs` driver gates green on `mac` (eager) and `vm`+`nix` (uring) |
| Zero allocation on warm get / miss submit / poll drain / Busy (pool) | MET | `tests/zero_alloc.rs` pool gates green (mock-composed pool; `Pool<Driver>` path is inert until T014) |
| Zero allocation on close-during-drain (deferred retire) | MET | dedicated gate green on both backends |
| DIO-G1 warm-hit store-elision (repeat hit stores nothing) | MET | two-directional proof via `clock_reference_stores` through the real `get()` path |
| EBR grace safety under concurrency | MET (and strengthened) | loom 4/4 on `mac`, `vm`, and `nix`; the suite found and fixed a real one-sided-fence bug in the shipping protocol |
| Warm-hit cost budget | RE-CHARACTERIZE AT T014 | the batch-7 SeqCst fences (first-pin publish; one per poll pass) invalidate every pre-batch-7 baseline; design.md's budget updated |

## Gate expectation summary

| Gate / plan | Threshold | Status today |
|-------------|-----------|--------------|
| paired_smoke (harness validity) | ≤ 1.25 | MET on all three environments |
| overlap (DIO-G3 shape) | ≤ 2.0 | NOT YET BINDING — advisory ~63× on the mock; binds at T014 on the real ring after the arena unification |
| pool_warm_path | ≤ 1.10 | PLAN READY, BENCH UNBUILT — baseline deliberately unpinned until after the batch-6 lock-free table (landed) and now the batch-7 fences; the A/B bench and its first pin are T014 work |
| uring_read_path | ≤ 1.10 | PLAN READY, BENCH UNBUILT — requires `Pool<Driver>` reads landing in pool frames (the T014 blocker set) |
| DIO-G1 parity (≤ 1.02 vs mmap), DIO-G2 scaling, DIO-G8 write-plane A/B | per scope | BIND AT T014 IN SIRA — not measurable from the dios crate alone |

## Bottom line

Everything that can be measured or proven at the dios level meets its
expectation: the harness self-validates on all three environments,
every zero-allocation and store-elision property holds as an
enforced test on both architectures, and the concurrency model is
loom-proven on both arm64 and x86-64 — including one real
memory-ordering bug found and fixed by the proofs themselves. No
binding performance gate exists that the crate currently fails. The
open performance questions are all concentrated at T014, are all
recorded with their blockers (ring arena unification, host protocol
validation, fence-stale baselines), and none can be legitimately
answered before the sira integration provides the real read path and
the pinned-host protocol runs.

---

## T014 sections (placeholders — filled under the pinned host protocol)

- fio QD64 random-read device floor (DIO-G3 lever): _pending_
- DIO-G1 parity run (block-fetch layer, decoded cache bypassed): _pending_
- RC-R2 scaling: _pending_
- overlap gate on the real ring: _pending_
- write-plane A/B vs RealFs (DIO-G8): _pending_
- scan-workload observation (per-ReaderCtx counters; S3-FIFO evidence): _pending_
