//! A consumer that polls only while it waits must still find a sequential scan
//! resident ahead of it: automatic readahead refills while the consumer works
//! through the window, not only once it blocks. The scan runs on the shipping
//! ring over a page-cache-resident file, whose buffered reads complete at
//! submission, so the schedule is deterministic. A pool that holds every page
//! leaves reclamation out; a pool a quarter of the file's size makes every
//! refill evict, and victims evicted while the consumer is pinned mature only
//! at a later pass. Readahead runs past the end of the file and resets its
//! stream there, so the last two windows are not steady.

#![cfg(target_os = "linux")]

use std::path::PathBuf;

use dios::{DirectIo, FrameGuard, Get, PageId, Pool, ReadyResult};

const GRANULE: u32 = 4096;
const PAGES: u32 = 4096;
const STEADY: std::ops::Range<u32> = 1024..PAGES - 256;
const FRAMES_WHOLE_FILE: u32 = PAGES + 1024;
const FRAMES_QUARTER_FILE: u32 = PAGES / 4;
const POLLS_MAX: u32 = 1 << 16;

fn fill_of(page: u32) -> u8 {
    u8::try_from(page % 251).expect("a byte-sized fill")
}

fn assert_page(guard: &FrameGuard<'_>, page: u32) {
    assert!(
        guard.iter().all(|&byte| byte == fill_of(page)),
        "page {page} carries its own bytes"
    );
}

#[test]
fn a_scan_that_polls_only_while_blocked_meets_no_pending_page_after_startup() {
    scan_meets_no_pending_page_after_startup(None, 128, FRAMES_WHOLE_FILE);
}

#[test]
fn a_two_vector_budget_keeps_the_scan_ahead_after_startup() {
    scan_meets_no_pending_page_after_startup(Some(256 * 1024), 64, FRAMES_WHOLE_FILE);
}

#[test]
fn a_scan_through_a_quarter_size_pool_stays_ahead_after_startup() {
    scan_meets_no_pending_page_after_startup(None, 128, FRAMES_QUARTER_FILE);
}

#[test]
fn a_two_vector_budget_through_a_quarter_size_pool_stays_ahead_after_startup() {
    scan_meets_no_pending_page_after_startup(Some(256 * 1024), 64, FRAMES_QUARTER_FILE);
}

#[test]
fn a_scan_whose_refills_run_beside_another_pinned_reader_stays_ahead_after_startup() {
    let paths = [0_u32, 1].map(|scan| {
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("scan-refill-pair-{scan}-{}", std::process::id()));
        let mut content = Vec::with_capacity((PAGES * GRANULE) as usize);
        for page in 0..PAGES {
            content.resize(content.len() + GRANULE as usize, fill_of(page));
        }
        std::fs::write(&path, &content).expect("write a page-cache-resident scan file");
        path
    });
    let pool = Pool::builder()
        .frame_count(PAGES / 2)
        .granule(GRANULE)
        .max_concurrent_readers(2)
        .peak_guards_per_reader(1)
        .max_inflight_reads(256)
        .miss_headroom(768)
        .build()
        .expect("a pool a quarter the size of both scans");
    let files = paths.each_ref().map(|path| {
        pool.open(path, DirectIo::Disabled)
            .expect("open a scan file")
    });
    let readers = [0, 1].map(|_| pool.register_reader().expect("a reader slot"));
    let mut steady_pending = 0_u32;
    for page in 0..PAGES {
        // The second reader's pin spans the first reader's get, where the refill
        // passes this interleaving schedules run.
        let held = pin_or_wait(
            &pool,
            &readers[1],
            PageId::new(files[1], page),
            &mut steady_pending,
        );
        drop(pin_or_wait(
            &pool,
            &readers[0],
            PageId::new(files[0], page),
            &mut steady_pending,
        ));
        drop(held);
    }
    assert_eq!(
        steady_pending, 0,
        "after startup both scans find every page resident"
    );
    drop(pool);
    for path in paths {
        std::fs::remove_file(path).expect("unlink a scan file");
    }
}

fn pin_or_wait<'pool>(
    pool: &'pool Pool,
    reader: &'pool dios::ReaderCtx,
    page: PageId,
    steady_pending: &mut u32,
) -> FrameGuard<'pool> {
    let index = page.granule_idx();
    let mut token = match pool.get(reader, page).expect("live file") {
        Get::Hit(guard) => {
            assert_page(&guard, index);
            return guard;
        }
        Get::Pending(token) => token,
        Get::Busy => panic!("page {index}: the pool reclaims a victim for each miss"),
    };
    if STEADY.contains(&index) {
        *steady_pending += 1;
    }
    for _ in 0..POLLS_MAX {
        pool.poll();
        match pool.ready(reader, token) {
            ReadyResult::Ready(guard) => {
                assert_page(&guard, index);
                return guard;
            }
            ReadyResult::NotYet(again) => token = again,
            ReadyResult::Err(error) => panic!("page {index}: a cached read failed: {error}"),
        }
    }
    panic!("page {index}: a cached read completes within the poll bound");
}

fn scan_meets_no_pending_page_after_startup(
    budget_bytes: Option<u32>,
    credits: u32,
    frame_count: u32,
) {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "scan-refill-{credits}-{frame_count}-{}",
        std::process::id()
    ));
    let mut content = Vec::with_capacity((PAGES * GRANULE) as usize);
    for page in 0..PAGES {
        content.resize(content.len() + GRANULE as usize, fill_of(page));
    }
    std::fs::write(&path, &content).expect("write the page-cache-resident scan file");
    let mut builder = Pool::builder()
        .frame_count(frame_count)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(256)
        .miss_headroom(768);
    if let Some(bytes) = budget_bytes {
        builder = builder.readahead_budget_bytes(bytes);
    }
    let pool = builder.build().expect("a pool for the scan");
    assert_eq!(
        pool.prefetch_stats().capacity,
        credits,
        "the budget sets the speculative credits"
    );
    let file = pool
        .open(&path, DirectIo::Disabled)
        .expect("open the scan file");
    let reader = pool.register_reader().expect("a reader slot");

    let mut steady_pending = 0_u32;
    for page in 0..PAGES {
        let mut token = match pool
            .get(&reader, PageId::new(file, page))
            .expect("live file")
        {
            Get::Hit(guard) => {
                assert_page(&guard, page);
                continue;
            }
            Get::Pending(token) => token,
            Get::Busy => panic!("page {page}: a single reader's miss reclaims its own victim"),
        };
        if STEADY.contains(&page) {
            steady_pending += 1;
        }
        let mut ready = None;
        for _ in 0..POLLS_MAX {
            pool.poll();
            match pool.ready(&reader, token) {
                ReadyResult::Ready(guard) => {
                    ready = Some(guard);
                    break;
                }
                ReadyResult::NotYet(again) => token = again,
                ReadyResult::Err(error) => panic!("page {page}: a cached read failed: {error}"),
            }
        }
        assert_page(
            &ready.expect("a cached read completes within the poll bound"),
            page,
        );
    }

    assert_eq!(
        steady_pending, 0,
        "after startup every page is resident before the consumer reaches it"
    );
    assert!(
        pool.prefetch_stats().automatic_admitted >= u64::from(STEADY.end - STEADY.start),
        "automatic readahead, not demand, read the steady region"
    );
    drop(pool);
    std::fs::remove_file(&path).expect("unlink the scan file");
}
