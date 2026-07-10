# Bench Plan: pool_warm_path

| Field | Value |
|-------|-------|
| Metric & direction | wall-time ratio candidate/base of the warm-hit path (PageTable probe + CLOCK reference), lower is better |
| Workload | uniform point lookups over a hot set that fits the pool: 64-frame pool, all pages resident, keys drawn uniformly at random from the resident set; 40 reps × 4096 iters |
| Baseline | the warm-hit path at the commit where T008's lock-free table lands — no SHA is pinned before then (the T007 `Mutex<PageTable>` is a substrate the design forbids on the warm path); once landed it runs as the interleaved A arm against the candidate B arm per the shared compare harness, and the first post-T008 run establishes the pin |
| Reps | 40 (protocol minimum 30) |
| Threshold | one-sided 95% CI upper bound of the ratio ≤ 1.10 (asserted by the shared compare harness) |
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

T007 substrate note: the epoch pin path (`Pool::pin`) publishes one Acquire load
+ one Release store on the first guard, one store on the last-guard drop, plus a
SeqCst fence after the publish store (store-buffer ordering vs. the poll-side
scan), and no shared RMW per hit — a pin/drop with no poll never advances the
epoch (pinned by
`epoch_guards::the_global_epoch_advances_at_poll_boundaries_not_per_pin`). The
`PageTable` lookup sits behind a `Mutex` as the T007 substrate; its lock-free
packed-atomic replacement on the warm path is owned by T008. The baseline SHA
stays unpinned until that lock-free table lands, so this gate never bakes in a
lock the design forbids on the warm path. The gate itself is asserted by the
shared compare harness (`mise run gate`), not in-bench; the DIO-G1 no-RMW /
zero-alloc proof is owned by T009/T014.

Batch-7 fence note: T009 added the poll-side half of the store-buffer pair — a
`SeqCst` fence in `advance_epoch` per poll pass — after the grace-period loom
model falsified the fence-free advance scan; the quiescent-reader first-pin fence
above is the other half. Any pre-batch-7 warm-path baseline is stale, so the
first post-batch-7 run re-establishes the pin and the T014 characterization runs
against the fenced code. Nested/repeat pins skip the fence, so it does not touch
the steady-state warm-hit cost.
