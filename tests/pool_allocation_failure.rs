use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ptr;

use dios::{PageId, Pool, PoolBuildError};

const GRANULE: u32 = 4096;
const FRAME_COUNT: u32 = 4096;
const FRAME_STATE_BYTES: usize = 8;
const FAILURE_BYTES_MIN: usize = FRAME_COUNT as usize * FRAME_STATE_BYTES;
const METADATA_FRAME_COUNT: u32 = 127;
const FRAME_PAGE_INDEX_BYTES: usize = METADATA_FRAME_COUNT as usize * size_of::<Option<PageId>>();
const FRAME_PAGE_INDEX_ALIGN: usize = align_of::<Option<PageId>>();

thread_local! {
    static FAIL_BYTES_MIN: Cell<usize> = const { Cell::new(0) };
    static FAIL_LAYOUT_ONCE: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}

fn allocation_should_fail(bytes: usize, align: usize) -> bool {
    let exceeds_minimum = FAIL_BYTES_MIN
        .try_with(|minimum| minimum.get() != 0 && bytes >= minimum.get())
        .unwrap_or(false);
    let matches_layout = FAIL_LAYOUT_ONCE
        .try_with(|target| {
            if target.get() == (bytes, align) {
                target.set((0, 0));
                true
            } else {
                false
            }
        })
        .unwrap_or(false);
    exceeds_minimum || matches_layout
}

struct FailingAllocator;

// SAFETY: successful allocation, reallocation, and deallocation forward the
// original pointer and layout to System. The armed branch returns null, as the
// GlobalAlloc contract permits, without touching the allocation.
unsafe impl GlobalAlloc for FailingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if allocation_should_fail(layout.size(), layout.align()) {
            ptr::null_mut()
        } else {
            // SAFETY: `layout` is forwarded unchanged to the system allocator.
            unsafe { System.alloc(layout) }
        }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if allocation_should_fail(new_size, layout.align()) {
            ptr::null_mut()
        } else {
            // SAFETY: `pointer`, `layout`, and `new_size` are forwarded unchanged.
            unsafe { System.realloc(pointer, layout, new_size) }
        }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: `pointer` and `layout` are forwarded unchanged.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static GLOBAL: FailingAllocator = FailingAllocator;

fn fail_explicit_capacity_allocation<T>(body: impl FnOnce() -> T) -> T {
    FAIL_BYTES_MIN.with(|minimum| minimum.set(FAILURE_BYTES_MIN));
    let result = body();
    FAIL_BYTES_MIN.with(|minimum| minimum.set(0));
    result
}

fn fail_exact_layout_once<T>(body: impl FnOnce() -> T) -> T {
    FAIL_LAYOUT_ONCE.with(|target| {
        target.set((FRAME_PAGE_INDEX_BYTES, FRAME_PAGE_INDEX_ALIGN));
    });
    let result = body();
    let remaining = FAIL_LAYOUT_ONCE.with(|target| target.replace((0, 0)));
    assert_eq!(
        remaining,
        (0, 0),
        "frame page index layout must be observed"
    );
    result
}

#[test]
fn explicitly_sized_pool_allocation_failure_is_typed() {
    let result = fail_explicit_capacity_allocation(|| {
        Pool::builder()
            .frame_count(FRAME_COUNT)
            .granule(GRANULE)
            .max_concurrent_readers(1)
            .peak_guards_per_reader(1)
            .max_inflight_reads(1)
            .miss_headroom(3)
            .build()
    });

    assert!(matches!(result, Err(PoolBuildError::Allocation)));
}

#[test]
fn frame_metadata_allocation_failure_is_typed() {
    let result = fail_exact_layout_once(|| {
        Pool::builder()
            .frame_count(METADATA_FRAME_COUNT)
            .granule(GRANULE)
            .max_concurrent_readers(1)
            .peak_guards_per_reader(1)
            .max_inflight_reads(1)
            .miss_headroom(3)
            .build()
    });

    assert!(matches!(result, Err(PoolBuildError::Allocation)));
}
