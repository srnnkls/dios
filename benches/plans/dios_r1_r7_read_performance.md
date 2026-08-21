# Bench Plan: Dios R1-R7 read-path performance

This plan is frozen before any R1-R7 product edit. Every performance ratio is
`candidate / base`; lower is better. Binding runs use real `Pool<Driver>` and
consume identical full 4 KiB granules in both arms. `MockDriver` is diagnostic
only and cannot close a gate. A one-word consumption lane is diagnostic only;
the full-granule fold is binding.

## Run identity and shared protocol

- The clean base source identity is
  `1004a2e6fcae0bcc9552dc3211c2416e388a250d`. Build it from a clean detached
  worktree into a base-only target directory.
- Candidate identity: record `git rev-parse HEAD` and `sha256sum` before every
  binding run, require an empty `git status --porcelain=v1`, and retain the
  exact candidate executable and its paired base executable. A run with an
  uncommitted tree, an old binary label, or a missing SHA-256 is invalid.
- Record the exact `Cargo.lock` SHA-256, Rust and mise toolchain versions,
  benchmark arguments, source commits, executable SHA-256 values, and runner
  script/generator SHA-256 values beside every raw result.
- Run on ssh host `nix`: Threadripper 3970X, NixOS kernel 6.6.64, Samsung 970
  PRO NVMe. Set the performance governor, record it, pin the requested CPUs
  with explicit affinity (CPU 0 for every single-thread lane; the DRP-G4 set
  below for contention), and record the kernel, CPU topology, NVMe identity,
  direct-I/O probe, transparent-hugepage state, and cache preparation.
- Each timed lane uses at least 30 fresh-process pairs. Pair 0 runs base then
  candidate, pair 1 candidate then base, and orders continue alternating. Both
  processes in a pair replay the same deterministic input. Warm resident lanes
  prefill and fault in their fixed set before timing; the cycling lane records
  its explicit completion and reclamation warmup instead of dropping caches.
- Write one row per process to retained raw CSV, including gate, lane, pair,
  order, arm, source commit, executable SHA-256, CPU set, iterations, elapsed
  nanoseconds, ns/op, checksum, and allocation count. Retain stdout/stderr and
  a provenance manifest with the CSV. Each lane's rich artifact is named
  `target/bench-samples/<lane>_process.csv`, with one row per process. Before
  comparison, run the lane's frozen conversion command:

  ```text
  mise run prepare-gate-pairs target/bench-samples/drp_g1_warm_get_process.csv target/bench-samples/drp_g1_warm_get_paired.csv target/bench-samples/drp_g1_warm_get_provenance.json
  mise run prepare-gate-pairs target/bench-samples/drp_g2_warm_ordinary_process.csv target/bench-samples/drp_g2_warm_ordinary_paired.csv target/bench-samples/drp_g2_warm_ordinary_provenance.json
  mise run prepare-gate-pairs target/bench-samples/drp_g2_cycling_reuse_process.csv target/bench-samples/drp_g2_cycling_reuse_paired.csv target/bench-samples/drp_g2_cycling_reuse_provenance.json
  mise run prepare-gate-pairs target/bench-samples/drp_g3_hint_materiality_process.csv target/bench-samples/drp_g3_hint_materiality_paired.csv target/bench-samples/drp_g3_hint_materiality_provenance.json
  mise run prepare-gate-pairs target/bench-samples/drp_g4_ordinary_base_8t_process.csv target/bench-samples/drp_g4_ordinary_base_8t_paired.csv target/bench-samples/drp_g4_ordinary_base_8t_provenance.json
  mise run prepare-gate-pairs target/bench-samples/drp_g4_ordinary_scaling_process.csv target/bench-samples/drp_g4_ordinary_scaling_paired.csv target/bench-samples/drp_g4_ordinary_scaling_provenance.json
  mise run prepare-gate-pairs target/bench-samples/drp_g4_hint_scaling_process.csv target/bench-samples/drp_g4_hint_scaling_paired.csv target/bench-samples/drp_g4_hint_scaling_provenance.json
  ```

  This conversion must validate the complete alternating base/candidate pair,
  matching lane, pair, workload, iteration count, checksum, source commit,
  executable SHA-256, and manifest SHA-256 fields. It then deterministically
  emits exactly `base_ns,candidate_ns`, in ascending pair order, to the distinct
  corresponding `target/bench-samples/<lane>_paired.csv` artifact. The
  lane-specific provenance manifest
  binds the SHA-256 of the rich process CSV, paired CSV, both executables, and
  conversion tool; any missing, duplicate, unmatched, reordered, or
  provenance-mismatched row makes conversion fail. Only the shared paired-log
  harness consumes the validated paired artifact and asserts one-sided 95%
  confidence bounds; no in-benchmark or hand-rolled statistic can close a
  gate.

## DRP-G1

| Field | Frozen value |
|---|---|
| Metric and direction | Hash quality compares candidate probes with the current four-round hash row by row. The binding speed metric is warm-get ns/op ratio, full-width one-round candidate / current four-round base; lower is better. |
| Workload | Quality uses both 50%-load matrices below. Speed uses anonymous warm `get` on real `Pool<Driver>` over the full-granule workload; both arms fold every byte of the same resident granules. |
| Repetitions | Quality matrices are deterministic exhaustive rows. Speed uses at least 30 order-alternated fresh-process pairs under the shared protocol. |
| Threshold | At 1,024 slots, candidate mean probes must be at most `1.05` times current and p99 at most current `+1` probe. At 131,072 and 524,288 calibration slots, and 2,048, 65,536, and 262,144 holdout slots, mean and p99 must each be at most `1.05` times current. Every row's maximum chain is `<64`. After all quality rows pass, the one-sided 95% CI upper bound of speed candidate/base must be `<=0.98`. |
| Artifacts and compare command | Rich rows: `target/bench-samples/drp_g1_warm_get_process.csv`; validated pairs: `target/bench-samples/drp_g1_warm_get_paired.csv`; compare: `mise run gate target/bench-samples/drp_g1_warm_get_paired.csv 0.98`. |
| Escalation lever | Retain the current four-round hash byte-for-byte and record the failed calibration, holdout, or speed sub-gate. Do not rerun the rejected packed candidate and do not introduce a third hash. |

### Historical calibration

The historical deterministic matrix is calibration because the small-table
integer p99 `+1` rule was selected after these results were visible. At 50%
load it crosses file populations 1, 16, and 256; sequential and
64-page-interleaved granules; and table sizes 1,024 (1 Ki), 131,072 (128 Ki),
and 524,288 (512 Ki) slots. Population rows that exceed the key count are
omitted. Base and candidate use full-key equality and identical insertion
order.

The packed expression `slot << 32 | generation ^ granule` is already rejected:
at 1,024 slots with 16 interleaved files it moved mean probes from `1.441` to
`1.709` and p99 from `5` to `11`. The sole eligible candidate is the settled
full-width one-round hash:

```text
file_page = (u64(generation) << 32) | u64(granule)
seed = driver ^ file_page ^ u64(slot).wrapping_mul(PHI)
hash = mix(seed)
```

Calibration must reproduce and retain every current/candidate mean, p99, and
maximum row plus the generator SHA-256 before the independent holdout runs.

### Independent holdout

Freeze the following 50%-load holdout and generator hash before any candidate
hash edit. None of its driver identities, populations, table sizes, generation
formulae, or permutations occurs in the calibration input.

- Driver identities are `0x71c3_5a09_d4e2_b687` and
  `0xd903_4f61_28bc_7a55`.
- Table sizes are 2,048, 65,536, and 262,144 slots. File populations are 3,
  31, and 127 where population does not exceed key count.
- For key ordinal `i` in `0..slots/2`, set file ordinal `f = i % files`, file
  slot `3 + 5*f`, generation `0x8000_0001 + 17*f`, and granule
  `11 + 257*(i/files)`.
- Use one round-robin insertion order (`i` ascending) and two shuffled orders.
  Each shuffle starts with `[0, 1, ..., count-1]`, with state equal to seed
  `0x243f_6a88_85a3_08d3` or `0x1319_8a2e_0370_7344`, then runs Fisher-Yates
  from `upper=count-1` down to `1`. Before each swap apply
  `state ^= state << 13; state ^= state >> 7; state ^= state << 17` with u64
  wrapping, then swap `upper` with `state % (upper+1)`.
- Base and candidate use full-key equality and byte-identical insertion order.
  Successful-probe p99 is nearest-rank `ceil(0.99 * count) - 1` in the sorted
  zero-based sample array.

Every driver/size/population/pattern/seed combination must satisfy the same
mean, p99, and maximum bounds above. Write results only after the plan and
generator SHA-256 are frozen. Quality alone cannot adopt the hash: only a clean
quality pass followed by the real `Pool<Driver>` speed pass does so.

The executable prerequisite
`mise run validate-drp-g1-probes target/bench-samples/drp_g1_probe_quality.csv`
validates every calibration and holdout row, its dimensions, insertion-order
identity, mean/p99/max calculations, thresholds, and generator SHA-256. It
must exit successfully before timing is authorized or
`drp_g1_warm_get_paired.csv` may be prepared or compared.

## DRP-G2

| Field | Frozen value |
|---|---|
| Metric and direction | ns/op ratio after production lease/hint support / immediately-before support, lower is better, plus post-warmup allocation count. No lease or hint API is called in either arm. |
| Workload | Two real `Pool<Driver>` lanes: (1) warm ordinary `get` over a resident deterministic set with a full-granule fold; (2) a bounded deterministic cycling working set larger than the pool, including read completion, CLOCK eviction, two successful epoch advances, reclamation, and frame reuse before continuing. |
| Repetitions | At least 30 order-alternated fresh-process pairs per lane under the shared protocol. The before and after commits and both executable SHA-256 values are retained separately. |
| Threshold | The one-sided 95% CI upper bound for each after/before ratio is `<=1.01`; the allocation counter is `zero` after warmup in both lanes. |
| Artifacts and compare commands | Warm ordinary rich rows: `target/bench-samples/drp_g2_warm_ordinary_process.csv`; validated pairs: `target/bench-samples/drp_g2_warm_ordinary_paired.csv`; compare: `mise run gate target/bench-samples/drp_g2_warm_ordinary_paired.csv 1.01`. Cycling/reuse rich rows: `target/bench-samples/drp_g2_cycling_reuse_process.csv`; validated pairs: `target/bench-samples/drp_g2_cycling_reuse_paired.csv`; compare: `mise run gate target/bench-samples/drp_g2_cycling_reuse_paired.csv 1.01`. |
| Escalation lever | Remove or redesign hint-only state until both unused-capability lanes and zero-allocation checks pass. The failure cannot be waived as outside the warm path. |

The cycling bound, frame count, working-set size, iteration count, input seed,
and expected completion/eviction/reuse counters are fixed in the runner before
the first measured candidate. Ordinary access must execute no hint-specific
branch, load, store, or RMW.

Both executable prerequisites must validate every post-warmup allocation count
in their rich artifacts:
`mise run validate-drp-g2-zero-alloc target/bench-samples/drp_g2_warm_ordinary_process.csv`
and
`mise run validate-drp-g2-zero-alloc target/bench-samples/drp_g2_cycling_reuse_process.csv`.
Each must exit successfully before its paired artifact may be prepared or its
timing result may authorize DRP-G2.

## DRP-G3

| Field | Frozen value |
|---|---|
| Metric and direction | ns/op ratio `get_with_hint` hit / ordinary `get` hit, lower is better. |
| Workload | One real file in real `Pool<Driver>`, with a fully resident deterministic page set. Both arms request the same pages and consume identical full-frame bytes while the normal `FrameGuard` is live. |
| Repetitions | At least 30 order-alternated fresh-process pairs under the shared protocol, using the same candidate source and binary identity contract. |
| Threshold | The one-sided 95% CI upper bound of hinted/ordinary is `<=0.95`. |
| Artifacts and compare command | Rich rows: `target/bench-samples/drp_g3_hint_materiality_process.csv`; validated pairs: `target/bench-samples/drp_g3_hint_materiality_paired.csv`; compare: `mise run gate target/bench-samples/drp_g3_hint_materiality_paired.csv 0.95`. |
| Escalation lever | Do not ship the public hint/lease surface. Retain liveness and alignment plus the DRP-G1 hash decision, and return the Sira consumer to ordinary guarded get/composition. |

The historical MockDriver result `0.8813 / 0.8980` is prior calibration only,
not a pass. This gate uses the shipping backend and exact full-frame bytes.

## DRP-G4

| Field | Frozen value |
|---|---|
| Metric and direction | Three lower-is-better ratios: ordinary candidate 8-thread ns/op / base-protocol 8-thread ns/op; within-candidate ordinary normalized 8-thread ns/op / 1-thread ns/op; and, only if DRP-G3 accepts hints, within-candidate hinted normalized 8-thread ns/op / 1-thread ns/op. |
| Workload | One shared real `Pool<Driver>`, one `ReaderCtx` per thread, 512 resident 4 KiB pages, and 64 disjoint pages per reader. Pin the eight-thread lane to CCX-0 CPUs `0-3,32-35`; pin the one-thread reference to CPU 0. The binding lane folds every full granule. |
| Repetitions | At least 30 order-alternated fresh-process pairs for each applicable comparison under the shared protocol. Base and candidate replay identical page orders. |
| Threshold | Ordinary candidate/base 8-thread one-sided 95% CI upper bound `<=1.00`. Candidate ordinary normalized 8-thread/1-thread upper bound `<=0.50`, equivalent to at least 2x aggregate scaling. If DRP-G3 accepts hints, candidate hinted normalized upper bound is also `<=0.50`; otherwise record it as not applicable. |
| Artifacts and compare commands | Ordinary base/candidate rich rows: `target/bench-samples/drp_g4_ordinary_base_8t_process.csv`; validated pairs: `target/bench-samples/drp_g4_ordinary_base_8t_paired.csv`; compare: `mise run gate target/bench-samples/drp_g4_ordinary_base_8t_paired.csv 1.00`. Ordinary scaling rich rows: `target/bench-samples/drp_g4_ordinary_scaling_process.csv`; validated pairs: `target/bench-samples/drp_g4_ordinary_scaling_paired.csv`; compare: `mise run gate target/bench-samples/drp_g4_ordinary_scaling_paired.csv 0.50`. When applicable, hinted scaling rich rows: `target/bench-samples/drp_g4_hint_scaling_process.csv`; validated pairs: `target/bench-samples/drp_g4_hint_scaling_paired.csv`; compare: `mise run gate target/bench-samples/drp_g4_hint_scaling_paired.csv 0.50`. |
| Escalation lever | Profile the full-granule-fold lane. The only in-scope levers are liveness-mirror contention, hint-stamp traffic, and `ReaderSlot` placement; weakening or removing the epoch fence is forbidden. |

The one-word lane may accompany the raw CSV for diagnosis, but it cannot
authorize any contention or scaling conclusion.
