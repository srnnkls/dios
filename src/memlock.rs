//! The `RLIMIT_MEMLOCK` seam shared by buffer registration and arena locking:
//! both charge the same limit, so one place reads it and one place locks a
//! range. Declared against the C ABI of the supported targets, where `rlim_t`
//! is a 64-bit unsigned integer, so the crate needs no `libc` dependency.

use std::ffi::{c_int, c_void};

#[cfg(target_os = "linux")]
const RLIMIT_MEMLOCK: c_int = 8;
#[cfg(target_os = "macos")]
const RLIMIT_MEMLOCK: c_int = 6;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("RLIMIT_MEMLOCK is target-specific; add this target's value from sys/resource.h");

pub(crate) const ENOMEM: i32 = 12;

#[repr(C)]
struct Rlimit {
    rlim_cur: u64,
    rlim_max: u64,
}

unsafe extern "C" {
    fn getrlimit(resource: c_int, rlim: *mut Rlimit) -> c_int;
    fn mlock(addr: *const c_void, len: usize) -> c_int;
    fn munlock(addr: *const c_void, len: usize) -> c_int;
}

/// The soft `RLIMIT_MEMLOCK` in bytes; `u64::MAX` when unlimited. Advisory:
/// the kernel's charge at registration or lock time is authoritative.
pub(crate) fn memlock_limit_bytes() -> u64 {
    let mut limit = Rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a live, writable `Rlimit` for the call's duration.
    let rc = unsafe { getrlimit(RLIMIT_MEMLOCK, &raw mut limit) };
    if rc == 0 { limit.rlim_cur } else { 0 }
}

/// Locks `len` bytes at `base` into memory.
///
/// # Errors
///
/// The errno of the refused lock (`ENOMEM` past `RLIMIT_MEMLOCK`, `EPERM`
/// without the privilege).
///
/// # Safety
///
/// `base..base + len` is one live mapping owned by the caller for as long as
/// the lock is held; the caller unlocks it with [`unlock_range`] before the
/// mapping is freed.
pub(crate) unsafe fn lock_range(base: *const u8, len: usize) -> Result<(), i32> {
    assert!(!base.is_null(), "a locked range has a live base");
    assert!(len > 0, "a locked range is non-empty");
    // SAFETY: the caller guarantees `base..base + len` is one live mapping.
    let rc = unsafe { mlock(base.cast(), len) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(ENOMEM))
    }
}

/// Unlocks a range locked by [`lock_range`].
///
/// # Safety
///
/// `base..base + len` is the range a prior [`lock_range`] call locked, still
/// mapped.
pub(crate) unsafe fn unlock_range(base: *const u8, len: usize) {
    assert!(!base.is_null(), "an unlocked range has a live base");
    assert!(len > 0, "an unlocked range is non-empty");
    // SAFETY: the caller guarantees `base..base + len` is the still-mapped
    // range a prior `lock_range` locked.
    let rc = unsafe { munlock(base.cast(), len) };
    assert_eq!(rc, 0, "unlocking a range this process locked cannot fail");
}
