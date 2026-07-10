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

## Baseline comparisons (added post-scope, user-directed)

All ratios are candidate/base geomean with the one-sided 95% CI upper
bound from the shared compare harness; the ring arm asserts every CQE
landed a full granule (an error or short read fails the bench rather
than skewing the ratio).

### mmap_warm_path — pool warm hit vs bare mmap read (sanity gate ≤ 3.0; the binding 1.02 DIO-G1 parity gate is T014's, at the block-fetch layer in sira)

| Env | Ratio geomean | CI95 upper | Gate |
|-----|---------------|------------|------|
| `mac` | 1.582 | 1.617 | PASS |
| `vm` | 1.480 | 1.499 | PASS |
| `nix` | 1.909 | 1.937 | PASS |

Both arms scan the identical 4 KiB granule; the arms differ only in
residency machinery (bare pointer arithmetic vs seqlock probe + epoch
publish + first-pin SeqCst fence + CLOCK check — the guard drops each
iteration, so every hit pays the fenced first-pin cost). The ~1.5–1.9×
pure-access overhead is the parity headroom DIO-G1's CRC+decode
amortization has to absorb; at a ~70 ns scan the absolute overhead is
tens of nanoseconds per hit.

### ring_read_bracket — driver-level ring read vs blocking pread, QD1 O_DIRECT (gate ≤ 1.25)

| Env | Ratio geomean | CI95 upper | Gate |
|-----|---------------|------------|------|
| `nix` | 0.932 | 0.935 | PASS — the ring is ~7% FASTER than pread |
| `vm` | 1.081 | 1.106 | PASS (advisory — virtio storage) |

On real NVMe the registered-buffer/fixed-file ring path beats the
classic blocking syscall even at queue depth 1 with zero overlap in
play; on the VM's virtualized storage the ring is ~8% slower, which is
the virtio per-op cost registered buffers cannot skip — a good example
of why `vm` stays advisory. No macOS arm (the ring is Linux-only).

### fio device bracket (nix, file-based O_DIRECT 4K randread — the DIO-G3 lever floor/ceiling)

| Arm | IOPS | Bandwidth | Mean latency |
|-----|------|-----------|--------------|
| io_uring QD64 (ceiling) | ~269,000 | ~1.03 GiB/s | 237.6 µs (queued) |
| psync QD1 (floor) | ~15,900 | ~62 MiB/s | 62.5 µs |

The QD1 device latency (~62 µs) dominates both arms of the ring
bracket above, which is why its ratio sits near 1.0; the QD64 ceiling
is the number T014's overlap gate measures the real ring against.

### Rejected baselines (assessed, not benched)

A tigerbeetle extract would measure Zig-vs-Rust codegen more than
design (the structural comparison lives in design.md); monoio/compio/
tokio-uring/glommio force incompatible workload shapes through their
owned-buffer and executor models and would enter as heavyweight
dependencies — a one-off monoio credibility number in an external
scratch crate remains an open offer, not a pinned baseline.

---

## T014 sections (placeholders — filled under the pinned host protocol)

- fio QD64 random-read device floor (DIO-G3 lever): recorded above
  (as-is protocol; T014 re-runs under governor/pinning/cache-drop)
- DIO-G1 parity run (block-fetch layer, decoded cache bypassed): _pending_
- RC-R2 scaling: _pending_
- overlap gate on the real ring: _pending_
- write-plane A/B vs RealFs (DIO-G8): _pending_
- scan-workload observation (per-ReaderCtx counters; S3-FIFO evidence): _pending_
