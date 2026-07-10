# Bench Plan: mmap_warm_path

| Field | Value |
|-------|-------|
| Metric & direction | wall-time ratio candidate/base — pool warm-hit through a `FrameGuard` over the bare mmap resident read, lower is better |
| Workload | 64 resident 4 KiB pages; each arm folds one whole granule (512-u64 XOR, `black_box`) at a uniformly random resident index, both arms replaying the identical per-rep index sequence; 40 reps × 256 iters. Base arm: bare-pointer read from a file-backed `PROT_READ`/`MAP_SHARED` mmap. Candidate arm: `Pool::get` hit over a `MockDriver`-composed pool, all pages resident — the warm hit never touches the driver |
| Baseline | the mmap resident read is definitionally the baseline (bare pointer arithmetic + the 4 KiB scan); no SHA is pinned — the arm is regenerated every run as the interleaved A arm |
| Reps | 40 (protocol minimum 30) × 256 iters_per_rep (one rep ≥ ~1 µs) |
| Threshold | one-sided 95% CI upper bound of the ratio ≤ 3.0 |
| Compare command | `mise run gate target/bench-samples/mmap_warm_path.csv 3.0` |
| Escalation lever | flamegraph the warm path (the `flamegraph` skill), diff the folded stacks against the `pool_warm_path` expectations; first suspects are the first-pin `SeqCst` publish fence and the seqlock probe retry |

## Notes

In-repo **SANITY** gate, not a scope gate. It asserts only that the pool's
residency machinery — the seqlock probe, the epoch publish/fence, and the CLOCK
reference — stays within 3× of a bare mmap read when the 4 KiB granule scan
dominates both arms. Both arms differ solely in how the granule pointer is
produced (bare pointer arithmetic vs. seqlock probe + epoch publish/fence +
CLOCK check); the read itself is identical work, so the ratio isolates the
residency overhead against a resident-read floor.

The **binding** 1.02 warm-parity gate is DIO-G1 at T014 in sira, at the
block-fetch layer, where CRC verification and block decode amortize the
residency overhead this bench measures in isolation. This bench neither asserts
nor replaces DIO-G1: a 3× SANITY bound on the bare residency cost and a 1.02
parity bound on the amortized block fetch measure different things. The mmap arm
here maps a plain file region for a resident-read floor; the DIO-G1 arm compares
against sira's own mmap block reader.

Because the `FrameGuard` is dropped every iteration, every hit is a **first
pin**, so each `get` fires the batch-7 first-pin `SeqCst` publish fence (the
poll-side half of the store-buffer pair; `pool_warm_path.md` batch-7 note). This
bench therefore measures the FENCED warm hit — the honest post-batch-7 shape,
not a nested-pin fence-elided steady state. macOS numbers are advisory (the
scope gates run only on the pinned Linux host); this SANITY bound is portable
and runs anywhere the mock backend composes.
