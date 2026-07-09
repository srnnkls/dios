---
name: flamegraph
description: Generate and read agent-consumable CPU profiles for dios — folded stacks, top-N self-time table, SVG, and before/after diffs (perf on Linux, sample on macOS; no sudo, no Xcode). Use when profiling a bench/example/binary, investigating a failed perf gate, or comparing profiles across a change.
---

# Flamegraph

The primary consumer is the agent: read `top_self.txt` and grep
`profile.folded`. The SVG is for humans.

## Generate

```sh
mise run flamegraph --bench smoke -- --bench --profile-time 10
mise run flamegraph --example <name>
mise run flamegraph --bin <name> -- <run args>
```

- Args before `--` select the cargo target; args after `--` go to the
  binary. Criterion benches need `--bench` and profit from
  `--profile-time <secs>` (pure looping, no stats overhead).
- Builds with `--profile profiling` (release codegen + debug symbols).

Outputs in `target/flamegraph/` (overwritten each run):

| File | Content |
|------|---------|
| `top_self.txt` | top-40 self-time frames: `samples percent frame` — read this first |
| `profile.folded` | one line per unique stack: `frame;frame;...;leaf <samples>` |
| `flamegraph.svg` | human view |

## Read folded stacks

```sh
# self time: frame appears as the leaf (last frame before the count)
grep 'dios::pool' target/flamegraph/profile.folded

# inclusive time of a function = sum over all stacks containing it
awk '/dios::pool::get/ { s += $NF } END { print s }' target/flamegraph/profile.folded

# total samples (denominator for shares)
awk '{ s += $NF } END { print s }' target/flamegraph/profile.folded
```

## Diff a change

```sh
mise run flamegraph --bench smoke -- --bench --profile-time 10
cp target/flamegraph/profile.folded target/flamegraph/before.folded
# apply the change, rebuild, profile again, then:
mise run flamegraph-diff target/flamegraph/before.folded target/flamegraph/profile.folded
```

Prints top regressions and improvements per leaf frame; writes
`diff.folded`, `diff.deltas.txt`, and `diff.svg` (red = grew,
blue = shrank).

## Caveats

- Profile the pinned workload from the bench plan, not an arbitrary run —
  a gate failure and its flamegraph must reference the same code path.
- macOS sampling uses `/usr/bin/sample` (1 ms period): fine at ≥ seconds
  of runtime, too coarse below that — raise `--profile-time`. Absolute
  numbers on macOS are advisory; gate profiling happens on the Linux box.
- Linux: stacks unwind via DWARF. If frames look broken under lld/mold,
  add `-C link-arg=-Wl,--no-rosegment` to `RUSTFLAGS` for the profiling
  build.
- `samply record <binary>` remains available for interactive human
  digging (Firefox Profiler UI); it cannot export text, so it plays no
  role in the agent pipeline.
