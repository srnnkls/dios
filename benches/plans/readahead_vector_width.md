# Bench Plan: readahead_vector_width

| Field | Value |
|-------|-------|
| Metric & direction | Elapsed ratio candidate/base of the cold sequential scan, lower is better; peak requests and bytes in flight as witnesses |
| Workload | `mmap_workloads` scan runner, `geometry` shape (64 MiB cold sequential scan, full-page fold, 8 MiB payload arena, shipping `io_uring`, `DirectIo::Required`, consumer on CPU 0), at the default 512 KiB budget (`scan-default-sample geometry:automatic:4:128:256`) and at 1 MiB (`scan-sample geometry:automatic:4:256:512`) |
| Baseline | `mw-quiescent` (`c49c01c`): READV vectors of at most 32 pages / 128 KiB |
| Candidate | `mw-v64q`: the same source with vectors of at most 64 pages / 256 KiB (`VECTOR_FRAMES_MAX`, `VECTOR_BYTES_MAX` and the arrays sized by them), an exploratory scratch build, not a branch commit |
| Reps | 200 alternating fresh-process pairs per budget: this guest's per-pair log-ratio SD is about 0.25 |
| Threshold | one-sided 95% CI upper bound ≤ 0.95 at each budget for wider vectors to count as material here |
| Compare command | `cargo bench --features bench --bench compare -- <paired.csv> 0.95` |
| Escalation lever | If wider vectors pass, adopting them reverses the owner's 2026-09-14 decision of at most 32 iovecs per SQE and goes to the owner with this evidence, the bio segment limits of the target devices and the per-slot iovec memory; if they fail, the 32-page cap stands |

## Workload contract

fio on this guest (contiguous buffers, O_DIRECT, sequential) reads 128 KiB at
depth 4 at 1,089 MiB/s and 256 KiB at depth 2 at 1,422 MiB/s with the same
512 KiB in flight, consistent with a per-request cost dominating; at 1 MiB in
flight request size stops mattering (1,460–1,600 MiB/s). At the default budget a
64-page vector keeps two vectors in flight (A = 128 ≥ 2V), so vector mode holds.
A two-vector window needed the refill pass to run while the consuming reader is
unpinned (`readahead_refill.md`, two-vector record), so the baseline is that
commit; traced on the candidate, the default budget holds two 256 KiB requests
in flight with 17 vectors per steady interval and 4 demand reads, the 1 MiB
budget four. The candidate's iovec arena grows from 32 to 64 entries per driver
slot.

## Notes

Local, advisory; `vda` reports `max_segments` 254, so a 64-iovec READV is one
request here, while a device with fewer segments splits it in the block layer.

## Results (local, advisory)

200 alternating fresh-process pairs per budget, base `mw-q-diag`
(`b1b17ee6…aaaa`) against candidate `mw-v64-diag` (`badac3fa…530a`); both
differ from `c49c01c` and its 64-page variant only by a stall diagnostic in
the scan request loop's error path. Samples in
`benches/evidence/readahead_vector_width/`.

| Budget | Ratio geomean | One-sided 95% upper | Bound | Result |
|---|---:|---:|---:|---|
| 512 KiB (default) | 1.0236 | 1.0408 | 0.95 | FAIL |
| 1 MiB | 0.9076 | 0.9244 | 0.95 | PASS |

Medians: 57.9 → 59.7 ms at the default budget, 49.8 → 45.7 ms at 1 MiB. Wider
vectors do not pass at each budget, so under the rule above the 32-page cap
stands. At the default budget two 256 KiB requests in flight lose to four
128 KiB ones; at 1 MiB four 256 KiB requests beat eight 128 KiB ones. The
evidence supports wider vectors only where the budget still keeps at least four
in flight, which would pair them with the owner's 1 MiB default decision.

An earlier attempt with the non-diagnostic candidate stopped once on "scan
request exceeded bounded progress limit". The bound counts polls, about a
second of busy polling on this guest, so a device stall and a hang look alike
there. 600 dedicated reproduction runs (300 per build) and the 800 samples
above found no further stall; the cause is unexplained.
