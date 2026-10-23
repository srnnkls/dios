# Bench Plan: residency_map

| Field | Value |
|-------|-------|
| Metric & direction | Elapsed ratio candidate/base per lane, lower is better; absolute pool/mmap ratio of the warm 4 KiB fold as the parity witness |
| Workload | `mmap_workloads` lanes on the shipping `io_uring` backend, one reader pinned to CPU 0: `resident_projection` (65,536 64-byte gets over 1,024 resident pages), `resident_decode` (8,192 full-page gets over 4,096 resident pages), `hotspot_mixed` (90% hot hits, 10% cold misses, 16 outstanding), and the cold controls `cold_gather` and `automatic_scan`; plus `benches/mmap_warm_path.rs` (64 resident pages, 4 KiB fold, pool vs file mmap in one process) |
| Baseline | the retained `mmap_workloads` executable built at `5eba702` (wake elision, hash-table translation, out-of-line hit helpers) |
| Reps | 30 alternating fresh-process pairs per lane, seeds `pair % 29`; `mmap_warm_path` keeps its 40 × 256 in-process pairs |
| Threshold | one-sided 95% CI upper bound: `resident_projection` ≤ 0.70, `resident_decode` ≤ 0.95, `hotspot_mixed` ≤ 1.00, `cold_gather` ≤ 1.02, `automatic_scan` ≤ 1.02; `mmap_warm_path` pool/mmap ≤ 1.10 |
| Compare command | `cargo bench --features bench --bench compare -- <paired.csv> <bound>` |
| Escalation lever | Profile the failing lane's hit path (`perf record` on the retained executable) and attribute the remaining cross-crate calls, loads and stores; do not weaken the liveness re-check or the epoch publication |

## Workload contract

A warm hit used to hash the page with four dependent splitmix rounds, read a
40-byte seqlock cell of a `2 × frames` open-addressed table, and then call six
small non-generic helpers (`begin_pin`, `commit_pin`, `frame_bytes`, the
liveness test, the CLOCK index check, the guard drop) that the consumer's crate
could not inline: `Pool::get` is generic and monomorphizes in the caller, but
the helpers lived in the `dios` rlib without `#[inline]`, and neither `dios`
nor `sira` builds with LTO. The candidate translates the leading
`direct_map_file_bytes` (16 GiB) of every file slot through one `AtomicU32` per
granule, keeps the hash table for later granules, re-reads the slot's
liveness word after the load, and marks the helpers `#[inline]` while keeping
the locked miss path out of line.

Correctness witnesses precede timing: the residency map's unit tests, the full
`bench,mock` suite, the zero-allocation suite, and the loom suite with the
model routed through the same map (pages below 8 direct, the rest through the
seqlock overflow). Without the re-check, the loom model
`mirror_get_racing_retire_and_slot_reuse_never_admits_reused_bytes` fails.

## Measurement boundary and attribution

Fixture creation, cache preparation, pool construction and the warm fill are
outside the timed region exactly as in `mmap_workloads`. Both executables run
identical lanes, seeds and checksums; only the library differs.

## Notes

Local runs on the 4-vCPU KVM guest are advisory; the binding host is `nix`.
The direct map reserves `4 × granules` bytes of address space per registered
file slot (16 MiB per slot at the default span and 4 KiB granules) with
`MAP_NORESERVE` and `MADV_NOHUGEPAGE`, commits one page per touched
`1,024 × granule` file range, and returns a retired slot's pages.

## Results (local, advisory)

Retained executables: base `mw-wake` (`937b390b…f399`), candidate `mw-map`
(`aaff1ec5…950b`). Thirty alternating fresh-process pairs per lane.

| Lane | Ratio geomean | One-sided 95% upper | Bound | Result |
|---|---:|---:|---:|---|
| `resident_projection` | 0.7778 | 0.8438 | 0.70 | FAIL |
| `resident_decode` | 0.8584 | 0.9111 | 0.95 | PASS |
| `hotspot_mixed` | 0.9921 | 1.0931 | 1.00 | FAIL |
| `cold_gather` | 1.0485 | 1.1250 | 1.02 | FAIL |
| `automatic_scan` | 0.9045 | 0.9585 | 1.02 | PASS |

The escalation lever ran on the failing warm lane. In `resident_projection`
the timed loop splits about 72% in the lane's out-of-line `consume` and 28% in
`Pool::get`. 84% of `consume` is its first load, and 49% of `Pool::get` sits on
the instruction after the reader's `SeqCst` fence (`lock or`): the previous
iteration's data load is still outstanding when the next pin fences, so each
get exposes it instead of overlapping it as the mmap arm does. The direct map
removed the translation miss; what remains is the per-first-pin fence, which
`benches/plans/epoch_barrier.md` measured and rejected for its cold-path cost.
`hotspot_mixed` and `cold_gather` are dominated by device latency on this
guest: `cold_gather` pair ratios range 0.571–1.779 with per-sample elapsed
12–36 ms, and its candidate median is lower (19.091 against 20.123 ms).

Pre-registered confirmation (recorded before running): 100 alternating pairs
of `cold_gather` with the same executables, seeds and 1.02 bound, adopted
whichever way it goes and not pooled with the qualification sample.

Confirmation, 100 pairs: geomean **0.9719**, one-sided 95% upper **1.0323**
against 1.02 → **FAIL**, adopted under the rule above. The point estimate
favours the candidate, but this guest's device noise (paired log-ratio SD
about 0.18) cannot resolve a 2% non-inferiority margin at 100 pairs; the
cold lanes need the pinned host, or a CPU-time gate, to decide.
**Owner decision required** before these failed gates are waived.

Characterization (no bound), same executables and protocol, median elapsed per
65,536-projection sample:

| Lane | Base | Candidate |
|---|---:|---:|
| `resident_access_ordinary` | 5.167 ms | 4.232 ms |
| `resident_access_hinted` | 6.153 ms | 4.812 ms |
| `resident_access_epoch_batch` | 4.759 ms | 4.133 ms |
| `resident_access_retained` | 1.012 ms | 1.013 ms |
| `resident_access_page_batch` | 1.583 ms | 1.250 ms |

With direct translation the hinted path is slower than an ordinary `get`: its
stamp and identity validation now costs more than the single entry load it
was built to skip.

`mmap_warm_path` (pool over the mock backend against a file mmap, 64 pages,
4 KiB fold, 40 × 256 in-process pairs), three consecutive runs: geomean
1.1329 / 1.2325 / 1.2441, one-sided upper 1.2013 / 1.2939 / 1.3616 against the
pre-registered 1.10 → **FAIL**; before this change the same bench read
1.4957 / 1.4765 (upper 1.5873 / 1.5510). The last run's medians are 107.4 ns
per mmap fold and 128.3 ns per pool fold: about 21 ns of residency machinery
per protected 4 KiB access, down from about 41 ns.
