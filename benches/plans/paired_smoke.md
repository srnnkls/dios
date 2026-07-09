# Bench Plan: paired_smoke

| Field | Value |
|-------|-------|
| Metric & direction | wall-time ratio candidate/base, lower is better |
| Workload | 4 KiB granule copy, identical closure on both sides; 40 reps × 256 iters |
| Baseline | the base closure itself (A/A run) — validates the harness, not a change |
| Reps | 40 (protocol minimum 30) |
| Threshold | one-sided 95% CI upper bound of the ratio ≤ 1.25 (asserted in-bench) |
| Compare command | `mise run gate target/bench-samples/paired_smoke.csv 1.25` |
| Escalation lever | an A/A run failing this bound means the harness or host is broken — fix before trusting any gate |

## Notes

Infrastructure check, not a perf gate: both sides run the same code, so
the ratio must sit near 1.0 on any healthy host. Real gates (DIO-G1..G3)
are owned by the scope and run only on the pinned Linux host.
