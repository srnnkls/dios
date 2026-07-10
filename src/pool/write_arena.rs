//! Granule-aligned write staging for the `O_DIRECT` data plane, separate from the
//! read pool and outside the watermark. One sector-aligned allocation backs
//! `slot_count` granule-sized slots; a slot is leased by [`WriteArena::alloc`]
//! and freed on drop or at the completion drain of the write that consumed it
//! (INV-11). Slots hand out `DerefMut<Target = [u8]>` views into disjoint
//! regions of the shared backing.

use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::Arc;

use crate::pool::SECTOR_BYTES;
use crate::sync::{AtomicBool, Ordering};

const SECTOR: usize = SECTOR_BYTES as usize;

#[derive(Debug)]
struct ArenaState {
    free: Box<[AtomicBool]>,
    base: NonNull<u8>,
    layout: Layout,
    granule: u32,
}

// SAFETY: `base` addresses one heap allocation partitioned into disjoint
// granule regions. At most one live `WriteSlot` exists per index (the `free`
// flag is claimed by an atomic swap and released on drop/lease), so a region is
// never aliased; the pointer is stable for the arena's life. Sharing the arena
// or sending a leased slot across threads therefore cannot create overlapping
// mutable access.
unsafe impl Send for ArenaState {}
// SAFETY: see the `Send` impl — per-slot exclusivity is enforced by `free`.
unsafe impl Sync for ArenaState {}

impl Drop for ArenaState {
    fn drop(&mut self) {
        // SAFETY: `base`/`layout` are the pair returned by `alloc_zeroed` in
        // `with_slots`, freed once here at end of life.
        unsafe { dealloc(self.base.as_ptr(), self.layout) }
    }
}

/// A fixed pool of granule-aligned staging slots over one shared allocation.
#[derive(Debug)]
pub struct WriteArena {
    state: Arc<ArenaState>,
}

impl WriteArena {
    pub(crate) fn new(slot_count: u32) -> Self {
        Self::with_slots(slot_count, crate::pool::GRANULE_DEFAULT)
    }

    /// Builds an arena of `slot_count` slots, each `granule` bytes and
    /// sector-aligned.
    ///
    /// # Panics
    ///
    /// If `slot_count` is zero, if `granule` is not a power of two or falls below
    /// the sector floor, or the total span overflows a `Layout`.
    #[must_use]
    pub fn with_slots(slot_count: u32, granule: u32) -> Self {
        assert!(slot_count > 0, "slot count must be positive");
        assert!(granule.is_power_of_two(), "granule must be a power of two");
        assert!(
            granule >= SECTOR_BYTES,
            "granule must not fall below the sector floor"
        );
        let span = (slot_count as usize)
            .checked_mul(granule as usize)
            .expect("write arena span within isize::MAX");
        let layout = Layout::from_size_align(span, SECTOR).expect("valid write arena layout");
        // SAFETY: `layout` has a non-zero size; a null return routes to
        // `handle_alloc_error`, so `base` is a live, sector-aligned, zeroed
        // allocation of `span` bytes.
        let ptr = unsafe { alloc_zeroed(layout) };
        let base = NonNull::new(ptr).unwrap_or_else(|| handle_alloc_error(layout));
        let mut free = Vec::with_capacity(slot_count as usize);
        for _ in 0..slot_count {
            free.push(AtomicBool::new(true));
        }
        Self {
            state: Arc::new(ArenaState {
                free: free.into_boxed_slice(),
                base,
                layout,
                granule,
            }),
        }
    }

    /// Leases a free staging slot, or `None` when every slot is in use. The
    /// lease borrows the arena; no refcount is taken until a submit consumes it.
    #[must_use]
    pub fn alloc(&self) -> Option<WriteSlot<'_>> {
        for (index, cell) in self.state.free.iter().enumerate() {
            if cell.swap(false, Ordering::AcqRel) {
                let slot = u32::try_from(index).ok()?;
                return Some(WriteSlot {
                    arena: &self.state,
                    slot,
                    consumed: false,
                });
            }
        }
        None
    }
}

/// A leased staging slot, borrowed from its [`WriteArena`]. Dropped unsubmitted
/// it frees at once; consumed by a submit it frees only when the write's
/// completion drains. Derefs to its granule-sized, sector-aligned bytes.
#[derive(Debug)]
pub struct WriteSlot<'arena> {
    arena: &'arena Arc<ArenaState>,
    slot: u32,
    consumed: bool,
}

impl WriteSlot<'_> {
    pub(crate) fn into_lease(mut self) -> WriteLease {
        self.consumed = true;
        WriteLease {
            state: Arc::clone(self.arena),
            slot: self.slot,
            released: false,
        }
    }

    fn region_offset(&self) -> usize {
        let slot = self.slot as usize;
        debug_assert!(slot < self.arena.free.len(), "slot index out of range");
        slot * self.arena.granule as usize
    }
}

impl Deref for WriteSlot<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: `region_offset` asserts `slot < slot_count`, so the product
        // `slot * granule` is at most `(slot_count - 1) * granule`, which fits the
        // `span = slot_count * granule` bytes the constructor allocated without
        // overflow; `add` therefore stays within the single allocation.
        let start = unsafe { self.arena.base.as_ptr().add(self.region_offset()) };
        // SAFETY: `[start, start + granule)` lies within the allocation, was
        // zero-initialised at `alloc_zeroed`, and lives as long as the borrowed
        // arena; the held `free` flag makes this slot the sole reader of the
        // region, so no aliasing `&mut` exists.
        unsafe { std::slice::from_raw_parts(start, self.arena.granule as usize) }
    }
}

impl DerefMut for WriteSlot<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as in `deref` — `region_offset` bounds `slot`, so the offset
        // arithmetic cannot overflow the allocation and `add` stays in bounds.
        let start = unsafe { self.arena.base.as_ptr().add(self.region_offset()) };
        // SAFETY: `[start, start + granule)` is within the allocation, is
        // initialised, and lives as long as the borrowed arena; `&mut self` plus
        // the held `free` flag makes this slot the region's sole owner, so the
        // mutable view is unaliased.
        unsafe { std::slice::from_raw_parts_mut(start, self.arena.granule as usize) }
    }
}

impl Drop for WriteSlot<'_> {
    fn drop(&mut self) {
        if !self.consumed {
            self.arena.free[self.slot as usize].store(true, Ordering::Release);
        }
    }
}

/// Ownership of a leased slot moved into an in-flight write. The slot is freed
/// exactly once — at completion drain via [`WriteLease::release`], or through
/// `Drop` as a teardown net if the write is never drained.
#[derive(Debug)]
pub(crate) struct WriteLease {
    state: Arc<ArenaState>,
    slot: u32,
    released: bool,
}

impl WriteLease {
    pub(crate) fn release(mut self) {
        self.free_once();
    }

    fn free_once(&mut self) {
        if !self.released {
            self.state.free[self.slot as usize].store(true, Ordering::Release);
            self.released = true;
        }
    }
}

impl Drop for WriteLease {
    fn drop(&mut self) {
        self.free_once();
    }
}
