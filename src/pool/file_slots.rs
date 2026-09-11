//! Fixed slots with intrusive per-file membership, mutated under pool control.
use std::mem::MaybeUninit;
use std::ops::{Index, IndexMut};

use crate::allocation::{MappedSlice, ZeroVacant, try_boxed_slice_with};
use crate::driver::FileId;

pub(super) trait FileOwned: Copy {
    fn file_slot(self) -> u32;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
struct SlotLink(u32);

impl SlotLink {
    fn from_index(index: usize) -> Self {
        Self(
            u32::try_from(index)
                .expect("slot index fits u32")
                .checked_add(1)
                .expect("slot index is below capacity"),
        )
    }

    fn index(self) -> Option<usize> {
        self.0.checked_sub(1).map(|index| index as usize)
    }
}

#[repr(C)]
pub(super) struct FileSlot<T: Copy> {
    previous: SlotLink,
    next: SlotLink,
    value: MaybeUninit<T>,
}

// SAFETY: Zero links denote vacancy; no accessor reads the MaybeUninit payload
// until insertion initializes it and installs a nonzero next link.
unsafe impl<T: Copy> ZeroVacant for FileSlot<T> {
    #[cfg(loom)]
    fn vacant() -> Self {
        Self {
            previous: SlotLink(0),
            next: SlotLink(0),
            value: MaybeUninit::uninit(),
        }
    }
}

impl<T: Copy> FileSlot<T> {
    pub(super) fn get(&self) -> Option<T> {
        // SAFETY: Insertion initializes the payload before setting next; removal
        // clears next. Both run exclusively under the pool control lock.
        self.next
            .index()
            .map(|_| unsafe { self.value.assume_init() })
    }

    pub(super) fn get_mut(&mut self) -> Option<&mut T> {
        // SAFETY: As for get; the exclusive borrow also excludes every accessor.
        self.next
            .index()
            .map(|_| unsafe { self.value.assume_init_mut() })
    }

    pub(super) fn is_none(&self) -> bool {
        self.next.0 == 0
    }
}

impl<T: Copy + std::fmt::Debug> std::fmt::Debug for FileSlot<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.get().fmt(formatter)
    }
}

#[derive(Debug)]
pub(super) struct FileSlots<T: FileOwned> {
    slots: MappedSlice<FileSlot<T>>,
    heads: Box<[SlotLink]>,
}

impl<T: FileOwned> FileSlots<T> {
    pub(super) fn try_new(capacity: u32, file_capacity: u32) -> Option<Self> {
        assert!(capacity > 0, "indexed slot capacity is positive");
        Some(Self {
            slots: MappedSlice::try_vacant(capacity)?,
            heads: try_boxed_slice_with(file_capacity, SlotLink::default)?,
        })
    }

    pub(super) fn insert(&mut self, index: usize, value: T) {
        assert!(
            self.slots[index].is_none(),
            "insertion claims a vacant slot"
        );
        let owner = value.file_slot() as usize;
        let link = SlotLink::from_index(index);
        let old_head = self.heads[owner];
        self.slots[index].value = MaybeUninit::new(value);
        self.slots[index].previous = link;
        self.slots[index].next = old_head.index().map_or(link, |_| old_head);
        if let Some(head) = old_head.index() {
            assert_eq!(
                self.slots[head].previous, old_head,
                "the head has no predecessor"
            );
            self.slots[head].previous = link;
        }
        self.heads[owner] = link;
    }

    pub(super) fn remove(&mut self, index: usize) {
        let value = self.slots[index]
            .get()
            .expect("removal names an occupied slot");
        let owner = value.file_slot() as usize;
        let link = SlotLink::from_index(index);
        let previous = self.slots[index].previous;
        let next = self.slots[index].next;
        if previous == link {
            assert_eq!(self.heads[owner], link, "only the head has no predecessor");
            self.heads[owner] = if next == link { SlotLink(0) } else { next };
        } else {
            let before = previous.index().expect("occupied predecessor");
            assert_eq!(self.slots[before].next, link, "predecessor links back");
            self.slots[before].next = if next == link { previous } else { next };
        }
        if next != link {
            let after = next.index().expect("occupied successor");
            assert_eq!(self.slots[after].previous, link, "successor links back");
            self.slots[after].previous = if previous == link { next } else { previous };
        }
        self.slots[index].next = SlotLink(0);
        self.slots[index].previous = SlotLink(0);
        assert!(self.slots[index].is_none(), "unlinking restores vacancy");
    }

    pub(super) fn for_file(&self, file: FileId) -> impl Iterator<Item = (usize, T)> + '_ {
        let mut cursor = self.heads[file.slot() as usize];
        let mut remaining = self.slots.len();
        std::iter::from_fn(move || {
            let index = cursor.index()?;
            assert!(remaining > 0, "membership traversal cannot cycle");
            remaining -= 1;
            let value = self.slots[index]
                .get()
                .expect("membership names an occupied slot");
            assert_eq!(value.file_slot(), file.slot(), "membership owner is exact");
            let next = self.slots[index].next;
            cursor = if next == cursor { SlotLink(0) } else { next };
            Some((index, value))
        })
    }

    pub(super) fn iter(&self) -> std::slice::Iter<'_, FileSlot<T>> {
        self.slots.iter()
    }
    pub(super) fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    #[cfg(feature = "bench")]
    pub(super) fn populate(&mut self) {
        assert!(
            self.heads.iter().all(|head| head.0 == 0),
            "population starts vacant"
        );
        for slot in self.slots.iter_mut() {
            slot.previous = SlotLink(0);
            slot.next = SlotLink(0);
        }
    }
}

impl<T: FileOwned> Index<usize> for FileSlots<T> {
    type Output = FileSlot<T>;
    fn index(&self, index: usize) -> &Self::Output {
        &self.slots[index]
    }
}

impl<T: FileOwned> IndexMut<usize> for FileSlots<T> {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.slots[index]
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::allocation::Occupiable;
    use crate::pool::PageId;

    impl FileOwned for PageId {
        fn file_slot(self) -> u32 {
            self.file().slot()
        }
    }

    fn members(slots: &FileSlots<PageId>, file: FileId) -> Vec<(usize, PageId)> {
        let mut values: Vec<_> = slots
            .for_file(file)
            .filter(|(_, page)| page.file() == file)
            .collect();
        values.sort_by_key(|(index, _)| *index);
        values
    }

    #[test]
    fn links_fit_the_existing_page_flag_and_padding() {
        assert_eq!(
            size_of::<FileSlot<PageId>>(),
            size_of::<Occupiable<PageId>>()
        );
        assert_eq!(size_of::<SlotLink>(), 4);
        assert_eq!(
            SlotLink::from_index(u32::MAX as usize - 1).index(),
            Some(u32::MAX as usize - 1)
        );
    }

    #[test]
    fn sparse_membership_unlinks_head_middle_tail_and_reuses_slots() {
        let mut slots = FileSlots::try_new(32, 2).expect("slots");
        let first = FileId::new(1, 0, 1);
        let second = FileId::new(1, 1, 1);
        for index in [27, 4, 19] {
            slots.insert(index, PageId::new(first, 0));
        }
        slots.insert(11, PageId::new(second, 3));
        slots.remove(4);
        slots.remove(19);
        slots.remove(27);
        assert!(members(&slots, first).is_empty());
        assert_eq!(members(&slots, second), [(11, PageId::new(second, 3))]);
        slots.insert(4, PageId::new(second, 4));
        slots.remove(11);
        assert_eq!(members(&slots, second), [(4, PageId::new(second, 4))]);
    }

    #[test]
    fn reference_model_covers_reuse_and_overlapping_terminal_generations() {
        let mut slots = FileSlots::try_new(37, 3).expect("slots");
        let mut model = [None; 37];
        let files = [
            FileId::new(1, 0, 1),
            FileId::new(1, 1, 1),
            FileId::new(1, 2, 1),
            FileId::new(1, 0, 2),
        ];
        let mut random = 731_u32;
        for step in 0..512 {
            random = random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let index = random as usize % model.len();
            if model[index].take().is_some() {
                slots.remove(index);
            }
            if random & 4 != 0 {
                let value = PageId::new(files[(random >> 12) as usize % files.len()], step);
                slots.insert(index, value);
                model[index] = Some(value);
            }
            for file in files {
                let expected: Vec<_> = model
                    .iter()
                    .enumerate()
                    .filter_map(|(index, page)| {
                        page.filter(|page| page.file() == file)
                            .map(|page| (index, page))
                    })
                    .collect();
                assert_eq!(members(&slots, file), expected);
            }
        }
    }
}
