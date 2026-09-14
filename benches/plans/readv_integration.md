# Proposed bench plan: READV integration with independent cache pages

**Superseded, 2026-09-14:** the owner selected READV and supplied the exact
cross-backend boundary and headline gates. The canonical draft is
[readahead-coalescing](../../scopes/draft/readahead-coalescing/scope.md), with
[readahead_coalescing.md](readahead_coalescing.md) as its bench plan. The
earlier proposal below is retained as context; its narrower posture/platform
boundary and proposed thresholds do not govern implementation.

Prepared 2026-09-14 after the owner's "go" following the corrected mechanism
probe and submit/ready split. This makes the next product step reviewable.
The earlier steering explicitly leaves mechanism selection to the owner;
until that choice is recorded, this document proposes work without amending
arena-modernization or plan-prefetch and without authorizing implementation.

## Decision proposed

Use ordinary READV to aggregate consecutive **missing file pages** into
independently owned 4 KiB cache frames on Linux with resolved Unregistered
posture. Keep single-page consumption through `get`/`ready` and the existing
explicit/automatic prefetch API. Bound one aggregate to 32 pages / 128 KiB
for the measured 4 KiB configuration. Start with prefetch admission, which
already sees windows; add no public span-acquisition API in this step.

Registered and eager backends retain their point-read path in this proposal.
No contiguous-run allocator is required for the Unregistered mechanism.
AM2's completion fanout and byte-exact progress responsibilities still apply,
but its contiguous first-frame representation cannot describe scattered
destinations. Its rejection of plain READV and whole-span fallback rule need
an explicit owner amendment before this proposal becomes implementable.

## Gate plan

| Field | Value |
|---|---|
| Metric & direction | Candidate/base whole-scan elapsed and thread CPU per useful 4 KiB, lower is better; exact submitted SQEs, CQEs, bytes, group sizes, in-flight **frames** and bytes, speculative occupancy, consumed/wasted speculation and time to terminal drain |
| Workload | Existing immutable fixture and full-page fold: cold 64 MiB scan with an 8 MiB payload arena, and three 256 MiB passes with a 64 MiB arena inside a 128 MiB process cap, swap zero. Independent 4 KiB cache pages in every product arm. Explicit and automatic prefetch measured separately; point/random/dependent and mixed-residency controls below |
| Host protocol | nix Threadripper 3970X / Samsung 970 PRO / Linux 6.6.64/ext4, worker CPU 0, controller CPU 4, performance governor, THP never. Preserve direct/Unregistered posture, file-local cold/PTE verification and before/after boot, queue limits and activity snapshots. No competing campaign |
| Baseline | Freeze the current product and exact harness before implementation. Also provide a benchmark-only per-page control in the candidate executable. Pair ordinary and aggregated admission at identical cache granules, frame/credit budgets, fixture, demand order, arena bytes and final-drain boundary; mmap MADV_SEQUENTIAL is a separately paired baseline |
| Reps | Two qualification plus 30 alternating fresh-process pairs per adopted comparison; smoke/selection samples never establish a win |
| Threshold | At the existing matched 2 MiB accepted-frame byte budget: aggregated/per-page elapsed upper <= 0.80 for both scan shapes, separately for explicit and automatic modes. A claim of closing the mmap gap requires aggregated/mmap elapsed upper <= 1.00 on each claimed shape. At unchanged 32-credit defaults, aggregated/per-page elapsed and CPU upper <= 1.02 on scans; no default mmap-win claim without its own <= 1.00 pair. Existing 12 adoption/regression gates retain their original bounds and require fresh product runs |
| Compare command | Shared `mise run gate CASE/paired.csv BOUND`; CPU uses separately validated `CASE/cpu/paired.csv`. Summaries use the shared compare implementation, with candidate/base orientation and raw-row/CSV hash reconciliation |
| Escalation lever | Any ownership, byte, allocation, credit or drain failure blocks the change. A failed performance gate is retained and triggers profiles of both exact arms plus bounded occupancy/phase replays with observer/plain elapsed and CPU upper <= 1.05. First distinguish range formation, per-frame completion/consumption CPU and insufficient overlap; do not change reclamation, default budgets or thresholds to conceal a failure |

These are proposed gates written before implementation, not passed product
results. The driver probe's depth-8 parity gate does not substitute for them.

## Separate request aggregation from available overlap

The current `reads_in_flight` counter already counts logical frames, including
prefetch. Preserve that meaning: one READV owning k destinations occupies k
read credits, despite consuming one driver SQE. Speculative credits also stay
per page and cover their existing in-flight/unconsumed lifecycle. Returning
one CQE must not release a single credit for k live frames or release credits
for unpublished short-read tails.

The default speculative ceiling is 32 pages. Eight full 128 KiB groups require
256 pending pages, so the probe's depth-8 result cannot be promised by merely
switching the opcode under that default. The matrix keeps two distinct rows:

| Configuration | Speculative pages | Read-frame ceiling | Pending useful-byte ceiling |
|---|---:|---:|---:|
| Existing scan default | 32 | 64 | 256 KiB total; at most 128 KiB speculative |
| Existing matched geometry control | 511 | 512 | 2 MiB total |

All per-page/aggregate comparisons preserve the row's exact budgets. Add a
256-page speculative / 257-read-frame characterization only if the 2 MiB
row needs an overlap boundary check; declare it before running. Configured
limits are never substituted for observed occupancy. Raising product defaults
would be a separate measured policy choice, not a side effect of aggregation.

## Integration responsibilities to review

1. **Range formation and admission.** Under the existing control lock, form
   bounded same-file consecutive miss prefixes from the caller's examined
   window or the predictor's current window. Resident/pending pages are holes:
   preserve their ordinary outcomes and split missing runs around them. Do
   not reread hits, duplicate singleflight entries, sort arbitrary caller
   plans into a new access policy, or wait for a future full group. A singleton
   uses the ordinary read path. Claim only available reserve frames and
   reserve all required miss/span metadata before submitting an aggregate.
2. **Ownership across the driver.** Today's `ReadLease`, `ReadRefusal`,
   `OpEntry` and completion payload carry one `InFlightFrame`. The aggregate
   path needs one bounded owner of every destination token and its stable
   iovec storage from admission through terminal completion. A slab index is
   not permission to duplicate, drop or recreate those unique frame tokens.
   Submission refusal returns every unsubmitted token for rollback. Driver
   retry/reap and teardown must retain the owner while the kernel can write.
3. **Completion fanout.** Resolve the aggregate by the driver's generation
   token before ordinary point lookup. Publish each complete page once through
   the existing frame/miss terminal path. Retire successful leading tokens;
   retain only the unpublished tail. Advance partial progress by exact bytes,
   trim the first remaining iovec and file offset together, and resubmit only
   if the resulting direct-I/O range meets the existing alignment contract.
   EOF, misaligned continuation, permanent error or refused resubmission fails
   every unpublished page and returns each remaining read/speculative credit.
4. **Bounded storage and fallback.** Preallocate destination/iovec/route
   metadata at construction and state its byte budget. Size route capacity
   for short aggregates too: `ceil(read_limit / 32)` slots cannot represent
   the permitted number of two-page runs. Metadata exhaustion falls back or
   defers before mutation; it cannot introduce a spin or force reclamation of
   live tokens. Keep queues, retries and short-read republications bounded.
5. **Existing semantics.** Guard lifetimes, EBR, retention, CLOCK, point-read
   singleflight and waiter-only cancellation remain owned by their existing
   scopes. Dropped demand interest never releases storage still used by an
   aggregate. File retirement waits for accepted I/O exactly as before.
   Automatic learning remains default-on with its existing disable option;
   explicit hints keep their protected-window semantics.

## Failing checks required before product code

| Case | Required witness |
|---|---|
| Full 32-page consecutive miss | One READV, 32 unique destinations/miss identities, exact byte count and per-page publication |
| Resident and already-pending holes | No read touches their payload; missing prefixes/suffixes submit independently; demand joins existing work |
| Point get races range admission | Exactly one writer per page and one terminal outcome per interest; add admission/join/completion to the Loom seam |
| Short read on and within page boundaries | Complete prefix publishes once; partial suffix preserves exact bytes; submitted storage excludes published frames |
| EOF/error/refused continuation | Every unpublished token and credit terminates once; successful prefix remains readable |
| Insufficient credits, miss slots, route slots or driver slots | No partially committed aggregate, no hot allocation, bounded fallback/defer and exact conservation |
| Dropped waiters, file retirement, pool teardown | Kernel-visible tokens/iovecs outlive accepted work; no premature publication or reuse |
| Repeated slot reuse and stale generation | Old completion cannot resolve a new aggregate; Miri writes through actual retained pointers across consume/reuse |
| Max admitted frames plus retained/hot pages | Preserve the existing progress/watermark proof; no credit counted per SQE in place of per frame |
| Registered, eager and disabled/speculation-free paths | Existing point semantics, zero allocation and original regression bounds stay intact |

Syscall-boundary fault tests must transfer each injected successful prefix,
not merely return a short byte count. Exercise actual Linux I/O under ASAN;
Miri covers pointer/ownership transitions and Loom covers relevant races.
Compile-fail lifetime and retention suites remain required product checks.

## Attribution and completion criterion

Record admitted pages separately from actual SQEs and terminal CQEs. Reconcile
each aggregate's byte progress and page outcomes, including retries and failed
tails. Measure warm resident accesses independently so a faster storage path
cannot hide an extra cost on every guard acquisition. CPU close to wall time
still includes busy polling; no spare-core or device-ceiling claim follows.

The remaining 2.72 us in the serial probe is not an implementation target for
this plan. It includes several phases and was not isolated to a function.
Use the end-to-end gates to identify the limiting resource after integration;
do not optimize a guessed coefficient first.

Implementation can be adopted only after the owner records the mechanism/
mixed-residency amendment, the safety checks pass and all applicable product
gates pass. Until then, the completed driver evidence remains a mechanism
result and the coarse-granule pool remains the already-measured scan option.
