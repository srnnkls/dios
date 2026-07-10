# Bench Plan: uring_read_path

| Field | Value |
|-------|-------|
| Metric & direction | wall-time ratio candidate/base of the async `READ_FIXED` miss path (submit → batched SQE fill at poll → non-blocking submit → CQE reap), lower is better |
| Workload | cold O_DIRECT single-frame reads over a preallocated file on the pinned NVMe fs: 64-frame ring pool, 4096-byte frames, offsets drawn uniformly over a working set larger than the pool; 40 reps × 4096 iters |
| Baseline | the io_uring backend's prior revision, run as the interleaved A arm against the candidate B arm through the shared compare harness; the baseline is pinned when the binding sweep lands at T014 |
| Reps | 40 (protocol minimum 30) |
| Threshold | one-sided 95% CI upper bound of the ratio ≤ 1.10, asserted by the shared compare command below (never hand-rolled statistics) |
| Compare command | `mise run gate target/bench-samples/uring_read_path.csv 1.10` |
| Escalation lever | per-worker rings (AD-4 escalation: `SINGLE_ISSUER` + `DEFER_TASKRUN`), then registered-buffer sizing review — both pre-recorded in the scope's ring-topology note |

## Notes

T004 changes are perf-relevant: they restructure the execute phase into
batched SQE-fill at poll's prepare plus CQE reap, and add
registered-buffer `READ_FIXED` reads through fixed files. This plan pins
the isolated per-miss ring cost so a fill/reap or registration change that
regresses the submit-to-reap path is caught before the binding sweep.

The **binding** Linux perf gates for the read plane are DIO-G1 (warm
block-fetch parity vs mmap) and DIO-G3 (64 concurrent cold gets ≤ 2.0×
single-miss p50), both owned by T014 on the pinned host — this plan does
not assert them. The bench itself lands with T014; this document fixes its
metric, workload, and gate ahead of the code per the bench-driven rule.
Runs on the pinned Linux host (Threadripper 3970X, NVMe 970 PRO, kernel
6.6) under the governor and cache-drop protocol the scope documents; the
ring backend exists only on Linux, so there is no macOS arm.

T005 adds a cold branch to the CQE reap: a `-EAGAIN`/`-EINTR` result under
the init-time bound re-queues the op's slot for a fresh SQE instead of
finalizing it (scope.md:596). On the fault-free read path the branch is a
single sign test on a non-negative CQE result and never re-queues, so it
adds no measured work to the pinned workload above; the retry cost is
exercised only under injected transients. No new plan is warranted — the
T014 gates on this metric cover the reap path with the branch in place.
