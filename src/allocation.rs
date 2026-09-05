use std::alloc::{Layout, alloc_zeroed};
use std::collections::VecDeque;
use std::ffi::{c_int, c_void};
use std::ptr::NonNull;

/// Allocates one non-empty fixed arena without invoking the process OOM handler.
pub(crate) fn allocate_zeroed(layout: Layout) -> Option<NonNull<u8>> {
    assert!(layout.size() > 0, "fixed arena layouts are non-empty");
    // SAFETY: `layout` is valid and non-empty. A null return becomes `None`
    // before the pointer is used.
    NonNull::new(unsafe { alloc_zeroed(layout) })
}

/// Reserves one fixed vector capacity without invoking the process OOM handler.
pub(crate) fn try_vec_with_exact_capacity<T>(capacity: u32) -> Option<Vec<T>> {
    let mut values = Vec::new();
    values.try_reserve_exact(capacity as usize).ok()?;
    Some(values)
}

/// Builds one boxed fixed-size slice after exactly reserving its outer storage.
pub(crate) fn try_boxed_slice_with<T>(
    capacity: u32,
    mut init: impl FnMut() -> T,
) -> Option<Box<[T]>> {
    let mut values = try_vec_with_exact_capacity(capacity)?;
    for _ in 0..capacity {
        values.push(init());
    }
    Some(values.into_boxed_slice())
}

/// Reserves one fixed deque capacity without invoking the process OOM handler.
pub(crate) fn try_vec_deque_with_exact_capacity<T>(capacity: u32) -> Option<VecDeque<T>> {
    let mut values = VecDeque::new();
    values.try_reserve_exact(capacity as usize).ok()?;
    Some(values)
}

// mmap(2)/munmap(2) declared against the C ABI of the supported targets, so the
// crate needs no `libc` dependency. The flag values come from each target's
// `sys/mman.h`.
unsafe extern "C" {
    fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
    fn sysconf(name: c_int) -> i64;
}

#[cfg(target_os = "linux")]
const SC_PAGESIZE: c_int = 30;
#[cfg(target_os = "macos")]
const SC_PAGESIZE: c_int = 29;

fn page_size() -> usize {
    // SAFETY: `sysconf` reads one process-wide constant and touches no memory.
    let size = unsafe { sysconf(SC_PAGESIZE) };
    usize::try_from(size).expect("the page size is a positive constant")
}

const PROT_READ: c_int = 1;
const PROT_WRITE: c_int = 2;
const MAP_PRIVATE: c_int = 2;
#[cfg(target_os = "linux")]
const MAP_ANONYMOUS: c_int = 0x20;
#[cfg(target_os = "macos")]
const MAP_ANONYMOUS: c_int = 0x1000;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("MAP_ANONYMOUS is target-specific; add this target's value from sys/mman.h");

/// One anonymous private mapping whose pages the kernel materialises on first
/// touch, so mapping a span costs no more than its page tables until a read
/// lands in it. Freed by `munmap` on drop.
#[derive(Debug)]
pub(crate) struct MappedArena {
    base: NonNull<u8>,
    len: usize,
    mapped_len: usize,
}

impl MappedArena {
    /// Maps `len` bytes at an address that is a multiple of `align`, trimming
    /// the over-mapped head and tail so only the aligned span (rounded up to
    /// whole pages) stays mapped.
    pub(crate) fn try_map(len: usize, align: usize) -> Option<Self> {
        assert!(len > 0, "fixed arena mappings are non-empty");
        assert!(align.is_power_of_two(), "arena alignment is a power of two");
        let page = page_size();
        let mapped_len = len.checked_next_multiple_of(page)?;
        let slack = if align > page { align } else { 0 };
        let total = mapped_len.checked_add(slack)?;
        // SAFETY: an anonymous private mapping takes no file and no address hint;
        // the kernel picks a free range or returns `MAP_FAILED`.
        let raw = unsafe {
            mmap(
                std::ptr::null_mut(),
                total,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if raw == MAP_FAILED {
            return None;
        }
        let raw_addr = raw.addr();
        let aligned = raw_addr.next_multiple_of(align);
        let head = aligned - raw_addr;
        let tail = total - head - mapped_len;
        if head > 0 {
            // SAFETY: `[raw, raw + head)` is the untouched prefix of the mapping
            // just created; unmapping it leaves the aligned span mapped.
            let rc = unsafe { munmap(raw, head) };
            assert_eq!(rc, 0, "trimming the head of a fresh mapping cannot fail");
        }
        if tail > 0 {
            let tail_start = raw.cast::<u8>().with_addr(aligned + mapped_len).cast();
            // SAFETY: `[aligned + mapped_len, raw + total)` is the untouched suffix of
            // the mapping just created; unmapping it leaves the span mapped.
            let rc = unsafe { munmap(tail_start, tail) };
            assert_eq!(rc, 0, "trimming the tail of a fresh mapping cannot fail");
        }
        let base = NonNull::new(raw.cast::<u8>().with_addr(aligned))?;
        Some(Self {
            base,
            len,
            mapped_len,
        })
    }

    pub(crate) fn base(&self) -> NonNull<u8> {
        self.base
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

impl Drop for MappedArena {
    fn drop(&mut self) {
        // SAFETY: `base..base + mapped_len` is exactly the span `try_map` left
        // mapped, unmapped once here at end of life.
        let rc = unsafe { munmap(self.base.as_ptr().cast(), self.mapped_len) };
        assert_eq!(rc, 0, "unmapping a span this arena mapped cannot fail");
    }
}

const MAP_FAILED: *mut c_void = usize::MAX as *mut c_void;

#[cfg(test)]
mod mapped_arena_tests {
    use super::*;

    #[test]
    fn a_mapping_honors_an_alignment_above_the_page_size() {
        let align = 2 * 1024 * 1024;
        let arena = MappedArena::try_map(3 * 4096, align).expect("a small mapping succeeds");
        assert_eq!(arena.base().as_ptr().addr() % align, 0);
        assert_eq!(arena.len(), 3 * 4096);
    }

    #[test]
    fn a_mapping_below_the_page_size_still_maps_whole_pages() {
        let arena = MappedArena::try_map(512, 512).expect("a small mapping succeeds");
        assert_eq!(arena.len(), 512);
        assert_eq!(arena.mapped_len, page_size());
    }

    #[test]
    fn a_span_beyond_the_address_space_is_refused_not_fatal() {
        assert!(MappedArena::try_map(usize::MAX / 2, 4096).is_none());
    }

    #[test]
    fn a_mapping_is_zeroed_and_writable() {
        let arena = MappedArena::try_map(8192, 4096).expect("a small mapping succeeds");
        // SAFETY: the span is a live private mapping owned solely by `arena`.
        let bytes = unsafe { std::slice::from_raw_parts_mut(arena.base().as_ptr(), 8192) };
        assert!(bytes.iter().all(|&b| b == 0));
        bytes[8191] = 0xA5;
        assert_eq!(bytes[8191], 0xA5);
    }
}
