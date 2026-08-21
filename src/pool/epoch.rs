//! Epoch-based reclamation (EBR) for the frame pool: per-reader epoch slots, the
//! publish-before-validate pin guard, and the poll-boundary advance/reclaim ring.
//!
//! A registered reader owns one [`ReaderSlot`] holding a published `local_epoch`
//! (`QUIESCENT` while it holds no guard) and a per-thread live-guard count. The
//! poll caller advances the pool's global epoch only when every registered
//! reader is quiescent or already at the current epoch, then reclaims frames that
//! have aged two full epochs — two advances guarantee every guard that could have
//! observed the old contents has dropped.

use std::collections::VecDeque;
use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::Arc;

use crate::pool::ReadFrameIdx;
use crate::product::LifecycleCounters;
use crate::sync::{AtomicBool, AtomicU64, Ordering, fence};

/// A reader publishes this sentinel as its `local_epoch` while it holds no guard;
/// the advance check reads it as "no constraint on the global epoch".
pub(crate) const QUIESCENT: u64 = u64::MAX;

/// One registered reader's epoch state. A `ReaderCtx` is thread-bound (`!Send`),
/// so only its owning thread writes `local_epoch`/`guard_count`; the atomics
/// carry those values across the shared slot table without a warm-path RMW (a
/// nested pin and a guard drop each do a plain load then store, never a
/// read-modify-write). The poll thread only ever reads `local_epoch`.
#[repr(align(64))]
#[derive(Debug)]
pub(crate) struct ReaderSlot {
    occupied: AtomicBool,
    local_epoch: AtomicU64,
    guard_count: AtomicU64,
}

/// Arc-owned reader registration table. It deliberately retains no frames,
/// driver, or Pool control state, so a registration can safely outlive Pool.
#[derive(Debug)]
pub(crate) struct ReaderRegistry {
    slots: Box<[ReaderSlot]>,
    lifecycle: Arc<LifecycleCounters>,
}

impl ReaderRegistry {
    pub(crate) fn with_capacity(capacity: u32, lifecycle: Arc<LifecycleCounters>) -> Self {
        Self {
            slots: (0..capacity).map(|_| ReaderSlot::vacant()).collect(),
            lifecycle,
        }
    }

    pub(crate) fn slots(&self) -> &[ReaderSlot] {
        &self.slots
    }

    pub(crate) fn register(self: &Arc<Self>) -> Option<ReaderCtx> {
        self.slots.iter().enumerate().find_map(|(index, slot)| {
            slot.try_occupy().then(|| {
                self.lifecycle.register_reader();
                ReaderCtx::new(
                    Arc::clone(self),
                    u32::try_from(index).expect("reader registry indexes by u32"),
                )
            })
        })
    }
}

impl ReaderSlot {
    pub(crate) fn vacant() -> Self {
        Self {
            occupied: AtomicBool::new(false),
            local_epoch: AtomicU64::new(QUIESCENT),
            guard_count: AtomicU64::new(0),
        }
    }

    /// Claims a vacant slot via CAS; a racing registrant that loses sees `false`
    /// and probes on. CAS is preferred over a counter's `fetch_add`-then-rollback
    /// because a rollback transiently publishes a phantom registration that a
    /// concurrent capacity check could observe.
    pub(crate) fn try_occupy(&self) -> bool {
        match self
            .occupied
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        {
            Ok(prior) => {
                debug_assert!(!prior, "a won CAS claims a slot that read vacant");
                true
            }
            Err(prior) => {
                debug_assert!(prior, "a lost CAS leaves an already-occupied slot");
                false
            }
        }
    }

    /// Releases the slot on `ReaderCtx` drop, first resetting the epoch state so a
    /// re-registration reuses a clean slot and a dead reader's stale epoch never
    /// stalls reclamation.
    pub(crate) fn vacate(&self) {
        debug_assert!(
            self.occupied.load(Ordering::Relaxed),
            "vacate releases a slot a live ReaderCtx holds"
        );
        assert_eq!(
            self.guard_count(),
            0,
            "a ReaderCtx drops only after its guards; no live guard outlives its reader"
        );
        self.local_epoch.store(QUIESCENT, Ordering::Release);
        self.guard_count.store(0, Ordering::Relaxed);
        self.occupied.store(false, Ordering::Release);
    }

    fn guard_count(&self) -> u64 {
        self.guard_count.load(Ordering::Relaxed)
    }

    /// Publishes the reader's epoch before the pin validates the frame, but only
    /// on its FIRST live guard — a nested pin finds a non-zero count and keeps the
    /// epoch already published by the outer guard. Returns whether it published.
    pub(crate) fn begin_pin(&self, global_epoch: u64) -> bool {
        assert!(
            global_epoch != QUIESCENT,
            "a published epoch is never the quiescent sentinel"
        );
        let first = self.guard_count() == 0;
        if first {
            self.local_epoch.store(global_epoch, Ordering::Release);
            // Store-buffer hazard: without a full barrier the publish store may
            // reorder after the pin's following table lookup and Resident load, so
            // a concurrent poller's Acquire scan reads a stale QUIESCENT, advances
            // twice, and reclaims a frame this guard still derefs. The SeqCst fence
            // pairs with the poll-side `permits_advance` scan (crossbeam-epoch pins
            // the same way). The interleaving proof is the T009 loom model.
            fence(Ordering::SeqCst);
        }
        first
    }

    /// Commits a validated pin, counting one more live guard for this reader.
    pub(crate) fn commit_pin(&self) {
        let count = self.guard_count();
        assert!(count < u64::MAX, "reader live-guard count within u64");
        self.guard_count.store(count + 1, Ordering::Relaxed);
    }

    /// Abandons a first pin whose validation failed: no guard was minted, so the
    /// just-published epoch must go back to quiescent or it would stall the
    /// advance forever.
    pub(crate) fn abort_pin(&self) {
        debug_assert_eq!(self.guard_count(), 0, "abort only before the first commit");
        self.local_epoch.store(QUIESCENT, Ordering::Release);
    }

    /// Drops one live guard; the reader goes quiescent on its last guard so a
    /// published epoch never outlives the guards that justified it.
    pub(crate) fn release_guard(&self) {
        let count = self.guard_count();
        assert!(count > 0, "a released guard was previously committed");
        let remaining = count - 1;
        self.guard_count.store(remaining, Ordering::Relaxed);
        if remaining == 0 {
            self.local_epoch.store(QUIESCENT, Ordering::Release);
        }
    }

    /// Whether this reader lets the global epoch advance past `global_epoch`: a
    /// vacant or quiescent reader is no constraint; an active one must already sit
    /// at the current epoch.
    pub(crate) fn permits_advance(&self, global_epoch: u64) -> bool {
        assert!(
            global_epoch != QUIESCENT,
            "the pool's global epoch is a real counter, never the sentinel"
        );
        let local = self.local_epoch.load(Ordering::Acquire);
        debug_assert!(
            local == QUIESCENT || local <= global_epoch,
            "a published local epoch never runs ahead of the global epoch"
        );
        local == QUIESCENT || local == global_epoch
    }
}

/// Advances `global_epoch` by one iff every reader permits, returning the epoch in
/// force after the attempt. Run single-writer under the AD-4 control lock; a reader
/// pinned at the current epoch blocks the advance and thereby stalls reclamation of
/// a frame under its live guard (INV-1).
pub(crate) fn advance_epoch(global_epoch: &AtomicU64, slots: &[ReaderSlot]) -> u64 {
    let epoch = global_epoch.load(Ordering::Acquire);
    // Symmetric half of the store-buffer litmus `begin_pin` opens: a reader
    // publishes `local_epoch` then `SeqCst`-fences before reading residency, so the
    // poller must `SeqCst`-fence before reading `local_epoch` or it can observe a
    // stale QUIESCENT and advance past a live guard (crossbeam-epoch fences its
    // collector the same way). Proven by the T009 grace-period loom model.
    fence(Ordering::SeqCst);
    let permitted = slots.iter().all(|slot| slot.permits_advance(epoch));
    if permitted {
        global_epoch.store(epoch + 1, Ordering::Release);
    }
    let advanced = global_epoch.load(Ordering::Acquire);
    debug_assert!(
        advanced == epoch || advanced == epoch + 1,
        "a single advance pass moves the epoch by at most one"
    );
    advanced
}

/// The fixed-capacity ring of frames awaiting reclamation, each tagged with the
/// global epoch at eviction. Capacity is the frame count — the most frames that
/// can be `Evicting` at once — so it never grows after construction.
#[derive(Debug)]
pub(crate) struct EvictQueue {
    entries: VecDeque<(ReadFrameIdx, u64)>,
    capacity: u32,
}

impl EvictQueue {
    pub(crate) fn with_capacity(capacity: u32) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity as usize),
            capacity,
        }
    }

    pub(crate) fn push(&mut self, frame: ReadFrameIdx, tagged_epoch: u64) {
        assert!(
            self.entries.len() < self.capacity as usize,
            "evict queue within its frame-count bound"
        );
        debug_assert!(
            self.entries
                .back()
                .is_none_or(|&(_, last)| tagged_epoch >= last),
            "evict tags enqueue in non-decreasing epoch order, so the front matures first"
        );
        self.entries.push_back((frame, tagged_epoch));
    }

    #[cfg(feature = "bench")]
    pub(crate) fn oldest_tagged_epoch(&self) -> Option<u64> {
        self.entries.front().map(|entry| entry.1)
    }

    /// Reclaims every frame that has aged two full epochs, running `reclaim` for
    /// each. Entries are tagged in non-decreasing epoch order, so the front
    /// matures first and a not-yet-matured front ends the pass.
    pub(crate) fn drain_matured<F: FnMut(ReadFrameIdx)>(
        &mut self,
        global_epoch: u64,
        mut reclaim: F,
    ) -> usize {
        let mut reclaimed = 0usize;
        while let Some(&(frame, tagged_epoch)) = self.entries.front() {
            if tagged_epoch.saturating_add(2) > global_epoch {
                break;
            }
            assert!(
                tagged_epoch.saturating_add(2) <= global_epoch,
                "a reclaimed frame has aged two full epochs past its evict tag"
            );
            self.entries.pop_front();
            reclaim(frame);
            reclaimed += 1;
        }
        reclaimed
    }
}

/// Lifetime-free per-reader epoch handle. The small registry is Arc-owned while
/// Pool bytes and driver resources remain exclusively Pool-owned.
#[derive(Debug)]
pub struct ReaderCtx {
    registry: Arc<ReaderRegistry>,
    slot: u32,
    _thread_bound: PhantomData<*const ()>,
}

impl ReaderCtx {
    fn new(registry: Arc<ReaderRegistry>, slot: u32) -> Self {
        Self {
            registry,
            slot,
            _thread_bound: PhantomData,
        }
    }

    pub(crate) fn slot(&self) -> &ReaderSlot {
        &self.registry.slots[self.slot as usize]
    }

    pub(crate) fn belongs_to(&self, registry: &Arc<ReaderRegistry>) -> bool {
        Arc::ptr_eq(&self.registry, registry)
    }
}

impl Drop for ReaderCtx {
    fn drop(&mut self) {
        self.slot().vacate();
        self.registry.lifecycle.release_reader();
    }
}

/// Epoch-pinned read access to a resident frame: `Deref<Target = [u8]>` over the
/// whole granule, `!Send` (the borrow is thread-bound). While it lives the
/// reader's epoch stays published, so its frame cannot be reclaimed; dropping the
/// last guard releases the epoch.
///
/// The three `compile_fail` blocks below pin the guard/reader lifetime and thread
/// invariants (INV-6, EBR per-thread slot) as library doctests.
///
/// A guard's borrow must not outlive the pool that minted it:
///
/// ```compile_fail
/// use dios::{FrameGuard, Get, PageId, Pool, ReaderCtx};
/// fn escapes<'pool>(
///     pool: &'pool Pool,
///     reader: &'pool ReaderCtx,
///     page: PageId,
/// ) -> FrameGuard<'static> {
///     match pool.get(reader, page).expect("the file remains live") {
///         Get::Hit(guard) => guard, // borrows `pool`; cannot escape as 'static
///         Get::Pending(_) | Get::Busy => panic!("the lifetime is the contract"),
///     }
/// }
/// ```
///
/// A `ReaderCtx` cannot cross a thread boundary (EBR per-thread slot):
///
/// ```compile_fail
/// use dios::Pool;
/// let pool = Pool::builder()
///     .frame_count(16).granule(4096)
///     .max_concurrent_readers(1).peak_guards_per_reader(1)
///     .max_inflight_reads(1).miss_headroom(3)
///     .build().unwrap();
/// let reader = pool.register_reader().unwrap();
/// std::thread::spawn(move || {
///     drop(reader); // ReaderCtx is !Send — a consuming use forces the move, which must not compile
/// });
/// ```
///
/// A `ReaderCtx` owns its bounded registration metadata and may outlive the
/// pool it was registered against:
///
/// ```no_run
/// use dios::{Pool, ReaderCtx};
/// fn outlives() -> ReaderCtx {
///     let pool = Pool::builder()
///         .frame_count(16).granule(4096)
///         .max_concurrent_readers(1).peak_guards_per_reader(1)
///         .max_inflight_reads(1).miss_headroom(3)
///         .build().unwrap();
///     pool.register_reader().unwrap()
/// }
/// ```
#[derive(Debug)]
pub struct FrameGuard<'pool> {
    bytes: &'pool [u8],
    slot: &'pool ReaderSlot,
    _thread_bound: PhantomData<*const ()>,
}

impl<'pool> FrameGuard<'pool> {
    pub(crate) fn new(bytes: &'pool [u8], slot: &'pool ReaderSlot) -> Self {
        Self {
            bytes,
            slot,
            _thread_bound: PhantomData,
        }
    }
}

impl Deref for FrameGuard<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.bytes
    }
}

impl Drop for FrameGuard<'_> {
    fn drop(&mut self) {
        self.slot.release_guard();
    }
}
