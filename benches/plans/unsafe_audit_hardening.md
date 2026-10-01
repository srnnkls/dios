# Bench Plan: unsafe_audit_hardening

Recorded before implementation, 2026-10-01. Baseline: dios
c1733cf819efaeb140c0b408178b3db1c47e05d6.

Four soundness fixes from the 2026-10-01 unsafe audit:

- F1: ring teardown drains with blocking waits under an init-set bound and
  leaks every kernel-reachable allocation when the bound is exhausted, instead
  of spinning to an assertion inside `Drop`.
- F2: `ReadVector::drop` aborts only tokens that were never submitted;
  submitted tokens are released by their completion or leaked with the driver.
- F4: the reader witness that unlocks `frame_bytes` is bound to the frame it
  validated, frame identities route through a loom-visible cell, and the loom
  suite runs from mise.
- F3: io_uring submission and completion queue access requires the AD-4 lock
  guard by type.

F1 and F2 touch teardown only. F3 changes signatures on the submit and reap
paths without adding work. F4 adds a frame index to a witness on the hit path,
which is the one change with a plausible cost.

| Field | Value |
|-------|-------|
| Metric & direction | wall-time ratio candidate/base, lower is better |
| Workload | `benches/frame_write_path.rs`, fresh process per sample: `hits` = 4096 × 4096 warm `Pool::get` over a 64-frame fully resident mock pool; `misses` = 64 × 2048 cold `get → poll → ready` cycles over a 256-frame mock pool at full occupancy |
| Baseline | c1733cf built into `build/dios-base` on the host |
| Reps | 30 interleaved fresh-process pairs per case, order reversed on alternate reps |
| Threshold | one-sided 95% CI upper bound of the ratio ≤ 1.03 for both cases |
| Compare command | `mise run gate target/bench-samples/frame_write_path_<case>_default.csv 1.03`, both arms built with default flags |
| Escalation lever | Profile the failed `hits` arm. If the witness frame index shows, keep the binding as a zero-sized lifetime-branded witness instead of a stored index. Never relax the bound silently. |

## Notes

Threadripper 3970X, ssh nix, kernel 6.6.64, performance governor, THP never,
CPU 2. No concurrent builds, tests or profilers during timing.

Pair driver: `mise run pair-bench frame_write_path <base_dir> <candidate_dir> <reps> <csv> -- <case>`.

Additional required gates, not statistical: `tests/zero_alloc.rs` on both
backends, every `tests/loom_pool.rs` schedule, the mock-enabled suite, miri
(`mise run miri`) and asan (`mise run asan`).

## Workload correction

At c1733cf the `misses` case panics on both arms (`a never-read page cannot
hit`): automatic readahead, default-on since #17, prefetches the sequential
cold pages the case requires to miss. Both arms now build the pool with
`Readahead::Disabled`, restoring the stated workload of every miss claiming
through CLOCK. The base arm carries that one bench-only change.

## Result

2026-10-01, nix, bench profile active, governor performance, THP never, CPU 2,
gate mise 2026.8.0 via `MISE_EXE`. 30 interleaved pairs per case, default flags.

| Case | ratio geomean | CI95 upper | Threshold | Result |
|------|---------------|-----------|-----------|--------|
| hits | 1.0038 | 1.0160 | 1.03 | PASS |
| misses | 0.9998 | 1.0023 | 1.03 | PASS |

Binaries: base `fc32a46a…`, candidate `6e3d2010…` (sha256 of
`frame_write_path-4c09c469f298852e`). Non-statistical gates: mock suite 360
(macOS) / 369 (Linux) passed, `mise run loom` 17 passed, `mise run miri` 52
passed, `mise run asan` 33 suites / 265 passed.
