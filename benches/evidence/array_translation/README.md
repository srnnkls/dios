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
- `mapping_kib`: the `Size` line of that mapping.

A gated run is valid only when `anonhugepages_kib` equals `mapping_kib` at both points;
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

## Results of 2026-11-18 (AT9)

Host `nix`, bench profile active, THP `never`, 30 measured fresh-process AB/BA pairs per
capture.

| Gate | Build | Candidate | Geomean | CI95 upper | Bound | Result | Evidence |
|------|-------|-----------|--------:|-----------:|------:|--------|----------|
| AT-G8 | default, shared runner | f193292 against 8597238 | 0.5454 | 0.5722 | 0.85 | passed | `at-g8/` |
| AT-G1 | layout 16 | fdada37 | 1.4881 | 1.5112 | 1.50 | failed | `at-g1/layout-16/` |
| AT-G1 | layout 32 | fdada37 | 1.5487 | 1.5541 | 1.50 | failed | `at-g1/layout-32/` |
| AT-G1 | layout 64 | fdada37 | 1.4491 | 1.4619 | 1.50 | passed | `at-g1/layout-64/` |
| AT-G2 | default | fdada37 | 1.0232 | 1.0273 | 1.10 | passed | `at-g2/` |

- AT-G8: `get` reads 23.0 ns per hit on the base, in its two stack-alignment modes
  (`at-g8/get-stack-alignment.md`), and 12.4 ns after the hit-path change; the difference
  is 10.6 ns (9.4 to 11.8). Two earlier captures were rejected by the validator for
  tooling defects fixed in 1dd0438 and fdada37.
- Reported, not gated: `get` against `mmap_random` on the fair lane at fdada37 is 3.31
  (12.43 against 3.76 ns per read; floor 3.68), from `at-g8-mmap/`.
- AT-G1 passed on three layout builds at 377a0ad (`at-g1-377a0ad/`) and fails on two of
  three at fdada37, after the `get` hit-path change. The session loop's machine code is
  the same in both trees. Per read the session arm executes 84.1 instructions and the
  chunked mmap arm 63.2 in every build; branch mispredicts per 1,000 reads are 75 on the
  mmap arm, 81 on the session arm in the three builds of 377a0ad, and 106, 102 and 83
  in the builds of fdada37 (single `perf stat` processes). The session read stands at
  5.99 to 6.11 ns against 3.95 to 4.22 ns for the chunked mmap arm; against the flat
  mmap loop the ratio is reported at 1.56 to 1.66.

## AT-G7 at the AT9 tree (2026-11-19)

Product code of 1dfecfd and c89d43a (identical in `src/`), 30 fresh-process pairs per lane,
evidence in `at-g7-1dfecfd/`.

| Lane | Geomean | CI95 upper | Bound | Result |
|------|--------:|-----------:|------:|--------|
| DRP-G2 warm | 0.9003 | 0.9021 | 1.01 | passed |
| DRP-G2 cycling | 0.9904 | 1.0017 | 1.01 | passed |
| DRP-G4 eight-thread | 0.2929 | 0.3104 | 1.00 | passed |
| DRP-G4 scaling | 0.1572 | 0.1648 | 0.50 | passed |
| RC-G1 cold, rerun | 0.8409 | 0.8433 | 1.00 | passed |
| RC-G1 pressure, rerun | 0.6012 | 0.6038 | 1.00 | passed |
| RC-G4 fragmented | 1.0013 | 1.0032 | 1.02 | passed, zero automatic admissions |
| RC-G4 dependent | 1.0000 | 1.0004 | 1.02 | passed, zero automatic admissions |

- The first RC-G1 capture could not start: AT4 had broken the frozen collector; fixed in
  c89d43a. Its next capture aborted on the steady-interval witness: one interval of one
  candidate row of the pressure case touched 34 read SQEs instead of at most 33; the cold
  case had completed at 0.832 (`rc-g1/capture-aborted-c89d43a.log`). Twelve diagnostic
  pressure captures, six at the current head and six at 5131d7d, completed with zero
  violations (`rc-g1/diagnosis.log`). The owner authorized one rerun (AT-D23); it passes.

## AT-G1 and AT-G2 after the liveness wave (2026-11-19)

Product code of 6ba0592 (AT-D18 to AT-D22: liveness once per view, grace period on slot
reuse, `Pool::session_file`), manifests frozen in 3033037, host `nix`, bench profile
active, THP `never`, 30 measured fresh-process AB/BA pairs per capture. The fdada37
captures are retained in `at-g1-fdada37/` and `at-g2-fdada37/`.

| Gate | Build | Geomean | CI95 upper | Bound | Result | Evidence |
|------|-------|--------:|-----------:|------:|--------|----------|
| AT-G1 | layout 16 | 1.3834 | 1.4000 | 1.50 | passed | `at-g1/layout-16/` |
| AT-G1 | layout 32 | 1.4795 | 1.4909 | 1.50 | passed | `at-g1/layout-32/` |
| AT-G1 | layout 64 | 1.5495 | 1.5537 | 1.50 | failed | `at-g1/layout-64/` |
| AT-G1 | pooled, 90 pairs (gated, AT-D24) | 1.4692 | 1.4830 | 1.50 | passed | `at-g1/paired.csv`, `at-g1/gate.txt` |
| AT-G2 | default | 1.0216 | 1.0250 | 1.10 | passed | `at-g2/` |

AT-G1 is decided on the pooled row (AT-D24, 2026-11-20); the three build rows are
reported. `validate.py pool-layouts` (a3f439c) wrote `at-g1/paired.csv` from the three
builds' pairs in layout order.

- Nanoseconds per read, chunked mmap arm against session arm: 4.40 / 6.09 (layout 16),
  3.94 / 5.82 (32), 3.92 / 6.08 (64); plain-array floor 4.49, 3.94, 3.94. Against the
  flat mmap loop the ratio is reported at 1.45, 1.58 and 1.66 (CI95 upper).
- The `get` loop's machine code in the runner of 6ba0592 matches the AT-G8 candidate
  executable instruction for instruction; only code addresses differ.
- At fdada37 layouts 16 and 32 failed and 64 passed; here 64 fails and 16 and 32 pass.
- Single `perf stat` processes per build (`at-g1/profile/counters.txt`): the session arm
  executes 78.5 instructions and 15.4 branches per read, against 84.1 and 16.4 at fdada37
  and 377a0ad; the chunked mmap arm 63.2 and 12.2. Cycles per read stay where they were:
  26.3 to 27.9 for the session arm (377a0ad 26.5 to 26.6, fdada37 27.9 to 28.3), 17.5
  for the mmap arm on layouts 32 and 64 and 22.2 on layout 16. Branch mispredicts per
  1,000 reads: session 90 to 98 (377a0ad 81, fdada37 98 to 101), mmap 75. On layout 64,
  55 % of the session loop's mispredicts fall on the chunk-loop bound, 8 % on the
  reference-bit test and 6 % on the entry test.

## AT-G3 under the bench-thp-madvise profile (2026-11-22)

Runner of 6ba0592 (`c1c162b4`), manifests frozen in e7f80f9, captures at e7f80f9 on
`nix`, 30 measured fresh-process AB/BA pairs each, lane `resident_access_session_fold_256m`
(65,536 resident pages, a 4 KiB fold per request). The gated row runs under THP `madvise`
(`bench-thp-madvise`, guard `DIOS_BENCH_HOST_PROFILE`), the equal-reach row under the
ordinary profile.

| Row | THP | Geomean | CI95 upper | Bound | Result | Evidence |
|-----|-----|--------:|-----------:|------:|--------|----------|
| AT-G3, dios arena on 2 MiB pages | `madvise` | 0.9104 | 0.9124 | 1.00 | passed | `at-g3/` |
| equal reach, arena on 4 KiB pages | `never` | 1.1305 | 1.1330 | | reported (AT-D12) | `at-g3-equal-reach/` |

- Nanoseconds per fold, mmap against session: 406.7 / 370.2 (gated), 399.5 / 451.7
  (equal reach). Plain-array floor 408.0 and 398.7; flat mmap loop 400.3 and 400.4.
- Witness: in every gated row the dios arena holds 262,144 KiB of `AnonHugePages` of
  262,144 KiB before and after the timed loop; the ext4 mapping, the flat loop and the
  floor array stay on 4 KiB pages (`AnonHugePages` 0, `FilePmdMapped` 0). Every
  equal-reach row holds no huge page.

