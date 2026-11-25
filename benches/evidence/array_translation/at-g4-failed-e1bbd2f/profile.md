# AT-G4 lever evidence: profile of `get` on the gated lane

Host `nix`, 2026-11-17, bench profile active, CPU 0. Executables retained by the capture
`target/array-translation/at-g4` on `nix` (`~/build/dios`): candidate
`mmap_workloads-1711bb63…fffac7` (0a9728a), base `base-build/mmap_workloads-3516a8f8…a2cf3`
(`main` at ade5310). Lane `resident_access_ordinary`, arm `dios_serial`.

## Timing (collector summary over the 30 measured AB/BA pairs, rejected set)

`ratio.json`: geomean 0.9073, CI95 upper 0.9231, bound 0.85. About 47 ns per `get` hit on
the base and 43 ns on the candidate (65,536 operations in about 3.1 ms and 2.8 ms).

## Flat profile (`perf record -F 9973`, `profile` mode, 128 repetitions, whole process)

Share of all samples; the timed loop is about 5% of the process.

| Symbol | Base | Candidate |
|--------|-----:|----------:|
| `PageTable::lookup` / `Translation::lookup` | 2.20 | 0.87 |
| `Pool<D>::get` | 1.38 | 0.89 |
| `Pool<D>::pin_owned` / `Pool<D>::pin_live` | 0.94 | 1.31 |
| `ReaderSlot::commit_pin` | no separate symbol | 0.33 |
| `ReaderSlot::begin_pin` | no separate symbol | 0.09 |
| `Frames::frame_bytes` | no separate symbol | 0.05 |
| `resident::timed_workload` | 0.90 | 1.34 |
| `resident::acquire` | 0.71 | 1.09 |
| `resident::resident_guard` | 0.17 | 0.24 |
| `FrameGuard::drop` | 0.11 | 0.10 |

## Candidate `pin_live` call sites (`perf annotate`, GOT slots resolved with `readelf -r` and `nm`)

`Pool<D>::pin_live` is instantiated in the bench crate and reaches six non-generic
functions of the `dios` crate through indirect calls, each carrying 6% to 9% of the
samples of `pin_live`:

- `dios::pool::epoch::ReaderSlot::begin_pin`
- `dios::pool::translation::Translation::lookup` (its samples sit on the prologue and the bound comparison)
- `dios::pool::file_generation_is_live`
- `dios::pool::clock::Clock::checked_index`
- `dios::pool::epoch::ReaderSlot::commit_pin`
- `dios::pool::frames::Frames::frame_bytes`

`timed_workload` reaches `FrameGuard::drop` the same way. `[profile.profiling]` inherits
`release` without LTO, so a non-generic function of `dios` is inlined into the bench
crate only when it is small enough for automatic cross-crate inlining or carries
`#[inline]`.
