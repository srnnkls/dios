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

## Workload contract

<For workload-level benches: useful operations/bytes; operation mix and request
distribution; working-set/pool sizes; warm/cold meaning at each layer;
configured and observed concurrency; closed/open arrival model; bounded
retries/drains; checksums and path counters that prove the intended workload.>

## Measurement boundary and attribution

<What setup, cache preparation, registration, and teardown are excluded or
included? What differs between arms? For interference tests, distinguish
foreground completion from total drain. Record source/binary/host identities
and raw samples. When explaining cost, specify the matched CPU profile and
bounded trace/counter replay, observer-overhead comparison, model units,
unidentified terms, and falsifying control. Skip tracing for simple benches
when it adds no useful evidence.>

## Notes

<host requirements, cache protocol, anything the numbers depend on>
