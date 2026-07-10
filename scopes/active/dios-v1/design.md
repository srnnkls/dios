# Design: dios-v1

---

## Problem

| Metric | Current | Target |
|--------|---------|--------|
| Miss handling | synchronous page fault, invisible to any scheduler, unbatchable | submitted op; worker overlaps other work (DIO-G3) |
| Warm block-fetch (Linux) | mmap fetch feeding CRC+decode (post read-perf/-concurrency baseline) | one-sided 95% CI upper bound of the ratio ≤ 1.02 (non-inferiority, 2% margin, DIO-G1) |
| Hot-path allocations | 0 (mmap path) | 0 (pool path, enforced harness) (DIO-G4) |
| IO-error model (reads) | SIGBUS on fault | `Err` from completion, fanned to all waiters |
| Eviction control | kernel reclaim + TLB shootdowns | explicit CLOCK, deferred reuse at poll boundaries |
| nmnm residency layer | specced only (`architecture.md:39-56`, stub `src/io/`) | implementable by this crate unmodified (DIO-R5) |

mmap's miss is a trap: it cannot be batched, made async, or observed by a
work-stealing scheduler — a stalled worker is a lost core. Under the
planned nmnm integration (fault → bubble to async → worker takes ready
work) page faults are the one IO shape that cannot participate. The write
plane additionally gains O_DIRECT on segment data (no page-cache dirtying
by large compactions) and a single IO abstraction for future nmnm
extraction.

Layering: the pool is the raw-granule residency layer directly replacing
mmap at the `BlockSource` seam. The decoded-block cache from
sira-read-perf sits ABOVE it and is unchanged; a decoded-cache hit
touches neither backend. DIO-G1 therefore measures at the block-fetch
layer with the decoded cache bypassed — an end-to-end warm get would be
dominated by the decoded cache and discriminate nothing.

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

### Multi-frame block assembly

Let blocks span granule boundaries and assemble them across frames.

**Rejected:** two frames are never contiguous, so a single `&[u8]` view
requires a scratch copy — an allocation or a reserved scratch region
that reintroduces per-read copies. AD-5/AD-6 instead bound encoded block
size by GRANULE (vNext padding + `ValueTooLarge` cap); the cost is
internal padding fragmentation, measured during T006 granule sizing.

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

## Epoch reclamation (EBR) — algorithm

Specified here so INV-1/INV-9 are testable rather than asserted.

State per pool: `global_epoch: AtomicU64`; per registered reader thread a
`local_epoch: AtomicU64` slot (u64::MAX = quiescent); an `evict_queue` of
`(frame_idx, tagged_epoch)` (fixed ring, capacity = frame count).

- Reader registration: fixed slot table sized `max_concurrent_readers`;
  registration beyond capacity fails at registration time. The returned
  `ReaderCtx<'pool>` is `!Send + !Sync` and cannot outlive the pool — an
  epoch slot belongs to exactly one thread, enforced by the type. Slots
  deregister via TLS destructor/RAII on thread exit — a dead thread's
  stale epoch must not stall reclamation.
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
- Stalled reader: a thread holding a guard indefinitely stalls epoch
  advance; reclamation halts but correctness holds. The watermark
  invariant (INV-9) bounds the frames a well-behaved reader can hold;
  sira's cursors drop guards at block boundaries, so stalls are bugs and
  surface as `Busy` under load, not corruption.
- Busy path: `get()` finding no evictable frame runs one bounded reclaim
  attempt — drain completions, advance the epoch if every reader
  permits, reclaim expired Evicting frames, one more CLOCK sweep — then
  returns `Busy`. Within the watermark this makes `Busy` reachable only
  under reclamation lag or a stalled reader (INV-9); a Busy-rate counter
  keeps the frequency observable, and `miss_headroom ≥ 3 ×
  max_inflight_reads` covers InFlight frames plus two grace periods of
  Evicting limbo (each miss admits at most one eviction).
- Shutdown: `Pool::drop` requires live-guard count 0 (debug panic /
  release block-and-drain), then quiesces in-flight ops, deregisters
  buffers, tears down.

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
| INV-1 | A frame is never reused while pinned or in-flight | frame state machine (Free→InFlight→Resident→Evicting→Free); Evicting→Free only after two epoch advances (EBR above); loom test |
| INV-2 | Zero allocation on get/submit/poll after warmup | alloc-count harness (nmnm `zero_alloc.rs` pattern) in CI |
| INV-3 | `submit` never waits on kernel completion or queue space (the AD-4 submit mutex is a bounded SQE-fill critical section; `poll_wait` parks in the kernel outside the mutex — the lock covers SQE fill + CQ drain only; blocking wrappers are metadata-plane-only; eager backend runs syscalls at poll, not submit) | uring: SQ-full → flush-and-retry-once then `SubmitErr::Full`; eager: bounded queue → `Full`; test with saturated queue; submit-while-poller-parked test |
| INV-4 | Kernel never writes into memory Rust may free | frames registered at pool init (uring only; eager uses the same non-moving slab unregistered), deregistered only in `Pool::drop` after quiesce on both backends; ops address frames by index, not pointer-lifetime |
| INV-5 | Journal one-barrier-per-micro-commit preserved | metadata plane stays buffered+fsync (AD-3); existing journal/crash suite runs through the new write plane unmodified (DIO-G5) |
| INV-6 | `FrameGuard` borrows cannot outlive residency | `&[u8]` tied to guard lifetime; guard `!Send`; compile-fail test |
| INV-7 | Resident frame content is immutable until Evicting; frames hold raw granules — per-block CRC lives above the seam in `segment::block_storage`, per fetch, as today | frames read-only until Evicting; eviction-re-read corruption test through BlockSource (DIO-G5) |
| INV-8 | Kernel ops always drain; shutdown quiesces | `Pool::drop`/`close` polls until in-flight count is 0; PendingToken drop is waiter-interest only |
| INV-9 | Deadlock freedom: pool sized ≥ watermark (max concurrent readers × peak guards per reader + miss_headroom, miss_headroom ≥ 3 × max_inflight_reads); within the watermark `Busy` is bounded, retriable backpressure — reachable only under reclamation lag or a stalled reader, never deadlock | peak guards per reader = static max merge fan-out `f(ln_runs [20] + levels)`; compaction counts as a reader at peak fan-out; reader slots capped at registration; under-watermark config fails open; get() runs one bounded reclaim attempt before returning Busy; Busy-rate counter; open-fail test + DIO-G7 recovery + loom |
| INV-10 | Encoded block size ≤ GRANULE | `ValueTooLarge` at the write path (AD-6); vNext writer padding (AD-5); cap-rejection test |
| INV-11 | No op references a closed fd, a reused op slot, or a reused write buffer | ownership: `close(FileHandle)` by value, close(2) deferred past drain; generational `FileId` checked at miss submit; `OpToken` issued by submit, slot reclaimed only at completion drain; `submit_write` consumes `WriteSlot`, arena slot freed at completion drain; close-with-in-flight and stale-generation fault tests |

---

## Complexity

| Dimension | Before | After | Delta |
|-----------|--------|-------|-------|
| Crates | sira workspace, no IO crate | standalone `dios` crate + sira dependency on it | +1 crate (own repository), extraction done up front |
| Read backends | 1 (mmap) | 2 behind `BlockSource` (pool: Linux; mmap: macOS) | +1 seam, +1 backend |
| Write plane | `CommitFs` → std::fs | `CommitFs` → `dios` driver (O_DIRECT data via WriteArena / buffered metadata) | same trait, new impl |
| Segment format | unpadded, unbounded blocks | vNext granule padding, version bump, GRANULE-capped blocks | writer + fixtures change; old stores rebuild |
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
| zero_alloc harness: warm get, miss submit, drain | INV-2 | 0 allocs both backends |
| saturate SQ / eager queue, then submit | INV-3 | `SubmitErr::Full`, no block, recovery after poll |
| single-thread writer exhausts WriteArena, alloc_wait (eager backend, no external poller) | write-plane liveness | slot freed by driver pump, no self-wait, no timeout |
| drop Pool with in-flight reads | INV-4, INV-8 | quiesce completes, no UAF under miri/asan run |
| crash suite through write plane | INV-5 | torn-tail replay + manifest swap pins green |
| crash suite on mock driver with seeded completion reordering | INV-5, write-plane ordering | dependent-step sequencing survives adversarial completion schedules; any submission-order assumption fails loudly |
| compile-fail: guard slice outlives guard | INV-6 | does not compile |
| compile-fail: ReaderCtx sent/shared across threads or outliving the pool | EBR per-thread epoch slots | does not compile |
| IO error on cold read | pool error channel | `ReadyResult::Err`, frame freed, all singleflight waiters get the error |
| corrupt block on disk, cold + re-read after eviction | INV-7 | CRC error surfaces from BlockSource verify on each fetch |
| pool sized below watermark | INV-9 | store open fails with config error |
| watermark-sized pool, all spare frames pinned, get(absent) | INV-9, DIO-G7 | `Busy`/`Pending`, no deadlock, recovers on guard drop |
| put exceeding GRANULE-bounded block size | INV-10 | `ValueTooLarge`, nothing reaches the writer |
| 64 concurrent cold gets (Linux, O_DIRECT) | DIO-G3 | wall ≤ 2.0x p50 single-miss latency |
| pinned parity bench (Linux, 1/5 scale, decoded cache bypassed) | DIO-G1 | ratio CI upper bound ≤ 1.02 (non-inferiority) |
| write-plane A/B: segment flush + journal micro-commit vs retained RealFs arm (Linux, pinned host) | DIO-G8 | each ratio CI upper bound ≤ 1.02 (non-inferiority) |
| scan workload: sweep > pool size interleaved with point-gets | eviction quality (S3-FIFO escalation evidence) | per-ReaderCtx hit/eviction counters recorded in measurements.md — observation, not a gate |
| pre-vNext store open | DIO-R3, AD-5 | rejected with rebuild-required format error |
| close(FileHandle) with ops in flight (both backends) | INV-11 | ops complete on the old fd; close(2) observed only after drain; no fd-number recycling race on eager |
| submit_read with a stale-generation FileId | INV-11 | rejected before an op is issued |
| submit while another thread is parked in poll_wait | INV-3 | submit returns without waiting on the poller's timeout |
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
  only relocate the mutex). What it costs is N× registered-buffer
  locked-memory accounting and per-ring sizing, both probed at open —
  which is why it waits for a profile that convicts the mutex, not a
  hunch (AD-4 trigger).
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
