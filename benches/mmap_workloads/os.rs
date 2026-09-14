use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::ptr::NonNull;

use serde::Serialize;

use super::catalog::{Arm, GRANULE, PAGES, PageNumber};

pub(super) struct Mapping {
    pointer: NonNull<libc::c_void>,
    length: usize,
    file: File,
}

// SAFETY: benchmark fixtures are sealed and never modified/truncated while mapped;
// all access is read-only and scoped workers finish before this owner is dropped.
unsafe impl Sync for Mapping {}

impl Mapping {
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let length = (PAGES as usize + 1) * GRANULE as usize;
        if file.metadata()?.len() != u64::try_from(length).expect("file length") {
            return Err(io::Error::other("fixture length differs from contract"));
        }
        // SAFETY: sysconf takes this constant selector and no pointers.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size != i64::from(GRANULE) {
            return Err(io::Error::other(
                "fault lanes require 4096-byte Linux pages",
            ));
        }
        // SAFETY: live fd, exact initialized file extent, read-only mapping. The
        // task-owned fixture remains immutable until all scoped readers finish.
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if pointer == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let pointer = NonNull::new(pointer).expect("Linux nonzero mmap allocation");
        Ok(Self {
            pointer,
            length,
            file,
        })
    }

    pub(super) fn page(&self, page: PageNumber) -> &[u8] {
        assert!(page.0 <= PAGES);
        let offset = page.0 as usize * GRANULE as usize;
        assert!(offset + GRANULE as usize <= self.length);
        // SAFETY: offset and extent are within the live immutable mapping.
        let pointer = unsafe { self.pointer.cast::<u8>().as_ptr().add(offset) };
        // SAFETY: the sealed file initializes this whole page; self owns the mapping.
        unsafe { std::slice::from_raw_parts(pointer, GRANULE as usize) }
    }

    pub(super) fn advice(&self, arm: Arm) -> io::Result<()> {
        let advice = match arm {
            Arm::MmapNormal => libc::MADV_NORMAL,
            Arm::MmapSequential => libc::MADV_SEQUENTIAL,
            _ => libc::MADV_RANDOM,
        };
        self.advise(advice)
    }

    pub(super) fn discard_ptes(&self) -> io::Result<()> {
        self.advise(libc::MADV_DONTNEED)
    }

    fn advise(&self, advice: i32) -> io::Result<()> {
        assert!(self.length > 0);
        // SAFETY: range is this owner's live mapping; callers hold no page borrow
        // when discarding PTEs, and the backing file is read-only and unchanged.
        let result = unsafe { libc::madvise(self.pointer.as_ptr(), self.length, advice) };
        status(result)
    }

    pub(super) fn discard_cache(&self) -> io::Result<()> {
        self.discard_ptes()?;
        self.file.sync_all()?;
        // SAFETY: fd remains open, whole-file advice has no userspace pointers.
        let result =
            unsafe { libc::posix_fadvise(self.file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        Ok(())
    }

    pub(super) fn residency(&self) -> io::Result<Vec<u8>> {
        let mut pages = vec![0; PAGES as usize + 1];
        // SAFETY: one output byte per system page, aligned address, live mapping.
        let result =
            unsafe { libc::mincore(self.pointer.as_ptr(), self.length, pages.as_mut_ptr()) };
        status(result)?;
        Ok(pages)
    }

    pub(super) fn present_ptes(&self) -> io::Result<Vec<bool>> {
        let mut bytes = vec![0_u8; (PAGES as usize + 1) * 8];
        let offset = u64::try_from(self.pointer.as_ptr().addr()).expect("virtual address")
            / u64::from(GRANULE)
            * 8;
        File::open("/proc/self/pagemap")?.read_exact_at(&mut bytes, offset)?;
        Ok(bytes
            .chunks_exact(8)
            .map(|entry| {
                u64::from_ne_bytes(entry.try_into().expect("pagemap entry")) & (1 << 63) != 0
            })
            .collect())
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: exact owned mapping; all scoped readers have already joined.
        let result = unsafe { libc::munmap(self.pointer.as_ptr(), self.length) };
        assert_eq!(result, 0, "owned mapping unmaps once");
    }
}

#[derive(Clone, Copy, Default, Debug, Serialize)]
pub(super) struct Usage {
    pub(super) minor_faults: u64,
    pub(super) major_faults: u64,
    pub(super) voluntary_switches: u64,
    pub(super) involuntary_switches: u64,
    pub(super) user_cpu_us: u64,
    pub(super) system_cpu_us: u64,
}

pub(super) fn usage() -> io::Result<Usage> {
    let mut value = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: valid writable rusage storage; success initializes all fields.
    status(unsafe { libc::getrusage(libc::RUSAGE_THREAD, value.as_mut_ptr()) })?;
    // SAFETY: successful getrusage initialized value above.
    let value = unsafe { value.assume_init() };
    Ok(Usage {
        minor_faults: positive(value.ru_minflt),
        major_faults: positive(value.ru_majflt),
        voluntary_switches: positive(value.ru_nvcsw),
        involuntary_switches: positive(value.ru_nivcsw),
        user_cpu_us: timeval_us(value.ru_utime),
        system_cpu_us: timeval_us(value.ru_stime),
    })
}

impl Usage {
    pub(super) fn since(self, before: Self) -> Self {
        Self {
            minor_faults: self.minor_faults - before.minor_faults,
            major_faults: self.major_faults - before.major_faults,
            voluntary_switches: self.voluntary_switches - before.voluntary_switches,
            involuntary_switches: self.involuntary_switches - before.involuntary_switches,
            user_cpu_us: self.user_cpu_us - before.user_cpu_us,
            system_cpu_us: self.system_cpu_us - before.system_cpu_us,
        }
    }
}

pub(super) fn cpu_ns() -> io::Result<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid writable timespec and the calling thread's CPU clock.
    status(unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut time) })?;
    Ok(positive(time.tv_sec) * 1_000_000_000 + positive(time.tv_nsec))
}

pub(super) fn pin(worker: u32) -> io::Result<()> {
    assert!(worker < 4);
    // SAFETY: all-zero cpu_set_t is a valid empty set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: worker is within CPU_SETSIZE and set is writable.
    unsafe { libc::CPU_SET(worker as usize, &mut set) };
    // SAFETY: valid initialized set of the stated size, pid zero means this thread.
    status(unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &raw const set) })
}

fn timeval_us(time: libc::timeval) -> u64 {
    positive(time.tv_sec) * 1_000_000 + positive(time.tv_usec)
}

fn positive(value: i64) -> u64 {
    u64::try_from(value).expect("kernel counter is nonnegative")
}

fn status(result: i32) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
