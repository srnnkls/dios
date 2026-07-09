# dios

Completion-based async direct-IO driver plus a userspace frame pool:
`submit(op, slot)` + `poll()`, a preallocated completion slab, and
cfg-selected backends — io_uring on Linux, eager-inline elsewhere. No
futures, no executor, no allocation on the hot path.

Status: pre-implementation. Architecture, invariants, and perf gates are
owned by the active scope under `scopes/`; process and style live in
`AGENTS.md`.

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
