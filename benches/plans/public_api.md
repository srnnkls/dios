# Bench/Test Plan: public_api

| Field | Value |
|-------|-------|
| Metric & direction | The surface-only changes add no data-plane work; completing the exposed ring write remains covered by the scope's DIO-G8 new/old wall-time ratios, lower is better |
| Workload | Compile the external-user contract in `tests/public_api.rs`; use the seeded mock's explicit unsupported-direct capability to check Required refusal vs Preferred fallback; run the existing zero-allocation and pool/driver suites; DIO-G8 later exercises segment flush and journal micro-commit on the pinned Linux host |
| Baseline | Current `feat/darwin-io-model` behavior before the public-surface change |
| Reps | Not applicable to the compile contract; DIO-G8 uses at least 30 interleaved repetitions and the other statistical plans retain their recorded repetitions |
| Threshold | `tests/public_api.rs` compiles and passes, including distinct PoolBuildError configuration/driver-init variants and the direct-policy behavior; zero allocations remain zero; existing pool/driver gates stay green; DIO-G8 CI upper bound ≤ 1.02 for each new/old ratio |
| Compare command | `mise run gate target/bench-samples/write_plane_segment.csv 1.02` and `mise run gate target/bench-samples/write_plane_journal.csv 1.02`; plus the existing commands in `benches/plans/pool_warm_path.md`, `benches/plans/uring_read_path.md`, and `benches/plans/overlap.md` |
| Escalation lever | Remove any wrapper that adds data-plane work; for a DIO-G8 segment failure, increase WriteArena batch depth, then block direct segment writes as recorded in the scope |

## Notes

`DirectIo` is an init-time ADT, pool file registration is a control-plane
operation, and moving advanced names under `dios::driver` has no generated-code
cost. `Get::{Hit, Pending, Busy}` remains the single scheduler-facing residency
ADT. `PendingToken` is a passive recheck capability: dropping it discards the
caller's interest while the admitted operation drains; it needs no custom empty
`Drop` implementation or waiter allocation.

The pool is the only public read-observation surface: `FrameGuard` provides the
zero-copy resident borrow. The advanced driver contract pins write-slot
ownership and completion draining only; it intentionally exposes neither a
copying raw-read escape hatch nor an unsound unleased frame borrow.
