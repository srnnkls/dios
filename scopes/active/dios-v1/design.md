# Design: dios-v1

---

Revision 14 records R7 without promoting it to shipping design. Moving repeated
frame verification and exact coordinates out of the read loop cut the aligned
mmap control from 519.178 to 244.537 ns/read. Unmodified general Dios `get`
then cost 335.570 ns/read. A typed file lease plus volatile, generation-checked
resident hint reduced that to 305.527 ns/read and materially beat general get
(ratio 0.8813 / CI upper 0.8980 in the dedicated paired gate), but remained
20.45% slower than locator mmap (CI upper 1.2229). The apparently hot optional
descriptor branch was a false attribution: replacing the niche-encoded option
with a dense vacant descriptor regressed and was reverted. The remaining
fresh-pass cost is dominated by an extra random descriptor/cache touch plus
the intended retained epoch/read-stability work; no guarantee was deliberately
removed, but the lifetime/reuse claim is not accepted until Loom passes. The
feature-mock prototype in the Dios point-proof worktree is evidence only, and
its hinted pin cannot ship before the get/retire and evict/reuse Loom schedules
prove that every outcome is stale or linearized before invalidation.

Revision 13 separates the earlier prototype evidence from shipping design. The prototype
is preserved at Sira commit `36211b4` in the repo-local worktrees named by
`scope.md`. It validates the aligned-prefix codec,
exact REMIX byte coordinates, bounded one-handle O scratch, and n=10 geometry,
but its corrected Threadripper n=1 replacement ratio is 1.1103 with CI upper
1.1408. These mechanisms remain candidates, not an approved vNext format,
until T018 removes that regression and clears the fresh-process four-length
gate. The full evidence ledger is
`resources/remix-dios-native-experiment.md`.

Revision 12 retains revision 11's page-native AD-5/AD-6/AD-8 contract but
preserves Sira's measured prefix grammar inside each independently verifiable
4 KiB frame. Existing AE010 evidence rejects fixed-width SoA as the shipping
default on the canonical workload; its 22-row geometry remains a reporter
control only. Eight frames remain one logical encoding/prefetch group, REMIX
stores stable frame/byte-offset coordinates, oversized extents stay bounded,
and earlier stores are rebuilt without a compatibility reader.

## Problem

| Metric | Current | Target |
|--------|---------|--------|
| Miss handling | synchronous page fault, invisible to any scheduler, unbatchable | submitted op; worker overlaps other work (DIO-G3) |
| Warm block-fetch (Linux) | mmap fetch feeding CRC+decode (post read-perf/-concurrency baseline) | one-sided 95% CI upper bound of the ratio ≤ 1.02 (non-inferiority, 2% margin, DIO-G1) |
| Hot-path allocations | 0 (mmap path) | 0 (pool path, enforced harness) (DIO-G4) |
| IO-error model (reads) | SIGBUS on fault | `Err` from completion, fanned to all waiters |
| Eviction control | kernel reclaim + TLB shootdowns | explicit CLOCK, deferred reuse at poll boundaries |
| nmnm residency layer | specced only (`architecture.md:39-56`, stub `src/io/`) | implementable by this crate unmodified (DIO-R5) |

mmap's miss is a trap: it cannot be batched, made async, or observed by
the embedding's IO plane — the fault fires inside a sync compute kernel
and stalls that worker invisibly. Under the planned nmnm integration
(unresolved `MemHandle` → `Pending` → bubble to the async,
runtime-agnostic IO side, which issues the IO and re-readies the work
item; work stealing exists only in nmnm's sans-io compute tier) page
faults are the one IO shape that cannot participate: residency is driven
entirely from the IO side, which needs the observable completions mmap
cannot provide. The write
plane additionally gains O_DIRECT on segment data (no page-cache dirtying
by large compactions) and a single IO abstraction for future nmnm
extraction.

Layering (revised 2026-07-11 — the original paragraph predated sira's
zero-copy work and described a cache that no longer exists): the pool is
the raw-granule residency layer directly replacing mmap at the
`BlockSource` seam. Post-RH003 the "decoded-block cache" owns no bytes:
`CachedBlock` holds `BlockPayload::Mapped`, a reference into backend
bytes kept alive for the cache entry's arbitrary lifetime
(`cursor.rs:117-123` states the pinning invariant). Under the pool that
would mean a long-lived frame pin per cache entry — the opposite of this
design's EBR model, whose guards are short-lived and cursor-scoped, whose
reclamation halts on a stalled guard, and whose watermark budgets a
handful of guards per reader. Resolution, owned by sira-point-format
batch 1 (a blocker of the sira phase): the cache degenerates to what it
already is post-zero-copy — a parse-once artifact memo plus the
FirstTouch verified bitmap, byte-source-independent metadata — and block
bytes reside only in the backend, fetched through a short-lived guard
inside `load_into` (`cursor.rs:937`), exactly the discipline the EBR
model assumes. DIO-G1 therefore measures the block-fetch layer with the
artifact memo warm in both arms; the earlier "decoded cache bypassed"
framing is retired — there is no byte cache to bypass, and bypassing the
metadata would compare a path neither backend ships.

---

## Alternatives

### Keep mmap + block cache everywhere (status quo direction)

The read-perf/-concurrency scopes' mmap-borrow + decoded-block cache.
Remains the macOS read path.

**Rejected (as Linux endpoint):** page-fault misses are unschedulable and
SIGBUS stays in the failure model; does not implement nmnm's residency
contract. Supersedes mmap-soundness.md's "keep mmap" conclusion for
Linux.

### Futures + owned buffers (monoio/tokio-uring shape)

`async fn read_at(buf: impl IoBufMut) -> BufResult<usize, B>` — ownership
round-trip makes drop-safety trivial.

**Rejected:** requires an executor sira and nmnm don't have; buffer
round-trip fights zero-alloc (owned buffers per op or a secondary pool);
futures through kernels contradicts nmnm's passive synchronous kernels.

### Runtime-allocated read views (glommio `DmaFile::read_at → ReadResult`)

Elegant API; pool hidden inside the runtime.

**Rejected:** allocates/refcounts per read from the runtime's dma
allocator — violates the zero-alloc constraint; ties the crate to a
thread-per-core executor.

### Blocking thread pool as the portable backend

N pread/pwrite workers completing into the shared queue.

**Rejected:** on macOS dios's write-plane callers are synchronous by
contract (`CommitFs`, journal barriers, compaction threads) and pool
misses blocking their caller at `poll()` already match mmap fault
concurrency — the pool's handoff latency, parking/wake machinery, and
second threading model buy overlap nobody consumes. Single-thread
prefetch overlap is a uring capability. The eager-inline backend (AD-7,
TigerBeetle's shipped darwin shape) replaces it: submit enqueues, poll
executes the syscall on the calling thread outside the lock.

### Whole-logical-group assembly

Keep today's approximately 32 KiB block as the verification and borrowing
unit, then assemble it from several Dios frames.

**Rejected:** a point selected by REMIX would acquire eight frames and either
hold a fragmented view or copy the whole logical block. AD-5 instead makes
each 4 KiB frame independently verifiable and keeps the approximately 32 KiB
unit only for encoding and prefetch. Ordinary records are frame-local, so a
point borrows one selected frame. The base range/extent protocol acquires,
decodes or copies, and drops one ordinary guard/epoch before acquiring the next
frame; a 16-frame request chunk is a control/scratch bound, not a shared epoch.
Only AD-6's explicitly bounded oversized-record path may use preallocated
caller scratch or a segmented consumer. A shared epoch/true gather remains a
conditional later optimization requiring its own INV-9 amendment and proof.

### Fixed-width SoA record frames

Pack 22 canonical 24-byte-key/150-byte-value rows into each aligned frame,
leaving a simple frame-local row ordinal and 152 bytes of padding.

**Rejected as the selected format; retained as projection F:** adaptive
execution's existing AE010 causal control built independent 5M-row prefix and
fixed-width stores. On Threadripper fixed width lost by 4.2% at n=10 and 15.6%
at n=256, used 0.99% more bytes, and a pre-resolved minor recovered at most
1.7%. Its transparent packing arithmetic remains useful as a conservative
geometry control, but only aligned prefix candidate B may enter T012, after its
own end-to-end format gate clears.

### compio-driver as the driver foundation

`Proactor` is the closest existing analog: completion-based, runtime-free
push/poll, io_uring + IOCP + poll drivers (resources/compio).

**Rejected:** driver-level owned-buffer round-trip (`push -> PushEntry<Key,
BufResult>`) cannot direct a read into a pool-assigned frame; per-op heap
allocation (`Key` wraps `ThinCell<RawOp<dyn Carry>>`) violates DIO-G4; no
registered buffers — only iour buf_ring, where the kernel picks the
buffer, inverting CLOCK-managed placement; macOS files dispatch to its
`AsyncifyPool` blocking thread pool, the AD-7-rejected shape. Its wins
(IOCP, cancellation) are non-goals; its op-state machines are reference
material for T003/T004. compio boxes per op partly because IOCP needs a
stable per-op pointer (`RawOp` ptr = `OVERLAPPED` ptr, key.rs) — dios's
fixed-capacity slab yields stable slot addresses without boxing, so even
a future IOCP backend would not require compio's allocation model.

### Selected: completion driver + registered frame pool, platform-split defaults

TigerBeetle's structure (intrusive completions, no kernel-op cancel,
poll-driven) with Rust-native soundness: frames are preallocated and
registered with the ring, ops reference frames by index, so no lifetime
crosses the kernel boundary — the classic io_uring drop-safety hazard is
dissolved structurally rather than papered over with owned-buffer
round-trips.

---

## Post-v1 Sira-fit product boundary (revision 10)

The completed v1 implementation exposed the right completion machinery
but left its Pool ownership seams too closely tied to that machinery.
Revision 10 makes `Pool` the crate-root product API and retains the leaf
completion API explicitly under `dios::driver`. Product callers never
name a driver op kind/token/completion batch, a raw read/close completion,
or a backend-specific arena.

The Pool owns one composed driver and all four product concerns:
residency, write/fsync admission, progress/waiting, and file retirement.
Its closed root vocabulary is `PoolWriteArena`, `PoolWriteSlot`,
`PoolToken`, `PoolCompletion::{Write,Fsync}`,
`PoolCompletionBatch`, `PoolSubmitError`, `SyncMode`, `PollReport`,
`PoolWakeHandle`, `GetError`, and `RetireStatus`. These are product types,
not aliases that permit accidental interchange with `dios::driver`
tokens, kinds, batches, or staging slots.

### Owned capabilities, borrowed bytes

- `ReaderCtx` owns `Arc<ReaderRegistry>` plus one slot and pool identity.
  It is lifetime-free and `!Send + !Sync`: it can be stored beside or
  outlive the Pool value, but its epoch slot never changes threads.
- `PendingToken` owns `Arc<MissInterests>` plus miss generation and pool
  identity. It is affine (`!Clone`) and `Send`; `Sync` is deliberately
  unspecified. A work item may
  move it to another thread, whose local `ReaderCtx` supplies the epoch
  pin when `ready` produces a guard. Drop releases waiter interest only.
- `FrameGuard<'pool>` remains a borrow of Pool bytes and the destination
  reader epoch slot and is `!Send + !Sync`. No owned capability turns a
  frame borrow into owned or `'static` bytes.
- Every `get`/`ready` checks Pool identity before observing target-pool
  state. A cross-pool reader/token is a programmer error. Retired file
  identity is an expected value: `GetError::StaleFile { page }`.

Pool Drop quiesces and releases the backend even when ReaderCtx or
PendingToken metadata remains alive. Their small Arc-owned registries
survive only long enough to make later capability Drop safe; they do not
retain the complete Pool/driver. This is the ownership shape required by
an embedding owner without self-reference or unsafe lifetime extension.

### Truthful progress and a composable wait

`poll_report` drains a preallocated raw backend batch, routes every read
completion privately, advances/reclaims epochs, and copies only owned
write/fsync results into the caller's fixed-capacity
`PoolCompletionBatch`. `PollReport::backend_completions` is the number of
CQEs actually drained in that invocation, independent of how many caller
results fit. Overflow caller results remain in a preallocated internal
backlog bounded by `PoolBuilder::max_inflight_product_ops`; later delivery
does not increment the backend count again. A full caller batch therefore
cannot stall internal reads or reclamation. A zero-capacity product batch is
valid and requests progress without delivery: completions are retained inside
the configured bound and delivered by later nonzero-capacity polls. Product-op
capacity remains occupied until the owned caller result is delivered, not merely
until its backend CQE is drained; after one retained result is delivered exactly
one admission slot becomes available.

`poll_wait` arms and parks outside the pool control lock, then performs
the same routing/report pass. A cloneable `PoolWakeHandle: Send + Sync`
shares a monotonic generation/event source with the wait. The owner
checks ingress, captures/arms the generation, rechecks, and parks; a
wake immediately before or during the park changes the generation and
must return the wait. Signals may coalesce because ingress and completion
queues retain the work, but the arm/park transition loses none. The same
wait returns for backend I/O completion or external Sira ingress, so the
owner neither busy-polls nor sleeps through new requests.

The shipping backend integrates a private platform wake primitive with its
actual blocking wait, so `PoolWakeHandle` interrupts the same wait hook as I/O;
no raw descriptor, unsafe handle, or backend primitive enters the public API.
Under the existing non-default `mock` feature, `dios::testing` exposes only a read-only
`ShippingWaitObservation`: fixed-at-construction counters and
`wait_until_parked`, which can neither release nor shorten the real wait. Tests
use it to distinguish signal-driven exit from deadline expiry on
`Pool<driver::Driver>` without substituting a mock wait.

The same feature exposes two distinct mock observations rather than overloading
the shipping hook. `MockWaitObservation` has exactly five actual-wait counters
(`parks_entered`, `parks_in_progress`, `parks_exited`, `wake_exits`, and
`timeout_exits`) plus `wait_until_parked`; it is read-only, non-gating, and
cannot release or shorten the mock backend's real blocking wait.
`MockPoolObservation` is an Arc-backed lifecycle handle with exactly seven
counters: registered readers, reader releases, live pending interests, pending
releases, backend operations in flight, backend completions, and quiesce calls.
It remains exact and readable after Pool drop, including capability releases
that happen later, without retaining the driver or changing lifecycle progress.

### One I/O owner and typed retirement

Pool write/fsync submissions use Pool-minted tokens and owned typed
completions. The pool holds a per-file fsync behind all previously
admitted writes for that file, because the lower driver deliberately
permits adversarial execution/CQE ordering. Unrelated files remain
independent. Terminal success or failure releases staging exactly once;
write and fsync failures remain distinct results.

Product resources are explicit and disabled by default.
`PoolBuilder::write_slots(n)` reserves exactly `n` staging slots and
`PoolBuilder::max_inflight_product_ops(n)` bounds the total admitted plus
retained write/fsync operations; both default to zero. At the latter bound a
further product submit returns `Full`. The shipping backend queue reservation
is the checked sum `max_inflight_reads + max_inflight_product_ops`; overflow is
a typed `PoolConfigError::QueueCapacityOverflow`. A zero-total configuration
may still use a private minimum backend queue depth of one, but it admits no
product operation. A shipping conformance case also exercises the positive sum:
one cold read and the full product bound admit concurrently before any poll,
the next product submit is `Full`, and all exact results drain. A `FileId`
minted by another Pool is a programmer identity
violation and panics before admission; `PoolSubmitError::ForeignPool` is
reserved for a staging slot presented to the wrong Pool.

Submit validation order is contractual. For writes, staging-slot Pool identity
is checked first and returns the unchanged slot with `ForeignPool`; next, a
foreign driver/FileId identity panics as programmer misuse; next, the live file
generation and retirement state return `StaleFile`; product capacity is checked
last and returns `Full`. Fsync follows the same order without the staging-slot
step. Thus a retired generation returns `StaleFile` even while product capacity
is saturated, and a foreign staging slot remains recoverable even when its file
identity and capacity would also fail. Driver identity remains prior even when
the foreign source has retired the `FileId`: using that retired foreign identity
on a saturated target is still a programmer panic, and write-slot unwind still
returns the target's staging capacity.

The deterministic test utility has one chronological recorder:
`MockIoEvent`, including `ReadAttempt { file, file_offset,
destination_offset, requested_len }` alongside write/fsync attempts,
completions, and closes. The frozen `read_attempts_in_order` and
`write_attempts_in_order` accessors are derived typed projections of that event
stream; no parallel attempt log exists.

`retire_file` moves the exact generational file identity through
`Live -> Retiring -> Retired`. The first transition closes all new
get/write/fsync admission immediately. Already-admitted backend ops,
pending or terminal interests, caller completion delivery, and live
guards remain valid. Retiring invalidates resident mappings, lets EBR
reclaim their frames after pins drain, delivers every admitted product
completion once, and defers fd close until backend in-flight reaches
zero. `Retired` is returned only after those obligations, reclamation,
and close finish. Repeated calls are idempotent and slot reuse never
revives an old generation.

---

## Epoch reclamation (EBR) — algorithm

Specified here so INV-1/INV-9 are testable rather than asserted.

State per pool: `global_epoch: AtomicU64`; per registered reader thread a
`local_epoch: AtomicU64` slot (u64::MAX = quiescent); an `evict_queue` of
`(frame_idx, tagged_epoch)` (fixed ring, capacity = frame count).

- Reader registration: fixed slot table sized `max_concurrent_readers`;
  registration beyond capacity fails at registration time. The returned
  lifetime-free `ReaderCtx` owns an `Arc` to the registry plus its slot
  and pool identity. It is `!Send + !Sync`, so an epoch slot belongs to
  exactly one thread, but it may outlive the Pool value and release its
  slot safely after Pool Drop. RAII deregistration remains exact; a dead
  thread's stale epoch must not stall reclamation.
- Guard create: reader publishes `local_epoch = global_epoch` (Acquire
  load, Release store), then a `SeqCst` fence before validating residency,
  BEFORE validating the frame — then re-checks the frame is still Resident
  and table-mapped; on observing Evicting or a removed mapping, abandon and
  take the miss path. Nested guards share the published epoch (per-thread
  guard count). The fence is not optional: the Acquire/Release-only variant
  was falsified by the T009 grace-period loom model — the publish store
  else sits in the store buffer past the residency read, the poller's scan
  reads a stale `u64::MAX`, and a frame is reclaimed under the live guard
  (store-buffer hazard; one half of the Dekker pair with the advance scan).
- Guard drop: when a thread's live-guard count reaches 0 it stores
  `local_epoch = u64::MAX`.
- Evict: CLOCK selects a Resident, unpinned-by-CLOCK frame → state
  Evicting, pushed to `evict_queue` tagged with the current global epoch.
  The PageTable entry is removed at this point, so no new guard can be
  created for the old contents.
- Epoch advance (poll caller only, under the AD-4 lock): a `SeqCst` fence
  before the `local_epoch` scan, then if every registered `local_epoch` is
  either `u64::MAX` or `== global_epoch`, increment `global_epoch`. This
  fence is the other half of the guard-create pair — the Acquire-load-only
  scan was falsified by the T009 grace-period loom model (store-buffer
  hazard: the scan could read a stale `u64::MAX` for a reader that had
  already published, and advance twice past a live guard).
- Reclaim (same poll pass): a queued frame with `tagged_epoch + 2 <=
  global_epoch` transitions Evicting → Free. Two advances guarantee every
  guard that could have observed the old contents has dropped.
- Retention extension: a separate `HELD`/count word is orthogonal to the packed
  frame-state/generation word; `FrameState` remains exactly `Free`, `InFlight`,
  `Resident`, or `Evicting`. Logical eviction and table removal remain allowed
  while retained. If EBR maturity finds a live retention, the frame stays
  `Evicting` with `HELD` set; physical `Free`/reuse waits for the last retained
  release and the release drain.
- Stalled reader: a thread holding a guard indefinitely stalls epoch
  advance; reclamation halts but correctness holds. The watermark
  invariant (INV-9) bounds the frames a well-behaved reader can hold;
  sira's cursors drop guards at block boundaries, so stalls are bugs and
  surface as `Busy` under load, not corruption. Containment escalation
  (recorded 2026-08-21, `resources/affinity-research-2026-08.md` §2;
  LeanStore 2018 is the closest published match and its authors
  abandoned it for this wedge, BTW 2023 — exiting to optimistic
  versioned reads, the branch this design rejects): first the
  diagnostics floor, sequenced behind the read-protocol work — the
  advance scan completes on denial instead of short-circuiting,
  recording every denying reader slot and its consecutive-denial
  count in poll-side control state under the AD-4 lock (extra scan
  cost on the deny path only; never a store on the reader slot line);
  report-only is the accepted floor (Seastar's and glommio's stall
  detectors contain nothing). Beside the counters, a park-boundary
  assert: the deadline-free wait verb the containment scope adds takes
  the caller's own `&ReaderCtx` and asserts its live-guard count is
  zero before arming. Today's `poll_wait(out, timeout)` is
  reader-agnostic and always deadline-bounded, so the assert arrives
  with that verb, not on the current signature. Deadline-bounded waits
  stay exempt and may hold guards (the INV-9 merge-miss case: stall
  bounded by the deadline, budgeted by `miss_headroom`). Escalation
  trigger: attribution records a reader slot denying consecutive
  advances past a threshold while
  the Busy rate holds above a floor, and the holder is not a fixable
  caller bug; threshold, window, and floor freeze in
  `benches/plans/stalled_reader_containment.md` when the diagnostics
  floor lands — this prose records the shape, the plan owns the
  numbers. Candidates: hazard-slots per guard (publish the pinned
  `FrameIdx`; INV-9's static peak-guard bound voids the
  unbounded-protected-set objection; a stalled reader freezes exactly
  its own frames; the fence moves from per-first-guard to per-guard
  and reclaim scans `max_readers × peak_guards` slots) or
  interval-based reclamation (per-frame birth/retire eras, one extra
  Relaxed store per pin on the owned slot line; a stalled reader
  blocks only frames born before its snapshot — cheaper reader side,
  softer bound than hazard-slots). One bench is unconditional and
  pre-trigger, riding the same post-DRP scope as the counters and the
  park assert: a fence proxy pricing hazard-slots' reader side by
  swapping the warm A/B's per-first-guard epoch publish for one slot
  store + SeqCst fence + one extra page-table probe per `get` (the
  hazard pin is lookup → publish → fence → revalidate; the warm A/B
  drops its guard every iteration, so an additive injection would
  price two fences, and on single-guard gets the unwidened arm is an
  upper bound — the with-widening arm decides), arms with and without
  the epoch-pin widening. A ratio-of-ratios over the neutral mmap arm
  (never the proxy's own unwidened arm, which shares the probe and
  would cancel it), asserted on the widened pair and leaving DIO-G1's
  2% margin intact at T014,
  makes hazard-slots eligible for owner-decided promotion to
  candidate end-state (it deletes the wedge class — no epoch
  consensus — rather than containing it): the proxy authorizes
  considering the promotion, never enacting it, and promotion is the
  one path that needs no attribution trigger. Breaking parity demotes
  hazard-slots below interval-based reclamation and leaves
  containment gated on the trigger. Either candidate amends INV-1's
  two-advance rule, INV-9's watermark and its `miss_headroom`
  derivation, and
  (hazard-slots) the warm-hit fence budget, and replaces the T009
  grace-period loom model, so either is its own scope, enacted only
  as an owner decision recorded in validation.yaml and gated on the
  stalled-reader workload (one parked guard-holder, readers cycling a
  working set larger than the pool; metric: non-stalled eviction
  throughput and Busy rate) — its gate shape, threshold, reps, and
  ratio orientation freeze in
  `benches/plans/stalled_reader_containment.md`; the test table
  registers the workload as observation until that enactment promotes
  it. Per-shard
  epoch domains are rejected: with one global page table any reader
  may pin any frame, so every grace period must consult every domain
  — the scan this design already has.
- Busy path: `get()` finding no evictable frame runs one bounded reclaim
  attempt — drain completions, advance the epoch if every reader
  permits, reclaim expired Evicting frames, one more CLOCK sweep — then
  returns `Busy`. Within the watermark this makes `Busy` reachable only
  under reclamation lag or a stalled reader (INV-9); a Busy-rate counter
  keeps the frequency observable, and `miss_headroom ≥ 3 ×
  max_inflight_reads` covers InFlight frames plus two grace periods of
  Evicting limbo (each miss admits at most one eviction).
- Shutdown: the borrowed `FrameGuard` type prevents a live guard from
  outliving Pool. `Pool::drop` quiesces in-flight ops, deregisters
  buffers, and tears down the backend exactly once. ReaderCtx and
  PendingToken may remain as small Arc-backed capability metadata and
  later release their slot/interest without retaining or touching the
  destroyed backend.

Warm-hit cost: one Acquire load + one Release store + one `SeqCst` fence
on the FIRST guard create (a nested or repeat pin under a live guard skips
the publish and the fence — it only bumps the per-thread count), one store
on last-guard drop, and the CLOCK reference bit set check-then-set (Relaxed
load; Relaxed store only when the bit is clear). Steady-state hot frames
keep the bit set, so repeat hits write nothing per-frame — no RMW ever. The
matching poll-side cost is one `SeqCst` fence per poll pass (the epoch
advance scan), off the warm-hit path. This is the answer to hot-page contention (index/bloom
frames — the "B+tree root" objection from the RavenDB mmap rebuttal);
the first-touch bit store rides in the DIO-G1 parity bench.

---

## Invariants

| ID | Invariant | Enforcement |
|----|-----------|-------------|
| INV-1 | A frame is never reused while epoch-pinned, retained, or in-flight | packed state/generation word with `FrameState` remaining exactly `Free`, `InFlight`, `Resident`, or `Evicting`; orthogonal `HELD`/count word; logical eviction remains allowed, but physical `Free`/reuse follows two epoch advances and, when retained, the last retained release plus release drain; loom tests |
| INV-2 | Zero allocation on get/submit/poll after warmup, including Pool write/fsync completion conversion, bounded overflow retention, and reuse of AD-8's fixed-capacity REMIX dedupe/order scratch | alloc-count harness (nmnm `zero_alloc.rs` pattern) in CI, with warmed O n=10 on both DIO-G10 corpus shapes |
| INV-3 | `submit` never waits on kernel completion or queue space; `poll_wait` parks outside pool/driver control and is losslessly wakeable by I/O or `PoolWakeHandle` ingress | uring: SQ-full → flush-and-retry-once then `SubmitError::Full`; eager: bounded queue → `Full`; saturated-submit, submit-while-parked, wake-before-park, and wake-during-park tests |
| INV-4 | Kernel never writes into memory Rust may free | frames registered at pool init (uring only; eager uses the same non-moving slab unregistered), deregistered only in `Pool::drop` after quiesce on both backends; ops address frames by index, not pointer-lifetime |
| INV-5 | Journal one-barrier-per-micro-commit preserved | metadata plane stays buffered+fsync (AD-3); existing journal/crash suite runs through the new write plane unmodified (DIO-G5) |
| INV-6 | Owned reader/pending capabilities cannot turn a residency borrow into transferable or `'static` bytes | `ReaderCtx: !Send + !Sync`; `PendingToken: Send + !Clone`, with `Sync` deliberately unspecified; `FrameGuard<'pool>: !Send + !Sync`, tied to Pool + destination ReaderCtx borrows; executable compile-fail/trait tests |
| INV-7 | Resident frame content is immutable until Evicting; frames hold raw granules — vNext's per-frame CRC lives above the seam in Sira and is verified for every newly fetched residency | frames read-only until Evicting; corrupt one selected frame and repeat after eviction through BlockSource (DIO-G5) |
| INV-8 | Kernel ops always drain; Pool shutdown quiesces exactly once even when lifetime-free reader/token metadata outlives the Pool value | `Pool::drop` polls until backend in-flight count is 0; PendingToken drop is waiter-interest only; drop-order observation test |
| INV-9 | Deadlock freedom: `frame_count >= (max_concurrent_readers × peak_guards_per_reader + miss_headroom).max(1) + max_retained_frames`; separately, `miss_headroom ≥ 3 × max_inflight_reads`. `peak_guards_per_reader` includes one transient vNext selected-record frame; a bounded request chunk is consumed through independent one-guard epochs and creates no hidden multi-frame pin. Within the watermark `Busy` is bounded, retriable backpressure — reachable only under reclamation lag or a stalled reader, never deadlock | peak guards per reader = static max merge fan-out `f(ln_runs [20] + levels)` plus key/column/transient selected-frame needs; compaction counts as a reader at peak fan-out; reader slots and distinct retained frames are capped by configuration; under-watermark config fails open; get() runs one bounded reclaim attempt before returning Busy; Busy-rate counter; open-fail/sequential-extent recovery tests + DIO-G7 recovery + loom. Any future shared `ReadEpoch` must amend this invariant first |
| INV-10 | Every ordinary vNext record is contained in one independently checksummed frame; every oversized record is an explicit 2..=16-frame extent | frame packer/decoder assertions (AD-5/AD-6); 4,040/4,041-byte boundary test; extent-cap rejection before writer I/O |
| INV-11 | No op references a closed fd, reused op slot, reused write buffer, or retired/reused product file generation | advanced ownership remains `close(FileHandle)` by value with deferred close; Pool closes get/write/fsync admission on `Retiring`, returns rejected `PoolWriteSlot`, drains admitted reads/writes/fsyncs and owned completions, waits for tokens/guards/EBR, and reaches `Retired` only after close; stale-generation and retirement matrix tests |

---

## Complexity

| Dimension | Before | After | Delta |
|-----------|--------|-------|-------|
| Crates | sira workspace, no IO crate | standalone `dios` crate + sira dependency on it | +1 crate (own repository), extraction done up front |
| Read backends | 1 (mmap) | 2 behind `BlockSource` (pool: Linux; mmap: macOS) | +1 seam, +1 backend |
| Write plane | `CommitFs` → std::fs | `CommitFs` → `dios` driver (O_DIRECT data via WriteArena / buffered metadata) | same trait, new impl |
| Segment format | unaligned, whole-block CRC over roughly 32 KiB packed blocks | aligned independently checksummed 4 KiB record frames, eight-frame logical groups, bounded extents, version bump | writer, REMIX ordinals, decoder, and fixtures change; old stores rebuild |
| unsafe surface | mmap map + borrow discipline | ring registration, epoch guard deref, frame state (encapsulated in `dios`) | shifted into one audited crate |
| Failure model (reads) | SIGBUS | `Err` completions | strictly better |
| Ops burden | kernel manages residency | crate manages residency (CLOCK, sizing, watermark) | new knobs: pool size ≥ watermark, GRANULE |

---

## Verification

### Test Cases

| Test | Validates | Expected |
|------|-----------|----------|
| loom: concurrent get/evict/complete on one frame | INV-1 | no interleaving reuses a pinned or in-flight frame |
| loom: guard held across evict + two epoch advances | INV-1, EBR | frame not freed until second advance after last drop |
| loom: probe/publish-epoch/evict interleave on guard create | INV-1, EBR | reader observing Evicting or removed mapping takes the miss path, never derefs |
| zero_alloc harness: warm get/miss, warmed REMIX O dedupe/order scratch, plus Pool write/fsync/report/overflow drain | INV-2, AD-8, DIO-G10 | 0 internal allocs after warmup on both backends |
| saturate SQ / eager queue, then submit | INV-3 | `SubmitErr::Full`, no block, recovery after poll |
| single-thread writer exhausts WriteArena, alloc_wait (eager backend, no external poller) | write-plane liveness | slot freed by driver pump, no self-wait, no timeout |
| drop Pool with in-flight reads | INV-4, INV-8 | quiesce completes, no UAF under miri/asan run |
| crash suite through write plane | INV-5 | torn-tail replay + manifest swap pins green |
| crash suite on mock driver with seeded completion reordering | INV-5, write-plane ordering | dependent-step sequencing survives adversarial completion schedules; any submission-order assumption fails loudly |
| compile-fail: guard slice outlives guard/Pool/ReaderCtx or crosses a thread | INV-6 | does not compile |
| trait pins: ReaderCtx !Send+!Sync; PendingToken Send+!Clone with Sync deliberately unspecified; FrameGuard !Send+!Sync | owned capability boundary | exact required traits compile; forbidden traits do not |
| drop Pool before live ReaderCtx/PendingToken metadata | INV-8, embedding ownership | backend quiesces once; later slot/interest drops release once without retaining the driver |
| move PendingToken to destination thread and resolve with its ReaderCtx | Sira routing / nmnm Gateway↔compute handoff | token resolves exact seeded page; no reader/guard crosses threads |
| IO error on cold read | pool error channel | `ReadyResult::Err`, frame freed, all singleflight waiters get the error |
| corrupt block on disk, cold + re-read after eviction | INV-7 | CRC error surfaces from BlockSource verify on each fetch |
| pool sized below watermark | INV-9 | store open fails with config error |
| watermark-sized pool, all spare frames pinned, get(absent) | INV-9, DIO-G7 | `Busy`/`Pending`, no deadlock, recovers on guard drop |
| aligned-prefix frame prospective-fill boundary | AD-5, INV-10 | prefix state resets at the frame, every admitted entry plus restart directory fits bytes 16..4092, REMIX minor is its exact byte offset, padding is zero, and the frame verifies independently |
| aligned-prefix B vs current-prefix mmap, n=1/10/256/4096 | AD-5, DIO-G10 | every B/current paired-log one-sided 95% CI upper bound is <=1.02 over at least 30 paired process reps; store and fetched bytes retained |
| targeted in-process B/current replacement preflight, n=1/10 | revision-13 evidence only | record both outcomes without promoting them to the fresh-process gate; observed nix rewarm n=1 1.1103/1.1408 FAIL and n=10 0.9609/0.9615 PASS |
| 24-byte key with 4,040- and 4,041-byte canonical values | AD-5, AD-6, INV-10 | 4,040 occupies exactly one ordinary frame; 4,041 occupies exactly two extent frames; every frame verifies independently |
| put exceeding the 16-frame extent bound | AD-6, INV-10 | `ValueTooLarge`, nothing reaches the writer |
| corrupt extent header with otherwise well-formed payload/padding | AD-5, AD-6, INV-10 | frame CRC fails before any header field controls allocation, indexing, or fetch count |
| extent with missing/truncated, duplicate, reordered, wrong-identity, wrong-total, or surplus continuation | AD-6, INV-10 | bounded decode returns typed corruption; no partial value, panic, allocation, or out-of-bound fetch |
| REMIX ordinal after eviction/reuse | AD-8, INV-1 | resolves the same file-relative frame and exact prefix-entry byte offset through a fresh `PageId`; a fixture makes row ordinal differ from byte offset and decode consumes only the stored coordinate; no stale `FrameIdx` exists in the view |
| experimental resident-file lease acquisition races retire and file-slot reuse | R7 adoption blocker, INV-1, INV-11 | lease acquisition either fails stale or linearizes before retirement; no hinted pin can authorize the reused file generation |
| experimental hinted pin races mapping removal, eviction, two epoch advances, and frame reuse | R7 adoption blocker, INV-1 | pin returns stale/fallback or a guard linearized before invalidation; it never exposes bytes from the reused frame generation |
| every reader sequentially consumes a 16-frame extent/request chunk at the exact watermark | INV-9, AD-8 | each guard/epoch drops before the next get; decode completes and one frame below the declared peak-guard formula fails open |
| 64 concurrent cold gets (Linux, O_DIRECT) | DIO-G3 | wall ≤ 2.0x p50 single-miss latency |
| pinned parity bench (Linux, 1/5 scale, artifact memo warm in both arms; no byte cache exists to bypass) | DIO-G1 | ratio CI upper bound ≤ 1.02 (non-inferiority) |
| write-plane A/B: segment flush + journal micro-commit vs retained RealFs arm (Linux, pinned host) | DIO-G8 | each ratio CI upper bound ≤ 1.02 (non-inferiority) |
| scan workload: sweep > pool size interleaved with point-gets | eviction quality (S3-FIFO escalation evidence) | per-ReaderCtx hit/eviction counters recorded in measurements.md — observation, not a gate |
| one parked guard-holder plus readers cycling a working set > pool size | stall containment escalation evidence | non-stalled eviction throughput and Busy rate recorded in measurements.md — observation, not a gate |
| pre-vNext store open | DIO-R3, AD-5 | rejected with rebuild-required format error |
| close(FileHandle) with ops in flight (both backends) | INV-11 | ops complete on the old fd; close(2) observed only after drain; no fd-number recycling race on eager |
| submit_read with a stale-generation FileId | INV-11 | rejected before an op is issued |
| submit while another thread is parked in poll_wait | INV-3 | submit returns without waiting on the poller's timeout |
| two idle product poll_wait calls use distinct short and long deadlines | INV-3 | each exact empty report lands in its own elapsed bracket (the long lower bound exceeds the short upper bound); cumulative counters are exactly two entries/exits/timeouts, zero wakes, zero in progress |
| external wake immediately before and during actual mock poll_wait park | INV-3, Sira ingress composition | wait returns promptly with zero backend completions; no missed wake or periodic polling |
| external wake during an observed shipping-backend park | INV-3, shipping Sira ingress composition | read-only test observation proves the real wait hook is in progress; wake returns promptly with zero results/CQEs, one signal exit, and zero timeout exits |
| caller completion capacity 0 then 1 with multiple write/fsync CQEs plus an internal read | truthful progress | zero-capacity poll drains/retains without delivery and capacity remains Full; report counts all drained CQEs; read readies/reclamation advances; one capacity-1 delivery releases exactly one admission slot; retained results deliver once without fictitious CQEs |
| default product capacities, exact configured saturation, and positive shipping sum | bounded product resources | defaults admit no staging/write/fsync; configured `write_slots` and `max_inflight_product_ops` bounds are exact; one cold read + the full product bound admit concurrently without polling, bound + 1 returns `Full`, exact read/write results drain; checked reservation cannot wrap |
| Pool write followed by fsync under adversarial lower-driver scheduling | durability ordering | fsync is withheld until the preceding write completes across bounded poll passes; CQE delivery order may vary; tokens/results remain exact; bytes match staging |
| cross-Pool FileId and staging slot submissions, including a source-retired foreign id on a saturated target | product identity | foreign FileId panics before source-generation or target-capacity checks; foreign slot returns `ForeignPool`; write-panic unwind drops its consumed RAII slot and the same Pool can immediately allocate/reuse that capacity |
| unified mock read/write attempt projections | deterministic observation | `MockIoEvent` contains every chronological attempt; frozen typed accessors equal projections of that one stream |
| independent injected Pool write and fsync failures | product error channel | exact owned typed failure per token; no cross-poison; staging released |
| retire file across in-flight read/write/fsync, pending/terminal token, live guard, and unpinned resident frame | INV-1, INV-8, INV-11 | new admission is typed stale immediately; old operations/capabilities finish once; frames Free and fd closed before idempotent Retired |
| reopen a retired file-table slot and use the old generation | INV-11 | old get/write/fsync identities remain typed stale; new generation works |
| negative probe on a full pool (steady-state table occupancy) | PageTable sizing | probe terminates at an Empty slot (≤ 50% load), never a full-table scan |
| repeat hits on a hot frame after first touch | CLOCK ref bit, warm-hit budget | no per-frame store once the bit is set; eviction victims reflect reference bits, not round-robin order |

---

## Design Notes

- Warm-hit cost budget vs mmap at the block-fetch layer: open-addressed
  probe + epoch publish (one load + one store) + clock-bit
  check-then-set + aligned slice — no syscall, no copy, no RMW. Both sides of the bench feed the same
  CRC+decode. If the probe shows up in the parity bench, per-worker
  last-frame memoization is the first lever; if that fails, the default
  flip is blocked and the decision returns to the scope owner (see
  Constraints).
- Copy semantics: mmap and O_DIRECT are both one-DMA, zero-CPU-copy per
  miss; buffered pread is the only per-miss-copy variant. Parity is
  decided by lookup overhead and miss scheduling, not copies.
- PageTable concurrency: warm-hit probes are lock-free and advisory.
  Realization (T008): a per-slot single-writer seqlock over atomics —
  the entry exceeds 64 bits, so a word-CAS packed cell was impossible;
  the seqlock returns non-torn snapshots (strictly stronger than the
  torn-read-safe minimum this paragraph originally assumed), with all
  writes serialized under the AD-4 lock. A stale read is safe in both
  directions. A
  false hit fails the guard-create recheck (the frame no longer maps the
  page) and falls to the miss path; a false miss (an entry mid
  backward-shift) also falls to the miss path, which re-probes
  authoritatively under the AD-4 lock before submitting — finding the
  page resident yields a Hit, never a duplicate read. Insert and delete
  run only under that lock; delete backward-shifts, so there are no
  tombstones and probe chains never degrade; 2× capacity bounds negative
  probes.
- True vmcache-style optimistic version reads were considered and set
  aside: revalidate-after-read over concurrently-evictable bytes is UB
  as a safe `&[u8]`; epoch grace periods deliver the same no-RMW warm
  path without torn-read exposure.
- Ring topology (AD-4): submit mutex + poll-caller-owned epoch advance is
  deliberately the simple v1 — misses are rare in the warm-dominated
  profile and warm hits never touch the lock. DIO-G2 is the check that
  this holds; per-worker rings are the recorded escalation and the
  designated end-state, not a redesign: the invariants constrain frame
  state and slab slots rather than ring count, and cross-thread
  readiness already flows through the frame state machine rather than
  CQs, so the pool API survives unchanged. The escalation buys the
  kernel's single-issuer fast path (`SINGLE_ISSUER` + `DEFER_TASKRUN`
  rings; concurrent `io_uring_enter` on one shared ring serializes on
  the kernel's internal lock anyway, so a lock-free shared SQ would
  only relocate the mutex). What it costs is per-ring sizing plus a
  locked-memory route chosen at open: `IORING_REGISTER_CLONE_BUFFERS`
  (kernel ≥ 6.12) registers the arena once and clones the table into
  each ring, accounted once; older kernels account the arena per ring
  against RLIMIT_MEMLOCK (headroom probed, typed error) unless the
  process holds CAP_IPC_LOCK (TigerBeetle's shipped systemd posture,
  exempt from accounting); unregistered reads remain the zero-memlock
  fallback at a per-op pinning cost needing its own gate — which is
  why the escalation waits for a profile that convicts the mutex, not
  a hunch (AD-4 trigger).
- Zero-alloc includes the drop path, unlike op-owns-buffer runtimes:
  monoio must park a dropped in-flight op's owned buffer as
  `Lifecycle::Ignored(Box::new(data))`
  (resources/monoio/monoio/src/driver/uring/lifecycle.rs) because the
  evaporating future owned it. dios ops never own buffers — PendingToken
  drop is interest-only and the frame's state machine carries it to
  Resident — so the T005 alloc-count harness asserts zero allocations on
  token drop too, not just on completion.
- kqueue appears nowhere in the crate: it is a readiness mechanism,
  readiness only serves sockets, and network ops are a non-goal. If
  socket ops ever enter (nmnm Gateway, E21), the macOS readiness choice
  is TigerBeetle's one-shot-per-op kevent (fits the intrusive completion
  slab, zero-alloc; `darwin.zig:158-182`) over mio-style persistent
  edge-triggered interest sets (resources/mio kqueue.rs:127-229), which
  pay off only at high connection counts.
- The eager backend executes the syscall outside the shared lock and
  re-acquires only to push the completion, so concurrent readers' misses
  on macOS overlap exactly as concurrent mmap faults do. macOS remains a
  correctness target with an advisory (non-gating) pool-vs-mmap
  measurement in T011; the perf gates stay Linux-only.
- Revision-10 migration is atomic at the crate API boundary. The
  executable library doctests in `src/pool/epoch.rs`, all affected tests,
  `examples/{api_fit_spike,gateway_contract,quickstart}.rs`, and
  `benches/{overlap,mmap_warm_path,mmap_tlb_pressure}.rs` move to the
  lifetime-free/Result-returning surface in T017. The obsolete
  ReaderCtx-cannot-outlive-Pool doctest is removed, not copied into inert
  integration-test prose. The guard escape and reader thread-affinity
  doctests remain executable. This document records the selected design.
  T017's A.5 gate is approved and Phase B is green; Phase C/final review is
  still pending.
