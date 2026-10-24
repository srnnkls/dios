# Bench Plan: synced_write

| Field | Value |
|-------|-------|
| Metric & direction | Time per durable 4 KiB write through `Pool`, from submission until its durability completion, ratio candidate/base, lower is better |
| Workload | `benches/synced_write.rs`: one pool over the shipping `io_uring` backend, `DirectIo::Required`, one 64-page file preallocated by writing it fully and syncing it, then QD1 overwrites of its pages in order, each made durable with `SyncMode::Data` before the next is submitted |
| Baseline | `submit_write`, wait for its completion, `submit_fsync(SyncMode::Data)`, wait for that: the sequence sira's commit log issues today |
| Candidate | `submit_write_synced(SyncMode::Data)`: one write whose completion covers its durability (`RWF_DSYNC` on the ring; write then sync on the eager backend) |
| Reps | 40 interleaved reps of 64 durable writes each (`dios::bench::run_paired`) |
| Threshold | one-sided 95% CI upper bound ≤ 0.85 |
| Compare command | `cargo bench --features bench --bench compare -- target/bench-samples/synced_write.csv 0.85` |
| Escalation lever | If the bound fails, trace the candidate's op with `strace -f` and the ring's worker threads: a synced write the kernel punts to io-wq still saves the second submission, so the next lever is linking the fsync to the write (`IOSQE_IO_LINK`) instead |

## Workload contract

sira acknowledges a durable commit after its log page's write completes and
then an fsync of the log completes, two ring round trips, the second always
executed by an io-wq worker. A write carrying `RWF_DSYNC` asks the kernel to
make that write durable before it completes. fio on this guest's virtio disk
(QD1, 4 KiB O_DIRECT overwrites of a preallocated file, write-back cache, no
FUA) sustains 4,056 durable writes per second as write then `fdatasync` and
5,847 as `dsync` writes (1.44×), which predicts about 0.70 here. The log is
preallocated by zero-filling, so its writes are pure overwrites, the case this
plan measures.

A synced write covers only its own range. On a device without FUA the kernel
follows it with a cache flush, which happens to cover earlier completed writes
too, but on a device with FUA it is written through and nothing else is
flushed, so no other write to the file, completed or not, becomes durable with
it. Callers with more than one page per durability point, or with earlier
writes not yet durable, keep the write-then-fsync form. (Corrected after the
results below: the first text claimed earlier completed writes were covered,
which holds only without FUA; the measurement is unaffected.)

## Correctness

Before timing: the mock records the requested durability on the write attempt,
the eager backend issues the sync after a complete write and reports a failed
sync as the write's error, and a real-ring test reads back what a synced write
put on disk. Zero-allocation evidence covers the new submit path.

## Flush witness

A candidate whose flag never reached the kernel would time as a bare write and
pass this gate easily, so the bench, before timing, runs one rep of each arm
between two reads of the backing disk's completed-flush counter (field 16 of
`/sys/block/<disk>/stat`) and prints flushes per durable write. On a disk with
a volatile cache and no FUA both arms must show one flush per write. On a disk
with FUA (the `nix` NVMe) a dsync overwrite becomes a FUA write and shows no
flush, so there the witness is the SQE unit test alone
(`a_synced_write_carries_its_durability_in_the_sqe_under_either_posture`,
which fails when the flag is dropped). This section was written after the first
run below; the witness code was in the bench before that run, and the gate and
timing protocol are unchanged.

## Results (local, advisory)

| Check | Reps | Ratio geomean | One-sided 95% upper | Bound | Result |
|---|---:|---:|---:|---:|---|
| Durable 4 KiB write, QD1 | 40 | 0.6693 | 0.7018 | 0.85 | PASS |

Median per durable write 290.1 → 182.7 µs; the candidate was faster in all 40
reps. Flush witness on `vda` (write back, FUA 0): base 1.00, candidate 1.00
flushes per durable write. Samples and the harness verdict are in
`benches/evidence/synced_write/`.

## Notes

Local, advisory; `nix` (NVMe 970 PRO) is binding, where the flush cost and the
punt cost differ.
