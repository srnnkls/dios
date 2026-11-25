//! Per-file array translation: one entry per granule per file slot, written
//! only by asserting writers, bounded by `file_granules_max`, and released per
//! slot in whole OS pages.
//!
//! Test surface: `dios::testing::Translation` with
//! `try_reserve(registered_file_capacity, file_granules_max) -> Option<Self>`,
//! `entry_count() -> u64`, `publish(PageId, ReadFrameIdx)`,
//! `clear(PageId) -> ReadFrameIdx`, `lookup(PageId) -> Option<ReadFrameIdx>`,
//! `release_slot(FileId)` and `resident_bytes() -> u64`.

#![cfg(any(feature = "mock", feature = "bench"))]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU32, Ordering};

use dios::driver::{Driver, FileHandle};
use dios::testing::{ReadFrameIdx, Translation};
use dios::{DirectIo, FileId, PageId};

const ENTRY_BYTES: u64 = 4;
const FRAME_INDEX_MAX: u32 = (1 << 30) - 1;

static FILE_SEQ: AtomicU32 = AtomicU32::new(0);

/// Opens `count` files on one driver so each holds a distinct file slot; the
/// handles keep the slots reserved for the test's duration.
fn files_on_one_driver(count: u32) -> (Driver, Vec<FileHandle>) {
    let driver = Driver::builder()
        .build()
        .expect("the test driver initializes");
    let handles = (0..count)
        .map(|_| {
            let n = FILE_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("dios_translation_{}_{n}.bin", std::process::id()));
            std::fs::write(&path, [0u8; 64]).expect("temp file writable");
            driver
                .open(&path, DirectIo::Disabled)
                .expect("open temp file")
        })
        .collect();
    (driver, handles)
}

fn ids(handles: &[FileHandle]) -> Vec<FileId> {
    handles.iter().map(FileHandle::file_id).collect()
}

fn reserve(registered_file_capacity: u32, file_granules_max: u32) -> Translation {
    Translation::try_reserve(registered_file_capacity, file_granules_max)
        .expect("a representable translation reservation succeeds")
}

fn panics(body: impl FnOnce()) -> bool {
    catch_unwind(AssertUnwindSafe(body)).is_err()
}

#[test]
fn one_entry_publishes_resolves_and_clears_without_touching_its_neighbors() {
    let (_driver, handles) = files_on_one_driver(2);
    let files = ids(&handles);
    let (file, neighbor) = (files[0], files[1]);
    let translation = reserve(2, 16);
    let page = PageId::new(file, 5);

    assert_eq!(translation.lookup(page), None, "a fresh entry is absent");
    translation.publish(page, ReadFrameIdx::new(0));
    assert_eq!(
        translation.lookup(page),
        Some(ReadFrameIdx::new(0)),
        "frame zero is a resident entry, distinct from the absent encoding"
    );
    for untouched in [
        PageId::new(file, 4),
        PageId::new(file, 6),
        PageId::new(neighbor, 5),
    ] {
        assert_eq!(
            translation.lookup(untouched),
            None,
            "publishing one granule leaves {untouched:?} absent"
        );
    }

    let neighbor_page = PageId::new(neighbor, 5);
    translation.publish(neighbor_page, ReadFrameIdx::new(FRAME_INDEX_MAX));
    assert_eq!(
        translation.lookup(neighbor_page),
        Some(ReadFrameIdx::new(FRAME_INDEX_MAX)),
        "the largest frame index below 2^30 round-trips"
    );
    assert_eq!(
        translation.clear(page),
        ReadFrameIdx::new(0),
        "clearing returns the frame the entry named"
    );
    assert_eq!(translation.lookup(page), None, "a cleared entry is absent");
    assert_eq!(
        translation.lookup(neighbor_page),
        Some(ReadFrameIdx::new(FRAME_INDEX_MAX)),
        "clearing one slot's entry leaves the same granule of another slot set"
    );
}

#[test]
fn writers_assert_an_absent_entry_before_publish_and_a_set_entry_before_clear() {
    let (_driver, handles) = files_on_one_driver(1);
    let file = ids(&handles)[0];
    let translation = reserve(1, 16);
    let page = PageId::new(file, 3);

    translation.publish(page, ReadFrameIdx::new(1));
    assert_eq!(translation.lookup(page), Some(ReadFrameIdx::new(1)));
    assert!(
        panics(|| translation.publish(page, ReadFrameIdx::new(2))),
        "publishing over a set entry asserts instead of updating in place"
    );
    assert_eq!(
        translation.lookup(page),
        Some(ReadFrameIdx::new(1)),
        "the refused publish left the entry unchanged"
    );
    assert_eq!(translation.clear(page), ReadFrameIdx::new(1));
    assert!(
        panics(|| {
            let _ = translation.clear(page);
        }),
        "clearing an absent entry asserts"
    );
}

#[test]
fn the_reservation_spans_every_slot_and_asserts_past_its_bounds() {
    const CAPACITY: u32 = 4;
    const GRANULES_MAX: u32 = 16;
    let (_driver, handles) = files_on_one_driver(2 * CAPACITY);
    let files = ids(&handles);
    let translation = reserve(CAPACITY, GRANULES_MAX);
    assert_eq!(
        translation.entry_count(),
        u64::from(CAPACITY) * u64::from(GRANULES_MAX),
        "one entry per granule per registered file slot"
    );

    let mut accepted = Vec::new();
    for &file in &files {
        let last = PageId::new(file, GRANULES_MAX - 1);
        if !panics(|| translation.publish(last, ReadFrameIdx::new(7))) {
            accepted.push(file);
        }
    }
    assert_eq!(
        accepted.len(),
        CAPACITY as usize,
        "exactly the slots below the registered capacity are addressable"
    );
    for &file in &accepted {
        assert_eq!(
            translation.lookup(PageId::new(file, GRANULES_MAX - 1)),
            Some(ReadFrameIdx::new(7)),
            "the last granule of every slot, the last slot included, resolves"
        );
        let beyond = PageId::new(file, GRANULES_MAX);
        assert!(
            panics(|| {
                let _ = translation.lookup(beyond);
            }),
            "a lookup at file_granules_max asserts"
        );
        assert!(
            panics(|| translation.publish(beyond, ReadFrameIdx::new(8))),
            "a publish at file_granules_max asserts"
        );
    }
}

#[test]
fn releasing_a_slot_leaves_it_absent_and_keeps_page_sharing_neighbors() {
    const SHARED_GRANULES_MAX: u32 = 16;
    const WIDE_GRANULES_MAX: u32 = 1 << 14;
    const WIDE_SLOT_BYTES: u64 = WIDE_GRANULES_MAX as u64 * ENTRY_BYTES;
    const STRIDE: u32 = 1024;
    let (_driver, handles) = files_on_one_driver(2);
    let files = ids(&handles);
    let (retired, neighbor) = (files[0], files[1]);

    let shared = reserve(2, SHARED_GRANULES_MAX);
    for granule in 0..SHARED_GRANULES_MAX {
        shared.publish(PageId::new(retired, granule), ReadFrameIdx::new(granule));
        shared.publish(
            PageId::new(neighbor, granule),
            ReadFrameIdx::new(SHARED_GRANULES_MAX + granule),
        );
    }
    for granule in 0..SHARED_GRANULES_MAX {
        assert_eq!(
            shared.clear(PageId::new(retired, granule)),
            ReadFrameIdx::new(granule)
        );
    }
    shared.release_slot(retired);
    for granule in 0..SHARED_GRANULES_MAX {
        assert_eq!(shared.lookup(PageId::new(retired, granule)), None);
        assert_eq!(
            shared.lookup(PageId::new(neighbor, granule)),
            Some(ReadFrameIdx::new(SHARED_GRANULES_MAX + granule)),
            "a 64-byte slot shares its OS page, so its release must not discard the neighbor's entries"
        );
    }

    let wide = reserve(2, WIDE_GRANULES_MAX);
    for granule in (0..WIDE_GRANULES_MAX).step_by(STRIDE as usize) {
        wide.publish(PageId::new(retired, granule), ReadFrameIdx::new(1));
    }
    wide.publish(PageId::new(neighbor, 0), ReadFrameIdx::new(2));
    let resident_before = wide.resident_bytes();
    assert!(
        resident_before >= WIDE_SLOT_BYTES,
        "touching every 4 KiB of the slot backs it: {resident_before} bytes resident"
    );
    for granule in (0..WIDE_GRANULES_MAX).step_by(STRIDE as usize) {
        let _ = wide.clear(PageId::new(retired, granule));
    }
    wide.release_slot(retired);
    #[cfg(target_os = "linux")]
    let resident_after = wide.resident_bytes();
    for granule in (0..WIDE_GRANULES_MAX).step_by(STRIDE as usize) {
        assert_eq!(
            wide.lookup(PageId::new(retired, granule)),
            None,
            "a released slot reads absent"
        );
    }
    assert_eq!(
        wide.lookup(PageId::new(neighbor, 0)),
        Some(ReadFrameIdx::new(2)),
        "releasing one slot keeps the next slot's entries"
    );
    #[cfg(target_os = "linux")]
    assert!(
        resident_after + WIDE_SLOT_BYTES <= resident_before,
        "the release returned the slot's whole OS pages: {resident_after} of {resident_before} bytes still resident"
    );
}
