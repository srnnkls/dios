# Bench Plan: readahead_refill

| Field | Value |
|-------|-------|
| Metric & direction | Elapsed ratio candidate/base per lane, lower is better; read SQEs and demand singletons per steady 1,024-page interval, and the fraction of scan time with zero reads in flight, as structural witnesses |
| Workload | Primary: the default automatic scan of `benches/plans/readahead_coalescing.md` RC-G1, `mmap_workloads scan-default-sample geometry:automatic:4:128:256` (64 MiB cold sequential scan, full-page fold, 8 MiB payload arena, default C=128/R=256, shipping `io_uring`, `DirectIo::Required`, one consumer pinned to CPU 0). Secondary scan: `mmap_workloads automatic_scan` (`dios_automatic`, R=64, 1,024 frames). Controls: `automatic_fragmented` and `automatic_dependent` (`dios_automatic`), `cold_gather` (`dios_batch`), `resident_projection` (`dios_serial`) |
| Baseline | the retained `mmap_workloads` executable built at `775bf64`; the candidate is the same runner rebuilt on this change |
| Reps | 30 alternating fresh-process pairs per lane (even pairs base first), seeds `pair % 29` where the lane takes a seed |
| Threshold | one-sided 95% CI upper bound: primary ≤ 0.90; `automatic_scan` ≤ 1.00; every control ≤ 1.02. Witness, from one traced candidate sample: the steady interval holds at most 33 read SQEs and no one-page demand read |
| Compare command | `cargo bench --features bench --bench compare -- <paired.csv> <bound>` |
| Escalation lever | If the primary misses its bound or the witness fails, trace whether refill passes fire and whether admission defers on the speculative reserve (victims evicted while the triggering reader is pinned mature one pass later); the next lever runs the pass at that reader's next quiescent `get`. If a control regresses, profile the hit path: a repeated warm hit must execute no new work |

## Workload contract

Automatic readahead extends a confirmed stream only inside `poll`, and the
driver hands queued reads to the kernel only inside `poll`. Consumers poll when
they block (the scan runner, sira's `await_ready`), so a consumer that keeps up
with readahead hits through the whole window without polling: the device
drains, the consumer reaches the window's end, misses, issues a one-page demand
read and waits for it before the next window is admitted. On this guest the
traced base scan spends 29.6% of its time with no read in flight and its steady
1,024-page interval holds 41 read SQEs, 8 of them one-page demand reads, against
the readahead-coalescing requirement of at most 33 with no steady singleton.

The candidate leaves the repeated warm hit untouched. The first touch that
consumes a speculative read, which already publishes a bounded dirty
notification, now also counts; every vector-width consumptions the consuming
reader tries the control lock and, when it is free, runs one readahead pass:
reconcile consumption, admit the eligible vectors, then drain so the admitted
reads reach the kernel and finished ones publish. A held lock means another
thread is already making progress, so the reader does not wait.

Correctness witnesses precede timing: a Linux test where a consumer that polls
only while blocked scans a page-cache-resident file through the shipping ring
and sees no `Pending` after startup (RED before the change), the readahead,
retention, zero-allocation and loom suites.

## Measurement boundary and attribution

As in `readahead_coalescing.md`: preparation, pool construction and the cold
cache check are outside timing; timing ends after demand consumption and the
accepted reads drain. Both executables run identical configurations and
checksums; only the library differs. The mmap SEQUENTIAL arm of the same
configuration is sampled beside them for the RC-G1 comparison, advisory here.

## Notes

Local runs on the 4-vCPU KVM guest are advisory; the binding host is `nix`.
fio on this guest's virtio disk (O_DIRECT, `io_uring`, sequential): 4 KiB at
depth 16 reaches 34 MiB/s, 128 KiB at depth 4 1,071 MiB/s (about 60 ms per
64 MiB, the default budget's shape), 128 KiB at depth 8 1,662 MiB/s. A deeper
default budget is a separate owner decision and is not part of this change.

## Escalation record (first candidate)

The first candidate built the hit's `FrameGuard` before branching on the touch
result, so every warm hit staged the guard on the stack and copied it out with
16-byte loads over narrower stores. `resident_projection` read 1.1294 / 1.1758
against 1.02, and an in-process A/B of 64-page hits measured 19.05 → 24.22 ns.
The lever ran: the disassembly attributed the stall, and the branch now precedes
the guard, which is built in place again (64-page hits 20.75 → 20.07 ns in the
same A/B). Every lane is re-run on the corrected executable; the first
candidate's samples are not pooled.

Pre-registered before that re-run: `automatic_fragmented` and
`automatic_dependent` make no hit in either arm (1,024 misses each), so the
changed path does not execute there. Beside their gates, the base executable is
paired against itself on both lanes (30 pairs, characterization, no bound) to
report each lane's noise floor on this guest. Their bounds are unchanged.

## Results (local, advisory)

Retained executables: base `mw-final` (`251e83de…f413`, at `775bf64`) and the
corrected candidate `mw-refill2` (`64972e69…22d7`); samples, CPU time, per-sample
counters and the traced witness are in `benches/evidence/readahead_refill/`
(the first candidate's samples sit apart in `first_candidate/`). Thirty
alternating fresh-process pairs per lane.

| Lane | Ratio geomean | One-sided 95% upper | Bound | Result |
|---|---:|---:|---:|---|
| default automatic scan (primary) | 0.7747 | 0.8066 | 0.90 | PASS |
| `automatic_scan` | 0.9139 | 0.9789 | 1.00 | PASS |
| `resident_projection` | 0.9530 | 1.0005 | 1.02 | PASS |
| `cold_gather` | 0.9791 | 1.0385 | 1.02 | FAIL |
| `automatic_fragmented` | 0.9896 | 1.0346 | 1.02 | FAIL |
| `automatic_dependent` | 1.0208 | 1.0484 | 1.02 | FAIL |

Primary median 66.484 → 52.092 ms per 64 MiB scan. Witness on the candidate:
the steady 1,024-page interval holds 33 read SQEs, all 32-page speculative
vectors (base: 33 vectors and 8 one-page demand reads); the whole scan issues 4
demand reads (base 126) and at most 4 requests in flight. The remaining time is
the device's: at 128 KiB and depth 4 this disk delivers about 120 µs per
vector, which puts 512 vectors at about 61 ms, against fio's 60 ms.

The three failing controls make no hit in either arm (1,024 misses each), so
the changed path does not execute in them. The pre-registered A/A pairs (the
base executable against a byte-identical copy) miss the same bound:
`automatic_fragmented` 1.0107 / 1.0563, `automatic_dependent` 1.0051 / 1.0396,
and `cold_gather`, added after its result, 0.9902 / 1.0326. On this guest those
lanes cannot resolve a 2% margin in 30 pairs. **Owner decision required** before
these failed gates are waived; the binding run on `nix` can decide them.

Characterization (no bound), 12 alternating samples on the candidate:
mmap SEQUENTIAL median 59.2 ms (46.2–104.0), the default budget 61.9 ms
(53.8–74.0), the retained 1 MiB alternative (C=256/R=512) 55.2 ms (43.9–65.5).
