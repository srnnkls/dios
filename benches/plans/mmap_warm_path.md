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

## Runs (2026-08-18, first recorded on both archs)

| tree | macOS (M1 Pro) | Linux (3970X) |
|---|---|---|
| main | 1.7337, ci95 1.7766 → PASS 3.0 | 2.0491, ci95 2.1083 → PASS 3.0 |
| `spike/prefetch-reserve` (reserve machinery present, `prefetch_reserve` 0) | 1.7967, ci95 1.8636 | 2.0904, ci95 2.1364 |

The spike-vs-main ratio-of-ratios is 1.02–1.04 on both archs — the
reserve machinery is warm-neutral when disabled (plan-prefetch
falsifier 2, machinery-cost half). The warm hit never enters
`first_free_frame` or the top-up path, and the numbers agree.

## Escalation run (2026-08-18, spike branch): attribution and the two levers tried

Perf profile of the isolated warm-hit loop (`warm_profile` scratch bench,
pinned host): the shared granule fold is 41.5 ns/op (the floor); the
~41 ns of machinery decomposes as `Pool::get` body ≈ 18% of samples
(control-mutex + liveness + glue), seqlock `PageTable::lookup` ≈ 11%,
`ReaderSlot::begin_pin` epoch publish ≈ 7.5%, remainder (pin_owned,
state, clock touch, guard drop, commit) ≈ 7%.

Lever 1 — lock-free file liveness (`file_live_generations: [AtomicU64]`
mirror written under the control mutex at register/retire-start, read
Acquire in `get`): semantically clean (full suite incl. the retirement
contract passes; a pin-first reorder WITHOUT the check was tried first
and correctly rejected by `pool_retire`). Measured effect single-
threaded: ~4 ns/op, at the edge of this bench's run-to-run band
(Linux 2.00–2.09 across five runs). Its real value is removing a global
mutex from the multi-reader warm path — unmeasurable in this
single-reader bench; adopt on contention grounds, verify under the
read-concurrency workloads.

Lever 2 — `#[inline]` hints across the pin path: measured REGRESSION
(Linux 2.00 → 2.09, macOS 1.69 → 1.90), reverted. Recorded so it is not
retried blind.

Remaining gap is structural at this working set: the seqlock probe and
the loom-guarded SeqCst epoch publish are protocol, not glue; per-reader
last-frame memoization (the pre-recorded next lever) only pays on
repeat-heavy hit patterns, not this uniform-random workload. Context
that bounds the effort: at realistic resident sets the gap is already
1.18 (Linux) / inverted (macOS) per `mmap_tlb_pressure`, and DIO-G1's
block-fetch amortization shrinks it further — the 64-page corner is the
floor's showcase, not the regime that matters.

## Stricter-assumptions audit (2026-08-18, spike branch): the protocol is already tight

Question: can conservative orderings be traded against invariants that
do not bite? Audit of every candidate, with the ceiling measured before
any design work:

- Reader-side `SeqCst` fence in `begin_pin` (asymmetric-fence /
  membarrier candidate): ceiling measured by removing the fence
  outright (unsound spike build, reverted) — Linux 2.00 vs a 2.00–2.09
  run band, macOS 1.68 vs 1.69–1.90. The fence costs ~2–4 ns: the
  store buffer holds one epoch store when it drains. The membarrier
  design (kernel-semantics assumption, loom-model split, Linux-only
  lane) buys nothing here. KILLED BY DATA.
- Seqlock table entry → single atomic word: requires packing `PageId`
  into 64 bits (generation width cap ~2^26 per fd slot) — an assumption
  that DOES bite, for single-digit ns. Rejected.
- CLOCK touch: already load-elided (store only on clear→set). Nothing
  left.
- Guard counters: already `Relaxed`, single-owner by `ReaderCtx: !Send`.
  Nothing left.
- File liveness: the one tightening that survives — the lock-free
  generation mirror (single-writer-under-control-lock, Acquire read)
  already taken above.

Conclusion: no single conservative ordering carries the gap; the ~40 ns
is breadth — many few-ns loads/stores/branches across several cache
lines per hit. Materially beating ~2× at tiny working sets would take
layout fusion (hit metadata on one cache line per frame), a redesign
with its own bench plan, not an assumption. The TLB result and DIO-G1
amortization continue to bound what that would be worth.
