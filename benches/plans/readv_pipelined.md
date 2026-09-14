# Bench plan: pipelined unregistered READ versus READV

Written before implementation, 2026-09-14, following the owner's follow-up
steering. Extends the [serial mechanism probe](readv_mechanism.md) with bounded
group depth. This is benchmark-only: product sources, arena-modernization,
plan-prefetch, the span slab and the open owner decision remain unchanged.

| Field | Value |
|---|---|
| Metric & direction | Candidate/base elapsed time, lower is better; absolute decimal GB/s, elapsed and thread CPU ns per useful 4 KiB, user/system CPU, exact SQEs/CQEs/bytes, submit/poll calls, completed groups, maximum unreaped requests/groups and refill-while-other-groups-pending counts |
| Workload | Three sequential passes over the existing immutable 256 MiB fixture, 768 MiB consumed with the common full-page u64 fold. Groups contain 32 consecutive 4 KiB file pages. Refill completed group slots at depths D=8 and D=16, with 1 MiB and 2 MiB maximum outstanding useful bytes respectively. Worker CPU 0, controller CPU 4; nix Threadripper 3970X/Samsung 970 PRO, Linux 6.6.64/ext4, performance governor, THP never, no competing storage campaign |
| Baseline | At each depth: (a) 32 ordinary 4 KiB READ SQEs into scattered pages; (b) one READV with 32 scattered 4 KiB iovecs; (c) one ordinary 128 KiB READ into a contiguous buffer; (d) one READV with 32 adjacent 4 KiB iovecs inside that contiguous buffer. Measure b/c, b/a, c/a and d/c as separate interleaved comparisons |
| Reps | Two qualification pairs plus 30 alternating fresh-process pairs per comparison, eight comparisons total. Identical fixture, useful work, depth, slot storage, ring capacity, fixed-file registration and Unregistered/direct buffer posture within each pair |
| Threshold | The overlap-parity claim requires the shared one-sided 95% CI upper of b/c <= 1.05 at D=8. D=16 b/c and both depths' b/a, c/a and d/c are characterization, with no adoption threshold. Original serial measurements remain separate evidence |
| Compare command | Retained shared `compare DEPTH8_VECTORED_OVER_CONTIGUOUS/paired.csv 1.05`, or `mise run gate DEPTH8_VECTORED_OVER_CONTIGUOUS/paired.csv 1.05`; all reported ratios use the shared `summarize` implementation |
| Escalation lever | Reject a sample for wrong bytes/checksum/page identities, completion-token reuse, non-direct posture, timed allocation, slot reuse before completion/consumption, undrained work, missing depth witnesses or host contamination. Retain a failed parity gate and profile both exact D=8 b/c arms, including a differential and replay/primary CPU ratios. Preserve loss/unwind limitations; do not relax the gate or implement a product mechanism |

## Workload contract

Each group slot owns a separately allocated, prepopulated 384 KiB aligned arena
and fixed 32-entry iovec arrays. All four arms allocate/touch the same storage:
D slots, 3 MiB at D=8 and 6 MiB at D=16. The contiguous region occupies the
first 128 KiB; scattered pages have 4 KiB gaps in the remaining region. The
adjacent-iovec arm points at those same first 32 pages as contiguous READ.
No physical-page contiguity is assumed. Nothing grows after timing begins.

Use the same ordinary ring flags and fixed-file registration as the serial
probe. Ring capacity is twice the maximum small-READ count at the chosen depth:
512 entries at D=8 and 1,024 at D=16, identical across arms. Depth is validated
and bounded at initialization; depth 1 remains available for smoke checks.
These are submitted groups and unreaped requests, not measured device depth.

Initially stage D groups. Poll currently available completions as a batch,
validate each token's group generation, slot and subrequest, and finish a group
only after all of its CQEs arrive. Consume its 32 pages in file order, then
stage the next sequential group in that freed slot. Flush newly staged groups
without waiting for the remaining active groups to complete. Completed groups
may retire out of order; the common wrapping-sum fold is order-independent.
This interpretation permits refill after any completed group with exactly D
buffer slots, without an unbounded reorder buffer or overwriting unread data.

Track the highest staged/unreaped request and group counts, batch progress,
and replacement admissions while another group remains pending. These cheap
witnesses prove concurrent admission and the absence of a whole-depth drain
barrier; they are not time-weighted occupancy or physical queue depth. Record
the complete distributions of observed groups at polling boundaries if useful,
labelled as poll-weighted. Do not substitute configured D into Little's law.

The finite 6,144-group schedule bounds all normal work. Consecutive no-progress
polls have the existing fixed POLLS_MAX limit, reset only on a reaped CQE.
Any error stops new admission; already staged/accepted requests drain before
storage destruction. If bounded draining fails, report and terminate the
dedicated process instead of reclaiming kernel-visible buffers.

## Measurement boundary and attribution

Preserve file-local cold preparation, absent target PTEs, direct-I/O alignment
witnesses, immutable fixture/source/executable hashes and terminal ring drains.
Setup, buffer population, registration, fixture preparation and result output
stay outside timing. Timing includes initial fill, SQE construction, all
submission/completion handling, refill, page consumption and final drain.
Primary samples use cheap counters only, with no per-request clocks or traces.

Record `read_ahead_kb`, `max_sectors_kb`, `max_hw_sectors_kb`, `max_segments`,
boot identity, host activity and governor before and after every campaign.
Use task-owned tmpfs for measurement artifacts, then archive each completed
campaign/profile and the exact executable durably before beginning the next.
Verify the archive hashes. Keep prior serial and scan evidence immutable.

On a failed D=8 parity gate, replay both arms with the retained executable,
eight repetitions, cycles at period 1,000,000, 32 KiB DWARF stacks, recorder
on CPU 4 and workers on CPU 0. Preserve raw perf, normalized stacks, lost and
throttle counts, recognized timed samples, excluded setup and orphaned stacks.
The existing 1% orphan/timed-sample qualification still controls whether CPU
category budgets can be reported. Missing callers do not become setup.

Report each arm's absolute GB/s beside the prior 3.35 GB/s large-transfer fio
reference and 3.37 GB/s coarse pressure scan. These are references from other
loops, not established device ceilings or equal-work gate baselines. D=8's
1 MiB ceiling is below the scan's observed approximately 1.68 MiB; D=16's
2 MiB ceiling is above it. Neither configured limit proves achieved overlap.

## Interpretation and verification

A passing D=8 b/c gate establishes <=5% parity under this pipelined workload.
A failure leaves measurable residual cost at that depth. Neither identifies
the serial 1.118 ratio as pure submission or page-pinning cost: it included
submission, completion, consumption and waiting. Adjacent versus scattered
iovecs tests a layout dimension while keeping iovec count constant; adjacent
READV versus contiguous READ tests their combined API/kernel costs, not a
uniquely identified kernel coefficient. One READV is not necessarily one bio.

Before measurement, add failing contract checks for the depth/adjacent arm and
completion-state reuse rules, then make them pass. Validate raw geometry,
depth, exact counters and paired CSV provenance. Run the Linux Rust contract
tests, Python validation tests, strict Clippy and formatting. Verify all product
source hashes against the retained snapshot and reassert retained product gates;
this benchmark-only change does not justify fresh product performance claims.

Whatever the result, the [owner decision](scan_geometry.md#open-owner-decision-coalescing-mechanism)
stays open until Sören records it. This experiment selects no allocator or
completion-fanout design.

## Pointer-provenance qualification repair

Before finalizing v1 evidence, a bounded Miri model rejected the iovec
construction pattern: repeated mutable vector indexing invalidated previously
derived pointers. Constructor/address checks and ASAN had not exercised that
Rust aliasing constraint. The earlier serial probe used the same pattern.
Retain both existing datasets, but treat them as unqualified for adoption.

Add a test that writes through every actual stored destination pointer and
then consumes/reuses the buffers. Prove it fails under Miri before changing
construction. Derive all destinations from one allocation-wide base pointer,
using bounded offsets without repeated mutable slice borrows. Rerun Miri,
Linux contract tests, strict Clippy/formatting and the ASAN smoke matrix.
The [Vec pointer contract](https://doc.rust-lang.org/std/vec/struct.Vec.html#method.as_mut_ptr)
documents the distinction between raw-pointer access and materializing slice
references; successful address checks alone do not prove pointer validity.

Collect a fresh v2 campaign with unchanged depth-8/depth-16 comparisons and
gates. Also collect two qualification plus 30 paired serial comparisons for
b/c and b/a at depth 1 with the corrected executable, using the serial plan's
existing b/c <= 1.05 gate and its failure-profile lever. The two refreshed
serial comparisons are added for validity, not to select a more favorable
baseline. Keep all executable/source identities and qualification failures.

## Submit/ready clock diagnostic

Written before implementation following `dios-steering-readv-why.md`, after
the corrected depth-8/depth-16 pairs completed. Preserve those primary samples
and their executable. Add a separate serial diagnostic command; the primary
`Probe::run` loop receives no per-group clocks.

| Field | Value |
|---|---|
| Metric & direction | Wall ns per 128 KiB group inside the submitting `ring.submit()` call, and from its return through observed/validated completion; report the remaining elapsed time separately. Lower is better. These are host intervals, not pure kernel CPU or device service time |
| Workload | Corrected b/c buffers, depth 1, same 6,144 groups/768 MiB/full-page fold, fixed-file/direct/Unregistered posture and pinned nix host protocol above. Preallocate exactly 6,144 timestamp records before timing, three monotonic timestamps per group, no per-poll clocks or allocation |
| Baseline | Instrumented scattered READV versus instrumented contiguous READ; separately, instrumented versus ordinary serial execution for each method using the same new executable |
| Reps | Two qualification plus 30 alternating fresh-process pairs for each of the three comparisons; retain every per-group interval and raw row |
| Threshold | Observer qualification requires the shared one-sided 95% upper of instrumented/ordinary elapsed and thread CPU <= 1.05 for each method. Phase ratios are characterization; no new product or mechanism adoption gate |
| Compare command | `compare OBSERVER/paired.csv 1.05` and `compare OBSERVER/cpu/paired.csv 1.05`; phase comparisons use the existing shared `summarize` implementation |
| Escalation lever | Reject invalid phase totals, wrong group order/count, nonempty submission at wait entry, timed allocations or ordinary probe contract failures. Retain observer gate failures and label the split perturbing; do not subtract estimated overhead or replace primary results. No host security changes or additional product implementation |

The submitting interval brackets the call with a known queued SQE. The wait
interval includes busy polling, completion checks and possible scheduling;
it starts after submission returns, so it excludes any I/O progress already
overlapping that call. It is not an off-CPU wait measurement. The diagnostic
reuses the existing poll/completion and refill/consume routines; its initial
submission is separate from the first completion poll. Observer pairs measure
that control-flow and clock overhead together. Validate exact accepted counts.

A larger submit interval and similar post-submit wait support localization
to the submission path. They cannot identify per-iovec pinning, DMA mapping,
PRP/SGL setup or iovec import individually. A failure to hide the delta under
overlap would not, by itself, prove device-side cost. Virtual gaps do not prove
physical segment counts, and IOMMU mappings can batch/coalesce scatterlists.
Keep these kernel mechanisms as source-supported hypotheses until measured.

## Owner budget probe: depths 2 and 4 (2026-09-14)

Written before benchmark changes or measurement, following
`/private/tmp/dios-steering-scope-feedback.md`. The owner has selected
Unregistered READV coalescing; the default speculative byte budget remains
an owner decision. This addendum supplies evidence for that decision and
does not authorize product code or select the budget.

| Field | Value |
|---|---|
| Metric & direction | Absolute elapsed/thread CPU ns per useful 4 KiB and decimal GB/s; lower time is better. Paired b/c elapsed ratio, SQEs/CQEs, exact consumed bytes/checksum, groups pending and refill-with-overlap witnesses |
| Workload | Existing immutable 256 MiB fixture, three passes, 6,144 groups of 32 consecutive 4 KiB pages, common full-page fold. D=2 permits 256 KiB pending useful bytes; D=4 permits 512 KiB. Identical pinned nix host, CPU 0 worker/CPU 4 controller, direct Unregistered I/O, fixed-file registration, file-local cold preparation and bounded refill loop as corrected depth-8 v2 |
| Baseline | (c) one contiguous 128 KiB READ; candidate (b) one READV over 32 scattered 4 KiB destinations. Both use D prepopulated 384 KiB arenas: 768 KiB at D=2, 1.5 MiB at D=4; ring entries D*64 (128/256). Only these two arms are requested |
| Reps | Two qualification pairs followed by 30 alternating fresh-process pairs at each depth; retain all rows, including qualification samples |
| Threshold | Characterization, no new adoption threshold. Every accepted row must satisfy the existing exact-work, zero-allocation, cold-state, achieved-admission and terminal-drain contract. Report shared one-sided 95% upper b/c beside its existing 1.05 parity reference, without turning a failing reference into a product rejection or relaxing RC-G1 |
| Compare command | `mise run gate PRIMARY/depth-D-vectored-over-contiguous/paired.csv 1.05` for D=2/4; shared `summarize` produces all ratios. The characterization records either outcome |
| Escalation lever | Reject contract/provenance/host failures and preserve artifacts. If a run fails or the host is busy, repair or wait and repeat the affected complete comparison. A timing gap is evidence for the owner's choice, not permission to change the budget, instrument the primary loop, or change product code |

Permit depths 2 and 4 in the existing bounded depth catalog and add a
collector selection for the existing b/c comparison. Check rejection of
unsupported depths and run both new depths through the existing contract
checks before measurement. The primary submission/poll/refill/consume loop,
buffer construction and maximum depth (16) stay identical. Record source and
executable hashes; earlier D=1/8/16 results come from the corrected v2 binary,
so label them historical references rather than contemporaneous pairs.

Use a task-owned remote checkout and tmpfs output; verify no competing
storage campaign, retain host snapshots, and archive exact source, executable,
raw rows and shared comparison output durably before another campaign.
Run Linux benchmark contract tests, Python validation, strict Clippy and
formatting; compare product-source hashes before/after. Reassert retained
regression gates; there is no new product performance measurement here.

Report D=1/2/4/8/16 b/c absolute rows together, with pending useful-byte
ceilings explicitly distinguished from observed/device queue depth. Include
the historical mmap sequential reference (1,552.23 ns per useful 4 KiB),
which is a separate pool workload, not a paired denominator for this probe.
The scope's candidate default is 512 KiB–1 MiB at 4 KiB; Sören records the
choice after reviewing this table. The depth-2 row also shows the smaller
256 KiB overlap point; it does not silently expand that candidate range.
