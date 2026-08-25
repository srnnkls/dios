#![cfg(feature = "mock")]

use std::path::Path;

use dios::testing::{FrameState, MockDriver, PoolBuilderTestingExt, PoolTestingExt};
use dios::{
    DirectIo, FileId, FrameGuard, Get, PageId, Pool, PoolCompletionBatch, ReaderCtx, RetainRefused,
    RetainRefusedReason,
};

const GRANULE: u32 = 4096;
const FRAME_COUNT: u32 = 6;
const POLL_BOUND: u32 = 8;

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
        Get::Pending(_) => panic!("the fixture inserts every page before access"),
        Get::Busy => panic!("the fixture has spare frames"),
    }
}

fn assert_fill(bytes: &[u8], fill: u8) {
    assert_eq!(bytes.len(), GRANULE as usize);
    assert!(bytes.iter().all(|&byte| byte == fill));
}

#[test]
fn budget_recovers_after_held_release_while_unrelated_eviction_reclaims() {
    let (pool, file) = pool_with_file("retention-budget-release-drain", 1);
    let reader = pool.register_reader().expect("reader slot is available");
    let page_a = PageId::new(file, 0);
    let page_b = PageId::new(file, 1);
    let page_c = PageId::new(file, 2);
    let frame_a = pool.insert_resident_frame(page_a, 0xA5);
    pool.insert_resident_frame(page_b, 0xB6);
    let frame_c = pool.insert_resident_frame(page_c, 0xC7);
    let Ok(retained_a) = resident_guard(&pool, &reader, page_a).into_retained() else {
        panic!("the first frame fits the one-frame budget");
    };

    assert_eq!(pool.evict_frame(page_a), frame_a);
    for _ in 0..4 {
        pool.poll();
    }
    assert_eq!(pool.frame_state(frame_a), FrameState::Evicting);
    assert_eq!(pool.retention_stats().retained_evictions_held, 1);

    let Err(RetainRefused { guard, reason }) =
        resident_guard(&pool, &reader, page_b).into_retained()
    else {
        panic!("a second frame must be refused while the HELD frame owns the budget");
    };
    assert!(matches!(reason, RetainRefusedReason::Exhausted));
    drop(guard);

    assert_eq!(pool.evict_frame(page_c), frame_c);
    let mut completions = PoolCompletionBatch::with_capacity(0);
    for _ in 0..POLL_BOUND {
        let report = pool.poll_report(&mut completions);
        assert_eq!(report.backend_completions(), 0);
        assert_eq!(pool.frame_state(frame_a), FrameState::Evicting);
        assert_fill(&retained_a, 0xA5);
        if pool.frame_state(frame_c) == FrameState::Free {
            assert_eq!(report.reclaimed_frames(), 1);
            break;
        }
        assert_eq!(report.reclaimed_frames(), 0);
    }
    assert_eq!(pool.frame_state(frame_c), FrameState::Free);

    drop(retained_a);
    let report = pool.poll_report(&mut completions);
    assert_eq!(report.reclaimed_frames(), 1);
    assert_eq!(pool.frame_state(frame_a), FrameState::Free);
    let Ok(retained_b) = resident_guard(&pool, &reader, page_b).into_retained() else {
        panic!("draining the final HELD release must restore admission");
    };
    assert_fill(&retained_b, 0xB6);
}

#[test]
fn zero_budget_matured_eviction_frees_without_held_attribution() {
    let (pool, file) = pool_with_file("retention-zero-budget-maturity", 0);
    let page = PageId::new(file, 0);
    let frame = pool.insert_resident_frame(page, 0xD8);
    assert_eq!(pool.evict_frame(page), frame);

    let mut completions = PoolCompletionBatch::with_capacity(0);
    for _ in 0..POLL_BOUND {
        let report = pool.poll_report(&mut completions);
        assert_eq!(report.backend_completions(), 0);
        if pool.frame_state(frame) == FrameState::Free {
            assert_eq!(report.reclaimed_frames(), 1);
            break;
        }
        assert_eq!(report.reclaimed_frames(), 0);
    }

    assert_eq!(pool.frame_state(frame), FrameState::Free);
    assert_eq!(pool.retention_stats().retained_evictions_held, 0);
}
