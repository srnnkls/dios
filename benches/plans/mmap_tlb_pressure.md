# Bench Plan: mmap_tlb_pressure

| Field | Value |
|-------|-------|
| Metric & direction | wall-time ratio candidate/base — pool warm-hit through a `FrameGuard` over the bare mmap resident read, lower is better. The QUESTION this bench answers is the TREND vs `mmap_warm_path` (the 64-page bench measured 1.48–1.91) and whether the ratio crosses 1.0 under real dTLB pressure |
| Workload | 65,536 resident 4 KiB pages (256 MiB working set); each arm folds one whole granule (512-u64 XOR, `black_box`) at a uniformly random index over the WHOLE set, both arms replaying the identical per-rep index sequence; 40 reps × 256 iters. Base arm: bare-pointer read from a file-backed `PROT_READ`/`MAP_SHARED` mmap (one dTLB entry per 4 KiB file page). Candidate arm: `Pool::get` hit over a `MockDriver`-composed pool whose arena is one contiguous anonymous allocation the kernel MAY back with transparent hugepages (one dTLB entry per 2 MiB when it does) |
| Baseline | the mmap resident read is definitionally the baseline; no SHA is pinned — the arm is regenerated every run as the interleaved A arm |
| Reps | 40 (protocol minimum 30) × 256 iters_per_rep |
| Threshold | one-sided 95% CI upper bound of the ratio ≤ 3.0 (the same in-bench SANITY bound as `mmap_warm_path`) |
| Compare command | `mise run gate target/bench-samples/mmap_tlb_pressure.csv 3.0` |
| Escalation lever | record `/sys/kernel/mm/transparent_hugepage/enabled` alongside every Linux run — the result is uninterpretable without it. If the pool does not win (ratio < 1.0) where THP is active, flamegraph the warm path (the `flamegraph` skill) and diff the folded stacks BEFORE theorizing about TLB effects |

## Notes

This is **characterization, not a binding gate**. It measures the same
candidate/base ratio as `mmap_warm_path` but under a working set 1024× larger, so
both arms run under real dTLB pressure instead of a TLB-resident 64-page hot set.
The hypothesis: the mmap arm pays one dTLB entry per 4 KiB file page and thrashes
the TLB, while the pool arena — a single contiguous anonymous allocation — is a
candidate for transparent hugepage backing (one entry per 2 MiB), so the pool's
residency overhead may be OFFSET or REVERSED by fewer TLB misses. The ratio
trending below the 64-page 1.48–1.91, and especially crossing 1.0, is the signal.

**The result is uninterpretable without the THP state.** Any Linux run MUST
record `/sys/kernel/mm/transparent_hugepage/enabled` (and, for the anonymous
arena specifically, whether `khugepaged` has collapsed it — `/proc/<pid>/smaps`
`AnonHugePages` for the arena range) next to the ratio. On `always` the arena is
eligible for THP without an madvise; on `madvise` it is not, and the pool arm
sees no hugepage benefit. macOS numbers are advisory (no THP knob; the darwin VM
manages superpages opaquely) and stand only as the portable trend datapoint.

The follow-on landed: the arena (`src/pool/frames.rs`) is now allocated 2 MiB-aligned
on Linux when it is at least a hugepage large and is `madvise(MADV_HUGEPAGE)`'d before
first touch, so the pool arm's anonymous arena is hugepage-eligible even under the
`madvise` THP policy (the mmap arm remains backed by 4 KiB file-cache pages). This
change's pass/fail gate is the nix `mmap_tlb_pressure` ratio improving from the
recorded 1.378 baseline (ci95 1.391) with the pinned regression benches
(`mmap_warm_path`, `ring_read_bracket`, the zero-alloc suite) staying green. The
`madvise` return is ignored by design — a `THP=never` kernel returns `EINVAL` and the
pool runs correctly on 4 KiB pages — so any Linux run must still record
`/sys/kernel/mm/transparent_hugepage/enabled` next to the ratio for the number to be
interpretable.

**Capacity:** the pool builder must compose at 65,536 frames (watermark and
config arithmetic stay within their `u32` bounds — the arena span 65,536 × 4096 =
256 MiB is well under `isize::MAX`). If any `u32`/capacity limit bites at 65,536,
that is reported as a FINDING rather than shrunk silently; the stated acceptable
fallback is 32,768 pages (128 MiB), still far above the TLB reach.

## Runs (2026-08-18, first completed runs)

As authored the bench could not complete: `QUEUE_CAPACITY = 8` sized the
mock event recorder (bound = queue × 16 = 128) below the warmup's
131,072 events; fixed to `RESIDENT_PAGES / 2` — warm hits never touch
the driver, so the measured path is unchanged.

| host | ratio geomean | ci95 upper | gate 3.0 |
|---|---|---|---|
| macOS (M1 Pro, 16 KiB pages) | 0.6439 | 0.6931 | PASS — crosses 1.0 in the POOL's favor |
| Linux (3970X, THP `always [madvise] never` — arena not hugepage-backed) | 1.1808 | 1.1945 | PASS |

The trend question is answered on both archs: the small-set ratio
(1.73 macOS / 2.05 Linux in `mmap_warm_path`) collapses to 0.64 / 1.18
under real dTLB pressure — residency bookkeeping amortizes into memory
stalls both arms share. The macOS inversion's mechanism is unattributed
(no flamegraph run; the escalation lever triggers only when the pool
LOSES under active THP, which did not occur).

## Granule sweep (2026-08-18, spike branch, pinned host, THP-advised arena)

The machinery is fixed per hit; useful bytes scale with granule — and the
fold itself runs faster on the hugepage-backed contiguous arena than on
the 4 KiB-paged file mapping, so the ratio crosses BELOW 1.0 rather than
asymptoting to it:

| granule | small set (64 pages) | 256 MiB set |
|---|---|---|
| 4 KiB | 2.04 | 1.18 |
| 16 KiB | 1.27 | 1.04 |
| 64 KiB | 0.98 (ci95 0.992 — flip) | 0.99 (ci95 1.002 — parity) |

Reading: the warm gap is not a fixed tax, it is machinery ÷
useful-bytes-per-pin. At 64 KiB granules the pool beats or matches bare
mmap in BOTH warm regimes on x86 (macOS already flipped at 4 KiB/256 MiB).
The trade is NOT free: coarser granules amplify cold reads (a 4 KiB need
fetches 64 KiB — the information-loss model's W > 1 on our own ledger)
and the granule is the consumer's settled format decision
(sira GRANULE_DEFAULT = 4096, M001; re-validation on real encoded
segments is already scheduled at T011/T014 — these numbers are input to
that decision, not an override). The real sweet-spot variable is bytes
consumed per pin, which sequential composition (contiguous span
consumption) raises without touching the format granule. Caveat: the
mmap arm's 4 KiB pages reflect ext4 file-backed mappings on this kernel;
filesystems with large-folio page cache would erode the arena's edge.

## THP attribution (2026-08-18, follow-up demanded by external review)

Verified live via `/proc/PID/smaps` during a timed run: the 256 MiB
anonymous arena VMA reports `AnonHugePages: 262144 kB` — 100%
PMD-mapped with 2 MiB THPs — while the file mapping stays on
`KernelPageSize: 4 kB`. Differential with the arena advised
`MADV_NOHUGEPAGE` (spike build, reverted):

| config | THP on | THP off | fixed-cost model predicts (no THP) |
|---|---|---|---|
| 4 KiB granule, 256 MiB set | 1.18 | 1.48 | — |
| 64 KiB granule, small set | 0.98 | 1.063 | 1.065 |

Readings: (1) the 64 KiB "second effect" is entirely THP — with it
removed the measurement lands on the pure fixed-cost amortization
prediction to three digits. (2) The fixed-cost model
R = 1 + (R_4K − 1)/scale is now validated at three points (16 KiB:
1.26 predicted / 1.27 measured small, 1.045 / 1.04 at 256 MiB; 64 KiB
THP-off: 1.065 / 1.063). The warm tax is an approximately FIXED
ABSOLUTE cost per protected access, not a multiplicative penalty.
(3) `advise_hugepage` (already shipped, `src/pool/frames.rs`) is worth
30 ratio points at 4 KiB/256 MiB — without it the at-scale warm gap
would be 1.48. (4) CI precision, corrected per review: the 64 KiB
small-set win clears its confidence interval (ci95 0.992 < 1.0); the
256 MiB result is PARITY within uncertainty (ci95 1.002), not a
demonstrated win. (5) The small-set sweep column confounds footprint
with granule (64 pages × 64 KiB = 4 MiB, 1,024 base-page translations
on the mmap side); the 16 KiB point and the fixed-256-MiB column are
the clean amortization evidence; a fixed-total-bytes sweep remains
open. (6) Span-guard qualification confirmed empirically: holding the
epoch pin across gets recovers only ~3% (scope-amortization run) — a
real `get_span` must batch liveness, lookup window, CLOCK accounting,
and guard lifetime to approach the one-granule result; its decisive
4-arm bench (constant 64 KiB useful work: one 64 KiB get / 16×4 KiB
span op / 16×4 KiB independent gets / bare mmap fold, × THP on/off) is
pre-registered here for whichever scope owns the span surface.
