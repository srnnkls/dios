# Bench Plan: write_run

| Field | Value |
|-------|-------|
| Metric & direction | Time to write a 64 MiB file sequentially through `Pool`, from the first submission until every write completes, ratio candidate/base, lower is better |
| Workload | `benches/write_run.rs`: one pool over the shipping `io_uring` backend, `DirectIo::Required`, a fresh empty file per rep, 16 write ops in flight, each op extending the file; no fsync inside the timed region |
| Baseline | One 4 KiB write slot per op (16,384 ops), the way sira's segment sink writes a draft today |
| Candidate | One 8-slot (32 KiB) run per op (2,048 ops), the size of sira's default data block |
| Reps | 30 interleaved reps of one 64 MiB file each (`dios::bench::run_paired`) |
| Threshold | one-sided 95% CI upper bound ≤ 0.60 |
| Compare command | `cargo bench --features bench --bench compare -- target/bench-samples/write_run.csv 0.60` |
| Escalation lever | If the bound fails, time 128 KiB runs (32 slots) in the same harness before changing the sink: if larger runs pass, the run size, not the mechanism, is the limit |

## Workload contract

An `O_DIRECT` write that extends its file cannot complete without blocking on
ext4's exclusive inode lock and block allocation, so io_uring hands it to an
io-wq worker, and the cost is paid per write. fio on this guest's virtio disk
(QD32, `O_DIRECT`, appending) moves 48.4 MiB/s in 4 KiB writes and 290.9 MiB/s in
128 KiB writes; sira's segment sink submits one 4 KiB slot per write, so a
draft is bound by the per-write cost. A run of adjacent write-arena slots is one
contiguous registered region, so it travels as one `WRITE_FIXED` with the same
buffer index a single slot uses, and its completion reports the run's bytes.

## Correctness before timing

`WriteArena::alloc_run(n)` claims `n` adjacent free slots or none (a lost race
rolls back every slot it claimed), never spans a registered-chunk boundary, and
refuses `n` above the run bound. A run's completion releases all of its slots;
a short write resubmits the run's exact tail; the mock records the run's length
on the attempt; a real-ring test reads back what a run wrote; the warmed pool
path allocates nothing. Existing single-slot behaviour is a run of one.

## Notes

Local, advisory; `nix` (NVMe 970 PRO, ext4) is binding, where block allocation
costs differ.

## Results (local, advisory)

| Check | Reps | Ratio geomean | One-sided 95% upper | Bound | Result |
|---|---:|---:|---:|---:|---|
| 64 MiB extending file, 16 in flight | 30 | 0.1980 | 0.2049 | 0.60 | PASS |

Median 1,164 → 225 ms per file (55.0 → 284.1 MiB/s); the candidate was faster
in all 30 reps, in line with fio's 4 KiB and 128 KiB appending rates on this
disk. Samples and the harness verdict are in `benches/evidence/write_run/`.
