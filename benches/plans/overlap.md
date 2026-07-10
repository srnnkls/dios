# Bench Plan: overlap

| Field | Value |
|-------|-------|
| Metric & direction | wall-time ratio of 64 concurrent cold `get`s to a single cold miss (candidate/base), lower is better |
| Workload | one reader over a 256-frame pool, 4 KiB granule; base arm drives one cold miss (`get` → `poll` → `ready`), candidate arm submits 64 cold `get`s then drains them to `Ready`; cold pages cycle through a range wider than the pool so the CLOCK recycles frames; 40 reps × 8 iters |
| Baseline | the single-cold-miss arm itself (interleaved A/B): the candidate's 64-miss wall time is measured against 64× the amortized single miss via the geomean ratio the shared harness computes |
| Reps | 40 (protocol minimum 30) |
| Threshold | one-sided 95% CI upper bound of the ratio ≤ 2.0 (asserted by the shared compare harness) |
| Compare command | `mise run gate target/bench-samples/overlap.csv 2.0` |
| Escalation lever | if the ring cannot overlap 64 cold reads within 2.0× a single miss, raise the pool's `max_inflight_reads` and the SQ depth, then re-measure; a persistent miss is a driver batching regression to profile with the flamegraph skill |

## Notes

DIO-G3. The BINDING run is T014 on the pinned Linux host (`nix`,
Threadripper 3970X, NVMe 970 PRO) under `O_DIRECT` with the real
`io_uring` backend, where 64 cold reads overlap in the kernel and the
2.0× bound is meaningful. On the container the eager/mock backend
executes each read inline at `poll`, so there is no kernel overlap and
the ratio runs near 64× — the numbers here are ADVISORY, proving only
that the bench compiles, drives the composed pool, and writes samples the
compare harness can gate. The gate is asserted by `mise run gate`, never
in-bench, so the advisory container run never fails the build.
