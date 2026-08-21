#![cfg(feature = "mock")]

use std::path::Path;

use dios::testing::{MockDriver, PoolBuilderTestingExt, PoolTestingExt};
use dios::{
    DirectIo, FileId, FrameGuard, Get, PageId, Pool, ReaderCtx, ReadyResult, RetainRefused,
    RetainRefusedReason,
};

const GRANULE: u32 = 4096;
const FRAME_COUNT: u32 = 6;

fn pool_with_pages(name: &str, pages: &[(u32, u8)]) -> (Pool<MockDriver>, FileId) {
    let mock = MockDriver::builder()
        .queue_capacity(2)
        .frames(FRAME_COUNT)
        .frame_bytes(GRANULE)
        .build();
    let file = mock
        .open(Path::new(name), DirectIo::Disabled)
        .expect("mock file opens");
    let file_id = file.file_id();
    for &(granule_idx, fill) in pages {
        mock.seed_page(&file, granule_idx, fill);
    }
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

fn get_guard<'pool>(
    pool: &'pool Pool<MockDriver>,
    reader: &'pool ReaderCtx,
    page: PageId,
) -> FrameGuard<'pool> {
    match pool.get(reader, page).expect("registered page is live") {
        Get::Hit(guard) => guard,
        Get::Pending(token) => {
            pool.poll();
            let ReadyResult::Ready(guard) = pool.ready(reader, token) else {
                panic!("mock read completes after one poll");
            };
            guard
        }
        Get::Busy => panic!("fixture has spare frames"),
    }
}

fn assert_bytes(bytes: &[u8], fill: u8) {
    assert_eq!(bytes.len(), GRANULE as usize);
    assert!(bytes.iter().all(|&byte| byte == fill));
}

#[test]
fn nonzero_budget_promotes_live_guard_with_its_bytes() {
    let (pool, file) = pool_with_pages("retention-promotes", &[(0, 0xA5)]);
    let reader = pool.register_reader().expect("reader slot is available");
    let guard = get_guard(&pool, &reader, PageId::new(file, 0));
    assert_bytes(&guard, 0xA5);

    let Ok(retained) = guard.into_retained() else {
        panic!("a live guard must promote within the configured budget");
    };
    assert_bytes(&retained, 0xA5);
}

#[test]
fn same_frame_promotions_are_independent_and_budget_recovers_after_both_drop() {
    let (pool, file) = pool_with_pages("retention-shared-frame", &[(0, 0xB6), (1, 0xC7)]);
    let reader = pool.register_reader().expect("reader slot is available");
    let page = PageId::new(file, 0);
    let first_guard = get_guard(&pool, &reader, page);
    let second_guard = get_guard(&pool, &reader, page);
    assert_eq!(first_guard.as_ptr(), second_guard.as_ptr());
    assert_bytes(&first_guard, 0xB6);
    assert_bytes(&second_guard, 0xB6);

    let Ok(first) = first_guard.into_retained() else {
        panic!("the first guard must promote");
    };
    let Ok(second) = second_guard.into_retained() else {
        panic!("a second guard for the same frame must promote independently");
    };

    let distinct_guard = get_guard(&pool, &reader, PageId::new(file, 1));
    let distinct_address = distinct_guard.as_ptr();
    let Err(RetainRefused {
        guard: distinct_guard,
        reason,
    }) = distinct_guard.into_retained()
    else {
        panic!("a distinct frame must be refused while both handles are retained");
    };
    assert!(matches!(reason, RetainRefusedReason::Exhausted));
    assert_eq!(distinct_guard.as_ptr(), distinct_address);

    drop(first);
    let Err(RetainRefused {
        guard: distinct_guard,
        reason,
    }) = distinct_guard.into_retained()
    else {
        panic!("one surviving same-frame handle must keep the budget occupied");
    };
    assert!(matches!(reason, RetainRefusedReason::Exhausted));
    assert_eq!(distinct_guard.as_ptr(), distinct_address);
    assert_bytes(&distinct_guard, 0xC7);

    drop(second);
    let Ok(later) = distinct_guard.into_retained() else {
        panic!("dropping both same-frame handles must restore promotion capacity");
    };
    assert_bytes(&later, 0xC7);
}

#[test]
fn distinct_second_frame_refusal_returns_the_same_live_guard() {
    let (pool, file) = pool_with_pages("retention-budget", &[(0, 0xC7), (1, 0xD8)]);
    let reader = pool.register_reader().expect("reader slot is available");
    let first_guard = get_guard(&pool, &reader, PageId::new(file, 0));
    let Ok(_first) = first_guard.into_retained() else {
        panic!("the first distinct frame must consume the one-frame budget");
    };

    let second_guard = get_guard(&pool, &reader, PageId::new(file, 1));
    let second_address = second_guard.as_ptr();
    let Err(RetainRefused { guard, reason }) = second_guard.into_retained() else {
        panic!("a distinct second frame must be refused at the one-frame budget");
    };
    assert!(matches!(reason, RetainRefusedReason::Exhausted));
    assert_eq!(guard.as_ptr(), second_address);
    assert_bytes(&guard, 0xD8);
}
