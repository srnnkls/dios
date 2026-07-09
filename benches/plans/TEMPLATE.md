# Bench Plan: <name>

| Field | Value |
|-------|-------|
| Metric & direction | <e.g. wall-time ratio candidate/base, lower is better> |
| Workload | <pinned workload: sizes, distributions, fixture, host protocol> |
| Baseline | <what the candidate is measured against> |
| Reps | <n ≥ 30, plus iters_per_rep sizing> |
| Threshold | one-sided 95% CI upper bound of the ratio ≤ <bound> |
| Compare command | `mise run gate target/bench-samples/<name>.csv <bound>` |
| Escalation lever | <pre-recorded action when the gate fails> |

## Notes

<host requirements, cache protocol, anything the numbers depend on>
