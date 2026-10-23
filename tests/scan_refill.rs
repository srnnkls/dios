//! A consumer that polls only while it waits must still find a sequential scan
//! resident ahead of it: automatic readahead refills while the consumer works
//! through the window, not only once it blocks. The scan runs on the shipping
//! ring over a page-cache-resident file, whose buffered reads complete at
//! submission, so the schedule is deterministic; the pool holds every page, so
//! reclamation plays no part. Readahead runs past the end of the file and resets
//! its stream there, so the last two windows are not steady.

#![cfg(target_os = "linux")]

use std::path::PathBuf;

use dios::{DirectIo, FrameGuard, Get, PageId, Pool, ReadyResult};

const GRANULE: u32 = 4096;
const PAGES: u32 = 4096;
const STEADY: std::ops::Range<u32> = 1024..PAGES - 256;
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
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("scan-refill-{}", std::process::id()));
    let mut content = Vec::with_capacity((PAGES * GRANULE) as usize);
    for page in 0..PAGES {
        content.resize(content.len() + GRANULE as usize, fill_of(page));
    }
    std::fs::write(&path, &content).expect("write the page-cache-resident scan file");
    let pool = Pool::builder()
        .frame_count(PAGES + 1024)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(256)
        .miss_headroom(768)
        .build()
        .expect("a pool that holds the whole scan");
    assert_eq!(
        pool.prefetch_stats().capacity,
        128,
        "the default 512 KiB budget"
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
            Get::Busy => panic!("page {page}: the pool holds every page of the scan"),
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
