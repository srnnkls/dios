# Bench Plan: pool_warm_path

| Field | Value |
|-------|-------|
| Metric & direction | wall-time ratio candidate/base of the warm-hit path (PageTable probe + CLOCK reference), lower is better |
| Workload | uniform point lookups over a hot set that fits the pool: 64-frame pool, all pages resident, keys drawn uniformly at random from the resident set; 40 reps × 4096 iters |
| Baseline | the current implementation at this plan's creation commit (pinned SHA), run as the interleaved A arm against the candidate B arm per the shared compare harness; the first run establishes the pin |
| Reps | 40 (protocol minimum 30) |
| Threshold | one-sided 95% CI upper bound of the ratio ≤ 1.10 (asserted in-bench) |
| Compare command | `mise run gate target/bench-samples/pool_warm_path.csv 1.10` |
| Escalation lever | the S3-FIFO / table-tuning notes already in the scope (per-worker last-frame memoization, then S3-FIFO if the scan-workload counters justify it) |

## Notes

Micro-level regression guard only. The binding gates are DIO-G1 (warm
parity vs mmap at the block-fetch layer) and DIO-G2 (8-thread vs 1-thread
scaling), both owned by T014 on the pinned Linux host; this plan pins the
isolated warm-hit cost so a table or CLOCK change that regresses the probe
is caught before the T014 sweep. The bench itself
lands in T008 alongside the overlap bench; this plan fixes its metric,
workload, and gate ahead of code. Runs on the pinned Linux host under the
governor and cache-drop protocol the scope documents.
