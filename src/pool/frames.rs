//! Preallocated, sector-aligned, non-moving read-frame arena and the per-frame
//! residency state machine (INV-1). All capacity is fixed at construction; a
//! frame base never moves for the arena's lifetime.

use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::ptr::NonNull;

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
}

/// A fixed set of `count` granule-sized frames in one sector-aligned, non-moving
/// allocation. Frame `i` owns the byte region `[i * granule, (i + 1) * granule)`;
/// its base is sector-aligned because the allocation is and `granule` is a
/// multiple of a sector.
#[derive(Debug)]
pub struct Frames {
    base: NonNull<u8>,
    layout: Layout,
    states: Box<[FrameState]>,
    count: u32,
    granule: u32,
}

// SAFETY: `base` addresses a heap allocation owned solely by this `Frames`; the
// bytes are reached only through `&self`/`&mut self` slice views, so sharing the
// arena across threads is no less sound than sharing any `Box<[u8]>`.
unsafe impl Send for Frames {}
// SAFETY: see the `Send` impl — all access is mediated by Rust's borrow rules
// over `&self`.
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
            states: vec![FrameState::Free; count as usize].into_boxed_slice(),
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
        self.states[self.checked_index(frame)]
    }

    /// Advances `frame` through the residency cycle, storing the new state.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range, or the transition is illegal (INV-1).
    pub fn advance(&mut self, frame: ReadFrameIdx, to: FrameState) {
        let index = self.checked_index(frame);
        self.states[index] = self.states[index].advance(to);
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
