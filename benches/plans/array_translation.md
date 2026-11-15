# Bench Plan: array_translation

Plan for [scopes/active/array-translation](../../scopes/active/array-translation/scope.md).
Product baseline is `main` at `ade5310`. No campaign has run; the only numbers below
are advisory spike results from an M1 Pro.

| Field | Value |
|-------|-------|
| Metric & direction | Wall-time ratio candidate/base per gate below, lower is better |
| Workload | Resident lanes of `benches/mmap_workloads` and `benches/mmap_tlb_pressure` on `nix`, defined per gate below |
| Baseline | Resident mmap read for AT-G1 to AT-G3 and AT-G5; `main` at `ade5310` for AT-G4; the optimistic floor arm for AT-G6; for AT-G7 the baselines of the frozen plans it inherits |
| Reps | 30 measured fresh-process AB/BA pairs after 2 qualification pairs; AT-G3 keeps its 40 in-process reps of 256 iterations |
| Threshold | One-sided 95% CI upper bound of the ratio at or below the bound of each gate |
| Compare command | Per gate in the [capture contract](#capture-contract) |
| Escalation lever | Per gate below; no bound changes without an owner decision recorded here |

## Gates

| Gate | Candidate / base | Bound |
|------|------------------|-------|
| AT-G1 | Session read, 64-byte projection / mmap projection, 1,024-page set | <= 1.50 |
| AT-G2 | Session read, whole-granule fold / mmap fold, 1,024-page set | <= 1.10 |
| AT-G3 | Session read, whole-granule fold / mmap fold, 256 MiB set, hugepage-backed arena | <= 1.00 |
| AT-G4 | `get` hit after the swap / `get` hit on `main` at `ade5310`, `resident_access_ordinary` | <= 0.85 |
| AT-G5 | `read_copy` of 64 bytes / mmap copy of 64 bytes, 1,024-page set (was WRC-G1) | <= 1.50 |
| AT-G6 | `read_copy` of 64 bytes / optimistic floor (was WRC-G2) | <= 1.20 |
| AT-G7 | Frozen DRP-G2 warm and cycling; DRP-G4 eight-thread and scaling; RC-G1 and RC-G4 | 1.01 / 1.01; 1.00 / 0.50; as recorded in `readahead_coalescing.md` |

The owner selected these bounds on 2026-11-14. AT-G5 and AT-G6 keep the bounds of
WRC-D2. AT-G1 to AT-G6 also require zero issued I/O, zero warmed allocations and equal
checksums across arms. AT-G7 inherits workload, baseline, placement, concurrency and
sample contracts from its own plans unchanged.

## Workload contract

AT-G1, AT-G2, AT-G5 and AT-G6 use the initialized resident fixture of the
`resident_access_*` lanes: 1,024 pages of 4 KiB, 4 MiB of payload, prefilled outside
timing with mmap PTEs populated. The request sequence is the fixed 65,536-request
sequence repeated 256 times, 16,777,216 reads per timed process, frozen before
measurement.

- AT-G1 reads one 64-byte projection per request. The session arm opens one session
  and one `SessionFile` per 16 requests, the same chunking as `resident_access_epoch_batch`.
- AT-G2 folds the whole granule (512 `u64` words) per request over sequential pages,
  with the same 16-request sessions.
- AT-G5 and AT-G6 copy the 64 bytes into a caller buffer and consume that buffer on
  every arm. The floor arm loads a packed version word, copies and rechecks with no
  translation, over a private buffer with no concurrent writer.
- AT-G3 runs `benches/mmap_tlb_pressure` with a session arm: 65,536 resident pages,
  a whole-granule fold at a uniformly random index over the whole set, both arms
  replaying the same index sequence, sessions of 16 requests.
- AT-G4 runs the `dios_serial` arm of `resident_access_ordinary` in the candidate
  executable against the same arm in the base executable built from `main` at
  `ade5310`, captured as rich rows by the collector's `--base-build` option (AT-D5).
  `pair-bench` is not used.
- AT-G7 runs the frozen lanes under their own plans. The DRP lanes need the frozen
  runner revision and fresh base of the owner amendment of 2026-09-14 first.

DRP prerequisite: the frozen-runner protocol revision and the fresh base under it are owned by Sören (DRP owner amendment of 2026-09-14) and are prepared in AT3, before its first AT-G7 campaign; old-protocol base samples are never paired with array-translation candidates.

Host protocol: `nix` (Threadripper 3970X, Linux 6.6.64, Samsung 970 PRO), performance
governor, the recorded bench profile behind `mise run bench-host-guard`, consumer on
CPU 0 and controller on CPU 4 for single-thread lanes. Concurrency is one reader in
AT-G1 to AT-G6; arrival is closed-loop.

## Measurement boundary and attribution

Timing covers the request loop only. Pool construction, prefill, PTE population,
session setup outside the loop, teardown and file registration are excluded and
reported separately. Session open and drop inside the loop are included.

Every run records source, build and executable identity, fixture, seed, work count,
CPU map, checksum, issued-I/O and allocation witnesses, and integer elapsed
nanoseconds per process. A pre-run manifest pins the expected identities. Rich rows
are validated and converted to `base_ns,candidate_ns` pairs by the `warm-prepare`
mode of `benches/mmap_workloads/validate.py` before the shared harness computes any
statistic; a missing, duplicated or reordered row or any manifest drift rejects the
set.

AT-G3 is interpretable only with the hugepage state. Each run records
`/sys/kernel/mm/transparent_hugepage/enabled` and the `AnonHugePages` of the arena
mapping from `/proc/<pid>/smaps`. A run whose arena is not fully hugepage-backed is
invalid, not failed. The [evidence README](../evidence/array_translation/README.md)
pins the witness format.

## Capture contract

Every step runs from the repository root of the workstation checkout. The capture and
paired-sample steps run on `nix` through `mise run remote --`, which syncs the tree to
`build/dios` and keeps the remote `target/`. After them, the workstation copies the
gate's remote output into the evidence directory with
`rsync -a nix:build/dios/target/array-translation/at-gN/ benches/evidence/array_translation/at-gN/`;
for AT-G7 the source is the `target/bench-samples/` files its plans name, copied into
`benches/evidence/array_translation/at-g7/`. The compare runs on the workstation over
the copied `paired.csv`, and its output is saved as `gate.txt` beside it. A frozen
manifest under `benches/evidence/array_translation/manifests/` pins the expected
source digests, executable SHA-256, fixture SHA-256, lane, arms, seeds, CPU map and
host profile of its gate before the first measured pair; the `manifest.json` that the
capture writes beside its rows must match it field for field, or the set is rejected.
Lanes and modes named below that do not exist yet are frozen here and added by the
task in the last column.

| Gate | Frozen manifest | Capture | Paired samples | Compare | Added by |
|------|-----------------|---------|----------------|---------|----------|
| AT-G1 | manifest `benches/evidence/array_translation/manifests/at-g1.json` | `mise run remote -- mise run bench-mmap-workloads target/array-translation/at-g1 --lanes resident_access_session_projection` | `mise run remote -- uv run --python 3.12 benches/mmap_workloads/validate.py warm-prepare target/array-translation/at-g1 benches/evidence/array_translation/manifests/at-g1.json target/array-translation/at-g1/paired.csv` | `mise run gate benches/evidence/array_translation/at-g1/paired.csv 1.50` | AT4 (lane, session arms of `warm-prepare`); `warm-prepare` AT3 |
| AT-G2 | manifest `benches/evidence/array_translation/manifests/at-g2.json` | `mise run remote -- mise run bench-mmap-workloads target/array-translation/at-g2 --lanes resident_access_session_fold` | `mise run remote -- uv run --python 3.12 benches/mmap_workloads/validate.py warm-prepare target/array-translation/at-g2 benches/evidence/array_translation/manifests/at-g2.json target/array-translation/at-g2/paired.csv` | `mise run gate benches/evidence/array_translation/at-g2/paired.csv 1.10` | AT4 (lane, session arms of `warm-prepare`); `warm-prepare` AT3 |
| AT-G3 | manifest `benches/evidence/array_translation/manifests/at-g3.json` | `mise run remote -- cargo bench --features bench --bench mmap_tlb_pressure -- session target/array-translation/at-g3` | written on `nix` by the capture as `target/array-translation/at-g3/paired.csv` beside `target/array-translation/at-g3/hugepage.json` | `mise run gate benches/evidence/array_translation/at-g3/paired.csv 1.00` | AT4 |
| AT-G4 | manifest `benches/evidence/array_translation/manifests/at-g4.json` pinning the base and candidate executables | `mise run remote -- mise run bench-mmap-workloads target/array-translation/at-g4 --lanes resident_access_ordinary --base-build /home/srnnkls/build/dios-at-base-ade5310` | `mise run remote -- uv run --python 3.12 benches/mmap_workloads/validate.py warm-prepare target/array-translation/at-g4 benches/evidence/array_translation/manifests/at-g4.json target/array-translation/at-g4/paired.csv` | `mise run gate benches/evidence/array_translation/at-g4/paired.csv 0.85` | AT3 (`--base-build`, `warm-prepare`), pinned by AT2 |
| AT-G5 | manifest `benches/evidence/array_translation/manifests/at-g5.json` | `mise run remote -- mise run bench-mmap-workloads target/array-translation/at-g5 --lanes resident_access_copy` | `mise run remote -- uv run --python 3.12 benches/mmap_workloads/validate.py warm-prepare target/array-translation/at-g5 benches/evidence/array_translation/manifests/at-g5.json target/array-translation/at-g5/paired.csv` | `mise run gate benches/evidence/array_translation/at-g5/paired.csv 1.50` | AT6 (lane); `warm-prepare` AT3 |
| AT-G6 | manifest `benches/evidence/array_translation/manifests/at-g6.json` | `mise run remote -- mise run bench-mmap-workloads target/array-translation/at-g6 --lanes resident_access_copy_floor` | `mise run remote -- uv run --python 3.12 benches/mmap_workloads/validate.py warm-prepare target/array-translation/at-g6 benches/evidence/array_translation/manifests/at-g6.json target/array-translation/at-g6/paired.csv` | `mise run gate benches/evidence/array_translation/at-g6/paired.csv 1.20` | AT6 (lane); `warm-prepare` AT3 |
| AT-G7 | manifest `benches/evidence/array_translation/manifests/at-g7.json` naming the frozen DRP runner and RC6 sources | `mise run remote -- mise run prepare-drp-product-harness /home/srnnkls/build/dios /home/srnnkls/build/dios-at-drp-harness`, then the frozen DRP-G2, DRP-G4, RC-G1 and RC-G4 captures of their plans, each through `mise run remote --` | `mise run remote -- mise run prepare-gate-pairs` per lane as `dios_r1_r7_read_performance.md` lists it | `mise run gate benches/evidence/array_translation/at-g7/drp_g2_warm_ordinary_paired.csv 1.01` and the other bounds of AT-G7 as their plans list them, each over its copied paired CSV | AT3 |

The AT-G1 and AT-G2 lanes pair the `mmap_random` arm against the session arm; AT-G5
pairs the mmap copy against `read_copy`; AT-G6 pairs the optimistic floor arm against
`read_copy`. For AT-G4, `--base-build` names a clean worktree of `main` at `ade5310`
at `/home/srnnkls/build/dios-at-base-ade5310` on `nix`; the collector builds the base
executable there with the candidate's build command and runs the unchanged `sample`
mode of each executable over one fixture, one fresh process and one uniquely named
JSON row per arm, in AB/BA order, 2 qualification pairs then 30 measured pairs. The
AT-G4 manifest pins both executables and the fixture by SHA-256, and `warm-prepare`
validates the rows against it before writing `paired.csv`.

Characterization, reported and not gated:

- Resident bytes of the translation mapping after each lane, with the arena size beside it.
- Session length sweep at 1, 4, 16 and 64 requests on the AT-G1 lane.
- The AT-G1 lane with the liveness check hoisted per file, to price that check.
- `read_copy` fast and fallback counts and optimistic-only page eviction on the existing pressure-random workload.
- A matched CPU profile of the session arm and the mmap arm for AT-G1.

## Escalation levers

- AT-G1, AT-G2: profile the failing arm with the `flamegraph` skill and diff against
  the mmap arm. Then, by owner-approved scope amendment and in this order: keep the
  CLOCK reference in the entry's spare bit; validate liveness once per file and delay
  slot reuse by a reclamation grace period.
- AT-G3: with a hugepage-backed arena and a failing ratio, profile both arms before
  any TLB reasoning. A confirmed failure is recorded as non-adoption of the
  beats-mmap claim and returns to the owner.
- AT-G4: profile `get`; the swap does not land until the gate passes.
- AT-G5, AT-G6: a safety failure blocks code. A performance failure keeps the result
  and profiles the exact arms. No fence removal, `FrameState` change, weakened witness,
  raised bound or substitution of batching or retention.
- AT-G7: a regression blocks adoption.

## Notes

Advisory spike, M1 Pro, 1,024 pages, 60 interleaved in-process pairs against mmap
(`benches/hot_read_paths.rs` on branch `spike/hot-read-paths`):

| Arm | Point 64 B | Whole granule |
|-----|-----------:|--------------:|
| `get` today | 11.0 | 1.72 |
| session over the hash probe | 6.8 to 7.2 | 1.33 |
| predictive placement over the hash probe | 6.6 to 6.7 | 1.18 to 1.21 |
| session over the array | 1.54 to 1.61 | 1.06 to 1.08 |
| session over the array with per-read liveness | 1.75 to 1.83 | 1.06 to 1.07 |
| session over the array, liveness and identity per read | 1.86 to 1.93 | 1.07 to 1.08 |
| session over the array, no CLOCK touch | 1.34 to 1.43 | 1.07 |
| session per read over the array | 2.48 to 2.56 | 1.08 to 1.09 |
| MMU translation with a state word | 1.51 to 1.53 | 1.01 to 1.03 |
| retained handle | 1.16 to 1.20 | 1.03 to 1.04 |

Scaling the M1 overhead by the 1.3 to 1.8 observed on arms measured on both hosts puts
the session arm with per-read liveness at 1.40 to 1.56 on Zen 2, so AT-G1 has little
margin. The spike's copy arms moved between 6.5 ns and 14.3 ns for unchanged source
across builds and support no projection for AT-G5.

`mmap_tlb_pressure` on `nix` (2026-08-18) measured today's `get` at 1.18 with the
hugepage-advised arena and 1.48 without. A filesystem with a large-folio page cache
would shrink that difference; ext4 on 6.6.64 has none.
