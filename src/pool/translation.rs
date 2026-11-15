//! Per-file array translation: one entry per granule per registered file slot,
//! bounded by `file_granules_max`.
//!
//! This scaffold carries the geometry and the asserting surface; it backs no
//! entries, so every lookup reads absent.

use crate::driver::FileId;
use crate::pool::{PageId, ReadFrameIdx};

/// Largest frame index an entry can name: the entry stores `frame + 1` in 32
/// bits and keeps zero for absent.
const FRAME_INDEX_MAX: u32 = (1 << 30) - 1;

const ENTRY_BYTES: u64 = 4;

#[derive(Debug)]
pub struct Translation {
    registered_file_capacity: u32,
    file_granules_max: u32,
}

impl Translation {
    /// Reserves one entry per granule per registered file slot; `None` when
    /// the mapping's byte length overflows `u64`.
    #[must_use]
    pub fn try_reserve(registered_file_capacity: u32, file_granules_max: u32) -> Option<Self> {
        u64::from(registered_file_capacity)
            .checked_mul(u64::from(file_granules_max))?
            .checked_mul(ENTRY_BYTES)?;
        Some(Self {
            registered_file_capacity,
            file_granules_max,
        })
    }

    #[must_use]
    pub fn entry_count(&self) -> u64 {
        u64::from(self.registered_file_capacity) * u64::from(self.file_granules_max)
    }

    /// Resolves `page` to the frame its entry names.
    ///
    /// # Panics
    ///
    /// If `page` lies outside the reserved slots or at or above
    /// `file_granules_max`.
    #[must_use]
    pub fn lookup(&self, page: PageId) -> Option<ReadFrameIdx> {
        self.assert_addressable(page);
        None
    }

    /// Sets the absent entry of `page` to `frame`.
    ///
    /// # Panics
    ///
    /// If `page` is not addressable, its entry is already set, or `frame` is
    /// above the largest representable frame index.
    pub fn publish(&self, page: PageId, frame: ReadFrameIdx) {
        assert!(
            frame.get() <= FRAME_INDEX_MAX,
            "an entry names a frame below 2^30"
        );
        assert_eq!(self.lookup(page), None, "publish names an absent entry");
    }

    /// Clears the set entry of `page` and returns the frame it named.
    ///
    /// # Panics
    ///
    /// If `page` is not addressable or its entry is absent.
    #[must_use]
    pub fn clear(&self, page: PageId) -> ReadFrameIdx {
        self.lookup(page).expect("clear names a set entry")
    }

    /// Releases the backing of `file`'s slot; every entry is already absent.
    ///
    /// # Panics
    ///
    /// If `file`'s slot lies outside the reservation.
    pub fn release_slot(&self, file: FileId) {
        assert!(
            file.slot() < self.registered_file_capacity,
            "a released slot lies inside the reservation"
        );
    }

    #[must_use]
    #[cfg_attr(
        not(any(feature = "mock", feature = "bench")),
        expect(
            clippy::unused_self,
            reason = "the scaffold backs no entry, so no page of the mapping is resident"
        )
    )]
    pub fn resident_bytes(&self) -> u64 {
        0
    }

    fn assert_addressable(&self, page: PageId) {
        assert!(
            page.file().slot() < self.registered_file_capacity,
            "a translated page lies in a reserved file slot"
        );
        assert!(
            page.granule_idx() < self.file_granules_max,
            "a translated granule lies below file_granules_max"
        );
    }
}
