//! Driver-owned, granule-aligned staging for the direct-I/O write plane.
//!
//! One allocation is fixed at driver initialization and, on Linux, registered
//! as buffer index 1 beside the read arena. [`WriteArena`] is only a borrowing
//! facade: callers cannot construct an unregistered arena or extend its life
//! beyond the driver. An admitted write records a slot index in the driver's
//! fixed completion slab; no ownership pointer or refcount crosses the hot path.

use std::alloc::{alloc_zeroed, dealloc, handle_alloc_error, Layout};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use crate::driver::Driver;
use crate::sync::{AtomicBool, AtomicU32, Condvar, Mutex, Ordering};

#[derive(Debug)]
pub(crate) struct ArenaState {
    free: Box<[AtomicBool]>,
    base: NonNull<u8>,
    layout: Layout,
    granule: u32,
    owner: u64,
    wait_gate: Mutex<()>,
    waiters: AtomicU32,
    released: Condvar,
}

// SAFETY: `base` addresses one stable allocation partitioned into disjoint
// granule regions. The atomic free bit admits at most one `WriteSlot` per index,
// and an admitted write retains that bit until completion drain.
unsafe impl Send for ArenaState {}
// SAFETY: see `Send`; every shared mutation is atomic or mutex-protected and
// disjoint slot regions never alias as mutable references.
unsafe impl Sync for ArenaState {}

impl ArenaState {
    #[must_use]
    pub(crate) fn preallocated(slot_count: u32, granule: u32, owner: u64) -> Self {
        assert!(slot_count > 0, "write slot count must be positive");
        assert!(granule.is_power_of_two(), "granule must be a power of two");
        let span = (slot_count as usize)
            .checked_mul(granule as usize)
            .expect("write arena span within isize::MAX");
        let layout = Layout::from_size_align(span, granule as usize)
            .expect("valid granule-aligned write arena layout");
        // SAFETY: `layout` is non-empty and valid. A null result is routed to the
        // allocation error handler, leaving a live zeroed allocation.
        let ptr = unsafe { alloc_zeroed(layout) };
        let base = NonNull::new(ptr).unwrap_or_else(|| handle_alloc_error(layout));
        Self {
            free: (0..slot_count).map(|_| AtomicBool::new(true)).collect(),
            base,
            layout,
            granule,
            owner,
            wait_gate: Mutex::new(()),
            waiters: AtomicU32::new(0),
            released: Condvar::new(),
        }
    }

    pub(crate) fn alloc(&self) -> Option<WriteSlot<'_>> {
        for (index, cell) in self.free.iter().enumerate() {
            if cell.swap(false, Ordering::AcqRel) {
                return Some(WriteSlot {
                    state: self,
                    slot: u32::try_from(index).expect("write slot index fits u32"),
                    consumed: false,
                });
            }
        }
        None
    }

    pub(crate) fn assert_owner(&self, owner: u64) {
        assert_eq!(self.owner, owner, "write slot used with a foreign driver");
    }

    pub(crate) fn region(&self, slot: u32) -> (*const u8, u32) {
        let index = slot as usize;
        assert!(index < self.free.len(), "write slot index within its arena");
        let offset = index * self.granule as usize;
        assert!(
            offset + self.granule as usize <= self.layout.size(),
            "write slot region lies within its arena"
        );
        // SAFETY: the checked slot range is within this allocation. The occupied
        // free bit keeps the region stable and unavailable to another lease.
        let source = unsafe { self.base.as_ptr().add(offset) };
        (source.cast_const(), self.granule)
    }

    pub(crate) fn release(&self, slot: u32) {
        let index = slot as usize;
        assert!(
            index < self.free.len(),
            "released write slot index is valid"
        );
        let was_free = self.free[index].swap(true, Ordering::AcqRel);
        assert!(!was_free, "a write slot is released exactly once");
        if self.waiters.load(Ordering::Acquire) > 0 {
            let gate = self
                .wait_gate
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            drop(gate);
            self.released.notify_one();
        }
    }

    pub(crate) fn wait_for_release(&self, timeout: Duration) {
        let gate = self
            .wait_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let prior_waiters = self.waiters.fetch_add(1, Ordering::AcqRel);
        assert!(
            prior_waiters < u32::MAX,
            "write waiter count does not exhaust"
        );
        if self.free.iter().any(|slot| slot.load(Ordering::Acquire)) {
            let prior_waiters = self.waiters.fetch_sub(1, Ordering::AcqRel);
            assert!(prior_waiters > 0, "registered write waiter is removed once");
            return;
        }
        let (gate, _) = self
            .released
            .wait_timeout(gate, timeout)
            .unwrap_or_else(PoisonError::into_inner);
        let prior_waiters = self.waiters.fetch_sub(1, Ordering::AcqRel);
        assert!(prior_waiters > 0, "registered write waiter is removed once");
        drop(gate);
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn base_ptr(&self) -> *mut u8 {
        self.base.as_ptr()
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn span_len(&self) -> usize {
        self.layout.size()
    }
}

impl Drop for ArenaState {
    fn drop(&mut self) {
        // SAFETY: `base` and `layout` are the pair returned by `alloc_zeroed`
        // and are freed exactly once after the driver has quiesced.
        unsafe { dealloc(self.base.as_ptr(), self.layout) }
    }
}

/// A borrowing view of the staging slots owned and registered by a [`Driver`].
#[derive(Debug, Clone, Copy)]
pub struct WriteArena<'driver> {
    driver: &'driver Driver,
}

impl<'driver> WriteArena<'driver> {
    pub(crate) fn new(driver: &'driver Driver) -> Self {
        Self { driver }
    }

    /// Leases a staging slot, or returns `None` when all fixed slots are held.
    #[must_use]
    pub fn alloc(&self) -> Option<WriteSlot<'driver>> {
        self.driver.alloc_write_slot()
    }

    /// Waits at most `timeout` for a slot. The eager backend pumps admitted work
    /// on this thread; the ring backend parks until a completion drain releases
    /// a slot.
    #[must_use]
    pub fn alloc_wait(&self, timeout: Duration) -> Option<WriteSlot<'driver>> {
        self.driver.alloc_write_slot_wait(timeout)
    }
}

/// An exclusive mutable lease of one driver-owned staging granule.
#[derive(Debug)]
pub struct WriteSlot<'driver> {
    state: &'driver ArenaState,
    slot: u32,
    consumed: bool,
}

impl WriteSlot<'_> {
    pub(crate) fn assert_owner(&self, owner: u64) {
        self.state.assert_owner(owner);
    }

    pub(crate) fn into_index(mut self) -> u32 {
        self.consumed = true;
        self.slot
    }

    fn region_offset(&self) -> usize {
        let slot = self.slot as usize;
        debug_assert!(slot < self.state.free.len(), "slot index out of range");
        slot * self.state.granule as usize
    }
}

impl Deref for WriteSlot<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        let start = self.region_offset();
        // SAFETY: the checked slot offset stays within the arena allocation.
        let start = unsafe { self.state.base.as_ptr().add(start) };
        // SAFETY: the slot index is in range, this lease holds its free bit, and
        // the complete granule is initialized within the arena allocation.
        unsafe { std::slice::from_raw_parts(start, self.state.granule as usize) }
    }
}

impl DerefMut for WriteSlot<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        let start = self.region_offset();
        // SAFETY: the checked slot offset stays within the arena allocation.
        let start = unsafe { self.state.base.as_ptr().add(start) };
        // SAFETY: this unique slot lease is the only accessor to its occupied,
        // initialized granule, whose checked offset is inside the allocation.
        unsafe { std::slice::from_raw_parts_mut(start, self.state.granule as usize) }
    }
}

impl Drop for WriteSlot<'_> {
    fn drop(&mut self) {
        if !self.consumed {
            self.state.release(self.slot);
        }
    }
}

pub(crate) fn shared(slot_count: u32, granule: u32, owner: u64) -> Arc<ArenaState> {
    Arc::new(ArenaState::preallocated(slot_count, granule, owner))
}
