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
| Compare command | `mise run gate target/bench-samples/frame_write_path_<case>_default.csv 1.03`, both arms built with default flags |
| Escalation lever | Profile the failed arm. If the claim compare-exchange shows, replace it with the load/store pair under the control lock, which already serializes every claim; the token stays. Never relax the bound silently. |

## Notes

Threadripper 3970X, ssh nix, kernel 6.6.64, performance governor, THP never,
CPU 2. No concurrent builds, tests or profilers during timing. Record source,
compiler features, binary and evidence hashes; keep every observation.

Pair driver: `mise run pair-bench frame_write_path <base_dir> <candidate_dir> <reps> <csv> -- <case>`
on the host alternates `base candidate` / `candidate base` per rep and appends
one `base_ns,candidate_ns` row per rep.

Touched paths and their expected cost:

- Miss claim: one `compare_exchange` replaces an Acquire load plus Release store.
- Completion record: `Option<InFlightFrame>` takes eight bytes per drained op —
  four for the frame index, four for the minting arena's id — growing
  `Completion` from 32 to 40 bytes. The batch is preallocated, so no allocation
  changes.
- Eager execute: `attempt` takes `&mut OpContext` instead of a copy.
- Hit path: `PinBegun`/`PinCommit` are zero-sized; `frame_bytes` and the hint
  validation take a witness reference that compiles to nothing.

Additional required gates, not statistical: `tests/zero_alloc.rs` on both
backends, every `tests/loom_pool.rs` schedule, the full mock-enabled suite, and
the pinned in-process ratio benches (`overlap`, `miss_table_pending_index`)
staying under their own recorded bounds.

## Superseded: the alignment-stable protocol amendment

An earlier amendment (2026-09-11) answered a failing default-build hits run —
1.0366, CI95 upper 1.0442 — by building both arms with 64-byte function and
block alignment, on the evidence that hardware counters were equal to within
0.1% and only cycles differed, making the gap a property of the link rather
than of the change. Review rejected that: a passing result on binaries that are
not shipped does not establish the bound for normal codegen, and the plan's own
rule sends a failed gate to its pre-recorded escalation lever rather than to a
new protocol.

The workload has since changed in ways that invalidate both of those runs, so
the question was settled by re-measuring rather than by choosing between them.

## Workload correction

`cold_miss` previously consumed a page, and on `Get::Busy` polled once and
returned without retrying it — so an iteration that never admitted still
counted toward `MISS_ITERS`. `Busy` is reachable here: the bounded claim pass
evicts at the current epoch but cannot age that victim by the required two
epochs in the same call. It now retries the same page through bounded
`Busy`/poll steps until it reaches `Pending`. The correction is observable:
with every iteration admitting, the misses arm overruns the mock's
construction-time event recorder at `queue_capacity` 16 384, which is why
`MOCK_QUEUE` is now 65 536. `MISS_INFLIGHT` is 64, so the mock's slab was never
the binding constraint at either value and admission behaviour is unchanged.

Both arms build from one `frame_write_path.rs`; the base arm carries the
baseline `src/` only.

## Re-gate, default builds

Recorded after the review fixes (arena-bound token, typed raw-read lease,
`route_completion_batch` extraction) and the workload correction. 30 interleaved
pairs per case, default flags, bench profile active, CPU 2:

| Case | ratio geomean | CI95 upper | Threshold | Result |
|------|---------------|-----------|-----------|--------|
| hits | 1.0159 | 1.0293 | 1.03 | PASS |
| misses | 0.9909 | 0.9927 | 1.03 | PASS |

The hits margin is 0.0007 — it passes, but this case stays placement-sensitive
and a re-link can move it. Corroborating alignment-stable arms, kept as
diagnosis and not as the gate: hits 1.0069 (CI95 upper 1.0200), misses 1.0139
(CI95 upper 1.0162). The escalation lever is untouched and still stands if a
future hits run fails.

## Validation status

Mac: 344 tests in the mock-enabled suite, 15 loom schedules, strict Clippy and
rustfmt. Linux nix: `tests/zero_alloc.rs` 23 passed;
`miss_table_pending_index` 1.0189 (CI95 upper 1.0277) against 1.0354 (CI95
upper 1.0495) on the baseline arm.

`benches/overlap.rs` aborts on this host — `mock event recorder stays within its
construction-time bound` — identically on the untouched baseline arm, so its
recorded bound is unverified for this change and the failure predates it.
