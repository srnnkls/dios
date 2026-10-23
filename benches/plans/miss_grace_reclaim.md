# Bench Plan: miss_grace_reclaim

| Field | Value |
|-------|-------|
| Metric & direction | Elapsed ratio candidate/base per lane, lower is better; `Get::Busy` count per sample as the structural witness |
| Workload | `mmap_workloads` lanes whose working set exceeds the pool, shipping `io_uring`, `DirectIo::Required`: `pressure_scan` (`dios_batch`, one reader, 16 outstanding, 196,608 sequential cold 4 KiB reads over the 65,536-page fixture into 16,384 frames), `pressure_random` (`dios_batch`, four readers, 131,072 random cold reads, 16,384 frames) and `automatic_scan` (`dios_automatic`, 16,384 sequential reads into 1,024 frames with automatic readahead) |
| Baseline | the retained `mmap_workloads` executable built at `89edeea`; the candidate is the same runner rebuilt on this change |
| Reps | 30 alternating fresh-process pairs per lane (even pairs base first), seeds `pair % 29` |
| Threshold | one-sided 95% CI upper bound ≤ 1.02 per lane; witness: zero `Busy` in every candidate `pressure_scan` sample, and the candidate `pressure_random` `Busy` median at most 5% of the base median |
| Compare command | `cargo bench --features bench --bench compare -- <paired.csv> 1.02` |
| Escalation lever | Profile the failing lane's miss path and attribute the added advance (one `SeqCst` fence and a reader-slot scan per evicting miss); if that is the cost, issue the second advance only after the first succeeded. `Busy` is not reintroduced for a victim no pinned reader overlaps |

## Workload contract

`claim_frame_bounded` used to evict one CLOCK victim and advance the epoch
once. The victim is tagged with the epoch in force at eviction and matures two
advances later, so once the pool was full every miss returned `Busy`, and the
caller's next `poll` freed the frame for the retry: one `Busy`, one extra
`poll` and one repeated `get` per cold read, with no reader pinned. The
candidate advances `EVICTION_GRACE_EPOCHS` (two) times after the eviction.
Back-to-back advances are as strict as advances at separate polls: a reader
pinned across the first blocks the second, so a guard that may still reach the
victim keeps the miss `Busy` exactly as before (DIO-G7, INV-1).

Correctness witnesses precede timing:
`a_scan_past_a_full_pool_reclaims_its_victim_within_the_miss_when_no_guard_pins_it`
(RED before the change at the first miss past the pool), the epoch, retention,
zero-allocation and DIO-G7 suites, and the loom suite.

## Measurement boundary and attribution

Fixture creation, cache preparation, pool construction and registration are
outside the timed region exactly as in `mmap_workloads`. Both executables run
identical lanes, seeds and checksums; only the library differs. Each sample's
`busy`, `polls` and `ready_checks` counters are retained beside its timing.

## Notes

Local runs on the 4-vCPU KVM guest are advisory; the binding host is `nix`.
This guest has no cgroup-v2 memory controller, so both local executables are
built from scratch worktrees that skip only the pressure lanes'
`MemoryMax=128M` precondition; that limit shapes the mmap arms' page cache,
and the `dios` arms read with `O_DIRECT` into their own frames. The binding
run keeps the precondition.

Pre-registered confirmation (recorded before running): a timing lane that fails
with the candidate median below the base median gets 100 further alternating
pairs with the same executables and bound, adopted whichever way it goes and
not pooled with the qualification sample.

## Results (local, advisory)

Retained executables: base `mw-wt-base` (`a5169c82…b86d`), candidate
`mw-wt-cand` (`22120887…3d47`), both carrying only the scratch precondition
bypass in `benches/evidence/miss_grace_reclaim/scratch-cgroup-bypass.diff`.
Thirty alternating fresh-process pairs per lane; samples, CPU time and
per-sample counters are in `benches/evidence/miss_grace_reclaim/`.

| Lane | Ratio geomean | One-sided 95% upper | Bound | Result |
|---|---:|---:|---:|---|
| `pressure_scan` | 0.9074 | 0.9362 | 1.02 | PASS |
| `pressure_random` | 0.9728 | 0.9917 | 1.02 | PASS |
| `automatic_scan` | 0.9415 | 1.0101 | 1.02 | PASS |

| Lane | `Busy` median base → candidate (candidate max) | `poll` median base → candidate |
|---|---:|---:|
| `pressure_scan` | 180,225 → 0 (0) | 918,113 → 847,907 |
| `pressure_random` | 113,004.5 → 4,907 (12,058) | 246,449.5 → 179,090 |
| `automatic_scan` | 13 → 0 (0) | 98,657.5 → 65,737 |

Witness: zero `Busy` in all 30 candidate `pressure_scan` samples, and the
candidate `pressure_random` median is 4.3% of the base median → PASS. In the
base, every `pressure_scan` miss after the pool filled (196,608 − 16,384 + 1)
was `Busy` once. The candidate's remaining `pressure_random` `Busy` answers are
misses that overlapped another reader's pin, the case the design keeps.

The frozen DRP-G2 cycling proof (`benches/read_path_product.rs`) observed each
reuse cycle as a `Busy` miss, a poll-reported reclaim and a second `get`. The
same victim is now reclaimed inside the admitting `get`, so the proof (schema
`dios-drp009-cycling-proof-v2`) admits through `Busy` answers when an older
library gives them, counts them, and observes the reclaim with a frame-state
census taken right after admission: 63 `Resident`, one `InFlight`, nothing
`Free` or `Evicting`, where every frame was `Resident` before. Its
expectations (96 completed reads, 32 evictions, 32 reclamations, 32 reuses)
are unchanged; `tests/drp009_gate_contract.rs` now pins zero `Busy` before
admission.
