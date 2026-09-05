# Bench Plan: miss_table_pending_index

| Field | Value |
|-------|-------|
| Metric & direction | cold sequential scan wall time under the dios backend (candidate/base), lower is better; the lookup cost of a cold `get` and of a read completion must be independent of the pool frame count |
| Workload | sira DM009 `darwin-scan`: one-shard sequential drain of a 5,000,000-pair store (623,638 rows, ~3,300 cold granule misses) against a 984 MB pool (240,241 frames, eager backend, `F_NOCACHE` copy before every trial); 5 interleaved trials mmap/dios |
| Baseline | dios b1db67d, same harness, same store, same host: dios p50 7,108 ms, mmap p50 265 ms |
| Reps | 5 trials (the sira harness's frozen arm shape; advisory host) |
| Threshold | candidate dios p50 ≤ 0.10 × baseline dios p50 (≤ 711 ms); the pinned dios regression benches stay green |
| Compare command | `SIRA_BENCH_DIR=<store parent> SIRA_DM009=darwin-scan <sira redb_workload bench binary> --bench`, read the `darwin-scan: backend=dios` line |
| Escalation lever | if the scan stays above the bound, flamegraph the arm again (samply, folded stacks) and attribute the residual to its frame before touching the pool |

## Notes

The flamegraph of the baseline arm (153,820 samples, 41,737 under
`drain_rows`) put 86% of the scan's self time in
`MissTable::find_pending` and 1.1% in `pread`. The table is sized to the
frame count so terminal `Succeeded` generations can protect their frame
until the last waiter drops, but `find_pending` and `find_by_token`
walked every slot on every cold `get` and every completion. Pending
misses are bounded by `max_inflight_reads` (each holds one read credit),
so the fix keeps a pending index of that capacity beside the slots.

Result on the advisory macOS host (Apple silicon, 2026-09-05): dios p50
635 ms, p95 673 ms (baseline 7,108 ms / 7,313 ms), an 11.2× reduction,
under the 711 ms bound. mmap p50 293 ms in the same run. The sira
Linux `overlap-fragmented-drain` arm (216,400 misses through the same
table, dios p50 213 ms vs 28.6 ms at the 5,262-frame watermark) is the
binding re-measurement on the pinned host once sira pins this revision.
