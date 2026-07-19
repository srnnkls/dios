//! T005 alloc-count harness (INV-2 / DIO-G4), driver-scoped.
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
//! alloc-free. Pool gates are `feature = "mock"` because `Pool<MockDriver>` is
//! the only working read path until the T014 arena unification wires
//! `Pool<Driver>`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use dios::driver::{CompletionBatch, Driver, FileHandle, SyncMode};
use dios::testing::{DriverReadTestingExt, ReadFrameIdx};
use dios::DirectIo;

const FRAME_BYTES: u32 = 4096;
const DRAIN_POLLS_MAX: u32 = 1_000_000;

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

static UNIQUE: AtomicU32 = AtomicU32::new(0);

fn temp_frame(tag: &str) -> PathBuf {
    let n = UNIQUE.fetch_add(1, Ordering::Relaxed);
    let mut path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&path).expect("target tmp dir");
    path.push(format!("zeroalloc-{tag}-{}-{n}", std::process::id()));
    std::fs::write(&path, vec![0x4Bu8; FRAME_BYTES as usize]).expect("seed a full frame");
    path
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

fn open(drv: &Driver, path: &Path) -> FileHandle {
    drv.open(path, DirectIo::Disabled)
        .expect("open the seeded file")
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

#[cfg(feature = "mock")]
mod pool_gates {
    use std::path::Path;

    use dios::testing::{
        FrameState, MockDriver, PoolBuilderTestingExt, PoolTestingExt, ReadFrameIdx,
    };
    use dios::{DirectIo, FrameGuard, Get, PageId, Pool, ReaderCtx};

    use super::armed_allocations;

    const GRANULE: u32 = 4096;
    const READY_POLLS_MAX: u32 = 64;

    fn a_mock(frames: u32) -> MockDriver {
        MockDriver::builder()
            .seed(0x9017_A110_C000)
            .queue_capacity(frames)
            .frames(frames)
            .frame_bytes(GRANULE)
            .retry_bound(0)
            .build()
    }

    fn pool_on(mock: MockDriver, frames: u32, peak: u32, headroom: u32) -> Pool<MockDriver> {
        Pool::builder()
            .frame_count(frames)
            .granule(GRANULE)
            .max_concurrent_readers(1)
            .peak_guards_per_reader(peak)
            .max_inflight_reads(1)
            .miss_headroom(headroom)
            .build_on(mock)
            .expect("a watermark-satisfying pool composes over the mock driver")
    }

    fn resolve<'pool>(
        pool: &'pool Pool<MockDriver>,
        reader: &'pool ReaderCtx<'pool>,
        page: PageId,
    ) -> FrameGuard<'pool> {
        let token = match pool.get(reader, page) {
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

        let allocations = armed_allocations(|| match pool.get(&reader, page) {
            Get::Hit(guard) => assert_eq!(guard.len(), GRANULE as usize),
            Get::Pending(_) => panic!("a resident page hits, it does not re-submit"),
            Get::Busy => panic!("a resident page is never Busy"),
        });

        assert_eq!(
            allocations, 0,
            "a warm pool get() Hit — page-table probe + epoch pin — allocates nothing (INV-2 / DIO-G4)"
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
        let allocations = armed_allocations(|| match pool.get(&reader, cold) {
            Get::Pending(_) => {}
            Get::Hit(_) => panic!("an unfetched page cannot hit"),
            Get::Busy => panic!("a spare frame exists; the miss submits"),
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
        let _token = match pool.get(&reader, cold) {
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

    #[test]
    fn a_busy_backpressure_get_allocates_nothing_and_disturbs_no_pinned_frame() {
        let frames = 4u32;
        let mock = a_mock(frames);
        let file = mock
            .open(Path::new("za-busy"), DirectIo::Disabled)
            .expect("mock open");
        let file_id = file.file_id();
        for idx in 0..frames {
            mock.seed_page(&file, idx, 0xF0 | u8::try_from(idx).expect("fits u8"));
        }
        let pool = pool_on(mock, frames, 1, 3);
        pool.register_file(file);
        let reader = pool.register_reader().expect("a reader slot");

        let mut guards = Vec::with_capacity(frames as usize);
        for idx in 0..frames {
            guards.push(resolve(&pool, &reader, PageId::new(file_id, idx)));
        }
        let absent = PageId::new(file_id, frames + 1);
        matches!(pool.get(&reader, absent), Get::Busy)
            .then_some(())
            .expect("warmup: every spare frame pinned, an absent get backpressures Busy");

        let count_state = |state| {
            (0..frames)
                .filter(|&i| pool.frame_state(ReadFrameIdx::new(i)) == state)
                .count()
        };
        let free_before = count_state(FrameState::Free);
        let allocations = armed_allocations(|| match pool.get(&reader, absent) {
            Get::Busy => {}
            Get::Pending(_) => panic!("no evictable frame exists — a further miss must be Busy"),
            Get::Hit(_) => panic!("an unfetched page cannot hit"),
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

        drop(match pool.get(&reader, page) {
            Get::Hit(guard) => guard,
            other => panic!("first warm get must hit: {other:?}"),
        });
        let after_first_hit = pool.clock_reference_stores();

        drop(match pool.get(&reader, page) {
            Get::Hit(guard) => guard,
            other => panic!("repeat warm get must hit: {other:?}"),
        });
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
}
