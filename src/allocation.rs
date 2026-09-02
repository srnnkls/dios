use std::alloc::{Layout, alloc, alloc_zeroed};
use std::collections::VecDeque;
use std::ptr::NonNull;

pub(crate) fn allocate(layout: Layout) -> Option<NonNull<u8>> {
    assert!(layout.size() > 0, "fixed arena layouts are non-empty");
    // SAFETY: `layout` is valid and non-empty. A null return becomes `None`
    // before the pointer is used.
    NonNull::new(unsafe { alloc(layout) })
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
