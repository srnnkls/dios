#![cfg(feature = "mock")]

use std::path::Path;

use dios::testing::{FrameState, MockDriver, PoolBuilderTestingExt, PoolTestingExt, ReadFrameIdx};
use dios::{
    DirectIo, FileId, FrameGuard, Get, PageId, PollReport, Pool, PoolCompletionBatch, ReaderCtx,
    RetainRefused, RetainRefusedReason, RetentionStats, RetireStatus,
};

const GRANULE: u32 = 4096;
const FRAME_COUNT: u32 = 6;
const POLL_BOUND: u32 = 8;

fn pool_with_file(name: &str) -> (Pool<MockDriver>, FileId) {
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
        .max_retained_frames(1)
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
        Get::Pending(_) => panic!("the test inserts the page before getting it"),
        Get::Busy => panic!("the fixture has spare frames"),
    }
}

fn assert_fill(bytes: &[u8], fill: u8) {
    assert_eq!(bytes.len(), GRANULE as usize);
    assert!(bytes.iter().all(|&byte| byte == fill));
}

fn assert_eviction_held(
    pool: &Pool<MockDriver>,
    frame: ReadFrameIdx,
    bytes: &[u8],
    fill: u8,
    poll_count: u32,
) {
    let mut completions = PoolCompletionBatch::with_capacity(0);
    for _ in 0..poll_count {
        let report: PollReport = pool.poll_report(&mut completions);
        assert_eq!(report.backend_completions(), 0);
        assert_eq!(report.reclaimed_frames(), 0);
        assert_eq!(pool.frame_state(frame), FrameState::Evicting);
        assert_fill(bytes, fill);
    }
}

fn poll_until_free(pool: &Pool<MockDriver>, frame: ReadFrameIdx) -> u32 {
    let mut completions = PoolCompletionBatch::with_capacity(0);
    let mut reclaimed = 0u32;
    for _ in 0..POLL_BOUND {
        let report: PollReport = pool.poll_report(&mut completions);
        assert_eq!(report.backend_completions(), 0);
        reclaimed = reclaimed.saturating_add(report.reclaimed_frames());
        if pool.frame_state(frame) == FrameState::Free {
            return reclaimed;
        }
    }
    panic!("the released frame must become Free within the poll bound");
}

fn assert_retained_last_drop_reclaims_on_next_poll(pool: &Pool<MockDriver>, frame: ReadFrameIdx) {
    let mut completions = PoolCompletionBatch::with_capacity(0);
    let report: PollReport = pool.poll_report(&mut completions);
    assert_eq!(report.backend_completions(), 0);
    assert_eq!(report.reclaimed_frames(), 1);
    assert_eq!(pool.frame_state(frame), FrameState::Free);
}

fn assert_no_further_reclaim(pool: &Pool<MockDriver>) {
    let mut completions = PoolCompletionBatch::with_capacity(0);
    let report = pool.poll_report(&mut completions);
    assert_eq!(report.backend_completions(), 0);
    assert_eq!(report.reclaimed_frames(), 0);
}

#[test]
fn matured_eviction_stays_held_until_retained_handle_drops() {
    let (pool, file) = pool_with_file("retention-reclaim-one");
    let reader = pool.register_reader().expect("reader slot is available");
    let page = PageId::new(file, 0);
    let frame = pool.insert_resident_frame(page, 0xA5);
    let guard = resident_guard(&pool, &reader, page);
    let Ok(retained) = guard.into_retained() else {
        panic!("the configured retention budget admits one frame");
    };

    assert_eq!(pool.evict_frame(page), frame);
    assert_eviction_held(&pool, frame, &retained, 0xA5, 4);

    drop(retained);
    assert_retained_last_drop_reclaims_on_next_poll(&pool, frame);
    assert_no_further_reclaim(&pool);
}

#[test]
fn matured_eviction_waits_for_last_of_two_retained_handles() {
    let (pool, file) = pool_with_file("retention-reclaim-two");
    let reader = pool.register_reader().expect("reader slot is available");
    let page = PageId::new(file, 0);
    let frame = pool.insert_resident_frame(page, 0xB6);
    let first_guard = resident_guard(&pool, &reader, page);
    let second_guard = resident_guard(&pool, &reader, page);
    assert_eq!(first_guard.as_ptr(), second_guard.as_ptr());
    let Ok(first) = first_guard.into_retained() else {
        panic!("the first handle promotes");
    };
    let Ok(second) = second_guard.into_retained() else {
        panic!("a second handle on the same frame promotes");
    };

    assert_eq!(pool.evict_frame(page), frame);
    assert_eviction_held(&pool, frame, &first, 0xB6, 4);

    drop(first);
    assert_eviction_held(&pool, frame, &second, 0xB6, 2);

    drop(second);
    assert_retained_last_drop_reclaims_on_next_poll(&pool, frame);
    assert_no_further_reclaim(&pool);
}

#[test]
fn plain_matured_eviction_still_reclaims_exactly_once() {
    let (pool, file) = pool_with_file("retention-reclaim-plain");
    let page = PageId::new(file, 0);
    let frame = pool.insert_resident_frame(page, 0xC7);

    assert_eq!(pool.evict_frame(page), frame);
    assert_eq!(poll_until_free(&pool, frame), 1);
    assert_no_further_reclaim(&pool);
}

#[test]
fn retention_stats_attribute_budget_retirement_and_held_eviction() {
    let (pool, file) = pool_with_file("retention-stats-attribution");
    let reader = pool.register_reader().expect("reader slot is available");
    let retained_page = PageId::new(file, 0);
    let refused_page = PageId::new(file, 1);
    let retained_frame = pool.insert_resident_frame(retained_page, 0xD8);
    pool.insert_resident_frame(refused_page, 0xE9);

    let Ok(retained) = resident_guard(&pool, &reader, retained_page).into_retained() else {
        panic!("the first distinct frame fits the budget");
    };
    let retiring_guard = resident_guard(&pool, &reader, retained_page);
    let budget_guard = resident_guard(&pool, &reader, refused_page);
    let Err(RetainRefused {
        guard: budget_guard,
        reason: budget_reason,
    }) = budget_guard.into_retained()
    else {
        panic!("a second distinct frame exceeds the budget");
    };
    let after_budget_refusal = retention_stats_values(&pool.retention_stats());
    assert!(matches!(budget_reason, RetainRefusedReason::Exhausted));

    assert_eq!(pool.retire_file(file), RetireStatus::Retiring);
    let Err(RetainRefused {
        guard: retiring_guard,
        reason: retiring_reason,
    }) = retiring_guard.into_retained()
    else {
        panic!("a same-frame promotion after retirement must be refused");
    };
    let after_retiring_refusal = retention_stats_values(&pool.retention_stats());
    assert!(matches!(retiring_reason, RetainRefusedReason::FileRetiring));
    drop((budget_guard, retiring_guard));

    for _ in 0..4 {
        pool.poll();
    }
    assert_eq!(pool.frame_state(retained_frame), FrameState::Evicting);
    assert_fill(&retained, 0xD8);
    let held = retention_stats_values(&pool.retention_stats());

    drop(retained);
    let pending_release = retention_stats_values(&pool.retention_stats());
    pool.poll();
    let released = retention_stats_values(&pool.retention_stats());

    assert_eq!(
        (
            after_budget_refusal,
            after_retiring_refusal,
            held,
            pending_release,
            released,
        ),
        (
            (1, 1, 0, 0, 0, 0),
            (1, 1, 0, 0, 1, 0),
            (1, 1, 0, 0, 1, 1),
            (1, 1, 0, 0, 1, 1),
            (0, 1, 0, 0, 1, 1),
        )
    );
}

fn retention_stats_values(stats: &RetentionStats) -> (u32, u64, u64, u64, u64, u64) {
    (
        stats.occupied_budget,
        stats.refused_budget,
        stats.refused_ceiling,
        stats.refused_contention,
        stats.refused_retiring,
        stats.retained_evictions_held,
    )
}
