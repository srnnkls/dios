# dios

Completion-based async direct-IO driver plus a userspace frame pool:
`submit(op, slot)` + `poll()`, a preallocated completion slab, and
cfg-selected backends — io_uring on Linux, eager-inline elsewhere. No
futures, no executor, no allocation on the hot path.

The pool and both platform backends are implemented. Linux uses `io_uring`
with configurable buffer registration; other platforms execute queued operations when the
caller polls. Architecture, invariants, and perf gates are owned by the active
scope under `scopes/`; process and style live in `AGENTS.md`.

## API

`Pool` is the product surface. It owns opened files, coalesces concurrent
misses, and lends resident bytes through a `FrameGuard`:

```rust,no_run
use std::path::Path;

use dios::{DirectIo, Get, PageId, Pool, ReadyResult};

let pool = Pool::builder()
    .frame_count(16)
    .max_concurrent_readers(1)
    .peak_guards_per_reader(1)
    .max_inflight_reads(1)
    .miss_headroom(3)
    .build()?;
let file = pool.open(Path::new("segment.data"), DirectIo::Preferred)?;
let reader = pool.register_reader()?;
let page = PageId::new(file, 0);

if let Get::Pending(mut token) = pool.get(&reader, page)? {
    let mut polls = 0u32;
    while polls < 1_000_000 {
        pool.poll();
        match pool.ready(&reader, token) {
            ReadyResult::Ready(frame) => {
                std::hint::black_box(&*frame);
                break;
            }
            ReadyResult::NotYet(pending) => token = pending,
            ReadyResult::Err(error) => return Err(error.into()),
        }
        polls += 1;
    }
    if polls == 1_000_000 {
        return Err("pool read exceeded the polling bound".into());
    }
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

The explicit completion driver and write-staging vocabulary live under
`dios::driver`. See `examples/quickstart.rs` for the bounded polling form.

### Optional resident hints

An ordinary warm `Pool::get` hit remains the default no-hint path: it performs
no hint-specific branch, load, store, or RMW. Callers that can reuse an exact
resident-page observation may opt into `Pool::lease_file`, `ResidentFileLease`,
`Pool::resident_hint`, `ResidentHint`, and `Pool::get_with_hint`. Hints are
advisory: a missing, mismatched, or stale hint falls back inside `get_with_hint`
to the ordinary `Pool::get` behavior.

A file lease protects the lifetime of one exact file generation. It does not
retain or pin frames, so pages covered by a live lease remain normally
evictable. This API makes no resident-set, frame-retention, or other R8 claim.
After pool construction, the zero-allocation proof covers ordinary warm hits,
hinted hits, stale-hint fallback, lease acquire/drop, and retirement progress
on eager-inline and Linux io_uring.

The binding R7 measurements selected this public API and retained the current
four-round page hash. Exact identities, confidence bounds, proof counts, and
artifacts are recorded in the
[R7 gate results](scopes/done/dios-r1-r7-read-performance/resources/gate-results.yaml).

### Retained frames

A nonzero `PoolBuilder::max_retained_frames` enables
`FrameGuard::into_retained`. If promotion is refused, `RetainRefused` returns
that same live guard for ordinary guarded access or copy-out.

A performance consumer promotes each distinct frame once while setting up one
bounded read session, not once per point. It then indexes bytes through the
retained handles until the session drops them. The
[R8 retained-set evidence and disposition](scopes/done/pinned-frame-retention/resources/r8-resident-set.md)
records the rejected prototype and the selected session shape.

### Readahead

`Pool::prefetch(&[PageId])` requests future residency without acquiring guards
or creating pending tokens. Its report partitions the window into resident,
pending, admitted, deferred and rejected pages. `poll` drives the accepted
reads; ordinary `get`/`ready` consumes their results. Retry deferred hints while
they remain useful. When extending a window, include its still-useful pages so
replacement can preserve them.

Automatic forward-sequential detection is enabled by default. Set
`.readahead(dios::Readahead::Disabled)` on the builder to use explicit hints
alone. `.prefetch_headroom(16)` sets the speculative credit ceiling;
`.prefetch_headroom(0)` disables all speculation. The default ceiling is at most
32, limited by spare frames and the configured in-flight read limit minus one.
A pool configured for only one in-flight read therefore has no default
speculative capacity.

Credits cover reads in flight and unconsumed resident pages. Per-reader
training starts with consecutive cold demands and extends through confirmed
speculative consumption. Discontinuities reset it; unrelated readers do not
advance each other's predictors. This initial policy learns forward sequential
access, with all metadata allocated at pool construction.

Resident hints accelerate an existing lookup; prefetch requests I/O; retained
handles preserve resident bytes. On the eager-inline backend, polling still
executes queued I/O on the caller's thread.

## Development

```sh
mise install        # toolchain + hooks (hk installs pre-commit)
mise run check      # clippy pedantic -D warnings, fmt, tests, rustdoc
```

## Benchmarks

Bench-driven development: every perf-relevant change needs a plan in
`benches/plans/` (copy `TEMPLATE.md`) before code.

```sh
mise run bench                                          # all benches
mise run gate target/bench-samples/<name>.csv <bound>   # assert a plan threshold
```

Two harnesses:

- criterion micro-benches (`benches/smoke.rs`) for exploratory
  measurement and baselines.
- paired A/B gate benches (`benches/paired_smoke.rs`) via
  `dios::bench::run_paired`: base and candidate interleaved in-process,
  samples written to `target/bench-samples/<name>.csv`, asserted by the
  shared compare harness (`benches/compare.rs`) as a one-sided 95% CI
  upper bound on the ratio.

The storage workload suite exercises seven access patterns on real `Pool`
instances: shared multirun readers, cold point batches, mixed-residency
partition pipelines, retained read sessions, foreground reads during
compaction-style writes, sequential pressure, and dependent-read controls.
These are synthetic storage workloads; their comparisons characterize Dios's
behavior rather than a complete database or a comparison with mmap.

```sh
mise run bench-workloads-smoke
mise run bench-workloads                     # 30 alternating pairs per lane
mise run bench-workloads-trace               # matched diagnostic API traces
mise run bench-workloads-observer            # interleaved trace-off/on overhead
mise run profile-workload -- point_batch candidate target/profiles/point_batch/candidate
```

Every run creates a separate artifact directory with checked work counts,
checksums, zero-allocation evidence, CSV measurements, and provenance. Full
runs require direct I/O; smoke runs are buffered and advisory. The suite's
[bench plan](benches/plans/workload_suite.md) specifies workload boundaries and
the [cost model](benches/plans/workload_cost_model.md) documents how CPU samples,
traces, and resource limits can be combined.
The [collection report](benches/evidence/workload_suite/README.md) includes
Linux measurements, 14 workload flamegraphs, observer costs, fresh regression
gates, and the separate advisory macOS results.

Use an explicit `DIOS_REGISTRATION_POLICY=registered` or `unregistered` consistently
across modes, on an idle benchmark host. Runs reject changes in the resolved
posture. After collecting primary, trace, and observer runs and profiles stored as
`PROFILES/<lane>/<arm>/{profile.folded,measurement/}`, generate the cost report:

```sh
uv run benches/workload_suite/analyze.py PRIMARY OUTPUT --trace TRACE --profiles PROFILES --observer OBSERVER
```

The analyzer requires matching executables, retains unmatched CPU samples,
and reports interleaved observer ratios from the shared compare harness. Trace spans
measure API observation latency; pending-interest depth is not device queue
depth. `mise run bench-workload-fio -- INPUT OUTPUT` provides optional read-only
QD1/QD16 calibration on an already-created suite input file on Linux.

macOS numbers are advisory; gates run on the pinned Linux host per the
scope's protocol.

The Linux mmap suite adds actual file-mapping baselines: resident accesses,
minor faults, cold point/gather/dependent reads, advice controls, shared readers,
and scans/random reads under a private memory limit. Five resident API controls
compare ordinary guards, reused hints, epoch batches, retained sessions and
multiple fields per guard. [Results and fault/profile evidence](benches/evidence/mmap_workloads/README.md)
include favorable and unfavorable results, setup costs and observer overhead.

```sh
mise run bench-mmap-workloads NEW_OUTPUT
mise run bench-mmap-workloads NEW_OUTPUT --mode smoke
mise run profile-mmap-workload PRIMARY NEW_PROFILE LANE ARM --repetitions 128
```

The [mmap plan](benches/plans/mmap_workloads.md) and
[resident API plan](benches/plans/mmap_access_amortization.md) define comparable
work and validated cache state. Use a new output directory per campaign and
the same retained executable for its timing, trace and profile replays.

The [prefetch plan](benches/plans/prefetch_admission.md) adds explicit-window,
automatic/disabled, memory-pressure and wrong-hint controls.
The [prefetch results](benches/evidence/mmap_workloads/prefetch.md) retain its
frozen comparisons, CPU costs and regression gates. To compare against
the retained pre-prefetch scan executable, preserving both runner identities:

```sh
uv run benches/mmap_workloads/frozen.py NEW_OUTPUT --baseline OLD_PRIMARY --candidate NEW_PRIMARY
```

The [scan geometry study](benches/evidence/scan_geometry/README.md) separates
matched lookahead from larger cache/read granules and confirms selected
configurations against mmap. Its [plan](benches/plans/scan_geometry.md) also
defines trace-overhead controls, CPU attribution and storage-path fio calibration.

```sh
mise run bench-scan-geometry NEW_OUTPUT --input EXISTING_FIXTURE --experiment lookahead
mise run bench-scan-geometry NEW_OUTPUT --input EXISTING_FIXTURE --experiment geometry
```

The [pipelined READ/READV probe](benches/evidence/readv_pipelined/README.md)
compares four submission mechanisms at eight and sixteen outstanding 128 KiB
groups. Its [plan](benches/plans/readv_pipelined.md) separates throughput parity
under overlap from the earlier serial probe's cost.

## Profiling

```sh
mise run flamegraph --bench smoke -- --bench --profile-time 10
mise run flamegraph-diff before.folded target/flamegraph/profile.folded
```

Outputs in `target/flamegraph/`: `profile.folded` (grep-able folded
stacks), `top_self.txt` (top-40 self-time frames), `flamegraph.svg`.
Sampling uses `perf` on Linux and `/usr/bin/sample` on macOS — no sudo,
no Xcode.
