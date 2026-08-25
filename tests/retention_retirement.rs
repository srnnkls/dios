#![cfg(feature = "mock")]

use std::path::Path;

use dios::testing::{
    FrameState, MockDriver, MockPoolTestingExt, PoolBuilderTestingExt, PoolTestingExt, ReadFrameIdx,
};
use dios::{
    DirectIo, FileId, FrameGuard, Get, PageId, PollReport, Pool, PoolCompletionBatch, ReaderCtx,
    RetainRefused, RetainRefusedReason, RetireStatus,
};

const GRANULE: u32 = 4096;
const FRAME_COUNT: u32 = 7;
const POLL_BOUND: u32 = 8;
const HELD_PROGRESS_POLLS: u32 = 4;

fn pool_with_file(name: &str, max_retained_frames: u32) -> (Pool<MockDriver>, FileId) {
    let mock = MockDriver::builder()
        .queue_capacity(1)
        .frames(FRAME_COUNT)
        .frame_bytes(GRANULE)
        .build();
    let file = mock
        .open(Path::new(name), DirectIo::Disabled)
        .expect("mock file opens");
    let file_id = file.file_id();
    let pool = Pool::builder()
        .frame_count(FRAME_COUNT)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(2)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .max_retained_frames(max_retained_frames)
        .build_on(mock)
        .expect("retention fixture satisfies the augmented watermark");
    pool.register_file(file);
    (pool, file_id)
}

fn resident_guard<'pool>(
    pool: &'pool Pool<MockDriver>,
    reader: &'pool ReaderCtx,
    page: PageId,
) -> FrameGuard<'pool> {
    match pool.get(reader, page).expect("registered page is live") {
        Get::Hit(guard) => guard,
        Get::Pending(_) => panic!("the fixture inserts the page before getting it"),
        Get::Busy => panic!("the fixture has spare frames"),
    }
}

fn assert_fill(bytes: &[u8], fill: u8) {
    assert_eq!(bytes.len(), GRANULE as usize);
    assert!(bytes.iter().all(|&byte| byte == fill));
}

fn poll_until_closed_and_free(pool: &Pool<MockDriver>, file: FileId, frame: ReadFrameIdx) -> u32 {
    let mut completions = PoolCompletionBatch::with_capacity(0);
    let mut reclaimed = 0u32;
    for _ in 0..POLL_BOUND {
        let report: PollReport = pool.poll_report(&mut completions);
        assert_eq!(report.backend_completions(), 0);
        reclaimed = reclaimed.saturating_add(report.reclaimed_frames());
        if pool.driver().is_closed(file) {
            assert_eq!(pool.frame_state(frame), FrameState::Free);
            return reclaimed;
        }
    }
    panic!("retirement must close and free its frame within {POLL_BOUND} polls");
}

#[test]
fn retiring_refuses_the_same_live_guard_then_slot_reuse_allows_fresh_promotion() {
    let path = Path::new("retention-retirement-guard-and-reopen");
    let (pool, file) = pool_with_file(path.to_str().expect("fixture path is UTF-8"), 1);
    let reader = pool.register_reader().expect("reader slot is available");
    let page = PageId::new(file, 0);
    let frame = pool.insert_resident_frame(page, 0xA5);
    let guard = resident_guard(&pool, &reader, page);
    let guard_address = guard.as_ptr();

    assert_eq!(pool.retire_file(file), RetireStatus::Retiring);
    let Err(RetainRefused { guard, reason }) = guard.into_retained() else {
        panic!("a guard promoted after its file began retiring");
    };
    assert!(matches!(reason, RetainRefusedReason::FileRetiring));
    assert_eq!(guard.as_ptr(), guard_address);
    assert_fill(&guard, 0xA5);
    assert_eq!(pool.frame_state(frame), FrameState::Evicting);

    let mut completions = PoolCompletionBatch::with_capacity(0);
    for _ in 0..POLL_BOUND {
        let report = pool.poll_report(&mut completions);
        assert_eq!(report.backend_completions(), 0);
        assert_eq!(report.reclaimed_frames(), 0);
        assert!(!pool.driver().is_closed(file));
        assert_eq!(pool.frame_state(frame), FrameState::Evicting);
        assert_fill(&guard, 0xA5);
    }

    drop(guard);
    assert_eq!(poll_until_closed_and_free(&pool, file, frame), 1);
    assert_eq!(pool.retire_file(file), RetireStatus::Retired);

    let reopened = pool
        .open(path, DirectIo::Disabled)
        .expect("the retired slot reopens for a new generation");
    assert!(file.aliases_slot(&reopened));
    assert_ne!(reopened, file);
    let reopened_page = PageId::new(reopened, 0);
    pool.insert_resident_frame(reopened_page, 0xB6);
    let fresh_guard = resident_guard(&pool, &reader, reopened_page);
    let Ok(retained) = fresh_guard.into_retained() else {
        panic!("the reopened generation retained the prior generation's retiring policy");
    };
    assert_fill(&retained, 0xB6);
}

#[test]
fn held_retained_frame_delays_retirement_until_release_progress_frees_it() {
    let (pool, file) = pool_with_file("retention-retirement-held", 2);
    let reader = pool.register_reader().expect("reader slot is available");
    let held_page = PageId::new(file, 0);
    let held_frame = pool.insert_resident_frame(held_page, 0xC7);
    let held_guard = resident_guard(&pool, &reader, held_page);
    let Ok(retained) = held_guard.into_retained() else {
        panic!("the configured retention budget admits one frame");
    };

    assert_eq!(pool.evict_frame(held_page), held_frame);
    let mut completions = PoolCompletionBatch::with_capacity(0);
    for _ in 0..HELD_PROGRESS_POLLS {
        let report = pool.poll_report(&mut completions);
        assert_eq!(report.backend_completions(), 0);
        assert_eq!(report.reclaimed_frames(), 0);
        assert_eq!(pool.frame_state(held_frame), FrameState::Evicting);
        assert_fill(&retained, 0xC7);
    }

    let retiring_page = PageId::new(file, 1);
    let retiring_frame = pool.insert_resident_frame(retiring_page, 0xD8);
    assert_ne!(retiring_frame, held_frame);
    let late_guard = resident_guard(&pool, &reader, retiring_page);
    let late_guard_address = late_guard.as_ptr();
    assert_eq!(pool.retire_file(file), RetireStatus::Retiring);
    let Err(RetainRefused { guard, reason }) = late_guard.into_retained() else {
        panic!("a pre-retirement guard promoted after its file began retiring");
    };
    assert!(matches!(reason, RetainRefusedReason::FileRetiring));
    assert_eq!(guard.as_ptr(), late_guard_address);
    assert_fill(&guard, 0xD8);
    assert!(!pool.driver().is_closed(file));
    drop(guard);

    let mut plain_reclaimed = 0u32;
    for _ in 0..POLL_BOUND {
        let report = pool.poll_report(&mut completions);
        assert_eq!(report.backend_completions(), 0);
        plain_reclaimed = plain_reclaimed.saturating_add(report.reclaimed_frames());
        assert!(!pool.driver().is_closed(file));
        assert_eq!(pool.frame_state(held_frame), FrameState::Evicting);
        assert_fill(&retained, 0xC7);
        if pool.frame_state(retiring_frame) == FrameState::Free {
            break;
        }
    }
    assert_eq!(pool.frame_state(retiring_frame), FrameState::Free);
    assert_eq!(plain_reclaimed, 1);

    for _ in 0..POLL_BOUND {
        let report = pool.poll_report(&mut completions);
        assert_eq!(report.backend_completions(), 0);
        assert_eq!(report.reclaimed_frames(), 0);
        assert!(!pool.driver().is_closed(file));
        assert_eq!(pool.frame_state(held_frame), FrameState::Evicting);
        assert_fill(&retained, 0xC7);
    }

    drop(retained);
    assert_eq!(poll_until_closed_and_free(&pool, file, held_frame), 1);
    assert_eq!(pool.retire_file(file), RetireStatus::Retired);
}
