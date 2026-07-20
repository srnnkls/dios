//! Gateway contract (T015): drives the nmnm Gateway loop shape against the REAL
//! [`Pool`] over the seeded [`MockDriver`], extending the T016 spike from a
//! stub pool to the composed miss path (T008). This is the compile-tested
//! API-fit proof for nmnm's specced-but-unbuilt `BlockCache`/`MemHandle`
//! residency layer — no nmnm wiring, only the contract its Gateway needs.
//!
//! The loop shape under test (a morsel worker over a `WorkItem` queue):
//!  - `get` -> `Hit`  : the page is resident; the item runs its kernel now.
//!  - `get` -> `Pending`: the page faults; the item PARKS and the worker takes
//!    OTHER ready work rather than blocking on the outstanding read.
//!  - `poll` + `ready`: resume parked items once their read has landed.
//!  - dropping a `PendingToken` cancels waiter INTEREST only — the in-flight
//!    read still completes and the page still becomes resident.
//!
//! What this proves beyond T016:
//!  1. Real interleaving: a parked (faulted) item does not block a later ready
//!     item — the ready item completes FIRST.
//!  2. Multiple concurrent in-flight misses across items do not clobber each
//!     other; each frame carries its own page's bytes.
//!  3. IO-error fanout across a cancelled+live waiter pair on ONE singleflight
//!     read: the live waiter observes `ReadyResult::Err` carrying the errno;
//!     the cancelled waiter (its token dropped) is unaffected.
//!  4. The residency-lease steal boundary (SHAPE ONLY — the lease is NOT built
//!     here, it is named for extraction).
//!
//! RESIDENCY-LEASE SEAM — the open decision this example pins but does not
//! resolve. nmnm's work-stealing executor moves a `WorkItem` across threads at
//! the Ready transition. [`FrameGuard`] is `!Send`, so it CANNOT be captured
//! into a stolen item. The missing surface is a Send-able, coarse
//! per-`WorkItem` residency lease: a refcount pin taken at the Ready transition
//! and released at item completion, keeping the page resident across the steal
//! so the destination worker re-borrows a `Hit` instead of re-missing. This
//! example carries a single-threaded STAND-IN at exactly that transition
//! (`residency_lease_steal_boundary_stand_in`); the required semantics, the
//! `!Send` rationale, and the pool-API surface live in
//! `scopes/draft/dios-v1/resources/extraction.md`.

use std::collections::VecDeque;
use std::path::Path;

use dios::testing::{
    Injected, MockDriver, MockPoolTestingExt, PoolBuilderTestingExt, PoolTestingExt,
};
use dios::{DirectIo, FileId, FrameGuard, Get, PageId, PendingToken, Pool, ReaderCtx, ReadyResult};

const FRAME_BYTES: u32 = 4096;
const READY_POLLS_MAX: u32 = 64;
const EIO: i32 = 5;

fn main() {
    faulted_worker_takes_ready_work();
    waiter_interest_drop_still_residents();
    multiple_inflight_misses_do_not_clobber();
    error_fanout_to_cancelled_and_live_pair();
    residency_lease_steal_boundary_stand_in();
    send_able_handles_cross_the_steal_boundary();
    println!("gateway contract: the nmnm Gateway loop shape drove the composed pool");
}

/// A pool over a seeded mock, sized for the Gateway scenarios: four concurrent
/// guards and three concurrent misses per reader. Pages are seeded before the
/// mock is composed in.
fn contract_pool(seed: u64, seeds: &[(u32, u8)]) -> (Pool<MockDriver>, FileId) {
    let frames = 16u32;
    let mock = MockDriver::builder()
        .seed(seed)
        .queue_capacity(16)
        .frames(frames)
        .frame_bytes(FRAME_BYTES)
        .retry_bound(0)
        .build();
    let file = mock
        .open(Path::new("gateway"), DirectIo::Disabled)
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
        .expect("watermark-satisfying contract pool composes over the mock");
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
    reader: &'pool ReaderCtx,
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
            ReadyResult::Err(err) => panic!("no fault injected, got io error: {err}"),
        }
    }
    panic!("token never readied under bounded polling");
}

fn warm<'pool>(pool: &'pool Pool<MockDriver>, reader: &'pool ReaderCtx, page: PageId, fill: u8) {
    match pool.get(reader, page).expect("the registered file is live") {
        Get::Pending(token) => {
            let guard = drive_ready(pool, reader, token);
            assert_frame_fill(&guard, fill);
        }
        Get::Hit(_) => panic!("first touch of a cold page cannot be a warm hit"),
        Get::Busy => panic!("a fresh pool has spare frames; warm-up must submit"),
    }
}

/// One unit of Gateway work: a page to make resident and the fill that proves
/// the kernel saw the right bytes. `fill` doubles as the item's identity in the
/// recorded completion order.
#[derive(Clone, Copy)]
struct WorkItem {
    page: PageId,
    fill: u8,
}

/// The nmnm Gateway worker loop over the REAL pool. `get`-`Hit` items run their
/// kernel immediately; `get`-`Pending` items park and let the worker take other
/// ready work; a bounded `poll`/`ready` drain resumes the parked ones. Returns
/// the fills in the order items COMPLETED — the interleaving oracle.
fn drain_gateway(pool: &Pool<MockDriver>, reader: &ReaderCtx, items: &[WorkItem]) -> Vec<u8> {
    let mut ready_queue: VecDeque<WorkItem> = items.iter().copied().collect();
    let mut parked: Vec<(u8, PendingToken)> = Vec::with_capacity(items.len());
    let mut completed: Vec<u8> = Vec::with_capacity(items.len());

    while let Some(item) = ready_queue.pop_front() {
        match pool
            .get(reader, item.page)
            .expect("the registered file is live")
        {
            Get::Hit(guard) => {
                assert_frame_fill(&guard, item.fill);
                completed.push(item.fill);
            }
            Get::Pending(token) => parked.push((item.fill, token)),
            Get::Busy => panic!("the contract pool is sized above the in-flight bound"),
        }
    }

    for _ in 0..READY_POLLS_MAX {
        if parked.is_empty() {
            return completed;
        }
        pool.poll();
        let mut still = Vec::with_capacity(parked.len());
        for (fill, token) in parked.drain(..) {
            match pool.ready(reader, token) {
                ReadyResult::Ready(guard) => {
                    assert_frame_fill(&guard, fill);
                    completed.push(fill);
                }
                ReadyResult::NotYet(handed_back) => still.push((fill, handed_back)),
                ReadyResult::Err(err) => panic!("no fault injected, got io error: {err}"),
            }
        }
        parked = still;
    }
    panic!("parked Gateway items never drained under bounded polling");
}

fn faulted_worker_takes_ready_work() {
    let (pool, file) = contract_pool(0x0060_0D5E, &[(1, 0x11), (5, 0x55)]);
    let reader = pool
        .register_reader()
        .expect("first reader slot is available");

    let hit_page = PageId::new(file, 1);
    warm(&pool, &reader, hit_page, 0x11);

    let items = [
        WorkItem {
            page: PageId::new(file, 5),
            fill: 0x55,
        },
        WorkItem {
            page: hit_page,
            fill: 0x11,
        },
    ];
    let completed = drain_gateway(&pool, &reader, &items);

    assert_eq!(
        completed,
        vec![0x11, 0x55],
        "the resident item (0x11) completes before the earlier faulted item (0x55) — the fault did not block the worker"
    );
}

fn waiter_interest_drop_still_residents() {
    let (pool, file) = contract_pool(0x00CA_9CE1, &[(3, 0xCC)]);
    let reader = pool.register_reader().expect("reader slot is available");
    let page = PageId::new(file, 3);

    {
        let interest = match pool
            .get(&reader, page)
            .expect("the registered file is live")
        {
            Get::Pending(token) => token,
            Get::Hit(_) => panic!("a cold page cannot hit"),
            Get::Busy => panic!("spare frames exist; a miss submits"),
        };
        assert_eq!(interest.page(), page);
    }
    pool.poll();

    match pool.get(&reader, page).expect("the registered file is live") {
        Get::Hit(guard) => assert_frame_fill(&guard, 0xCC),
        Get::Pending(_) => panic!(
            "dropping a PendingToken cancels waiter interest only — the read still completed and made the page resident"
        ),
        Get::Busy => panic!("a resident page is never Busy"),
    }
}

fn multiple_inflight_misses_do_not_clobber() {
    let (pool, file) = contract_pool(0x00D5_7EED, &[(10, 0x10), (11, 0x11), (12, 0x12)]);
    let reader = pool.register_reader().expect("reader slot is available");

    let sources = [
        (PageId::new(file, 10), 0x10u8),
        (PageId::new(file, 11), 0x11u8),
        (PageId::new(file, 12), 0x12u8),
    ];

    let mut pending = Vec::with_capacity(sources.len());
    for (page, _) in sources {
        match pool
            .get(&reader, page)
            .expect("the registered file is live")
        {
            Get::Pending(token) => pending.push(token),
            Get::Hit(_) => panic!("a cold source cannot hit"),
            Get::Busy => panic!("three misses sit within max_inflight_reads"),
        }
    }
    assert_eq!(
        pending.len(),
        3,
        "three misses are in flight simultaneously before any poll"
    );

    for ((_, fill), token) in sources.iter().zip(pending) {
        let guard = drive_ready(&pool, &reader, token);
        assert_frame_fill(&guard, *fill);
    }
}

fn error_fanout_to_cancelled_and_live_pair() {
    let (pool, file) = contract_pool(0x00E1_2FA1, &[(9, 0x99)]);
    let reader = pool.register_reader().expect("reader slot is available");
    let page = PageId::new(file, 9);

    let live = {
        let cancelled = match pool
            .get(&reader, page)
            .expect("the registered file is live")
        {
            Get::Pending(token) => token,
            Get::Hit(_) => panic!("a cold page cannot hit"),
            Get::Busy => panic!("spare frames exist; a miss submits"),
        };
        let live = match pool
            .get(&reader, page)
            .expect("the registered file is live")
        {
            Get::Pending(token) => token,
            Get::Hit(_) => panic!("the read is still in flight; a joiner cannot hit"),
            Get::Busy => panic!("a singleflight joiner submits no new read"),
        };
        assert_eq!(cancelled.page(), page);
        live
    };

    pool.driver().inject_next(Injected::Io(EIO));
    let err = drive_err(&pool, &reader, live);
    assert_eq!(
        err.raw_os_error(),
        Some(EIO),
        "the fanned-out failure carries the read's errno to the live waiter"
    );

    warm(&pool, &reader, page, 0x99);
}

fn drive_err<'pool>(
    pool: &'pool Pool<MockDriver>,
    reader: &'pool ReaderCtx,
    token: PendingToken,
) -> dios::IoError {
    let mut token = token;
    for _ in 0..READY_POLLS_MAX {
        match pool.ready(reader, token) {
            ReadyResult::Err(err) => return err,
            ReadyResult::NotYet(handed_back) => {
                token = handed_back;
                pool.poll();
            }
            ReadyResult::Ready(_) => panic!("the injected fault must surface as Err, not a hit"),
        }
    }
    panic!("the injected fault never fanned out under bounded polling");
}

/// Carries ONLY the Send-able [`PageId`] — never the `!Send` [`FrameGuard`],
/// which cannot cross the steal boundary. In nmnm the missing residency lease
/// travels with this payload; the stand-in re-borrows the page on resume.
#[derive(Clone, Copy)]
struct StolenWork {
    page: PageId,
}

/// The residency-lease steal boundary — SHAPE ONLY. An item faults, readies
/// (the Ready transition, where nmnm acquires the coarse residency lease), then
/// is "stolen" to another worker. `FrameGuard` is `!Send` and is DROPPED at the
/// boundary; only the Send-able [`StolenWork`] crosses. On resume the page must
/// still be resident — a `Hit`, not a re-miss.
///
/// Single-threaded stand-in: with no eviction pressure the pool keeps the page
/// resident across the poll on its own, so this runs GREEN. Under real pressure
/// that residency is NOT guaranteed without the lease — the lease's required
/// semantics, and why `FrameGuard` cannot cross, are owned by
/// `scopes/draft/dios-v1/resources/extraction.md`.
fn residency_lease_steal_boundary_stand_in() {
    let (pool, file) = contract_pool(0x005E_A100, &[(7, 0x77)]);
    let reader = pool.register_reader().expect("reader slot is available");
    let page = PageId::new(file, 7);

    let token = match pool
        .get(&reader, page)
        .expect("the registered file is live")
    {
        Get::Pending(token) => token,
        Get::Hit(_) => panic!("a cold page cannot hit"),
        Get::Busy => panic!("spare frames exist; a miss submits"),
    };
    let guard = drive_ready(&pool, &reader, token);
    assert_frame_fill(&guard, 0x77);

    let stolen = StolenWork { page };
    assert_send(&stolen);
    drop(guard);
    pool.poll();

    match pool
        .get(&reader, stolen.page)
        .expect("the registered file is live")
    {
        Get::Hit(guard) => assert_frame_fill(&guard, 0x77),
        Get::Pending(_) => panic!(
            "resume re-missed — the residency lease (extraction.md) must keep the page resident across the steal so the destination worker Hits"
        ),
        Get::Busy => panic!("a resident page is never Busy"),
    }
}

fn assert_send<T: Send>(_value: &T) {}

/// `FrameGuard: !Send` — the negative space this positive-only check relies on —
/// is pinned by `tests/guard_compile_fail.rs::frame_guard_is_not_send`. The
/// thread-boundary mechanism that makes `!Send` matter (`ReaderCtx: !Send`,
/// `E0277`) is pinned by `compile_fail` doctest B in `src/pool/epoch.rs`.
fn send_able_handles_cross_the_steal_boundary() {
    fn require_send<T: Send>() {}
    require_send::<PageId>();
    require_send::<PendingToken>();
}
