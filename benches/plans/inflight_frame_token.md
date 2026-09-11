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
| Compare command | `mise run gate target/bench-samples/frame_write_path_<case>.csv 1.03`, both arms built with `RUSTFLAGS="-C llvm-args=-align-all-functions=6 -C llvm-args=-align-all-nofallthru-blocks=6"` (protocol amendment below) |
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
- Completion record: `Option<InFlightFrame>` adds four bytes per drained op;
  the batch is preallocated, so no allocation changes.
- Eager execute: `attempt` takes `&mut OpContext` instead of a copy.
- Hit path: `PinBegun`/`PinCommit` are zero-sized; `frame_bytes` and the hint
  validation take a witness reference that compiles to nothing.

Additional required gates, not statistical: `tests/zero_alloc.rs` on both
backends, every `tests/loom_pool.rs` schedule, the full mock-enabled suite, and
the pinned in-process ratio benches (`overlap`, `miss_table_pending_index`)
staying under their own recorded bounds.

## Protocol amendment: alignment-stable arms

Recorded 2026-09-11 after the first candidate run, before any threshold change;
the 1.03 bound is untouched. Default-build hits pairs measured candidate/base
1.0366 (CI95 upper 1.0442, FAIL) while misses measured 0.9996. Hardware
counters on the hits binaries were equal to within 0.1% in instructions,
branches, branch misses and L1d loads, differing only in cycles; `pin_owned`
and the warm prefix of `Pool::get` disassemble identically and the `Pool`
field offsets are unchanged. The difference is code placement on the Zen 2
front end, a property of the link, not of the change. Both arms are therefore
built with 64-byte function and non-fallthrough block alignment for this gate,
removing placement luck from the ratio. Under that protocol: hits 0.9921
(CI95 upper 0.9990), misses 1.0051 (CI95 upper 1.0083), both PASS. Raw
default-build samples are kept beside the gated ones in
`benches/evidence/inflight_frame_token/`.

## Validation status

Mac: 339 tests in the mock-enabled suite, 15 loom schedules, the five token
unit tests under Miri, strict Clippy and rustfmt. Linux nix: the io_uring suite
passes except `drp009_gate_contract::linux_flamegraph_bounds_perf_mmap_ring_for_eight_mib_memlock_host`
and `r7_source_manifest::r7_source_manifest_reconstructs_the_clean_extraction`,
both of which fail identically on the untouched baseline tree on that host.
`tests/zero_alloc.rs` passes on both backends.
