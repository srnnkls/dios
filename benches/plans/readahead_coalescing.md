# Bench plan: readahead coalescing

Revision 3, written before product implementation, 2026-09-14. Ordinary
READV is selected; [scope](../../scopes/active/readahead-coalescing/scope.md)
and [design](../../scopes/active/readahead-coalescing/design.md) passed their
[Astra/Claude review](../../scopes/active/readahead-coalescing/review.yaml).
The owner recorded the 512 KiB default budget and scope approval on
2026-09-14 in [validation.yaml](../../scopes/active/readahead-coalescing/validation.yaml).
This supersedes
`readv_integration.md`. The [RC6 results](../evidence/readahead_coalescing/results.md)
record measured passes and remaining blockers. Numeric bounds remain unchanged;
the owner clarified RC-G6 reporting and authorized the confirmation below.

| Field | Value |
|---|---|
| Metric & direction | Candidate/base whole-workload elapsed ratio, lower is better; CPU ns per useful 4 KiB, actual SQEs/CQEs, vector lengths, useful/read bytes, in-flight frames/bytes, speculative occupancy and terminal outcomes |
| Workload | Existing immutable 256 MiB + spare-page fixture and full-page u64 fold. Cold 64 MiB scan with 8 MiB payload arena for every pool arm. Three sequential 256 MiB passes with 64 MiB payload arena inside private memory.max=128 MiB, MemorySwapMax=0. One consumer, 4 KiB pages. Owner-selected budget 512 KiB: C=128 speculative pages, scan read limit R=256, miss headroom 768. Preserve fragmented/dependent/wrong-hint and frozen DRP work from prefetch_admission.md |
| Host protocol | Pinned nix, Threadripper 3970X / Samsung 970 PRO / Linux 6.6.64/ext4; CPU 0 worker, CPU 4 controller; prescribed DRP placement for those gates. Performance governor, THP never, no competing campaign, direct/Unregistered read buffers. Snapshot boot, governor, activity, read_ahead_kb, max_sectors_kb and max_segments before/after |
| Baseline | (1) mmap MADV_SEQUENTIAL; (2a) frozen current per-page mechanism at old default C=32/R=64; (2b) that same mechanism at new C/R, matched to the coalesced default; (3) unchanged frozen DRP runner/baselines; (4) disabled-demand candidate control for non-sequential lanes. Pool scan comparisons share payload arena, work and poll/consume loop; 2a intentionally retains old C/R. Freeze measured working-tree sources/executable before any product edit |
| Reps | Two qualification pairs, then 30 alternating fresh-process pairs for each comparison; identical useful work/seed inside each pair. Wrong-hint witnesses hold in every one of 30 fresh-process repetitions on eager and Linux. Frozen DRP retains its original aggregation/sample contract |
| Threshold | Six owner requirements below; one-sided 95% upper of candidate/base. Pin the owner-selected budget, derived read/watermark limits and arena before implementation; no later budget, granule or comparator change to obtain a pass |
| Compare command | Shared `mise run gate CASE/paired.csv BOUND` with table bounds; shared `summarize` for characterization. Validate raw identities/checksum and exact CSV reconstruction. Observer controls use `mise run gate OBSERVER/paired.csv 1.05` and `OBSERVER/cpu/paired.csv 1.05` |
| Escalation lever | Reject safety/path/byte/allocation/drain/host failures. If RC-G1 fails, profile both exact failing arms at the pool layer, retaining CPU demand, achieved overlap and observer qualification. Inspect run admission, reconciliation and completion before more kernel attribution. Keep failures and bounds; no relaxation or budget increase without owner decision |

## Gate table

| Gate | Candidate / base or witness | Pass requirement |
|---|---|---|
| RC-G1 | Default automatic coalesced 4 KiB scan / mmap SEQUENTIAL; cold and private 128 MiB-capped three-pass shapes separately | Elapsed-ratio upper <= **1.00** in each |
| RC-G2 | Coalesced default / frozen per-page mechanism, both (a) old default C=32/R=64 and (b) matched new C/R; same shapes, arena, work and poll/consume loop | Elapsed-ratio upper <= **0.80** for both comparisons in each shape; preserve original improvement requirement and prove it at equal new budgets |
| RC-G3 | Frozen DRP-G2 warm/cycling and DRP-G4 ordinary/scaling | Original **1.01 / 1.01 / 1.00 / 0.50** bounds |
| RC-G4 | Cold fragmented/dependent: coalesced default / disabled demand | Elapsed-ratio upper <= **1.02** in each; **zero automatic admissions** |
| RC-G5 | Wrong-hint demand-hot canary | **Zero protected evictions**, **zero demand-hot misses**, bounded credits, full recovery |
| RC-G6 | CPU and elapsed per useful 4 KiB for every arm; polls/page and native prefetch stats; mechanism fields and qualified prefetch-control CPU/page for every arm that can emit them | Mandatory reporting; coarse 256 KiB reference approximately **1,163 ns elapsed / 1,158 ns CPU** per useful 4 KiB; qualified attribution for CPU categories, unresolved CPU reported explicitly; owner accepts the retained cold-candidate category nulls for RC6 under the disposition below |

RC-G6 is not another CPU ratio bound or a physical resource ceiling. All
previously adopted prefetch gates retain their original bounds. Frozen DRP
keeps `max_inflight_reads(1)`, hence zero default speculative credits; it
does not inherit the new scan budget or arena.

## Budget and controls

The owner recorded the default of 512 KiB (524,288 bytes) on 2026-09-14:
C=128/R=256/headroom=768, 897 frames including one guard. The retained
alternative, 1 MiB with C=256/R=512/headroom=1536, would need 1,793 frames;
raising the default later is a separate owner decision with that evidence.
An 8 MiB cold payload arena fits both; use it for every new pool scan arm.
The previous cold evidence used 4 MiB; collect fresh baselines rather than
reuse those timings. Pressure stays at 64 MiB within its private 128 MiB cap.
Descriptor, route and notification metadata are additional and reported.

The [budget probe](../evidence/readv_pipelined/budget-depths/README.md) gives
READV 1,244.97 ns/page at depth 2 and 1,183.67 at depth 4, against historical
mmap 1,552.23; depth 8 is 1,164.53. Depth 4 is past the knee (1.6% from 1 MiB)
and leaves 368 ns/page of headroom for pool-layer cost, which motivated the
512 KiB selection. This calibrates overlap and proves no end-to-end pool
result. If RC-G1 fails at 512 KiB, the lever is profiling the pool layer, not
the budget. Prior 32-to-64 point-read credits improved cold scans 6.6%; that
motivates the matched-budget control but does not predict its cost at 128
credits.

The headline candidate must use the actual default, without an explicit
prefetch override. The frozen per-page matched control uses its existing
page-count override to reach the same C/R without coalescing. Comparison 2a
measures the complete change; 2b isolates it from budget/read-limit changes.
Both share independent 4 KiB pages, payload arena and ordinary guard use.
Coarse-granule measurements remain labelled references. No candidate-only
change to caller poll cadence is permitted.

## Workload and path witnesses

Count frames separately from SQEs: a k-page vector reserves k read credits
and k speculative credits but submits one SQE. Record initial/continuation
vector lengths, short bytes, CQEs, per-page publications/failures and credits
at drain. Assert checksums, exact page identities and read bytes, no live
destinations at teardown and zero timed allocations.

The default automatic full-page scan must reach a 32-page confirmed window within
its first 1,024 useful pages. After startup, select an interior 1,024-page
interval per pass that includes a refill cycle. Count read SQEs whose file
ranges intersect it, including demand READs as well as speculative READV.
With no holes or injected failures, require full
32-page initial vectors and <=33 SQEs (including boundary overlap). This
rejects an initial burst followed by point refills. Short/error continuations
are counted separately and tested through fault injection. One free credit
or one newly eligible horizon page cannot trigger automatic singleton refill;
ordinary demand stays independent. Explicit hints extend in vector-sized
chunks while repeating the useful examined prefix for protection.

Small-capacity compatibility uses current per-page automatic admission when
min(C,R-1) cannot cover two granule-adjusted full vectors. Check C=1/8/32,
R=33/C=32, and an explicit override C>R-1; no full-window refill barrier.
Check two ready streams sharing a constrained budget: a credit-deferred
stream retains its admission turn. The 128/256-credit default candidates
retain full-vector mode; transient shortages do not shrink their vectors.

Record outstanding requests/bytes, resident-unconsumed credits and refills
while other requests remain pending. In vector mode B >= 2; B vector credits target B-1 to B
speculative vectors; this is neither device depth nor a guarantee of overlap.
The existing poll/consume protocol remains part of the measured workload.

An empty poll with no completion/notification/invalidation must visit zero
speculative entries at capacities 32, 128 and 256. A completed/dirty run
visits its affected entries (at most 32), plus explicitly counted one-time
invalidation cleanup. Test consumption after the last CQE so credits cannot
be stranded waiting for another I/O completion. Include racing notification,
frame reuse and ordered-feedback checks.

Under explicit full-credit hint extension, record protection lookups and
replacement candidates visited. For examined prefix L and capacity C, require
at most L protected-page resolutions and C replacement-candidate visits per
call, with O(1) preallocated stamp checks per candidate; count admission/CLOCK
work separately. Include duplicates, multiple runs and protected newly
admitted pages so later replacement cannot evict an earlier useful hint.

Do not add extent snapshots. Measure reads beyond the consumer stop and
actual EOF/error outcomes. Every vector is <=128 KiB, but already accepted
vectors can also reach EOF before reset; report aggregate wasted calls/bytes
rather than assume one wasted request per pass. Extent tracking is a separate
owner-approved follow-up only if RC-G6 demonstrates a material cost.

Mixed-residency controls have resident/pending holes every 2/4/8 pages,
duplicates, file switches and partial-window consumption. Require no read
into holes, correct report partition and bounded progress under exhausted
reserve/miss/route/driver capacity. A short read with every other driver slot
occupied continues in the original reserved slot, without a fresh reservation.
Retirement with a held continuation permits the existing logical read's
bounded remainder chain on its retained descriptor; it blocks new logical
admissions and waits for terminal/reclamation conditions before close.
These are mechanism/safety checks, not additional timing adoption gates.

Exercise plain READV in both registration postures where existing memlock
permits a small registered arena. Unregistered remains the headline posture;
eager correctness/zero-allocation is required, macOS timings advisory. Change
no memlock or host security settings and claim no READV_FIXED on Linux 6.6.

## Measurement boundary and cost evidence

Exclude fixture creation, cache preparation, pool/slab allocation, registration
and output. Include admission, SQE construction, polling/completion, full-page
consumption and final drain. Detailed traces/profiles stay outside the workload
memory cgroup. Use task-owned tmpfs; archive each completed campaign/profile
and its exact executable durably, verifying hashes before the next campaign.

Record thread CPU independently of elapsed. Prior prefetch (~340 ns/page)
and poll/completion (~180 ns/page) category estimates motivate batching, not
fixed coefficients to subtract. Larger capacities make empty-poll scans a
specific risk: report polls/useful page, control entry visits and qualified
prefetch-control CPU/useful page alongside achieved overlap.

Keep request clocks out of primary samples. Detailed replays use the same
executable/work and bounded records, with paired trace/plain elapsed and CPU
upper <=1.05. Validate overflow, sampling loss/throttle and caller unwinding;
preserve unresolved CPU instead of inventing a complete category budget.
Profile and diff failing pool arms before another implementation change.
The 2.72 us serial-probe residual is noted and not pursued.

## Pre-implementation and adoption

Scope review, default-budget selection and owner scope approval precede
product work. Freeze source/executable baseline before first-batch dispatch;
RC1 uses that snapshot while independent RC2 edits. Capture bounded RED for
vector ownership, short/failed continuation, range/point join, EOF prefix
handling, file retirement, full queues and credit conservation before the
corresponding task. Run fault injection, Loom, stored-pointer Miri, syscall
ASAN, both-backend zero-allocation and existing lifetime/retention suites;
strict Clippy/formatting must pass. Fresh product gates follow safety checks.

RC1 prepares the exact six-comparison matrix, immutable frozen/candidate
identities, actual-default scan commands, vector-sized explicit extensions,
paired elapsed/CPU output and measured-witness rejection. The executable
[capture contract](../evidence/readahead_coalescing/README.md) names the
RC3/RC4 observation producer sites and exact collection/validation commands.
Until those producers exist, the runner emits a null coalescing observation
and the collector rejects it for RC gates. Retained regressions remain the
baseline; harness preparation claims no product gate pass.

## Owner-directed DRP diagnostic pairing, 2026-09-14

Sören's steering in `/private/tmp/dios-steering-drp-g4.md` requires a direct
`aa97c827fcf916fd3ecbbc71775dace0a27190a3` base versus
`5edb6a7f64bcd0137cbd75d03821780b38c679f2` candidate comparison before further
work on the failed DRP-G4 ordinary eight-thread gate. Use the immutable
`workload-frozen-812c812` runner/build script, release builds with the same
`bench` feature, original full-4-KiB fold, 32,768 iterations, fixture and
CPU set `0-3,32-35`; controller CPU 4 and the existing pinned-host protocol.
Run two qualification pairs and one campaign of 30 alternating fresh-process
pairs. Preserve clean product trees, executable/source/feature identities,
raw process rows, exact paired CSV and before/after host snapshots.

The diagnostic ratio is `5edb6a7 / aa97c82`, lower is better. Use the existing
shared `summarize` and `mise run gate paired.csv 1.00`; this comparison does
not replace the canonical frozen-base adoption gate or change its result.
The canonical DRP converter pins the older adoption baseline, so retain the
direct comparison separately and validate its exact raw contract/identities
before passing elapsed pairs to the shared comparison harness. An upper below
1.00 rejects the suspected branch regression; a ratio around 1.10–1.25
supports the owner-directed commit bisect. Report ambiguous evidence as such.

Report the direct result to Sören before further gate work. If the regression
is confirmed, compare the four branch commits to locate its onset, then test
source-backed shared-state/layout hypotheses. Preserve per-arm sorted elapsed
and mode counts/in-mode comparisons as descriptive evidence, alongside the
unchanged whole-sample gate. Do not rerun until pass, alter the iteration count
or CPU set, or increase the sample count to average away the suspected delta.
Exact-arm profiles and cache-miss/context-switch counts follow the direct
result under the frozen G4 diagnostic lever. Any later protocol amendment is
a separate owner decision.

## Owner dispositions after RC6 review, 2026-09-14

[Sören’s recorded dispositions](../evidence/readahead_coalescing/owner-dispositions-20260914.md)
authorize one [pre-registered confirmation](../evidence/readahead_coalescing/drp-confirmation-registration.json)
for `drp_g4_ordinary_base_8t`: 2 qualification plus 400 measured pairs, the
same frozen executables/work/placement/protocol and **1.00** bound. Adopt
that result either way, retain the failed 30 pairs separately, and do not pool
or make a second attempt. The earlier no-large-campaign instruction above is
withdrawn because the direct pairing dismissed the suspected regression.
The frozen protocol’s “at least 30” contract is unchanged. A future DRP
resolution change is recorded separately in
[dios_r1_r7_read_performance.md](dios_r1_r7_read_performance.md#owner-follow-up-drp-g4-resolution).

RC-G6 mechanism reporting now applies to **every arm that can emit the fields**.
The frozen executables predate the observation seam; deriving unavailable
observations from configured geometry or prefetch totals remains forbidden.
Frozen arms report CPU/elapsed per useful 4 KiB, polls/page and their native
prefetch stats. Their actual SQE/CQE/range counts, EOF/consumer-stop waste
and control-entry visits remain null with the limitation next to them.
The candidate reports every RC-G6 field with qualified attribution; unresolved
CPU remains explicit. This is an owner reporting clarification, not a numeric
bound relaxation: RC-G6 has no numeric adoption bound. Optional frozen
`strace -f -e trace=io_uring_enter` replay may supply exact submitted-SQE totals
from syscall returns; its timing is discarded. Skip if unavailable; no further
tracing or host-security change is authorized.

## Owner adoption: cold CPU attribution limitation

On 2026-09-14, the [owner accepted the candidate cold CPU nulls](../evidence/readahead_coalescing/owner-cold-attribution-disposition-20260914.md)
for RC6 closure, superseding the preceding requirement only for this retained
cold attribution record. The exact executable's CPU-clock capture has 249 timed
and 366 orphaned samples; caller unwinding is incomplete. The plan's 1% orphan
qualification rule continues to withhold category budgets. Retain the measured
cold totals, 1,472.5 ns elapsed / 1,464 ns CPU per useful 4 KiB, and the qualified
candidate pressure attribution. Every numeric gate and the 512 KiB default remain
unchanged; RC-G6 has no numeric adoption bound.

Carry qualified cold attribution into **readahead-efficiency** as a mandatory
pre-implementation deliverable. Its bench plan must specify a frame-pointer
diagnostic replay with a separate perturbed executable and its own source/build/
executable identity. Preserve the RC6 primary and failed profiles separately;
the perturbed replay cannot be presented as the unchanged primary executable.
More repetitions cannot repair structural caller-unwinding loss. No new RC6
capture or timing campaign is authorized or required by this disposition.
