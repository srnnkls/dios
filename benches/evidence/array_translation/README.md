# Evidence: array_translation

Evidence for the [array_translation bench plan](../../plans/array_translation.md).
The [capture contract](../../plans/array_translation.md#capture-contract)
pins one frozen manifest and one capture command per gate. AT-G4 and AT-G7 ran for AT3 on
2026-11-17; AT8 retains every pair of the binding campaign here.

## Layout

| Path | Content |
|------|---------|
| `manifests/at-g1.json` .. `manifests/at-g7.json` | Frozen pre-run manifests, committed before the first measured pair of their gate |
| `at-gN/manifest.json` | Manifest the capture wrote, compared field for field with the frozen one |
| `at-gN/rows/` | Rich process rows as captured |
| `at-gN/paired.csv` | `base_ns,candidate_ns` pairs derived from the validated rows |
| `at-gN/gate.txt` | Output of the `mise run gate` command of the gate |
| `at-g3/hugepage.json` | Hugepage witness of every AT-G3 run |

## Hugepage witness (AT-G3)

Each AT-G3 run records, before the first and after the last timed iteration:

- `transparent_hugepage`: the bracketed mode read from `/sys/kernel/mm/transparent_hugepage/enabled`.
- `anonhugepages_kib`: the `AnonHugePages` line of the arena mapping in the process's own `/proc/self/smaps`.
- `arena_kib`: the `Size` line of that mapping.

A run is valid only when `anonhugepages_kib` equals `arena_kib` at both points. An
invalid run is excluded and recorded, never counted as a failure or a pass. The AT-G3
manifest pins the expected THP mode.

## Prerequisite for AT-G7

The plan's [workload contract](../../plans/array_translation.md#workload-contract) owns
the DRP prerequisite of AT-G7.

## Results of 2026-11-17 (AT3)

Host `nix`, bench profile active, 30 measured fresh-process AB/BA pairs per lane, shared
`mise run gate` harness. Ratios are candidate/base.

| Gate | Lane | Candidate | Geomean | CI95 upper | Bound | Result | Evidence |
|------|------|-----------|--------:|-----------:|------:|--------|----------|
| AT-G4 | `resident_access_ordinary` | e1bbd2f | 0.9076 | 0.9189 | 0.85 | failed | `at-g4-failed-e1bbd2f/` |
| AT-G4 | `resident_access_ordinary` | b8f95c1 | 0.7462 | 0.7539 | 0.85 | passed | `at-g4/` |
| AT-G7 | `drp_g2_warm_ordinary` | 5131d7d | 0.9159 | 0.9181 | 1.01 | passed | `at-g7/drp/` |
| AT-G7 | `drp_g2_cycling_reuse` | 5131d7d | 0.9974 | 0.9978 | 1.01 | passed | `at-g7/drp/` |
| AT-G7 | `drp_g4_ordinary_base_8t` | 5131d7d | 0.3060 | 0.3236 | 1.00 | passed | `at-g7/drp/` |
| AT-G7 | `drp_g4_ordinary_scaling` | 5131d7d | 0.1652 | 0.1741 | 0.50 | passed | `at-g7/drp/` |
| AT-G7 | RC-G1 `cold-mmap` | 5131d7d | 0.8296 | 0.8318 | 1.00 | passed | `at-g7/rc-g1/` |
| AT-G7 | RC-G1 `pressure-mmap` | 5131d7d | 0.5978 | 0.6015 | 1.00 | passed | `at-g7/rc-g1/` |
| AT-G7 | RC-G4 `automatic_dependent` | 5131d7d | 0.9998 | 1.0004 | 1.02 | passed | `at-g7/rc-g4/` |
| AT-G7 | RC-G4 `automatic_fragmented` | 5131d7d | 1.0251 | 1.0800 | 1.02 | failed | `at-g7/rc-g4/` |
| AT-G7 | RC-G4 `automatic_fragmented`, rerun | 5131d7d | 0.9935 | 1.0023 | 1.02 | passed | `at-g7/rc-g4-rerun/` |

- AT-G4 failed at e1bbd2f; `at-g4-failed-e1bbd2f/profile.md` holds the profile of the
  plan's lever, and b8f95c1 inlines the hit-path helpers it names. All 64 rows of the
  passing set show zero pending reads, zero allocations and matching checksums.
- The DRP lanes ran under the DRP owner amendment of 2026-09-14 (2,097,152 iterations
  and CPUs `0-7` for DRP-G4), with the bases a94860f and 1004a2e rebuilt under the
  revised runner in the same campaign (owner decision AT-D7).
- The first `automatic_fragmented` set failed on pair 16 at 2.18: its automatic-arm
  process polled 1.18M times instead of 0.51M for the same 1,024 reads; the other 29
  pairs lie within 0.971 to 1.003. The owner authorized one rerun at the same commit
  and executables (AT-D8); its pairs range from 0.794 to 1.031. Both sets record zero
  automatic admissions.
- `manifests/at-g7.json` was assembled from the as-run identities after the captures;
  the per-lane `pre-run.json` of each DRP lane was written before its first pair.
- RC-G1 raw rows are in `rows.tar.zst` per case; executables and the fixture stay on
  `nix` under `~/build/dios/target/array-translation/` and `~/build/dios-gates/`.
- Linux suites on `nix` at 5131d7d's tree before the runner fixes: `mise run test` 419
  passed, `mise run asan` 274 passed.
