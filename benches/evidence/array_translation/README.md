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
| `at-gN/report.json` | ns per read per arm, their difference and the floor comparison (F11) |
| `at-gN/layout-NN/` | One capture per layout build of AT-G1, AT-G5 and AT-G6 (F12) |

## Hugepage witness (AT-G3)

Each AT-G3 row records, before the first and after the last timed iteration (rule F7 of the plan's fairness contract):

- `transparent_hugepage`: the bracketed mode read from `/sys/kernel/mm/transparent_hugepage/enabled`.
- `anonhugepages_kib`: the `AnonHugePages` line of the arena mapping in the process's own `/proc/self/smaps`.
- `arena_kib`: the `Size` line of that mapping.

A gated run is valid only when `anonhugepages_kib` equals `arena_kib` at both points;
an equal-reach run is valid only when `anonhugepages_kib` is 0 at both points. An
invalid run is excluded and recorded, never counted as a failure or a pass. Each
AT-G3 manifest pins its expected THP mode.

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

## Results of 2026-11-17 (AT5)

Host `nix`, bench profile active, THP `never`, 30 measured fresh-process AB/BA pairs per
capture. Ratios are session read over the `mmap_random` arm in the same 16-request
chunks (AT-D16); the flat column is the reported, ungated ratio over `mmap_random_flat`.

| Gate | Build | Candidate | Geomean | CI95 upper | Bound | Result | Flat geomean / upper | Evidence |
|------|-------|-----------|--------:|-----------:|------:|--------|----------------------|----------|
| AT-G1 | layout 16 | 3e373a9, flat base | 1.9131 | 1.9359 | 1.50 | failed | | `at-g1-failed-3e373a9/` |
| AT-G1 | layout 32 | 3e373a9, flat base | 1.9666 | 1.9850 | 1.50 | failed | | `at-g1-failed-3e373a9/` |
| AT-G1 | layout 64 | 3e373a9, flat base | 1.9107 | 1.9234 | 1.50 | failed | | `at-g1-failed-3e373a9/` |
| AT-G1 | layout 16 | 377a0ad | 1.4698 | 1.4880 | 1.50 | passed | 1.5612 / 1.5750 | `at-g1/layout-16/` |
| AT-G1 | layout 32 | 377a0ad | 1.4694 | 1.4867 | 1.50 | passed | 1.5682 / 1.5889 | `at-g1/layout-32/` |
| AT-G1 | layout 64 | 377a0ad | 1.4607 | 1.4676 | 1.50 | passed | 1.5670 / 1.5870 | `at-g1/layout-64/` |
| AT-G2 | default | 3e373a9, flat base | 1.0479 | 1.0507 | 1.10 | passed | | `at-g2-3e373a9/` |
| AT-G2 | default | 377a0ad | 1.0236 | 1.0301 | 1.10 | passed | 1.0245 / 1.0319 | `at-g2/` |

ns per read at 377a0ad, from `report.json`:

| Lane | Build | mmap, chunked | floor, chunked | mmap, flat | session | session minus chunked mmap (95% interval) |
|------|-------|--------------:|---------------:|-----------:|--------:|-------------------------------------------|
| 64-byte read | layout 16 | 3.950 | 3.941 | 3.707 | 5.81 | 1.859 (1.800 to 1.953) |
| 64-byte read | layout 32 | 3.941 | 3.931 | 3.712 | 5.80 | 1.853 (1.796 to 1.944) |
| 64-byte read | layout 64 | 3.952 | 3.964 | 3.699 | 5.77 | 1.821 (1.793 to 1.853) |
| 4 KiB fold | default | 53.12 | 53.34 | 53.12 | 54.38 | 1.262 (0.841 to 1.674) |

- AT-G1 failed at 3e373a9 against the flat mmap loop, at about 7.1 ns per session read.
  Between that and the passing captures: the session read's hit path was slimmed
  (74099d6, 7.1 to about 5.8 ns), and the owner decided that the mmap and floor arms of
  a session lane iterate the same 16-request chunks as the session arm (AT-D16,
  377a0ad). Against the flat loop the session read still stands at about 1.57.
  `at-g1-failed-3e373a9/profile.md` holds the profile and the scratch experiment.
- The floor array was 16 bytes past a page boundary before 377a0ad; floor figures of
  earlier captures carry a two-cache-line read per 64-byte projection.
- Every row of the passing captures shows zero pending reads, zero allocations and
  zero faults in the timed region.
- AT-G3 is not captured: it waits on the `bench-thp-madvise` host profile.
