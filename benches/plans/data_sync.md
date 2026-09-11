# Bench Plan: explicit data synchronization

| Field | Value |
|-------|-------|
| Metric & direction | Full-durability sira CL011 single-commit elapsed ratio, candidate / pinned original dios, lower is better |
| Workload | TR 3970X, ext4, performance governor, THP never; 100k seed rows and 1000 disjoint single-row commits, original CL011 data and checks. Full data durability on both sides. |
| Baseline | dios 13c3af5 + sira 7dc9f48, Full maps to fsync; preserve original logs and binaries |
| Reps | 30 paired repetitions; stage smoke 3 repetitions before the full gate |
| Threshold | one-sided 95% CI upper bound candidate/base <= 0.40 |
| Compare command | Export paired elapsed columns to the shared `mise run gate <samples.csv> 0.40` input schema; report actual invocation in evidence |
| Escalation lever | If the gain fails, trace sync submission/completion and compare raw fsync/fdatasync again; do not weaken data durability or relax the gate |

The prior isolated DATASYNC experiment measured about 1.18ms versus 3.41ms per commit. This patch must add an explicit Data mode, retain Full metadata semantics, and preserve mode through held pool barriers and both executors. Functional routing, independent errors, write-before-sync ordering, and existing zero-allocation tests precede benchmark qualification. Sira also measures mmap/redb and fold-settled batches; the broader clear-win gate is owned by its commitlog scope.

## Qualification

Passed on the pinned TR host, 30 alternating A/B process pairs:
`cargo bench --features bench --bench compare -- benches/evidence/data_sync/paired.csv 0.40`.
The shared harness reports geomean 0.3464, one-sided 95% upper 0.3482, threshold 0.4000.
The candidate includes header recycling/page-window changes in the sira caller; the
single-row arm writes one page and isolates the data synchronization improvement.
Both arms retain 1000 covering syncs, 1000 acknowledgements and final fold settlement
checks. CSV and harness verdict are checked in alongside this plan.

Linux mock-enabled tests and strict all-target clippy passed. The Mac full bench+mock
suite, 23 zero-allocation tests, strict clippy and rustdoc passed. Held barriers preserve
Data/Full; the real ring core preserves each mode across EINTR; Full remains unchanged.
