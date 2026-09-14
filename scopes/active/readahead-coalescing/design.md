# Design: bounded readahead coalescing

Revision 3, 2026-09-14. The owner selected ordinary READV;
the revised [review gate passed](review.yaml). The owner approved implementation and selected a 512 KiB default;
[validation.yaml](validation.yaml) records the decision and rationale.
[scope.md](scope.md) defines the requirements and
[the bench plan](../../../benches/plans/readahead_coalescing.md) defines gates.

## Why this shape

The pool already owns reserve frames, singleflight entries, speculative
credits and forward windows. Its current one-page admission loses the window
before the driver sees it. Pass a bounded run through that seam and route its
one completion back to ordinary per-page terminal handling. Preserve separate
cache frames so point readers, retention and reclamation keep their semantics.

The driver probe removes much of the per-SQE cost under overlap. It does not
amortize the pool's page-by-page admission, reconciliation and completion work.
The owner records approximately 340 ns/page of prefetch bookkeeping and
180 ns/page of poll/completion as planning risks. These are prior estimates,
not independently validated universal coefficients. Parent operations must
batch control/reconciliation/notification per run while keeping required
per-page state transitions. The remaining serial-probe 2.72 us is not a target.

## Destination owner and storage

Introduce an internal read-destination enum: a point `InFlightFrame` or a
bounded ordered frame bundle. Its vector form owns up to 32 distinct existing
`InFlightFrame` tokens, each paired with its page/miss ordinal; no token is
cloned or reconstructed from an index. The owner travels through admission,
driver retry, completion/refusal and pool publication. All success/error/
teardown branches must consume or return every token.

RC2 carries this destination enum through `OpEntry`, `OpContext` and the
private construction/extraction of `Completion`; the completion owns the
returned bundle at the final outcome. A partial vector completion retains
the same driver slot through an exclusive continuation lease. Adapt the existing
point consumer in that task so it remains buildable. Pool vector admission
stays disabled until RC3 adds the vector method to `PoolBackend`, updates its
Driver/MockDriver/MockRingDriver implementations and installs span routing.

Keep iovec backing storage preallocated per driver completion-slab slot,
independently allocated from mutable op metadata. Build pointers from audited
allocation-wide raw bases and existing per-frame transfer witnesses. No mutable
slice may cover another outstanding slot's kernel-visible descriptors. Once
submitted, a slot's iovecs/destinations are immutable until its CQE is observed.
The pointed-to payload and iovec memory outlive the kernel operation even if
the caller drops all interest. Miri must write through actual stored pointers
after other slots are prepared and after repeated completion/reuse.

The bundle uses fixed-capacity initialized storage with an explicit live
prefix; extraction empties each token position once. A completion can move
the bundle after the kernel operation has ended. A partial completion owns
the reserved slot while the pool extracts published tokens and rebuilds the
unpublished suffix in place. The slot never enters the free list between
these phases; continuation does not reserve a new slot or increment the
logical operation generation. Published tokens never return to the driver.
The lease retains file accounting until final completion/abort, including
file retirement. Dropping an unresubmitted lease terminates the unpublished
tail and releases the slot; no kernel access is outstanding then. Accepted
resubmission transfers that ownership back to the driver. Completion storage and any
`OpContext` extension must remain allocation-free on both backend paths.

At 4 KiB, `vector_frames_max = 32` and `vector_bytes_max = 128 KiB`. For other
legal granules, effective width is `min(32, floor(128 KiB / granule))`; a width
below two selects the unchanged point path. Do not newly reject existing
large-granule configurations. Checked arithmetic covers file offsets,
PageId windows, descriptor lengths and construction-time storage sizes.

## Bounded routing

A pool span slot owns an ordered list of miss-slot identities/generations,
the original file/run identity, current driver `OpToken`, original byte count,
monotone filled bytes and count of already published pages. Use explicit
Free/Preparing/InFlight/Completing states; no combinations of independent
flags. Driver tokens retain their existing generation source.

Size the route slab at construction to at most `max_inflight_reads` entries,
with at most 32 miss ordinals per entry. A smaller allocation may use the
proven two-page-minimum bound; do not use `ceil(read_limit / 32)`, which cannot
represent many two-page runs. Report exact metadata bytes at build/benchmark
time. An init-sized driver-slot-to-route index with generation checks consults
the span route in O(1) before the existing point `find_by_token`. Size this
index for the backend's actual operation-slot namespace (including product
ops), validated at pool construction; update all PoolBackend implementations.
Do not scan all routes or perform per-frame linear token lookup within a span.
Route exhaustion defers the run before frame or table mutation. No dynamic
map, allocation, duplicate token namespace or waiting for a free route.

## Admission transaction

The caller holds pool control for classification and commit. Reconcile
speculative state once for the batch, then inspect at most the existing
request-prefix bound. Preserve slice order and protect every page of the full
examined prefix during replacement, including pages outside the current run.
The unexamined suffix is deferred and grants no replacement protection.
Form same-file consecutive absent runs, splitting
at every resident/pending hole. Duplicate occurrences get ordinary report
outcomes after earlier admissions; they cannot acquire a second destination.

For each bounded missing run:

1. Automatic vector admission first requires enough free speculative credits and
   confirmed eligible pages for the target vector width. It defers while
   only one credit or one new page is available. The target is the effective
   vector width, reduced only by a smaller confirmed startup window or a
   genuinely finite horizon tail. A sliding horizon advancing by one page
   is not such a tail. Explicit admission instead uses the currently supplied
   run and available resources immediately; a singleton takes point READ.
   Both paths respect read-frame credits, reserve inventory, miss slots and
   vector/byte limits; automatic resource shortage defers the full target
   rather than shrinking it into steady point reads.
2. Reserve a route and every required miss slot/credit before claiming frames.
   All preparation is private under control. A preparation failure returns
   inventory immediately; no partial table mutation or predictor advance.
3. Claim k arbitrary free reserve frames, construct their unique bundle and
   submit one vector operation. Driver refusal returns all tokens for abort
   and restores every reserved resource before returning `deferred`.
4. Commit the k miss entries, route identity and per-page speculative records
   before releasing control. A concurrent poller may reap at the driver but
   cannot route/publish through pool control until these identities exist.
   Notify once for the admitted batch.

Define automatic mode at construction. Let V be the granule-adjusted vector
width and A=min(speculative capacity, R.saturating_sub(1)). Use vector mode
only if V >= 2 and A >= 2*V; otherwise keep the current per-page automatic
policy. This avoids draining an entire small budget before every refill,
including explicit overrides and R=33/C=32. In vector mode, do not shrink
V merely because only a few credits are free. The two proposed defaults
both use V=32 and satisfy this condition. Startup widths may still be smaller.

Ready streams keep round-robin admission order. A successful run advances the
turn; a credit-deferred stream keeps its turn until capacity permits it.
A stream with no confirmed eligible window is not ready and cannot block
others while awaiting demand. Readiness changes with existing demand/feedback
events; do not add a reader-array scan to every empty poll. Bounded shared
credits limit concurrent windows, not demand progress; no physical-overlap
guarantee is made for competing demand or stalled consumers.

Do not concatenate discontiguous file offsets merely because their memory
destinations form one iovec array: READV advances one contiguous file range.
Do not re-read resident holes into their live payload or into throwaway frames.

## Automatic windows, refill and EOF

Replace the predictor's one-page request handoff with a bounded run handoff.
Retain reader/incarnation isolation, confirmed demand horizon and the `next`
cursor. Use checked offsets and commit cursor progress only for classified
or admitted pages; deferral preserves the next unprocessed page. In vector
mode, a confirmed startup width below 32 permits a short vector. Once width reaches 32,
accumulate a whole new vector before refill. A sliding horizon advancing
one page is not a new finite-tail exception. Below the construction-time
two-vector condition, preserve per-page automatic admission; zero credits
disable it. Resident/pending holes split runs
without rereading them; classify a whole eligible window before splitting it.

`Pool::prefetch` is opportunistic: extending a protected window by one page
can issue a point read. It does not delay a caller's explicit singleton to
collect a future call. The explicit benchmark repeats its useful protected
window and extends the admission frontier in vector-sized chunks. Automatic
admission waits for a complete eligible vector as specified above.

No file-length snapshot, stat, growth tracking or extent-generation reset is
introduced. Short reads and `Ok(0)` use existing terminal paths: publish the
complete prefix, fail the unpublished remainder once, and reset the predicting
incarnation. Late feedback cannot restart that incarnation. A later demand
sequence can establish a new pattern normally; accepted vectors still drain.
Each request is at most 128 KiB. Several vectors may already be beyond EOF
before its first completion is observed, so a universal one-request-per-pass
bound is not claimed without extent knowledge. The admitted window bounds
speculative waste; RC-G6 measures EOF calls/bytes and late accepted tails.
Extent tracking returns only as a separate change if that cost matters.

## Byte budget and arena geometry

The owner-selected default budget remains pending between 512 KiB and 1 MiB.
Convert bytes to whole pages with floor division, capped by spare frames and
R-1, retaining demand reservation. An explicit page-count override preserves
its existing semantics. Caller-configured R is not silently increased.
The scan runner uses R=2*C, miss headroom=3*R and an 8 MiB cold payload arena
for candidates and frozen controls; pressure retains its 64 MiB payload arena.
At 4 KiB the two budgets require 897 / 1,793 minimum frames, including one
guard. Report descriptor, routing and notification metadata separately.
The frozen DRP runner keeps R=1 and zero default speculative credits.

B vector credits, B >= 2 in vector mode, target a cycle between B-1 and B speculative vectors,
including resident-unconsumed pages. This does not guarantee kernel occupancy:
poll cadence and consumption determine actual accepted I/O overlap. Record
refill-with-pending and outstanding-request witnesses. Do not introduce a
new polling policy only into the candidate to manufacture a comparison win.

## Bookkeeping proportional to changed runs

Replace capacity scans in `reconcile`, obsolete eviction and marker harvest
with preallocated indexes and notifications. A completed span names its
entries directly. Frame-to-speculation-slot indexes remove linear terminal
lookup. Per-incarnation membership schedules obsolete cleanup on invalidation
or a terminal/reclamation event that can make deferred eviction possible;
it does not rescan pinned/in-flight obsolete entries on every empty poll.

First speculative-consumption CAS success publishes a dirty-frame bit into
an init-sized hierarchical bitmap. This extends only the existing first
consumption branch in `Clock`; reference-bit policy and repeated hits stay
unchanged. Propagate the dirty notification through every bounded level even
when an ancestor bit was already set. The consumer atomically takes dirty
bits and examines only those frames, resolving current entries under control.
EBR prevents frame reuse while the publishing reader is pinned; stale bits
for finished entries cannot terminalize a replacement entry. An empty root
costs O(1). Repeated notifications coalesce into fixed frame bits, with no
queue overflow. A notification racing a bitmap drain remains discoverable;
Loom covers that handoff and frame/entry reuse.

Preserve ordered per-reader/incarnation feedback, including earlier markers
made visible by a later acquisition. Drain the relevant dirty batch before
admission and apply feedback in page order using preallocated bounded storage;
remove the repeated capacity scans and quadratic insertion sort. Fixed-width
key ordering over changed feedback is linear in that batch. Consumption of
already completed pages must progress even without another CQE; the bitmap
is drained when poll runs, not only after an I/O completion.

The cost target is O(1) empty-poll control work plus O(k) per completed or
notified run, k <= 32: linear in affected pages/events rather than configured
capacity. Reset cleanup is charged once to affected entries. Admission and
CLOCK's existing eviction work are counted separately. No claim of constant
work for a nonempty fanout or guaranteed progress without caller polling.

Explicit protection also batches its work. Resolve the bounded examined
prefix through existing page/miss lookups and mark protected speculative
entries with a preallocated per-call stamp; newly admitted protected entries
receive that stamp too. A replacement cursor traverses the speculative slab
at most once across the entire hint call. Each membership check is a stamp
comparison, not `protected.contains` over the slice. A skipped ineligible
entry may wait until the next call; the current call reports deferral.
Thus protection/replacement overhead is O(examined prefix + capacity), with
bounded page lookups, admission and existing CLOCK work counted separately.
Stamp exhaustion is checked before reuse; no allocation or stale protection.

## Completion, continuation and conservation

For a span with k pages of granule g and total k*g bytes:

- Validate the CQE belongs to the current driver generation and does not
  exceed the requested remainder. Driver transient retries keep their existing
  bound and retain the same ownership.
- On positive bytes b, increase `filled` by b. Publish newly complete pages
  up to `floor(filled/g)` through existing miss/frame terminal operations.
  Each publication returns that page's read credit; it does not imply that
  an unconsumed speculative page's separate credit is free.
- The remaining file offset is original offset + filled. The first remaining
  destination starts at `filled % g`; later destinations start at zero. The
  requested suffix is original total - filled. Remove published tokens before
  rebuilding these descriptors in the same reserved slot. Resubmit only if all direct-I/O alignment
  requirements hold; otherwise fail the unpublished tail like the point path.
- `Ok(0)` is EOF for all unpublished pages. A permanent error, misalignment
  or abandoned continuation also terminates every unpublished miss once. Preserve already
  published pages. Return each unfinished token through abort and release
  its read/speculative failure accounting exactly once.

Continuation retains the original slot and file accounting after the CQE;
only final completion/abort returns them. If the submission queue cannot
accept the suffix immediately, keep it in the existing bounded ready path,
with the normal progress bound. Exhaustion of unrelated completion-slab
slots is not a terminal error for this operation. A completion/continuation
lease is consumed once, and teardown handles leases as well as accepted I/O.

Retirement is not an abort condition for a held continuation. It continues
the already admitted logical read on the retained descriptor, just as the
existing point-read remainder path does. File retirement rejects new logical
admissions and waits for this chain's terminal result and ordinary interests/
reclamation before closing. The original slot/read credit stays owned while
the lease is held or queued; its bounded short/error rules still apply.
Explicit abandonment/drop may terminate a lease, but retirement alone does
not. Cover retirement before resubmission and during a later short completion.

Every positive continuation advances at least one byte. Bound positive
completions by the original byte count (at most 128 KiB for vectors); bound
transient attempts by that count times the existing `(retry_bound + 1)`, using
checked arithmetic at init. No-progress success is terminal EOF, not a retry.

Conservation at control boundaries:

`read_credits_used = sum(unpublished point/span destinations)`

`speculative_credits_used = in-flight unpromoted + resident unconsumed`

Demand promotion may free a speculative credit while an in-flight read credit
remains held. Waiter drop frees neither unfinished-read ownership nor credits.
Route storage returns only after no continuation can target it. Pool/driver
teardown drains accepted operations before destroying descriptors or arenas.

## Backends and registration

Linux uses one ordinary READV for vector operations in either current
registration posture. Existing point dispatch keeps READ/READ_FIXED posture
behavior. Eager fills the vector in order on the polling thread outside the
submit lock, using a vectored syscall where available or a bounded per-element
pread loop. It returns a successful prefix/short count for common pool
continuation and creates no I/O worker. The mock copies each reported prefix
across vector elements and injects EOF, failure and reordering consistently.

READV_FIXED appears in the
[Linux 6.15 operation enum](https://github.com/torvalds/linux/blob/v6.15/include/uapi/linux/io_uring.h#L272-L275).
It is a later option for registered buffers, not part of this scope or a
claimed improvement on the pinned 6.6 host. Plain READV follows the
[ordered iovec contract](https://man7.org/linux/man-pages/man3/io_uring_prep_readv.3.html).

## Alternatives and review risks

- Per-page submission is the preserved point path and comparison baseline;
  it leaves measured per-SQE overhead on consecutive prefetch misses.
- AM2 runs are deferred for the owner's Registered/pre-6.15 condition. They
  are not required to read into scattered frames with ordinary READV.
- A separate coarse-granule pool already has favorable scan evidence but
  changes acquisition granularity and can create duplicate residency. This
  scope preserves independent pages instead.
- The byte budget is an explicit pending owner decision. RC-G2 compares
  coalescing both with the frozen old default and with the current per-page
  mechanism at the same new budget, read limit and arena. This separates a
  capacity increase from vector benefits; the earlier 6.6% gain from 32 to
  64 point-read credits is motivation, not proof at 128 or 256 credits.
- Extent snapshots are excluded. Count EOF waste and already accepted tails;
  revisit separately only if RC-G6 shows it matters.
- Byte-exact completion fanout and lifetime ownership are the principal
  safety surface. Review must cover publication racing a point join, route
  reuse, file retirement, bounded progress and per-frame conservation.

Admission/descriptor preparation is O(k), k <= 32; terminal fanout is O(k)
over a span's lifetime. Span route lookup is O(1); control notification work
is proportional to affected entries as specified above.
Allocation remains zero after warmup. No new work belongs to repeated warm
hits. Approval of this draft, including its safety model and bench gates,
precedes all product implementation.
