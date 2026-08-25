# Design: pinned-frame-retention

## Problem

EBR retention is reader-granular: while any guard lives, the reader's
published epoch blocks `advance_epoch`, stalling `drain_matured` for every
frame in the pool. A consumer retaining one value for a long time (sira's
`ValueGuard`) delays reclamation of frames it never touched, and
`peak_guards_per_reader` must be sized for transient AND retained guards
together.

R8 additionally demonstrated the performance target: acquire a bounded set of
pages once, precompose integer descriptors, and consume successive values with
no per-read `get` or epoch guard. Its mock-only counter-array prototype retained
8,035 of 8,059 frames without a global budget and made CLOCK skip retained
victims; it is evidence, not an implementation candidate. Exact provenance,
numbers, and disposition are in `resources/r8-resident-set.md`.

## Alternatives considered (rejected)

- Per-frame refcounts for every guard: atomic RMW on the store-only hot
  path (src/pool/epoch.rs:121-167).
- Consumer-side copy-out only: measured near-free for small values (sira
  AE009 follow-up) but scales with value size; kept as the refusal
  fallback.
- Owned `Arc` frames: frames leave pool control; allocation on an
  allocation-free path.
- New `FrameState` variant: closed assert-guarded public enum; audit of
  every equality comparison for state the retention word already carries.
- Direct `Evicting -> Free` inside drop: residency is AD-4 single-writer;
  drop holds no lock.
- AD-4 lock on promotion: sira's single-value safety fallback may promote per
  public point-read, while read-session setup promotes a bounded batch; a
  control-plane mutex there serializes readers against every poll pass —
  the pattern the consumer architecture escaped. Quantitative sketch
  (falsifiable, confirmed by the contended-promotion arm): sira's AE009
  record measures ~0.7 us/warm point read (~1.4M promotions/s/thread),
  while a poll pass holds AD-4 for O(frame-scan) work — tens of
  microseconds at bench frame counts — so one promotion convoying
  behind one poll pass adds >= 10x its entire read latency, and the
  uncontended lock/unlock pair alone (~20 ns) is ~3% of the read.
  The mint-time file-slot capture and per-slot flags below keep
  promotion lock-free.
- Budget released at last drop (revision 2): breaks the ring-capacity
  bound. Budget released only by the consumer (revision 3): leaks the
  unit on the dominant resident-cycle drop. Both superseded by the
  word-state rule below.
- R8 `ResidentSetLease` retention array: preserved the desired zero-pin read
  loop, but had no pool-wide budget and rejected retained frames after CLOCK
  selected them. At near-total retention this taxes unrelated misses with
  repeated bounded sweeps. This scope retains the consumer shape while allowing
  normal logical eviction and deferring only physical reuse.

## DRP010 baseline seam inventory

The frozen implementation baseline is
`4264896e7d2e1a2a5d6d71322a46cb7d8a3de7e7`, equal to HEAD and
`origin/main` at the T1 audit. DRP010 is complete on this identity.

- `src/pool/frames.rs:19-23,130-162,337-367` packs state in bits 0..1 and
  residency generation in bits 2..63 of one `AtomicU64`. The states remain
  `Free`, `InFlight`, `Resident`, and `Evicting`; only entry into `Resident`
  increments generation.
- The production `Evicting -> Free` site is
  `src/pool/mod.rs:1928-1935`. Its `advance_and_reclaim` callers are
  `src/pool/mod.rs:1354-1407,1530-1544,1711-1723`; the last range is the
  current `claim_frame_bounded` seam. Eviction unmapping and queue pushes are
  `src/pool/mod.rs:1524-1526,1670-1678,1735-1739`. Retirement liveness is
  `src/pool/mod.rs:1481-1488`.
- `EvictQueue::drain_matured` is
  `src/pool/epoch.rs:211-269`. The Loom model has two callers, not one:
  `src/pool/loom_model.rs:292-309` and `400-410`.
- Guards are minted at two production sites. Ordinary `pin_owned` mints at
  `src/pool/mod.rs:1578-1595`; hinted access calls `FrameGuard::new` directly
  at `src/pool/mod.rs:1144-1157`. T3 threads frame, file-slot, and `Retention`
  metadata through both sites.
- Watermark validation is `src/pool/mod.rs:559-604`, with owning arithmetic at
  `593-601`. The `Frames` Send/Sync contract is
  `src/pool/frames.rs:164-173`. The wait seams remain
  `src/product.rs:327-339` and `src/driver.rs:1547-1571`.

Retention adds a separate `AtomicU32` HELD/count word orthogonal to this packed
`AtomicU64` state/generation word. HELD never becomes a `FrameState` bit. No
baseline incompatibility blocks T1 or the separate HELD/count design.

## Selected: retain promotion

`into_retained` raises the frame's retention count while the reader's
epoch is still published, then releases the epoch guard — the frame is
continuously protected (epoch until count is visibly nonzero, retention
after).

### Reachability invariant (published-epoch form)

While a reader PUBLISHES epoch E0, `permits_advance`
(src/pool/epoch.rs:170-184) bounds the global epoch to {E0, E0+1}. A
nested guard publishes nothing (`begin_pin` publishes only the first,
src/pool/epoch.rs:121-140; pinned by tests/epoch_guards.rs:335), so the bound is
always the OUTER published epoch — covering promotion from inside a read
holding other guards, sira's call shape. Evict tags are >= E0 (tags are
the global epoch at push time under the control lock); maturity needs
`global >= tag + 2 >= E0 + 2` — forbidden while the reader publishes. A
promotable frame's evict entry is provably immature; no "reclamation
committed" refusal exists (revision 1's `Maturing` was this unreachable
arm; tests/epoch_guards.rs:96). Loom-checked incl. the nested case; the
runtime assert lives at the free site.

### Retention module and state

`src/pool/retention.rs` (module-per-concern, keeping the retain verb out
of the EBR module) owns the non-generic `Retention`: per-frame packed
`AtomicU32` words (u16 count, HELD bit), per-frame Relaxed `AtomicU64`
tag slots (mutual exclusion carried by AD-4: written only at HELD pop,
read only by the release consumer, meaningful only while HELD — a free
relaxed atomic beats `UnsafeCell` + `unsafe impl Sync` under the crate's
`undocumented_unsafe_blocks` lint), per-file-slot retiring flags
(Release/Acquire), the occupied-budget tally, by-reason refusal
counters (four, by cause), the `retained_evictions_held` diagnostic, the
release ring, `Arc<WaitState>`,
`max_retained_frames`, and the frame count. Words, tally, ring, and
flags are proof-bearing
and route through `crate::sync` (ARCH-3); the refusal counters are
diagnostics on `std` atomics with an explicit `alias_guard` allowlist
entries (`src/sync.rs` + `src/pool/alias_guard.rs` edits owned by T2;
ALLOWLIST matches per source line, so each std-atomic counter
declaration carries its own (file, marker) entry — five entries, shaped
like loom_model.rs:37). The ring-pending flag lives on WaitState in
src/product.rs — OUTSIDE the alias-guard scan directory and on std
primitives loom cannot model; recorded as deliberate: the handshake is
mutex-mediated so no atomic-ordering proof is owed, and the loom
drop-path cases stub the wake behind a named stand-in latch.
Teardown home, explicit: `Pool` gains an `impl<D> Drop` (none exists
today; E0367 forces the unbounded impl, so the drain body uses only
non-generic fields and cannot call the PoolBackend-bounded helpers)
that returns immediately under the zero-budget bypass, then takes
`control.get_mut()` — exclusive, no locking; its LockResult is taken
with the same cfg split the pool already uses for lock()
(unwrap_or_else(PoisonError::into_inner) on std so a poisoned mutex
cannot double-panic Drop into SIGABRT; expect under loom) — runs
the release drain from the Control-owned cursor, and releases each
pending unit (pending ring entries are a legal at-rest state — HELD
last-drop with no later poll); `Retention::drop`'s
`occupied_budget == 0` assert then runs as the field drops and catches
only `mem::forget`ed handles (documented hazard: a forgotten handle
wedges a unit and its file's retirement). `FrameGuard` gains the frame index,
the file-slot index, and `&'pool Retention`. Both mint sites have the page and
frame in hand: ordinary `pin_owned` at src/pool/mod.rs:1578-1595 and hinted
access's direct `FrameGuard::new` at src/pool/mod.rs:1144-1157. T3 threads all
three metadata values through both. `Pool<D>`'s only D-dependent field is the
driver (src/pool/mod.rs:744-765), so the borrow stays non-generic and
`FrameGuard`/`Get`/`ReadyResult` keep their signatures. `into_retained`
moves `bytes`, the frame index, and the backref into `RetainedFrame`.
`epoch.rs` keeps only the `into_retained` entry point.

### Promotion protocol (lock-free, bounded)

1. Read the word. `count == u16::MAX` -> refuse `Exhausted` (saturating;
   the assert covers only non-wrap).
2. `count == 0`: reserve a budget unit (`fetch_add`; if over
   `max_retained_frames`, `fetch_sub` back, re-read the word — `count >
   0` now means retry as step 3, else refuse `Exhausted`). Then CAS
   exactly `(0, HELD clear) -> (1, HELD clear)`, with a paired assert
   that promotion never observes HELD: a HELD frame is unmapped
   (`remove_shared` precedes every evict push — src/pool/mod.rs:1524-1526,
   1670-1678,1735-1739) and both guard mint paths validate a Resident mapping
   (ordinary `pin_owned`, src/pool/mod.rs:1578-1595; hinted access,
   src/pool/mod.rs:1144-1157), so no guard on a HELD frame can exist. On CAS
   loss: release the reservation, retry from step 1.
3. `count >= 1`: CAS `count -> count + 1` (no budget interaction).
4. Acquire-load the file-slot retiring flag (paired with retire_file's
   Release store). No store-buffer litmus is claimed: retirement's wait
   reads frame_pages/FrameState under AD-4 and never loads the retention
   word, so there is no second half to pair and SeqCst would buy
   nothing; the flag is an eventual-admission policy device (a promotion
   that beats the store's visibility is an existing retention, which is
   safe). If set: roll back via the WORD-STATE RULE (a plain decrement —
   see Release; if it takes the count to 0 with HELD clear, the unit is
   released; a concurrent increment having intervened, the count stays
   > 0 and the unit stays held, correctly backing the surviving
   retention). Refuse `FileRetiring`.
5. Release the epoch guard (`ManuallyDrop`).

Orderings, normative: step-2/3 CASes are strong `compare_exchange`
(AcqRel success, Acquire failure); the drop `fetch_sub` and tally
`fetch_add`/`fetch_sub` are AcqRel; ring ticket `fetch_add` AcqRel, slot
sequence Release-published, consumer Acquire; retiring flag
Release/Acquire; tag slots Relaxed (AD-4 mutual exclusion). The loop
iterates at most `max_concurrent_readers + 1` — the possible concurrent
contenders on one word (other promoters bounded by readers; the drain
cannot contend for a promotable frame by the published-epoch invariant)
— a bounded-retry POLICY per AGENTS.md, not a sufficiency proof (one
competitor can fail the same CAS repeatedly); exhaustion refuses
`Exhausted`, counted as `refused_contention`, and a contended
same-frame promotion arm records the refusal rate under sustained
same-word traffic (the single-value path can promote per point-read, so a hot frame is a
benign workload, not an anomaly).
Recorded transients, both permitted and loom-oracle-allowed: the tally
can spike past budget while two reservations are in flight (spurious
`Exhausted` for a racing UNRELATED promotion), and a same-frame second
promoter can be refused while the first promoter's reservation is
in flight. Safe refusals; copy-out fallback.

Retiring-flag semantics are POLICY (eventual admission), not safety: a
promotion whose Acquire load precedes the Release store's visibility is
an existing retention which retirement waits for; even a missed flag is
safe (evict -> HELD -> `frame_pages` keeps the file Retiring). The flag
is stored by `retire_file` before its sweep and CLEARED by
`register_file_internal` when the slot hosts a new generation (assert:
the prior entry is absent or Retired — first registrations hit the None
case throughout existing tests) — slot reuse is tested behavior
(tests/pool_retire.rs:498-507).

Slot-staleness reachability: a promotion's mint-time file-slot index
cannot be stale. A live FrameGuard keeps its frame non-Free with
frame_pages intact, which blocks progress_retirements from closing the
file (src/pool/mod.rs:1481-1488); without the close the driver cannot reissue
the slot, so the slot observed at mint is the slot at promotion, and
the generation is deliberately not re-checked. Pinned by T5's two REACHABLE cases (a pre-reopen guard blocks the very
reopen a stale-slot promotion would need, so that state is
unconstructible): a pre-retirement guard observes FileRetiring and
blocks closure until dropped; a fresh guard on the reopened generation
promotes successfully.

### Release (word-state rule)

Drop performs ONE `fetch_sub(1)` — no loop; the count is provably
nonzero while a handle exists and subtracting one cannot cross into the
HELD bit. Post-value:

- `count > 0`: done.
- `count == 0`, HELD clear: release the budget unit HERE. This is the
  dominant cycle (retain, drop while Resident); no ring, no consumer.
- `count == 0`, HELD set: WAIT-FREE ring push — `fetch_add` a ticket,
  store the frame index into slot ticket & mask, Release-publish the
  sequence; no loop, so consumer turnover cannot starve a producer and
  drop needs no failure channel. Then set the ring-pending flag and
  call `WaitState::wake_if_parked()`. The wake handshake is
  MUTEX-MEDIATED, deliberately unlike the retiring flag (which needs no
  pairing because nothing on the other side loads the word): a naked
  store-then-load pairing here is the store-buffer litmus and
  Release/Acquire does NOT close it, so both sides go through the
  generation mutex instead — wake_if_parked checks parks_in_progress
  and wakes under the mutex; WaitState::wait rechecks the flag under
  the mutex after registering its park; begin_platform_wait
  (src/product.rs:327-339) — the park entered by the shipping Linux
  io_uring path DriverCore::poll_wait_ring_for_pool
  (src/driver.rs:1547-1571), whose existing `None` branch already means do-not-park,
  so the caller needs no edit — rechecks the flag and returns None when
  set. Normative in-mutex order on BOTH park paths:
  `parks_in_progress` increments BEFORE the flag check (the platform
  park blocks in the kernel after the mutex is released, so only a
  parks-visible producer writes the eventfd); on a set flag the poller
  decrements and returns without blocking. Counter discipline on that
  return: `parks_entered` increments only AFTER the flag check, so a
  flag-set return leaves `parks_entered`/`parks_exited` untouched,
  matching the existing generation-based do-not-park early return and
  preserving the exact-counter invariant tests/pool_progress.rs pins;
  the T7 push-before-park case names its expected counter triple. The AD-4 drain
  clears the flag BEFORE scanning the ring (normative — clear-after-
  scan loses a concurrently published entry until the timeout; a
  publish racing the clear simply re-sets it). Liveness: a push either
  wakes a parked poller or precedes a pre-park recheck; the poll_wait
  timeout is the named backstop. The wake internals (Linux notify
  retries EINTR) are pre-existing machinery outside the loop-free
  claim, which covers the word and ring transitions only. Note:
  `fetch_sub` returns the PRE-decrement value; every rule here is
  stated over the computed post-value.
  The unit stays held; the consumer releases it at free.

The rule is a function of the word state, never provenance — the
retiring rollback goes through the identical decrement, which closes the
round-3 rollback-corruption interleaving (A rolls back 2 -> 1: count
stays > 0, unit stays held for B's surviving retention).

### Release ring

Fixed slot array, capacity = `max_retained_frames` rounded up to a power
of two WITH A FLOOR OF 2 (at capacity 1 the publish value for ticket t
equals the claim value for ticket t+1, so the stale-sequence breach
assert cannot discriminate and a lost entry would wedge silently; the
logical bound stays the configured value), sequence numbers per slot.
Word layout, normative: the count occupies bits 0..15 and HELD is bit
16, so `fetch_sub(1)` borrows only within the count field while the
count > 0 invariant holds (asserted). Producers are wait-free: `fetch_add` ticket
(AcqRel), slot = ticket & mask, store frame index, Release-publish the
sequence. Normative sequence encoding (Vyukov-shape, so an implementer
needs no further decisions): slot i's sequence initializes to i; the
producer holding ticket t (slot t & mask) asserts the observed sequence
equals t (a stale sequence is the capacity-proof breach assert), stores
the frame index, and Release-publishes sequence t + 1; the consumer at
head h (slot h & mask) treats sequence == h + 1 (Acquire) as published,
copies the entry out, Release-stores sequence h + capacity (re-arming
the slot for ticket h + capacity), and advances h. Tickets are u64 and
never wrap (asserted; 2^63 HELD episodes are not reachable). The single
consumer (AD-4 holder) pops in ticket order; its head cursor is a plain
field in AD-4-owned Control — the ring type's pop signature is
`pop(&self, cursor: &mut u64) -> Option<Entry>` with the AD-4 holder
owning the cursor, so T2 unit-tests the ring self-contained; on
reaching an unpublished sequence the consumer STOPS the pass (never
spins — the producer re-signals via the pending flag); and it COPIES
THE ENTRY OUT BEFORE releasing that entry's budget unit — the sequencing the
capacity proof depends on, carried by the tally's AcqRel RMW chain
(a turnover producer's ticket for that slot cannot be issued until the
unit release it needs is visible). Capacity proof, stated over COMMITTED units (not the tally,
which is allowed to overshoot transiently during in-flight
reservations): a unit is committed when its reservation's post-
`fetch_add` value was <= `max_retained_frames` AND its `0 -> 1` CAS won
— every over-budget reservation rolls back before committing, so
committed <= `max_retained_frames` at all times, and the tally never
drops below the committed count (rollbacks subtract only their own
reservation). An entry is pushed only at `(0, HELD)`, whose unit is
committed and released only by the consumer's pop; a `(0, HELD)` frame
is unmapped and count-0, so no path re-pushes it. Hence outstanding
entries <= committed units <= capacity, and ticket t is issued only
after ticket t-cap's unit was released at its pop — slot t & mask is
free at claim time. In-flight reservations produce no push (a push
requires a committed `(0, HELD)` word). Asserted: a claimed slot with a
stale sequence is a capacity-proof breach. Loom: two producers racing
the consumer across turnover.

### Reclaim protocol

- `drain_matured` (AD-4) pops a matured entry; CAS loop bounded by the
  strictly-decreasing count (each failure is a completed decrement; at
  most the observed count iterations, asserted). BOTH drain properties
  rest on the published-epoch invariant, restated here: no promotion can
  target a matured frame (a promoter's reader publishes E0 <= tag, so
  the entry cannot be matured) — this is what bounds the loop AND what
  makes the CAS-free `count == 0` path safe against a concurrent
  `(0) -> (1)` promotion; asserted at the site, loom case racing a
  promotion against the count==0 reclaim. `count == 0` -> plain
  pre-existing reclaim (advance to Free, clear `frame_pages`; NO tally
  interaction). `count > 0` -> set HELD, write the entry's tag to the
  tag slot, LEAVE `frame_pages` intact (`progress_retirements` tests
  `page.is_some_and(.. file ..) && state != Free`,
  src/pool/mod.rs:1481-1488). New signature: the closure reports
  `FrameOutcome {Freed, Held}` —
  `FnMut(ReadFrameIdx, u64) -> FrameOutcome` — and the return value is
  the Freed count. The current queue seam is
  `src/pool/epoch.rs:211-269`; T4 updates the production caller at
  `src/pool/mod.rs:1928-1935` and both Loom callers at
  `src/pool/loom_model.rs:292-309,400-410`.
- Release drain: FIRST step of `advance_and_reclaim`
  (`src/pool/mod.rs:1928-1935`), before `drain_matured`. Every caller reaches
  that single site (`src/pool/mod.rs:1354-1407,1530-1544,1711-1723`), including
  `claim_frame_bounded` at `src/pool/mod.rs:1711-1723`. Per frame: assert
  `FrameState::Evicting`, `tag + 2 <= global` (against the epoch read at
  the START of the pass — valid because the tag matured in an earlier
  pass; the operand is named to prevent a spurious-failure
  implementation), `count == 0`, HELD set; then clear HELD, advance
  `Evicting -> Free`, clear `frame_pages[frame]`, release the budget
  unit, count in `reclaimed_frames`. A release free is genuine poll
  progress (a frame became claimable); `poll_wait`'s early return on it
  is intended.

### Direct free (no second grace)

Premises: HELD is set only on a matured entry (maturity proves every
pre-eviction guard dropped), and `remove_shared` precedes every evict
push — `retire_file_frames` at src/pool/mod.rs:1524-1526, the test seam at
src/pool/mod.rs:1670-1678, and the production CLOCK path
`evict_one_victim` at src/pool/mod.rs:1735-1739, whose conditional
`frame_pages` lookup T4 hardens to an asserted expect. No guard on a released
frame can exist; the consumer frees directly. Deref-over-Evicting is already
admitted by the Frames Send/Sync contract (src/pool/frames.rs:164-173).

### Capacity arithmetic (INV-9 delta and representability)

Representability validation at build (typed open-time errors, public
variant `PoolConfigError::RetentionUnrepresentable { requested: u32,
limit: u32 }`, exported and pinned in tests/public_api.rs): the
power-of-two ring capacity must not overflow u32; `max_retained_frames`
plus the peak transient reservation overshoot must fit the tally width;
`max_concurrent_readers + 1` must fit the retry counter width. The
peak transient reservation overshoot is `max_concurrent_readers` (one
in-flight reservation per registered reader; promotion is sequential
within a thread). No fourth check is needed for the augmented INV-9
watermark: `PoolBuilder::validate` computes in u64 and clamps only the
REPORTED value (src/pool/mod.rs:559-604, with the arithmetic at 593-601) —
semantics that extend to the augmented sum unchanged.


    frame_count >= (max_concurrent_readers x peak_guards_per_reader
                    + miss_headroom).max(1) + max_retained_frames

(Owning formula: `PoolBuilder::validate`, src/pool/mod.rs:559-604; arithmetic
at 593-601 — no inflight
term; `miss_headroom >= 3 x max_inflight_reads` is separate and
untouched.) The tally is OCCUPIED BUDGET: frames retained or pending
release. `PoolConfigError::BelowWatermark` keeps its shape with the
augmented watermark.

This arithmetic is necessary but not sufficient for the Linux shipping
backend. Its registered frame arena is charged to `RLIMIT_MEMLOCK` on the
pinned host (Linux 6.6.64). The measured 8 MiB soft and hard limit admits
2,048 4 KiB frames in raw arithmetic, but the causal probe measured failure
already at 1,984 frames (7.75 MiB — 256 KiB below the limit); the extra
charge is not yet attributed (candidates in
`resources/r8-resident-set.md`). The R8 workload retains 8,035 pages
(31.4 MiB) and used an 8,059-frame pool (31.5 MiB full arena); the exact
shipping rerun cannot be constructed under current host limits. The owner
decision is recorded (scope.md, revision 11): the shipping baseline
operates under the stock unprivileged 8 MiB limit (systemd
`DefaultLimitMEMLOCK`; the kernel's own default is 64 KiB and is NOT a
supported target — it would exclude every registered posture).
Consequences:

- Baseline: the unregistered/probed arena posture delivered by
  `arena-modernization:AM4` — probed registration with unregistered
  READ/READV fallback, not delivered here. It surrenders the pinned
  no-fault guarantee (unpinned pages may minor- or MAJOR-fault under
  pressure, so the bench's timed-region fault counters become
  load-bearing warmth evidence rather than a hygiene assert). The change
  touches the miss/I-O path and the pinning guarantee only; the retained
  hot loop performs no I/O and is backend-independent. The TLB lever
  survives: `MADV_HUGEPAGE` on the anonymous arena carries no memlock
  charge (the 31.4 MiB retained set overruns 4 KiB-page L2 dTLB reach on
  the pinned host but fits ~16 2 MiB entries, given a 2 MiB-aligned
  arena base and an admitting THP policy — both recorded per arm). The
  lever is asymmetric only against an un-madvised or ext4 file-mapping
  baseline: on XFS >= 5.18 a 2 MiB-file-aligned madvised file mapping
  MAY itself obtain PMD folios (unverified on the 6.6.64 host; each
  arm's recorded `AnonHugePages`/`FilePmdMapped` settles it).
- Opt-in optimization knob, never a baseline requirement: registered arena
  plus a raised host limit, restoring the pinned no-fault guarantee and
  `READ_FIXED`'s miss-path savings. Sparse registration
  (`IORING_REGISTER_BUFFERS2`/`BUFFERS_UPDATE`, kernel 5.13+) is the
  intermediate posture: a hot sub-arena registered within the default
  8 MiB budget, extended without quiescing. Registration order matters
  under THP: pinning freezes page size (khugepaged refuses pinned pages),
  so the arena is populated as hugepages before any registration. The
  arena-modernization scope owns these mechanisms; this scope consumes
  the active posture.
- A host-capacity retained set remains a labelled scaled capacity test; it
  changes the TLB regime and its curve does not extrapolate to R8 scale.

MockDriver remains useful for deterministic safety schedules but cannot close
the shipping-backend performance gate, which now waits on the
unregistered-backend task.

### Zero-budget bypass

`max_retained_frames == 0` (default): promotion refuses before touching
any word, `drain_matured` skips the retention CAS (behaviorally
identical — no word is ever nonzero), no ring or slot storage is
constructed. Parity is a gated bench arm.

## Consumer mapping (informative)

This section fixes the intended call shape and benchmark model only. The
production Sira owner table, descriptors, and fallback are outside this Dios
scope and require their own cross-repository task after this API is proven.

sira's performance path owns a fixed-capacity `Vec<RetainedFrame<'read>>` in a
bounded read session. Setup gets and promotes each distinct page once, aborts
the whole setup on refusal by dropping the partial vector, and then freezes both
the owner vector and a precomposed integer descriptor sequence. Each successive
read indexes the owner vector and borrows the selected bytes directly; it does
not call `Pool::get`, promote, touch CLOCK, or mutate retention state. The
phantom `'read` on sira's public guard/session surface is the required lifetime;
shared pool and `ReaderCtx` borrows are compatible with its Connection-owned
ReadCore.

The separate single-value safety path may still put one `RetainedFrame` inside
`RetainedValue`; refusal maps to its existing owned-copy or guarded fallback.
That per-point promotion/drop path is not used to claim the R8 performance
result. Promotion remains one or two CAS plus one Acquire flag load and release
one `fetch_sub`, but both are amortized outside the successive-read loop for the
performance path.

Performance ceiling: once promotion and composition sit outside the loop, a
successive read is an index plus a bounded borrow — the same virtual-memory
load a warm mmap dereference performs. The design targets parity with the
warm mapped read (R8 measured +5 ns against the locator, a tie-gate
residual); its wins over the guarded path come from deleted per-read
software work, and any win over mmap itself must come from the memory
architecture recorded under Capacity, not from further access-path work.

## Complexity

Per frame: one `AtomicU32` word + one `AtomicU64` tag slot. Per file
slot: one atomic flag. Pool: ring (`max_retained_frames` slots), tally,
four by-reason refusal counters plus one `retained_evictions_held`
diagnostic — all at build; nothing grows after
warmup. Promotion: 1-2 CAS + 1 Acquire flag load, lock-free, bounded.
Release: one `fetch_sub`; the HELD path adds a wait-free ring push, a
ring-pending flag store, and a
parked-gated wake only on the HELD path. Poll: one bounded ring drain at
the head of `advance_and_reclaim`, absent at zero budget.
