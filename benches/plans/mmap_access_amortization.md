# Bench plan: resident access amortization

Written before implementation, 2026-09-13, following the user's question about
epoch reclamation and whether independent 64-byte guarded reads represent the
intended API use. Benchmark-only: existing product APIs and safety protocol.

| Field | Value |
|---|---|
| Metric and direction | Dios/mmap elapsed, lower is faster; absolute ns/projection; setup, teardown, and reuse break-even |
| Workload | Same initialized fixture as mmap_workloads; 65,536 64-byte projections over 1,024 resident pages, all PTEs present; one pinned CPU |
| Baseline | Actual resident read-only mmap, identical request order, byte offsets and checksum for each paired lane |
| Repetitions | 30 fresh-process AB/BA pairs per lane, same seed per pair, 2 qualification pairs |
| Threshold | Characterization, no superiority gate. Exact work/checksum, zero timed allocations, resident cache/PTE witnesses, no I/O during the loop; pinned DRP-G2 1.01/1.01 and DRP-G4 1.00/0.50 unchanged |
| Compare command | Existing `mmap_workloads summarize paired.csv`, using the shared ratio harness |
| Escalation lever | Reject incomplete or mismatched work; retain losses. If amortization fails, inspect the exact public API path and setup cost; do not remove the epoch fence or alter safety contracts |

Five separately paired controls:

- `resident_access_ordinary`: ordinary get/guard/drop on each projection.
- `resident_access_hinted`: acquire one exact file lease and one resident hint
  per hot page during measured session setup; reuse the hint on each lookup.
- `resident_access_epoch_batch`: retain the first useful guard of each bounded
  16-read batch while acquiring/consuming the other 15 pages. At most two guards
  coexist. Every requested page is consumed once in the original order; no
  dummy anchor and no new public API. The epoch stays published for that batch.
- `resident_access_retained`: promote each hot page once during measured session
  setup, then access through its retained handle. Report the 4 MiB of physically
  retained payload and handle storage. Setup and final release are separate from
  repeated steady access, and must be included in session amortization results.
- `resident_access_page_batch`: 16 adjacent 64-byte fields from one page under
  one guard. The mmap arm consumes those same fields in the same order. This
  changes the access pattern explicitly; it is not a replacement for random
  independent page access.

These controls execute on the calling thread pinned to CPU 0; both arms use
the same boundary. They are reported separately from the original dispatched
worker lanes. Pool fill, mmap population and request construction remain outside
session timing for all arms. Hints and retained-handle preparation and teardown
are recorded separately, including allocation of their fixed-capacity vectors.
Keep the original executable and all unfavorable measurements. No claim that
retention has free setup, free memory, or a bounded-reclaim cost for arbitrary
long-lived sessions follows from this benchmark.

For a reusable setup, report the observed mean setup-plus-teardown divided by
the per-access saving versus ordinary get, when the saving is positive. This
is an arithmetic amortization estimate, not a statistical acceptance gate.
