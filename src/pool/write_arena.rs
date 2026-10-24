//! Driver-owned, granule-aligned staging for the direct-I/O write plane.
//!
//! One allocation is fixed at driver initialization and, on Linux, registered
//! as buffer index 1 beside the read arena. [`WriteArena`] is only a borrowing
//! facade: callers cannot construct an unregistered arena or extend its life
//! beyond the driver. An admitted write records a slot index in the driver's
//! fixed completion slab; no ownership pointer or refcount crosses the hot path.

use std::alloc::{Layout, dealloc, handle_alloc_error};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use crate::allocation::REGISTERED_CHUNK_BYTES;
use crate::driver::Driver;
use crate::sync::{AtomicBool, AtomicU32, Condvar, Mutex, Ordering};

/// The most adjacent slots one write may carry: 128 KiB at a 4 KiB granule,
/// where appending writes on the measured disks stop gaining from size.
pub const WRITE_RUN_SLOTS_MAX: u32 = 32;

/// Adjacent arena slots leased, written and released together; a single slot is
/// a run of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlotRun {
    first: u32,
    count: u32,
}

impl SlotRun {
    fn slots(self) -> std::ops::Range<u32> {
        self.first..self.first + self.count
    }
}

/// Whether `first..first + count` stays inside one registered chunk, so a run
/// is one contiguous registered buffer.
fn run_within_one_chunk(first: u32, count: u32, slots_per_chunk: u32) -> bool {
    assert!(count >= 1, "a run holds at least one slot");
    first / slots_per_chunk == (first + count - 1) / slots_per_chunk
}

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
    pub(crate) fn try_preallocated(slot_count: u32, granule: u32, owner: u64) -> Option<Self> {
        assert!(slot_count > 0, "write slot count must be positive");
        assert!(granule.is_power_of_two(), "granule must be a power of two");
        let span = (slot_count as usize)
            .checked_mul(granule as usize)
            .expect("write arena span within isize::MAX");
        let layout = Layout::from_size_align(span, granule as usize)
            .expect("valid granule-aligned write arena layout");
        let free = crate::allocation::try_boxed_slice_with(slot_count, || AtomicBool::new(true))?;
        let base = crate::allocation::allocate_zeroed(layout)?;
        Some(Self {
            free,
            base,
            layout,
            granule,
            owner,
            wait_gate: Mutex::new(()),
            waiters: AtomicU32::new(0),
            released: Condvar::new(),
        })
    }

    #[must_use]
    pub(crate) fn preallocated(slot_count: u32, granule: u32, owner: u64) -> Self {
        Self::try_preallocated(slot_count, granule, owner).unwrap_or_else(|| {
            let span = (slot_count as usize)
                .checked_mul(granule as usize)
                .expect("write arena span within isize::MAX");
            let layout = Layout::from_size_align(span, granule as usize)
                .expect("valid granule-aligned write arena layout");
            handle_alloc_error(layout)
        })
    }

    fn slot_count(&self) -> u32 {
        u32::try_from(self.free.len()).expect("arena slot count fits u32")
    }

    pub(crate) fn alloc(&self) -> Option<WriteSlot<'_>> {
        self.alloc_run_within(self.slot_count(), 1)
    }

    /// Leases `count` adjacent slots below `limit` inside one registered chunk,
    /// or none; a race lost on any slot of a window releases the window's slots.
    pub(crate) fn alloc_run_within(&self, limit: u32, count: u32) -> Option<WriteSlot<'_>> {
        assert!(count >= 1, "a run holds at least one slot");
        assert!(
            count <= WRITE_RUN_SLOTS_MAX,
            "a run of {count} slots exceeds the run bound {WRITE_RUN_SLOTS_MAX}"
        );
        let limit = limit.min(self.slot_count());
        let slots_per_chunk = self.slots_per_chunk();
        let mut first = 0u32;
        while first.checked_add(count).is_some_and(|end| end <= limit) {
            if !run_within_one_chunk(first, count, slots_per_chunk) {
                first = (first / slots_per_chunk + 1) * slots_per_chunk;
                continue;
            }
            match self.claim_run(first, count) {
                Ok(()) => {
                    return Some(WriteSlot {
                        state: self,
                        run: SlotRun { first, count },
                        consumed: false,
                    });
                }
                Err(taken) => first = taken + 1,
            }
        }
        None
    }

    fn slots_per_chunk(&self) -> u32 {
        u32::try_from(REGISTERED_CHUNK_BYTES / self.granule as usize)
            .expect("a chunk holds at most u32::MAX granules")
    }

    /// Claims every slot of `first..first + count`, or releases the ones it
    /// claimed and names the first slot it found taken.
    fn claim_run(&self, first: u32, count: u32) -> Result<(), u32> {
        for slot in first..first + count {
            if !self.free[slot as usize].swap(false, Ordering::AcqRel) {
                for claimed in first..slot {
                    self.release(claimed);
                }
                return Err(slot);
            }
        }
        Ok(())
    }

    pub(crate) fn assert_owner(&self, owner: u64) {
        assert_eq!(self.owner, owner, "write slot used with a foreign driver");
    }

    pub(crate) fn region(&self, run: SlotRun, source_offset: u32, len: u32) -> *const u8 {
        let index = run.first as usize;
        assert!(
            run.first + run.count <= self.slot_count(),
            "write run within its arena"
        );
        let run_bytes = run.count * self.granule;
        assert!(
            source_offset <= run_bytes,
            "write source starts within its run"
        );
        assert!(
            len <= run_bytes - source_offset,
            "write source range lies within its run"
        );
        let offset = index * self.granule as usize + source_offset as usize;
        assert!(
            offset + len as usize <= self.layout.size(),
            "write slot region lies within its arena"
        );
        // SAFETY: the checked run range is within this allocation. The occupied
        // free bits keep the region stable and unavailable to another lease.
        let source = unsafe { self.base.as_ptr().add(offset) };
        source.cast_const()
    }

    pub(crate) fn release_run(&self, run: SlotRun) {
        for slot in run.slots() {
            self.release(slot);
        }
    }

    fn release(&self, slot: u32) {
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

    pub(crate) fn base_ptr(&self) -> *mut u8 {
        self.base.as_ptr()
    }

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

    /// Leases `count` adjacent slots that one write carries as a single
    /// operation, or returns `None` when no such window is free.
    ///
    /// # Panics
    ///
    /// If `count` is zero or above [`WRITE_RUN_SLOTS_MAX`].
    #[must_use]
    pub fn alloc_run(&self, count: u32) -> Option<WriteSlot<'driver>> {
        self.driver.alloc_write_run(count)
    }

    /// Waits at most `timeout` for a slot. The eager backend pumps admitted work
    /// on this thread; the ring backend parks until a completion drain releases
    /// a slot.
    #[must_use]
    pub fn alloc_wait(&self, timeout: Duration) -> Option<WriteSlot<'driver>> {
        self.driver.alloc_write_slot_wait(timeout)
    }
}

/// An exclusive mutable lease of one driver-owned staging granule, or of a run
/// of adjacent granules that one write carries.
#[derive(Debug)]
pub struct WriteSlot<'driver> {
    state: &'driver ArenaState,
    run: SlotRun,
    consumed: bool,
}

impl WriteSlot<'_> {
    pub(crate) fn assert_owner(&self, owner: u64) {
        self.state.assert_owner(owner);
    }

    pub(crate) fn into_run(mut self) -> SlotRun {
        self.consumed = true;
        self.run
    }

    /// The adjacent granules this lease spans.
    #[must_use]
    pub fn slot_count(&self) -> u32 {
        self.run.count
    }

    /// Grows this lease in place to `count` adjacent granules, below `limit`,
    /// by claiming the slots after it; its bytes stay where they are. Returns
    /// `false` and leaves the lease unchanged when one of those slots is taken
    /// or the run would leave its registered chunk or the limit.
    ///
    /// # Panics
    ///
    /// If `count` is shorter than the lease or above [`WRITE_RUN_SLOTS_MAX`].
    pub(crate) fn try_extend_within(&mut self, limit: u32, count: u32) -> bool {
        assert!(
            count >= self.run.count,
            "a lease of {} granules cannot grow to {count}",
            self.run.count
        );
        assert!(
            count <= WRITE_RUN_SLOTS_MAX,
            "a run of {count} slots exceeds the run bound {WRITE_RUN_SLOTS_MAX}"
        );
        let first = self.run.first;
        let limit = limit.min(self.state.slot_count());
        let fits = first.checked_add(count).is_some_and(|end| end <= limit)
            && run_within_one_chunk(first, count, self.state.slots_per_chunk());
        if !fits {
            return false;
        }
        if count == self.run.count {
            return true;
        }
        let claimed = self
            .state
            .claim_run(first + self.run.count, count - self.run.count)
            .is_ok();
        if claimed {
            self.run.count = count;
        }
        claimed
    }

    /// Keeps the first `count` granules and releases the rest, so a run that
    /// filled only a prefix writes only that prefix.
    ///
    /// # Panics
    ///
    /// If `count` is zero or longer than the lease.
    pub fn truncate(&mut self, count: u32) {
        assert!(count >= 1, "a lease keeps at least one granule");
        assert!(
            count <= self.run.count,
            "a lease of {} granules cannot keep {count}",
            self.run.count
        );
        for slot in self.run.first + count..self.run.first + self.run.count {
            self.state.release(slot);
        }
        self.run.count = count;
    }

    fn region_offset(&self) -> usize {
        let slot = self.run.first as usize;
        debug_assert!(
            slot + self.run.count as usize <= self.state.free.len(),
            "run out of range"
        );
        slot * self.state.granule as usize
    }

    fn region_len(&self) -> usize {
        self.run.count as usize * self.state.granule as usize
    }
}

impl Deref for WriteSlot<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        let start = self.region_offset();
        // SAFETY: the checked slot offset stays within the arena allocation.
        let start = unsafe { self.state.base.as_ptr().add(start) };
        // SAFETY: the run is in range, this lease holds its free bits, and its
        // complete granules are initialized within the arena allocation.
        unsafe { std::slice::from_raw_parts(start, self.region_len()) }
    }
}

impl DerefMut for WriteSlot<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        let start = self.region_offset();
        // SAFETY: the checked slot offset stays within the arena allocation.
        let start = unsafe { self.state.base.as_ptr().add(start) };
        // SAFETY: this unique lease is the only accessor to its occupied,
        // initialized granules, whose checked offset is inside the allocation.
        unsafe { std::slice::from_raw_parts_mut(start, self.region_len()) }
    }
}

impl Drop for WriteSlot<'_> {
    fn drop(&mut self) {
        if !self.consumed {
            self.state.release_run(self.run);
        }
    }
}

pub(crate) fn shared(slot_count: u32, granule: u32, owner: u64) -> Arc<ArenaState> {
    Arc::new(ArenaState::preallocated(slot_count, granule, owner))
}

pub(crate) fn try_shared(slot_count: u32, granule: u32, owner: u64) -> Option<Arc<ArenaState>> {
    Some(Arc::new(ArenaState::try_preallocated(
        slot_count, granule, owner,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRANULE: u32 = 4096;

    fn arena(slots: u32) -> ArenaState {
        ArenaState::preallocated(slots, GRANULE, 7)
    }

    fn first_of(slot: &WriteSlot<'_>) -> u32 {
        slot.run.first
    }

    #[test]
    fn a_run_claims_adjacent_slots_as_one_contiguous_region() {
        let arena = arena(8);
        let run = arena.alloc_run_within(8, 3).expect("three free slots");
        assert_eq!(run.run, SlotRun { first: 0, count: 3 });
        assert_eq!(run.len(), 3 * GRANULE as usize, "the lease spans the run");
        let single = arena.alloc().expect("a fourth slot");
        assert_eq!(first_of(&single), 3, "a single slot is a run of one");
        let rest = arena.alloc_run_within(8, 4).expect("the last four slots");
        assert_eq!(rest.run, SlotRun { first: 4, count: 4 });
        assert!(arena.alloc().is_none(), "every slot is leased");
        drop(run);
        let again = arena.alloc_run_within(8, 3).expect("the released run");
        assert_eq!(again.run, SlotRun { first: 0, count: 3 });
    }

    #[test]
    fn a_window_with_a_taken_slot_releases_what_it_claimed() {
        let arena = arena(6);
        let low = arena.alloc_run_within(6, 2).expect("slots 0 and 1");
        let held = arena.alloc().expect("slot 2");
        assert_eq!(first_of(&held), 2);
        drop(low);
        let run = arena.alloc_run_within(6, 3).expect("slots 3..6");
        assert_eq!(
            run.run,
            SlotRun { first: 3, count: 3 },
            "the window over the held slot is skipped"
        );
        let freed = arena.alloc_run_within(6, 2).expect("slots 0 and 1 again");
        assert_eq!(
            freed.run,
            SlotRun { first: 0, count: 2 },
            "the skipped window's claimed slots were released"
        );
        assert!(arena.alloc_run_within(6, 1).is_none());
    }

    #[test]
    fn a_run_honours_the_enabled_limit() {
        let arena = arena(8);
        assert!(
            arena.alloc_run_within(3, 4).is_none(),
            "four slots exceed a limit of three"
        );
        let run = arena
            .alloc_run_within(4, 4)
            .expect("exactly the enabled slots");
        assert_eq!(run.run, SlotRun { first: 0, count: 4 });
    }

    #[test]
    fn a_run_never_spans_a_registered_chunk() {
        assert!(run_within_one_chunk(0, 8, 8));
        assert!(run_within_one_chunk(8, 4, 8));
        assert!(run_within_one_chunk(7, 1, 8));
        assert!(
            !run_within_one_chunk(6, 4, 8),
            "slots 6..10 straddle the boundary at 8"
        );
        assert!(!run_within_one_chunk(7, 2, 8));
    }

    #[test]
    fn truncating_a_run_releases_its_tail_and_shortens_the_lease() {
        let arena = arena(8);
        let mut run = arena.alloc_run_within(8, 6).expect("six free slots");
        run.truncate(2);
        assert_eq!(run.slot_count(), 2);
        assert_eq!(
            run.len(),
            2 * GRANULE as usize,
            "the lease covers the kept prefix"
        );
        let tail = arena
            .alloc_run_within(8, 6)
            .expect("the released tail and the free rest");
        assert_eq!(tail.run, SlotRun { first: 2, count: 6 });
        drop(run);
        let head = arena.alloc_run_within(8, 2).expect("the dropped prefix");
        assert_eq!(head.run, SlotRun { first: 0, count: 2 });
    }

    #[test]
    fn extending_a_lease_claims_the_slots_after_it_or_nothing() {
        let arena = arena(8);
        let mut lease = arena.alloc().expect("slot 0");
        lease[0] = 0x7e;
        assert!(lease.try_extend_within(8, 4), "slots 1..4 are free");
        assert_eq!(lease.run, SlotRun { first: 0, count: 4 });
        assert_eq!(lease[0], 0x7e, "the leased bytes stay in place");
        let blocker = arena.alloc_run_within(8, 1).expect("slot 4");
        assert_eq!(first_of(&blocker), 4);
        assert!(!lease.try_extend_within(8, 6), "slot 4 is taken");
        assert_eq!(
            lease.run,
            SlotRun { first: 0, count: 4 },
            "a refused growth changes nothing"
        );
        assert!(
            arena
                .alloc_run_within(8, 3)
                .is_some_and(|tail| tail.run == SlotRun { first: 5, count: 3 }),
            "the refused growth released what it claimed"
        );
        assert!(
            !lease.try_extend_within(3, 5),
            "the limit bounds the growth"
        );
    }

    #[test]
    #[should_panic(expected = "keeps at least one granule")]
    fn truncating_to_nothing_is_a_programmer_error() {
        let arena = arena(4);
        let mut run = arena.alloc_run_within(4, 2).expect("two free slots");
        run.truncate(0);
    }

    #[test]
    #[should_panic(expected = "exceeds the run bound")]
    fn a_run_above_the_bound_is_a_programmer_error() {
        let arena = arena(WRITE_RUN_SLOTS_MAX + 1);
        let _ = arena.alloc_run_within(WRITE_RUN_SLOTS_MAX + 1, WRITE_RUN_SLOTS_MAX + 1);
    }
}
