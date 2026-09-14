---
created: 2026-09-14
status: active
issue_type: Feature
revision: 3
---

# Scope: readahead-coalescing

Coalesce consecutive readahead misses into ordinary READV operations while
preserving independent cache-page ownership and ordinary `get`/`ready`
consumption. Sören selected this mechanism after the
[corrected driver measurements](../../../benches/evidence/readv_pipelined/README.md).
The [owner decision](../../../benches/plans/scan_geometry.md#owner-decision-coalescing-mechanism)
is closed. Revision 3 incorporates the owner feedback in
`/private/tmp/dios-steering-scope-feedback.md` and has passed a fresh
[Astra/Claude review](review.yaml). The owner recorded the 512 KiB default
byte budget and scope approval on 2026-09-14 in [validation.yaml](validation.yaml);
implementation proceeds under [tasks.yaml](tasks.yaml).

## Evidence and intended result

Scattered READV / contiguous READ elapsed upper is 1.1054 serial, 1.0047
at eight pending groups and 1.0045 at sixteen. Serial submission is 14.05
versus 9.34 us/group, with approximately equal post-submit completion wait.
The residual 2.72 us/group is noted probe-local work and is not pursued here.
Physical/DMA segment counts and individual kernel cost coefficients remain
unmeasured; the mechanism decision does not depend on asserting them.

The [depth-2/4 budget probe](../../../benches/evidence/readv_pipelined/budget-depths/README.md)
measures READV at 1,244.97 / 1,183.67 ns per useful 4 KiB with 256 / 512 KiB
pending-byte ceilings. The historical mmap reference is 1,552.23 ns; depth 8
READV is 1,164.53 ns. These separate workloads motivate a budget choice;
neither their difference nor configured depth proves achievable pool overhead.

The headline is a **default automatic, 4 KiB-page pool** beating or tying
mmap SEQUENTIAL on the existing cold and pressure scan shapes, while retaining
the warm, non-sequential, wrong-hint and safety contracts. Driver parity is
evidence for the mechanism, not proof that the pool meets that headline.

## Requirements

1. **Bounded vector destination.** A driver/backend read can own up to 32
   distinct frame destinations. The completion slab owns preallocated iovec
   storage that stays valid through completion, retry and teardown. Linux
   submits one ordinary READV for a multi-page run; eager fills the ordered
   vector on the poll caller; the mock transfers and schedules vector reads.
   Point reads retain their current per-frame operation. No hot allocation.
2. **One Linux coalesced path.** Ordinary READV is used for both Unregistered
   and Registered arenas on the existing Linux floor. Registering the arena
   does not make this opcode a fixed-buffer operation. READV_FIXED is deferred
   until a separately approved kernel-floor change to 6.15 or later. AM2's
   run allocator is deferred, revisited only for a Registered deployment below
   that floor with measured binding pinning cost. No allocator or AM2 product
   work belongs to this scope.
3. **Automatic range admission.** The predictor offers a bounded prefix of
   its existing `next`/`width` window, capped at 32 pages,
   `vector_bytes_max` (128 KiB) and its confirmed lookahead horizon. Automatic
   vector mode requires capacity for at least two full vectors after reserving
   a demand read; smaller configurations keep current per-page automatic
   admission. In vector mode, wait for enough free speculative credits and
   eligible pages for a full vector, or a smaller confirmed startup window.
   A one-page advance of a sliding horizon is not a new short-tail exception.
   One admission processes the run. Progress the predictor only over pages actually resolved as resident,
   pending, admitted or permanently rejected; a deferred suffix stays retryable.
   Preserve per-reader/incarnation isolation and the existing disable option.
   Preserve round-robin turns across ready streams: credit deferral does not
   consume the waiting stream's turn or repeatedly skip it at refill.
4. **Explicit ranges and mixed residency.** Scan only the existing bounded
   prefix of the caller's slice. Coalesce consecutive absent pages of one file
   in slice order. A resident/pending page, duplicate, file switch or index gap
   ends a missing run; preserve the existing report for each input occurrence.
   Do not reread a resident/pending hole or sort arbitrary plans. Preserve the
   full examined prefix's protection of useful speculation. The unexamined
   suffix is deferred and does not participate in replacement protection.
   Explicit singleton hints take the ordinary read path without waiting for
   future calls. Document on `Pool::prefetch` that sliding a window one page
   at a time can produce point reads; callers seeking coalescing extend it
   in vector-sized chunks while repeating the useful protected prefix.
   Mark protected membership once per call through existing page lookups and
   preallocated entry marks. Replacement scans speculative entries at most
   once per call, not once per deferred page; no nested slice `contains` scan.
5. **Per-frame credits.** Reserve k read credits and k speculative credits
   when admitting k speculative pages, regardless of SQE count. The existing
   read-frame ceiling and speculative lifecycle remain in force. The default
   ceiling is derived from an owner-selected byte budget, bounded by spare
   frames and `max_inflight_reads - 1`. Each read credit returns at its miss
   entry's terminal outcome. Speculative credits retain their existing promotion/unused/
   failure terminal rules. Short-read tails keep their credits; waiter drop
   releases neither storage nor unfinished-read credit.
6. **Span completion.** A preallocated route slab resolves one driver token
   to k miss entries. Dispatch consults it before existing point lookup.
   Publish complete pages exactly once. Preserve byte-exact progress inside
   an iovec, resubmit only the unpublished tail, and never expose partial
   pages. Every positive completion advances total filled bytes; `Ok(0)` is
   terminal EOF for the unpublished remainder. Bound continuation attempts
   and preserve the existing direct-I/O alignment refusal semantics. A short
   continuation reuses the same reserved driver slot after its CQE; slab
   exhaustion cannot refuse it. Release the slot only at the final outcome.
7. **Failure and lifetime ownership.** Admission reserves all needed metadata
   before commit. Refusal rolls back every unsubmitted token/credit. EOF,
   permanent error and misaligned continuation fail
   every unpublished page once while preserving an already published prefix.
   File retirement and teardown keep all kernel-visible destinations and
   iovecs alive until accepted logical reads finish, including their bounded
   continuations. Retirement stops new logical page admission; a held
   continuation may resume the same read on the retained descriptor. It does
   not abort solely because retirement began. No additional cancellation model.
8. **Granule and EOF compatibility.** At 4 KiB, a vector is at most 128 KiB.
   Other legal pool granules retain their semantics; reduce vector width to
   the byte bound and use point reads when fewer than two pages fit. Introduce
   no extent snapshot. A short read publishes complete prefix pages; `Ok(0)`
   fails the unpublished tail once and resets that predictor incarnation.
   Drain already accepted work. EOF waste is measured by RC-G6; extent
   tracking returns only as a separately approved change if that cost matters.
9. **Event-driven bookkeeping.** Reconcile completed/consumed runs and
   explicitly invalidated predictions, with O(1) idle-poll overhead and O(k)
   per changed run, k <= 32. Remove per-poll scans of speculative capacity
   from reconciliation, obsolete eviction and marker harvest. Consumption
   after I/O completion must still deliver feedback without a future CQE.
   Keep repeated warm hits unchanged; report polls and prefetch-control CPU
   per useful page and assert bounded entry visits in the mechanism checks.
   Explicit hint protection/classification and replacement together are
   O(examined prefix + speculative capacity) per call, apart from the existing
   bounded page-table/CLOCK operations; do not multiply those two lengths.

## Invariants and limits

- One `InFlightFrame` owner per writable destination; an aggregate owns k
  distinct tokens, not permission inferred from a first-frame index.
- Sum of unpublished admitted logical frames is at most
  `max_inflight_reads`; the existing watermark proof remains per frame.
- Every per-page interest and credit has exactly one terminal disposition.
- Published pages are removed from future write destinations immediately.
- Iovec memory and active slot metadata have independent borrow lifetimes;
  reusing another slot cannot invalidate a pending operation's raw pointers.
- All slabs, arrays, lookup scans, queue capacities and retry limits are
  fixed at construction. Admit/defer under exhaustion; never grow or spin.

The owner-selected default budget is 512 KiB (524,288 bytes): 128
speculative pages at 4 KiB. Depth 4 of the budget probe is past the knee
(1,183.67 ns per useful 4 KiB at 512 KiB versus 1,164.53 at 1 MiB, a 1.6%
difference), leaves 368 ns/page of headroom to mmap's 1,552.23 for
pool-layer cost, and halves the minimum pool tax and the speculative
pollution bound relative to 1 MiB. The 1 MiB row stays retained; raising the
default later is a separate owner decision with that evidence. If RC-G1
fails at 512 KiB, the lever remains profiling the pool layer, not the
budget. For general granule g,
`C = min(floor(budget_bytes / g), spare_frames, max_inflight_reads - 1)`
with saturating subtraction; an explicit `prefetch_headroom` keeps its
existing page-count semantics. Read limits remain caller-configured.

The gate runner scales its read-frame limit to R=2*C and miss headroom to
3*R, preserving INV-9. With one guard and no retention, it needs at least
1+3*R+C frames: 897 frames at the selected 512 KiB (C=128, R=256, miss
headroom 768), against 1,793 at the retained 1 MiB alternative. Use an 8 MiB
cold payload arena for every new pool comparison, including both frozen
controls; the previous cold evidence used 4 MiB. Pressure remains 64 MiB in
the private 128 MiB cap. Metadata is additional and must be reported. The
frozen DRP runner retains `max_inflight_reads(1)`, hence zero default
speculative credits.

For vector mode with B >= 2, full-vector refills target B-1 to B outstanding
speculative vectors, including resident-unconsumed pages. This is not a
guarantee of B-1 device requests: caller polling, completion and consumption
determine actual I/O overlap. Its witness is measured. Headline failure
blocks adoption and invokes the owner-specified pool-layer escalation.

## Untouched boundaries

`FrameState`, CLOCK replacement policy, EBR, retention, ring topology, guard
semantics and the repeated warm hit path retain their existing protocols. Reuse their terminal
operations; add no new per-hit epoch publication, lock, reference count or
learning step. The existing first speculative-consumption marker may publish
a bounded dirty notification for requirement 9; a repeated warm hit does no
new work. File/page formats and public guard APIs remain unchanged.
This scope adds no `get_span`, run allocator, executor, background worker,
kernel-security change, new registration policy or READV_FIXED dependency.

## Acceptance and verification

The [bench plan](../../../benches/plans/readahead_coalescing.md) owns the
unchanged six owner requirements and exact host/statistical protocol.
Before each implementation task, prove the corresponding failing check.

| Trigger | Required outcome |
|---|---|
| Full consecutive absent window | One READV per bounded run; exact SQE/byte count and one publication per frame |
| Steady default automatic scan after startup | Full 32-page vectors at 4 KiB; at most 33 read SQEs intersect each interior 1,024-page witness interval, including demand READs and boundary overlap; no steady singleton refill |
| Capacity below two full vectors, including explicit overrides | Preserve current per-page automatic admission; no full-window refill barrier |
| Multiple ready streams sharing credits | Commit advances round-robin turn; credit deferral retains it; no deterministic skip at each refill |
| One credit/page becomes free while a full window slides | Defer automatic refill until a full vector is eligible; demand remains independent |
| Resident/pending holes or duplicate hints | Correct report partition and singleflight join; no write to a hole |
| Partial-window consumption/abandonment | Useful protected pages survive replacement; unused credits recover |
| Short read in the middle of an iovec | Exact first-tail offset; completed prefix remains immutable; progress is bounded |
| Short read while all other driver slots are occupied | Continue in the original slot; no fresh reserve/refusal |
| Error after a successful prefix or EOF | Prefix stays readable; unpublished tail fails once; all credits reconcile |
| File retirement with active vector or held continuation | Same admitted logical read may finish its bounded continuation chain on retained file/buffer ownership; no new logical admission; retirement completes after ordinary terminal/reclamation conditions |
| Full driver/route/miss queues | Atomic pre-submit rollback or bounded deferral; demand still progresses |
| Span publication racing point-get join | Loom proves single writer, observable terminal state and credit conservation |
| Reused vector slab slot | Miri exercises actual raw destinations and repeated lifetime transitions |
| Idle poll, post-completion consumption, pattern reset | No capacity scan; no lost marker or credit; bounded work on affected entries only |
| Explicit protected prefix under full credits | One membership pass and at most one replacement sweep per call; preserve useful pages and report partition |
| Real eager and io_uring execution | Zero allocations after warmup, correct full and short I/O; ASAN on syscall paths |

Run the existing lifetime/compile-fail, retention, fault and zero-allocation
suites, plus strict Clippy and formatting. Fresh product regressions are
required after implementation; retained benchmark-only results are not a
substitute. No product tests or performance gates are claimed passed by this
documentation draft.

## Dependencies and review stop

The implemented prefetch and registration-posture contracts are prerequisites.
Completion fanout responsibilities move into this scope; no dependency on
constructing AM2's contiguous runs is introduced. Work is ordered in
[tasks.yaml](tasks.yaml), with the graph in [dependencies.yaml](dependencies.yaml).
[validation.yaml](validation.yaml) records the review status and both owner
decisions: the 512 KiB default budget and scope approval through the
owner's explicit implement invocation, 2026-09-14. Recording them claims no
product test or gate pass. RC1 and RC2 share the first batch; an immutable baseline
snapshot must exist before dispatch so RC1 can work from it while RC2 edits.
The budget-probe archive already contains a source/executable snapshot;
verify its product identity against the approved starting tree before use.
