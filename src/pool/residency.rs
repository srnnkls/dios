//! Lock-free `PageId → ReadFrameIdx` translation. The leading
//! `granules_per_file` granules of every registered file slot translate through
//! one direct entry each, so a warm hit performs a single load with no hashing
//! or probing; later granules fall back to the bounded open-addressed
//! [`PageTable`]. Every page lives in exactly one of the two structures.
//!
//! A direct entry holds `frame + 1` (zero is vacant) and names a frame of the
//! slot's current generation only: a slot is reused only after its retiring
//! file has unmapped and reclaimed every frame (`Pool::progress_retirements`),
//! and a reader re-reads the slot's liveness word after the entry, so an entry
//! inserted for a newer generation can never answer an older `PageId`.
//!
//! Writes happen only under the AD-4 control lock. The direct table is mapped
//! sparsely: its resident size is one page of entries per touched
//! `1024 × granule` file range, half what the kernel's page tables would cost
//! an mmap of the same accesses, and a retired slot hands its pages back.

use crate::allocation::MappedSlice;
use crate::pool::{PageId, PageTable, ReadFrameIdx};
use crate::sync::{AtomicU32, Ordering};

/// One page of direct entries; per-slot spans are whole multiples of it so a
/// retired slot's span discards as whole pages. Loom tracks every atomic and
/// discards nothing, so its models size spans exactly.
pub(crate) const GRANULES_PER_ENTRY_PAGE: u32 = if cfg!(loom) { 1 } else { 1024 };

/// The byte span of every file that translates directly unless the builder
/// overrides it: 16 GiB, which at the default 4 KiB granule reserves 16 MiB of
/// address space per registered file slot.
pub(crate) const DIRECT_MAP_FILE_BYTES_DEFAULT: u64 = if cfg!(any(miri, loom)) {
    1 << 20
} else {
    16 << 30
};

#[derive(Debug)]
pub(crate) struct ResidencyMap {
    direct: MappedSlice<AtomicU32>,
    granules_per_file: u32,
    file_slots: u32,
    overflow: PageTable,
}

impl ResidencyMap {
    /// Reserves direct entries for `direct_file_bytes` of each of `file_slots`
    /// files plus the overflow table for `frame_count` frames. `None` when a
    /// reservation is refused or its size is unrepresentable.
    pub(crate) fn try_new(
        frame_count: u32,
        file_slots: u32,
        granule: u32,
        direct_file_bytes: u64,
    ) -> Option<Self> {
        assert!(granule > 0, "a granule is positive");
        let granules_per_file = Self::granules_per_file(granule, direct_file_bytes);
        let entries = usize::try_from(u64::from(granules_per_file) * u64::from(file_slots)).ok()?;
        Some(Self {
            direct: MappedSlice::try_vacant_sparse(entries)?,
            granules_per_file,
            file_slots,
            overflow: PageTable::try_with_frame_count(frame_count)?,
        })
    }

    /// Whole entry pages covering `direct_file_bytes`, clamped to the `u32`
    /// granule space.
    fn granules_per_file(granule: u32, direct_file_bytes: u64) -> u32 {
        let granules = direct_file_bytes.div_ceil(u64::from(granule));
        let pages = granules.div_ceil(u64::from(GRANULES_PER_ENTRY_PAGE));
        let clamped = pages.min(u64::from(u32::MAX / GRANULES_PER_ENTRY_PAGE));
        u32::try_from(clamped * u64::from(GRANULES_PER_ENTRY_PAGE))
            .expect("clamped below the u32 granule space")
    }

    #[cfg(test)]
    fn granules_per_file_configured(&self) -> u32 {
        self.granules_per_file
    }

    #[inline]
    fn direct_index(&self, page: PageId) -> Option<usize> {
        let granule = page.granule_idx();
        if granule >= self.granules_per_file {
            return None;
        }
        let slot = page.file().slot();
        assert!(
            slot < self.file_slots,
            "file slot is within the fixed registry"
        );
        Some(slot as usize * self.granules_per_file as usize + granule as usize)
    }

    /// The frame currently mapped for `page`. Lock-free probes are advisory in
    /// the same way as the overflow table's: callers publish a reader epoch
    /// first and re-check liveness after, and the miss path re-probes under
    /// the control lock.
    #[inline]
    #[must_use]
    pub(crate) fn lookup(&self, page: PageId) -> Option<ReadFrameIdx> {
        match self.direct_index(page) {
            Some(index) => decode(self.direct[index].load(Ordering::Acquire)),
            None => self.overflow.lookup(page),
        }
    }

    /// Inserts or updates `page → frame` under the control lock. The Release
    /// store orders the frame's earlier publish before any reader that loads
    /// the entry.
    pub(crate) fn insert_shared(&self, page: PageId, frame: ReadFrameIdx) {
        match self.direct_index(page) {
            Some(index) => self.direct[index].store(encode(frame), Ordering::Release),
            None => self.overflow.insert_shared(page, frame),
        }
    }

    /// Removes `page` under the control lock, returning its frame if mapped.
    pub(crate) fn remove_shared(&self, page: PageId) -> Option<ReadFrameIdx> {
        match self.direct_index(page) {
            Some(index) => decode(self.direct[index].swap(0, Ordering::AcqRel)),
            None => self.overflow.remove_shared(page),
        }
    }

    /// Hands a retired slot's direct pages back to the kernel. Called under
    /// the control lock once the slot's file is `Retired`, when every entry
    /// of the slot is already vacant.
    pub(crate) fn release_slot(&self, slot: u32) {
        assert!(
            slot < self.file_slots,
            "file slot is within the fixed registry"
        );
        let start = slot as usize * self.granules_per_file as usize;
        debug_assert!(
            self.direct[start..start + self.granules_per_file as usize]
                .iter()
                .all(|entry| entry.load(Ordering::Relaxed) == 0),
            "a retired slot maps no frame"
        );
        self.direct
            .discard_vacant(start, self.granules_per_file as usize);
    }
}

fn encode(frame: ReadFrameIdx) -> u32 {
    frame
        .get()
        .checked_add(1)
        .expect("frame indexes stay below the u32 frame capacity")
}

#[inline]
fn decode(entry: u32) -> Option<ReadFrameIdx> {
    entry.checked_sub(1).map(ReadFrameIdx::new)
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::driver::FileId;

    fn page(slot: u32, generation: u32, granule: u32) -> PageId {
        PageId::new(FileId::new(7, slot, generation), granule)
    }

    #[test]
    fn spans_round_up_to_whole_entry_pages_and_clamp_to_the_granule_space() {
        assert_eq!(ResidencyMap::granules_per_file(4096, 0), 0);
        assert_eq!(ResidencyMap::granules_per_file(4096, 1), 1024);
        assert_eq!(ResidencyMap::granules_per_file(4096, 4096 * 1025), 2048);
        assert_eq!(
            ResidencyMap::granules_per_file(4096, u64::MAX),
            u32::MAX / GRANULES_PER_ENTRY_PAGE * GRANULES_PER_ENTRY_PAGE
        );
    }

    #[test]
    fn direct_and_overflow_pages_round_trip_through_one_interface() {
        let map = ResidencyMap::try_new(8, 2, 4096, 4096 * 1024).expect("small map");
        assert_eq!(map.granules_per_file_configured(), 1024);
        let direct = page(1, 3, 1023);
        let overflow = page(1, 3, 1024);
        assert_eq!(map.lookup(direct), None);
        map.insert_shared(direct, ReadFrameIdx::new(0));
        map.insert_shared(overflow, ReadFrameIdx::new(5));
        assert_eq!(map.lookup(direct), Some(ReadFrameIdx::new(0)));
        assert_eq!(map.lookup(overflow), Some(ReadFrameIdx::new(5)));
        assert_eq!(map.lookup(page(0, 3, 1023)), None, "slots are disjoint");
        assert_eq!(map.remove_shared(direct), Some(ReadFrameIdx::new(0)));
        assert_eq!(map.remove_shared(direct), None);
        assert_eq!(map.remove_shared(overflow), Some(ReadFrameIdx::new(5)));
        assert_eq!(map.lookup(overflow), None);
    }

    #[test]
    fn a_zero_span_routes_every_page_through_the_overflow_table() {
        let map = ResidencyMap::try_new(8, 2, 4096, 0).expect("overflow-only map");
        let first = page(0, 1, 0);
        map.insert_shared(first, ReadFrameIdx::new(3));
        assert_eq!(map.lookup(first), Some(ReadFrameIdx::new(3)));
        assert_eq!(map.overflow.len(), 1, "the overflow table holds the page");
    }

    #[test]
    fn a_released_slot_reads_vacant_and_accepts_new_entries() {
        let map = ResidencyMap::try_new(8, 2, 4096, 4096 * 4096).expect("small map");
        let old = page(1, 1, 2048);
        map.insert_shared(old, ReadFrameIdx::new(2));
        assert_eq!(map.remove_shared(old), Some(ReadFrameIdx::new(2)));
        map.release_slot(1);
        let reused = page(1, 2, 2048);
        assert_eq!(map.lookup(reused), None);
        map.insert_shared(reused, ReadFrameIdx::new(4));
        assert_eq!(map.lookup(reused), Some(ReadFrameIdx::new(4)));
    }
}
