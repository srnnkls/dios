# AGENTS.md

dios is a completion-based async direct-IO driver plus a userspace frame
pool, in Rust. TigerBeetle-shaped: `submit(op, slot)` + `poll()`, a
preallocated completion slab, cfg-selected backends — io_uring on Linux,
eager-inline elsewhere (submit enqueues, poll executes the syscall on the
calling thread). No futures, no executor, no `dyn` dispatch, no
allocation on the hot path. Design goals in order: safety, performance,
developer experience.

## Decisions Live in the Scope

Settled architecture (backends, ring topology, eviction and
reclamation, dependency policy, formats), invariants, and gate numbers
are owned by the active scope's `scope.md`/`design.md`. Read them
before changing anything they cover; do not restate or relitigate them
here, in code, or in comments. When this file and the scope disagree,
the scope wins. This file carries only durable process and style.

## Safety

- Assert function arguments, return values, and invariants — two per
  function on average; assert positive and negative space; pair
  assertions across code paths (before write, after read).
- Assertions detect programmer errors and crash; operating errors
  (IO failure, corruption, exhaustion) are values — `Result`, handled at
  every call site. Never `.unwrap()` an expected failure.
- Put a limit on everything: every queue, loop, and retry has a fixed
  bound set at init. All capacity is allocated at startup; nothing grows
  after warmup (amortized-zero is not zero).
- Simple, explicit control flow; no recursion. Split compound
  conditions into nested branches; state invariants positively; give
  an `if` its `else` when the negative space needs handling.
- Push `if`s up and `for`s down: parents own control flow and state,
  leaf helpers stay pure. Hard limit of 70 lines per function.
- Explicitly sized types (`u32`, `u64`) over `usize` except where an API
  forces it. Treat index, count, and size as distinct concepts; show
  intent with division (`div_ceil`, exact, floor).
- Isolate `unsafe` in small audited modules (ring registration, guard
  deref, frame state). Every unsafe block states the invariant that
  makes it sound. `unsafe fn` bodies use inner `unsafe {}` blocks.
- Zero warnings at the strictest settings. Fix causes, don't suppress;
  if truly unavoidable, `#[expect(lint, reason = "…")]`, never
  `#[allow]`.

## Bench-Driven Development

No implementation work starts without a bench plan: what is measured,
on what workload and host protocol, and the numeric gate deciding
pass/fail — written before code. A change touching no perf-relevant
path states so; its gate: the pinned regression benches stay green.

- Gates are falsifiable statistics on pinned workloads, e.g. the
  one-sided 95% CI upper bound of an interleaved A/B ratio over ≥ 30
  reps ≤ a stated bound. "Looks faster" is not a gate.
- A failed gate blocks the change and triggers its pre-recorded
  escalation lever; relaxing a gate is an explicit owner decision,
  recorded with the reason — never silent.
- Plans live in `benches/plans/`; each states metric and direction,
  workload, baseline, reps, threshold with ratio orientation, the
  compare command, and the escalation lever. Gates are asserted by the
  shared compare harness — never hand-rolled statistics.
- Design choices are settled by benching the alternatives, or by a
  back-of-envelope sketch over network/disk/memory/CPU that a bench
  later confirms. Optimize the slowest resource first, by frequency.
- Batch: amortize syscalls and completions; never react to external
  events one at a time. Separate control plane from data plane; extract
  hot loops into standalone functions with primitive arguments.
- Bench code is first-class: pinned host, governor, and cache-drop
  protocol; reviewed and kept green.

## Bench Host

The Linux perf gates run on the Threadripper 3970X box, ssh host
`nix` (NixOS, kernel 6.6.64, NVMe Samsung 970 PRO). `mise run remote
-- <command>` syncs the tree there and runs the command through mise
(e.g. `mise run remote -- mise run test`); the remote `target/`
persists across runs. For ad-hoc ssh, the box's login shell is
nushell — route commands through `bash -ls` (the login env carries
`NIX_LD`, required to exec mise-installed toolchains; a stdin script
bypasses nushell parsing). Box tooling (mise, gcc, fio) is declared in
its `/etc/nixos/configuration.nix`; the project toolchain comes from
`mise.toml`, same as everywhere else.

## Rust

- Edition 2024, MSRV pinned (`cfg_select!` needs 1.95+). Clippy
  pedantic enabled via `[lints.clippy]` in Cargo.toml; `cargo fmt` and
  `cargo clippy` run clean after every session.
- Parse, don't validate: constructors enforce invariants; core logic
  sees only valid types. Newtype indexes and ids (`FrameIdx`, `OpSlot`,
  `PageId`); no bare integers across an API, no `bool` parameters.
- Make illegal states unrepresentable: state machines as enums, not
  flag clusters.
- Stdlib first: `LazyLock`/`OnceLock`, `cfg_select!`, native
  `async`-free APIs. Generics over trait objects.

## Naming

- `snake_case`; no abbreviations. Units and qualifiers last, descending
  significance: `latency_us_max`, not `max_latency_us`.
- A helper called by one function takes the caller's name as prefix
  (`read_sector` / `read_sector_callback`); callbacks go last.
- Comments say why, never what; prefer a rename or a pinning test; a
  blatantly true assertion beats a comment. Full sentences.

## Working Method

- Think before coding: state assumptions; present competing
  interpretations instead of picking silently; push back when a simpler
  approach exists. Name confusion — don't code through it.
- Simplicity is the hardest revision, not the first draft. Minimum code
  that solves the problem: no speculative features, no single-use
  abstractions, no unrequested configurability.
- Surgical changes: touch only what the task requires; match existing
  style; remove only orphans your own change created. Every changed
  line traces to the request.
- Zero technical debt: a known showstopper (unbounded queue, potential
  deadlock, hot-path alloc) blocks merge — never a TODO.
- Goal-driven: every task becomes a verifiable check before code is
  written — a failing test, a loom interleaving, a bench gate. Write
  the failing check first, make it pass, refactor while green.

## Verification

- `cargo test` with fault injection at the syscall boundary (the
  scope's test table owns the case list), loom for lock-free
  interleavings, miri on pure-memory paths and asan on syscall paths,
  compile-fail tests for lifetime escapes, the zero-alloc harness on
  both backends.
- Fuzzers and loom are a safety net, not a substitute for a mental
  model: encode understanding as assertions first.
