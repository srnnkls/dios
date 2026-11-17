//! Per-file array translation: one entry per granule per registered file slot,
//! bounded by `file_granules_max`. An entry is zero when absent and the frame
//! index plus one otherwise; every slot lives in one reserved mapping indexed
//! by `(file slot << file_granules_shift) | granule`.

use crate::allocation::MappedSlice;
use crate::driver::FileId;
use crate::pool::{PageId, ReadFrameIdx};
use crate::sync::{AtomicU32, Ordering};

/// Largest frame index an entry can name: the entry stores `frame + 1` in 32
/// bits and keeps zero for absent.
const FRAME_INDEX_MAX: u32 = (1 << 30) - 1;

/// Largest frame count whose every frame index an entry can name.
pub(crate) const FRAME_COUNT_MAX: u32 = FRAME_INDEX_MAX + 1;

const ENTRY_BYTES: u64 = 4;

/// Byte length of the mapping holding one entry per granule per registered
/// file slot; `None` when it overflows `u64`.
pub(crate) fn entry_bytes(registered_file_capacity: u32, file_granules_max: u32) -> Option<u64> {
    u64::from(registered_file_capacity)
        .checked_mul(u64::from(file_granules_max))?
        .checked_mul(ENTRY_BYTES)
}

pub struct Translation {
    entries: MappedSlice<AtomicU32>,
    registered_file_capacity: u32,
    file_granules_shift: u32,
}

impl Translation {
    /// Reserves one entry per granule per registered file slot; `None` when
    /// the mapping's byte length overflows `u64` or the host refuses the
    /// reservation.
    ///
    /// # Panics
    ///
    /// If `file_granules_max` is not a power of two.
    #[must_use]
    pub fn try_reserve(registered_file_capacity: u32, file_granules_max: u32) -> Option<Self> {
        assert!(
            file_granules_max.is_power_of_two(),
            "file_granules_max is a power of two"
        );
        let entry_count = entry_bytes(registered_file_capacity, file_granules_max)? / ENTRY_BYTES;
        Some(Self {
            entries: MappedSlice::try_reserved(entry_count)?,
            registered_file_capacity,
            file_granules_shift: file_granules_max.trailing_zeros(),
        })
    }

    #[must_use]
    pub fn entry_count(&self) -> u64 {
        u64::from(self.registered_file_capacity) << self.file_granules_shift
    }

    /// Resolves `page` to the frame its entry names.
    ///
    /// # Panics
    ///
    /// If `page` lies outside the reserved slots or at or above
    /// `file_granules_max`.
    #[must_use]
    #[inline]
    pub fn lookup(&self, page: PageId) -> Option<ReadFrameIdx> {
        entry_frame(self.entry(page))
    }

    /// The entries of `file`'s slot indexed by granule: the slice's base and
    /// length are the slot's entry base and `file_granules_max`.
    ///
    /// # Panics
    ///
    /// If `file`'s slot lies outside the reservation.
    #[inline]
    pub(crate) fn slot_entries(&self, file: FileId) -> &[AtomicU32] {
        assert!(
            file.slot() < self.registered_file_capacity,
            "a session file lies in a reserved file slot"
        );
        let start = usize::try_from(u64::from(file.slot()) << self.file_granules_shift)
            .expect("a reserved entry index fits usize");
        let entries = self.entries.elements(start, 1 << self.file_granules_shift);
        debug_assert_eq!(
            entries.len().trailing_zeros(),
            self.file_granules_shift,
            "a slot holds file_granules_max entries"
        );
        entries
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
        let entry = self.entry(page);
        assert_eq!(
            entry.load(Ordering::Relaxed),
            0,
            "publish names an absent entry"
        );
        entry.store(frame.get() + 1, Ordering::Release);
    }

    /// Clears the set entry of `page` and returns the frame it named.
    ///
    /// # Panics
    ///
    /// If `page` is not addressable or its entry is absent.
    #[must_use]
    pub fn clear(&self, page: PageId) -> ReadFrameIdx {
        let entry = self.entry(page);
        let word = entry.load(Ordering::Relaxed);
        assert_ne!(word, 0, "clear names a set entry");
        entry.store(0, Ordering::Release);
        ReadFrameIdx::new(word - 1)
    }

    /// Releases the whole OS pages inside `file`'s slot; every entry of the
    /// slot is already absent.
    ///
    /// # Panics
    ///
    /// If `file`'s slot lies outside the reservation.
    pub fn release_slot(&self, file: FileId) {
        assert!(
            file.slot() < self.registered_file_capacity,
            "a released slot lies inside the reservation"
        );
        let start = u64::from(file.slot()) << self.file_granules_shift;
        let end = start + (1u64 << self.file_granules_shift);
        // SAFETY: the entries are atomics, and the writers cleared every entry
        // of the slot before its file retired, so each one already reads zero.
        unsafe { self.entries.release_vacant(start, end) };
    }

    #[must_use]
    #[cfg(any(feature = "mock", feature = "bench"))]
    pub fn resident_bytes(&self) -> u64 {
        self.entries.resident_bytes()
    }

    #[inline]
    fn entry(&self, page: PageId) -> &AtomicU32 {
        assert!(
            page.file().slot() < self.registered_file_capacity,
            "a translated page lies in a reserved file slot"
        );
        assert!(
            page.granule_idx() >> self.file_granules_shift == 0,
            "a translated granule lies below file_granules_max"
        );
        let index = (u64::from(page.file().slot()) << self.file_granules_shift)
            | u64::from(page.granule_idx());
        self.entries
            .element(usize::try_from(index).expect("a reserved entry index fits usize"))
    }
}

/// The frame a translation entry names, loaded `Acquire` so a set entry
/// carries the frame's completed publication.
#[inline]
pub(crate) fn entry_frame(entry: &AtomicU32) -> Option<ReadFrameIdx> {
    let word = entry.load(Ordering::Acquire);
    word.checked_sub(1).map(ReadFrameIdx::new)
}

impl std::fmt::Debug for Translation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Translation")
            .field("registered_file_capacity", &self.registered_file_capacity)
            .field("file_granules_max", &(1u64 << self.file_granules_shift))
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::DEFAULT_REGISTERED_FILE_CAPACITY;
    use crate::pool::FILE_GRANULES_MAX_DEFAULT;

    #[test]
    fn slot_entries_of_a_default_reservation_index_the_slot_by_granule() {
        let translation =
            Translation::try_reserve(DEFAULT_REGISTERED_FILE_CAPACITY, FILE_GRANULES_MAX_DEFAULT)
                .expect("the default reservation maps");
        let file = FileId::new(0, DEFAULT_REGISTERED_FILE_CAPACITY - 1, 1);
        let granule_idx = FILE_GRANULES_MAX_DEFAULT - 1;
        translation.publish(PageId::new(file, granule_idx), ReadFrameIdx::new(7));

        let entries = translation.slot_entries(file);

        assert_eq!(entries.len(), FILE_GRANULES_MAX_DEFAULT as usize);
        assert_eq!(
            entry_frame(&entries[granule_idx as usize]),
            Some(ReadFrameIdx::new(7))
        );
        assert_eq!(entry_frame(&entries[0]), None);
    }
}
