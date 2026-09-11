//! Membership keeps only ownership; `Frames` already stores the complete `PageId`.
use super::PageId;
use super::file_slots::{FileOwned, FileSlot, FileSlots};

#[derive(Clone, Copy, Debug)]
pub(super) struct FrameFileSlot(u32);

impl FrameFileSlot {
    pub(super) fn for_page(page: PageId) -> Self {
        Self(page.file().slot())
    }
}

impl FileOwned for FrameFileSlot {
    fn file_slot(self) -> u32 {
        self.0
    }
}

pub(super) type FramePages = FileSlots<FrameFileSlot>;
const _: () = assert!(size_of::<FileSlot<FrameFileSlot>>() == 12);

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::driver::FileId;
    use crate::pool::frames::{FrameState, Frames, ReadFrameIdx};

    fn publish(frames: &Frames, index: &mut FramePages, frame: u32, page: PageId) {
        let frame = ReadFrameIdx::new(frame);
        let token = frames.claim(frame, page).expect("a Free frame claims");
        frames.publish(token);
        index.insert(frame.get() as usize, FrameFileSlot::for_page(page));
    }

    fn pages_for_file(frames: &mut Frames, index: &FramePages, file: FileId) -> Vec<PageId> {
        index
            .for_file(file)
            .map(|(frame, _)| {
                frames.exact_page_exclusive(ReadFrameIdx::new(
                    u32::try_from(frame).expect("frame index"),
                ))
            })
            .filter(|page| page.file() == file)
            .collect()
    }

    #[test]
    fn mapped_frame_record_fits_twenty_bytes() {
        assert!(
            size_of::<FileSlot<FrameFileSlot>>() <= 20,
            "per-frame metadata must leave room below the measured RSS cap"
        );
        assert_eq!(size_of::<FileSlot<FrameFileSlot>>(), 12);
    }

    #[test]
    fn canonical_identity_survives_eviction_until_unlink_and_reuse() {
        let mut frames = Frames::preallocated(4, 4096);
        let mut index = FramePages::try_new(4, 2).expect("membership");
        let old_file = FileId::new(u64::MAX, 1, u32::MAX);
        let old = PageId::new(old_file, u32::MAX);
        assert!(pages_for_file(&mut frames, &index, old_file).is_empty());
        publish(&frames, &mut index, 3, old);
        let frame = ReadFrameIdx::new(3);
        frames.advance(frame, FrameState::Evicting);
        assert_eq!(pages_for_file(&mut frames, &index, old_file), [old]);
        frames.advance(frame, FrameState::Free);
        index.remove(3);
        assert!(pages_for_file(&mut frames, &index, old_file).is_empty());
        let new_file = FileId::new(0, 1, 0);
        let new = PageId::new(new_file, 0);
        publish(&frames, &mut index, 3, new);
        assert_eq!(pages_for_file(&mut frames, &index, new_file), [new]);
        assert!(pages_for_file(&mut frames, &index, old_file).is_empty());
    }

    #[test]
    fn canonical_identity_distinguishes_drivers_and_generations_sharing_a_slot() {
        let mut frames = Frames::preallocated(4, 4096);
        let mut index = FramePages::try_new(4, 1).expect("membership");
        let files = [
            FileId::new(0, 0, 1),
            FileId::new(0, 0, 2),
            FileId::new(u64::MAX, 0, 1),
        ];
        for (frame, file) in files.into_iter().enumerate() {
            publish(
                &frames,
                &mut index,
                u32::try_from(frame).expect("frame index"),
                PageId::new(file, 7),
            );
        }
        for file in files {
            assert_eq!(
                pages_for_file(&mut frames, &index, file),
                [PageId::new(file, 7)]
            );
        }
    }
}
