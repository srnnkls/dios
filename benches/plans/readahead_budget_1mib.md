# Bench Plan: readahead_budget_1mib

| Field | Value |
|-------|-------|
| Metric & direction | Elapsed ratio candidate/base of the cold sequential scan, lower is better; read SQEs per steady interval and peak requests in flight as witnesses |
| Workload | `mmap_workloads` scan runner, `geometry` shape: 64 MiB cold sequential scan, full-page fold, 8 MiB payload arena, shipping `io_uring`, `DirectIo::Required`, one consumer pinned to CPU 0 |
| Baseline | the default budget, `scan-default-sample geometry:automatic:4:128:256` (512 KiB: C=128, R=256) |
| Candidate | the retained 1 MiB alternative of `readahead_coalescing.md`, `scan-sample geometry:automatic:4:256:512` (C=256, R=512, miss headroom 1,536, 1,793 minimum frames) |
| Reps | 30 alternating fresh-process pairs, one retained executable (`mw-refill2`, at `8938b82`) for both arms |
| Threshold | one-sided 95% CI upper bound ≤ 0.95 for the 1 MiB budget to count as a material scan improvement on this host; mmap SEQUENTIAL is sampled beside them for the RC-G1 comparison, advisory |
| Compare command | `cargo bench --features bench --bench compare -- <paired.csv> 0.95` |
| Escalation lever | If the bound fails, trace one candidate sample and attribute the time between device waits and the consumer's per-vector work (refill pass, polls); a default change stays an owner decision either way |

## Workload contract

The readahead refill keeps the window full, so the default scan now waits on
the device at four 128 KiB vectors in flight. fio on this guest's disk reads
128 KiB at depth 4 at 1,071 MiB/s and at depth 8 at 1,662 MiB/s, so doubling
the credits should cut device-bound scan time where the consumer keeps up. The
cost is frames: the 1 MiB budget needs 1,793 minimum frames against 897 and
pins up to 256 speculative frames instead of 128.

## Notes

Local runs on the 4-vCPU KVM guest are advisory; the owner selected 512 KiB on
`nix`, where depth 4 was past the knee. This plan measures a configuration that
already exists as an override and changes no default.

## Results (local, advisory)

One retained executable, `mw-refill2` (`64972e69…22d7`), 30 alternating
fresh-process pairs per comparison; samples in
`benches/evidence/readahead_budget_1mib/`.

| Comparison (candidate / base) | Ratio geomean | One-sided 95% upper | Bound | Result |
|---|---:|---:|---:|---|
| 1 MiB / 512 KiB | 0.9965 | 1.0719 | 0.95 | FAIL |
| 1 MiB / mmap SEQUENTIAL | 0.9921 | 1.0561 | 1.00 | advisory |
| 512 KiB / mmap SEQUENTIAL | 1.1076 | 1.1724 | 1.00 | advisory |

Medians: 512 KiB 65.4 ms against 1 MiB 61.5 ms; mmap 60.0 against 1 MiB 55.7
ms; mmap 57.4 against 512 KiB 64.0 ms. The escalation lever ran on the
driver probe (`probe-sample`, 768 MiB in 128 KiB groups, two rounds): READV
over scattered frames, one contiguous READ and READV over adjacent buffers
each read 1.0–1.4 GiB/s at depth 4 and at depth 8, with no consistent order.
This disk saturates at depth 4, so a 64 MiB scan bottoms out near 50–58 ms for
dios and mmap alike; the probe did not reproduce the earlier fio row of
1,662 MiB/s at depth 8. The default stays 512 KiB. The budget is now a builder knob,
`PoolBuilder::readahead_budget_bytes`, so a host whose storage does keep gaining
past depth 4 can select 1 MiB without the explicit page-count override; that
knob changes no path at its default, and its gate is the pinned regression
benches staying green.
