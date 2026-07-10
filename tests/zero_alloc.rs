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
//! backends" across the two hosts. The pool `get()` zero-alloc gates and the
//! close-during-drain case are T009's, layered onto this same file.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use dios::{CompletionBatch, Driver, FileHandle, OpenHow, ReadFrameIdx, SyncMode};

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
        .retry_bound(3)
        .build()
}

fn open(drv: &Driver, path: &Path) -> FileHandle {
    drv.open(path, OpenHow::read_write())
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
