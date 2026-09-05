# Bench Plan: miss_table_pending_index

| Field | Value |
|-------|-------|
| Metric & direction | wall-time ratio of one cold `get` on a 32,768-frame pool to one cold `get` on a 256-frame pool (candidate/base), lower is better; the per-miss cost must not grow with the frame count |
| Workload | two mock-backed pools, 4 KiB granule, one reader each, `max_inflight_reads` 64, `miss_headroom` 192; each pool is warmed to full occupancy before sampling so every measured miss claims through the CLOCK; both arms drive a never-read page (`get` → `poll` → `ready`) on a monotonic page counter; 40 reps × 64 iters, interleaved A/B |
| Baseline | the 256-frame arm itself (interleaved A/B): at a frame-count-independent miss path the ratio sits near 1.0; a per-miss walk over a frame-sized table scales it with 32,768 / 256 = 128 |
| Reps | 40 (protocol minimum 30) |
| Threshold | one-sided 95% CI upper bound of the ratio ≤ 1.25 |
| Compare command | `mise run gate target/bench-samples/miss_table_pending_index.csv 1.25` |
| Escalation lever | a failing gate means a cold-miss step walks a frame-sized structure again; profile the candidate arm with the flamegraph skill and attribute the walk to its frame before touching the pool |

## Notes

Bench: `cargo bench --features bench,mock --bench miss_table_pending_index`
writes `target/bench-samples/miss_table_pending_index.csv`; the gate is
asserted only by `mise run gate`, never in-bench. The mock backend
executes each read inline at `poll`, so the ratio isolates pool-side CPU
work: no kernel, no device. The mock's event recorder is bounded by
16 × `queue_capacity`, so the bench builds the mock with a queue of
8,192 to hold the warm-up and the sampled misses.

`MissTable` is sized to the frame count so terminal `Succeeded`
generations can protect their frame until the last waiter drops, but
`find_pending` and `find_by_token` walked every slot on every cold `get`
and every completion, and `first_free_frame` walked frame states from 0
on every claim. Pending misses are bounded by `max_inflight_reads` (each
holds one read credit), so the fix keeps a pending index of that
capacity beside the slots and a free-frame stack beside the frame states.

Measured on the advisory macOS host (Apple silicon, 2026-09-05):

| Revision | ratio geomean | ci95 upper | gate at 1.25 |
|----------|---------------|------------|--------------|
| b1db67d (main) | 74.88 | 76.42 | FAIL |
| this branch | 1.10 | 1.15 | PASS |

Origin: the flamegraph of sira's DM009 `darwin-scan` arm (984 MB pool,
240,241 frames, ~3,300 cold misses) put 86% of the scan's self time in
`MissTable::find_pending`; dios p50 fell from 7,108 ms to 200 ms on that
arm with mmap at 247 ms in the same run. The sira Linux
`overlap-fragmented-drain` arm (216,400 misses through the same table) is
the binding re-measurement on the pinned host once sira pins this revision.
