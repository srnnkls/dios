//! `io_uring` backend (T004). Reads issue `READ_FIXED` against a registered
//! frame slab through fixed files; async fsync rides the ring. The batched
//! SQE-fill runs at poll's prepare phase under the AD-4 mutex, the kernel wait
//! (`EXT_ARG` on `poll_wait`) runs outside it, and CQEs reap under the mutex,
//! routed to slab slots by echoed `user_data`. The backend carries the data
//! plane through the ring poll seam ([`RingExecutor`]) and never implements the
//! eager [`EagerExecutor::attempt`](crate::driver) path. The metadata plane
//! (`write_all_blocking`/`fsync_blocking`) is a direct `pwrite`/`fsync` on the
//! retained file (AD-3), never the ring.

#![cfg(target_os = "linux")]

use std::alloc::{self, Layout};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::ptr::NonNull;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use io_uring::{IoUring, opcode, squeue, types};

use crate::driver::{Backend, Executor, MAX_FILES, OpKind, ReadFrameIdx, RingExecutor};
use crate::error::IoError;

const SLAB_ALIGN: usize = 4096;
const EINTR: i32 = 4;
const EAGAIN: i32 = 11;
const EBADF: i32 = 9;
const EIO: i32 = 5;
const ETIME: i32 = 62;

/// Layout-compatible mirror of `libc::iovec` (the sole `register_buffers`
/// argument type), so the crate needs no direct `libc` dependency.
#[repr(C)]
struct Iovec {
    iov_base: *mut core::ffi::c_void,
    iov_len: usize,
}

pub(crate) struct Uring {
    ring: IoUring,
    slab: FrameSlab,
    files: Mutex<Box<[Option<File>]>>,
    frame_bytes: u32,
}

impl std::fmt::Debug for Uring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Uring")
            .field("frame_bytes", &self.frame_bytes)
            .finish_non_exhaustive()
    }
}

impl Uring {
    pub(crate) const KIND: Backend = Backend::Uring;

    pub(crate) fn new(frames: u32, frame_bytes: u32, queue_capacity: u32) -> Self {
        assert!(frames > 0, "frame count must be positive");
        assert!(frame_bytes > 0, "frame size must be positive");
        assert!(queue_capacity > 0, "queue capacity must be positive");

        let ring = IoUring::new(queue_capacity.next_power_of_two())
            .expect("io_uring init on a supported kernel");
        let slab = FrameSlab::new(frames as usize * frame_bytes as usize);

        ring.submitter()
            .register_files_sparse(MAX_FILES)
            .expect("register a sparse fixed-file table");
        let iov = Iovec {
            iov_base: slab.as_ptr().cast(),
            iov_len: slab.len(),
        };
        // SAFETY: a one-element slice over the on-stack iovec; `register_buffers`
        // reads it only for the duration of the call.
        let bufs = unsafe { std::slice::from_raw_parts(std::ptr::from_ref(&iov).cast(), 1) };
        // SAFETY: `Iovec` is layout-compatible with `libc::iovec`; the slab keeps a
        // fixed address and the ring drops before it (declaration order), so the
        // registered buffer stays valid for every op the ring issues.
        unsafe { ring.submitter().register_buffers(bufs) }
            .expect("register the frame slab as a fixed buffer");

        let mut files = Vec::with_capacity(MAX_FILES as usize);
        files.resize_with(MAX_FILES as usize, || None);
        Self {
            ring,
            slab,
            files: Mutex::new(files.into_boxed_slice()),
            frame_bytes,
        }
    }

    fn lock_files(&self) -> MutexGuard<'_, Box<[Option<File>]>> {
        self.files.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn copy_frame(&self, frame: ReadFrameIdx, out: &mut [u8]) -> usize {
        self.slab.copy_frame(frame, self.frame_bytes, out)
    }

    fn push_sqe(&self, entry: &squeue::Entry) {
        // SAFETY: SQ userspace access is serialised by the caller holding the AD-4
        // mutex; no second submission handle exists concurrently.
        let mut sq = unsafe { self.ring.submission_shared() };
        // SAFETY: the SQE addresses the registered slab buffer and a fixed fd, both
        // valid for the whole in-flight op; fill is bounded so the SQ has room.
        let pushed = unsafe { sq.push(entry) };
        pushed.expect("ring SQ has room: fill is bounded by queue_capacity == SQ depth");
    }

    fn check_enter(outcome: std::io::Result<usize>) {
        let Err(error) = outcome else {
            return;
        };
        let code = error.raw_os_error().unwrap_or(0);
        assert!(
            matches!(code, ETIME | EINTR | EAGAIN),
            "io_uring_enter returned an unexpected errno {code}: EBUSY is unreachable given \
             the slab gates admission at queue_capacity and CQ >= SQ >= in-flight ({error})"
        );
    }
}

impl Executor for Uring {
    fn register_file(&self, slot: u32, file: File) -> Result<(), IoError> {
        let raw = file.as_raw_fd();
        assert!(raw >= 0, "a retained File yields a valid descriptor");
        assert!(
            (slot as usize) < MAX_FILES as usize,
            "fd slot within the table"
        );
        self.ring
            .submitter()
            .register_files_update(slot, &[raw])
            .map_err(IoError::from)?;
        let mut files = self.lock_files();
        assert!(
            files[slot as usize].is_none(),
            "fd slot reused before its prior file was retired"
        );
        files[slot as usize] = Some(file);
        Ok(())
    }

    fn clean_bytes(&self, kind: OpKind) -> u32 {
        match kind {
            OpKind::Fsync => 0,
            OpKind::Read | OpKind::Write => self.frame_bytes,
        }
    }

    fn schedule(&self, ready_len: usize) -> usize {
        ready_len
    }

    fn retire_file(&self, slot: u32) {
        assert!(
            (slot as usize) < MAX_FILES as usize,
            "retire slot within the table"
        );
        self.ring
            .submitter()
            .register_files_update(slot, &[-1])
            .expect(
                "clearing a retiring fd's fixed-file slot cannot fail on a live ring; a failure \
                 leaves the closing fd's slot stale with no sound recovery",
            );
        let mut files = self.lock_files();
        let file = files[slot as usize].take();
        debug_assert!(file.is_some(), "retire of a slot that holds no live file");
        drop(file);
    }
}

impl RingExecutor for Uring {
    fn read_len(&self) -> u32 {
        self.frame_bytes
    }

    fn push_read(&self, user_data: u64, fd_slot: u32, frame: ReadFrameIdx, offset: u64, len: u32) {
        assert!(
            (fd_slot as usize) < MAX_FILES as usize,
            "read targets a table slot"
        );
        assert!(len <= self.frame_bytes, "a read spans at most one frame");
        let buf = self.slab.frame_ptr(frame, self.frame_bytes);
        let entry = opcode::ReadFixed::new(types::Fixed(fd_slot), buf, len, 0)
            .offset(offset)
            .build()
            .user_data(user_data);
        self.push_sqe(&entry);
    }

    fn push_fsync(&self, user_data: u64, fd_slot: u32) {
        assert!(
            (fd_slot as usize) < MAX_FILES as usize,
            "fsync targets a table slot"
        );
        let entry = opcode::Fsync::new(types::Fixed(fd_slot))
            .build()
            .user_data(user_data);
        self.push_sqe(&entry);
    }

    fn submit(&self) {
        Self::check_enter(self.ring.submitter().submit());
    }

    fn submit_and_wait(&self, want: u32, timeout: Duration) {
        let timespec = types::Timespec::from(timeout);
        let args = types::SubmitArgs::new().timespec(&timespec);
        Self::check_enter(self.ring.submitter().submit_with_args(want as usize, &args));
    }

    fn reap<F: FnMut(u64, i32)>(&self, limit: u32, mut sink: F) -> u32 {
        assert!(limit > 0, "a reap drains into a non-empty batch");
        // SAFETY: CQ userspace access is serialised by the caller holding the AD-4
        // mutex; no second completion handle exists concurrently.
        let mut cq = unsafe { self.ring.completion_shared() };
        let mut reaped = 0u32;
        while reaped < limit {
            let Some(cqe) = cq.next() else { break };
            sink(cqe.user_data(), cqe.result());
            reaped += 1;
        }
        reaped
    }

    fn blocking_write(&self, fd_slot: u32, buf: &[u8], offset: u64) -> Result<u32, i32> {
        let files = self.lock_files();
        let Some(file) = files.get(fd_slot as usize).and_then(Option::as_ref) else {
            return Err(EBADF);
        };
        match file.write_at(buf, offset) {
            Ok(bytes) => {
                assert!(
                    bytes <= buf.len(),
                    "pwrite reported more bytes than requested"
                );
                Ok(u32::try_from(bytes).expect("write count within the u32 bound"))
            }
            Err(error) => Err(error.raw_os_error().unwrap_or(EIO)),
        }
    }

    fn blocking_fsync(&self, fd_slot: u32) -> Result<(), i32> {
        let files = self.lock_files();
        let Some(file) = files.get(fd_slot as usize).and_then(Option::as_ref) else {
            return Err(EBADF);
        };
        file.sync_all()
            .map_err(|error| error.raw_os_error().unwrap_or(EIO))
    }
}

/// The registered read-buffer backing: a fixed-address, page-aligned byte
/// region carved into frames. Alignment satisfies both `O_DIRECT` sector rules
/// and the registered-buffer contract; the address never moves after init.
#[derive(Debug)]
struct FrameSlab {
    ptr: NonNull<u8>,
    len: usize,
    layout: Layout,
}

// SAFETY: the slab is a plain byte region reached only through the completion
// protocol — the kernel writes a frame solely while its op is in flight, and a
// reader touches a frame solely after that op's completion has drained — so no
// two accesses to one frame race across threads.
unsafe impl Send for FrameSlab {}
// SAFETY: see the `Send` justification above.
unsafe impl Sync for FrameSlab {}

impl FrameSlab {
    fn new(bytes: usize) -> Self {
        assert!(bytes > 0, "the slab spans at least one frame");
        let layout = Layout::from_size_align(bytes, SLAB_ALIGN).expect("a valid slab layout");
        // SAFETY: `layout` has non-zero size (asserted above).
        let raw = unsafe { alloc::alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).unwrap_or_else(|| alloc::handle_alloc_error(layout));
        Self {
            ptr,
            len: bytes,
            layout,
        }
    }

    fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    fn len(&self) -> usize {
        self.len
    }

    fn frame_ptr(&self, frame: ReadFrameIdx, frame_bytes: u32) -> *mut u8 {
        let start = frame.get() as usize * frame_bytes as usize;
        assert!(
            start + frame_bytes as usize <= self.len,
            "the frame region lies within the slab"
        );
        // SAFETY: `start + frame_bytes <= len` (asserted), so the offset stays in
        // the allocation.
        unsafe { self.ptr.as_ptr().add(start) }
    }

    fn copy_frame(&self, frame: ReadFrameIdx, frame_bytes: u32, out: &mut [u8]) -> usize {
        let start = frame.get() as usize * frame_bytes as usize;
        let count = out.len().min(frame_bytes as usize);
        assert!(start < self.len, "the frame starts within the slab");
        assert!(
            start + count <= self.len,
            "the frame region lies within the slab"
        );
        // SAFETY: `start + count <= len` (asserted), so the offset stays in the
        // allocation.
        let src = unsafe { self.ptr.as_ptr().add(start) };
        // SAFETY: `[src, src+count)` is in-bounds; `out` is a caller buffer that
        // cannot overlap the slab.
        unsafe {
            std::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), count);
        }
        count
    }
}

impl Drop for FrameSlab {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from `alloc_zeroed` with `layout` and is freed once.
        unsafe {
            alloc::dealloc(self.ptr.as_ptr(), self.layout);
        }
    }
}
