# File-indexed retirement

Recorded before implementation, 2026-09-10. Baseline: dios
69d46c281c8548c5e66da53858975a13ba4275e4, the exact dependency of the SIRA
close-cache candidate. All prior SIRA source, corpus, durability and budgets
remain fixed. Goal: eliminate repeated whole-pool frame and miss scans while
preserving every retirement, pending-interest, completion, lease and EBR rule.

## Alternatives and choice

The baseline does up to two whole frame-array scans, one whole miss-array
scan and one product-array scan for every retiring file on each progress
pass. SIRA retires individual files as their last reader drops. A batched
progress scan would reduce work per poll across many retiring files, but
would still scan the whole pool for each separately initiated retirement
unless SIRA also changes admission/batching. A new retire_all wrapper over
the current loop does not address this cost.

Start with per-file membership for mapped frames and occupied miss slots.
Indexed iteration visits only that file's entries; an empty frame list
proves no mapped Resident/Evicting/HELD frames remain. Pending reads remain
represented by miss entries, including terminal generations with interests.
Keep exact generational identity filtering and the existing bounded product
scan. Do not add counters to reader/pending-token drop paths or change EBR.

Memory sketch: replace each existing Occupiable<T> flag/padding with two
u32 links, retaining MaybeUninit<T>. Zero links denote vacancy; occupied
head/tail links point to their own slot. This needs no spare u32 index value:
encoded index is index+1, valid through the largest representable capacity.
For PageId and MissEntry the two links should fit the existing eight-byte
flag/padding footprint. Assert both concrete size bounds. Add two fixed u32
head tables indexed by configured file capacity, approximately 8.3 KiB for
1,057 file slots, with no growing collection. If the size proof fails,
reconsider the batched alternative before adding frame-scaled memory.

## Host and protocol

Threadripper 3970X, ssh nix, kernel 6.6.64, performance governor, THP never,
CPU 2. No concurrent builds/tests/profilers during timing. Record source,
compiler/features, binary and evidence hashes. Interleave exact baseline
and candidate binaries and reverse order on alternate reps. Keep every
observation, including failures/outliers. macOS measurements are advisory.

| Field | Value |
|---|---|
| Metric and direction | Candidate/baseline retirement wall ratio, lower is better |
| Workload | Pre-populated mock pool, capacities 16,384 and 262,144 frames, 1/16/136 files, 64 interleaved resident frames per file; retire each file and drive its normal progress to Retired; setup outside timing |
| Baseline | Unchanged 69d46c2, same harness and fixture |
| Reps | 30 alternating fresh-process pairs per case; fixture sufficiently large for timer resolution |
| Threshold | Shared bootstrap one-sided 95% upper candidate/base <= 0.25 for 262,144 frames and 136 files; <= 1.03 for remaining cases |
| Compare command | cargo bench --features bench --bench compare -- target/retirement-pairs/<case>.csv <threshold> |
| Escalation lever | Profile the exact failed arm; adjust indexing/traversal, or prototype batched progress with explicit SIRA batching; retain failed evidence and never relax threshold silently |

SIRA integration gates use the existing 1,000,000-row seed-3 corpus, 24-byte
keys, 150-byte values, 488 bounded scans and 256 puts per round, 64 rounds.
Materializing and point workloads each use 30 alternating process pairs;
true-range deletion has three additional regression pairs. Each process
receives a freshly copied and fully fsynced engine corpus and the identical
persisted-cache template. Every scan and final verification reopen must
match full bytes/cardinality. Verification reopen remains outside close.

| Integration metric | Gate |
|---|---|
| Complete materializing workload candidate/base | Shared CI95 upper <= 0.98 |
| Complete point workload candidate/base | Shared CI95 upper <= 1.00 |
| Measured close, materializing and point | Shared CI95 upper candidate/100,000,000 ns <= 1.00 over all 30 pairs |
| First scan and average warm scan per run, both workloads | Shared CI95 upper candidate/base <= 1.03 |
| Peak measured RSS | Every untraced candidate <= 650 MiB; pool 984,027,136 bytes and block cache 192 MiB unchanged |
| Durability and cache identity | Existing crash/reopen, cache and sync-overlap tests pass; no protocol-step removal |

Use the existing dios compare harness for every statistical gate, never
custom statistics. For the absolute close gate, record base_ns as the fixed
100,000,000 ns budget and candidate_ns as the observed measured close;
retain the raw timings and mapping. Escalation for failed integration gates:
profile only the measured close or failing scan arm, inspect fixed-index
memory/touch costs, and revise the candidate. A failed gate leaves the change
unqualified; the 100 ms and 650 MiB thresholds are not silently relaxed.

## Correctness before performance

First write a deterministic failing work-count check on retirement, then
implement indexed traversal. Exercise list membership under noncontiguous
insertion, head/middle/tail removal, slot reuse and multiple file generations,
including a reference-model sequence. Preserve all existing retirement,
retention, invalidation, partial/error read, product completion and guard
checks. Run the full mock-enabled dios suite, shipping-backend zero-allocation
checks on macOS and Linux, existing Loom schedules, strict Clippy and rustfmt.
Run new pure-memory index tests under Miri and the relevant syscall paths
under the available address-sanitizer setup. Record any unavailable tool
rather than claiming its gate passed. Then run SIRA's existing relevant
retirement, cache identity, sync-overlap and routed reopen regressions using
the local dependency override. No dependency publication is implied.

## Escalation after the first indexed candidate

The complete 30-pair matrix passes all six micro gates, both complete-workload
gates and all first/warm-scan gates, but fails the two absolute 100 ms close
gates (CI95 upper 125.12 and 126.04 ms). One materializing run reaches
650.01171875 MiB, exceeding the unchanged RSS cap by 12 KiB. Preserve this
candidate, every observation, and all gate failures. Close-only profiles show
the repeated retirement scans are gone; remaining user CPU is mainly cache
encoding, with synchronization and memory-release syscalls contributing wall
time. No cache or durability protocol change is part of this Dios experiment.

Revise only the reverse frame index to remove its repeated pool driver id.
Retain exact file slot, generation and granule index (three u32 fields), plus
the two membership links: 20 bytes versus 32 bytes per mapped frame record.
The owning container stores the driver's identity once and reconstructs the
unchanged full PageId. Reject foreign-driver insertion before any mutation.
Miss records retain their full PageId; the warm page table and lock-free
protocols remain unchanged. This saves 12 bytes per touched frame, in addition
to leaving existing pool/cache capacities fixed.

First add a failing <=20-byte frame-record assertion against the current
32-byte layout. Require exact round trips at maximal ids, generation reuse,
and foreign-driver rejection under unit tests and Miri. Repeat the existing
mock, allocation, concurrency, sanitizer and SIRA integration checks after
implementation. Freeze the first candidate's source/binaries separately.
Repeat the same six 30-pair micro cases and the same 30 materializing/point
pairs plus three range pairs against the unchanged original baseline. Every
original numeric gate remains in force, including 100 ms close and 650 MiB
RSS. Report any remaining failure as unqualified; no threshold is relaxed.

Further inspection found the existing ExactPageCells already retain every
frame's full PageId for hint validation. Use those authoritative cells under
the control lock; mapped Resident/Evicting membership proves initialization,
and the same lock excludes reuse. This preserves even the existing synthetic
completion test seam, which permits arbitrary hashable PageIds. The compact
index therefore needs only the file-slot key plus two links: 12 bytes per
frame, saving 20 bytes against the original 32-byte reverse map. No identity
reconstruction or new driver constraint is needed. Keep the <=20-byte RED
memory gate and additionally assert the concrete 12-byte layout. Miri must
exercise resident/evicting identity stability, exact driver and generation
filtering, and reuse after unlinking. All original numerical gates and the
full unchanged epoch fixtures still apply.
