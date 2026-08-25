# Pinned-frame-retention amendments

## Scope

This plan gates T10–T13 against clean `f6f816b33c13c121e02767735f861d46c8939371` main. T13 carries no new performance claim; its existing T017 correctness and zero-allocation gates remain binding. T11 and T12 are control-plane changes unless implementation adds work to ordinary poll or completion loops.

## Transient guard gate

Metric: candidate/baseline wall-time ratio; lower is better. The gate is the one-sided 95% confidence-interval upper bound from 10,000 deterministic bootstrap resamples.

Workload: `pfr_transient_guard` on `nix`, CPU 0, performance governor, shipping registered backend. One real file, 256 4-KiB frames, 128 resident pages, fixed shuffled order. Each operation performs ordinary `Pool::get`, keeps the guard live while folding 64 descriptor-selected bytes, then drops it. Each arm runs 8,192 warm hits. Checksums must match; post-warmup allocations and per-thread minor and major faults must be zero.

Protocol: 40 order-alternated pairs. Baseline is clean `f6f816b33c13c121e02767735f861d46c8939371`; candidate is one clean committed final T10–T13 product. The runner, both products, lockfiles, toolchain, profile, and input are sealed before repetition zero.

Threshold: CI95 upper bound <= 1.0100.

Compare command:

```sh
mise run validate-pfr-pairs pfr_transient_guard target/bench-samples/pfr_transient_guard_process.csv target/bench-samples/pfr_transient_guard.csv target/bench-samples/pfr_transient_guard_provenance.json && mise run gate target/bench-samples/pfr_transient_guard.csv 1.01
```

Escalation lever: inspect the `ReaderSlot::commit_pin` guard-count data flow and generated transient-hit code. Remove redundant loads or branches without weakening the unconditional peak assertion. A valid failure blocks the PR; the threshold is not relaxed.

## Nested transient guard gate

Metric, host, sealing, pair count, bootstrap method, and threshold match the
ordinary transient gate: candidate/baseline CI95 upper <= 1.0100 over 40
order-alternated pairs.

Workload: keep one outer ordinary `FrameGuard` live while repeatedly acquiring,
folding 64 descriptor-selected bytes through, and dropping an inner guard over
the same 128-page resident working set. Each arm runs 8,192 timed inner guards;
the outer guard is established and dropped outside timing. Checksums,
post-warmup allocations, and per-thread minor and major faults must match the
ordinary gate controls.

Compare command:

```sh
mise run validate-pfr-pairs pfr_nested_transient_guard target/bench-samples/pfr_nested_transient_guard_process.csv target/bench-samples/pfr_nested_transient_guard.csv target/bench-samples/pfr_nested_transient_guard_provenance.json && mise run gate target/bench-samples/pfr_nested_transient_guard.csv 1.01
```

Escalation lever: inspect the nested `commit_pin` count/ceiling load and branch;
remove redundant work without weakening the release assertion.

## Allocation and lifecycle gates

T13 retains T017's DIO-G4 requirement: warm get/miss, write, fsync, bounded reports, and overflow drains allocate zero times after warmup on both backends.

```sh
cargo test --features bench,mock --test zero_alloc
```

T13's original `pool_progress`, `pool_retire`, `pool_write`, product-capacity, Loom, Linux, and strict lint suites must pass after conflict resolution.

T11 changes the file-retirement scratch used by the completion loop, so
`pfr_zero_budget_bypass` is mandatory: 40 sealed, order-alternated pairs with
candidate/baseline CI95 upper <= 1.0100 and the same checksum, allocation, and
fault controls.

```sh
mise run validate-pfr-pairs pfr_zero_budget_bypass target/bench-samples/pfr_zero_budget_bypass_process.csv target/bench-samples/pfr_zero_budget_bypass.csv target/bench-samples/pfr_zero_budget_bypass_provenance.json && mise run gate target/bench-samples/pfr_zero_budget_bypass.csv 1.01
```

Escalation lever: keep retirement traversal bounded by configured capacity but
move scratch initialization and capacity bookkeeping to build or registration.

T12 adds no additional timing gate while confined to mode readback and errors.
If T12 adds steady-state work to retention-enabled poll or drain loops,
`pfr_nonzero_poll` also becomes mandatory at CI95 upper <= 1.0100:

The same 40-pair, sealed-product, warm-state, checksum, allocation, and fault controls apply. The escalation lever is to move file-capacity or mode bookkeeping back to registration/retirement control paths. A valid failure blocks the PR.

## Results

The sealed v20 run on `nix` passed all active gates:

| Lane | Pairs | Ratio geomean | One-sided CI95 upper | Threshold | Verdict |
|---|---:|---:|---:|---:|---|
| `pfr_transient_guard` | 40 | 0.9136 | 0.9223 | 1.0100 | PASS |
| `pfr_nested_transient_guard` | 40 | 0.9158 | 0.9221 | 1.0100 | PASS |
| `pfr_zero_budget_bypass` | 40 | 0.9966 | 0.9973 | 1.0100 | PASS |

Each validated process artifact has 80 rows and each paired artifact has 40 rows. Checksums matched, and timed allocations, minor faults, and major faults were zero. The active host profile used the performance governor and THP `never`. `pfr_nonzero_poll` remained inactive because T12 added no poll or drain work. Evidence is retained at `.peer/pinned-frame-retention/20260825T173039Z-2ce5ae/final-benchmark-artifacts-v20` and on `nix` at `/home/srnnkls/build/dios/target/bench-samples/pfr-amendments-v20`.
