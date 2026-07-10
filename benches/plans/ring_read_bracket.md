# Bench Plan: ring_read_bracket

| Field | Value |
|-------|-------|
| Metric & direction | wall-time ratio candidate/base — driver ring read over the blocking `pread`, lower is better |
| Workload | QD1 4 KiB O_DIRECT reads at uniformly random 4 KiB-aligned offsets over a 64 MiB preallocated file (working set ≫ the 64-frame driver), both arms replaying the identical per-rep offset sequence; 40 reps × 8 iters. Base arm: blocking `pread` (`FileExt::read_exact_at`) into a 4 KiB-aligned user buffer from an O_DIRECT fd. Candidate arm: `Driver::submit_read` → `poll` → completion; the arm asserts a full-granule CQE (an error or short read fails the bench, mirroring the `pread` arm's `read_exact_at`) and probes one landed frame byte — no whole-granule copy-out, since sira borrows the frame in place |
| Baseline | the blocking `pread` arm is definitionally the baseline; no SHA is pinned — it is regenerated every run as the interleaved A arm |
| Reps | 40 (protocol minimum 30) × 8 iters_per_rep (device-bound: even one iter is ~70 µs, so 8 keeps reps well above the 1 µs floor) |
| Threshold | one-sided 95% CI upper bound of the ratio ≤ 1.25 |
| Compare command | `mise run gate target/bench-samples/ring_read_bracket.csv 1.25` |
| Escalation lever | flamegraph the ring submit-to-reap path (the `flamegraph` skill) and compare against the fio QD64 ceiling recorded in `measurements.md`; a ratio > 1.25 signals broken batching or submission, not device latency |

## Notes

Brackets the driver-level ring read against the classic syscall on a
device-bound QD1 4 KiB workload. At QD1 both arms are dominated by the ~60–90 µs
NVMe device latency, so the metric isolates the ring's per-op submit/reap
overhead: a ring path more than 25% over `pread` signals broken batching or
submission, since the device floor is common to both arms. Both arms open the
same file O_DIRECT (the ring via `Driver::open(..).direct()`, the `pread` arm via
`OpenOptions::custom_flags(O_DIRECT)`) so the comparison holds the data plane
fixed and varies only the issue path. The candidate asserts a full-granule CQE
so an error or short read fails the bench rather than counting as a fast
completion that would distort the ratio, and probes a single landed byte as a
bytes-landed check; it never copies the whole granule out, because sira borrows
the frame in place and a full copy would measure work the real caller never does.

Linux-only — the ring backend exists only on Linux, so there is no macOS arm.
Runs on the pinned host (Threadripper 3970X, NVMe 970 PRO, kernel 6.6) under the
scope's governor and cache-drop protocol, and advisorily on the arm64 container
(container numbers are advisory — VM storage). The gate is asserted by the shared
compare harness (`mise run gate`), never in-bench. The bench creates its own
preallocated temp file (full granules written and fsynced) under
`CARGO_TARGET_TMPDIR`/`temp_dir` and removes it on completion.
