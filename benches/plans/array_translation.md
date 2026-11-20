# Bench Plan: array_translation

Plan for [scopes/active/array-translation](../../scopes/active/array-translation/scope.md).
Product baseline is `main` at `ade5310`. AT-G4 and AT-G7 ran on `nix` on 2026-11-17 and
their results are in the [evidence README](../evidence/array_translation/README.md);
the numbers below are advisory spike results from an M1 Pro.

| Field | Value |
|-------|-------|
| Metric & direction | Wall-time ratio candidate/base per gate below, lower is better |
| Workload | Resident lanes of `benches/mmap_workloads` on `nix`, defined per gate below |
| Baseline | Resident mmap read for AT-G1 to AT-G3 and AT-G5; `main` at `ade5310` for AT-G4; the optimistic floor arm for AT-G6; for AT-G7 the baselines of the frozen plans it inherits; `get` as AT5 leaves it for AT-G8 |
| Reps | 30 measured fresh-process AB/BA pairs after 2 qualification pairs, for every gate; three layout builds for AT-G1, AT-G5 and AT-G6 (F12) |
| Threshold | One-sided 95% CI upper bound of the ratio at or below the bound of each gate |
| Compare command | Per gate in the [capture contract](#capture-contract) |
| Escalation lever | Per gate below; no bound changes without an owner decision recorded here |

## Gates

| Gate | Candidate / base | Bound |
|------|------------------|-------|
| AT-G1 | Session read, 64-byte projection / mmap projection, 1,024-page set | <= 1.50 |
| AT-G2 | Session read, whole-granule fold / mmap fold, 1,024-page set | <= 1.10 |
| AT-G3 | Session read, whole-granule fold / mmap fold, 256 MiB set, hugepage-backed arena against ext4 mmap on 4 KiB pages | <= 1.00 |
| AT-G4 | `get` hit after the swap / `get` hit on `main` at `ade5310`, `resident_access_ordinary` | <= 0.85 |
| AT-G5 | `read_copy` of 64 bytes / mmap copy of 64 bytes, 1,024-page set (was WRC-G1) | <= 1.50 |
| AT-G6 | `read_copy` of 64 bytes / optimistic floor (was WRC-G2) | <= 1.20 |
| AT-G7 | Frozen DRP-G2 warm and cycling; DRP-G4 eight-thread and scaling; RC-G1 and RC-G4 | 1.01 / 1.01; 1.00 / 0.50; as recorded in `readahead_coalescing.md` |
| AT-G8 | `get` hit after the hit-path change / `get` hit as AT5 leaves it, fair `get` lane | <= 0.85 |

The owner selected these bounds on 2026-11-14. AT-G5 and AT-G6 keep the bounds of
WRC-D2. AT-G1 to AT-G6 and AT-G8 also require zero issued I/O, zero warmed allocations
and equal checksums across arms. AT-G7 inherits workload, baseline, placement, concurrency and
sample contracts from its own plans unchanged.

## Fairness contract

Owner decisions AT-D9 to AT-D13 of 2026-11-17. A lane decides AT-G1, AT-G2, AT-G3,
AT-G5, AT-G6 or AT-G8 only when it satisfies F1 to F11; F12 adds a repetition level to
the tight gates. No rule removes a cost that a caller of the product API pays, and no
bound changes. AT-G4 is grandfathered on its legacy lane: both of its arms run the same
loop of `resident_access_ordinary`, so F2 to F4 do not bear on its ratio, its recorded
result stands, and the campaign's recapture builds that one runner against both trees
(F10): the AT-G4 command with `--shared-runner` appended, into
`target/array-translation/at-g4-campaign`, under its own frozen manifest
`manifests/at-g4-campaign.json`, compared at the same 0.85.

| Rule | Requirement | Checked by |
|------|-------------|------------|
| F1 | Both arms consume the same bytes in the same order through the same fold, from one request vector built before timing | Checksum equality per pair in `warm-prepare` |
| F2 | The timed loop of an arm holds the request load, the access expression, the one outcome branch the API requires with its failure arm cold and out of line, the shared fold and the checksum add; no error conversion, re-wrapping, arm dispatch or per-read counter. Counts are derived after the loop | Review |
| F3 | One loop function per arm, `#[inline(never)]`, with primitive arguments; the arm is selected once outside it and no harness helper sits between the loop and the product call | Review |
| F4 | Barriers are identical: `black_box` on the request input and on the fold output on both arms or on neither; the fold has the same attributes and argument types on both | Review |
| F5 | The harness adds no inlining attribute to product code and forces none through its own; the build profile is pinned in the manifest and the evidence retains the disassembly of each loop function | Review of the retained disassembly |
| F6 | Each arm runs one untimed pass over the full request set through its loop function, then a timed region of at least 100 ms at the lane's one repetition count, which is the same for every arm of the lane and its floor; a resident lane records zero major faults and a zero minor-fault delta in the timed region | Row validation |
| F7 | Each row records, per arm, the backing page size of the mapping it reads (`AnonHugePages`, `FilePmdMapped`, `KernelPageSize`), the THP mode, the advice applied and the filesystem. A lane is equal-reach or product-posture and is named as such | Row validation |
| F8 | The plan states the regime per arm (resident, minor fault, major fault) and the setup excluded from timing; setup time is reported | Plan review |
| F9 | Same CPU and, for mmap against pool, the same executable; one fresh process per sample in AB/BA order | `warm-prepare` |
| F10 | A comparison across product versions builds one runner source against both product trees; an arm that calls an API one tree lacks is compiled under a build-script cfg detected from that tree's sources, the runner hash covers the cfg-gated sources, and unequal runner hashes reject the set | `warm-prepare` |
| F11 | Beside `paired.csv`, `warm-prepare` writes `report.json` with ns per read per arm (elapsed over the row's operation count), the paired difference with its 95% interval from the shared harness's `summarize`, and the same for the floor comparison, which the capture records as a second comparison of the lane. `paired.csv` and the compare command keep their form | `report.json` |
| F12 | AT-G1, AT-G5 and AT-G6 run on three builds that differ only in a recorded code-layout perturbation (function alignment of 16, 32 and 64 bytes), 30 pairs each under one frozen manifest per build; AT-G5 and AT-G6 pass only when all three pass, AT-G1 passes on the 90 pairs of its three builds pooled, each build's result reported beside it (AT-D24) | Manifests and gate outputs |

Fair lanes live in their own module of `benches/mmap_workloads`. The floor arm
`plain_array` runs the lane's loop and fold over a plain in-process array of the lane's
geometry with the lane's request vector; it is reported with every fair lane and never
gated. The optimistic floor of AT-G6 is a separate arm with the same geometry: it runs
over a plain array of the lane's 1,024 pages with the lane's request vector. An inlined-fold variant of each fair lane
is reported as characterization.

Legacy lanes are frozen: `resident_access_ordinary`, `resident_access_hinted`,
`resident_access_epoch_batch`, `resident_access_retained`, `resident_access_page_batch`,
`resident_projection`, `resident_decode` and the characterization mode of
`mmap_tlb_pressure` keep the loop code of `604410a`. They are harness-shape lanes:
their mmap-against-pool ratios, 5.97 for `get` on 2026-11-17 included, are history and
are not cited as the cost of the product against mmap. AT-G4 stands as recorded; both
of its executables carry the runner hash `45e94b46`. A recapture of AT-G4 follows F10.

## Workload contract

AT-G1, AT-G2, AT-G5 and AT-G6 use the initialized resident fixture of the
`resident_access_*` lanes: 1,024 pages of 4 KiB, 4 MiB of payload, prefilled outside
timing with mmap PTEs populated. The request sequence is the fixed 65,536-request
sequence, frozen before measurement and repeated per timed process: 256 times for the
whole-granule lane of AT-G2 (16,777,216 reads) and 2,048 times for the 64-byte lanes of
AT-G1, AT-G5, AT-G6 and AT-G8 (134,217,728 reads), so that the floor arm's timed region
also meets F6.

- AT-G1 reads one 64-byte projection per request. The session arm opens one
  `FileSession` with `Pool::session_file` per 16 requests (AD-18), the same chunking as
  `resident_access_epoch_batch`.
  In every session lane (AT-G1, AT-G2, AT-G3) the `mmap_random` and `plain_array` arms
  iterate the same 16-request chunks with nothing at the chunk boundary, so the three
  arms differ only in the access expression and in opening and dropping the session
  (AT-D16). The arm `mmap_random_flat` runs the same requests in one flat loop; its
  comparison against `dios_session` is captured with every session-lane capture and
  reported in `report.json`, not gated.
- AT-G2 folds the whole granule (512 `u64` words) per request over sequential pages,
  with the same 16-request sessions.
- AT-G5 and AT-G6 copy the 64 bytes into a caller buffer and consume that buffer on
  every arm. The floor arm loads a packed version word, copies and rechecks with no
  translation, over a private plain array of the lane's 1,024 pages with no concurrent
  writer.
- AT-G3 runs the fair lane `resident_access_session_fold_256m`: the fixture's 65,536
  pages all resident (a 256 MiB set), a whole-granule fold per request at a uniformly
  random index, a fixed 65,536-index sequence that covers every page, repeated 64 times
  (4,194,304 folds), sessions of 16 requests, arms `mmap_random` and `dios_session` on
  the shipping backend. The run is product-posture (F7): the arena is hugepage-backed
  under the `bench-thp-madvise` host profile while the ext4 file mapping stays on
  4 KiB pages. The same lane captured under the ordinary profile, arena on 4 KiB
  pages, is the equal-reach row: validated by `warm-prepare` against
  `manifests/at-g3-equal-reach.json`, reported and not gated (AT-D12). `mmap_tlb_pressure` keeps only
  its characterization mode.
- AT-G4 runs the `dios_serial` arm of `resident_access_ordinary` in the candidate
  executable against the same arm in the base executable built from `main` at
  `ade5310`, captured as rich rows by the collector's `--base-build` option (AT-D5).
  `pair-bench` is not used.
- AT-G8 runs the `dios_get` arm of the fair lane `resident_access_fair_get` (64-byte
  projection, the AT-G1 request sequence and repetitions) in one runner source built
  against the tree AT5 leaves and against the tree after the hit-path change (F10).
  The lane's own pair, `mmap_random` against `dios_get` in the changed tree's
  executable, is a second capture into `target/array-translation/at-g8-mmap` with its
  floor comparison, validated against `manifests/at-g8-mmap.json`, reported and not
  gated.
- AT-G7 runs the frozen lanes under their own plans. The DRP lanes need the frozen
  runner revision and fresh base of the owner amendment of 2026-09-14 first.

DRP prerequisite: the frozen-runner protocol revision and the fresh base under it are owned by Sören (DRP owner amendment of 2026-09-14) and are prepared in AT3, before its first AT-G7 campaign; old-protocol base samples are never paired with array-translation candidates.

THP prerequisite: the `bench-thp-madvise` host profile (THP `madvise`, otherwise the recorded bench profile) is owned by Sören and exists on `nix` before the first AT-G3 capture; the AT-G3 manifest pins it and the collector accepts it for AT-G3 only.

Host protocol: `nix` (Threadripper 3970X, Linux 6.6.64, Samsung 970 PRO), performance
governor, the recorded bench profile behind `mise run bench-host-guard`, consumer on
CPU 0 and controller on CPU 4 for single-thread lanes. Concurrency is one reader in
AT-G1 to AT-G6; arrival is closed-loop.
Bench tasks run through the retained mise executable
`/home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64` that `bench-host-guard` pins (owner decision of 2026-08-24, `scopes/done/pinned-frame-retention/validation.yaml`).

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
mapping from `/proc/<pid>/smaps`. In the gated capture under `bench-thp-madvise`, a run
whose arena is not fully hugepage-backed is invalid, not failed. In the equal-reach
capture a run is valid only when the arena holds no huge page (`AnonHugePages` of 0). The [evidence README](../evidence/array_translation/README.md)
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
| AT-G1 | manifest `benches/evidence/array_translation/manifests/at-g1.json` | `mise run remote -- env MISE_EXE=/home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 /home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 run bench-mmap-workloads target/array-translation/at-g1 --lanes resident_access_session_projection` | `mise run remote -- /home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 exec uv@0.12.12 -- uv run --python 3.12 benches/mmap_workloads/validate.py warm-prepare target/array-translation/at-g1 benches/evidence/array_translation/manifests/at-g1.json target/array-translation/at-g1/paired.csv` | `uv run --python 3.12 benches/mmap_workloads/validate.py pool-layouts benches/evidence/array_translation/at-g1`, then `mise run gate benches/evidence/array_translation/at-g1/paired.csv 1.50` (pooled, gated) and `mise run gate benches/evidence/array_translation/at-g1/layout-NN/paired.csv 1.50` per build (reported) | AT4 (lane, session arms of `warm-prepare`); `warm-prepare` AT3; `pool-layouts` AT5 (AT-D24) |
| AT-G2 | manifest `benches/evidence/array_translation/manifests/at-g2.json` | `mise run remote -- env MISE_EXE=/home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 /home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 run bench-mmap-workloads target/array-translation/at-g2 --lanes resident_access_session_fold` | `mise run remote -- /home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 exec uv@0.12.12 -- uv run --python 3.12 benches/mmap_workloads/validate.py warm-prepare target/array-translation/at-g2 benches/evidence/array_translation/manifests/at-g2.json target/array-translation/at-g2/paired.csv` | `mise run gate benches/evidence/array_translation/at-g2/paired.csv 1.10` | AT4 (lane, session arms of `warm-prepare`); `warm-prepare` AT3 |
| AT-G3 | manifest `benches/evidence/array_translation/manifests/at-g3.json` pinning the `bench-thp-madvise` profile; `manifests/at-g3-equal-reach.json` for the ungated row | `mise run remote -- env MISE_EXE=/home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 /home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 run bench-mmap-workloads target/array-translation/at-g3 --lanes resident_access_session_fold_256m` under `bench-thp-madvise`; the same command into `target/array-translation/at-g3-equal-reach` under the ordinary profile | `mise run remote -- /home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 exec uv@0.12.12 -- uv run --python 3.12 benches/mmap_workloads/validate.py warm-prepare target/array-translation/at-g3 benches/evidence/array_translation/manifests/at-g3.json target/array-translation/at-g3/paired.csv` | `mise run gate benches/evidence/array_translation/at-g3/paired.csv 1.00` | AT4 |
| AT-G4 | manifest `benches/evidence/array_translation/manifests/at-g4.json` pinning the base and candidate executables | `mise run remote -- env MISE_EXE=/home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 /home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 run bench-mmap-workloads target/array-translation/at-g4 --lanes resident_access_ordinary --base-build /home/srnnkls/build/dios-at-base-ade5310` | `mise run remote -- /home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 exec uv@0.12.12 -- uv run --python 3.12 benches/mmap_workloads/validate.py warm-prepare target/array-translation/at-g4 benches/evidence/array_translation/manifests/at-g4.json target/array-translation/at-g4/paired.csv` | `mise run gate benches/evidence/array_translation/at-g4/paired.csv 0.85` | AT3 (`--base-build`, `warm-prepare`), pinned by AT2 |
| AT-G5 | manifest `benches/evidence/array_translation/manifests/at-g5.json` | `mise run remote -- env MISE_EXE=/home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 /home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 run bench-mmap-workloads target/array-translation/at-g5 --lanes resident_access_copy` | `mise run remote -- /home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 exec uv@0.12.12 -- uv run --python 3.12 benches/mmap_workloads/validate.py warm-prepare target/array-translation/at-g5 benches/evidence/array_translation/manifests/at-g5.json target/array-translation/at-g5/paired.csv` | `mise run gate benches/evidence/array_translation/at-g5/paired.csv 1.50` | AT6 (lane); `warm-prepare` AT3 |
| AT-G6 | manifest `benches/evidence/array_translation/manifests/at-g6.json` | `mise run remote -- env MISE_EXE=/home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 /home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 run bench-mmap-workloads target/array-translation/at-g6 --lanes resident_access_copy_floor` | `mise run remote -- /home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 exec uv@0.12.12 -- uv run --python 3.12 benches/mmap_workloads/validate.py warm-prepare target/array-translation/at-g6 benches/evidence/array_translation/manifests/at-g6.json target/array-translation/at-g6/paired.csv` | `mise run gate benches/evidence/array_translation/at-g6/paired.csv 1.20` | AT6 (lane); `warm-prepare` AT3 |
| AT-G7 | manifest `benches/evidence/array_translation/manifests/at-g7.json` naming the frozen DRP runner and RC6 sources | DRP lanes: `mise run remote -- env MISE_EXE=/home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 /home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 exec -- bash benches/evidence/array_translation/at-g7/build_drp.sh "$(git rev-parse HEAD)"`, then `mise run remote -- /home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 exec uv@0.12.12 -- uv run --python 3.12 benches/evidence/array_translation/at-g7/collect_drp.py "$(git rev-parse --short=7 HEAD)"`. RC-G1: `mise run remote -- env MISE_EXE=/home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 /home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 run bench-scan-geometry /dev/shm/dios-at-g7-rc-g1 --input target/array-translation/at-g4/input --experiment coalescing --mode run --cases cold-mmap,pressure-mmap --frozen-binary /home/srnnkls/build/dios-readv-budget-20260914/target/budget-archives/mmap_workloads`. RC-G4: `mise run remote -- env MISE_EXE=/home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 /home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 run bench-mmap-workloads target/array-translation/at-g7-rc-g4 --lanes automatic_fragmented,automatic_dependent --input target/array-translation/at-g4/input` | written on `nix` by the captures: the DRP converter's `prepare` per lane behind `validate-g2-zero-alloc` for the DRP-G2 lanes, and each RC collector's own `paired.csv` | `mise run gate benches/evidence/array_translation/at-g7/drp/drp_g2_warm_ordinary/paired.csv 1.01`, `drp_g2_cycling_reuse` at `1.01`, `drp_g4_ordinary_base_8t` at `1.00`, `drp_g4_ordinary_scaling` at `0.50`; `mise run gate benches/evidence/array_translation/at-g7/rc-g1/cold-mmap/paired.csv 1.00` and `pressure-mmap` at `1.00`; `mise run gate benches/evidence/array_translation/at-g7/rc-g4/automatic_fragmented/paired.csv 1.02` and `automatic_dependent` at `1.02` with zero automatic admissions | AT3 |
| AT-G8 | manifest `benches/evidence/array_translation/manifests/at-g8.json` pinning both product trees and the one runner hash | `mise run remote -- env MISE_EXE=/home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 /home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 run bench-mmap-workloads target/array-translation/at-g8 --lanes resident_access_fair_get --base-build /home/srnnkls/build/dios-at-base-at5 --shared-runner` | `mise run remote -- /home/srnnkls/build/dios-gates/tools/mise-v2026.8.0-linux-x64 exec uv@0.12.12 -- uv run --python 3.12 benches/mmap_workloads/validate.py warm-prepare target/array-translation/at-g8 benches/evidence/array_translation/manifests/at-g8.json target/array-translation/at-g8/paired.csv` | `mise run gate benches/evidence/array_translation/at-g8/paired.csv 0.85` | AT9 (`--shared-runner` and the lane in AT4) |

The AT-G1 and AT-G2 lanes pair the `mmap_random` arm against the session arm; AT-G5
pairs the mmap copy against `read_copy`; AT-G6 pairs the optimistic floor arm against
`read_copy`. For AT-G4, `--base-build` names a clean worktree of `main` at `ade5310`
at `/home/srnnkls/build/dios-at-base-ade5310` on `nix`; the collector builds the base
executable there with the candidate's build command and runs the unchanged `sample`
mode of each executable over one fixture, one fresh process and one uniquely named
JSON row per arm, in AB/BA order, 2 qualification pairs then 30 measured pairs. The
AT-G4 manifest pins both executables and the fixture by SHA-256, and `warm-prepare`
validates the rows against it before writing `paired.csv`.

For AT-G1, AT-G5 and AT-G6 the capture, paired-sample, rsync and compare steps run once
per layout build: the capture takes `--layout 16`, `--layout 32` or `--layout 64` and
writes `target/array-translation/at-gN/layout-16`, `layout-32` or `layout-64`; the
paired-sample step validates that directory against its own frozen manifest
`manifests/at-gN-layout-16.json`, `-32.json` or `-64.json`, which records the alignment
flag, and writes `paired.csv` and `report.json` inside it; the compare runs over each
copied `at-gN/layout-NN/paired.csv` (F12); for AT-G1, `pool-layouts` first checks that
each of the three copied builds holds 30 measured pairs under a distinct recorded layout
and writes their pairs into `at-g1/paired.csv`, which the gate decides (AT-D24).
`--shared-runner` builds this tree's runner sources against the base tree named by
`--base-build` (F10).

Characterization, reported and not gated:

- Resident bytes of the translation mapping after each lane, with the arena size beside it.
- Session length sweep at 1, 4, 16 and 64 requests on the AT-G1 lane.
- `read_copy` fast and fallback counts and optimistic-only page eviction on the existing pressure-random workload.
- A matched CPU profile of the session arm and the mmap arm for AT-G1.

## Escalation levers

- AT-G1, AT-G2: profile the failing arm with the `flamegraph` skill and diff against
  the mmap arm. The liveness lever and the fused open are adopted and the CLOCK bit in
  the entry is rejected (AT-D18 to AT-D22); a failure after them returns to the owner
  with the profile, and code layout is not tuned to pass.
- AT-G3: with a hugepage-backed arena and a failing ratio, profile both arms before
  any TLB reasoning. A confirmed failure is recorded as non-adoption of the
  beats-mmap claim and returns to the owner.
- AT-G4: profile `get`; the swap does not land until the gate passes.
- AT-G5, AT-G6: a safety failure blocks code. A performance failure keeps the result
  and profiles the exact arms. No fence removal, `FrameState` change, weakened witness,
  raised bound or substitution of batching or retention.
- AT-G7: a regression blocks adoption.
- AT-G8: profile the fair `get` arm; the hit-path change does not land until the gate
  passes. No assertion is removed to pass it.

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
