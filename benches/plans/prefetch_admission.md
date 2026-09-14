# Bench plan: prefetch admission and default readahead

Written before product implementation, 2026-09-13. Scope:
`scopes/draft/plan-prefetch/design.md`. The owner requested an explicit
readahead API and selected automatic detection enabled by default with a
disable option. Existing baseline evidence stays preserved.

The short-read fault fixture also verifies bytes across a reslice. The mock
previously returned an injected positive short count without transferring its
prefix; correcting that simulation touches no shipping performance path and
remains subject to the same pinned regression gates.

| Field | Value |
|---|---|
| Metric and direction | Candidate/base elapsed, lower is better; CPU/read, useful and wasted reads, outstanding work, p99 observation spans, and demand-hot misses |
| Host protocol | Pinned nix, CPU 0 or prescribed four-worker placement, performance governor, THP never, direct/unregistered, identical initialized fixture, file-local cold verification; private 128 MiB cap for pressure lanes |
| Baseline | Frozen current product at 812c812; exact old executable and harness retained. Also pair explicit prefetch against disabled pure demand in the candidate executable. Actual mmap remains a separately named characterization baseline |
| Repetitions | At least 30 alternating fresh-process pairs with identical work/seed; 2 qualification pairs; shared bootstrap harness only |
| Compare command | Shared `mmap_workloads summarize paired.csv` for characterization; `mise run gate paired.csv BOUND` for the adopted bounds below |
| Escalation lever | Reject a path-witness or safety failure. If recycling destroys overlap, measure reserve inventory/admission alternatives within the existing EBR protocol. If that fails, retain the old implementation and record the failed candidate. Do not relax bounds or change ring topology/fences to obtain a pass |

Adoption gates:

1. Explicit prefetch versus pure demand on fragmented reads with frame
   recycling: one-sided 95% upper elapsed ratio <= 0.50, preserving the
   existing prefetch existence threshold. Include whole-window and incremental
   consumption; accepted checksums, drains and zero timed allocations required.
2. The existing 64 MiB scan with a 1,024-frame pool and the three-pass
   256 MiB scan with a 64 MiB arena: explicit prefetch / frozen current
   16-pending pipeline upper <= 0.80 in each lane.
3. Default automatic sequential readahead / disabled demand on those scan
   shapes: upper <= 0.80. Record its ratio to explicit windows and mmap without
   adopting an unsupported parity threshold.
4. Disabled and default automatic modes preserve existing DRP-G2 warm/cycling
   bounds 1.01/1.01 and DRP-G4 ordinary/scaling bounds 1.00/0.50. The frozen
   historical runner and product identities remain the baselines.
5. Cold fragmented/dependent controls: default / disabled upper <= 1.02,
   with no automatic sequential admissions on their non-sequential traces.
6. Wrong explicit hints beside a demand-hot canary: zero evictions of the
   declared protected hot set within its stated capacity, zero demand-hot
   misses, bounded credits, and complete recovery after abandoned speculation.
   Record p99 and I/O amplification; no tail-latency superiority claim is
   admitted from an unqualified instrumented replay.

The frozen DRP runner constructs pools with `max_inflight_reads(1)`. The new
default credit budget is consequently zero, and both readahead selections
construct the same empty predictor/credit state. Preserve that runner: its
fresh regression results cover this shared zero-credit path, not active
prediction. The scan, non-sequential and wrong-hint lanes exercise active
budgets separately. This does not change any numeric bound above.

The frozen scan comparison command is `uv run benches/mmap_workloads/frozen.py
NEW_OUTPUT --baseline OLD_PRIMARY --candidate NEW_PRIMARY`. It preserves
per-arm executable and runner identities and validates matched work before
writing CSVs for `mise run gate`.

Use a bounded observation-only trace and an interleaved observer-overhead
campaign for new paths. Profiles must use the exact retained candidate
executable and preserve unresolved kernel samples. Default learning cannot be
evaluated by accidentally recognizing the benchmark's modular permutation as
an arbitrary random stream; the first policy is explicitly sequential.
