# AT-G1 lever evidence: profile of the session read on the gated lane

Host `nix`, 2026-11-17, bench profile active, CPU 0, THP `never`. Executable of the
layout-16 capture at `3e373a9` (`mmap_workloads-36a24a66…`), lane
`resident_access_session_projection`, 134,217,728 reads per process.

## Gate

| Build | Geomean | CI95 upper | Session ns/read | mmap ns/read | Difference ns (95% interval) |
|-------|--------:|-----------:|----------------:|-------------:|------------------------------|
| layout 16 | 1.9131 | 1.9359 | 7.06 | 3.69 | 3.37 (3.29 to 3.46) |
| layout 32 | 1.9666 | 1.9850 | 7.27 | 3.70 | 3.58 (3.51 to 3.65) |
| layout 64 | 1.9107 | 1.9234 | 7.05 | 3.69 | 3.36 (3.31 to 3.40) |

Bound 1.50. The floor arm reads 3.83 to 3.84 ns.

## Flat profile (`perf record -F 19997`, one `sample` process per arm)

| Symbol | Session arm | mmap arm |
|--------|------------:|---------:|
| `catalog::consume` (shared fold) | 57.2% (about 4.0 ns) | 54.6% (about 2.0 ns) |
| loop function | 33.7% (about 2.4 ns) | 41.3% (about 1.5 ns) |
| `Pool::session` | 2.1% | |
| `ReadSession::file` | 2.3% | |

`Pool::session` and `ReadSession::file` are out-of-line calls, once per 16 reads. Inside
`dios_session_loop`, by inlined function: loop body 26.7%, `black_box` 10.3%,
`SessionArena::frame_bytes` 9.2%, `SessionFile::resolve` 8.9%, atomic loads 7.4%,
`Option::as_ref` 5.2%, `Clock::checked_index` 5.1%, `Option::map_or` 4.4%,
`release_guard` 4.0%, pointer add 3.6%, slice `get` 3.2%, `reference_observed` 2.3%.
No single instruction holds more than 5%. The locked instructions of the CLOCK
first-reference path (`lock cmpxchg`, `lock incq`) carry no samples: the reference bit
is already set in the steady state.

## Scratch experiment (not gate evidence)

Throwaway worktree of `3e373a9` on `nix`, default build, floor arm `plain_array`, three
processes each:

| Floor arm variant | ns/read |
|-------------------|--------:|
| unchanged | 3.845, 3.829, 3.820 |
| plus one table load the payload address depends on (`table[page] - 1`) | 3.847, 3.823, 3.818 |
| plus a liveness word compare and a per-frame reference byte check | 3.850, 3.847, 3.843 |

One dependent translation load, a liveness compare and a reference check cost at most
0.02 ns per read in this harness when written directly. The 3.4 ns of the session read
are therefore not the price of the indirection or of its three per-read checks.
