# nmnm Residency Extraction Checklist (T015)

The compile-tested contract this checklist extracts is
`examples/gateway_contract.rs` (runs against the real `Pool<MockDriver>`). This
document does not restate the example — it names the one surface the example
could not build and routes it into nmnm.

## The residency lease (named surface)

The example proves the Gateway loop shape but stops at one gap: nmnm's
work-stealing executor moves a `WorkItem` across threads at the Ready
transition, and `dios::FrameGuard` is `!Send`. The missing surface is a
*residency lease* — a Send-able, coarse, per-`WorkItem` handle. It has the
following properties:

- The lease is acquired at the Ready transition during a pool get or ready query.
- It acts as a coarse refcount pin on residency rather than a fine per-guard epoch pin.
- The pool releases the lease at item completion or on cancellation.
- It keeps the page resident across the steal so the destination worker re-borrows a hit rather than triggering a re-miss.

The nmnm residency layer is specced in `resources/nmnm/architecture.md`
§Gateway and §BlockCache (lines 39-56), a path rooted at the sira checkout
(`~/projects/sira`, scope.md Context and Key Files table). This dios repository
carries no `resources/nmnm/` tree — the pool is shaped against that spec, not
vendored alongside it — so from here the anchor resolves only against the sira
working copy; the `BlockCache`/`MemHandle` contract this checklist extracts is
that document's, and the executable half is `examples/gateway_contract.rs`.

### Refcount semantics

The count is per frame: the number of live leases naming that frame's resident
page. It is incremented once per `WorkItem` at its Ready transition and
decremented once at that item's completion — never per hit. At the Ready
transition the frame is already `Resident` and epoch-pinned by the acquiring
reader's `FrameGuard` (`Pool::ready` mints the guard through `pin`, publishing
`local_epoch` via `begin_pin`/`commit_pin`); the lease converts that
instantaneous, thread-bound epoch pin into a Send-able coarse claim on the
frame's residency that survives the guard's drop.

- The counter array is a lock-free per-frame array owned directly by the `Pool`, alongside the CLOCK reference bits and `global_epoch` rather than inside the mutex-guarded `Control`, so acquire and release stay off the control-plane lock. It is allocated at pool construction, so nothing grows after warmup under invariant INV-2. A frame's count is bounded by the number of work items that can concurrently hold one page resident. This is bounded by the deadlock-freedom watermark and the executor's fixed morsel-queue depth.
- The pool performs a fetch add of one on the resolved frame's counter at the Ready transition, or at Get::Hit when the item is stolen, before the thread-bound guard is dropped.
- The lease performs a fetch subtract of one in its drop implementation when the item completes or is cancelled.
- A work item cancelled before it readies never acquired a lease. It drops the PendingToken as waiter interest only under invariant INV-8. A work item cancelled after it readies holds a lease, and its drop releases the count without re-readying.
- A frame whose count is non-zero is ineligible for CLOCK eviction until the count reaches zero.

### Reclamation gate — where the count sits

The gate sits at CLOCK victim selection, in `evict_one_victim`
(`src/pool/mod.rs`). That sweep already skips any frame not `Resident`; the
lease adds one predicate — a `Resident` frame with a non-zero lease count is
passed over, the same second chance a set CLOCK reference bit already grants.
This keeps a leased page from ever taking the Resident-to-Evicting transition because once the frame is evicting, the mapping is removed, and a resumed item's get call would return Pending, which is the re-miss that the stand-in panics on. The gate scopes to the automatic CLOCK evictor; `evict_frame` is a `doc(hidden)` test seam that takes `Resident -> Evicting` directly, and its caller honors the precondition of never evicting a leased page.

This gate is analogous to but distinct from EBR. The epoch mechanism
(`ReaderSlot::permits_advance`, `EvictQueue::drain_matured`) gates the later
Evicting-to-Free transition on time — two global-epoch advances after the
mapping was already removed — protecting a fine guard's `&[u8]` from
reclamation. The lease gates the earlier Resident-to-Evicting transition on
residency demand, keeping the mapping alive so a re-borrow still hits. EBR
answers "is any thread still dereferencing these bytes?"; the lease answers
"does any stolen `WorkItem` still need this page resident?" They compose: a
re-borrowed lease-held page mints a fresh `FrameGuard` on the destination
`ReaderCtx` and re-enters EBR normally.

### Honest cost

A shared refcount is a genuine cross-thread read-modify-write. The warm-hit
epoch path is deliberately RMW-free — a plain Acquire load plus Release store,
never a `fetch_*` (`ReaderSlot` doc, design.md warm-hit cost: "no RMW ever").
An atomic RMW per lease is heavier than that load/store, which fixes the
warm-path budget question: the lease must not sit on the per-hit path. The
`fetch_add`/`fetch_sub` pair is paid once per `WorkItem` at its Ready transition
— a miss-to-ready already costs a syscall-scale event — and never per resident
hit. A hit that runs its kernel inline and drops its guard takes the RMW-free
epoch path untouched. Benching the added acquire and release pair per stolen
`WorkItem` against the DIO-G1 warm-hit parity gate — the check that it stays off
the hot loop — is owned by the later nmnm-integration scope.

- [x] Lease acquire and release edges are pinned to concrete pool entry points.
- [x] The refcount bound is fixed at initialization to ensure capacity limits.
- [x] The cancellation path releases the lease without a ready transition.
- [x] The eviction interaction is stated, where leased frames are ineligible for eviction.

## Why `FrameGuard` cannot cross

A `FrameGuard` is meaningful only on its minting thread, so it cannot travel
with a stolen item; a coarse lease keyed by a Send-able `PageId` is the only
shape that crosses.

- Under invariant INV-6, the borrow is tied to residency. `FrameGuard` holds a byte slice into the frame plus the minting ReaderSlot; the slice cannot outlive the pool that minted it. Doctest A on FrameGuard pins this in the epoch code and is verified in the compile-fail tests.
- The `!Send` marker is enforced by the guard structure. `FrameGuard` carries a thread-bound phantom data marker which makes it non-Send. The compile-fail tests assert this property.
- The root cause is the `ReaderCtx` per-thread epoch-slot affinity. The guard's epoch pin lives in the reader's slot which is accessed with single-writer plain loads and stores on the assumption that only the owning thread touches them. Moving a guard to another thread would violate this single-writer assumption and corrupt the epoch state.
- Only Send-able crossing payloads may traverse the steal boundary. Only `PageId` and `PendingToken` cross for a resolved page, and both are Send. The destination worker calls get with the lease's PageId to mint a fresh guard under its own `ReaderCtx`.

- [x] The INV-6 invariant and the guard `!Send` marker are cited to their source.
- [x] The `ReaderCtx` per-thread epoch-slot affinity is explained as the root cause.
- [x] The Send-able crossing payload is identified and verified to exclude the guard.

## Pool-API surface nmnm would need

This is a specification for a later nmnm-integration scope. T015 builds none of it; the enumeration below is
the extraction deliverable. Signatures are sketches; each names the invariant it
upholds.

- The lease handle type is `pub struct ResidencyLease<'pool> { page: PageId, counter: &'pool AtomicU32 }`. It is Send because the inner atomic reference is Send and Sync, and PageId is Copy. It borrows only the pool's counter array rather than any thread-bound slot, and its Drop implementation runs a fetch subtract of one with Release ordering. This design upholds the invariant of maintaining residency across thread boundaries without carrying thread-bound state.
- The acquire entry point is a Pool method called at the Ready transition, such as `pub fn lease(&self, page: PageId) -> ResidencyLease<'_>`, or an accessor fused into ReadyResult::Ready that returns a tuple of the frame guard and residency lease. It composes with get and ready queries, which already resolve the resident frame, meaning the lease is acquired via a fetch add on the resolved counter with no secondary lookup. This upholds invariant INV-2 because it requires no heap allocation.
- The re-borrow path on the destination thread is initiated when the destination worker calls `pool.get(dest_reader, lease.page())`. Because the lease maintains residency, this call returns Get::Hit and mints a fresh frame guard under the destination thread's reader context. This upholds invariant INV-6 by keeping each guard thread-local.
- The interaction with the miss path, CLOCK, and EBR is minimized. One predicate is added to the clock eviction sweep to skip any resident frame that has a non-zero lease count. The miss table, the CLOCK reference-bit second chance, and the EBR grace period remain completely unchanged. This design upholds invariant INV-1 because a frame is never reused while it is still leased.
- The zero-alloc obligation is strictly maintained. Neither the fetch add on acquire nor the fetch sub on release allocates any memory under invariant INV-2. This behavior is guarded by extending the alloc-count harness to assert zero allocations on both backends during lease operations.

- [x] The Send-able lease handle type is defined along with its borrow semantics.
- [x] The acquire entry point is specified as a pool method and its composition is outlined.
- [x] The re-borrow path on the destination thread is described including reader context pinning.
- [x] The interaction with the miss path, CLOCK, and EBR grace period is detailed.
- [x] The zero-alloc obligation is stated along with the guarding allocator tests.

## Extraction steps

1. The nmnm Gateway clause set from `resources/nmnm/architecture.md` is resolved and mapped to cover each example assertion. This step is completed in this document.
2. Binding `ResidencyLease` to nmnm's `MemHandle` is owned by the nmnm-integration scope: `MemHandle` wraps the dios `ResidencyLease` plus the `PageId`, and its Drop implementation releases the lease.
3. Routing the steal boundary through the lease edges is owned by the nmnm-integration scope: at the Ready transition the worker acquires the lease, captures the Send-able handle into the work item, and drops the frame guard; the destination worker re-borrows the page under its own reader context.
4. Enforcing zero-alloc-after-warmup on the residency path is owned by the nmnm-integration scope: the alloc-count harness asserts zero allocations on both backends across the lease lifecycle.

- [x] The team mapped each example scenario to its nmnm Gateway clause in the table below.
- [ ] Binding `ResidencyLease` to `MemHandle` in the nmnm-side codebase is owned by the nmnm-integration scope.
- [ ] Routing the steal boundary through the lease acquire and release edges is owned by the nmnm-integration scope.
- [x] The document states the alloc-count harness that nmnm reuses to enforce the zero-alloc-after-warmup guarantee.

## Contract cross-reference

Each row ties an `examples/gateway_contract.rs` assertion to the nmnm Gateway
clause it discharges.

| Example function | Invariant pinned | nmnm Gateway clause |
|---|---|---|
| `faulted_worker_takes_ready_work` | a parked (faulted) item does not block a later ready item; the ready item completes first | async fault ⇒ worker takes other ready work |
| `waiter_interest_drop_still_residents` | dropping a `PendingToken` cancels interest only; the read still residents the page | cancellation = channel closure, not op cancel |
| `multiple_inflight_misses_do_not_clobber` | concurrent in-flight misses each fill their own frame | overlappable scheduled IO |
| `error_fanout_to_cancelled_and_live_pair` | one singleflight failure fans out `Err(errno)` to the live waiter; the cancelled waiter is unaffected | error surfaces as a value to every live waiter |
| `residency_lease_steal_boundary_stand_in` | the page stays resident across the steal; the destination worker `Hit`s | kernels see only resident borrowed buffers |
| `send_able_handles_cross_the_steal_boundary` | `PageId`/`PendingToken` are Send; `FrameGuard` is not | work items cross threads; buffers do not |

The steal-boundary row runs successfully only because the contract pool applies no
eviction pressure. Under real pressure, concurrent misses claiming every frame can trigger clock victim selection which could select the leased frame and remove its mapping. This would cause the resumed get call to return Pending, resulting in a re-miss. The `ResidencyLease` prevents this by making the frame ineligible for eviction. This guarantee is owned by the `ResidencyLease` surface design and is implemented under the later nmnm-integration scope.
