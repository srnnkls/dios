---
created: 2026-07-07
status: active
issue_type: Feature
revision: 10
parent: sira-read-concurrency
blocked_by: [sira-point-format]  # sira-read-perf, sira-read-concurrency, sira-read-hotpath (PR #5) all done; sira-point-format batch 1 converges the cache onto this scope's seam
---

# Scope: dios-v1

> **Revision 10 — post-v1 Sira-fit API evolution (2026-07-20).** The
> completed v1 batches and their review evidence remain historical facts. This
> revision adds pending task T017 and supersedes the original public pool
> signatures where they conflict with the product API below. Its RED tests are
> still in A.5 review; no implementation or review gate is claimed green.

Async direct-IO layer + userspace buffer pool, built as the standalone
crate `dios` (this repository); sira consumes it as a dependency. Both
read backends work on both platforms
behind the `BlockSource` seam; the platform split is defaults only — on
Linux the pool is sira's default segment read backend (replacing mmap),
on macOS mmap stays default with the pool selectable by config. The
entire write plane (segments, manifest, journal) routes through the
crate on both platforms. The
crate's API is shaped to later implement nmnm's specced-but-unbuilt
`BlockCache`/`MemHandle` residency layer (nmnm `architecture.md:39-56`,
`src/io/` is a one-line stub) without modification.

Sequencing: fully sequential. The original blockers (sira-read-perf,
sira-read-concurrency) are done, as is sira-read-hotpath (merged, PR #5);
the live blocker for the sira phase is sira-point-format, whose batch 1
dissolves the zero-copy layering collision and converges sira's cache
onto the seam this scope needs (design.md, Layering). The measurement
baseline is the read-hotpath exit gate — Mac: point 1.47x redb (1.67µs),
range 1.98x, col-point 5.0µs; Threadripper: point 1.88x (2.46µs), range
1.91x, col-point 11µs — and moves as sira-point-format lands, so DIO-G1
re-baselines at execution time. The TR box (this repo's pinned gate host)
now carries a validated sira bench setup the DIO-G1 sira arm reuses
as-is: rust 1.96 via mise, stores under ~/build/sira-readprof/full. The
pool replaces mmap as the raw-block residency layer feeding
CRC-verify + decode; what sits above it post-zero-copy is metadata only
(parse-once artifact memo + FirstTouch verified bitmap), not a byte
cache.

## Goal

Replace page-fault-driven IO with scheduled IO: misses become submitted,
overlappable operations instead of invisible thread stalls, eviction
becomes explicit, and SIGBUS leaves the failure model — at strict warm-hit
parity with mmap at the block-fetch layer and zero hot-path allocation.

## Context

- `dios` lives in its own repository (this one); sira lives at
  `~/projects/sira`. `sira/src/...`, `sira-fixtures`, `resources/...`,
  and sira scope paths throughout this document are relative to that
  repository's root. The sira-wiring phase executes there against
  `dios` as a dependency; the blocking scopes (`sira-read-perf`,
  `sira-read-concurrency`) are active there under `scopes/active/`.
- sira's mmap surface is two readers (`sira/src/segment/cursor.rs:25`
  `SegmentReader.mmap`, `sira/src/columnar.rs:1137`); the block-load
  choke points are `load_into` (`cursor.rs:937`) and
  `load_key_block`/`decoded_column` (`columnar.rs:1354`/`1367`); the
  layering-collision site is `BlockPayload::Mapped` (`cursor.rs:117-123`,
  whose invariant comment states the cache entry pins its mapping —
  refs refreshed 2026-07-11 against merged read-hotpath). Writes already
  flow through the `CommitFs` trait (`sira/src/manifest.rs:264`).
- `sira/src/segment/block_storage.rs` already exists and is unrelated:
  it holds CRC verification (`verify_and_strip_crc`) used by the exact
  files this scope touches. It stays where it is. The new read-backend
  seam is therefore named `BlockSource` (`sira/src/block_source.rs`) to
  avoid the collision.
- TigerBeetle's io layer (resources/tigerbeetle/src/io) proves the shape:
  caller-owned intrusive completions, zero per-op allocation, io_uring on
  Linux (EXT_ARG timeouts, EAGAIN resubmit, O_DIRECT probed per-fs).
  Critical negative result: kqueue cannot do async regular-file IO —
  TigerBeetle's darwin backend executes file ops synchronously inline
  (`darwin.zig:316-319`, `598-625`, `824-834`) with F_NOCACHE +
  F_FULLFSYNC. There is no kernel-op cancel API; ops always drain.
- nmnm (resources/nmnm) is a synchronous morsel engine; async exists only
  at the Gateway boundary. Its residency contract: async fault →
  worker takes other ready work; kernels see only resident borrowed
  buffers; zero-alloc-after-warmup enforced by alloc-count harnesses;
  cancellation = channel closure. `SpillStore` (`src/execution/spill.rs:63-67`)
  is a documented disk-backend drop-in seam.
- Copy semantics (corrects a conflation in
  scopes/draft/sira-read-hotpath/resources/mmap-soundness.md): mmap and
  io_uring+O_DIRECT are both zero-CPU-copy, one-DMA per miss. The
  "per-miss copy" objection applies only to buffered-pread pools. mmap's
  real warm edge is lookup/pin avoidance + shared page cache + warm
  restarts; its miss cost is an unbatchable, unschedulable synchronous
  fault (plus TLB shootdowns under reclaim).

### Key Files

| File | Lines | Description |
|------|-------|-------------|
| `sira/src/segment/cursor.rs` | 530-547 | `load_into` — row block-load choke point, seam site |
| `sira/src/columnar.rs` | 1219-1231, 1332-1386 | columnar block/cell loads, seam site |
| `sira/src/segment/block_storage.rs` | all | existing CRC-verify module — unrelated to the seam, unchanged |
| `sira/src/manifest.rs` | 264-393 | `CommitFs` trait + `RealFs`, write-plane seam, darwin fsync ops |
| `sira/src/journal.rs` | all | micro-commit journal codec/replay |
| `sira/src/segment/writer.rs` | all | segment writer — gains sector padding |
| `sira/src/storage.rs` | 1556-1671, 2244-2310 | read routing, Arc-clone snapshot discipline, commit barrier accounting |
| `sira/src/parallel.rs` | 9-51 | executor-agnostic scan stub the pool's overlap serves |
| `resources/tigerbeetle/src/io/*.zig` | — | reference implementation |
| `resources/nmnm/architecture.md` | §Gateway, §BlockCache | target API contract for extraction |

### Architecture Decisions

#### AD-1: Completion driver, not futures

**Context:** sira has no async runtime; nmnm's kernels are synchronous
with async only at the Gateway; hot path must not allocate.

**Decision:** TigerBeetle-shaped completion-based driver: `submit(op,
slot)` + `poll()`, preallocated completion slab, cfg-selected concrete
backend per platform. No futures, no tokio/monoio dependency, no `dyn`
dispatch on the hot path.

**Alternatives:**
- Futures + owned buffers (monoio `BufResult`): idiomatic but drags in an
  executor, round-trips buffer ownership, mismatches nmnm's sync kernels.
- Driver core + futures veneer: deferred; the veneer can be a later
  feature-gated adapter crate without touching the core.

#### AD-2: Platform-split defaults — pool on Linux, mmap on macOS, both available everywhere

**Context:** macOS has no io_uring. The portable backend executes
syscalls eagerly at `poll()` on the calling thread (AD-7), so a pool
miss on macOS blocks its caller exactly like an mmap page fault blocks
its faulting thread — concurrent readers' misses overlap identically.
What macOS loses vs mmap is shared-page-cache warm restarts; what it
gains is explicit eviction and an `Err`-based failure model.

**Decision:** Both read backends function on both platforms behind the
`BlockSource` seam, selected by store config. Defaults: Linux pool,
macOS mmap. The macOS pool path is supported, not a stub — it is the
fallback when mmap is unwanted and the candidate default if measurement
shows it competitive (advisory pool-vs-mmap comparison on macOS in
T011, non-gating). Pool logic (frames, CLOCK, EBR, singleflight,
backpressure) is thereby fully exercisable on the Darwin dev machine;
only uring specifics require the Linux box.

#### AD-3: Write plane through the crate; O_DIRECT for segment data only

**Decision:** Segment writes, manifest, and journal route through the
crate on both platforms. O_DIRECT (Linux) / F_NOCACHE (macOS) applies to
segment data files only. Metadata files (manifest, CURRENT, journal) use
buffered writes + explicit fsync through the driver: journal appends and
manifest edits are small and unaligned, which O_DIRECT rejects
(EINVAL) — sectorizing those formats buys nothing and risks the commit
protocol. Explicit fsync/fdatasync ops are kept — no blanket O_DSYNC —
so the existing `SyncMode`/`F_FULLFSYNC`/`F_BARRIERFSYNC` barrier
accounting in the commit path (`storage.rs`/`manifest.rs`) and the
journal's one-barrier-per-micro-commit behavior carry over intact.

#### AD-4: Ring topology — one shared ring, submit mutex, poll-caller owns epochs

**Context:** io_uring SQ/CQ are single-producer/single-consumer;
`scan_parallel` workers each submit misses and consume completions.

**Decision:** v1 uses one ring per store guarded by a submit mutex; any
thread may `poll()`, holding the same lock to drain the CQ; the poll
caller advances the reclamation epoch. Warm hits never touch the ring or
the lock — only misses and write-plane ops contend, and the profiled
workload is warm-dominated. The eager backend (AD-7) shares the
submit/drain lock discipline but executes the syscall outside the lock,
re-acquiring only to push the completion. Lock boundary (INV-3): the
mutex covers SQE fill and CQ drain only — `poll_wait`'s kernel wait
(EXT_ARG) happens outside it, so a parked poller never makes `submit`
wait.

**Escalation (recorded; per-worker rings are the designated end-state
topology if shared-ring contention ever shows):**

- Trigger: DIO-G2 fails and the profile attributes the failure to the
  submit mutex (hold time or handoff), not to device saturation or
  cross-CCX placement. If the profile shows handoff cost rather than
  serialization itself, flat combining under the existing topology is
  the cheaper first lever — no API or invariant change.
- Topology: one ring per registered IO thread (readers, writer,
  compaction), each opened with `IORING_SETUP_SINGLE_ISSUER` +
  `IORING_SETUP_DEFER_TASKRUN` (kernel ≥ 6.0/6.1, probed at open; plain
  per-worker rings on older kernels) — strictly one issuer per ring,
  the kernel's intended fast path. No thread ever touches another
  thread's SQ/CQ, so ring serialization disappears rather than moving
  into the kernel's shared-ring `uring_lock`.
- The frame arena is registered in every ring. Locked-memory accounting
  is per registration — N rings account the arena N times (the exact
  limit interface is kernel-version dependent, RLIMIT_MEMLOCK in the
  common case) — so headroom is probed at open with a typed error.
- What stays shared: the pool control plane (page table, singleflight,
  CLOCK hand, evict queue, epoch advance) keeps its mutex, but with
  every syscall outside it the critical sections shrink to pure memory
  operations; a worker draining its own CQ re-acquires it only to
  publish frame transitions and advance the epoch.
- Cross-thread readiness is already ring-agnostic: `ready(token)` reads
  the frame state machine, never a CQ, so a singleflight miss submitted
  on worker A's ring readies waiters on B and C without them seeing A's
  completions. The CompletionSlab stays pool-global (`user_data` slot
  indexes are ring-agnostic); per-ring SQ/CQ depths derive from the
  slab bound.
- Progress rule: readiness is published only when the originating
  worker drains its own CQ, so a worker with in-flight ops must pump
  its ring — on every pool call and before parking — until its
  in-flight count reaches zero, and thread deregistration (the TLS
  destructor path) drains the ring to zero in-flight before the slot
  is released. An issuer that submits and never polls again would
  otherwise strand every waiter on its ops.
- INV-1..11 are unchanged by construction — they constrain frame state
  and slab slots, never ring count. Enacting the escalation is an owner
  decision recorded in validation.yaml, gated by a DIO-G2 re-run.

#### AD-7: Portable backend — eager-inline execution

**Context:** kqueue does no async file IO; TigerBeetle's darwin backend
(`darwin.zig:316-319`, 598-625) enqueues at submit and executes the
syscall synchronously when the loop drains — files never take a
readiness path because regular-file syscalls never return WouldBlock.
On macOS, dios's write-plane callers are synchronous by contract
(`CommitFs`, journal barriers, compaction threads), and pool-read
misses blocking their caller at `poll()` matches mmap fault semantics
(AD-2).

**Decision:** The portable backend executes ops eagerly at `poll()` on
the calling thread, syscall outside the shared lock: `submit` enqueues
only (never blocks), `poll` runs pread/pwrite/fsync inline and
completes them — one threading model. F_NOCACHE on data files,
`darwin_file_sync_op` (F_FULLFSYNC/F_BARRIERFSYNC) for barriers.
Single-thread miss overlap (prefetch-style) is a uring capability.
The eager backend reads into the same preallocated, non-moving frame
slab — ring registration is uring-only; the quiesce-before-free
invariant (INV-4) holds identically on both backends.

#### AD-5: Segment format vNext

**Context:** O_DIRECT reads are issued as granule-aligned extents; a
block that spans a granule boundary would need multi-frame assembly,
which cannot yield a contiguous `&[u8]` without copying (violates
zero-alloc). Existing segments were written without padding, so their
blocks may span granule boundaries.

**Decision:** vNext segment format guarantees no block spans a granule
boundary (writer inserts zeroed padding; GRANULE is a hard upper bound
on encoded block size). Store open requires the current format version
and fails with a rebuild-required error otherwise.

#### AD-6: Hard value-size cap

**Context:** The writer gives an entry larger than the block-size target
its own block (`writer.rs:97-99`), values accepted to u32::MAX —
encoded block size is unbounded. Measured on gestalt's store (36,423
rows): max value 758 B, 36,421 values ≤ 16 B.

**Decision:** A put whose encoded block would exceed GRANULE is rejected
with `ValueTooLarge` at the write path (the existing u32 check tightens
to the granule bound). T006's granule sizing takes maximum value size as
an input. Escalation if the cap binds: WiscKey-style value log, own
scope.

### Constraints

- Zero hot-path allocation: no allocation on warm get, miss submit, or
  completion drain after warmup; enforced by an alloc-count harness
  (pattern: nmnm `tests/zero_alloc.rs`).
- Strict warm parity (Linux), precise form: measured at the block-fetch
  layer with the artifact memo warm in both arms (post sira-point-format
  batch 1 the cache above the seam holds only byte-source-independent
  metadata, so the bench compares pool-frame fetch against mmap fetch,
  both feeding CRC+decode, with no byte cache to bypass — the earlier
  "cache disabled or cold" framing is retired; it fought the zero-copy
  layering), on the
  pinned 1/5-scale bench, ≥ 30 interleaved repetitions; PASS iff the
  upper bound of the one-sided 95% CI of the wall-time ratio pool/mmap
  is ≤ 1.02 — a one-sided non-inferiority bound with a 2% margin: true
  parity passes, any real regression ≥ 2% fails (an upper bound of 1.00
  would demand measured superiority and reject true parity ~95% of the
  time). Full scale reported honestly. If the gate fails after the
  per-worker last-frame memoization lever, the Linux default flip is
  BLOCKED and the decision (relax the gate vs keep mmap default) returns
  to the scope owner — it is not relaxed silently.
- Minimum pool size (deadlock freedom): a reader's k-way merge holds one
  pinned guard per source plus key/column blocks concurrently.
  `frame_count >= watermark = max_concurrent_readers × peak_guards_per_reader
  + miss_headroom`, where `peak_guards_per_reader` derives from the
  STATIC maximum merge fan-out — the whole-stack merge bound
  `f(ln_runs [default 20] + level count)` from config — not the
  open-time source count, and `max_concurrent_readers` counts compaction
  as a concurrent reader at that peak fan-out (compaction reads through
  the pool). Enforced at store open: a configuration below the watermark
  fails open, it does not deadlock at runtime. Reader-thread
  registration slots equal `max_concurrent_readers`; registering beyond
  capacity fails at registration. Within the watermark, `Busy` is
  bounded, retriable backpressure — never deadlock: `get()` runs one
  bounded reclaim attempt (drain completions, advance the epoch if
  possible, reclaim expired Evicting frames, one more CLOCK sweep)
  before returning `Busy`, so it is reachable only under reclamation
  lag or a stalled reader, always retriable via `poll()`; recovery is
  pinned by DIO-G7 and a Busy-rate counter keeps frequency observable.
  `miss_headroom ≥ 3 × max_inflight_reads`: one InFlight frame per
  outstanding miss plus up to two grace periods of Evicting limbo, since
  each miss admits at most one eviction.
- Linux bench host: AMD Threadripper 3970X box (32c/64t Zen 2, 4 CCDs /
  8 CCXs, 16MB L3 per CCX), ssh host `nix` — NixOS, kernel 6.6.64
  (clears the ≥ 5.15 uring floor, the ≥ 6.1 `statx(STATX_DIOALIGN)`
  probe, and the AD-4 escalation's `SINGLE_ISSUER`/`DEFER_TASKRUN`
  flags), NVMe Samsung 970 PRO, fio installed for the DIO-G3 device
  floor. Reached via `mise run remote -- <command>` (syncs the tree,
  runs through mise; see AGENTS.md Bench Host). Remaining T014 entry
  validation: bench directory on the NVMe with a real
  O_DIRECT-supporting fs (probe green; not tmpfs), and the protocol
  documented in resources/measurements.md — benches pinned to a fixed
  CCX set (cross-CCX placement skews DIO-G2), performance governor,
  cache-drop method. DIO-G1..G3/G8 run only on this host; the Darwin
  dev machine cannot falsify them.
- Linux kernel ≥ 5.15 for the io_uring backend (registered buffers,
  EXT_ARG); the portable eager backend (AD-7) is the fallback elsewhere.
- O_DIRECT support and required alignment probed per opened file/device
  via `statx(STATX_DIOALIGN)` (kernel ≥ 6.1; TigerBeetle-style write
  probe as the pre-6.1 fallback), the probed alignment encoded in the
  open handle's constructor so misalignment is rejected before an op is
  issued. If unsupported (tmpfs, some CI): pool stays the default
  backend with buffered reads and a logged warning; DIO-G1..G3 results
  are valid only with O_DIRECT active; sira never silently falls back
  to mmap on Linux. Buffered-fallback performance is explicitly
  non-gating — the path exists for correctness where O_DIRECT is
  unavailable, not as a performance target.
- No new default dependencies beyond the `io-uring` binding crate
  (Linux-only, no runtime); no tokio/monoio/glommio. `dios` has no
  dependency on `sira` (structural — separate repository).
- `submit` never blocks; kernel ops always drain (no kernel-op cancel in
  v1 — shutdown quiesces in-flight ops before frame teardown). Dropping
  a `PendingToken` cancels WAITER INTEREST only: the in-flight read
  still completes and the frame becomes Resident under CLOCK. The token
  is lifetime-free, `Send + !Clone` with `Sync` deliberately unspecified,
  and retains only bounded
  `Arc`-backed interest/identity state; moving it does not move a guard or
  reader epoch slot.
- Every `Pool` submit/poll path allocates its staging, raw completion
  batch, caller-completion backlog, and wake state at construction. A
  caller batch's capacity limits delivery only: it never limits backend
  CQ draining, internal read routing, epoch advancement, or reclamation.
  An admitted product operation continues to occupy its configured slot after
  backend completion until its owned caller result is delivered.
- `Pool::poll_wait` arms a generation-tracked wait outside pool control.
  A `PoolWakeHandle` signal immediately before or during the actual park
  is observed; signals may coalesce because the ingress/completion queues
  retain the work, but no arm/park race loses the transition. I/O
  completion and external owner-loop ingress wake the same product wait. The
  shipping backend integrates a private platform wake primitive into its real
  blocking wait; no raw descriptor or unsafe/backend handle is public. A
  observation-only `dios::testing` seam under the existing non-default `mock`
  feature reports actual mock and shipping wait entry/in-progress/exit cause
  without gating, releasing, or shortening that wait, using counters allocated
  at Pool construction. Its separate Arc-backed pool-lifecycle observation
  remains exact and readable after the observed `Pool` value is dropped.
- Sealed-segment immutability and the existing snapshot discipline
  (Arc-clone under brief state lock, `storage.rs:1556-1671`) are
  unchanged.

### Tech Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Crate name | `dios` (standalone crate, own repository) | Extraction done up front — backs sira and nmnm as independent consumers |
| io_uring binding | `io-uring` crate (tokio-rs, runtime-free) | Raw SQE/CQE control for registered buffers + EXT_ARG without an executor |
| Kernel writes into | Registered frames (`IORING_REGISTER_BUFFERS`, `READ_FIXED`) on uring; eager uses the same preallocated slab unregistered | Frames outlive the ring by construction — solves drop-safety structurally, removes per-op buffer accounting |
| Pin discipline | Epoch-based read guards; frame reuse deferred to grace period at `poll()` boundaries (algorithm specified in design.md) | No per-frame refcount write on warm hits (hot-page contention — the RavenDB objection); quiescence is natural in a poll-driven driver |
| Eviction | CLOCK (second chance) v1; a hit sets the reference bit check-then-set (Relaxed load; Relaxed store only when clear) | Zero-alloc, no ordering structure to maintain; steady-state hot frames keep the bit set so repeat hits stay read-only — the first-touch store is the only per-frame write and rides in the DIO-G1 bench; S3-FIFO escalation only if the T014 scan-workload observation (sweep > pool size interleaved with point-gets, per-ReaderCtx hit/eviction counters) shows scan-pollution |
| Miss dedup | Singleflight per PageId; on error, ALL waiters receive the error and the frame returns to Free | Concurrent misses on one page coalesce into one submitted read |
| Durability ops | Explicit fsync/fdatasync ops, no O_DSYNC default | Preserves commit-path barrier accounting and journal barrier-per-commit semantics |
| Metadata IO | Buffered writes + fsync through the driver (manifest, CURRENT, journal) | Small unaligned writes are incompatible with O_DIRECT; crash semantics preserved exactly (AD-3) |
| Portable backend | Eager-inline (AD-7): submit enqueues, poll executes pread/pwrite/fsync on the calling thread outside the lock; F_NOCACHE + `darwin_file_sync_op` on darwin | Write-plane callers are synchronous by contract; pool misses block their caller like mmap faults |
| SQPOLL | Off; knob only | Burns a core — wrong default for an embedded library |

### Data Model

| Entity | Purpose | Key Fields |
|--------|---------|------------|
| `PageId` | Stable address of an aligned file extent | `file: FileId` (generational — stale generation rejected at submit, INV-11), `granule_idx: u32` |
| `Frame` | Preallocated, sector-aligned buffer slot; ring-registered on uring, unregistered on eager | `data: [u8; GRANULE]` (aligned 4096), `state: Free\|InFlight\|Resident\|Evicting`, `page: PageId`, clock bit (set check-then-set on hit) |
| `PageTable` | PageId → frame lookup | open-addressed, fixed capacity = 2× frame count rounded up to a power of two (≤ 50% occupancy at full pool, bounding negative-probe length); lock-free probes are advisory — the miss path re-probes authoritatively under the AD-4 lock before submitting, guard-create recheck catches stale hits; insert/delete only under the lock, delete by backward-shift (no tombstones); no rehash ever |
| `CompletionSlab` | Op slots at fixed capacity from open — `frame_count + write_slots + metadata_headroom` bounds in-flight ops, so no growth path exists (a lazily-growing slab is amortized-zero, not zero); slots acquired at submit (issuing an `OpToken` = slot + generation, generation bumped at reclaim so a reused slot never aliases a stale token) and reclaimed at completion drain — never caller-chosen (INV-11); `user_data` = slot index | op kind, fd, offset, frame index, waker/callback token |
| `ReaderCtx` | Lifetime-free affine registration; owns an `Arc` to the reader registry plus its slot identity; `!Send + !Sync`; may outlive the `Pool` value and releases exactly its slot on Drop | pool identity, reader slot, thread-affinity marker |
| `PendingToken` | Lifetime-free single-owner miss interest; `Send + !Clone`, with `Sync` deliberately unspecified; owns an `Arc` to interest state plus pool identity, so it may cross a work-steal boundary and resolve using the destination thread's `ReaderCtx` | page, miss slot + generation, pool identity, active-interest bit |
| `FrameGuard<'pool>` | Borrowed epoch-pinned read access, `Deref<Target=[u8]>`, `!Send + !Sync`; remains bounded by the `Pool` and destination `ReaderCtx` borrows | frame index, epoch ticket |
| `PoolWriteArena<'pool>` / `PoolWriteSlot<'pool>` | Closed crate-root product staging types, backend-erased and distinct from `dios::driver` staging vocabulary; registered O_DIRECT (Linux) / aligned F_NOCACHE (darwin), outside the read watermark | pool identity, exactly `PoolBuilder::write_slots` reusable slots (default 0), `DerefMut` slot |
| `PoolToken` / `PoolCompletion` | Pool-minted operation identity and owned caller result; completions are exactly typed `Write` or `Fsync`, never `Read`/close or raw driver kinds | pool identity, slab slot + generation, typed owned result; admitted + retained population bounded by `max_inflight_product_ops` (default 0) |
| `PoolWakeHandle` | Cloneable external wake capability, `Send + Sync`; shares a monotonic wake generation with `Pool::poll_wait` | pool identity, wake generation/event source |
| `Pool` | Product API and sole owner of read placement, writes, fsync, progress, and file retirement | frames, page table, one composed driver, bounded caller-completion backlog |
| `dios::driver::Driver` | Advanced cfg-selected completion driver, namespaced below the product API | `Uring` (Linux) / `Eager` (portable, AD-7) |
| `dios::testing::MockIoEvent` | Feature-gated sole chronological mock recorder; read/write/fsync attempts, completions, and closes | `ReadAttempt { file, file_offset, destination_offset, requested_len }`; frozen read/write attempt accessors are derived projections, never parallel logs |

Frame granule: fixed per store at open. GRANULE is a hard upper bound on
encoded block size (AD-5); the default is fixed during T006 from a
block-size distribution measurement over representative stores (the
fragmentation cost of padding small key blocks is part of that
measurement). Segment vNext layout requirements (full spec is T012's
deliverable, reviewed before implementation):

- header format-version bump; store open rejects any non-current
  version with a rebuild-required error (AD-5)
- writer inserts zeroed padding so no block crosses a granule boundary;
  block index offsets address true block starts (padding is transparent
  to the index and to CRC coverage, which is per-block payload,
  unchanged)
- file length padded to a whole number of granules; footer/fixture
  tooling (`sira-fixtures`) updated alongside

## Requirements

### Functional Requirements

- DIO-R1 crate: `dios` is a standalone crate whose primary API is the
  frame `Pool`; the lower completion driver
  (read/write/fsync/open/close) remains available explicitly under
  `dios::driver`. No sira types enter either public layer and there is no
  `sira` dependency (structural — separate repository).
- DIO-R2 pool: preallocated registered frames, PageTable, CLOCK eviction,
  epoch pin guards, and singleflight misses with error fanout.
  `get(page)` returns `Result<Get<'_>, GetError>`; the residency ADT
  remains exactly `Hit(FrameGuard)`, `Pending(PendingToken)`, or `Busy`,
  while a retired/stale file is
  `GetError::StaleFile { page }`. `ReaderCtx` and `PendingToken` are
  lifetime-free owned capabilities; `FrameGuard` remains borrowed.
  `poll_report`/`poll_wait` route reads privately and report typed
  progress. The pool is a raw granule cache and knows nothing of blocks
  or CRCs — per-block CRC verification stays in sira above the seam.
- DIO-R2b product write staging: `PoolWriteArena` and `PoolWriteSlot` are
  closed crate-root staging types for the O_DIRECT data plane, separate
  from the read pool and outside its watermark. The same `Pool` submits
  writes and fsync barriers and returns `PoolToken`; caller-visible
  outcomes arrive only as owned `PoolCompletion::{Write,Fsync}` values.
  `write_slots` and `max_inflight_product_ops` are explicit fixed builder
  capacities and both default to zero, so read-only Pools pay for and admit no
  product writes. The shipping queue reserves their checked sum with
  `max_inflight_reads`; arithmetic overflow is a typed configuration error.
  Metadata-only users may still choose the explicit blocking convenience
  wrappers under `dios::driver`.
- DIO-R3 sira reads: both backends behind the `BlockSource` seam at the
  two choke points, selected by store config; defaults Linux=pool,
  macOS=mmap (AD-2). Store open rejects non-current segment format
  versions (AD-5). Coverage: backend×platform matrix tests (pool-on-macOS
  included), old-format open-rejection test, advisory (non-gating)
  pool-vs-mmap measurement on macOS in T011.
- DIO-R4 sira writes: segment writer (O_DIRECT data plane, WriteArena
  staging), manifest and journal (buffered + fsync metadata plane, AD-3)
  route through the crate on both platforms; segment format vNext per
  AD-5; writes exceeding the AD-6 cap rejected with `ValueTooLarge`.
- DIO-R5 nmnm-readiness: a compile-tested example demonstrates the
  Gateway contract — async submit → resident borrowed buffer, worker
  never blocks on a miss, waiter-interest drop on pending tokens —
  without any nmnm wiring.
- DIO-R6 Sira owner-loop fit: a lifetime-free owner can store `Pool`,
  `ReaderCtx`, and pending misses without self-reference or unsafe
  lifetime extension; pending interests may cross threads without moving
  guards; external ingress and I/O share a lossless wait/wake boundary;
  one `Pool` owns reads, writes, fsync, and typed retirement. Raw
  `driver::OpKind`, `driver::OpToken`, `driver::CompletionBatch`, raw
  read/close completions, and backend-specific arena types never occur in
  a product signature.

### Technical Requirements (gates)

- DIO-G1 strict parity (Linux): block-fetch-layer warm point-get via
  pool vs mmap, artifact memo warm in both arms (no byte cache exists to
  bypass post sira-point-format batch 1); PASS iff upper bound of the
  one-sided 95% CI of the ratio ≤ 1.02 (non-inferiority, 2% margin;
  protocol in Constraints).
- DIO-G2 scaling (Linux): non-inverse threaded reads per RC-R2
  methodology (8t ≥ 2x 1t throughput target inherited), under the AD-4
  ring topology.
- DIO-G3 overlap (Linux): 64 concurrent cold point-gets on distinct
  blocks complete within 2.0x the p50 single-miss latency (not 64x) —
  demonstrates scheduled misses. Escalation lever: T014 records the fio
  QD64 random-read wall on the same host/file as the device floor; if
  the gate fails, it re-forms as ≤ 1.2x that floor — owner-signed,
  recorded, never silent.
- DIO-G4 zero-alloc: alloc-count harness shows 0 allocations on warm get,
  miss submit, and completion drain after warmup, both backends.
- DIO-G5 crash safety: existing crash suite (CommitFs fault shims, torn
  journal tail, manifest atomic swap) green through the new write plane
  on both platforms; includes a corrupt-block case re-read after
  eviction (re-fetch through `BlockSource` re-verifies, surfacing the
  CRC error — per-fetch verification semantics unchanged from today).
- DIO-G6 no regression: the full sira suite on both platforms — poison
  oracles, laziness pins, routing contract, absent-key class, RC-R1..R3
  on the platform-default backends. Owned by T014 as an explicit
  full-sweep step.
- DIO-G7 backpressure: `get()` beyond the declared watermark yields
  `Busy` without deadlock and without blocking submit; a pool sized
  below the watermark fails at open (deadlock-freedom constraint).
- DIO-G8 write-plane non-inferiority (Linux): segment flush wall time
  and journal micro-commit latency through the new write plane
  (WriteArena O_DIRECT data + buffered/fsync metadata) vs the pre-dios
  `CommitFs`/`RealFs` baseline — the old impl is retained behind the
  bench as the baseline arm — interleaved A/B on the pinned host, ≥ 30
  reps; PASS iff the one-sided 95% CI upper bound of each ratio
  (new/old) is ≤ 1.02. O_DIRECT forgoes page-cache write-behind, so
  this is the gate that catches a flush regression the read gates
  cannot see. Escalation levers, in order: WriteArena batch depth
  (stage multiple granules per submit-await window); if the segment
  arm still fails, the segment-data O_DIRECT default is BLOCKED
  (buffered segment writes through the driver stay the default,
  O_DIRECT config-selectable) and the decision returns to the scope
  owner. The journal arm has no lever beyond wrapper thinning — the
  syscall sequence is identical by design (AD-3), so its regression
  blocks outright.
- DIO-G9 Sira-fit product contract: the T017 RED suite pins lifetime-free
  owned capabilities, cross-pool identity rejection, product-only typed
  completions, truthful progress independent of caller batch capacity,
  external and I/O wakeups without an arm/park race, write-before-fsync
  execution, and deferred typed file retirement across every admitted
  read/write/fsync/token/guard. The post-warmup pool submit/poll/overflow
  path records zero allocations. It also pins default-disabled product
  capacities, exact configured saturation, checked read+product queue
  reservation proven both at overflow and by one concurrently admitted read plus
  the full configured product-operation bound on the shipping backend,
  capacity-zero progress whose retained results continue to hold admission
  capacity until delivery, distinct caller-supplied timeout deadlines, and both
  timeout- and signal-driven exits of the actual product wait. Mock lifecycle
  and wait observations are read-only exact counters; the Arc-backed lifecycle
  observation remains readable after Pool drop.
  This gate remains pending until A.5,
  GREEN implementation, full-suite/doctest/example/bench migrations, and
  Phase C review complete.

## Acceptance Criteria

- [ ] Given a warm 1/5-scale store on Linux with the artifact memo warm
  in both arms (block bytes resident only in the backend under test),
  when the pinned block-fetch bench runs pool vs mmap interleaved ≥ 30
  reps,
  then the one-sided 95% CI upper bound of the ratio is ≤ 1.02
  (non-inferiority margin, DIO-G1).

- [ ] Given the RC-R2 threaded bench (shared Arc, 8 threads) on Linux,
  when reads run on the pool backend,
  then throughput ≥ 2x the 1-thread rate and never inverse-scales (DIO-G2).

- [ ] Given a cold store on Linux with O_DIRECT active,
  when 64 point-gets to distinct uncached blocks are issued concurrently,
  then total wall ≤ 2.0x the p50 single-miss latency (DIO-G3).

- [ ] Given the alloc-count harness armed after warmup,
  when a warm get, a miss submit, and a completion drain execute,
  then zero allocations are recorded on either backend (DIO-G4).

- [ ] Given a kill-9 mid-micro-commit-stream with writes routed through the crate,
  when the store reopens,
  then journal replay yields the same prefix-consistent state the current
  suite pins, on both platforms (DIO-G5).

- [ ] Given the full sira test suite on both platforms after the default
  flip,
  when it runs on the platform-default backends,
  then every existing pin (poison oracles, laziness, routing, RC-R1..R3)
  is green (DIO-G6).

- [ ] Given macOS,
  when a store opens with default config,
  then reads route to mmap, writes/journal route through the eager
  backend, and the full test suite is green (DIO-R3/R4); and given the
  pool backend is selected by config on macOS,
  then the full suite is green on the pool path too (DIO-R3, AD-2).

- [ ] Given a store written with a pre-vNext segment format,
  when it is opened,
  then open fails with a rebuild-required format error (DIO-R3, AD-5).

- [ ] Given a put whose encoded block would exceed GRANULE,
  when the write is submitted,
  then it is rejected with `ValueTooLarge` before the oversize block is
  formed (AD-6, DIO-R4).

- [ ] Given the pinned Linux host with the pre-dios `RealFs` baseline
  arm available,
  when segment flush and journal micro-commit run interleaved A/B
  through the new write plane ≥ 30 reps,
  then the one-sided 95% CI upper bound of each ratio is ≤ 1.02
  (DIO-G8).

- [ ] Given a Sira-shaped owner storing `Pool`, `ReaderCtx`, and an
  unresolved `PendingToken`, when the pool is dropped before the two
  owned capabilities and the token is alternatively moved to a
  destination thread, then backend I/O quiesces exactly once, each
  capability releases exactly once, the destination `ReaderCtx` can
  resolve the token, and no guard crosses either lifetime/thread
  boundary (DIO-R6/DIO-G9).

- [ ] Given a caller completion batch smaller than the ready backend
  completion set, when `poll_report` drains, then every backend CQE is
  counted in that pass, internal reads/reclamation progress regardless
  of caller capacity, overflow write/fsync results remain in a bounded
  preallocated backlog while continuing to occupy admission capacity,
  and later calls deliver every exact token/result once, releasing one
  capacity slot per delivery with no allocation after warmup
  (DIO-G4/DIO-G9).

- [ ] Given a shipping Pool configured with one in-flight read and two
  in-flight product operations, when one cold read and two product writes are
  submitted without an intervening poll, then all three admit concurrently,
  the next product submit returns `Full` with its unchanged staging slot, and
  draining yields the exact read bytes and both exact write tokens/results
  (DIO-G9).

- [ ] Given a shard owner parked in `Pool::poll_wait`, when either I/O
  completes or a cloned `PoolWakeHandle` signals Sira ingress immediately
  before/during the park, then the wait returns without consuming its
  deadline, no pool-control lock is held while parked, and no wake is
  lost; the shipping-backend case is proven only after a read-only test
  observation reports its actual wait hook in progress, with a signal exit and
  no timeout exit (DIO-R6/DIO-G9).

- [ ] Given two idle product waits with materially different deadlines, when
  each reaches its deadline, then their elapsed brackets distinguish the
  supplied durations and the read-only observation reports exactly two entries,
  two exits, two timeout exits, zero wake exits, and zero waits in progress
  (DIO-R6/DIO-G9).

- [ ] Given a file with admitted reads, writes, fsyncs, pending/terminal
  tokens, or live guards, when `retire_file` begins, then all new
  get/write/fsync admission rejects the exact stale file immediately,
  old capabilities and typed completions drain exactly once, frames
  reclaim and the fd closes before `Retired`, repeated calls are
  idempotent, and the old generation remains stale after slot reuse
  (INV-11/DIO-G9).

- [ ] Given a pool sized below the declared watermark,
  when the store opens,
  then open fails with a configuration error (DIO-G7); and given a
  correctly sized pool with every spare frame pinned,
  when get() targets a non-resident page,
  then the caller receives `Busy`/`Pending`, no thread blocks, and the
  store recovers once guards drop (DIO-G7).

## API Contract

Revision-10 crate surface pinned by T017. The Pool layer is the product
API; the previously shipped completion layer remains explicitly advanced
under `dios::driver`:

```rust
// dios::driver — advanced leaf API; no allocation after init
pub mod driver {
pub struct Driver(backend::Impl);           // cfg: Uring | Eager (portable)
pub struct FileHandle { /* fd index + generation */ }  // driver-owned; !Copy
pub struct OpToken(u64);                     // slab slot + generation, issued by submit, echoed
                                             // in the completion — a reused slot never aliases a
                                             // stale token (ABA-safe)
pub struct ReadFrameIdx(u32);                // read-pool frame; reuse governed by the pool's frame state machine (INV-1)
impl Driver {
    pub fn open(&self, path: &Path, how: OpenHow) -> Result<FileHandle, IoError>;  // probes O_DIRECT support + alignment (per Constraints)
    pub fn close(&self, fd: FileHandle);     // consumes; close(2) deferred until the fd's in-flight
                                             // ops drain; close errors are non-actionable by design —
                                             // durability rides the explicit fsync barriers (AD-3), a
                                             // close(2) failure after the barrier cannot un-persist
                                             // acknowledged data. Operating failures (EIO-class) are
                                             // logged, never surfaced; only EBADF asserts — a
                                             // double-close is a driver state bug, not an operating error
    pub fn submit_read(&self, fd: &FileHandle, frame: ReadFrameIdx, off: u64) -> Result<OpToken, SubmitError>;
    pub fn submit_write<'a>(&self, fd: &FileHandle, buf: WriteSlot<'a>, off: u64) -> Result<OpToken, (SubmitError, WriteSlot<'a>)>;
    pub fn submit_fsync(&self, fd: &FileHandle, barrier: crate::SyncMode) -> Result<OpToken, SubmitError>;
    pub fn poll(&self, out: &mut CompletionBatch) -> usize;
    pub fn poll_wait(&self, out: &mut CompletionBatch, timeout: Duration) -> usize;
}
} // end dios::driver
// poll never sleeps awaiting events — uring: non-blocking CQ drain; eager:
// executes queued syscalls inline on the calling thread (AD-7). poll_wait's
// kernel wait happens OUTSIDE the AD-4 mutex (lock boundary stated in AD-4).
// resource leases (INV-11): an op can never reference a closed fd
// (&FileHandle at submit + by-value close with deferred close(2)), a reused
// op slot (OpToken is issued by submit and reclaimed at completion drain —
// never caller-chosen), or a reused write buffer (submit_write consumes the
// WriteSlot; the arena slot returns to Free only when the write's completion
// is drained; the Err arm hands the slot back to the caller).
// the kernel writes only into ReadFrameIdx buffers and reads only from
// WriteSlot buffers — the two registered sets are disjoint by type
// SubmitError::Full = SQ/queue full after one flush-retry — backpressure, never a block

// crate root — closed product API; no driver token/kind/batch/arena aliases
pub struct Pool<D = driver::Driver> { /* frames, one D, bounded state */ }
pub struct PoolBuilder { /* fixed resource configuration */ }
pub struct ReaderCtx { /* Arc<ReaderRegistry>, slot, identity; !Send + !Sync */ }
pub struct PendingToken { /* Arc<MissInterests>, generation, identity; Send + !Clone; Sync deliberately unspecified */ }
pub struct FrameGuard<'pool>; // borrowed from Pool + ReaderCtx; Deref<[u8]>; !Send + !Sync

pub enum Get<'pool> {
    Hit(FrameGuard<'pool>),
    Pending(PendingToken),
    Busy,
}
pub enum GetError {
    StaleFile { page: PageId },
}
pub enum PoolConfigError {
    // existing granule/watermark variants omitted
    QueueCapacityOverflow {
        max_inflight_reads: u32,
        max_inflight_product_ops: u32,
    },
}
pub enum ReadyResult<'pool> {
    Ready(FrameGuard<'pool>),
    NotYet(PendingToken),
    Err(IoError),
}

pub struct PoolWriteArena<'pool> { /* backend-erased pool staging view */ }
pub struct PoolWriteSlot<'pool>; // DerefMut<[u8]>, carries pool identity
pub struct PoolToken(u64);       // opaque pool slot + generation; not driver::OpToken
pub enum PoolSubmitError {
    Full,
    StaleFile { file: FileId },
    ForeignPool,
}
pub enum SyncMode { Full }
pub enum PoolCompletion {
    Write { token: PoolToken, result: Result<u32, IoError> },
    Fsync { token: PoolToken, result: Result<(), IoError> },
}
pub struct PoolCompletionBatch { /* fixed-capacity owned results */ }
pub struct PollReport { /* u32 backend_completions + reclaimed_frames */ }
pub struct PoolWakeHandle { /* Clone + Send + Sync, generation tracked */ }
pub enum RetireStatus { Retiring, Retired }

impl PoolBuilder {
    pub fn write_slots(self, slots: u32) -> Self;
    pub fn max_inflight_product_ops(self, operations: u32) -> Self;
}
impl PoolCompletionBatch {
    // capacity 0 is valid: advance/retain without caller delivery
    pub fn with_capacity(capacity: usize) -> Self;
    pub fn iter(&self) -> impl Iterator<Item = &PoolCompletion>;
}
impl PoolWakeHandle {
    pub fn wake(&self);
}
impl<D: PoolBackend> Pool<D> {
    pub fn register_reader(&self) -> Result<ReaderCtx, RegisterError>;
    pub fn get<'pool>(&'pool self, r: &'pool ReaderCtx, page: PageId)
        -> Result<Get<'pool>, GetError>;
    pub fn ready<'pool>(&'pool self, r: &'pool ReaderCtx, token: PendingToken)
        -> ReadyResult<'pool>;

    pub fn write_arena(&self) -> PoolWriteArena<'_>;
    pub fn submit_write<'pool>(
        &'pool self,
        file: FileId,
        slot: PoolWriteSlot<'pool>,
        offset: u64,
    ) -> Result<PoolToken, (PoolSubmitError, PoolWriteSlot<'pool>)>;
    pub fn submit_fsync(
        &self,
        file: FileId,
        mode: SyncMode,
    ) -> Result<PoolToken, PoolSubmitError>;

    pub fn poll_report(&self, out: &mut PoolCompletionBatch) -> PollReport;
    pub fn poll_wait(
        &self,
        out: &mut PoolCompletionBatch,
        timeout: Duration,
    ) -> PollReport;
    pub fn wake_handle(&self) -> PoolWakeHandle;
    pub fn retire_file(&self, file: FileId) -> RetireStatus;
}

// existing non-default `mock` feature; no production/backend primitive is exposed
pub mod testing {
    pub enum MockIoEvent {
        ReadAttempt {
            file: FileId,
            file_offset: u64,
            destination_offset: u32,
            requested_len: u32,
        },
        // write/fsync attempts, all completions, and closes omitted
    }
    pub struct MockPoolObservation { /* Arc-backed exact counters, read-only */ }
    impl MockPoolObservation {
        pub fn registered_readers(&self) -> u32;
        pub fn reader_releases(&self) -> u32;
        pub fn live_pending_interests(&self) -> u32;
        pub fn pending_releases(&self) -> u32;
        pub fn backend_ops_in_flight(&self) -> u32;
        pub fn backend_completions(&self) -> u32;
        pub fn quiesce_calls(&self) -> u32;
    }
    pub struct MockWaitObservation { /* fixed counters, read-only */ }
    impl MockWaitObservation {
        pub fn wait_until_parked(&self, timeout: Duration) -> bool;
        pub fn parks_entered(&self) -> u32;
        pub fn parks_in_progress(&self) -> u32;
        pub fn parks_exited(&self) -> u32;
        pub fn wake_exits(&self) -> u32;
        pub fn timeout_exits(&self) -> u32;
    }
    pub struct ShippingWaitObservation { /* fixed counters, read-only */ }
    impl ShippingWaitObservation {
        pub fn wait_until_parked(&self, timeout: Duration) -> bool;
        pub fn parks_entered(&self) -> u32;
        pub fn parks_in_progress(&self) -> u32;
        pub fn parks_exited(&self) -> u32;
        pub fn wake_exits(&self) -> u32;
        pub fn timeout_exits(&self) -> u32;
    }
    pub trait ShippingWaitTestingExt {
        fn observe_shipping_waits(&self) -> ShippingWaitObservation;
    }
    pub trait MockPoolTestingExt {
        fn driver(&self) -> &MockDriver;
        fn observe(&self) -> Arc<MockPoolObservation>;
    }
    impl MockDriver {
        pub fn observe_waits(&self) -> MockWaitObservation;
    }
}

// Metadata-only advanced use remains namespaced rather than growing Pool.
impl driver::Driver {
    pub fn write_all_blocking(&self, fd: Fd, buf: &[u8], off: u64) -> Result<(), IoError>;
    pub fn fsync_blocking(&self, fd: Fd, barrier: SyncMode) -> Result<(), IoError>;
}
```

Semantics: `EINTR` resubmitted internally on every op; `EAGAIN`
resubmitted on reads (XFS can EAGAIN a blocking file read under
io_uring — TigerBeetle `linux.zig:599-604`) but surfaced as an error on
writes (TigerBeetle returns WouldBlock there, `linux.zig:737-741`);
unlike TigerBeetle, both resubmit paths carry a fixed retry bound set at
init. Short reads resliced and resubmitted by the pool up to the extent
length, not the caller; alignment EINVAL is a programming error surfaced
loudly. "Never blocks" for `submit_*` means
no waiting on kernel completion or queue space — the AD-4 submit mutex
is a bounded SQE-fill critical section, not an IO wait; the blocking
wrappers are the explicit exception, used only on the metadata plane.
Kernel ops always drain. Dropping a `PendingToken` drops waiter interest
only; its owned interest state makes Drop safe after the `Pool` value is
gone, while a moved token can resolve only against a destination
`ReaderCtx` carrying the same pool identity. `Pool::drop` quiesces the
backend exactly once even if reader/token capability metadata outlives
it. `FrameGuard` remains borrowed, so bytes never outlive either Pool or
the reader epoch pin.

`PollReport::backend_completions` counts every CQE drained in that call,
including private reads, regardless of `PoolCompletionBatch` capacity.
The caller batch is reset/refilled with owned write/fsync results only;
overflow stays in a preallocated pool backlog and later delivery reports
no fictitious backend completion. Read routing, epoch advancement, and
reclamation therefore cannot be starved by a full caller batch. Capacity zero
is a valid progress-only batch: it drains and retains within the configured
product bound, and subsequent capacity-one polls deliver exactly one retained
result apiece with zero new backend completions. Retained results continue to
occupy `max_inflight_product_ops` capacity through caller delivery; draining a
CQE alone does not release admission, while delivering one result releases
exactly one slot.
`poll_wait` performs the same routing/report pass after waking and parks
outside pool control. Its generation protocol closes wake-before-park
and wake-during-park races for both I/O and `PoolWakeHandle` ingress. The real
shipping backend wait is interruptible through a private platform primitive.
Under the existing non-default `mock` feature, `MockWaitObservation` and
`ShippingWaitObservation` expose exactly five actual-wait counters plus
`wait_until_parked`; the observations are read-only and cannot release, gate,
or shorten a wait. `MockPoolObservation` exposes exactly the seven lifecycle
counters above through an Arc-backed snapshot handle: all remain exact and
readable after the observed Pool drops, including later reader/pending releases,
without retaining its driver. No observation exposes a raw/unsafe platform
handle or participates in production synchronization.

One Pool owns one driver for reads, writes, and fsync. Product capacities are
opt-in: `write_slots` and `max_inflight_product_ops` both default to zero.
The former bounds reusable staging slots; the latter bounds all admitted
write/fsync operations including completed results retained for caller
delivery, and the next submission returns `PoolSubmitError::Full`. The
shipping backend queue reservation is the checked sum of
`max_inflight_reads + max_inflight_product_ops`; overflow returns
`PoolConfigError::QueueCapacityOverflow` with both operands. Implementations
may use an internal minimum queue depth of one when the sum is zero, without
enabling product admission.

A per-file fsync is not submitted to a reordering backend until all preceding
writes for that file have completed; this may require multiple bounded
`poll_report` passes and no single-poll co-completion is promised. CQE delivery
order remains unconstrained. Write and fsync failures remain independent owned
completion values, and every terminal write releases its staging slot. A
foreign `FileId` is a programmer error and panics before write/fsync admission;
`PoolSubmitError::ForeignPool` applies only to a staging slot minted by another
Pool. Panic while consuming a write slot drops that RAII value and immediately
restores the originating Pool's reusable slot capacity.

Submit checks have one observable precedence: write-slot Pool identity first
(`ForeignPool` with the exact slot returned), then file/driver identity
(programmer panic), then live-generation and retirement state (`StaleFile`),
then product capacity (`Full`). Fsync omits only the first step. Consequently a
stale/retired generation wins over saturation, while a foreign slot wins over
both foreign-file misuse and saturation. Foreign driver identity also wins when
the source Pool has already retired that `FileId`: presenting that retired
foreign identity to a saturated target still panics before either source
generation state or target capacity can affect the outcome.

`retire_file` atomically closes get/write/fsync admission for that exact
generation, then returns `Retiring` until admitted backend ops, caller
completion delivery, pending/terminal interests, borrowed guards, EBR
reclamation, and the deferred fd close have all completed. Already-minted
capabilities remain valid. Repeated calls are idempotent; `Retired` is
terminal, and reopening a reused slot never makes the old generation
live. The advanced driver's by-value `close(FileHandle)` retains its
existing lower-level deferred-drain guarantee.

## Dependency Graph

> Machine-readable: [dependencies.yaml](dependencies.yaml)

```
Phase 1 (crate core)      T001 scaffold → T002 driver surface → {T003 eager, T004 uring, T016 API-fit spike} → T005 harness
Phase 2 (pool)            T006 frames/table/CLOCK/granule (gated by T016) → T007 epoch guards → {T008 miss+overlap, T009 zero-alloc+loom}
Phase 3 (sira wiring)     T010 BlockSource seam → T011 read routing; T012 vNext format → T013 write plane → T014 gates
Phase 4 (nmnm-readiness)  T015 contract example (extends the T016 spike) + extraction checklist
Post-v1 API evolution     {T002,T008,T009,T015} → T017 Sira-fit Pool product API (independent of deferred sira wiring)
```

Phase labels group by concern, not sequencing: T010 (seam refactor, no
behavior change) runs in batch 1 alongside T001. Phases 1, 2, and 4
land in this repository; Phase 3 (sira wiring) lands in the sira
repository against `dios` as a dependency.
T017 is a repository-local follow-up on `feat/dios-api`; it does not
unblock or silently start T010–T014 in the sira repository.

## Non-Goals

- nmnm integration (implementing `BlockCache`/`ResolvesBlocks` inside
  nmnm, Gateway wiring) — later scope; this scope only proves API fit.
- A futures adapter / async-runtime interop layer.
- Kernel-op cancellation (v1 ops always drain); multi-frame block
  assembly (AD-5 makes GRANULE a hard block-size bound instead).
- Multi-process cache sharing (a page-cache property we knowingly give up
  on Linux).
- Value log / key-value separation (escalation path if the AD-6 cap
  binds).
- SQPOLL as default; NVMe passthrough; network ops in the driver.
- Flipping the macOS default to the pool (stays config-selectable; a
  default change needs the T011 advisory measurement).

## Verification

- `cargo test` — unit, fault-injection (short read, EAGAIN,
  EINVAL alignment, fd exhaustion), loom/concurrency for frame state and
  epoch reclamation.
- Alloc-count harness in `tests/zero_alloc.rs` (DIO-G4).
- Linux bench host (Phase 3 entry gate): pinned parity/scaling/overlap
  benches (DIO-G1..G3) against the mmap baseline landed by
  sira-read-perf/-concurrency, the write-plane A/B (DIO-G8) against the
  retained `RealFs` baseline arm, and the scan-workload observation
  (hit/eviction counters into resources/measurements.md — the S3-FIFO
  escalation's evidence), protocol per Constraints.
- Full sira suite on both platforms — explicit full-sweep step in T014
  (DIO-G6); crash suite through the new write plane incl.
  re-read-after-eviction CRC case (DIO-G5).
- backend×platform matrix tests (incl. pool-on-macOS full suite) +
  old-format open-rejection test (DIO-R3).
- T017 Sira-fit RED/GREEN targets:
  `tests/embedded_owner.rs`, `tests/pool_progress.rs`,
  `tests/pool_write.rs`, `tests/pool_retire.rs`, and
  `tests/public_api.rs`, with capability/identity migrations in
  `tests/guard_compile_fail.rs`, `tests/miss.rs`,
  `tests/real_pool.rs`, and `tests/zero_alloc.rs`. The complete suite,
  not a prose migration note, must compile against one API. These targets pin
  capacity through caller delivery, explicit submit-check precedence, PoolToken
  generation on slot reuse, the single chronological `MockIoEvent` recorder,
  a positive shipping checked-sum admission/drain, disjoint short/long timeout
  brackets with exact causes, retired-foreign identity precedence, exact
  Arc-backed lifecycle counters readable after Pool drop, and external wake of
  observed actual mock and `Pool<Driver>` backend parks through the read-only
  `mock`-feature seam.
- Mandatory executable documentation/examples migration in the same
  GREEN change: rewrite `src/pool/epoch.rs` library `compile_fail`
  doctests (remove the obsolete ReaderCtx-cannot-outlive-Pool case;
  retain guard-borrow escape and ReaderCtx thread-affinity; add the
  PendingToken Send/non-Clone and product/driver separation pins), plus
  `examples/api_fit_spike.rs`, `examples/gateway_contract.rs`, and
  `examples/quickstart.rs`.
- Mandatory bench migration: `benches/overlap.rs`,
  `benches/mmap_warm_path.rs`, and `benches/mmap_tlb_pressure.rs` use
  lifetime-free capabilities and `Result<Get, GetError>`. The existing
  overlap/parity plans retain their thresholds; T017 adds no new
  performance claim, while its pool write/fsync/report/overflow
  alloc-count gate extends DIO-G4.

## Gotchas & Learnings

- kqueue does no async file IO — never model the darwin backend on it
  (TigerBeetle `darwin.zig` executes file ops synchronously inline).
- XFS can EAGAIN blocking-file reads under io_uring; resubmit.
- O_DIRECT unsupported on tmpfs — probe and fall back buffered or CI
  misreports parity.
- O_DIRECT alignment constrains buffer address, file offset, and length
  of the syscall — not block placement inside the file. Granule-aligned
  extent reads satisfy it regardless of where blocks sit; the only
  format constraint is that no block spans a granule (AD-5).
- io_uring keeps fds (and flocks) alive until in-flight ops complete —
  affects lock-file handling on unclean shutdown (TigerBeetle
  `linux.zig:1558-1583`).
- io_uring completion order ≠ submission order: dependent write-plane
  steps (segment data → fsync → rename → dir sync) are sequenced by
  awaiting completions/barriers, never by submission order; only
  disjoint-offset data writes within a pre-barrier window may float.
  IOSQE_IO_LINK is a later optimization, not the correctness mechanism
  (resources/iggy-thread-per-core-io-uring.md, crash-halfway lesson).
- F_NOCACHE ≠ O_DIRECT: it drops cache but doesn't enforce alignment;
  self-impose sector alignment on darwin anyway.
- Registered buffers are per-ring, but the same arena may be registered
  in multiple rings — keeps the per-worker-ring escalation (AD-4) open.
  Locked-memory accounting is per registration: N rings account the
  arena N times — probe headroom before enacting the escalation.
- The mmap-soundness.md "per-miss copy" claim does not apply to
  O_DIRECT variants; do not cite it against this design (see Context).
- EBR reader-thread slots must deregister via TLS destructor/RAII on
  thread exit, or a dead thread's stale epoch stalls reclamation
  forever.
- macOS write/read coherency: a segment written with F_NOCACHE may only
  be opened for reading (mmap or pool) after finalize + F_FULLFSYNC +
  manifest publish — the existing sealed-segment commit discipline;
  never read a segment mid-write.

## Open Questions

- [x] GRANULE default value — RESOLVED in T006: 4096 (sector floor), from
  the recorded S003 gestalt-store measurement (max value 758 B over
  36,423 rows; padding fragmentation negligible). Derivation in
  `GRANULE_DEFAULT`'s rustdoc; per-store override stays a construction
  parameter (marker M001).
