---
name: benches
description: Bench-driven development for dios — write a bench plan before perf work, add criterion micro-benches or paired A/B gate benches, run the shared compare harness, read results. Use when implementing anything perf-relevant, adding benchmarks, running or defining gates, or comparing baselines.
---

# Benches

## Non-negotiables

- No implementation work starts without a plan in `benches/plans/<name>.md`
  (copy `TEMPLATE.md`; every field filled, including the escalation lever).
  A change touching no perf-relevant path states so; its gate is that the
  pinned regression benches stay green.
- Gates are asserted by the shared compare harness (`mise run gate`) —
  never hand-roll statistics in a bench.
- A failed gate blocks the change and triggers the plan's escalation
  lever. Relaxing a threshold is an owner decision, recorded in the plan.

## Choosing a harness

| Need | Harness |
|------|---------|
| Explore cost of a code path, keep baselines | criterion (`benches/smoke.rs` pattern) |
| Falsifiable A-vs-B gate with a ratio threshold | paired gate bench (`benches/paired_smoke.rs` pattern) |
| End-to-end CLI/process timing | hyperfine (ad hoc) |

## Writing a paired gate bench

```rust
use dios::bench::{ratio_gate, run_paired, write_samples};

let samples = run_paired("my_gate", 40, 256, || base_impl(), || candidate_impl());
let gate = ratio_gate(&samples, 10_000);
write_samples(Path::new("target/bench-samples"), &samples).expect("write samples");
```

- `run_paired` interleaves the closures and alternates their order each
  rep; it asserts reps ≥ 30 and rejects zero-length reps (size
  `iters_per_rep` so one rep is ≥ ~1 µs).
- `ratio_gate` returns the geometric-mean ratio candidate/base and the
  one-sided 95% CI upper bound (percentile bootstrap, deterministic seed).
- Register the bench in `Cargo.toml` with `harness = false` and
  `required-features = ["bench"]`.

## Commands

```sh
mise run bench                                          # everything
cargo bench --features bench --bench <name>             # one target
mise run gate target/bench-samples/<name>.csv <bound>   # assert plan threshold (exit 1 on FAIL)
cargo bench --features bench --bench smoke -- --save-baseline <tag>
mise x cargo:critcmp@0.1.8 -- critcmp <tag_a> <tag_b>   # compare criterion baselines
```

## Protocol

- Interleave A and B in one process (`run_paired` does this); never a
  full A run followed by a full B run.
- macOS numbers are advisory. Scope gates (DIO-G1..G3) run only on the
  pinned Linux host under the scope's protocol (governor, CCX pinning,
  cache drop) — see `scopes/` for the active gate definitions.
- After a failed gate: profile the exact failing bench with the
  `flamegraph` skill and diff folded stacks before/after.

## Escalation options

- iai-callgrind (Linux): deterministic instruction counts for noise-free
  CI gating of pure-CPU hot paths.
- tango-bench: finer paired sensitivity if `run_paired` resolution is
  insufficient.
- Bencher.dev: historical trend gating if runner noise becomes real.
