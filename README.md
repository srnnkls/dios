# dios

Completion-based async direct-IO driver plus a userspace frame pool:
`submit(op, slot)` + `poll()`, a preallocated completion slab, and
cfg-selected backends — io_uring on Linux, eager-inline elsewhere. No
futures, no executor, no allocation on the hot path.

The pool and both platform backends are implemented. Linux uses registered
`io_uring` buffers; other platforms execute queued file operations when the
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

if let Get::Pending(mut token) = pool.get(&reader, page) {
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

macOS numbers are advisory; gates run on the pinned Linux host per the
scope's protocol.

## Profiling

```sh
mise run flamegraph --bench smoke -- --bench --profile-time 10
mise run flamegraph-diff before.folded target/flamegraph/profile.folded
```

Outputs in `target/flamegraph/`: `profile.folded` (grep-able folded
stacks), `top_self.txt` (top-40 self-time frames), `flamegraph.svg`.
Sampling uses `perf` on Linux and `/usr/bin/sample` on macOS — no sudo,
no Xcode.
