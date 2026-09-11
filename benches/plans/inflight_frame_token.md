# Bench Plan: inflight_frame_token

Recorded before implementation, 2026-09-11. Baseline: dios
d7548f8ed4e7ac8b8262b380381ae31d8934e9fa.

Writer exclusivity over a frame moves from the residency word plus external
discipline to a linear `InFlightFrame` token minted by one compare-exchange
(`Free → InFlight`), owned by the driver op for the life of the transfer, and
consumed by `publish`/`abort`. The driver's separate `raw_read_inflight` flag
table is removed. Reader access to frame bytes and exact identities takes a
zero-sized pin or lock witness. No new allocation, no new atomic on the hit path.

| Field | Value |
|-------|-------|
| Metric & direction | wall-time ratio candidate/base, lower is better |
| Workload | `benches/frame_write_path.rs`, fresh process per sample: `hits` = 4096 × 4096 warm `Pool::get` over a 64-frame fully resident mock pool; `misses` = 64 × 2048 cold `get → poll → ready` cycles over a 256-frame mock pool at full occupancy, every miss claiming through CLOCK |
| Baseline | d7548f8 plus the bench-only commit that adds `frame_write_path` (no `src/` change), built into `build/dios-base` on the host |
| Reps | 30 interleaved fresh-process pairs per case, order reversed on alternate reps |
| Threshold | one-sided 95% CI upper bound of the ratio ≤ 1.03 for both cases |
| Compare command | `mise run gate target/bench-samples/frame_write_path_<case>.csv 1.03` |
| Escalation lever | Profile the failed arm. If the claim compare-exchange shows, replace it with the load/store pair under the control lock, which already serializes every claim; the token stays. Never relax the bound silently. |

## Notes

Threadripper 3970X, ssh nix, kernel 6.6.64, performance governor, THP never,
CPU 2. No concurrent builds, tests or profilers during timing. Record source,
compiler features, binary and evidence hashes; keep every observation.

Pair driver: `mise run pair-frame-write-path <base_dir> <candidate_dir> <case> <reps> <csv>`
on the host alternates `base candidate` / `candidate base` per rep and appends
one `base_ns,candidate_ns` row per rep.

Touched paths and their expected cost:

- Miss claim: one `compare_exchange` replaces an Acquire load plus Release store.
- Completion record: `Option<InFlightFrame>` adds four bytes per drained op;
  the batch is preallocated, so no allocation changes.
- Eager execute: `attempt` takes `&mut OpContext` instead of a copy.
- Hit path: `PinBegun`/`PinCommit` are zero-sized; `frame_bytes` and the hint
  validation take a witness reference that compiles to nothing.

Additional required gates, not statistical: `tests/zero_alloc.rs` on both
backends, every `tests/loom_pool.rs` schedule, the full mock-enabled suite, and
the pinned in-process ratio benches (`overlap`, `miss_table_pending_index`)
staying under their own recorded bounds.
