use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ptr;

#[cfg(feature = "mock")]
use dios::testing::{MockDriver, PoolBuilderTestingExt};
use dios::{Pool, PoolBuildError};

const GRANULE: u32 = 4096;
const FRAME_COUNT: u32 = 4096;
const FRAME_STATE_BYTES: usize = 8;
const FAILURE_BYTES_MIN: usize = FRAME_COUNT as usize * FRAME_STATE_BYTES;
const METADATA_FRAME_COUNT: u32 = 127;
const REGISTERED_FILES: u32 = 61;
const FILE_GENERATION_BYTES: usize = REGISTERED_FILES as usize * size_of::<u64>();
const FILE_GENERATION_ALIGN: usize = align_of::<u64>();

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
        target.set((FILE_GENERATION_BYTES, FILE_GENERATION_ALIGN));
    });
    let result = body();
    let remaining = FAIL_LAYOUT_ONCE.with(|target| target.replace((0, 0)));
    assert_eq!(
        remaining,
        (0, 0),
        "file generation table layout must be observed"
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
fn file_table_allocation_failure_is_typed() {
    let result = fail_exact_layout_once(|| {
        Pool::builder()
            .frame_count(METADATA_FRAME_COUNT)
            .granule(GRANULE)
            .max_concurrent_readers(1)
            .peak_guards_per_reader(1)
            .max_inflight_reads(1)
            .miss_headroom(3)
            .registered_file_capacity(REGISTERED_FILES)
            .build()
    });

    assert!(matches!(result, Err(PoolBuildError::Allocation)));
}

#[cfg(any(target_os = "linux", feature = "mock"))]
fn translation_pool(
    registered_file_capacity: u32,
    file_granules_max: u32,
) -> Result<Pool, PoolBuildError> {
    Pool::builder()
        .frame_count(64)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .registered_file_capacity(registered_file_capacity)
        .file_granules_max(file_granules_max)
        .build()
}

#[cfg(feature = "mock")]
fn translation_pool_on_mock(
    registered_file_capacity: u32,
    file_granules_max: u32,
) -> Result<Pool<MockDriver>, PoolBuildError> {
    let mock = MockDriver::builder()
        .queue_capacity(4)
        .frames(64)
        .frame_bytes(GRANULE)
        .build();
    Pool::builder()
        .frame_count(64)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .registered_file_capacity(registered_file_capacity)
        .file_granules_max(file_granules_max)
        .build_on(mock)
}

#[cfg(target_os = "linux")]
fn meminfo_kib(field: &str) -> u64 {
    let meminfo = std::fs::read_to_string("/proc/meminfo").expect("procfs meminfo is readable");
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or_else(|| panic!("meminfo carries {field}"))
}

#[cfg(any(target_os = "linux", feature = "mock"))]
const TRANSLATION_ENTRY_BYTES: u64 = 4;
#[cfg(any(target_os = "linux", feature = "mock"))]
const TRANSLATION_FILE_GRANULES_MAX: u32 = 1 << 31;

#[cfg(target_os = "linux")]
#[test]
fn translation_reservation_above_ram_plus_swap_builds() {
    let overcommit = std::fs::read_to_string("/proc/sys/vm/overcommit_memory")
        .expect("overcommit policy is readable");
    assert_eq!(
        overcommit.trim(),
        "0",
        "the case needs heuristic overcommit"
    );
    let commit_bytes = (meminfo_kib("MemTotal:") + meminfo_kib("SwapTotal:")) * 1024;
    let slots = commit_bytes
        .div_ceil(u64::from(TRANSLATION_FILE_GRANULES_MAX) * TRANSLATION_ENTRY_BYTES)
        .next_power_of_two()
        * 2;
    let slots = u32::try_from(slots).expect("the host's RAM plus swap needs few 8 GiB slots");
    let reservation = translation_pool(slots, TRANSLATION_FILE_GRANULES_MAX);
    assert!(
        reservation.is_ok(),
        "{slots} slots of 8 GiB exceed {commit_bytes} bytes of RAM plus swap and still build: {:?}",
        reservation.err()
    );
}

#[cfg(feature = "mock")]
#[test]
fn translation_reservation_is_refused_only_beyond_the_address_space() {
    let beyond_address_space = 1u32 << 16;
    assert!(
        u64::from(beyond_address_space)
            * u64::from(TRANSLATION_FILE_GRANULES_MAX)
            * TRANSLATION_ENTRY_BYTES
            > 1 << 48,
        "the reservation exceeds a 48-bit address space"
    );
    let refused = translation_pool_on_mock(beyond_address_space, TRANSLATION_FILE_GRANULES_MAX);
    assert!(
        matches!(refused, Err(PoolBuildError::Allocation)),
        "a reservation the host refuses is a typed build error: {:?}",
        refused.err()
    );
    assert!(
        translation_pool(4, 16).is_ok(),
        "a refused reservation leaves the process able to build the next pool"
    );
}
