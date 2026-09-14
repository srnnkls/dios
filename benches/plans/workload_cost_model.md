# Workload measurement and cost model

Research date: 2026-09-13. Companion to `workload_suite.md`, written before
the runner and instrumentation. This is an explanatory model with measurable
inputs, not a replacement for the scope's statistical acceptance gates.

## Research and its consequences

1. Fio distinguishes submission, completion, and total latency and asks users
   to check achieved I/O depth. A configured window alone proves no overlap.
   Record pending admissions, completions, maximum outstanding interests, and
   traced interest lifetimes. Dios interest depth is explicitly not kernel or
   device queue depth. [Fio documentation](https://fio.readthedocs.io/en/latest/fio_doc.html)
2. RocksDB separates inexpensive per-thread counters from more intrusive timing
   levels. Keep counters in all measurements, but compile out event timestamps
   in primary runs. Trace replays use the identical workload, and report their
   elapsed-time overhead against untraced runs.
   [Perf Context and IO Stats Context](https://github.com/facebook/rocksdb/wiki/Perf-Context-and-IO-Stats-Context)
3. CPU flamegraphs describe sampled execution. Blocked time needs complementary
   evidence, and summed off-CPU durations across threads are not request latency.
   Pair folded stacks with per-worker CPU time and API spans. Never label all
   non-CPU elapsed time as disk service. A busy polling thread can spend most of
   its CPU waiting for I/O progress.
   [Off-CPU analysis](https://www.brendangregg.com/offcpuanalysis.html)
4. Open and closed workload generators answer different questions. This suite
   is closed-loop: finite requests, bounded outstanding work, refill after
   completion. Its latency observations do not establish service p99 under an
   independent arrival process. A later service SLO study needs scheduled
   arrivals, queue delay, offered/admitted/completed rates, and dropped work.
   [Open Versus Closed: A Cautionary Tale, NSDI 2006](https://www.usenix.org/legacy/event/nsdi06/tech/full_papers/schroeder/schroeder_html/)
5. YCSB makes request distribution and operation mix explicit. Name our exact
   warm/cold mixture, footprint, sequentiality, and worker count; do not call
   deterministic hotspot traces Zipfian or claim to implement YCSB itself.
   [YCSB workload template](https://github.com/brianfrankcooper/YCSB/blob/master/workloads/workload_template)
6. Storage performance varies with the stack and experimental setup. Preserve
   source/binary identities, host metadata, cache preparation, alternating
   pairs, and negative controls. Retain raw observations instead of reporting
   only favorable averages.
   [On the Performance Variation in Modern Storage Stacks, FAST 2017](https://www.usenix.org/conference/fast17/technical-sessions/presentation/cao)
7. Resource ceilings help explain why optimizing one stage stops helping when
   another dominates. Use a time-budget analogue of Roofline, with measured
   throughput ceilings rather than nominal NVMe or DRAM specifications.
   [Berkeley Lab's Roofline explanation](https://cs-newsarchive.lbl.gov/news/2017/roofline-model-boosts-manycore-code-optimization-efforts/)
8. Perf records a selected event at a configurable frequency and buffers it in
   mmap pages. Preserve those settings and sample-loss diagnostics; missing
   observations can bias attribution. CPU-clock sampling misses sufficiently
   short workers; the dense Linux replay uses a fixed-cycle period. Remove
   the printed period before collapsing so weights count actual observations.
   [perf-record manual](https://man7.org/linux/man-pages/man1/perf-record.1.html)

## Quantities and boundaries

| Symbol | Observation |
|---|---|
| `N`, `H`, `M` | Completed useful reads, immediate hits, pending read admissions |
| `A`, `P`, `E` | Ready checks, poll calls, reclaimed frames reported by polls |
| `W`, `F` | Completed page writes and data-sync barriers |
| `B` | Useful bytes consumed, excluding repeat decode passes |
| `Bd` | CPU bytes processed, including repeat decode passes |
| `T`, `Tfg` | Total sample wall time and foreground completion wall time |
| `C` | Sum of measured worker CPU nanoseconds; unavailable is null, never zero |
| `qmax`, `qavg` | Maximum and time-average unresolved reader interests |
| `L_i` | Admission-to-observed-ready lifetime of pending interest `i` |

`M * 4096` estimates submitted read payload only for these unique cold-page
traces. General singleflight workloads need distinct backend read counts:
multiple interests can correspond to one I/O. CQE counts include writes and
barriers; do not equate all CQEs with reads. No metric here measures physical
NAND traffic, device service time, DRAM traffic, or mutex waiting in isolation.

## CPU demand

An initial additive accounting model is:

```text
C ≈ H*c_hit + M*c_miss + A*c_ready + P*c_poll
    + E*c_reclaim + W*c_write + F*c_sync + Bd*c_decode + c_worker_harness
```

The coefficients have units of CPU nanoseconds per event (or per processed
byte). Flamegraph shares locate likely terms; counters supply denominators.
For a mutually exclusive sampled category `k`, estimate its CPU budget as
`C_k ≈ C * samples_k / samples_total`, using the unprofiled primary worker
clock for `C`, then report `C_k / event_count_k`. Report the profile/primary
CPU ratio separately; different replay times confound causal overhead with
drift. Fixed-cycle samples weight cycles, so CPU-time attribution assumes
approximately stable effective frequency within the workload.
Never add inclusive stack percentages or multiply CPU percentages by elapsed
wall time. The categories are approximate because inlining and unobserved
kernel work affect attribution. Show unmatched samples and sample counts.
The CPU window covers worker execution; parent dispatch and result handling
contribute to elapsed time but are not included in `C`. Timer/counter boundary
bookkeeping is a small unseparated worker cost, so attribution remains an
estimate even when every retained stack is categorized.

These quotients are workload-local attribution, not separately identified
universal regression coefficients. For example, a poll both drains completions
and reclaims frames; this suite cannot separate those costs from counters alone.
Report joint categories when the profile cannot distinguish them. A predictive
fit needs independent perturbations of hit fraction, window, bytes processed,
and reclamation pressure, then held-out workloads with prediction residuals.

## Waiting, overlap, and throughput

For a complete bounded replay whose interests start and end inside the window:

```text
qavg = sum(L_i) / Tfg
X_pending = M / Tfg
L_mean = sum(L_i) / M
qavg = X_pending * L_mean
```

This finite-window accounting identity is a useful consistency check. It does
not prove device parallelism: an interest can remain unresolved after its I/O
has completed, and readiness-check order affects observation latency.

A first-order lower-envelope model for equal work is:

```text
T_floor ≈ max(C / worker_count,
              read_bytes / calibrated_read_bandwidth,
              write_bytes / calibrated_write_bandwidth,
              dependent_chain_length * calibrated_single_read_latency)
```

Mixed read/write bandwidth must be calibrated jointly; separate read and write
peaks cannot predict interference. Shared control-plane serialization can add
another ceiling, but it needs lock/CPU evidence before assigning its cost.
Startup, drain tails, queueing, and scheduling explain residual above the
envelope. Decode traffic in `Bd` is logical input traffic, not measured DRAM
traffic, so it cannot establish a DRAM Roofline without hardware counters.

For repeated sessions the expected break-even relationship is:

```text
reuse_count * (ordinary_access_cost - retained_access_cost)
    > promotion_cost + release_cost
```

Measure the setup/release spans and both CPU profiles; do not attribute an
end-to-end saving wholly to guard removal. For background writes, compare
`Tfg_with_writes / Tfg_without_writes`, and report the drain tail `T - Tfg`.

## Collection and interpretation

- Primary `run` samples have counters but no per-operation clocks or trace
  writes. `trace` replays write bounded, preallocated API spans after timing.
  Events identify request, page, operation, start/end nanoseconds, and worker.
  No trace buffer grows; overflow fails instead of silently discarding events.
- A `profile` replay holds work steady for enough bounded repetitions to sample
  it. Use the existing flamegraph task. Keep folded stacks, top self-time, and
  SVG per lane/arm instead of overwriting another lane's evidence. Restrict
  attribution to stacks under the named timed worker function so input creation,
  pool startup, output formatting, and fixture checksums do not dominate.
- Trace the same lane/arm and binary as its profile; retain both identities.
  Use `observe` for 30 interleaved trace-off/on pairs per fixed lane/arm;
  comparing separate full timing and tracing runs confounds drift with overhead.
  A large
  overhead is evidence to reduce tracing for diagnosis, never a reason to use
  traced throughput as the primary result.
- On Linux, optional `perf stat` cycles/instructions/cache-misses/context-switches
  and scheduler/block tracepoints can test hypotheses. Missing permissions are
  recorded; the suite does not require sudo or alter host security settings.
- macOS `sample(1)` snapshots all threads and can include blocked stacks. Those
  advisory stack-residency shares are not Linux on-CPU shares. This runner leaves
  macOS worker CPU time unavailable and the analyzer withholds absolute CPU
  attribution there instead of multiplying incompatible observations.
  The installed Inferno 0.12.7 sample converter's `IGNORE_SYMBOLS` list filters
  selected wait/read leaves. Report sample counts over the folded input, retain
  the original `sample.txt`, and do not interpret this partial filtering as a
  complete separation of on-CPU and off-CPU time.
- A read-only fio QD1/QD16 4 KiB calibration on the same filesystem/device can
  bound raw storage behavior. Keep its JSON and distinguish its engine/queue
  semantics from Dios's application-level window. No raw-device writes.
- Report what is measured, what is estimated, what remains unidentifiable,
  and which next experiment would distinguish the competing explanations.

The analyzer accepts `--observer RUN` for those dedicated observer pairs and
their shared-harness ratios. `--trace RUN` supplies diagnostic API lifetimes;
it is never substituted for the primary throughput run.
