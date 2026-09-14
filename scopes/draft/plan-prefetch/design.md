# Explicit prefetch and default sequential readahead

Execution boundary recorded 2026-09-13, before product implementation. The owner
requested closing the measured gaps, explicit readahead hints, and automatic
detection enabled by default with a disable option. The mmap benchmark suite
and resident-access controls are independent baseline evidence.

## Public contract

- `Pool::prefetch(&[PageId]) -> PrefetchReport` requests future residency. It
  never creates a guard or returns a ticket. Later `get`/`ready` is the sole
  consumption path. It enqueues work; `poll`/`poll_wait` drive completion.
- Reports partition requested pages into resident, already pending, newly
  admitted, deferred, and rejected. A stale file is rejected as an operating
  condition; a foreign pool identity remains a programmer error. The report
  describes admission, not completed I/O. Later demand observes read errors.
- A call examines a bounded prefix of the supplied slice; its fixed bound is
  established at pool construction. The unexamined suffix is deferred. All
  queues, metadata and request scratch are allocated at construction.
- `PoolBuilder::prefetch_headroom` sets the speculative credit ceiling.
  A default budget is bounded by the existing frame and in-flight limits;
  a configuration with no spare capacity has zero speculative capacity rather
  than failing an otherwise valid existing pool configuration. Explicit zero
  disables speculative admission.
- `Readahead::Automatic` is the default; `Readahead::Disabled` disables pattern
  detection while retaining explicit prefetch when credits are configured.
  The first automatic policy detects forward sequential demand per reader,
  grows bounded lookahead after confirmation, and resets on discontinuities.
  It is not a general predictor for random or arbitrary strided traffic.
- Resident hints remain exact frame/generation lookup observations. They do
  not request I/O, hold a frame, or select a readahead policy.

## Mechanism and safety boundary

Keep the existing shared driver, singleflight miss table, packed frame states,
guard/retention semantics and two-advance EBR protocol. Prefetch admission is a
batch under pool control using a preallocated free-frame reserve; it never
calls ordinary demand `get` or spins on `Busy`. Notify once for an admitted
window. Driver SQEs still describe individual granules; larger contiguous I/O
coalescing is a separate measured extension, not an implicit format change.

A credit covers a speculative read from admission through resident-unconsumed
state. Its outcome is exactly one demand promotion, unused eviction, failure,
or refused submission. Demand joining an in-flight speculative read promotes
at join; a resident hit promotes at first consumption. Existing CLOCK
first-touch control flow is the observation opportunity: repeated hits to an
already-referenced page must retain their current read-only path. No new
per-read epoch publication, reference count, lock, allocation, or access-pattern
update is permitted on that repeated warm path.

Credit state is orthogonal metadata, never a new `FrameState`. Resident
consumption may publish a marker without taking pool control; admission and
poll reconcile markers before using credits. Until reconciled, capacity may
be conservatively unavailable, but may never be over-issued. Frame reuse and
file retirement must clear the old observation before a new residency can
use the same index. Model the consume/reconcile/evict race explicitly.

The reserve is inventory of currently free destinations, not a permanent
partition of resident payload. A bounded poll top-up uses matured frames and
produces at most the uncovered victim deficit. Demand retains forward progress
when speculative credits or reserve inventory are exhausted. Unused
speculation is considered before demand-hot victims; measure partial-window
consumption as well as whole batches so reserve replenishment cannot silently
destroy useful lookahead. A failure here blocks adoption and triggers the
pre-recorded reserve/admission experiment, not weaker correctness assertions.

Automatic observation begins on the cold demand path. Speculative-consumption
feedback is reconciled during control/poll work, without learning on every
warm load. Reader/file identity boundaries prevent unrelated readers from
forming one synthetic sequential stream. Explicit requests have priority
over inferred work when competing for a bounded credit inventory. Prediction
errors, EOF and abandoned windows must release resources through the same
terminal paths as explicit prefetch.

Speculative first-touch records the consuming reader in the credit marker.
Credit promotion by another reader does not advance the originating stream.
The poller harvests markers into scratch bounded by the credit ceiling and
orders feedback by reader, incarnation and page; recycled metadata slots must
not create false discontinuities. Release/acquire on consumption observations
and a bounded rescan preserve earlier observations published by that reader.
This confirms consecutive consumption coverage at poll boundaries, not a total
trace of all resident accesses. Gaps reset training. Obsolete automatic entries
are evicted once resident; accepted reads finish before their resources return.

## Current platform posture

Registration is already configurable in the shipping tree; the draft's old
registration-layer exclusion is historical. Linux comparisons explicitly use
unregistered direct reads to match the frozen mmap baseline. No registration
policy or backend is changed here. On eager-inline platforms, prefetch queues
bounded work which executes on the poll caller; it does not create I/O overlap
or a worker pool. Automatic speculation must be measured there separately.

## Required evidence

The bench plan owns numeric gates. Safety checks cover window accounting,
zero allocation, duplicate/pending coalescing, cancellation by abandonment,
partial consumption, read errors and short reads, file retirement, full queues,
reserve replenishment during frame recycling, and automatic detection/reset/
disable. Add a Loom model for consumption markers racing reconciliation and
eviction. Preserve the existing lifetime, retention, EBR and regression tests.

Public counters distinguish admission, demand promotion, unused eviction,
failure and deferral. Promotion is evidence of demand interest, not a guarantee
of successful bytes served. CPU profiles, actual outstanding spans and bytes
read must accompany throughput. The existing synthetic permutation is a
constant modular stride; it must not be sold as evidence of a general-purpose
learned random-access predictor.
