# Bench Plan: wake_elision

| Field | Value |
|-------|-------|
| Metric & direction | Elapsed ratio candidate/base for the `cold_gather` lane's `dios_batch` arm, lower is better; per-wake platform signal count as the structural witness |
| Workload | `mmap_workloads` `cold_gather`: 1,024 random cold 4 KiB pages of the 65,536-page fixture, 16 outstanding `Pool::get` misses, shipping `io_uring`, `DirectIo::Required`, one reader pinned to CPU 0, busy `poll_report` progress |
| Baseline | `main` at `ade5310`, retained executable; the candidate is the same runner rebuilt on this change |
| Reps | 30 alternating fresh-process pairs (even pairs base first, odd pairs candidate first), seeds `pair % 29` |
| Threshold | one-sided 95% CI upper bound of the ratio ≤ 1.00 |
| Compare command | `cargo bench --features bench --bench compare -- <paired.csv> 1.00` |
| Escalation lever | `strace -c -f` both arms over one sample and attribute the remaining `write`/`futex`/`read` syscalls per miss; the wake protocol itself is not weakened |

## Workload contract

Every admitted demand miss, explicit or automatic prefetch admission, product
submission and reaped caller completion calls `WaitState::wake`. Before this
change each call takes the generation mutex, issues `futex(FUTEX_WAKE)` through
`Condvar::notify_all` and, on the shipping backend, writes the private eventfd.
The eventfd write additionally completes the armed `POLL_ADD`, so the next reap
drains the eventfd with a `read` and re-arms the poll with another SQE. The
candidate keeps the generation bump under the same mutex and signals only when
`parks_in_progress` is nonzero, which both park paths increment under that
mutex before they block.

The witness is a test-only signal counter: a wake with no parker signals zero
times and the next `wait` returns without parking; a wake during an entered
park signals exactly once and exits the park through the wake path. The
existing wait, retention and teardown suites keep their exact counters.

## Measurement boundary and attribution

Fixture creation, cache discard, pool construction and the pre-timing warm
page are outside the timed region, exactly as in `mmap_workloads`. Both arms
run the identical lane, seed and checksum. CPU time per arm is retained beside
elapsed time because the arm spins in `poll_report`, so CPU tracks elapsed.

## Notes

Local qualification ran on a 4-vCPU KVM guest with a virtio disk
(`/dev/vda`, mq-deadline) and is advisory. The binding host is `nix` under
the `bench-host-guard` protocol. The lane's mmap arm is not part of this
comparison.

## Results (local, advisory)

Qualification, 30 pairs, retained executables `mw-base-ade5310`
(`b583c784…be15d`) and `mw-wake`: geomean **0.9598**, one-sided 95% upper
**1.0202** against 1.00 → **FAIL** under the pre-registered bound. The
escalation lever ran next. `strace -f -c` over one seed-3 sample of each
executable (1,024 misses plus setup):

| syscall | base | candidate |
|---|---:|---:|
| `futex` | 1,141 | 5 |
| `write` | 1,137 | 1 |
| `io_uring_enter` | 268 | 104 |
| `read` | 355 | 198 |
| total | 3,095 | 502 |

The removed calls are exactly the per-miss futex wake, eventfd write, and
the reap-side eventfd drain plus `POLL_ADD` re-arm submission that each
spurious eventfd completion forced. Roughly 2.5 syscalls per miss are gone;
on this guest they cost about 0.7 µs of an 18.5 µs QD16 miss.

Pre-registered confirmation (recorded before running): 100 alternating
pairs of the same two executables, same lane, arm, seeds `pair % 29` and
bound 1.00; the result is adopted whichever way it goes and is not pooled
with the qualification sample.

Confirmation, 100 pairs: geomean **0.9637**, one-sided 95% upper **0.9981**
against 1.00 → **PASS**, adopted under the rule above. Median elapsed per
sample 17.294 → 16.815 ms; median CPU 17.185 → 16.347 ms. The binding
re-run on `nix` remains owed.
