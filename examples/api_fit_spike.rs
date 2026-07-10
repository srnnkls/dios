//! API-fit spike (T016): drives two consumer shapes against the composed
//! [`Pool`] over the seeded [`MockDriver`], falsifying the `Get`/`Pending`/`ready`
//! contract end to end (the T008 miss path now owns the bookkeeping this spike
//! once stubbed).
//!
//! The two shapes exercise the external consumer contracts this API must serve:
//!  1. nmnm Gateway loop — a faulted worker takes other resident work rather
//!     than blocking; and the waiter-interest drop, where dropping a
//!     `PendingToken` cancels interest only, yet the in-flight read still
//!     completes and the page becomes resident.
//!  2. sira k-way-merge suspend — cursors hold pinned `FrameGuard`s across a
//!     suspend on a non-resident next block, and an unrelated `poll()` must
//!     leave the held bytes stable.
//!
//! Two ADT decisions mirror the scope contract:
//!  A. Residency is a three-variant sum `Get::{Hit(FrameGuard), Pending(token),
//!     Busy}`, not an `Option<Guard>` beside a status flag.
//!  B. A readiness re-check is `ReadyResult::{Ready(guard), NotYet(token),
//!     Err(io)}`, handing the token back on `NotYet` (a non-consuming
//!     poll-again) and freeing the frame on `Err`.

use std::path::Path;

use dios::mock::MockDriver;
use dios::{FileId, FrameGuard, Get, OpenHow, PageId, PendingToken, Pool, ReaderCtx, ReadyResult};

const FRAME_BYTES: u32 = 4096;
const READY_POLLS_MAX: u32 = 64;

fn main() {
    gateway_loop_shape();
    k_way_merge_suspend_shape();
    println!("api-fit spike: both consumer shapes drove the composed pool");
}

/// A pool over a seeded mock, sized for four concurrent guards and three
/// concurrent misses. Pages are seeded before the mock is composed in.
fn spike_pool(seed: u64, seeds: &[(u32, u8)]) -> (Pool<MockDriver>, FileId) {
    let frames = 16u32;
    let mock = MockDriver::builder()
        .seed(seed)
        .queue_capacity(16)
        .frames(frames)
        .frame_bytes(FRAME_BYTES)
        .retry_bound(0)
        .build();
    let file = mock
        .open(Path::new("spike"), OpenHow::read_write())
        .expect("mock file opens");
    let file_id = file.file_id();
    for &(granule_idx, fill) in seeds {
        mock.seed_page(&file, granule_idx, fill);
    }
    let pool = Pool::builder()
        .frame_count(frames)
        .granule(FRAME_BYTES)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(4)
        .max_inflight_reads(3)
        .miss_headroom(9)
        .build_on(mock)
        .expect("watermark-satisfying spike pool composes over the mock");
    pool.register_file(file);
    (pool, file_id)
}

fn assert_frame_fill(guard: &FrameGuard<'_>, fill: u8) {
    let frame_bytes = FRAME_BYTES as usize;
    assert_eq!(
        guard.len(),
        frame_bytes,
        "a frame guard borrows the whole granule"
    );
    assert!(
        guard.iter().all(|&byte| byte == fill),
        "the borrowed bytes are this page's seeded content, not another frame's"
    );
    assert_eq!(guard[0], fill, "leading byte identifies the page");
    assert_eq!(
        guard[frame_bytes - 1],
        fill,
        "trailing byte identifies the page"
    );
}

fn drive_ready<'pool>(
    pool: &'pool Pool<MockDriver>,
    reader: &'pool ReaderCtx<'pool>,
    token: PendingToken,
) -> FrameGuard<'pool> {
    let mut token = token;
    for _ in 0..READY_POLLS_MAX {
        match pool.ready(reader, token) {
            ReadyResult::Ready(guard) => return guard,
            ReadyResult::NotYet(handed_back) => {
                token = handed_back;
                pool.poll();
            }
            ReadyResult::Err(err) => panic!("the spike injects no faults, got io error: {err}"),
        }
    }
    panic!("token never readied under bounded polling");
}

fn warm<'pool>(
    pool: &'pool Pool<MockDriver>,
    reader: &'pool ReaderCtx<'pool>,
    page: PageId,
    fill: u8,
) {
    match pool.get(reader, page) {
        Get::Pending(token) => {
            let guard = drive_ready(pool, reader, token);
            assert_frame_fill(&guard, fill);
        }
        Get::Hit(_) => panic!("first touch of a cold page cannot be a warm hit"),
        Get::Busy => panic!("a fresh pool has spare frames; warm-up must submit"),
    }
}

fn gateway_loop_shape() {
    let (pool, file) = spike_pool(0x0060_0D5E, &[(1, 0xBB), (2, 0xAA), (3, 0xCC)]);
    let reader = pool
        .register_reader()
        .expect("first reader slot is available");

    let page_resident = PageId::new(file, 1);
    let page_miss = PageId::new(file, 2);
    let page_dropped = PageId::new(file, 3);

    warm(&pool, &reader, page_resident, 0xBB);

    let pending = match pool.get(&reader, page_miss) {
        Get::Pending(token) => token,
        Get::Hit(_) => panic!("a cold page cannot hit"),
        Get::Busy => panic!("spare frames exist; a miss submits, it does not backpressure"),
    };

    match pool.get(&reader, page_resident) {
        Get::Hit(guard) => assert_frame_fill(&guard, 0xBB),
        Get::Pending(_) => {
            panic!("a resident page must hit — re-submitting would block the worker")
        }
        Get::Busy => panic!("a resident hit never backpressures"),
    }

    let resumed = drive_ready(&pool, &reader, pending);
    assert_frame_fill(&resumed, 0xAA);
    drop(resumed);

    let interest = match pool.get(&reader, page_dropped) {
        Get::Pending(token) => token,
        Get::Hit(_) => panic!("a cold page cannot hit"),
        Get::Busy => panic!("spare frames exist; a miss submits"),
    };
    drop(interest);
    pool.poll();
    match pool.get(&reader, page_dropped) {
        Get::Hit(guard) => assert_frame_fill(&guard, 0xCC),
        Get::Pending(_) => {
            panic!(
                "dropping a PendingToken cancels waiter interest only — the read still completed and made the page resident"
            )
        }
        Get::Busy => panic!("a resident page is never Busy"),
    }
}

fn k_way_merge_suspend_shape() {
    let (pool, file) = spike_pool(
        0x00D5_7EED,
        &[(10, 0x10), (11, 0x11), (12, 0x12), (20, 0x20)],
    );
    let reader = pool.register_reader().expect("reader slot is available");

    let sources = [
        (PageId::new(file, 10), 0x10u8),
        (PageId::new(file, 11), 0x11u8),
        (PageId::new(file, 12), 0x12u8),
    ];

    let mut pending = Vec::with_capacity(sources.len());
    for (page, _) in sources {
        match pool.get(&reader, page) {
            Get::Pending(token) => pending.push(token),
            Get::Hit(_) => panic!("a cold source cannot hit"),
            Get::Busy => panic!("k < frame count; every source must submit"),
        }
    }

    let mut guards = Vec::with_capacity(sources.len());
    for ((_, fill), token) in sources.iter().zip(pending) {
        let guard = drive_ready(&pool, &reader, token);
        assert_frame_fill(&guard, *fill);
        guards.push(guard);
    }

    let next_block = PageId::new(file, 20);
    let suspended = match pool.get(&reader, next_block) {
        Get::Pending(token) => token,
        Get::Hit(_) => panic!("the next block is cold"),
        Get::Busy => panic!("still within the frame budget"),
    };

    let held = guards[1][0];
    assert_eq!(held, 0x11, "sibling guard's content before the poll");
    pool.poll();
    assert_eq!(
        guards[1][0], held,
        "a held guard's bytes stay stable across an unrelated poll"
    );
    assert_frame_fill(&guards[2], 0x12);

    let resumed = drive_ready(&pool, &reader, suspended);
    assert_frame_fill(&resumed, 0x20);
    drop(resumed);
    drop(guards);
}
