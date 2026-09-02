//! T005 alloc-count harness (INV-2 / DIO-G4), covering both the advanced
//! driver and the crate-root Pool product path.
//!
//! A thread-local counting `#[global_allocator]` records allocations only inside
//! an armed window on the measuring thread, so parallel libtest threads never
//! pollute the count. After warmup, a warm `submit` + `poll`-drain cycle must
//! allocate NOTHING — the completion slab, ready queue, fd table, and completion
//! batch are all fixed at init, and the eager/uring poll paths reuse them.
//!
//! `Driver` binds the cfg-selected shipping backend, so this file measures the
//! eager backend on darwin and the `io_uring` backend on Linux — DIO-G4's "both
//! backends" across the two hosts.
//!
//! T009 seam the implementer must expose for the store-elision gate below:
//! `Pool::clock_reference_stores(&self) -> u64` — cumulative CLOCK reference-bit
//! stores, observed across the real `get()` hit path. The close-during-drain
//! gate needs no new seam; it asserts the batch-5 deferred-retire path stays
//! alloc-free. The real `Pool<Driver>` write/fsync/report gate runs on the
//! cfg-selected shipping backend; deterministic read/backpressure and bounded
//! overflow gates additionally run on `Pool<MockDriver>` under `feature =
//! "mock"`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use dios::driver::{CompletionBatch, Driver, FileHandle};
use dios::testing::{DriverReadTestingExt, PoolTestingExt, ReadFrameIdx};
use dios::{
    DirectIo, FrameGuard, Get, PageId, Pool, PoolCompletion, PoolCompletionBatch, PoolToken,
    PoolWriteArena, ReaderCtx, ReadyResult, RetireStatus, SyncMode,
};

const FRAME_BYTES: u32 = 4096;
const DRAIN_POLLS_MAX: u32 = 1_000_000;
const RETIRE_POLLS_MAX: u32 = 32;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

struct CountingAllocator;

// SAFETY: every method forwards the caller's layout/pointer unchanged to the
// system allocator, which satisfies the `GlobalAlloc` contract; the only added
// behaviour is a thread-local counter bump that never itself allocates
// (const-initialised `Cell`s).
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.try_with(Cell::get).unwrap_or(false) {
            ALLOCS.with(|count| count.set(count.get() + 1));
        }
        // SAFETY: `layout` is forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.try_with(Cell::get).unwrap_or(false) {
            ALLOCS.with(|count| count.set(count.get() + 1));
        }
        // SAFETY: `ptr`/`layout`/`new_size` are forwarded unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr`/`layout` are forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// Counts allocations charged to the current thread while `body` runs.
fn armed_allocations(body: impl FnOnce()) -> u64 {
    ALLOCS.with(|count| count.set(0));
    ARMED.with(|armed| armed.set(true));
    body();
    ARMED.with(|armed| armed.set(false));
    ALLOCS.with(Cell::get)
}

fn armed_allocations_result<T>(body: impl FnOnce() -> T) -> (u64, T) {
    ALLOCS.with(|count| count.set(0));
    ARMED.with(|armed| armed.set(true));
    let result = body();
    ARMED.with(|armed| armed.set(false));
    let allocations = ALLOCS.with(Cell::get);
    (allocations, result)
}

static UNIQUE: AtomicU32 = AtomicU32::new(0);

fn temp_frames(tag: &str, frame_count: u32, fill: u8) -> PathBuf {
    let n = UNIQUE.fetch_add(1, Ordering::Relaxed);
    let mut path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&path).expect("target tmp dir");
    path.push(format!("zeroalloc-{tag}-{}-{n}", std::process::id()));
    let byte_count =
        usize::try_from(frame_count).expect("test frame count fits usize") * FRAME_BYTES as usize;
    std::fs::write(&path, vec![fill; byte_count]).expect("seed full frames");
    path
}

fn temp_frame(tag: &str) -> PathBuf {
    temp_frames(tag, 1, 0x4B)
}

fn driver() -> Driver {
    Driver::builder()
        .queue_capacity(4)
        .frames(2)
        .frame_bytes(FRAME_BYTES)
        .write_slots(1)
        .retry_bound(3)
        .build()
        .expect("the test driver initializes")
}

fn product_pool() -> Pool<Driver> {
    Pool::builder()
        .frame_count(4)
        .granule(FRAME_BYTES)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .write_slots(1)
        .max_inflight_product_ops(2)
        .build()
        .expect("the cfg-selected shipping pool initializes")
}

fn product_retention_pool() -> Pool<Driver> {
    Pool::builder()
        .frame_count(5)
        .granule(FRAME_BYTES)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .max_retained_frames(1)
        .build()
        .expect("the cfg-selected shipping pool initializes with retention")
}

fn open(drv: &Driver, path: &Path) -> FileHandle {
    drv.open(path, DirectIo::Disabled)
        .expect("open the seeded file")
}

fn resolve_product_page<'pool>(
    pool: &'pool Pool<Driver>,
    reader: &'pool ReaderCtx,
    page: PageId,
) -> FrameGuard<'pool> {
    for _ in 0..DRAIN_POLLS_MAX {
        let mut token = match pool.get(reader, page).expect("the product file is live") {
            Get::Hit(guard) => return guard,
            Get::Pending(token) => token,
            Get::Busy => {
                pool.poll();
                continue;
            }
        };
        for _ in 0..DRAIN_POLLS_MAX {
            match pool.ready(reader, token) {
                ReadyResult::Ready(guard) => return guard,
                ReadyResult::NotYet(returned) => {
                    token = returned;
                    pool.poll();
                }
                ReadyResult::Err(error) => panic!("fault-free product warmup failed: {error:?}"),
            }
        }
        panic!("submitted product warmup did not complete within the fixed poll bound");
    }
    panic!("product warmup remained Busy beyond the fixed poll bound");
}

/// Polls until at least one completion drains, bounded by an iteration cap.
fn drain_one(drv: &Driver, out: &mut CompletionBatch) {
    for _ in 0..DRAIN_POLLS_MAX {
        if drv.poll(out) > 0 {
            return;
        }
    }
    panic!("poll made no progress draining a completion");
}

fn drain_product_write_and_fsync(
    pool: &Pool<Driver>,
    out: &mut PoolCompletionBatch,
    write: PoolToken,
    fsync: PoolToken,
) -> u32 {
    let mut backend_completions = 0u32;
    let mut write_seen = false;
    let mut fsync_seen = false;
    for _ in 0..DRAIN_POLLS_MAX {
        let report = pool.poll_report(out);
        backend_completions += report.backend_completions();
        assert_eq!(report.reclaimed_frames(), 0);
        for completion in out.iter() {
            match completion {
                PoolCompletion::Write {
                    token,
                    result: Ok(bytes),
                } => {
                    assert_eq!(*token, write);
                    assert_eq!(*bytes, FRAME_BYTES);
                    assert!(!write_seen, "the write result is delivered once");
                    write_seen = true;
                }
                PoolCompletion::Fsync {
                    token,
                    result: Ok(()),
                } => {
                    assert_eq!(*token, fsync);
                    assert!(!fsync_seen, "the fsync result is delivered once");
                    fsync_seen = true;
                }
                PoolCompletion::Write {
                    result: Err(error), ..
                } => panic!("the real product write failed: {error}"),
                PoolCompletion::Fsync {
                    result: Err(error), ..
                } => panic!("the real product fsync failed: {error}"),
            }
        }
        if write_seen && fsync_seen {
            return backend_completions;
        }
    }
    panic!("the real product write/fsync did not drain within the bounded poll budget");
}

#[test]
fn a_warm_read_submit_and_poll_drain_allocate_nothing() {
    let path = temp_frame("read");
    let drv = driver();
    let fd = open(&drv, &path);
    let mut out = CompletionBatch::with_capacity(4);

    drv.submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("warmup submit");
    drain_one(&drv, &mut out);

    let allocations = armed_allocations(|| {
        drv.submit_read(&fd, ReadFrameIdx::new(1), 0)
            .expect("armed submit within capacity");
        drain_one(&drv, &mut out);
    });

    assert_eq!(
        allocations, 0,
        "a warm read submit + poll-drain reuses the fixed slab/ready-queue/batch — \
         zero allocations after warmup (INV-2 / DIO-G4)"
    );
}

#[test]
fn a_warm_fsync_submit_and_poll_drain_allocate_nothing() {
    let path = temp_frame("fsync");
    let drv = driver();
    let fd = open(&drv, &path);
    let mut out = CompletionBatch::with_capacity(4);

    drv.submit_fsync(&fd, SyncMode::Full).expect("warmup fsync");
    drain_one(&drv, &mut out);

    let allocations = armed_allocations(|| {
        drv.submit_fsync(&fd, SyncMode::Full)
            .expect("armed fsync within capacity");
        drain_one(&drv, &mut out);
    });

    assert_eq!(
        allocations, 0,
        "the fsync completion path allocates nothing after warmup (INV-2 / DIO-G4)"
    );
}

#[test]
fn a_warm_write_slot_submit_and_poll_drain_allocate_nothing() {
    let path = temp_frame("write");
    let drv = driver();
    let fd = open(&drv, &path);
    let arena = drv.write_arena();
    let mut out = CompletionBatch::with_capacity(4);

    let mut warm = arena.alloc().expect("warmup staging slot");
    warm.fill(0x51);
    drv.submit_write(&fd, warm, 0).expect("warmup write");
    drain_one(&drv, &mut out);

    let allocations = armed_allocations(|| {
        let mut slot = arena.alloc().expect("armed staging slot");
        slot.fill(0xA3);
        drv.submit_write(&fd, slot, 0)
            .expect("armed write within capacity");
        drain_one(&drv, &mut out);
    });

    assert_eq!(
        allocations, 0,
        "write-slot acquire + submit + drain uses only init-time arena/slab storage"
    );
}

#[test]
fn an_idle_poll_allocates_nothing() {
    let drv = driver();
    let mut out = CompletionBatch::with_capacity(4);
    let _ = drv.poll(&mut out);

    let allocations = armed_allocations(|| {
        let drained = drv.poll(&mut out);
        assert_eq!(
            drained, 0,
            "no ops are in flight, so an idle poll drains nothing"
        );
    });

    assert_eq!(
        allocations, 0,
        "draining an empty ready queue touches no heap (INV-2 / DIO-G4)"
    );
}

#[test]
fn a_deferred_close_retiring_during_a_poll_drain_allocates_nothing() {
    let warm_path = temp_frame("close-warm");
    let drv = driver();
    let mut out = CompletionBatch::with_capacity(4);

    let warm_fd = open(&drv, &warm_path);
    drv.submit_read(&warm_fd, ReadFrameIdx::new(0), 0)
        .expect("warmup submit");
    drain_one(&drv, &mut out);
    drv.close(warm_fd);
    while drv.poll(&mut out) > 0 {}

    let path = temp_frame("close-drain");
    let fd = open(&drv, &path);
    let id = fd.file_id();
    drv.submit_read(&fd, ReadFrameIdx::new(1), 0)
        .expect("in-flight submit before close");
    drv.close(fd);
    assert!(
        !drv.is_closed(id),
        "close(2) is deferred while the read is still in flight"
    );

    let allocations = armed_allocations(|| {
        drain_one(&drv, &mut out);
        while drv.poll(&mut out) > 0 {}
    });

    assert!(
        drv.is_closed(id),
        "the fd retired once its in-flight read drained — the deferred close(2) fired inside the armed poll"
    );
    assert_eq!(
        allocations, 0,
        "a deferred retire completing mid-drain reuses the fixed retire scratch — \
         zero allocations (INV-2 / DIO-G4, batch-5 reap-path scratch)"
    );
}

#[test]
fn a_warmed_real_pool_write_fsync_and_bounded_report_drain_allocate_nothing() {
    let path = temp_frame("real-pool-product-write");
    let pool = product_pool();
    let file = pool
        .open(&path, DirectIo::Disabled)
        .expect("the real product pool opens the fixture");
    let arena: PoolWriteArena<'_> = pool.write_arena();
    let mut completions = PoolCompletionBatch::with_capacity(2);

    let mut warm_slot = arena.alloc().expect("one warmup product staging slot");
    warm_slot.fill(0x51);
    let warm_write = pool
        .submit_write(file, warm_slot, 0)
        .expect("the warmup product write admits");
    let warm_fsync = pool
        .submit_fsync(file, SyncMode::Full)
        .expect("the warmup product fsync admits");
    assert_eq!(
        drain_product_write_and_fsync(&pool, &mut completions, warm_write, warm_fsync),
        2
    );

    let mut measured_backend_completions = 0u32;
    let allocations = armed_allocations(|| {
        let mut slot = arena
            .alloc()
            .expect("the warmed PoolWriteArena reuses its staging slot");
        slot.fill(0xA3);
        let write = pool
            .submit_write(file, slot, 0)
            .expect("the measured product write admits");
        let fsync = pool
            .submit_fsync(file, SyncMode::Full)
            .expect("the measured product fsync admits");
        measured_backend_completions =
            drain_product_write_and_fsync(&pool, &mut completions, write, fsync);
    });

    assert_eq!(measured_backend_completions, 2);
    assert_eq!(
        allocations, 0,
        "the warmed shipping PoolWriteArena + submit_write + submit_fsync + bounded poll_report path allocates nothing"
    );
}

#[test]
fn real_pool_warm_get_hit_allocates_nothing() {
    let path = temp_frames("real-pool-warm-get-hit", 1, 0xC4);
    let pool = product_pool();
    let file = pool
        .open(&path, DirectIo::Disabled)
        .expect("the real product pool opens the fixture");
    let reader = pool
        .register_reader()
        .expect("the product pool has one reader slot");
    let page = PageId::new(file, 0);
    drop(resolve_product_page(&pool, &reader, page));

    let (allocations, outcome) = armed_allocations_result(|| pool.get(&reader, page));
    match outcome.expect("the product file remains live") {
        Get::Hit(guard) => assert_eq!(guard[0], 0xC4),
        Get::Pending(_) => panic!("a warmed product page must hit"),
        Get::Busy => panic!("a warmed product page is never Busy"),
    }

    assert_eq!(
        allocations, 0,
        "a warmed shipping-backend Pool::get hit reuses fixed residency state"
    );
}

#[test]
fn real_pool_warm_hinted_hit_allocates_nothing() {
    let path = temp_frames("real-pool-hinted-hit", 1, 0xA5);
    let pool = product_pool();
    let file = pool
        .open(&path, DirectIo::Disabled)
        .expect("the real product pool opens the fixture");
    let reader = pool
        .register_reader()
        .expect("the product pool has one reader slot");
    let page = PageId::new(file, 0);
    drop(resolve_product_page(&pool, &reader, page));
    let lease = pool
        .lease_file(file)
        .expect("the warmed product file admits a lease");
    let hint = pool
        .resident_hint(&lease, page)
        .expect("the warmed product page mints a hint");

    let (allocations, outcome) =
        armed_allocations_result(|| pool.get_with_hint(&reader, &lease, page, Some(hint)));
    match outcome.expect("the product lease remains live") {
        Get::Hit(guard) => assert_eq!(guard[0], 0xA5),
        Get::Pending(_) => panic!("a warmed hinted product page must hit"),
        Get::Busy => panic!("a warmed hinted product page is never Busy"),
    }

    assert_eq!(
        allocations, 0,
        "a shipping-backend hinted hit reuses preallocated residency metadata"
    );
}

#[test]
fn real_pool_stale_hint_ordinary_fallback_allocates_nothing() {
    let source_path = temp_frames("real-pool-stale-hint-source", 8, 0x51);
    let target_path = temp_frames("real-pool-stale-hint-target", 1, 0xA2);
    let pool = product_pool();
    let source_file = pool
        .open(&source_path, DirectIo::Disabled)
        .expect("the real product pool opens the source fixture");
    let target_file = pool
        .open(&target_path, DirectIo::Disabled)
        .expect("the real product pool opens the target fixture");
    let reader = pool
        .register_reader()
        .expect("the product pool has one reader slot");
    let source_page = PageId::new(source_file, 0);
    drop(resolve_product_page(&pool, &reader, source_page));
    let source_lease = pool
        .lease_file(source_file)
        .expect("the warmed source product file admits a lease");
    let stale_hint = pool
        .resident_hint(&source_lease, source_page)
        .expect("the warmed source product page mints a hint");
    for granule_idx in 1..=4 {
        drop(resolve_product_page(
            &pool,
            &reader,
            PageId::new(source_file, granule_idx),
        ));
    }
    assert!(
        pool.resident_hint(&source_lease, source_page).is_none(),
        "the old source observation is stale after bounded frame reuse"
    );
    let target_page = PageId::new(target_file, 0);
    drop(resolve_product_page(&pool, &reader, target_page));
    let target_lease = pool
        .lease_file(target_file)
        .expect("the warmed target product file admits a lease");

    let (allocations, outcome) = armed_allocations_result(|| {
        pool.get_with_hint(&reader, &target_lease, target_page, Some(stale_hint))
    });
    match outcome.expect("the stale hint falls back through the live target product file") {
        Get::Hit(guard) => assert_eq!(guard[0], 0xA2),
        Get::Pending(_) => panic!("the warmed product fallback target must hit"),
        Get::Busy => panic!("the warmed product fallback target is never Busy"),
    }

    assert_eq!(
        allocations, 0,
        "a shipping-backend stale hint fallback reuses the ordinary get allocation budget"
    );
}

#[test]
fn real_pool_lease_acquire_and_drop_allocate_nothing() {
    let path = temp_frame("real-pool-lease-acquire-drop");
    let pool = product_pool();
    let file = pool
        .open(&path, DirectIo::Disabled)
        .expect("the real product pool opens the fixture");

    let (allocations, lease_result) = armed_allocations_result(|| pool.lease_file(file).map(drop));
    lease_result.expect("a live product file acquires a resident lease");

    assert_eq!(
        allocations, 0,
        "shipping-backend resident lease acquisition and drop reuse preallocated state"
    );
}

#[test]
fn real_pool_last_lease_drop_and_retirement_progress_allocate_nothing() {
    let path = temp_frame("real-pool-last-lease-retirement");
    let pool = product_pool();
    let file = pool
        .open(&path, DirectIo::Disabled)
        .expect("the real product pool opens the fixture");
    let lease = pool
        .lease_file(file)
        .expect("the retiring product file has one pre-existing lease");
    assert_eq!(pool.retire_file(file), RetireStatus::Retiring);

    let allocations = armed_allocations(|| {
        drop(lease);
        for _ in 0..RETIRE_POLLS_MAX {
            pool.poll();
        }
    });

    assert!(
        <Pool<Driver> as PoolTestingExt>::file_is_retired_observed(&pool, file),
        "the armed bounded polls complete retirement before observation"
    );
    assert_eq!(
        allocations, 0,
        "shipping-backend last lease drop and retirement progress reuse preallocated state"
    );
}

#[test]
fn shipping_pool_retained_release_cycle_allocates_nothing() {
    let path = temp_frames("shipping-retained-release-drain", 2, 0x4B);
    let pool = product_retention_pool();
    let file = pool
        .open(&path, DirectIo::Disabled)
        .expect("the shipping pool opens the fixture");
    let reader = pool.register_reader().expect("one reader slot");

    let warmup_page = PageId::new(file, 0);
    let warmup_frame = pool.insert_resident_frame(warmup_page, 0xC4);
    let warmup_guard = resolve_product_page(&pool, &reader, warmup_page);
    shipping_pool_retained_release_cycle(&pool, warmup_page, warmup_frame, warmup_guard);

    let page = PageId::new(file, 1);
    let frame = pool.insert_resident_frame(page, 0xD5);
    let guard = resolve_product_page(&pool, &reader, page);
    let allocations = armed_allocations(|| {
        shipping_pool_retained_release_cycle(&pool, page, frame, guard);
    });

    assert_eq!(allocations, 0);
}

fn shipping_pool_retained_release_cycle(
    pool: &Pool<Driver>,
    page: PageId,
    frame: ReadFrameIdx,
    guard: FrameGuard<'_>,
) {
    let held_before = pool.retention_stats().retained_evictions_held;
    let Ok(retained) = guard.into_retained() else {
        panic!("the configured budget admits one retained frame");
    };
    assert_eq!(pool.evict_frame(page), frame);
    for _ in 0..4 {
        pool.poll();
    }
    assert_eq!(pool.frame_state(frame), dios::testing::FrameState::Evicting);
    assert_eq!(
        pool.retention_stats().retained_evictions_held,
        held_before + 1
    );

    drop(retained);
    assert_eq!(pool.retention_stats().occupied_budget, 1);
    pool.poll();
    assert_eq!(pool.frame_state(frame), dios::testing::FrameState::Free);
    assert_eq!(pool.retention_stats().occupied_budget, 0);
}

#[cfg(feature = "mock")]
mod pool_gates {
    use std::path::Path;

    use dios::testing::{
        FrameState, MockDriver, MockPoolTestingExt, PoolBuilderTestingExt, PoolTestingExt,
        ReadFrameIdx,
    };
    use dios::{
        DirectIo, FrameGuard, Get, PageId, Pool, PoolCompletionBatch, ReaderCtx, RetireStatus,
        SyncMode,
    };

    use super::{armed_allocations, armed_allocations_result};

    const GRANULE: u32 = 4096;
    const READY_POLLS_MAX: u32 = 64;

    fn a_mock(frames: u32) -> MockDriver {
        MockDriver::builder()
            .seed(0x9017_A110_C000)
            .queue_capacity(frames)
            .frames(frames)
            .frame_bytes(GRANULE)
            .write_slots(1)
            .retry_bound(0)
            .build()
    }

    fn pool_on(mock: MockDriver, frames: u32, peak: u32, headroom: u32) -> Pool<MockDriver> {
        pool_on_with_product_capacity(mock, frames, peak, headroom, 0, 0)
    }

    fn pool_on_with_product_capacity(
        mock: MockDriver,
        frames: u32,
        peak: u32,
        headroom: u32,
        write_slots: u32,
        max_inflight_product_ops: u32,
    ) -> Pool<MockDriver> {
        Pool::builder()
            .frame_count(frames)
            .granule(GRANULE)
            .max_concurrent_readers(1)
            .peak_guards_per_reader(peak)
            .max_inflight_reads(1)
            .miss_headroom(headroom)
            .write_slots(write_slots)
            .max_inflight_product_ops(max_inflight_product_ops)
            .build_on(mock)
            .expect("a watermark-satisfying pool composes over the mock driver")
    }

    fn resolve<'pool>(
        pool: &'pool Pool<MockDriver>,
        reader: &'pool ReaderCtx,
        page: PageId,
    ) -> FrameGuard<'pool> {
        let token = match pool.get(reader, page).expect("the registered file is live") {
            Get::Pending(token) => token,
            Get::Hit(guard) => return guard,
            Get::Busy => {
                panic!("a cold page within the watermark submits, it does not backpressure")
            }
        };
        let mut token = token;
        for _ in 0..READY_POLLS_MAX {
            match pool.ready(reader, token) {
                dios::ReadyResult::Ready(guard) => return guard,
                dios::ReadyResult::NotYet(handed_back) => {
                    token = handed_back;
                    pool.poll();
                }
                dios::ReadyResult::Err(err) => panic!("fault-free miss must not error: {err:?}"),
            }
        }
        panic!("miss never readied within the bounded poll budget");
    }

    fn drain_backend_and_retain_pair(
        pool: &Pool<MockDriver>,
        retain_all: &mut PoolCompletionBatch,
    ) -> u32 {
        let mut backend_completions = 0u32;
        for _ in 0..READY_POLLS_MAX {
            let report = pool.poll_report(retain_all);
            backend_completions += report.backend_completions();
            assert_eq!(report.reclaimed_frames(), 0);
            assert_eq!(retain_all.iter().count(), 0);
            if backend_completions == 2 {
                return backend_completions;
            }
        }
        panic!("the write and withheld fsync drain within the bounded poll budget");
    }

    #[test]
    fn a_warm_pool_hit_allocates_nothing() {
        let frames = 8u32;
        let mock = a_mock(frames);
        let file = mock
            .open(Path::new("za-warm-hit"), DirectIo::Disabled)
            .expect("mock open");
        let file_id = file.file_id();
        mock.seed_page(&file, 4, 0xC4);
        let pool = pool_on(mock, frames, 1, 3);
        pool.register_file(file);
        let reader = pool.register_reader().expect("a reader slot");
        let page = PageId::new(file_id, 4);

        drop(resolve(&pool, &reader, page));

        let allocations = armed_allocations(|| {
            match pool
                .get(&reader, page)
                .expect("the registered file is live")
            {
                Get::Hit(guard) => assert_eq!(guard.len(), GRANULE as usize),
                Get::Pending(_) => panic!("a resident page hits, it does not re-submit"),
                Get::Busy => panic!("a resident page is never Busy"),
            }
        });

        assert_eq!(
            allocations, 0,
            "a warm pool get() Hit — page-table probe + epoch pin — allocates nothing (INV-2 / DIO-G4)"
        );
    }

    #[test]
    fn a_warm_pool_hinted_hit_allocates_nothing() {
        let frames = 4u32;
        let mock = a_mock(frames);
        let file = mock
            .open(Path::new("za-hinted-hit"), DirectIo::Disabled)
            .expect("mock open");
        let file_id = file.file_id();
        let pool = pool_on(mock, frames, 1, 3);
        pool.register_file(file);
        let reader = pool.register_reader().expect("a reader slot");
        let page = PageId::new(file_id, 0);
        pool.insert_resident_frame(page, 0xA5);
        let lease = pool
            .lease_file(file_id)
            .expect("the live file admits a resident lease");
        let hint = pool
            .resident_hint(&lease, page)
            .expect("the resident page mints a hint");

        let (allocations, outcome) =
            armed_allocations_result(|| pool.get_with_hint(&reader, &lease, page, Some(hint)));
        match outcome.expect("the lease remains live") {
            Get::Hit(guard) => assert_eq!(guard[0], 0xA5),
            Get::Pending(_) => panic!("a resident hinted page must hit"),
            Get::Busy => panic!("a resident hinted page is never Busy"),
        }

        assert_eq!(
            allocations, 0,
            "a warmed hinted hit reuses preallocated residency metadata"
        );
    }

    #[test]
    fn a_stale_hint_ordinary_fallback_allocates_nothing() {
        let frames = 4u32;
        let mock = a_mock(frames);
        let source = mock
            .open(Path::new("za-stale-hint-source"), DirectIo::Disabled)
            .expect("source mock open");
        let source_file = source.file_id();
        let target = mock
            .open(Path::new("za-stale-hint-target"), DirectIo::Disabled)
            .expect("target mock open");
        let target_file = target.file_id();
        let pool = pool_on(mock, frames, 1, 3);
        pool.register_file(source);
        pool.register_file(target);
        let reader = pool.register_reader().expect("a reader slot");
        let source_page = PageId::new(source_file, 0);
        let target_page = PageId::new(target_file, 0);
        pool.insert_resident_frame(source_page, 0x51);
        pool.insert_resident_frame(target_page, 0xA2);
        let source_lease = pool
            .lease_file(source_file)
            .expect("the source file admits a resident lease");
        let target_lease = pool
            .lease_file(target_file)
            .expect("the target file admits a resident lease");
        let stale_hint = pool
            .resident_hint(&source_lease, source_page)
            .expect("the resident source page mints a hint");
        pool.evict_frame(source_page);
        pool.poll();
        pool.poll();

        let (allocations, outcome) = armed_allocations_result(|| {
            pool.get_with_hint(&reader, &target_lease, target_page, Some(stale_hint))
        });
        match outcome.expect("the stale hint falls back through the live target file") {
            Get::Hit(guard) => assert_eq!(guard[0], 0xA2),
            Get::Pending(_) => panic!("the resident fallback target must hit"),
            Get::Busy => panic!("the resident fallback target is never Busy"),
        }

        assert_eq!(
            allocations, 0,
            "a stale hinted lookup reuses the ordinary get allocation budget"
        );
    }

    #[test]
    fn lease_acquire_and_drop_allocate_nothing() {
        let frames = 4u32;
        let mock = a_mock(frames);
        let file = mock
            .open(Path::new("za-lease-retirement"), DirectIo::Disabled)
            .expect("mock file");
        let file_id = file.file_id();
        let pool = pool_on(mock, frames, 1, 3);
        pool.register_file(file);

        let (acquire_drop_allocations, lease_result) =
            armed_allocations_result(|| pool.lease_file(file_id).map(drop));
        lease_result.expect("a live file acquires a resident lease");
        assert_eq!(
            acquire_drop_allocations, 0,
            "resident lease acquisition and drop clone/decrement only preallocated state"
        );
    }

    #[test]
    fn last_lease_drop_and_retirement_progress_allocate_nothing() {
        let frames = 4u32;
        let mock = a_mock(frames);
        let file = mock
            .open(Path::new("za-last-lease-retirement"), DirectIo::Disabled)
            .expect("mock file");
        let file_id = file.file_id();
        let pool = pool_on(mock, frames, 1, 3);
        pool.register_file(file);
        let lease = pool
            .lease_file(file_id)
            .expect("the retiring file has one pre-existing lease");
        assert_eq!(pool.retire_file(file_id), RetireStatus::Retiring);
        let retirement_allocations = armed_allocations(|| {
            drop(lease);
            for _ in 0..READY_POLLS_MAX {
                pool.poll();
            }
        });
        assert!(
            pool.driver().is_closed(file_id),
            "last lease drop wakes bounded retirement progress"
        );
        assert_eq!(
            retirement_allocations, 0,
            "last lease drop and bounded retirement progress reuse preallocated control state"
        );
    }

    #[test]
    fn a_cold_pool_miss_submits_within_the_preallocated_budget_without_allocating() {
        let frames = 8u32;
        let mock = a_mock(frames);
        let file = mock
            .open(Path::new("za-miss-submit"), DirectIo::Disabled)
            .expect("mock open");
        let file_id = file.file_id();
        for idx in 0..frames {
            mock.seed_page(&file, idx, 0x40 | u8::try_from(idx).expect("fits u8"));
        }
        let pool = pool_on(mock, frames, 1, 3);
        pool.register_file(file);
        let reader = pool.register_reader().expect("a reader slot");

        drop(resolve(&pool, &reader, PageId::new(file_id, 0)));

        let cold = PageId::new(file_id, 1);
        let allocations = armed_allocations(|| {
            match pool
                .get(&reader, cold)
                .expect("the registered file is live")
            {
                Get::Pending(_) => {}
                Get::Hit(_) => panic!("an unfetched page cannot hit"),
                Get::Busy => panic!("a spare frame exists; the miss submits"),
            }
        });

        assert_eq!(
            allocations, 0,
            "a cold get() miss-submit reuses the fixed completion slab + singleflight table (INV-2 / DIO-G4)"
        );
    }

    #[test]
    fn a_pool_poll_drain_allocates_nothing() {
        let frames = 8u32;
        let mock = a_mock(frames);
        let file = mock
            .open(Path::new("za-poll-drain"), DirectIo::Disabled)
            .expect("mock open");
        let file_id = file.file_id();
        mock.seed_page(&file, 2, 0x22);
        mock.seed_page(&file, 3, 0x33);
        let pool = pool_on(mock, frames, 2, 3);
        pool.register_file(file);
        let reader = pool.register_reader().expect("a reader slot");

        drop(resolve(&pool, &reader, PageId::new(file_id, 2)));

        let cold = PageId::new(file_id, 3);
        let _token = match pool
            .get(&reader, cold)
            .expect("the registered file is live")
        {
            Get::Pending(token) => token,
            other => panic!("expected a fresh miss to submit, got {other:?}"),
        };
        let allocations = armed_allocations(|| {
            pool.poll();
        });

        assert_eq!(
            allocations, 0,
            "poll() draining a completion + advancing the epoch reuses fixed scratch (INV-2 / DIO-G4)"
        );
    }

    fn a_busy_backpressure_get_setup(
        frames: u32,
        peak: u32,
    ) -> (Pool<MockDriver>, dios::FileId, ReaderCtx) {
        let mock = a_mock(frames);
        let file = mock
            .open(Path::new("za-busy"), DirectIo::Disabled)
            .expect("mock open");
        let file_id = file.file_id();
        for idx in 0..frames {
            mock.seed_page(&file, idx, 0xF0 | u8::try_from(idx).expect("fits u8"));
        }
        let pool = pool_on(mock, frames, peak, 3);
        pool.register_file(file);
        let reader = pool.register_reader().expect("a reader slot");
        for idx in 0..frames {
            drop(resolve(&pool, &reader, PageId::new(file_id, idx)));
        }
        (pool, file_id, reader)
    }

    #[test]
    fn a_busy_backpressure_get_allocates_nothing_and_disturbs_no_pinned_frame() {
        let frames = 7u32;
        let peak = 4u32;
        let (pool, file_id, reader) = a_busy_backpressure_get_setup(frames, peak);

        let mut guards = Vec::with_capacity(peak as usize);
        for idx in 0..peak {
            guards.push(resolve(&pool, &reader, PageId::new(file_id, idx)));
        }
        let absent = PageId::new(file_id, frames + 1);
        matches!(
            pool.get(&reader, absent)
                .expect("the registered file is live"),
            Get::Busy
        )
        .then_some(())
        .expect("warmup: every spare frame pinned, an absent get backpressures Busy");

        let count_state = |state| {
            (0..frames)
                .filter(|&i| pool.frame_state(ReadFrameIdx::new(i)) == state)
                .count()
        };
        let free_before = count_state(FrameState::Free);
        let allocations = armed_allocations(|| {
            match pool
                .get(&reader, absent)
                .expect("the registered file is live")
            {
                Get::Busy => {}
                Get::Pending(_) => {
                    panic!("no evictable frame exists — a further miss must be Busy")
                }
                Get::Hit(_) => panic!("an unfetched page cannot hit"),
            }
        });

        assert_eq!(
            allocations, 0,
            "the Busy bounded-reclaim attempt (drain, advance, reclaim, one sweep) allocates nothing (INV-2 / DIO-G4)"
        );
        assert_eq!(
            (free_before, count_state(FrameState::Free)),
            (0, 0),
            "Busy freed no frame — no pinned frame was reclaimed under a live guard (INV-1)"
        );
        assert_eq!(
            count_state(FrameState::InFlight),
            0,
            "Busy submitted no read — no held frame was recycled to InFlight for the refused miss (INV-1)"
        );
        for (idx, guard) in guards.iter().enumerate() {
            let fill_byte = 0xF0 | u8::try_from(idx).expect("fits u8");
            assert_eq!(
                guard[0], fill_byte,
                "each live guard still reads its own page's bytes after the Busy attempt — content untouched"
            );
        }
        drop(guards);
    }

    #[test]
    fn a_repeat_warm_hit_performs_no_reference_bit_store() {
        let frames = 8u32;
        let mock = a_mock(frames);
        let file = mock
            .open(Path::new("za-store-elision"), DirectIo::Disabled)
            .expect("mock open");
        let file_id = file.file_id();
        mock.seed_page(&file, 6, 0x66);
        let pool = pool_on(mock, frames, 1, 3);
        pool.register_file(file);
        let reader = pool.register_reader().expect("a reader slot");
        let page = PageId::new(file_id, 6);

        let baseline = pool.clock_reference_stores();
        drop(resolve(&pool, &reader, page));

        drop(
            match pool
                .get(&reader, page)
                .expect("the registered file is live")
            {
                Get::Hit(guard) => guard,
                other => panic!("first warm get must hit: {other:?}"),
            },
        );
        let after_first_hit = pool.clock_reference_stores();

        drop(
            match pool
                .get(&reader, page)
                .expect("the registered file is live")
            {
                Get::Hit(guard) => guard,
                other => panic!("repeat warm get must hit: {other:?}"),
            },
        );
        let after_repeat_hit = pool.clock_reference_stores();

        assert_eq!(
            after_first_hit - baseline,
            1,
            "exactly one clear->set reference store occurs across warmup + first hit — an \
             always-elide/no-op (FIFO-degraded) CLOCK records zero here (design.md:164)"
        );
        assert_eq!(
            after_repeat_hit - after_first_hit,
            0,
            "a repeat hit on an already-referenced frame stores nothing — the DIO-G1 hot-path invariant \
             (an always-store impl, which T006's return-value test admits, records one here)"
        );
    }

    #[test]
    fn warm_pool_write_fsync_and_overflow_drain_allocate_nothing() {
        let frames = 4u32;
        let mock = a_mock(frames);
        let file = mock
            .open(Path::new("za-pool-write"), DirectIo::Disabled)
            .expect("mock open");
        let file_id = file.file_id();
        let pool = pool_on_with_product_capacity(mock, frames, 1, 3, 1, 2);
        pool.register_file(file);
        let mut retain_all = PoolCompletionBatch::with_capacity(0);
        let mut completions = PoolCompletionBatch::with_capacity(1);

        let warm_slot = pool.write_arena().alloc().expect("one staging slot");
        pool.submit_write(file_id, warm_slot, 0)
            .expect("warmup write admits");
        pool.submit_fsync(file_id, SyncMode::Full)
            .expect("warmup barrier admits");
        assert_eq!(drain_backend_and_retain_pair(&pool, &mut retain_all), 2);
        for _ in 0..2 {
            let report = pool.poll_report(&mut completions);
            assert_eq!(report.backend_completions(), 0);
            assert_eq!(report.reclaimed_frames(), 0);
            assert_eq!(completions.iter().count(), 1);
        }

        let mut backend_completions = 0u32;
        let mut first_backend_completions = 0u32;
        let mut second_backend_completions = 0u32;
        let mut first_delivered = 0usize;
        let mut second_delivered = 0usize;
        let allocations = armed_allocations(|| {
            let slot = pool
                .write_arena()
                .alloc()
                .expect("warm staging reuses storage");
            pool.submit_write(file_id, slot, GRANULE.into())
                .expect("armed write admits");
            pool.submit_fsync(file_id, SyncMode::Full)
                .expect("armed barrier admits");

            backend_completions = drain_backend_and_retain_pair(&pool, &mut retain_all);

            let report = pool.poll_report(&mut completions);
            first_backend_completions = report.backend_completions();
            assert_eq!(report.reclaimed_frames(), 0);
            first_delivered = completions.iter().count();

            let report = pool.poll_report(&mut completions);
            second_backend_completions = report.backend_completions();
            assert_eq!(report.reclaimed_frames(), 0);
            second_delivered = completions.iter().count();
        });

        assert_eq!(backend_completions, 2);
        assert_eq!(first_backend_completions, 0);
        assert_eq!(first_delivered, 1, "caller capacity limits delivery only");
        assert_eq!(second_backend_completions, 0);
        assert_eq!(second_delivered, 1, "the overflow completion is retained");
        assert_eq!(
            allocations, 0,
            "the warmed product write path is fixed-capacity"
        );
    }

    #[test]
    fn retained_promote_final_drop_and_release_drain_allocate_nothing() {
        let frames = 5u32;
        let mock = a_mock(frames);
        let file = mock
            .open(Path::new("za-retained-release-drain"), DirectIo::Disabled)
            .expect("mock file opens");
        let file_id = file.file_id();
        let pool = Pool::builder()
            .frame_count(frames)
            .granule(GRANULE)
            .max_concurrent_readers(1)
            .peak_guards_per_reader(1)
            .max_inflight_reads(1)
            .miss_headroom(3)
            .max_retained_frames(1)
            .build_on(mock)
            .expect("retention fixture satisfies its watermark");
        pool.register_file(file);
        let reader = pool.register_reader().expect("one reader slot");

        let warmup_page = PageId::new(file_id, 0);
        let warmup_frame = pool.insert_resident_frame(warmup_page, 0xC4);
        let warmup_guard = resolve(&pool, &reader, warmup_page);
        retained_promote_final_drop_and_release_drain_allocate_nothing_cycle(
            &pool,
            warmup_page,
            warmup_frame,
            warmup_guard,
        );

        let page = PageId::new(file_id, 1);
        let frame = pool.insert_resident_frame(page, 0xD5);
        let guard = resolve(&pool, &reader, page);
        let allocations = armed_allocations(|| {
            retained_promote_final_drop_and_release_drain_allocate_nothing_cycle(
                &pool, page, frame, guard,
            );
        });

        assert_eq!(allocations, 0);
    }

    fn retained_promote_final_drop_and_release_drain_allocate_nothing_cycle(
        pool: &Pool<MockDriver>,
        page: PageId,
        frame: ReadFrameIdx,
        guard: FrameGuard<'_>,
    ) {
        let held_before = pool.retention_stats().retained_evictions_held;
        let Ok(retained) = guard.into_retained() else {
            panic!("the configured budget admits one retained frame");
        };
        assert_eq!(pool.evict_frame(page), frame);
        for _ in 0..4 {
            pool.poll();
        }
        assert_eq!(pool.frame_state(frame), FrameState::Evicting);
        let held = pool.retention_stats();
        assert_eq!(held.occupied_budget, 1);
        assert_eq!(held.retained_evictions_held, held_before + 1);

        drop(retained);
        assert_eq!(pool.retention_stats().occupied_budget, 1);
        pool.poll();
        assert_eq!(pool.frame_state(frame), FrameState::Free);
        assert_eq!(pool.retention_stats().occupied_budget, 0);
    }
}
