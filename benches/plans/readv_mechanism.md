# Bench plan: unregistered READ versus READV

Written before implementation, 2026-09-13, following the owner's steering.
Benchmark-only driver bypass, continuing row 2 of the plan-prefetch evidence
ladder. No product coalescing, run allocator or span publication is implemented.

| Field | Value |
|---|---|
| Metric & direction | Candidate/base elapsed time, lower is better; CPU ns/4 KiB, user/system CPU, exact submitted SQEs and reaped CQEs, bytes, submit/poll calls and maximum accepted reads/bytes |
| Workload | Read the existing immutable 256 MiB fixture three times, consuming every 4 KiB using the common u64 fold. One 128 KiB group at a time, 32 file-consecutive pages; all accepted reads drain before consuming/reusing the group's storage. CPU 0, controller CPU 4, nix performance governor and THP never, ext4/Samsung 970 PRO; direct I/O required, buffers unregistered, fixed-file registration shared by all arms |
| Baseline | (a) 32 independent READ SQEs into scattered 4 KiB destinations; (b) one READV SQE with the same 32 scattered iovecs; (c) one READ SQE into a virtually contiguous 128 KiB buffer. The READV/contiguous pair uses c as base; the amortization pair uses a as base |
| Reps | Two qualification pairs and 30 alternating fresh-process pairs for each comparison; identical useful bytes, group order, consumption and final drain |
| Threshold | The "READV is free" claim means one-sided 95% upper of b/c <= 1.05 on this probe. Report b/a as characterization, without an invented adoption threshold |
| Compare command | Retained shared `compare paired.csv 1.05` or `mise run gate paired.csv 1.05`; summaries use the retained mmap_workloads executable's `summarize` command |
| Escalation lever | Reject non-direct posture, short/error completions, wrong/duplicate completion tokens, unequal checksums/bytes, timed allocation, nonempty final ring or host contamination. Retain a failed b/c gate and profile both exact arms before any explanation or design. Do not relax the gate, change queue budgets or start a run allocator |

## Workload contract

The 128 KiB accepted-byte ceiling is identical: 32 small requests or one large
request. This is a submit-group/drain-group closed loop, not the previous
2 MiB ceiling or a continuously replenished device-saturation test. The same
32 page identities are consumed in file order after each group completes.
Use separate 4 KiB-aligned slots with gaps for the scattered destinations,
preallocated and touched before timing; physical contiguity is not asserted.
Use the same bounded arena allocation and 32-entry iovec storage in every arm.
The contiguous arm uses a 128 KiB subrange; gaps and unused storage are recorded.

One bare io_uring is owned by the probe. It uses the existing backend's ordinary
ring setup and fixed-file posture, without registered buffers or special poll
flags. Ring and destination lifetimes are contained in the probe; destinations
and iovecs stay live and unmodified through all completions. Error paths drain
accepted I/O before ordinary destruction. If the bounded drain cannot complete,
terminate the dedicated process rather than reclaim kernel-visible storage.
Expected I/O failures are reported as failed samples, never ignored or retried
until a favorable observation appears.

All initialization, cold file-local preparation, buffer population and ring/file
registration are outside timing. Timing includes SQE construction, submission,
polling, exact CQE checks, full-page consumption and terminal drain. Report
OS input bytes and both thread CPU clocks; record the fixture and executable
hashes, device `read_ahead_kb`, queue limits and host snapshots. Primary runs
contain no per-request timestamps. Event counts identify the mechanism directly;
extra tracing is unnecessary unless a failing gate needs attribution.

## Interpretation and owner decision

This isolates the cost of three submission mechanisms at equal useful work.
It cannot establish one bio or NVMe command per READV. The block layer may split
or merge requests according to device/stack limits, and block tracepoints are
unavailable to the benchmark account. Major-fault counts likewise do not count
block reads. See the [liburing READV contract](https://man7.org/linux/man-pages/man3/io_uring_prep_readv.3.html)
and [Linux 6.6 queue limits](https://github.com/torvalds/linux/blob/v6.6/Documentation/ABI/stable/sysfs-block).

If b/c passes, ordinary vectored reads are a measured alternative for the
Unregistered posture. That does not resolve Registered READ_FIXED handling,
mixed residency, completion fanout, ownership or retention. The open decision
and span-slab dependency remain in [scan_geometry.md](scan_geometry.md).
