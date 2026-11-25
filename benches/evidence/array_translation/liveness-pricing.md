# Liveness pricing (AT8)

Scratch characterization of what file-liveness validation costs per read in the adopted
design (design.md "File liveness", owner decisions AT-D18 to AT-D22), at product tree
43b34c9, layout-32 build, on the bench host `nix`. Nothing here is a gate; no gate
bound is evaluated.

## Result

Prices in ns per read are paired per round (same seed, same round, adjacent processes), from the quiet batch (rounds 17 to 26, n = 10 pairs). Instructions and branches per read are perf stat differences between variants.

| Price | Paired median | Mean ± sd | Range | Instructions per read | Branches per read |
|---|---|---|---|---|---|
| Session read, liveness per read instead of per view (V1 − V0) | +0.279 ns | +0.284 ± 0.070 | +0.195 to +0.427 | +6.19 | +1.063 |
| Session read, liveness once per view (V0 − V2) | +0.068 ns | +0.038 ± 0.102 | −0.212 to +0.139 | −0.50 | +0.125 |
| `get` hit, mirror validated per hit (V0 − V3) | +0.629 ns | +0.558 ± 0.621 | −1.023 to +1.482 | +15.99 | +2.000 |

- V1 − V0 is positive in all 10 quiet pairs and in all 23 clean pairs over every batch (pooled median +0.277 ns).
- V0 − V2 cannot be told apart from zero at this n. The once-per-view check runs once per 16-read session in this arm (`SESSION_REQUESTS = 16`), which matches the 0.125 branches per read (2 per session). V2 retires 0.50 more instructions per read than V0, which this report does not attribute.
- V0 − V3: the spread comes from two outlier processes (V0 round 17 at 12.217 and V3 round 24 at 12.599 ns). Without those two rounds the 8 pairs give +0.640 ± 0.095 ns (range +0.542 to +0.834). The pooled median over 23 clean pairs is +0.644 ns.
- Session/mmap ratio by median, quiet batch: V0 5.646 / 3.928 = 1.437, V1 5.920 / 3.919 = 1.511, V2 5.582 / 3.924 = 1.423.

The paired control differences have medians within ±0.013 ns. These pairs change no code in the measured path: mmap_random for V1, V2 and V3 against V0; dios_get for V1 and V2 against V0; dios_session for V3 against V0. The pairs agree with the instruction counts, which are identical to within 0.01 per read across those variants.

## Variants

Each variant is a copy of `git archive 43b34c9` (without `benches/evidence` and `scopes`) carrying exactly one edit:

- V0 is the tree as is.
- V1 adds the per-read mirror load and compare back into `FileView::resolve`, in the order it had before AT-D19: entry load, then the mirror compare, then the frame match. The once-per-view check in `FileView::open` stays, as it did before AT-D19.
- V2 removes the session read's only liveness check, the one in `FileView::open`. This variant is unsound and serves only as a lower bound.
- V3 removes the mirror validation from `Pool::pin_live`, which is `get`'s lock-free hit path and also serves the re-resolve inside `get_missed`. This variant is unsound and serves only as a lower bound.

V1:

```diff
--- a/src/pool/session.rs
+++ b/src/pool/session.rs
@@ -12,7 +12,10 @@
 use super::frames::Frames;
 use super::frames::SessionArena;
 use super::translation::{Translation, entry_frame};
-use super::{Clock, GetError, PageId, Pool, PoolBackend, file_generation_is_live};
+use super::{
+    Clock, GetError, PageId, Pool, PoolBackend, file_generation_is_live, file_live_word,
+    live_word_is_live,
+};
 
 /// Many resident-only reads of one reader under one epoch pin; borrows the
 /// pool and the reader and is neither `Send` nor `Sync`.
@@ -51,6 +54,8 @@
     entries: &'pool [AtomicU32],
     arena: SessionArena<'pool>,
     clock: &'pool Clock,
+    live_generation: &'pool AtomicU64,
+    live_word: u64,
     file: FileId,
 }
 
@@ -264,6 +269,8 @@
             entries,
             arena,
             clock,
+            live_generation: &file_live_generations[file.slot() as usize],
+            live_word: file_live_word(file),
             file,
         })
     }
@@ -284,7 +291,13 @@
                 page: PageId::new(self.file, granule_idx),
             });
         };
-        let Some(frame) = entry_frame(entry) else {
+        let frame = entry_frame(entry);
+        if !live_word_is_live(self.live_generation, self.live_word) {
+            return Err(GetError::StaleFile {
+                page: PageId::new(self.file, granule_idx),
+            });
+        }
+        let Some(frame) = frame else {
             return Ok(SessionRead::Absent);
         };
         let (reference_bit, bytes) = self.arena.frame(pin, frame);
```

V2:

```diff
--- a/src/pool/session.rs
+++ b/src/pool/session.rs
@@ -254,11 +254,6 @@
             "file identity used with a foreign pool"
         );
         let entries = translation.slot_entries(file);
-        if !file_generation_is_live(&file_live_generations[file.slot() as usize], file) {
-            return Err(GetError::StaleFile {
-                page: PageId::new(file, 0),
-            });
-        }
         Ok(Self {
             reader,
             entries,
```

V3:

```diff
--- a/src/pool/mod.rs
+++ b/src/pool/mod.rs
@@ -2618,13 +2618,6 @@
             slot.abort_pin(begun);
             return Err(PinMiss::Absent);
         };
-        if !file_generation_is_live(
-            &self.file_live_generations[page.file().slot() as usize],
-            page.file(),
-        ) {
-            slot.abort_pin(begun);
-            return Err(PinMiss::StaleFile);
-        }
         Ok(self.pin_commit(begun, frame, slot))
     }
```

After the copies were made, `diff -r v0 vN` on the host showed only these hunks.

## Method

- Build per variant follows `build_binary` and `layout_environment(32)` in `benches/mmap_workloads/collect.py`, with each tree building its own runner:
  `RUSTFLAGS="-C llvm-args=-align-all-functions=5" DIOS_PRODUCT_SOURCE_COMMIT=43b34c9e32b6bc5007e0127006c845363d7cd51e mise exec -- cargo build --profile profiling --features bench --bench mmap_workloads --message-format=json`.
  The build cfg `dios_session_file` is set in every variant, so the session arm uses `Pool::session_file`. Runner sha256 prefixes:
  - V0: `69b7dd187ff8e497`
  - V1: `346054403fa713ac`
  - V2: `a4e5657ab90222b4`
  - V3: `d52aaff44900d78e`
- The fixture is the runner's own: `runner-v0 create input`, created in the scratch directory (65,536 pages of 4,096 bytes).
- Each timed process runs as `flock /tmp/dios-bench.lock taskset -c 0 ./runner-vN sample input LANE ARM SEED plain OUT.json`, with seed = round. The lanes and arms are `resident_access_session_projection` with arms `dios_session` and `mmap_random`, and `resident_access_fair_get` with arm `dios_get`. Every process performs 134,217,728 reads. ns per read = `elapsed_ns / operations`.
- Within a round, all three arms run, each over the four variants, and the variant order rotates by round. THP is checked for `[never]` before every process.
- A process counts as contaminated, and is excluded, when `elapsed_ns > 1.02 × cpu_ns`. From round 9 on, a contaminated process is retried, at most 5 attempts.
- Instructions per read come from perf 6.6.64 (`nix-shell -p linuxPackages.perf`) as `perf stat -x, -e instructions:u,cycles:u,branches:u,branch-misses:u` around the same sample command. They are whole-process counts divided by reads, so they include the fixture warm pass and pool setup; the differences between variants are the per-read price.

## Host state

| Item | Value |
|---|---|
| Kernel | 6.6.64 (NixOS) |
| THP | `always madvise [never]` before, during and after every process |
| THP defrag | `[madvise]` |
| CPU frequency | Governor `performance`, driver `acpi-cpufreq`, boost 1 |
| Uptime at the first build | 3 minutes |

A foreign workload overlapped the first two batches. During rounds 1 to 16 and the first perf pass (seeds 101 and 102), another agent's `mmap_workload_contract` test binary from `~/build/dios` ran without the bench lock. Its runs time-shared CPU 0 and roughly doubled `elapsed_ns` against `cpu_ns`, and the contamination rule excluded 35 of 193 processes in those rounds. Rounds 17 to 26 and the second perf pass (seeds 103 and 104) recorded no process from `~/build/dios` before or after each timed process, and none of their 120 timed processes was excluded.

## Summary per variant and arm

Quiet batch, rounds 17 to 26, n = 10 processes each:

| Arm | Variant | Mean ns/read | sd | Median | Min | Max |
|---|---|---|---|---|---|---|
| dios_session | V0 | 5.653 | 0.049 | 5.646 | 5.588 | 5.772 |
| dios_session | V1 | 5.937 | 0.069 | 5.920 | 5.849 | 6.087 |
| dios_session | V2 | 5.615 | 0.101 | 5.582 | 5.509 | 5.879 |
| dios_session | V3 | 5.641 | 0.058 | 5.624 | 5.576 | 5.772 |
| mmap_random | V0 | 3.929 | 0.009 | 3.928 | 3.916 | 3.942 |
| mmap_random | V1 | 3.920 | 0.011 | 3.919 | 3.909 | 3.946 |
| mmap_random | V2 | 3.926 | 0.006 | 3.924 | 3.918 | 3.932 |
| mmap_random | V3 | 3.977 | 0.139 | 3.935 | 3.919 | 4.372 |
| dios_get | V0 | 11.552 | 0.243 | 11.444 | 11.420 | 12.217 |
| dios_get | V1 | 11.756 | 0.672 | 11.449 | 11.392 | 13.099 |
| dios_get | V2 | 11.460 | 0.061 | 11.439 | 11.408 | 11.606 |
| dios_get | V3 | 10.995 | 0.566 | 10.829 | 10.735 | 12.599 |

All clean processes, rounds 2 to 26 (n = 23 or 24):

| Arm | V0 median | V1 median | V2 median | V3 median |
|---|---|---|---|---|
| dios_session | 5.620 | 5.913 | 5.582 | 5.617 |
| mmap_random | 3.928 | 3.920 | 3.926 | 3.935 |
| dios_get | 11.441 | 11.434 | 11.419 | 10.783 |

Paired differences per round, ns per read:

| Arm | Difference | Quiet n | Quiet median | Quiet mean ± sd | Pooled n | Pooled median |
|---|---|---|---|---|---|---|
| dios_session | V1 − V0 | 10 | +0.279 | +0.284 ± 0.070 | 23 | +0.277 |
| dios_session | V0 − V2 | 10 | +0.068 | +0.038 ± 0.102 | 23 | +0.056 |
| dios_get | V0 − V3 | 10 | +0.629 | +0.558 ± 0.621 | 23 | +0.644 |
| dios_session | V3 − V0 (control) | 10 | −0.012 | −0.012 ± 0.084 | 24 | +0.006 |
| dios_get | V1 − V0 (control) | 10 | +0.004 | +0.203 ± 0.738 | 23 | +0.002 |
| dios_get | V2 − V0 (control) | 10 | −0.011 | −0.092 ± 0.266 | 23 | −0.024 |
| mmap_random | V1 − V0 (control) | 10 | −0.013 | −0.009 ± 0.015 | 23 | −0.012 |

perf stat per read. Instructions and branches come from seeds 103 and 104 and match seeds 101 and 102 to within 0.05 per read. Cycles and branch misses come from seeds 103 and 104:

| Arm | Variant | Instructions | Cycles (two processes) | Branches | Branch misses per 1,000 reads |
|---|---|---|---|---|---|
| dios_session | V0 | 77.82 | 26.18 / 24.77 | 15.209 | 84.12 / 82.57 |
| dios_session | V1 | 84.01 | 26.03 / 25.87 | 16.272 | 78.66 / 77.88 |
| dios_session | V2 | 78.32 | 24.66 / 24.50 | 15.084 | 82.79 / 83.05 |
| dios_session | V3 | 77.83 | 24.82 / 26.83 | 15.210 | 83.72 / 83.17 |
| mmap_random | V0 to V3 | 62.76 | 16.87 to 17.38 | 12.086 | 62.66 |
| dios_get | V0 | 166.93 | 57.36 / 49.84 | 31.154 | 0.45 / 0.45 |
| dios_get | V1 | 166.93 | 49.43 / 50.84 | 31.155 | 0.44 / 0.44 |
| dios_get | V2 | 166.93 | 49.36 / 49.91 | 31.155 | 0.45 / 0.44 |
| dios_get | V3 | 150.93 | 47.15 / 46.75 | 29.155 | 0.45 / 0.44 |

## Raw per-process numbers

The tables give ns per read by round, with seed = round. `(x r)` marks a contaminated process (elapsed > 1.02 × cpu time), excluded above. Two values in one cell are a rejected attempt and its retry.

dios_session:

| round | V0 | V1 | V2 | V3 |
|---|---|---|---|---|
| 1 | (11.046 r) | (11.642 r) | (11.068 r) | (11.201 r) |
| 2 | 5.597 | 5.902 | 5.615 | 5.579 |
| 3 | 5.559 | 5.884 | 5.546 | 5.574 |
| 4 | 5.605 | 5.815 | 6.536 | 5.600 |
| 5 | 5.581 | 5.919 | 5.577 | 5.869 |
| 6 | 5.811 | 5.959 | 5.595 | 5.593 |
| 7 | 5.597 | (11.719 r) | (11.118 r) | 5.749 |
| 8 | (11.512 r) | (11.718 r) | (11.084 r) | (11.234 r) |
| 9 | 5.682 | (13.903 r) 5.923 | 5.582 | 5.618 |
| 10 | 5.621 | 5.845 | 5.541 | 6.452 |
| 11 | 5.569 | 5.911 | 5.780 | 5.582 |
| 12 | 5.671 | 5.909 | 5.600 | 5.586 |
| 13 | 5.615 | 7.034 | 5.776 | 6.311 |
| 14 | 5.622 | 5.899 | 5.557 | 5.661 |
| 15 | 5.610 | 7.464 | 5.558 | 5.615 |
| 16 | 5.606 | 5.855 | 5.539 | 5.613 |
| 17 | 5.648 | 6.009 | 5.509 | 5.609 |
| 18 | 5.645 | 5.922 | 5.589 | 5.636 |
| 19 | 5.588 | 5.892 | 5.560 | 5.621 |
| 20 | 5.667 | 5.919 | 5.879 | 5.713 |
| 21 | 5.645 | 5.927 | 5.564 | 5.599 |
| 22 | 5.672 | 5.884 | 5.567 | 5.576 |
| 23 | 5.615 | 5.913 | 5.614 | 5.772 |
| 24 | 5.619 | 5.849 | 5.639 | 5.626 |
| 25 | 5.772 | 5.966 | 5.650 | 5.616 |
| 26 | 5.660 | 6.087 | 5.575 | 5.645 |

mmap_random:

| round | V0 | V1 | V2 | V3 |
|---|---|---|---|---|
| 1 | (7.858 r) | (7.784 r) | (7.832 r) | (7.824 r) |
| 2 | 3.919 | 3.894 | 3.922 | 3.895 |
| 3 | 3.926 | 3.912 | 3.920 | 3.891 |
| 4 | 3.931 | 3.928 | 3.892 | 3.916 |
| 5 | 3.981 | 3.894 | 4.032 | 3.899 |
| 6 | 3.944 | 3.910 | 4.466 | 3.992 |
| 7 | (7.871 r) | (7.823 r) | (7.839 r) | (7.873 r) |
| 8 | (7.877 r) | (7.821 r) | (7.868 r) | (7.833 r) |
| 9 | 4.897 | 3.924 | 3.915 | 3.927 |
| 10 | 3.917 | 3.915 | 3.932 | 3.938 |
| 11 | 3.925 | 3.946 | 3.956 | 3.952 |
| 12 | 3.924 | 3.920 | 3.923 | 3.935 |
| 13 | 3.928 | 3.934 | 3.935 | 3.936 |
| 14 | 3.920 | 3.914 | 3.928 | 3.938 |
| 15 | 3.929 | 3.924 | 3.913 | 3.946 |
| 16 | 3.946 | 3.926 | 3.934 | 3.941 |
| 17 | 3.918 | 3.921 | 3.932 | 4.372 |
| 18 | 3.928 | 3.909 | 3.922 | 3.951 |
| 19 | 3.942 | 3.923 | 3.932 | 3.944 |
| 20 | 3.929 | 3.946 | 3.920 | 3.921 |
| 21 | 3.925 | 3.912 | 3.929 | 3.932 |
| 22 | 3.941 | 3.915 | 3.921 | 3.923 |
| 23 | 3.916 | 3.918 | 3.932 | 3.935 |
| 24 | 3.921 | 3.924 | 3.918 | 3.919 |
| 25 | 3.937 | 3.909 | 3.922 | 3.937 |
| 26 | 3.932 | 3.920 | 3.926 | 3.935 |

dios_get:

| round | V0 | V1 | V2 | V3 |
|---|---|---|---|---|
| 1 | (14.492 r) | (22.628 r) | (22.551 r) | (21.566 r) |
| 2 | 11.355 | 11.337 | 11.326 | 10.856 |
| 3 | 11.305 | 11.287 | 11.322 | 10.679 |
| 4 | 11.537 | 11.345 | 11.332 | 10.706 |
| 5 | 11.340 | 11.736 | 11.401 | 10.766 |
| 6 | 11.345 | 11.447 | 11.371 | 10.908 |
| 7 | (22.648 r) | (22.761 r) | (22.651 r) | (21.375 r) |
| 8 | (22.872 r) | (22.692 r) | (22.688 r) | (21.391 r) |
| 9 | 11.341 | 11.348 | 11.366 | 10.825 |
| 10 | 11.541 | 11.363 | 11.478 | 10.742 |
| 11 | 11.476 | 11.696 | 11.372 | 10.783 |
| 12 | 11.462 | 11.469 | 11.372 | 10.749 |
| 13 | 11.415 | 11.384 | 11.405 | 10.728 |
| 14 | 11.396 | 11.439 | 11.545 | 12.138 |
| 15 | 11.593 | 11.430 | 11.419 | 10.759 |
| 16 | 13.033 | 11.422 | 11.420 | 10.742 |
| 17 | 12.217 | 11.392 | 11.422 | 10.735 |
| 18 | 11.432 | 13.099 | 11.408 | 10.890 |
| 19 | 11.420 | 11.422 | 11.421 | 10.777 |
| 20 | 11.531 | 11.431 | 11.411 | 10.832 |
| 21 | 11.607 | 12.957 | 11.488 | 10.773 |
| 22 | 11.447 | 11.458 | 11.502 | 10.800 |
| 23 | 11.441 | 11.440 | 11.456 | 10.827 |
| 24 | 11.575 | 11.458 | 11.421 | 12.599 |
| 25 | 11.428 | 11.434 | 11.606 | 10.884 |
| 26 | 11.425 | 11.467 | 11.466 | 10.831 |

## Artifacts

The artifacts live on `nix` at `~/build/dios-liveness-pricing-20261125T103523Z`:

- Variant trees: `v0` to `v3`, with `vN.diff` and `vN.build.log` beside them.
- Runners: `runner-vN`.
- Fixture: `input/`.
- Per-process JSON and logs: `out/<arm>/`.
- perf CSV: `perf/`.
- Host snapshots: `host/`.
- Scripts: `build.sh`, `run.sh` (rounds 1 to 8 and perf seeds 101 and 102), `run2.sh` (rounds 9 to 16), `run3.sh` (rounds 17 to 26) and `perf2.sh` (perf seeds 103 and 104).

The earlier attempt's directory `~/build/dios-liveness-pricing-20261124T161059Z` holds four source copies with no build outputs and no record of their edits. Nothing in it was reused, and it was left in place.
