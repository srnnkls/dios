# Bench plan: mmap workloads

Written before implementation, 2026-09-13. Benchmark-only work; no product
algorithm, topology, ordering, format, or performance gate changes.

| Field | Value |
|---|---|
| Metric and direction | Paired whole-workload elapsed `Dios / mmap`, lower is faster; also ns/read, worker CPU, page faults, context switches, bytes read, and memory reclaim |
| Workload | Sealed, fully initialized and synced 256 MiB file, 4 KiB pages, deterministic requests; matrix below |
| Baseline | Actual read-only `MAP_SHARED` file mapping, with explicit NORMAL, RANDOM, or SEQUENTIAL advice; never a mock or a depth-one Dios arm |
| Repetitions | 30 alternating AB/BA fresh-process pairs per comparison, same seed/work in each pair; 2 untimed qualification pairs; smoke is 1 pair and cannot emit accepted ratios |
| Threshold | Characterization has no adopted superiority threshold. Acceptance requires exact checksums/operation counts, zero timed allocations, verified cache posture, complete drains, and all pinned regression gates staying green. Historical DRP-G2 bounds 1.01/1.01 and DRP-G4 bounds 1.00/0.50 are unchanged |
| Compare command | `mmap_workloads summarize <paired.csv>` uses `dios::bench::ratio_gate`; existing regression CSVs use `mise run gate <csv> <existing-bound>` |
| Escalation lever | Failed path witnesses invalidate the campaign, preserve artifacts, and repair preparation/instrumentation before rerunning. Performance losses remain visible; profile the losing arm without altering product code or relaxing a gate |

## Questions and workloads

These are storage-engine-shaped page kernels, not complete database queries.
Both arms execute the same page fold or 64-byte projection and checksum every
result. The dependent walk obtains its next page from the current page's first
word; it cannot look ahead. Independent requests use a fixed permutation, with
no RNG or request-vector growth in the timed loop.

| Lane | Work and initial residency | mmap baseline | Dios candidate |
|---|---|---|---|
| resident_projection | 65,536 projections over 1,024 hot pages | RANDOM, PTEs populated | ordinary guarded get, warm pool |
| resident_decode | 8,192 full-page folds over 4,096 hot pages | RANDOM, PTEs populated | ordinary guarded get, warm pool |
| minor_projection | 4,096 unique projections; all pages in page cache, PTEs dropped | RANDOM, demand minor faults | same pages resident in pool |
| cold_point | 1,024 fragmented projections, target pages absent from page cache/pool | RANDOM | one outstanding read |
| cold_gather | 1,024 independent full-page folds, cold | RANDOM, plus separately paired NORMAL control | window 16 on one worker |
| faulting_readers | 4,096 fragmented full-page reads, cold, four workers | RANDOM, one fault per worker | shared pool, one outstanding per worker |
| dependent_walk | 1,024 cold dependent projections | RANDOM | window one |
| scan_decode | 16,384 contiguous full-page folds, cold | SEQUENTIAL, plus separately paired NORMAL control | window 16 |
| hotspot_mixed | 8,192 projections: every tenth access is a unique cold page; others sample 1,024 prefaulted pages | RANDOM | window 16, same trace |
| pressure_scan | three passes over all 65,536 pages, full-page folds | SEQUENTIAL | window 16 |
| pressure_random | two permuted passes over all pages, four workers, full-page folds | RANDOM | window 16 per worker |

Pressure arms each run in a new user systemd service with `MemoryMax=128M`,
`MemorySwapMax=0`, and a 180-second runtime bound. The 256 MiB dataset exceeds
that limit. Dios uses a fixed 64 MiB frame arena for pressure lanes; mmap uses
the available file-cache budget. The cap covers each whole process and its
charged memory, not equal payload-cache capacity. Capture memory.stat/events
and pressure before/after; require positive reclaim and refault evidence for
the mmap pressure-random lane. Scan SEQUENTIAL advice may explicitly release
pages; distinguish that from memory-limit reclaim. Do not claim reproduction
of the paper's 2 TB, 100 GB page-cache, 1/10 SSD experiments on this host.

## Preparation and timing

- Linux primary host: nix, 3970X, kernel 6.6.64, Samsung 970 PRO, ext4,
  performance governor, THP never. Workers pinned to CPUs 0-3; single-worker
  lanes CPU 0. Record filesystem, page size, affinity, memory cap, host load,
  source/executable/fixture hashes and resolved I/O/registration posture.
- Input creation happens once outside all samples using create-new, initialized
  non-sparse bytes and sync. No raw device access, global cache drop, or updates
  to a live mapped file. Each process owns its mapping until all readers join.
- Before every cold arm, close prior mappings, sync the fixture, then issue
  file-local `POSIX_FADV_DONTNEED`. Verify target residency with `mincore`;
  an ignored hint is a failure, not proof of coldness. Warm only declared hot
  pages. Cold means absent from OS cache/pool, never guaranteed NAND-cold.
- Minor-fault preparation warms the file, discards only mapping PTEs with
  `MADV_DONTNEED`, and verifies page-cache residency. It must record positive
  minor faults and zero major faults. Fault-around means faults need not equal
  pages. Check the present bit in the process's own `/proc/self/pagemap` before
  timing as well: minor targets must have absent PTEs but resident cache pages;
  warm targets must have present PTEs. No physical frame number is requested.
  Resident lanes require zero major faults; minor counters can include
  non-file process pages, so no exact zero-minor requirement is invented.
- Dios uses the shipping backend, `DirectIo::Required`, explicit unregistered
  buffers. Pool construction, caller-scratch touches, registration, prefill, mapping,
  advice, request construction, thread creation/join, and artifact output are
  outside the timed worker loop. A spare page primes lazy backend state.
- Primary wall time spans dispatch to completion of all useful work; per-worker
  elapsed and thread CPU are retained. Capture worker `getrusage` deltas for
  minor/major faults and voluntary/involuntary switches. Record `/proc/self/io`
  and cgroup snapshots outside timing. These byte counters are OS accounting,
  not physical NAND traffic. Global TLB interrupt deltas are context only.
- Bound requests, traces, pending windows, workers, polls (1,000,000 per progress
  episode), and subprocess runtime. Empty completion interests at finish;
  failures are errors and leave an incomplete campaign manifest.

Qualification confirmed an important first-fill cost: the shipping unregistered
arena and control tables reserve virtual memory lazily. Cold Dios arms therefore
also incur **minor anonymous faults** as unused capacity materializes; they do
not incur file-backed major faults. These are included and counted, not hidden
as zero-allocation work. Only declared hot pages are prefilled. The pressure
traces distinguish initial fill from later passes; do not call a first-fill
batch a pure steady-state measurement. No product population policy was changed.

## Diagnostic evidence and cost model

Replay identical work with a preallocated trace of per-request acquire-to-consume
spans and fixed-size progress blocks. Primary timing compiles request clocks
out. Interleave trace-off/on pairs for observer overhead before interpreting
latency percentiles. These are closed-loop service spans, not arrival-time SLOs.
Store trace/observer artifacts on task-owned tmpfs and copy them only after
collection, so an earlier arm's buffered trace writes do not compete with the
next arm's input reads. The fixture remains on the NVMe. Stream serialization
through a fixed buffer; a JSON object tree for every event exceeds the memory
limit even though it is created after timing.
Record phase/block throughput through initial fill and subsequent passes so
reclaim stalls cannot disappear into a single throughput average.

Use matched executable CPU profiles recorded to task-owned tmpfs. Preserve raw
perf statistics, lost/throttled samples, folded stacks and workload-only SVGs.
The representative profile set is resident_projection, cold_gather,
pressure_scan, and pressure_random, both arms. Initially use cycles/100,000 for
short-worker lanes and cpu-clock/997 Hz for the long pressure workers; require
at least 100 workload samples and zero lost records. Pressure profiles attach
from outside the workload cgroup: a task-owned wrapper stops before exec, perf
acknowledges enabled counters, then the wrapper continues into the exact
retained benchmark. The recorder's memory and tmpfs output are thereby outside
the 128 MiB cap. Profiles remain separate, perturbed diagnostic replays.
CPU budgets scale **unprofiled worker CPU**, not wall time. Model:

```text
mmap CPU ~= projection/decode + minor_faults*c_minor
          + major_faults*c_fault_submission + reclaim/page_table work
Dios CPU ~= projection/decode + hits*c_get + misses*c_admit
          + polls*c_poll + ready_checks*c_ready + frame_reclaim + anonymous_faults*c_anon
wall ~= critical path through CPU, blocking faults, overlapped I/O and scheduling
```

Major-fault latency includes I/O and scheduling; CPU samples cannot assign that
whole latency to the fault handler. Page-fault counts are not device requests.
Without kernel symbols/tracepoint permissions, TLB-shootdown and reclaim CPU
coefficients remain unidentified. Warm, minor, cold-serial, batched, dependent,
and pressure controls discriminate hypotheses; do not fit one universal ns/fault.

## Research basis

- [Crotty, Leis, Pavlo, CIDR 2022](https://vldb.org/cidrdb/papers/2022/p13-crotty.pdf):
  separates resident pointer access from blocking faults and reclaim overhead;
  read-only random and sequential experiments, advice variants, CPU/IO/TLB data.
- [mmapbench](https://github.com/viktorleis/mmapbench): source reads one byte
  per logical page access and reports page-equivalent throughput. Our useful
  byte counts reflect actual projection/full-page consumption. The upstream
  unbounded raw-device runner and global cache drops are not used here.
- [mincore](https://man7.org/linux/man-pages/man2/mincore.2.html),
  [madvise](https://man7.org/linux/man-pages/man2/madvise.2.html),
  [posix_fadvise](https://man7.org/linux/man-pages/man2/posix_fadvise.2.html):
  distinguish PTE discard, page-cache residency, and advisory eviction.
- [getrusage](https://man7.org/linux/man-pages/man2/getrusage.2.html): thread-local
  minor/major faults and context switches; capture separate from preparation.
- [pagemap](https://docs.kernel.org/admin-guide/mm/pagemap.html) and
  [cgroup memory counters](https://docs.kernel.org/admin-guide/cgroup-v2.html):
  verify mapping presence and memory-limit reclaim/refaults separately.

The only added library dependency is Linux **dev-only** libc, already in the
lockfile transitively, for correct platform ABI definitions in bench FFI.

## Qualification log

The first primary campaign (`target/mmap-primary`, executable
`82be9d34048207f1f4386fe15449dec4d1f67b913dbde50c1afaea13c5a089f3`)
completed all 30 pairs, but subsequent pressure-trace qualification was killed
at 128 MiB while materializing the export JSON tree. Its raw timings and the
failed trace/log remain preserved. The exporter was changed to stream borrowed
events, with the limit unchanged; the final primary and diagnostics are rerun
together against the resulting executable. This does not change product code
or select samples by their performance.

Profile qualification at cycles/100,000 lost records with a 128-page perf
buffer. A 1,024-page attach buffer was refused by the existing host limit.
Recapture short workers at cycles/1,000,000, retaining the 128-page bound and
increasing resident replay count to 128. The zero-loss and 100-workload-sample
requirements remain unchanged; rejected captures remain retained. The perf
control acknowledgement includes a NUL terminator on kernel 6.6, which the
collector now accepts before resuming its stopped child.
