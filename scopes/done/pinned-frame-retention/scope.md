---
created: 2026-08-17
status: done
issue_type: Feature
revision: 14
---

# Scope: pinned-frame-retention

Adds retained frame access beside the transient epoch guard: promoting a live
`FrameGuard` yields a `RetainedFrame` that holds a per-frame retention count,
releases the reader's epoch, and keeps its frame resident until dropped.

Revision 14 adds the four missing Sira integration obligations: T10 enforces
`peak_guards_per_reader`; T11 adds new-artifact creation, configurable file
capacity, typed capacity refusal, and fallible preallocation; T12 exposes the
negotiated direct-I/O mode and durability boundary; T13 integrates the reviewed
T017 lifecycle-progress hardening from `d4203d95`. T10–T13 require their own
implementation and review gates.

Retention stops publishing the reader's epoch, and `peak_guards_per_reader`
shrinks to bounding transient guards only. "Pin" remains the EBR epoch verb;
retention uses the `retain` vocabulary, and the subsystem lives in its own
module `src/pool/retention.rs` per the repo's module-per-concern layout.

Motivating performance consumer: sira's R8 read session. The consumer acquires
and promotes each distinct page once, freezes a bounded owner table plus a
precomposed sequence of integer descriptors, performs successive reads without
`Pool::get` or a per-read guard, and drops the retained owners with the session.
The Dios primitive remains per-frame; the consumer composes the set without a
second Dios lease abstraction. A single-value retained guard remains a valid
safety use case, but per-point promote/drop is not the performance shape or the
retained-access benchmark. The evidence, formal FAIL, rejected R8
implementation, and selected-design consequence are recorded in
`resources/r8-resident-set.md`.

The exact R8 scale also collides with the shipping backend's measured buffer-
registration limit on `nix`: 8,035 retained 4 KiB pages require 32,911,360
bytes (31.4 MiB), while both soft and hard `RLIMIT_MEMLOCK` are 8 MiB — the
stock unprivileged limit as shipped by systemd (`DefaultLimitMEMLOCK`); the
kernel's own default is 64 KiB, and supporting that floor would exclude every
registered posture and is NOT required. OWNER DECISION (recorded 2026-08-20,
revision 11): the shipping baseline must operate under that stock 8 MiB
unprivileged limit; raising it is demoted to an opt-in optimization knob and
can never be a baseline requirement. That knob is a higher `LimitMEMLOCK`, or
`CAP_IPC_LOCK`, which exempts a ring's registered buffers from
`RLIMIT_MEMLOCK` accounting outright rather than raising the ceiling (the
exemption is fixed when the ring is created);
`AmbientCapabilities=CAP_IPC_LOCK` is the posture in TigerBeetle's shipped
systemd unit. Neither is a deployment dios requires:
`scopes/active/dios-v1/resources/affinity-research-2026-08.md` §1. The default-compatible arm is the
probed-registration arena posture (sparse or unregistered fallback) delivered
by `arena-modernization:AM4`, which this scope does not implement — the
exact-R8 shipping validation is blocked on that task, no longer on a choice.
A smaller retained-set workload remains a labelled scaled experiment,
non-equivalent to R8.

Implementation is serialized after
`dios-r1-r7-read-performance:DRP010`. That prerequisite supplies the selected
frame-state/reclaim baseline; it supplies no R8 behavior. Downstream, the
draft `arena-modernization` scope serializes after this scope's T7 and its
AM4 discharges T8b's arena-posture dependency (its own review gate is still
pending — recorded on the dependency entry). Before retention code
starts, T1 replays scope review against the pinned DRP010 branch and records how
the orthogonal HELD/count word interoperates with the selected packed
frame-state/generation and reclaim baseline. The two scopes therefore do
not mutate `frames.rs`, `pool/mod.rs`, `sync.rs`, Loom seams, or public exports
concurrently.

## Requirements

### Promotion (lock-free)

- `FrameGuard::into_retained(self)` returns
  `Result<RetainedFrame<'pool>, RetainRefused<'pool>>`. The retention count
  is raised while the guard's epoch is still published; no unprotected
  window. No lock is acquired. To make that implementable, `FrameGuard`
  carries: `bytes`, the reader slot, the frame index, the frame's file-slot
  index (captured at mint — `pin_owned` has the page in hand), and
  `&'pool Retention`; `into_retained` moves `bytes`, the frame index, and
  the backref into `RetainedFrame` (its `Deref` serves the moved `bytes`).
- Promotion loop (normative spec in design.md, with every memory
  ordering spelled there): saturating refusal at the count ceiling;
  budget reserve/rollback/retry at `0 -> 1` with the CAS written exactly
  as `(0, HELD clear) -> (1, HELD clear)` (strong `compare_exchange`,
  AcqRel success / Acquire failure) and a paired assert that promotion
  never observes HELD; iteration bound `max_concurrent_readers + 1` as
  a bounded-retry POLICY per AGENTS.md — NOT a sufficiency proof (one
  competing promoter can fail the same CAS repeatedly on a hot frame) —
  with `refused_contention` as its observability and a contended
  same-frame promotion arm carrying a NUMERIC GATE (pinned workload,
  one-sided 95% CI upper bound on the refusal rate, threshold and
  escalation lever pre-recorded in the T1 plan — a rate-only recording
  would let a 100% refusal rate pass); the deterministic
  retry-exhaustion path is a separate unit test; bound exhaustion
  refuses `Exhausted`.
- Refusal reasons, exhaustively: `Exhausted` (budget full, count ceiling,
  or promotion retry bound) and `FileRetiring`. `RetainRefused` carries the
  still-live guard back (`ManuallyDrop` decomposition).
- Same-frame spurious refusal is PERMITTED and documented: at the budget
  boundary a racing second promoter of the same frame may observe
  `Exhausted` while the first promoter's reservation is in flight. Safe
  refusal, copy-out fallback; the loom oracle allows it.
- `RetainedFrame<'pool>` keeps shared borrows of pool and originating
  `ReaderCtx` (all read ops take `&self`); pinned by a ReaderCtx-lifetime
  compile_fail case.

### Bounded read-session composition contract

- This contract shapes the Dios API and primary benchmark; production Sira
  owner-table/descriptor/fallback implementation is a separately reviewed
  cross-repository task and is not delivered or claimed by this scope.
- Dios exposes only the per-frame promotion primitive. A consumer builds a
  fixed-capacity owner table of `RetainedFrame`s during session setup. Setup is
  all-or-nothing: on any refusal it drops every already-promoted handle before
  falling back to guarded reads or copy-out. No partially built table is
  published to the read loop.
- After setup, the consumer freezes the owner table and a precomposed sequence
  of integer `(retained_index, byte bounds)` descriptors. A descriptor carries
  no address or persistent pool-frame identity. Bounds are validated during
  setup; the timed loop borrows bytes through the indexed `RetainedFrame` and
  cannot outlive the session owner.
- Successive reads perform no `Pool::get`, epoch pin, residency-stamp
  validation, CLOCK touch, retention atomic, allocation, or copy. Promotion
  and final handle drops are outside that loop. Ordinary guarded access remains
  the refusal and non-session fallback.
- Logical eviction is permitted while a handle lives: the lookup mapping may
  be removed and a later ordinary `get` may load another copy. The retained
  handle remains byte-stable; physical frame reuse waits until release. This is
  the selected design's deliberate alternative to R8's repeated retained-
  victim skipping.

### Release (loop-free) and budget rule

- `RetainedFrame::drop` performs ONE `fetch_sub(1)` (AcqRel) on the word —
  no CAS loop (the count is provably nonzero while a handle exists;
  subtracting one cannot touch the HELD bit). The post-value decides:
  - `count > 0`: done.
  - `count == 0`, HELD clear: this drop RELEASES THE BUDGET UNIT. This is
    the dominant cycle (retain, drop while Resident) and it must recycle
    budget with no ring or consumer involvement.
  - `count == 0`, HELD set: wait-free ring push (one `fetch_add` ticket,
    one slot store, one Release sequence publish — no loop, no CAS; slot
    availability is guaranteed by the committed-units capacity proof),
    then set the ring-pending flag on `WaitState` (Release) and wake iff
    `parks_in_progress > 0`. The budget unit stays held — the release
    CONSUMER releases it at free, keeping every live ring entry backed by
    a held unit.
  - Wake protocol, MUTEX-MEDIATED on both sides (no atomic Dekker to
    prove): the producer sets the ring-pending flag and calls
    `WaitState::wake_if_parked()`, which checks `parks_in_progress` and
    wakes UNDER the generation mutex; BOTH park paths recheck the flag
    under that mutex before blocking — `WaitState::wait` (eager/mock)
    AND `begin_platform_wait` (the shipping Linux io_uring path, which
    returns its existing "do not park" `None` when the flag is set).
    The AD-4 drain clears the flag BEFORE scanning the ring, so a
    publish racing the clear re-sets it and no entry is lost. Liveness:
    a push either wakes a parked poller or precedes a pre-park recheck;
    the `poll_wait` timeout is the named final backstop. The wake path
    reuses existing machinery whose Linux notify retries EINTR — the
    loop-free claim covers the retention word and ring transitions, not
    the pre-existing wake internals. (`fetch_sub` returns the PRE-value;
    the rules above are stated over the computed post-value.)
    Acceptance covers push-before-park and push-during-park on the
    eager path, a Linux-lane case through `begin_platform_wait`, and
    the publish-between-clear-and-scan interleaving.
- The budget rule is a function of the WORD STATE, never of provenance:
  whoever takes `count` to 0 (user drop or retiring rollback) follows the
  identical rule above. `drain_matured`'s `count == 0` path is the plain
  pre-existing reclaim (advance to Free, clear `frame_pages`) with NO
  tally interaction; the phrase "full reclaim step" refers exclusively to
  the release consumer's action.

### Transient path and zero-budget fast path

- `begin_pin`/`release_guard` gain no RMW and no shared-cacheline traffic.
- `max_retained_frames == 0` (default) bypasses the protocol wholesale —
  promotion refuses before touching the word, `drain_matured` skips the
  retention CAS, no ring exists (nothing zero-capacity is constructed).
  Bypass parity is a gated bench arm.

### Retention state and the reclaim protocol

- `Retention` (non-generic, in `src/pool/retention.rs`): per-frame packed
  `AtomicU32` words (u16 count + HELD bit), per-frame `AtomicU64` tag
  slots (Relaxed; correctness carried by AD-4 mutual exclusion — written
  only at HELD pop, read only by the release consumer, meaningful only
  while HELD is set, asserted), per-file-slot atomic retiring flags, the
  occupied-budget tally, by-reason refusal counters, the
  `retained_evictions_held` diagnostic, and the release ring, plus
  `Arc<WaitState>` (wake latch + parks accessor —
  `WaitState` gains a `pub(crate)` parks accessor and the ring-pending
  flag consulted by its pre-park recheck; `src/product.rs` changes),
  `max_retained_frames`, and the frame count. The zero-budget bypass
  elides the ring and the word/tag/flag arrays. Proof-bearing atomics
  (words, tally, ring, flags) route through `crate::sync` per ARCH-3;
  the by-reason refusal counters and `retained_evictions_held` are
  classified diagnostics and carry explicit `alias_guard` allowlist
  entries — one per counter declaration; ALLOWLIST matches per source
  line (`src/sync.rs`, `src/pool/alias_guard.rs` in T2). `FrameGuard`/`Get`/`ReadyResult`
  stay non-generic (pinned in `tests/public_api.rs`). `FrameState` is
  UNCHANGED. Teardown: `Pool` owns everything exclusively at drop, so
  it runs a FINAL RELEASE DRAIN (releasing units for pending ring
  entries — a legal state, e.g. a HELD last-drop with no later poll)
  and THEN asserts `occupied_budget == 0`, which at that point can only
  catch a `mem::forget`ed handle — the documented hazard (a forgotten
  handle wedges a unit and its file's retirement). Acceptance: drop the
  pool immediately after a HELD last-drop.
- Release ring, fully specified: a fixed array of slots (capacity =
  `max_retained_frames` rounded up to a power of two, FLOOR 2 — at
  capacity 1 the breach assert cannot discriminate; the LOGICAL bound
  stays `max_retained_frames`) with sequence numbers. Word layout
  normative: count in bits 0..15, HELD at bit 16 (what makes the
  loop-free `fetch_sub` borrow-safe; asserted).
  Producers are WAIT-FREE: `fetch_add` a ticket (AcqRel), slot =
  ticket & mask, store the frame index, publish the sequence (Release) —
  no loop, so no producer can be starved by consumer turnover. The
  single consumer (AD-4 holder) pops in ticket order (Acquire). Slot
  availability at claim is guaranteed by the committed-units proof
  (ticket t is issued only after ticket t-cap's unit was released at its
  pop) and asserted (negative space: a claimed slot with a stale
  sequence is a capacity-proof breach). Loom case: two producers racing
  the consumer across turnover.
- `drain_matured` (AD-4) pops a matured entry and CAS-loops on the word —
  bounded by the strictly-decreasing count (each failure is a completed
  decrement — no promotion can target a matured frame, the
  published-epoch invariant restated at this site; asserted):
  `count == 0` -> plain reclaim; `count > 0` -> set HELD, write the
  entry's tag into the tag slot, LEAVE `frame_pages` INTACT (the mapping
  entry keeps `progress_retirements`' liveness test truthful). The entry
  leaves the ring either way. `drain_matured`'s return value becomes
  frames-reaching-Free (HELD pops excluded); its one production caller
  and the loom-model caller change accordingly (T4).
- The release drain is the FIRST step of `advance_and_reclaim` (before
  `drain_matured`), under AD-4 — the single drain site, reached by every
  HELD-setting path including `claim_frame_bounded`. Per drained frame:
  assert the four conditions (`Evicting`; tag matured against the global
  epoch read at the start of the pass; `count == 0`; HELD set), then
  clear HELD, advance `Evicting -> Free`, clear `frame_pages[frame]`,
  release the budget unit, count it in `reclaimed_frames`. A release-
  drain free counts as poll progress (it IS progress — a frame became
  claimable); `poll_wait` returning early on it is intended.

### Reclamation accounting

- `PollReport::reclaimed_frames` counts exactly the frames that reached
  `Free` in the pass. Test pins reported == frames reaching Free.

### Retirement

- `retire_file` stores the file-slot's retiring flag (Release, before
  the sweep); promotion loads it Acquire after committing the count. The
  flag is an EVENTUAL-ADMISSION POLICY device, not a safety device: a
  promotion whose load precedes the store's visibility behaves as an
  existing retention which retirement waits for, and that is safe
  (evict -> HELD -> `frame_pages` keeps the file Retiring). No
  store-buffer pairing is claimed — retirement's wait reads
  `frame_pages`/`FrameState` under AD-4, never the retention word, so
  there is no second litmus half and SeqCst would buy nothing. The flag
  store/clear wiring and behavioral retirement tests land together in T5,
  after the core promotion/reclaim primitive is green.
- Slot-staleness reachability: a promotion's captured file-slot index
  cannot be stale. A live `FrameGuard` keeps its frame non-Free with
  `frame_pages` intact, which blocks `progress_retirements` from closing
  the file (mod.rs:1187-1192), and without the close the driver cannot
  reissue the slot — so the slot observed at mint is the slot at
  promotion, and the generation is deliberately not re-checked. Pinned
  by T5's two reachable cases (a pre-reopen guard blocks the very reopen
  a stale-slot promotion would need): a pre-retirement guard observes
  FileRetiring and blocks closure until dropped; a fresh guard on the
  reopened generation promotes successfully.
- The flag is CLEARED when the slot is reused: `register_file_internal`
  resets `flags[slot]` when publishing a new generation, with an assert
  that the prior entry is ABSENT OR `Retired` (first registrations hit
  the `None` case throughout the existing tests). Acceptance case: retire a
  file with a retained frame, release, close, reopen into the same slot,
  and assert promotion on the new generation succeeds.
- Retirement completion waits on existing retentions (retained frames
  keep `frame_pages` + non-Free state until released); observable via
  `RetireStatus` and retention stats.

### Capacity

- `PoolBuilder::max_retained_frames(u32)`, default 0. INV-9 delta:
  `frame_count >= (max_concurrent_readers x peak_guards_per_reader
  + miss_headroom).max(1) + max_retained_frames`;
  `PoolConfigError::BelowWatermark` keeps its shape, reporting the
  augmented watermark, plus representability validation (normative
  checks and the public `PoolConfigError::RetentionUnrepresentable
  { requested, limit }` variant in design.md Capacity, pinned in
  tests/public_api.rs). Budget denominates DISTINCT frames; the tally is
  OCCUPIED BUDGET — frames retained or pending release, transiently
  including in-flight reservations (documented on the public stat).
- The retention watermark proves pool-internal capacity; it cannot make Linux
  registered-buffer memory available. Pinned host: Linux 6.6.64 (see
  `resources/r8-resident-set.md`); every kernel-version qualifier below
  resolves against that floor. The shipping `READ_FIXED` arena is
  `RLIMIT_MEMLOCK`-charged and the 8 MiB hard limit puts a raw ceiling at
  2,048 4 KiB frames. In the causal probe, 1,024 frames succeeded while
  1,984 (7.75 MiB — 256 KiB BELOW the limit) failed with `ENOMEM`; the
  extra charge is not yet attributed (candidates and the rerun observable
  are recorded in `resources/r8-resident-set.md`). 8,035 retained pages
  alone need 31.4 MiB. Under the recorded default-memlock baseline, exact
  R8-scale shipping evidence requires the `arena-modernization:AM4` posture
  (registered `READ_FIXED` at that scale exists only behind the opt-in
  raised-limit knob); reducing the retained set changes the workload and
  must be reported as a scaled capacity experiment, not an R8 reproduction.
- What each memory-architecture property costs, under the recorded
  baseline: the pinned no-fault guarantee and `READ_FIXED`'s miss-path
  savings are what registration (and hence memlock) buys — under the
  default-compatible unregistered arena a retained page may minor-fault
  under memory pressure, so the bench's timed-region fault counter is
  load-bearing warmth evidence there. A third posture exists between
  all-registered and unregistered: sparse registration
  (`IORING_REGISTER_BUFFERS2` + `IORING_REGISTER_BUFFERS_UPDATE`,
  kernel 5.13+, within the 6.6.64 floor) — a table sized up front (`nr`
  fixed at registration; UPDATE fills or replaces slots without
  quiescing, but growing the table still requires re-registration)
  pinning a hot sub-arena within the 8 MiB budget. Over a THP-backed
  arena the memlock charge quantises to 2 MiB per touched compound page,
  so that budget admits at most FOUR hugepage granules — the hot
  sub-arena is sized in granules, not frames. Measured registration
  value for context: +11% on a YCSB-style buffer manager and 4-6% on
  PostgreSQL 18 ("High-Performance DBMSs with io_uring: When and How to
  Use It", Jasny et al., VLDB 19(9) 2026, arXiv 2512.04859) — real, not
  existential, consistent with treating registration as a knob. The
  probed try-register-warn-fallback pattern is established practice
  (Netty io_uring, glommio). Mechanism ownership: the arena-modernization
  scope delivers probed/sparse registration and the unregistered
  fallback. Arena posture is part of ARM IDENTITY, not ambient state:
  the exact-R8 shipping arm runs the Unregistered posture (the recorded
  baseline); a sparse- or fully-registered run is a separately labelled
  arm, never a substitute — a sparse arena is mixed pinned/unpinned and
  its fault counts are not comparable. T1 records the posture as a
  pinned precondition of each arm. The TLB lever survives the baseline:
  hugepage backing is an anonymous-arena property (`MADV_HUGEPAGE`, no
  memlock charge) on either posture — at R8 scale the 31.4 MiB retained
  set overruns the pinned host's L2 dTLB reach at 4 KiB pages (~8 MiB
  on the Threadripper 3970X) but fits ~16 hugepage entries, PROVIDED the
  arena base is 2 MiB-aligned (an unaligned base yields 4 KiB-mapped
  tails) and the host's THP policy admits it — arms record
  `/sys/kernel/mm/transparent_hugepage/{enabled,defrag}` and assert the
  expected `AnonHugePages` before timing and before any registration.
  The lever is asymmetric only against an un-madvised or ext4
  file-mapping baseline (ext4 large folios land in 6.16 — unreachable on
  this host): on XFS >= 5.18 a 2 MiB-file-aligned `MADV_HUGEPAGE`d file
  mapping MAY obtain PMD folios (unverified on 6.6.64 — the recorded
  `FilePmdMapped` reading, not the version number, settles each arm), so
  bench arms record both sides' page-size state
  (`AnonHugePages`/`FilePmdMapped`). Registration is a THP freeze — a
  pinned 4 KiB page can never be collapsed (khugepaged refuses pinned
  pages) — so any registered arm populates the arena as hugepages BEFORE
  registering. A scaled retained set changes the TLB regime, so its
  curve does not extrapolate to R8 scale.

### Bounds and semantics (v1)

- `RetainedFrame` is `!Send`, `!Sync`, not `Clone` — pinned via
  compile_fail (pool lifetime, ReaderCtx lifetime, `!Send`),
  `assert_not_sync!`, `assert_not_clone!` in the existing harness.
- Retained bytes are a point-in-time snapshot (documented; exact for
  immutable-file consumers).
- `RetainedFrame::Debug` prints length only.

## API Contract

```rust
impl<'pool> FrameGuard<'pool> {
    pub fn into_retained(self) -> Result<RetainedFrame<'pool>, RetainRefused<'pool>>;
}

pub struct RetainedFrame<'pool> { /* bytes + frame index + &'pool Retention; !Send + !Sync + !Clone */ }
impl Deref for RetainedFrame<'_> { type Target = [u8]; }
impl fmt::Debug for RetainedFrame<'_> { /* length only */ }
impl Drop for RetainedFrame<'_> { /* one fetch_sub; word-state rule decides budget/ring/wake */ }

pub struct RetainRefused<'pool> {
    pub guard: FrameGuard<'pool>,
    pub reason: RetainRefusedReason, // Exhausted | FileRetiring
}

pub enum RetainRefusedReason { Exhausted, FileRetiring }   // root-exported with RetainedFrame/RetainRefused

pub struct RetentionStats {
    // Committed retained/pending-release frames PLUS in-flight
    // reservations: a concurrent snapshot may transiently exceed
    // max_retained_frames (pinned by the stats acceptance test).
    pub occupied_budget: u32,
    pub refused_budget: u64,         // budget full
    pub refused_ceiling: u64,        // per-frame count ceiling
    pub refused_contention: u64,     // promotion retry bound exhausted
    pub refused_retiring: u64,
    pub retained_evictions_held: u64, // matured evictions deferred by retention
}
// RetainRefusedReason stays two variants: the consumer's decision (copy
// out) is identical for every Exhausted cause; the counters carry the
// diagnosis budget-vs-ceiling-vs-contention.
impl<D: PoolBackend> Pool<D> { pub fn retention_stats(&self) -> RetentionStats; }
```

## Observability

- `Pool::retention_stats()` as above — public and production-visible (the
  existing `LifecycleCounters` surface is `pub(crate)`/mock-gated and
  cannot carry the consumer signal). Paired asserts on tally transitions.
  `retained_evictions_held` increments exactly when `drain_matured` changes a
  retained `Evicting` frame into `HELD`; it does not count CLOCK candidates or
  ordinary resident-cycle drops.

## Boundaries

- Sira-side owner-table and descriptor composition are sira's work. Victim
  selection is untouched (`cache-semantics-injection` owns it): a retained
  frame may be selected and logically evicted, then becomes `HELD` at matured
  reclaim instead of being physically reused. No R8-style retained-victim
  rejection is added. Under the zero-budget bypass no frame becomes `HELD` due
  to retention and the diagnostic remains zero. `FrameState` unchanged; no
  generics added to the guard surface; no
  `Clone`/`Send`/`Sync`; no cross-thread release execution.

## Tech Decisions

- Word-state budget rule (release on `1 -> 0` with HELD clear; consumer
  releases HELD frames): closes both round-3 criticals — the dominant
  resident-cycle leak and the rollback-provenance corruption — with one
  provenance-free rule.
- Loop-free drop (`fetch_sub`), fixed-bound promotion CAS loop refusing
  `Exhausted` on exhaustion, count-bounded drain loop: every retry bounded
  at init per AGENTS.md, and the drop path — which has no failure channel
  — needs no bound because it has no loop.
- Mutex-mediated wake: `WaitState::wake` costs a mutex + condvar +
  eventfd write(2), so an unconditional per-drop wake is a syscall on a
  hot-path-adjacent drop. `wake_if_parked()` checks the parked count
  under the generation mutex both park paths already use — no ordering
  proof needed, cost confined to the rare HELD-drop path; the
  promote/release bench arm is GATED for exactly this reason.
- Lock-free promotion via mint-time file-slot capture + per-slot atomic
  retiring flags (Release/Acquire eventual-admission policy — normative
  ordering statement lives in design.md alone; other sections cite it);
  AD-4-on-promotion rejected (the single-value fallback may promote per point,
  and session setup may promote a bounded batch).
- Tag slots as Relaxed `AtomicU64` under AD-4 mutual exclusion — the
  lint config (`undocumented_unsafe_blocks`) makes `UnsafeCell` +
  `unsafe impl Sync` strictly worse than a free relaxed atomic.
- `src/pool/retention.rs` as the owning module; `epoch.rs` keeps only the
  `into_retained` entry point.
- Budget held until consumer free FOR HELD FRAMES ONLY — pending releases
  occupy budget (ring capacity provable), while resident-cycle drops
  recycle budget immediately.
- Zero-budget bypass; non-generic `Retention` backref;
  refusal-carries-guard; `frame_pages` preserved at HELD pop / cleared at
  consumer free — all as revision 3, verified by the round-3 native
  review.

## Verification

- Loom (each case marked REAL primitive or NAMED STAND-IN; the model
  gains a second ReaderSlot with `held_frame` made per-slot, and records
  its expected state-space/preemption bound in T6 before T4 lands):
  retain-while-Evicting vs `drain_matured` (real); concurrent last-drops
  (real); concurrent first promotions at the budget boundary incl.
  rollback racing a concurrent increment, with the oracle asserting the
  tally never drops below the number of frames with `count > 0` (real);
  nested-guard promotion (real); two producers vs consumer on the ring
  (real); ring turnover via a "drain driver" stand-in entry running
  release-drain + `drain_matured` in `advance_and_reclaim` order
  (stand-in for `claim_frame_bounded`); promotion vs `retire_file` flag
  store on a model flag (stand-in); direct-free soundness (real).
- Acceptance matrix with owners: budget boundary and recovery INCLUDING
  the dominant cycle — retain, drop while Resident, repeat past
  `max_retained_frames`, assert no `Exhausted` and tally 0 at rest (T7);
  refusal returns the same live guard (T7); N retentions one frame = one
  budget unit (T7); non-last vs last drop (T7); unrelated reclamation
  unaffected (T7); default-0 bypass parity (T7/T8); PollReport == frames
  reaching Free (T7); wake-during-park via the existing
  `MockWaitObservation::wait_until_parked` harness (T7); multiple retained
  handles remain lifetime-bound and byte-stable across logical eviction (T7);
  retention stats behavioral test including exact
  `retained_evictions_held` transitions and a zero value under the zero-budget
  bypass (T7); `FileRetiring` refusal,
  retired-while-HELD stays
  Retiring then closes, retire-reopen-same-slot promotion succeeds (T5).
  The count-ceiling refusal is a UNIT test on the word-transition helper
  (65,535 live handles end-to-end is not a reasonable construction).
- Zero-alloc over promote/release/drain; miri + asan lanes.
- Bench plan (T1, pre-code, template-conformant): R8-shaped retained-session
  access versus the transient guarded path, with promotion and fixed-capacity
  descriptor composition outside the timer and identical useful-byte work
  (gated); transient guard A/B (gated), nonzero-budget poll boundary A/B
  (gated), zero-budget bypass
  A/B (gated, parity), promote/release including gated-wake behavior
  (GATED). Before freezing the shipping retained-session arm, the plan records
  the recorded default-memlock baseline: the exact 8,035-page shipping arm
  targets the unregistered-buffer backend and waits for its gate; a
  registered raised-limit arm may appear only as an opt-in optimization
  comparison. Mock evidence or any smaller preflighted scaled arm cannot be
  relabelled as that proof.
- Warm-state controls (T1, normative for every timed arm): timed-region
  fault counts — BOTH `ru_minflt` AND `ru_majflt`, read with
  `RUSAGE_THREAD` on the pinned bench thread — recorded for BOTH sides
  of each pair. Per-arm rule, explicit: a nonzero MAJOR-fault count
  invalidates the pair under EVERY posture; registered arms and
  exact-R8 arms additionally require zero minor faults; other
  unregistered arms record minor faults as load-bearing warmth
  evidence. The host record includes NUMA node count and
  `numa_balancing` state (the 3970X presents a single node, making a
  zero-hinting-fault expectation reasonable — record it, don't assume
  it). Any run labelled an exact R8 rerun additionally prefaults the
  locator mmap baseline (`MADV_POPULATE_READ`), and pins sira's
  `BlockVerification` mode identically across arms (a `PerDecode` arm
  times crc32c, not retention). Page-cache residency alone proves
  neither PTE presence nor TLB state. Rerun identity also pins the
  segment layout: sira has since adopted SAP1 aligned-frame superblocks
  (feat/sira-sap1-format), so both arms of any pair run one identical
  layout, and a pair on the new layout is a NEW baseline, not an R8
  reproduction — R8-exact identity stays bound to the provenance source
  bases in `resources/r8-resident-set.md`. OWNERSHIP: the
  retained-vs-locator arm requires sira-side artifacts (locator mmap,
  `BlockVerification`, segment corpus) that no task in this scope owns —
  that tie gate is discharged by a sira-side companion run under these
  same controls, is NOT among the arms T1 authors or T8 executes, and
  is recorded as a cross-repo obligation in `dependencies.yaml`. The
  threshold stays <= 1.02 and is a TIE gate with the equal-regime
  qualifier stated: at equal bytes, equal segment layout, and equal
  page-size/TLB state, the retained path cannot undercut a warm mapped
  load — it removes software work, not the virtual-memory access. A
  measured ratio below 1.0 is interpretable ONLY where the arms'
  recorded page-size states differ (the Capacity lever); absent that
  difference it indicates a measurement defect, not a win.
- Asserts: paired positive/negative on word transitions, HELD set/clear,
  tally reserve/release, ring never-full, promotion-never-observes-HELD,
  tag-slot-valid-only-while-HELD, Resident-victim-has-frame_pages
  (hardened expect in `evict_one_victim`).
