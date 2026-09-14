# Bench Plan: storage workload suite

Written before implementation. This adds characterization benchmarks using the
shipping `Pool` API; it changes no product or performance-relevant library path.
It models storage access patterns, not complete databases, search engines, or
transaction protocols. The suite does not claim superiority over mmap or another
engine, and its exploratory comparisons do not introduce new shipping gates.

## Measurements

Every comparison uses candidate/base elapsed time (lower is better), 30 paired
samples after three untimed warmup pairs, alternating AB/BA. Both arms consume
the same deterministic requests and bytes. The shared `PairedSamples`,
`ratio_gate`, and `write_samples` support supplies the confidence interval and
CSV format; `mise run gate` remains the only performance gate evaluator.

| Lane | Workload | Base | Candidate |
|---|---|---|---|
| `multirun_readers` | 4,096 warm full-granule reads, round-robin over eight immutable files and 64 resident pages | One reader worker | Four reader workers sharing one pool, equal total work |
| `point_batch` | 128 distinct fragmented cold pages | Demand reads at depth one | Window of 16 pending reads |
| `partition_pipeline` | 128 full-granule reads, exactly 96 warm and 32 distinct cold; four checksum/decode passes per page | Demand reads at depth one | Window of 16, consume ready pages while other reads are pending |
| `retained_session` | 4,096 64-byte projections over 16 warm pages | Ordinary guard per projection | Promote each page once, reuse retained handles, then release the session |
| `compaction_interference` | The partition trace plus, in the candidate, 32 aligned output pages and one data-sync barrier | Foreground reads alone | Same foreground reads with bounded compaction-style output in flight |
| `sequential_control` | 1,024 distinct cold full-granule reads, round-robin across eight sequential file streams and exceeding pool capacity | Demand reads | Window of 16 |
| `dependent_control` | 128 cold reads whose next address is decoded from the preceding frame | Depth-one chain | Same depth-one chain; batching cannot bypass the data dependency |

`compaction_interference` deliberately has unequal background work: its ratio
measures foreground slowdown, not a speedup or equal-work throughput. Report
foreground elapsed time and total time separately; total includes draining all
writes and the barrier. Verify every output byte after the timing window.
The other lanes have equal useful operations, useful bytes, and checksums.

Report per arm: elapsed nanoseconds, foreground nanoseconds, useful operations
and bytes, checksum, hits, pending admissions, Busy responses, polls, maximum
pending reads, and completed writes/barriers. The reader-scaling lane includes
dispatch and completion notification but excludes thread creation, registration,
and joining. The session lane includes promotion and final release. All timed
queues and scratch storage have fixed capacities; no work is silently dropped.
Worker-local allocation counters must remain zero in the timed workload.
`poll_backend_completions` and `poll_reclaimed` count only explicit poll
reports: a pressured `get` can also drain/reclaim internally. Their values are
lower bounds on total internal activity, not conservation checks. Exact
terminal read work is established by the consumed-page counts and checksums.

## Fixture and host protocol

- Granule: 4 KiB. Eight fully written and synced 8 MiB input files, plus a
  separate preallocated output file. Byte patterns encode page and word identity
  and the dependent chain's next address. Input creation is outside timing.
- Fresh 256-frame pools per arm/sample prevent one arm warming the other's
  resident set. Prime lazy synchronization with one spare page (global 16,383),
  outside every measured trace, then prefill exactly the lane's warm pages.
  The spare occupies one of the 256 frames until evicted. Cold means
  absent from the Dios pool, not absent from an SSD controller cache. The
  sequential lane exceeds the pool to exercise reclamation during the sample.
- Four reader slots, two ordinary guards per reader, 32 in-flight reads,
  96 frames of miss headroom, 16 retained frames, 16 staging slots, and 17
  product operations. These satisfy the existing scope watermark. One pool at
  a time keeps registered memory well below the host's 8 MiB memlock limit.
- Use `registration_policy_from_env`; record the requested posture. Require
  direct I/O for timing, with no silent buffered fallback. Smoke mode may use
  buffered I/O and is always advisory. Record backend, OS/architecture, CPU
  affinity, governor, kernel, build Rust version/profile, executable hash,
  compiled runner hash, source commit, worktree status, fixture recipe, and
  all workload constants beside the samples.
- Reject a run if its resolved registration or I/O mode changes between samples.
  Fresh-ring startup can transiently refuse registration even below the nominal
  per-pool memlock size. For repeatable characterization, select an explicit
  posture with `DIOS_REGISTRATION_POLICY=registered` or `unregistered`; never
  combine fallback and registered samples or silently relabel either one.
- Binding regression runs remain on `nix`: Threadripper 3970X, Linux 6.6.64,
  Samsung 970 PRO, performance governor, THP `never`. Run the existing host
  guard first. Pin this suite to CPUs 0-3; the single worker has that same
  allowed set, and four workers may use those four cores. Record the actual
  allowed set; this is shared-core scaling, not a per-core-affinity claim.
- Reserve an idle storage host and inspect active benchmark processes before
  and after collection. Another campaign on the same device invalidates an
  isolated-resource claim even if CPU affinity and the host guard pass. Retain
  overlapping/incomplete runs, then repeat during an idle window; do not change
  the polling bound to accommodate interference outside this workload contract.
- No global cache drop is needed for direct-I/O inputs. Sync fixture creation
  before opening direct descriptors. macOS eager-inline runs are advisory;
  they measure the same protocol without Linux kernel I/O overlap.

## Acceptance and escalation

The numerical shipping thresholds are unchanged: DRP-G2 ordinary warm and
cycling-reuse candidate/base upper CI <= 1.01; DRP-G4 ordinary eight-thread
candidate/base <= 1.00 and eight-thread/one-thread <= 0.50. Their pinned workload,
baseline commits, preparation, and compare commands remain owned by
`dios_r1_r7_read_performance.md` and the recorded DRP gate manifest.

Before code, add a failing workload-contract check. Acceptance additionally
requires exact operation/checksum agreement, the planned warm/cold counts,
observed pending widths, successful bounded drains, output verification, and
all seven lanes producing complete 30-row paired artifacts. A failed
correctness check blocks the suite. A failed existing regression gate triggers
profiling of that exact lane with the flamegraph skill; investigate accidental
product changes or host/protocol drift, without relaxing a threshold.

New workload ratios are characterization results. Record unfavorable results
as faithfully as favorable ones. A speedup claim or future optimization must
get its own pre-recorded numeric gate and escalation lever before product code
changes. Do not turn the negative control into an assumed speedup gate.

## Commands

```sh
mise exec -- cargo test --features bench --test workload_suite_contract
mise run bench-workloads-smoke
mise run bench-workloads
mise run remote -- mise run bench-workloads
```

`bench-workloads` runs the host guard and pins CPUs 0-3 on Linux. The benchmark
CLI also accepts `list`, `smoke [output-directory]`, and
`run [output-directory] [lane]`. Each run uses a new output directory so stale
artifacts cannot be mistaken for current evidence. Smoke performs one pair per
lane and emits no gate-compatible paired CSV. Full runs emit one paired CSV
per lane plus rich measurements and provenance. To assert a separately
approved bound against one such artifact:

```sh
mise run gate target/bench-samples/workloads/RUN/point_batch.csv BOUND
```

`trace [output-directory] [lane]` repeats the 30 pairs with bounded API events;
`observe [output-directory] [lane]` compares clocks/events disabled versus enabled
within each fixed workload/arm: 30 alternating off/on pairs after three warmups,
using total elapsed time and the shared ratio harness. It records untraced rows
in the output directory and traced rows in its `traced/` child. This isolates
observer cost more directly than comparing two separately collected runs.
These ratios are characterization, with no adopted observer-overhead threshold.
`profile output-directory lane arm repetitions` repeats one arm (1..=10,000
times) for the flamegraph task. Both use required direct I/O and retain the
same workload definitions. The diagnostic cost model and research are in
[`workload_cost_model.md`](workload_cost_model.md).

## Advisory macOS profile completion

The owner selected macOS profiling while the Linux host's other campaign runs.
Keep the existing primary/trace/observer measurements and identical executable.
Reuse the two complete retained-session profiles; preserve the sparse initial
point-read profile and collect a longer replay in a new directory. Complete both
arms of all seven lanes, one profiler at a time.

Use 4,096 repetitions for multi-reader lanes, 2,048 for sequential lanes and the
compaction candidate, and 10,000 for the remaining short lanes. Require at least
100 retained timed stack observations for descriptive attribution; if a lane
still falls short at the existing 10,000-repetition limit, report it as sparse.
Do not change its work or present macOS snapshots as Linux CPU measurements.
This collection changes no library path and adds no performance threshold.
For readable SVGs, also render a workload-only view: retain the stack suffix
starting at `engine::timed_workload`, merge equal suffixes across worker
instances, and assert that sample weight equals the analyzer's timed count.
Preserve the original folded stacks and raw sample output beside that view.

## Linux collection after the host becomes available

The owner made the Threadripper available on 2026-09-13. Collect fresh primary,
trace, and observer runs sequentially under explicit `unregistered` posture,
then profile both arms of every lane using the same executable. Start profiles
at 1,024 repetitions; retain sparse attempts and increase repetitions, up to the
existing 10,000 limit, until at least 100 timed CPU samples are available.
Preserve pre/post host process and device snapshots, raw perf recordings, and
collector logs. Record perf's event and sample-loss diagnostics. Run read-only
fio QD1/QD16 calibration only after Dios measurements, on a suite input file.
This completes characterization and attribution; the separate DRP clean-source,
fresh-process protocol still owns shipping regression verdicts.

The first Linux capture lost 11.09% of samples with the legacy 16-page perf
buffer, and its package-shell rebuild changed the executable SHA-256. Preserve
that rejected attempt. Subsequent captures use an immutable copy matching the
primary manifest, `cpu-clock` at 997 Hz, a 16 KiB DWARF stack, and 128 mmap
pages. A 1,024-page attempt exceeded the host's permitted mapping budget and
collected no samples; no security settings were changed. Require zero reported
lost events for attribution; retain and diagnose
any failure. These are collector-quality checks, not new shipping thresholds.

The 997 Hz CPU-clock replay under-sampled workers shorter than one millisecond.
Inferno also treated perf's printed period as a stack weight, not an observation
count. Correct conversion removes the period field before folding. A fixed
100,000-cycle trial resolved the short retained worker, but lost three chunks.
For the complete dense replay, record only CPUs 0-3 with 1,024 mmap pages,
place the recorder on CPU 4, and write perf data to a task-owned tmpfs directory;
copy recordings to persistent storage only after measurement. This avoids
profiler output competing with the measured NVMe input. Use 256 repetitions for
warm multi-reader and retained lanes, and 64 for the I/O lanes, with the same
minimum of 100 actual timed samples and zero reported losses. Keep the earlier
CPU-clock and failed sequential replay as diagnostics. Fixed-cycle shares are
cycle-weighted execution estimates; scaling them by primary worker CPU time
assumes approximately stable effective CPU frequency within that workload.
Report profiler/primary CPU ratios as a perturbation diagnostic, not an
interleaved causal overhead estimate.

The CPU-filtered capture requires privileges unavailable at this host's
`perf_event_paranoid=1`. The final attempt keeps per-process recording with
128 pages per CPU, the dedicated recorder core, and tmpfs output; it requests
no extra privileges. Preserve the refused collector logs separately.
