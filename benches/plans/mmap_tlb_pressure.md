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

The follow-on — an explicit `madvise(MADV_HUGEPAGE)` on the arena to force
hugepage backing under the `madvise` THP policy — is a SEPARATE owner decision,
not this bench. This bench only measures what the default policy already gives.

**Capacity:** the pool builder must compose at 65,536 frames (watermark and
config arithmetic stay within their `u32` bounds — the arena span 65,536 × 4096 =
256 MiB is well under `isize::MAX`). If any `u32`/capacity limit bites at 65,536,
that is reported as a FINDING rather than shrunk silently; the stated acceptable
fallback is 32,768 pages (128 MiB), still far above the TLB reach.
