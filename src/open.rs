//! Backend-agnostic direct-IO probe (and the darwin barrier fsync the eager
//! backend issues). Direct support and its required alignment are probed per
//! opened file; the result is an observable [`IoMode`], never a silent bool.
//!
//! Darwin: `F_NOCACHE` drops the page cache but does not enforce alignment, so a
//! sector alignment is self-imposed. Linux: `statx(STATX_DIOALIGN)` (kernel
//! ≥ 6.1) with a TigerBeetle-style `O_DIRECT` write-probe fallback pre-6.1.

use std::fs::File;

use crate::driver::IoMode;
use crate::error::IoError;

#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::ffi::c_int;

/// Direct-I/O policy for an opened data file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DirectIo {
    /// Use the buffered page-cache path.
    Disabled,
    /// Prefer direct I/O, falling back to buffered I/O when unsupported.
    Preferred,
    /// Require direct I/O and reject files that cannot provide it.
    Required,
}

pub(crate) fn probe(file: &File, policy: DirectIo) -> Result<IoMode, IoError> {
    if policy == DirectIo::Disabled {
        return Ok(IoMode::Buffered);
    }
    #[cfg(target_os = "linux")]
    let mode = probe_direct(file)?;
    #[cfg(not(target_os = "linux"))]
    let mode = probe_direct(file);
    if policy == DirectIo::Required && mode == IoMode::Buffered {
        let error = std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "direct IO is unsupported for this file",
        );
        Err(IoError::from(error))
    } else {
        Ok(mode)
    }
}

#[cfg(target_os = "macos")]
const F_NOCACHE: c_int = 48;
#[cfg(target_os = "macos")]
const F_FULLFSYNC: c_int = 51;
#[cfg(target_os = "macos")]
const DARWIN_SECTOR_BYTES: u32 = 4096;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
}

#[cfg(target_os = "macos")]
fn probe_direct(file: &File) -> IoMode {
    use std::os::unix::io::AsRawFd;

    let fd = file.as_raw_fd();
    assert!(fd >= 0, "an owned File yields a valid descriptor");
    // SAFETY: `fd` is live for the call (owned by `file`); `F_NOCACHE` consumes
    // its one int argument and only toggles the descriptor's cache policy.
    let status = unsafe { fcntl(fd, F_NOCACHE, 1) };
    if status == -1 {
        return IoMode::Buffered;
    }
    let alignment =
        crate::alignment::Alignment::new(DARWIN_SECTOR_BYTES).expect("4096 is a power of two");
    debug_assert!(
        alignment.get().is_power_of_two() && alignment.get() >= 512,
        "a direct sector alignment is a power of two at least 512"
    );
    IoMode::Direct(alignment)
}

#[cfg(target_os = "macos")]
pub(crate) fn full_fsync(file: &File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    let fd = file.as_raw_fd();
    assert!(fd >= 0, "an owned File yields a valid descriptor");
    // SAFETY: `fd` is live for the call (owned by `file`); `F_FULLFSYNC` takes no
    // variadic argument and only flushes the device write cache.
    let status = unsafe { fcntl(fd, F_FULLFSYNC) };
    if status == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// statx(2) and fcntl(2) declared to match glibc's C ABI on the linux build
// targets; signatures follow the man pages.
#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn statx(dirfd: c_int, pathname: *const u8, flags: c_int, mask: u32, buf: *mut u8) -> c_int;
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
}

// O_DIRECT is arch-specific in linux uapi asm/fcntl.h: 0x4000 on x86_64, 0x10000 on aarch64 (where 0x4000 is O_DIRECTORY).
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const O_DIRECT: c_int = 0x4000;
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const O_DIRECT: c_int = 0x10000;
#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "x86_64", target_arch = "aarch64"))
))]
compile_error!("O_DIRECT is arch-specific; add this target_arch's value from linux asm/fcntl.h");

#[cfg(target_os = "linux")]
fn probe_direct(file: &File) -> Result<IoMode, IoError> {
    use std::os::unix::io::AsRawFd;

    let fd = file.as_raw_fd();
    let Some(alignment) = statx_dio_align(fd).or_else(|| write_probe(file)) else {
        eprintln!("dios: direct IO unavailable on this file; falling back to buffered reads");
        return Ok(IoMode::Buffered);
    };
    probe_direct_apply(fd)?;
    Ok(IoMode::Direct(alignment))
}

/// Sets `O_DIRECT` on the retained descriptor itself, so the fd the driver
/// registers and issues reads against is the direct one — the probe never opens
/// a separate `O_DIRECT` fd that would leak or diverge from the registered handle.
///
/// # Errors
///
/// The `fcntl` failure as an [`IoError`]: an operating error, not a programmer
/// bug, so the open path surfaces it rather than aborting.
#[cfg(target_os = "linux")]
fn probe_direct_apply(fd: c_int) -> Result<(), IoError> {
    const F_GETFL: c_int = 3;
    const F_SETFL: c_int = 4;

    assert!(fd >= 0, "an owned File yields a valid descriptor");
    // SAFETY: `F_GETFL` reads the descriptor's status flags and takes no variadic
    // argument.
    let flags = unsafe { fcntl(fd, F_GETFL) };
    if flags < 0 {
        return Err(IoError::from(std::io::Error::last_os_error()));
    }
    // SAFETY: `F_SETFL` consumes one int; on a descriptor the probe proved
    // direct-capable, adding `O_DIRECT` only switches the data plane.
    let status = unsafe { fcntl(fd, F_SETFL, flags | O_DIRECT) };
    if status == -1 {
        return Err(IoError::from(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn statx_dio_align(fd: c_int) -> Option<crate::alignment::Alignment> {
    const AT_EMPTY_PATH: c_int = 0x1000;
    const STATX_DIOALIGN: u32 = 0x0000_2000;
    const STATX_LEN: usize = 256;
    // Byte offsets into `struct statx` (linux uapi, include/uapi/linux/stat.h):
    // `stx_mask` at 0, `stx_dio_offset_align` at 156.
    const MASK_OFFSET: usize = 0;
    const DIO_OFFSET_ALIGN_OFFSET: usize = 156;
    const _: () = assert!(
        DIO_OFFSET_ALIGN_OFFSET + 4 <= STATX_LEN,
        "the dio-align field lies within the statx buffer"
    );

    assert!(fd >= 0, "an owned File yields a valid descriptor");
    let mut buf = [0u8; STATX_LEN];
    // SAFETY: `buf` is 256 bytes = `sizeof(struct statx)`; an empty path with
    // `AT_EMPTY_PATH` stats `fd`, and statx writes only into `buf`.
    let status = unsafe {
        statx(
            fd,
            c"".as_ptr().cast(),
            AT_EMPTY_PATH,
            STATX_DIOALIGN,
            buf.as_mut_ptr(),
        )
    };
    if status != 0 {
        return None;
    }
    let mask = u32::from_ne_bytes(buf[MASK_OFFSET..MASK_OFFSET + 4].try_into().ok()?);
    if mask & STATX_DIOALIGN == 0 {
        return None;
    }
    let align = u32::from_ne_bytes(
        buf[DIO_OFFSET_ALIGN_OFFSET..DIO_OFFSET_ALIGN_OFFSET + 4]
            .try_into()
            .ok()?,
    );
    crate::alignment::Alignment::new(align)
}

#[cfg(target_os = "linux")]
fn write_probe(file: &File) -> Option<crate::alignment::Alignment> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    const PROBE_BYTES_MAX: u32 = 4096;

    let fd = file.as_raw_fd();
    assert!(fd >= 0, "an owned File yields a valid descriptor");
    let probe_path = format!("/proc/self/fd/{fd}");
    for candidate in [512u32, 1024, 2048, 4096] {
        assert!(
            candidate <= PROBE_BYTES_MAX,
            "a probe read stays within the aligned scratch buffer"
        );
        let Some(alignment) = crate::alignment::Alignment::new(candidate) else {
            continue;
        };
        let Ok(direct) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECT)
            .open(&probe_path)
        else {
            continue;
        };
        if direct_read_ok(&direct, candidate) {
            return Some(alignment);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn direct_read_ok(file: &File, len: u32) -> bool {
    use std::os::unix::fs::FileExt;

    #[repr(align(4096))]
    struct SectorAligned([u8; 4096]);

    let mut buf = SectorAligned([0u8; 4096]);
    assert!(
        len as usize <= buf.0.len(),
        "a probe read stays within the aligned scratch buffer"
    );
    file.read_at(&mut buf.0[..len as usize], 0).is_ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn probe_direct(_file: &File) -> IoMode {
    IoMode::Buffered
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn full_fsync(file: &File) -> std::io::Result<()> {
    file.sync_all()
}
