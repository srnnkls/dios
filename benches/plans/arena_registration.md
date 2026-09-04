# Bench Plan: arena_registration

| Field | Value |
|-------|-------|
| Metric & direction | wall-time ratio candidate/base — pool read under the `Unregistered` posture over the same read under `Registered`, lower is better; the geomean is the recorded per-op cost of not registering |
| Workload | QD1 4 KiB `O_DIRECT` pool reads (`get` → `ready`/`poll`) at uniformly random 4 KiB-aligned offsets over a 64 MiB preallocated file (working set ≫ the 64-frame pool, so every read is a miss), both arms replaying the identical per-rep offset sequence; 40 reps × 8 iters. Base arm: a pool built with `RegistrationPolicy::Registered` (`READ_FIXED` against buffer index 0). Candidate arm: a pool built with `RegistrationPolicy::Unregistered` (plain `READ` addressing the same slab by pointer). Both arms assert `registration_posture()` before timing |
| Baseline | the `Registered` arm is definitionally the baseline; no SHA is pinned — it is regenerated every run as the interleaved A arm |
| Reps | 40 (protocol minimum 30) × 8 iters_per_rep (device-bound: one iter is ~70 µs, so 8 keeps reps well above the 1 µs floor) |
| Threshold | one-sided 95% CI upper bound of the ratio ≤ 1.15 |
| Compare command | `mise run gate target/bench-samples/arena_registration.csv 1.15` |
| Escalation lever | flamegraph both arms' submit-to-reap path; a ratio > 1.15 at QD1 means the unregistered path does more than skip the fixed-buffer lookup (a copy, a pin per op, or a broken batch) and blocks the `Unregistered` default until explained |

## Notes

The first dios-side measurement of what buffer registration buys. Literature
puts whole-pool gains at 4–6% (PostgreSQL 18, arXiv 2512.04859) and up to
11% on a YCSB-style buffer manager; at QD1 both arms share the ~60–90 µs
NVMe device floor, so the ratio isolates the per-op pin/unpin the kernel
performs for an unregistered `READ` against the registered lookup. The
number is recorded, not optimised: `Unregistered` is the default-compatible
posture under the stock 8 MiB `RLIMIT_MEMLOCK`, and the bound only guards
against the unregistered path being broken, not against it being slower.

The paired gate is one half of T14's acceptance. The other half is the
pinned regression bench `read_path_product` staying green when its pool is
built under each shippable posture; it takes the posture from
`DIOS_REGISTRATION_POLICY` (`auto` | `registered` | `unregistered`) and
prints the selected posture and lock readback, so no bench code forks per
posture. `pinned_frame_retention` builds its product executables from a
pinned worktree and runs `Auto`; at R8 scale under the stock limit that
resolves to `Unregistered`, the arm identity T1 records.

Linux-only — the ring backend exists only there; the eager backend has one
posture. The `Registered` arm needs `RLIMIT_MEMLOCK` ≥ 68 KiB plus the
ring's own accounting (well under the 8 MiB stock limit); the bench refuses
to run, rather than silently comparing two unregistered arms, if the
`Registered` build is refused. Runs on the pinned host (Threadripper 3970X,
NVMe 970 PRO, kernel 6.6) under the scope's governor and cache-drop
protocol. The gate is asserted by the shared compare harness
(`mise run gate`), never in-bench. The bench creates its own preallocated
temp file under `CARGO_TARGET_TMPDIR`/`temp_dir` and removes it on
completion.

## Recorded result

2026-09-04, nix (kernel 6.6.64, 970 PRO), advisory run outside the bench
profile (no governor pin, no cache drop), 64-frame pools, both arenas
locked: pairs 40, ratio geomean 0.9991, ci95 upper 1.0035, gate PASS at
1.15. At QD1 on this host registration buys nothing measurable; the
literature's 4–6% is a whole-pool number at depth, not a per-op one.
