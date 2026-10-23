# Bench Plan: epoch_barrier

**Status: REJECTED 2026-10-23 — the candidate and its bench were removed; the
fence pair stays.** Results and retained samples are below so the lever is not
retried blind.

| Field | Value |
|-------|-------|
| Metric & direction | Elapsed ratio candidate/base of warm `Pool::get` + guard drop over a fixed random page order, lower is better; base orders reader publication with the `SeqCst` fence pair, candidate with the asymmetric pair (compiler fence on the reader, `membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED)` on the poller) |
| Workload | `benches/epoch_barrier.rs`: two real `Pool<Driver>` instances (shipping `io_uring`) over one fixture, each with every page resident before timing; one reader pinned to one CPU; 65,536 and 1,024 resident 4 KiB pages; each iteration sums the first 64 bytes of one page from a fixed splitmix order |
| Baseline | the same executable's `EpochBarrier::Fence` pool, forced through the bench-only builder override |
| Reps | `dios::bench::run_paired`, 40 interleaved reps per lane, 4,096 gets per rep at 65,536 pages and 16,384 at 1,024 pages |
| Threshold | one-sided 95% CI upper bound ≤ 0.95 at 65,536 pages and ≤ 1.00 at 1,024 pages; `cold_gather` with the asymmetric pair ≤ 1.02 of the fence pair |
| Compare command | `cargo bench --features bench --bench compare -- target/bench-samples/epoch_barrier_<lane>.csv <bound>` |
| Escalation lever | If the cold lane fails, issue the heavy barrier only when the evict queue's oldest entry is unmatured and the free list is below the read bound; if a warm lane fails, profile the hit path and attribute the remaining stores/loads before touching the protocol |

## Workload contract

`ReaderSlot::begin_pin` publishes the reader's epoch and must order that store
before the mapping load; `advance_epoch` must order the eviction's unmap before
its scan of reader epochs. This is the store-buffering litmus, and either pair
forbids its relaxed outcome. The fence pair issues a full barrier on every first
pin. On x86-64 that barrier drains the store buffer and serializes the
reader's later loads, so independent misses of consecutive gets stop overlapping. The
asymmetric pair moves that cost to the advancing poller, which forces a barrier
on every running thread of the process only when an advance can reclaim
something (a non-empty evict queue).

Correctness is not measured by this bench: the loom suite keeps the `SeqCst`
pair (loom models fences, not `membarrier`), the protocol argument is recorded
in `src/pool/epoch.rs`, and the epoch, retention and zero-allocation suites run
against the shipping default.

## Measurement boundary and attribution

Fixture creation, pool construction, the warm fill and registration are outside
the timed region. Both pools are built before timing and share the request
order. The cold lane reuses `mmap_workloads cold_gather` with retained
executables, 30 alternating fresh-process pairs.

## Notes

Local runs on the 4-vCPU KVM guest are advisory; the binding host is `nix`.
`membarrier` falls back to the fence pair when the kernel or a seccomp policy
refuses `MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED`.

## Results (local, advisory) and disposition

The candidate ran on top of the direct residency map and the inlined hit path
(`benches/plans/residency_map.md`). Samples are in `benches/evidence/epoch_barrier/`.

| Lane | Ratio geomean | One-sided 95% upper | Bound | Result |
|---|---:|---:|---:|---|
| warm, 65,536 resident pages | 0.9914 | 1.0572 | 0.95 | FAIL |
| warm, 1,024 resident pages | 0.9415 | 0.9782 | 1.00 | PASS |
| `cold_gather`, 30 process pairs | 1.1099 | 1.1676 | 1.02 | FAIL |
| `automatic_scan`, 30 process pairs | 1.0147 | 1.0717 | 1.02 | FAIL |

At 65,536 pages the hit path is bound by its remaining data and metadata
misses, not by the fence. The cold lanes regress because the design pins one
epoch advance per poll whenever the evict queue is non-empty
(`tests/epoch_guards.rs`), and every advance needs the poller half of the
barrier: under the asymmetric pair that is a `membarrier` system call per
spin of a polling consumer. The pre-recorded escalation lever, a lazier
advance, would change those pinned semantics, so it is not taken. The fence
pair costs a few nanoseconds per first pin and nothing per poll beyond the
fence itself; a reader that wants fence-free gets nests them under one
guard.
