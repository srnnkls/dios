# Bench Plan: arena_population

| Field | Value |
|-------|-------|
| Metric & direction | wall-time ratio of constructing a 65,536-frame (256 MiB, 4 KiB granule) arena alone to constructing it and touching every frame (candidate/base), lower is better; the ratio is the share of the eager-fill open cost a build still pays |
| Workload | `TestFrames::preallocated(65_536, 4096)` per iteration on both arms; the base arm adds `populate()` (one zero fill per frame, the fill construction used to perform); one iteration per rep so each rep maps and unmaps a fresh span; 40 reps, interleaved A/B |
| Baseline | the eager arm itself (interleaved A/B): construction that still touches the span sits near 1.0; a mapping that reserves without touching pays only page tables, the `MADV_HUGEPAGE` hint, and the state and identity tables |
| Reps | 40 (protocol minimum 30) |
| Threshold | one-sided 95% CI upper bound of the ratio ≤ 0.10 |
| Compare command | `mise run gate target/bench-samples/arena_population.csv 0.10` |
| Escalation lever | a failing gate means construction touches the span again (a zeroing pass, a populating `madvise`, or a lock attempt that succeeded and paid the faults); profile the candidate arm with the flamegraph skill and attribute the faults to their call before touching the arena |

## Notes

Bench: `cargo bench --features bench --bench arena_population` writes
`target/bench-samples/arena_population.csv`; the gate is asserted only
by `mise run gate`, never in-bench. Both arms run in-process with no
kernel I/O; on Linux with `THP=never` the base arm takes one minor
fault per 4 KiB page (65,536 faults), on a `madvise` THP host one per
2 MiB, so the absolute base time varies by host while the ratio bound
holds on both.

Origin: the perf profile of sira's DM009 cold arms on the pinned host
(dios 8dca698, `tr-overlap-perf5.log`) attributed 453 ms of each ~1,015 ms
dios open to the arena's zero fill (242k minor faults per open over a
984 MB pool), 68 ms to a buffer-registration attempt that pinned every
page before the kernel refused it against an 8 MiB `RLIMIT_MEMLOCK`, and
102 ms of each close to unmapping the populated span. mmap's cold open
on the same store measured 15 ms.

The arena is now an anonymous private mapping whose pages materialise
on the first read into each frame, so an open charges the pool's
capacity, not its footprint. The arena lock attempt moved ahead of
posture selection: `mlock` checks the limit before it populates, so its
`ENOMEM` lets an `Auto` build skip the registration attempt that would
pin every page first. An explicit `Registered` posture still reaches
the kernel (`CAP_IPC_LOCK` exempts a ring, so the advisory limit alone
decides nothing). Locking or registering an arena populates it as the
kernel's part of that posture; on hosts where the limit admits both,
open cost is unchanged by design.

The product-level re-measurement is sira's DM009 cold-point and
cold-range-10 arms on the pinned host once sira pins this revision.
