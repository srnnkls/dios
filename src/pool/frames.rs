//! Preallocated, sector-aligned, non-moving read-frame arena and the per-frame
//! residency state machine (INV-1). All capacity is fixed at construction; a
//! frame base never moves for the arena's lifetime.

use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::driver::ReadFrameIdx;
use crate::pool::SECTOR_BYTES;

const SECTOR: usize = SECTOR_BYTES as usize;

/// Residency of one frame. The only legal cycle is
/// `Free → InFlight → Resident → Evicting → Free` (INV-1); any other edge is a
/// programmer error and panics through [`FrameState::advance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameState {
    Free,
    InFlight,
    Resident,
    Evicting,
}

impl FrameState {
    /// Advances to `to`, returning it.
    ///
    /// # Panics
    ///
    /// If `self → to` is not one of the four legal edges of the residency cycle
    /// — the frame state machine admits no other transition (INV-1).
    #[must_use]
    pub fn advance(self, to: FrameState) -> FrameState {
        let legal = matches!(
            (self, to),
            (FrameState::Free, FrameState::InFlight)
                | (FrameState::InFlight, FrameState::Resident)
                | (FrameState::Resident, FrameState::Evicting)
                | (FrameState::Evicting, FrameState::Free)
        );
        assert!(legal, "illegal frame transition {self:?} -> {to:?}");
        to
    }

    fn to_tag(self) -> u8 {
        match self {
            FrameState::Free => 0,
            FrameState::InFlight => 1,
            FrameState::Resident => 2,
            FrameState::Evicting => 3,
        }
    }

    fn from_tag(tag: u8) -> FrameState {
        match tag {
            0 => FrameState::Free,
            1 => FrameState::InFlight,
            2 => FrameState::Resident,
            3 => FrameState::Evicting,
            other => panic!("frame state tag {other} out of range"),
        }
    }
}

/// A fixed set of `count` granule-sized frames in one sector-aligned, non-moving
/// allocation. Frame `i` owns the byte region `[i * granule, (i + 1) * granule)`;
/// its base is sector-aligned because the allocation is and `granule` is a
/// multiple of a sector.
#[derive(Debug)]
pub struct Frames {
    base: NonNull<u8>,
    layout: Layout,
    states: Box<[AtomicU8]>,
    count: u32,
    granule: u32,
}

// SAFETY: `base` addresses a heap allocation owned solely by this `Frames`. A
// read borrow (`frame_bytes`) is handed out only for a Resident/Evicting frame
// and a byte write (`fill`) targets only a Free frame; the residency state
// machine keeps those two disjoint per frame, so no write ever aliases a live
// shared borrow of the same granule. Sharing the arena is thus no less sound
// than sharing any `Box<[u8]>` behind that discipline.
unsafe impl Send for Frames {}
// SAFETY: see the `Send` impl — every frame's residency state lives in an
// `AtomicU8` and its bytes are gated by that state machine.
unsafe impl Sync for Frames {}

impl Frames {
    /// Preallocates `count` frames of `granule` bytes each, sector-aligned and
    /// all `Free`. The whole span is allocated once and never grows.
    ///
    /// # Panics
    ///
    /// If `count` is zero, if `granule` is not a power of two or falls below the
    /// sector floor, or the total span overflows a `Layout`.
    #[must_use]
    pub fn preallocated(count: u32, granule: u32) -> Self {
        assert!(count > 0, "frame count must be positive");
        assert!(granule.is_power_of_two(), "granule must be a power of two");
        assert!(
            granule >= SECTOR_BYTES,
            "granule must not fall below the sector floor"
        );
        let span = (count as usize)
            .checked_mul(granule as usize)
            .expect("frame arena span within isize::MAX");
        let layout = Layout::from_size_align(span, SECTOR).expect("valid frame arena layout");
        // SAFETY: `layout` has a non-zero size (count, granule both positive); a
        // null return is routed to `handle_alloc_error`, so `base` is a live,
        // sector-aligned, zeroed allocation of `span` bytes.
        let ptr = unsafe { alloc_zeroed(layout) };
        let base = NonNull::new(ptr).unwrap_or_else(|| handle_alloc_error(layout));
        Self {
            base,
            layout,
            states: (0..count)
                .map(|_| AtomicU8::new(FrameState::Free.to_tag()))
                .collect(),
            count,
            granule,
        }
    }

    #[must_use]
    pub fn count(&self) -> u32 {
        self.count
    }

    /// The `granule`-byte region backing `frame`. The base never moves.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range for the configured count.
    #[must_use]
    pub fn frame_bytes(&self, frame: ReadFrameIdx) -> &[u8] {
        let index = self.checked_index(frame);
        let offset = index * self.granule as usize;
        // SAFETY: `offset + granule <= span` because `index < count`.
        let start = unsafe { self.base.as_ptr().add(offset) };
        // SAFETY: the region is initialised (zeroed at alloc) and lives as long
        // as `&self`.
        unsafe { std::slice::from_raw_parts(start, self.granule as usize) }
    }

    /// The residency state of `frame`.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range for the configured count.
    #[must_use]
    pub fn state(&self, frame: ReadFrameIdx) -> FrameState {
        FrameState::from_tag(self.states[self.checked_index(frame)].load(Ordering::Relaxed))
    }

    /// Advances `frame` through the residency cycle, storing the new state. Takes
    /// `&self` so the composed pool drives the state machine through a shared
    /// borrow while guards hold read borrows of other frames' bytes. The load
    /// then store is not one atomic RMW: callers serialize it under the pool's
    /// AD-4 lock (poll) or single-writer discipline (miss completion), which T009
    /// loom models; a bare interleave here would be a race.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range, or the transition is illegal (INV-1).
    pub fn advance(&self, frame: ReadFrameIdx, to: FrameState) {
        let index = self.checked_index(frame);
        let current = FrameState::from_tag(self.states[index].load(Ordering::Relaxed));
        self.states[index].store(current.advance(to).to_tag(), Ordering::Relaxed);
    }

    /// Fills `frame`'s whole granule with `byte`, standing in for a read
    /// completion writing the frame's contents.
    ///
    /// # Safety
    ///
    /// No live `frame_bytes` borrow of `frame`'s granule may exist, and no other
    /// thread may touch it, for the duration of the write: `frame` must be
    /// unmapped in the page table (pre-Resident in the miss-completion path), so
    /// `pin` cannot have handed out a guard over these bytes. This raw seam is
    /// superseded by T008's state-gated fill lease.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range.
    pub unsafe fn fill(&self, frame: ReadFrameIdx, byte: u8) {
        let index = self.checked_index(frame);
        let offset = index * self.granule as usize;
        // SAFETY: `offset + granule <= span` because `index < count`.
        let start = unsafe { self.base.as_ptr().add(offset) };
        // SAFETY: the caller contract guarantees no live borrow of this granule,
        // so the write aliases no shared reference.
        unsafe { std::ptr::write_bytes(start, byte, self.granule as usize) };
    }

    fn checked_index(&self, frame: ReadFrameIdx) -> usize {
        let index = frame.get() as usize;
        assert!(index < self.states.len(), "frame index out of range");
        index
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        // SAFETY: `base`/`layout` are exactly the pair returned by `alloc_zeroed`
        // in `preallocated`, freed once here at end of life.
        unsafe { dealloc(self.base.as_ptr(), self.layout) }
    }
}
