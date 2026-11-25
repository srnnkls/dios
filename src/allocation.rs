use std::alloc::{Layout, alloc_zeroed};
use std::collections::VecDeque;
use std::ffi::{c_int, c_void};
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

/// Hands out the process-wide identity a fixed arena carries, so a handle
/// minted against one arena is rejected by another. Bumped once per arena
/// construction — never on the hot path.
static NEXT_ARENA_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

pub(crate) fn next_arena_id() -> u32 {
    NEXT_ARENA_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

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
    #[cfg(not(any(loom, miri)))]
    fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
    #[cfg(any(feature = "mock", feature = "bench"))]
    fn mincore(addr: *mut c_void, len: usize, residency: *mut u8) -> c_int;
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
#[cfg(all(target_os = "linux", not(miri)))]
const MAP_NORESERVE: c_int = 0x4000;
#[cfg(all(target_os = "macos", not(miri)))]
const MAP_NORESERVE: c_int = 0x40;
#[cfg(not(any(loom, miri)))]
const MADV_DONTNEED: c_int = 4;

/// Whether a mapping is charged against the commit limit when it is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitAccounting {
    Charged,
    Reserved,
}

/// One anonymous private mapping whose pages the kernel materialises on first
/// touch, so mapping a span costs no more than its page tables until a read
/// lands in it. The mapping is never trimmed: an alignment above the page
/// size leaves untouched slack around the aligned span, and the whole range
/// goes back with one `munmap` on drop, so no cut ever asks the kernel to
/// split a VMA.
#[derive(Debug)]
pub(crate) struct MappedArena {
    base: NonNull<u8>,
    len: usize,
    raw: *mut c_void,
    total: usize,
}

impl MappedArena {
    /// Maps `len` bytes (rounded up to whole pages) at an address that is a
    /// multiple of `align`.
    pub(crate) fn try_map(len: usize, align: usize) -> Option<Self> {
        Self::try_map_accounted(len, align, CommitAccounting::Charged)
    }

    fn try_map_accounted(len: usize, align: usize, accounting: CommitAccounting) -> Option<Self> {
        assert!(len > 0, "fixed arena mappings are non-empty");
        assert!(align.is_power_of_two(), "arena alignment is a power of two");
        let page = page_size();
        let mapped_len = len.checked_next_multiple_of(page)?;
        let slack = if align > page { align } else { 0 };
        let total = mapped_len.checked_add(slack)?;
        let flags = match accounting {
            CommitAccounting::Charged => MAP_PRIVATE | MAP_ANONYMOUS,
            #[cfg(not(miri))]
            CommitAccounting::Reserved => MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE,
            // Miri's mmap shim accepts only private anonymous flags and keeps no
            // commit limit to opt out of.
            #[cfg(miri)]
            CommitAccounting::Reserved => MAP_PRIVATE | MAP_ANONYMOUS,
        };
        // SAFETY: an anonymous private mapping takes no file and no address hint;
        // the kernel picks a free range or returns `MAP_FAILED`.
        let raw = unsafe {
            mmap(
                std::ptr::null_mut(),
                total,
                PROT_READ | PROT_WRITE,
                flags,
                -1,
                0,
            )
        };
        if raw == MAP_FAILED {
            return None;
        }
        let aligned = raw.addr().next_multiple_of(align);
        debug_assert!(aligned + mapped_len <= raw.addr() + total);
        let base = NonNull::new(raw.cast::<u8>().with_addr(aligned))?;
        Some(Self {
            base,
            len,
            raw,
            total,
        })
    }

    pub(crate) fn base(&self) -> NonNull<u8> {
        self.base
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    #[cfg(any(test, feature = "bench"))]
    pub(crate) fn mapped_len(&self) -> usize {
        self.total
    }
}

impl Drop for MappedArena {
    fn drop(&mut self) {
        // SAFETY: `raw..raw + total` is exactly the mapping `try_map` created,
        // unmapped once here at end of life; the result is ignored because a
        // refusal leaves nothing further this value could do with the span.
        let _ = unsafe { munmap(self.raw, self.total) };
    }
}

const MAP_FAILED: *mut c_void = usize::MAX as *mut c_void;

/// Element types whose vacant value is the all-zero bit pattern, so a table of
/// them can live in zero pages the kernel materialises on first touch.
///
/// # Safety
///
/// In the shipping build every all-zero `Self` must be a valid, initialised
/// value. Under `cfg(loom)` the atomics carry model state, so that build
/// writes `vacant()` into every element instead.
pub(crate) unsafe trait ZeroVacant: Sized {
    #[cfg(loom)]
    fn vacant() -> Self;
}

macro_rules! zero_vacant_atomics {
    ($($atomic:ty => $zero:expr),* $(,)?) => {
        $(
            // SAFETY: the shipping atomic is a transparent wrapper over its
            // integer, whose zero is the vacant value named here.
            unsafe impl ZeroVacant for $atomic {
                #[cfg(loom)]
                fn vacant() -> Self {
                    <$atomic>::new($zero)
                }
            }
        )*
    };
}

zero_vacant_atomics! {
    crate::sync::AtomicBool => false,
    crate::sync::AtomicU32 => 0,
    crate::sync::AtomicU64 => 0,
}

#[cfg(loom)]
zero_vacant_atomics! {
    std::sync::atomic::AtomicBool => false,
    std::sync::atomic::AtomicU32 => 0,
    std::sync::atomic::AtomicU64 => 0,
}

// SAFETY: the shipping cell is a transparent wrapper over
// `std::cell::UnsafeCell`, and a `MaybeUninit` admits every bit pattern.
unsafe impl<T> ZeroVacant for crate::sync::UnsafeCell<MaybeUninit<T>> {
    #[cfg(loom)]
    fn vacant() -> Self {
        Self::new(MaybeUninit::uninit())
    }
}

/// One optionally occupied `Copy` element whose all-zero bytes are the vacant
/// state, so a table of them lives in an untouched mapping. `Option<T>` has no
/// guaranteed vacant bit pattern for an arbitrary payload; this cell carries
/// its own flag ahead of the payload.
#[repr(C)]
pub(crate) struct Occupiable<T: Copy> {
    occupied: bool,
    value: MaybeUninit<T>,
}

// SAFETY: `occupied == false` is the vacant state and leaves the payload
// unread, so all-zero bytes are a valid vacant cell for every `Copy` payload.
unsafe impl<T: Copy> ZeroVacant for Occupiable<T> {
    #[cfg(loom)]
    fn vacant() -> Self {
        Self {
            occupied: false,
            value: MaybeUninit::uninit(),
        }
    }
}

impl<T: Copy> Occupiable<T> {
    pub(crate) fn get(&self) -> Option<T> {
        // SAFETY: `set` initialises the payload before raising the flag, and
        // `clear` lowers the flag without reading the payload.
        self.occupied.then(|| unsafe { self.value.assume_init() })
    }

    pub(crate) fn is_none(&self) -> bool {
        !self.occupied
    }

    pub(crate) fn set(&mut self, value: T) {
        self.value = MaybeUninit::new(value);
        self.occupied = true;
    }

    pub(crate) fn clear(&mut self) {
        self.occupied = false;
    }
}

impl<T: Copy + std::fmt::Debug> std::fmt::Debug for Occupiable<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.get().fmt(f)
    }
}

/// One fixed-length table of vacant elements backed by a private anonymous
/// mapping, so constructing it charges page tables rather than one write fault
/// per page, and its resident size tracks the slots a pool has touched.
pub(crate) struct MappedSlice<T> {
    mapping: Option<MappedArena>,
    len: usize,
    _elements: PhantomData<T>,
}

impl<T: ZeroVacant> MappedSlice<T> {
    pub(crate) fn try_vacant(capacity: u32) -> Option<Self> {
        Self::try_vacant_accounted(capacity as usize, CommitAccounting::Charged)
    }

    /// Reserves `len` vacant elements without charging the commit limit, so
    /// the address space can exceed RAM plus swap and only touched pages are
    /// ever backed.
    pub(crate) fn try_reserved(len: u64) -> Option<Self> {
        Self::try_vacant_accounted(usize::try_from(len).ok()?, CommitAccounting::Reserved)
    }

    fn try_vacant_accounted(len: usize, accounting: CommitAccounting) -> Option<Self> {
        let layout = Layout::array::<T>(len).ok()?;
        if layout.size() == 0 {
            return Some(Self {
                mapping: None,
                len,
                _elements: PhantomData,
            });
        }
        let mapping = MappedArena::try_map_accounted(layout.size(), layout.align(), accounting)?;
        #[cfg(loom)]
        {
            let base = mapping.base().cast::<T>();
            for index in 0..len {
                // SAFETY: `base` is aligned for `T` and spans `len` elements.
                unsafe { base.as_ptr().wrapping_add(index).write(T::vacant()) };
            }
        }
        Some(Self {
            mapping: Some(mapping),
            len,
            _elements: PhantomData,
        })
    }

    pub(crate) fn empty() -> Self {
        Self {
            mapping: None,
            len: 0,
            _elements: PhantomData,
        }
    }
}

impl<T> MappedSlice<T> {
    fn base(&self) -> NonNull<T> {
        self.mapping
            .as_ref()
            .map_or(NonNull::dangling(), |mapping| mapping.base().cast::<T>())
    }

    /// Borrows one element without forming a reference to the whole table, so
    /// a sparse reservation is never retagged or touched beyond `index`.
    ///
    /// # Panics
    ///
    /// If `index` lies outside the slice.
    pub(crate) fn element(&self, index: usize) -> &T {
        assert!(index < self.len, "an element index lies inside the slice");
        // SAFETY: `index < len`, so the offset stays inside the mapping's
        // `len` elements.
        let element = unsafe { self.base().add(index) };
        // SAFETY: the element is initialised and aligned for `T` and owned by
        // this value for its lifetime, as for `deref`.
        unsafe { element.as_ref() }
    }

    /// Borrows elements `start..start + len` without forming a reference to
    /// the whole table, so a sparse reservation is never retagged or touched
    /// beyond the range.
    ///
    /// # Panics
    ///
    /// If the range exceeds the slice.
    pub(crate) fn elements(&self, start: usize, len: usize) -> &[T] {
        assert!(
            start <= self.len && len <= self.len - start,
            "an element range lies inside the slice"
        );
        // SAFETY: `start + len <= self.len`, so the offset stays inside the
        // mapping's `len` elements.
        let first = unsafe { self.base().add(start) };
        // SAFETY: the range is initialised and aligned for `T` and owned by
        // this value for its lifetime, as for `deref`.
        unsafe { std::slice::from_raw_parts(first.as_ptr(), len) }
    }

    /// Returns to the kernel the whole OS pages inside elements
    /// `start..end`; pages shared with elements outside the range stay backed.
    ///
    /// # Safety
    ///
    /// Every element in `start..end` holds the all-zero vacant value and is
    /// interior-mutable, so a page the kernel discards reads back unchanged.
    ///
    /// # Panics
    ///
    /// If the range is empty or exceeds the slice.
    pub(crate) unsafe fn release_vacant(&self, start: u64, end: u64) {
        assert!(start < end, "a released element range is non-empty");
        assert!(
            end <= self.len as u64,
            "a released element range lies inside the slice"
        );
        let Some(mapping) = self.mapping.as_ref() else {
            return;
        };
        let element_bytes = size_of::<T>() as u64;
        let page = page_size() as u64;
        let first_byte = (start * element_bytes).next_multiple_of(page);
        let end_byte = (end * element_bytes) / page * page;
        if first_byte >= end_byte {
            return;
        }
        let offset = usize::try_from(first_byte).expect("an element offset fits usize");
        let len = usize::try_from(end_byte - first_byte).expect("a released span fits usize");
        #[cfg(not(any(loom, miri)))]
        {
            let address = mapping
                .base()
                .as_ptr()
                .wrapping_add(offset)
                .cast::<c_void>();
            // SAFETY: `address..address + len` is whole pages inside this
            // mapping; by the caller's contract every element there is already
            // zero, which is what a discarded private anonymous page reads as.
            // The advice is a give-back only, so its result is not needed.
            let _ = unsafe { madvise(address, len, MADV_DONTNEED) };
        }
        #[cfg(any(loom, miri))]
        let _ = (mapping, offset, len);
    }

    /// Bytes of the mapping currently backed by resident pages.
    #[cfg(any(feature = "mock", feature = "bench"))]
    pub(crate) fn resident_bytes(&self) -> u64 {
        const PAGES_PER_QUERY: usize = 512;
        let Some(mapping) = self.mapping.as_ref() else {
            return 0;
        };
        let page = page_size();
        let pages = mapping.total.div_ceil(page);
        let mut resident_pages = 0u64;
        let mut residency = [0u8; PAGES_PER_QUERY];
        for first in (0..pages).step_by(PAGES_PER_QUERY) {
            let count = PAGES_PER_QUERY.min(pages - first);
            let address = mapping.raw.cast::<u8>().wrapping_add(first * page);
            // SAFETY: `count` pages from `address` lie inside the mapping, and
            // `residency` holds one byte per page queried.
            let result = unsafe {
                mincore(
                    address.cast::<c_void>(),
                    count * page,
                    residency.as_mut_ptr(),
                )
            };
            assert_eq!(result, 0, "mincore succeeds over a live mapping");
            resident_pages += residency[..count]
                .iter()
                .filter(|&&state| state & 1 != 0)
                .count() as u64;
        }
        resident_pages * page as u64
    }
}

impl<T> Deref for MappedSlice<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        // SAFETY: the mapping holds `len` initialised elements aligned for `T`
        // (zero pages under `ZeroVacant`, or written by `try_vacant`), owned
        // exclusively by this value for its lifetime; a zero-sized `T` needs
        // no mapping and reads through the dangling base.
        unsafe { std::slice::from_raw_parts(self.base().as_ptr(), self.len) }
    }
}

impl<T> DerefMut for MappedSlice<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: as for `deref`, with `&mut self` excluding other borrows.
        unsafe { std::slice::from_raw_parts_mut(self.base().as_ptr(), self.len) }
    }
}

impl<T> Drop for MappedSlice<T> {
    fn drop(&mut self) {
        // SAFETY: the elements are initialised and dropped exactly once here,
        // before the field drop order returns the mapping.
        unsafe {
            std::ptr::drop_in_place(std::ptr::slice_from_raw_parts_mut(
                self.base().as_ptr(),
                self.len,
            ));
        };
    }
}

// SAFETY: the table owns its elements like `Box<[T]>`, so it is `Send` and
// `Sync` exactly when `[T]` is.
unsafe impl<T: Send> Send for MappedSlice<T> {}
// SAFETY: see the `Send` impl.
unsafe impl<T: Sync> Sync for MappedSlice<T> {}

impl<T: std::fmt::Debug> std::fmt::Debug for MappedSlice<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod mapped_arena_tests {
    use super::*;

    #[test]
    fn a_mapping_honors_an_alignment_above_the_page_size() {
        let align = 2 * 1024 * 1024;
        let arena = MappedArena::try_map(3 * 4096, align).expect("a small mapping succeeds");
        assert_eq!(arena.base().as_ptr().addr() % align, 0);
        assert_eq!(arena.len(), 3 * 4096);
        assert_eq!(
            arena.mapped_len(),
            (3usize * 4096).next_multiple_of(page_size()) + align
        );
    }

    #[test]
    fn a_mapping_below_the_page_size_still_maps_whole_pages() {
        let arena = MappedArena::try_map(512, 512).expect("a small mapping succeeds");
        assert_eq!(arena.len(), 512);
        assert_eq!(arena.mapped_len(), page_size());
    }

    #[test]
    fn a_span_beyond_the_address_space_is_refused_not_fatal() {
        assert!(MappedArena::try_map(usize::MAX / 2, 4096).is_none());
    }

    #[test]
    fn a_zero_sized_element_table_keeps_its_length() {
        struct Marker;
        // SAFETY: a zero-sized value has exactly one bit pattern.
        unsafe impl ZeroVacant for Marker {
            #[cfg(loom)]
            fn vacant() -> Self {
                Marker
            }
        }
        let table = MappedSlice::<Marker>::try_vacant(7).expect("a zero-sized table succeeds");
        assert_eq!(table.len(), 7);
        assert_eq!(table.iter().count(), 7);
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
