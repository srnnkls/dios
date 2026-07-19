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

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use io_uring::{opcode, squeue, types, IoUring};

use crate::driver::{Backend, Executor, OpKind, RingExecutor, MAX_FILES};
use crate::error::IoError;
use crate::pool::Frames;
use crate::pool::ReadFrameIdx;

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
    frames: Arc<Frames>,
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

    pub(crate) fn new(frames: Arc<Frames>, queue_capacity: u32) -> Result<Self, IoError> {
        assert!(frames.count() > 0, "frame count must be positive");
        let frame_bytes = frames.granule();
        assert!(frame_bytes > 0, "frame size must be positive");
        assert!(queue_capacity > 0, "queue capacity must be positive");

        let ring = IoUring::new(queue_capacity.next_power_of_two()).map_err(IoError::from)?;

        ring.submitter()
            .register_files_sparse(MAX_FILES)
            .map_err(IoError::from)?;
        let iov = Iovec {
            iov_base: frames.base_ptr().cast(),
            iov_len: frames.span_len(),
        };
        // SAFETY: a one-element slice over the on-stack iovec; `register_buffers`
        // reads it only for the duration of the call.
        let bufs = unsafe { std::slice::from_raw_parts(std::ptr::from_ref(&iov).cast(), 1) };
        // SAFETY: `Iovec` is layout-compatible with `libc::iovec`; the slab keeps a
        // fixed address and the ring drops before it (declaration order), so the
        // registered buffer stays valid for every op the ring issues.
        unsafe { ring.submitter().register_buffers(bufs) }.map_err(IoError::from)?;

        let mut files = Vec::with_capacity(MAX_FILES as usize);
        files.resize_with(MAX_FILES as usize, || None);
        Ok(Self {
            ring,
            frames,
            files: Mutex::new(files.into_boxed_slice()),
            frame_bytes,
        })
    }

    fn lock_files(&self) -> MutexGuard<'_, Box<[Option<File>]>> {
        self.files.lock().unwrap_or_else(PoisonError::into_inner)
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

    #[cfg(any(feature = "mock", feature = "bench"))]
    fn copy_frame(&self, frame: ReadFrameIdx, out: &mut [u8]) -> usize {
        self.frames.copy_frame(frame, out)
    }
}

impl RingExecutor for Uring {
    fn push_read(
        &self,
        user_data: u64,
        fd_slot: u32,
        frame: ReadFrameIdx,
        file_offset: u64,
        destination_offset: u32,
        requested_len: u32,
    ) {
        assert!(
            (fd_slot as usize) < MAX_FILES as usize,
            "read targets a table slot"
        );
        assert!(
            requested_len <= self.frame_bytes,
            "a read spans at most one frame"
        );
        let destination = self
            .frames
            .frame_ptr(frame, destination_offset, requested_len);
        let entry = opcode::ReadFixed::new(types::Fixed(fd_slot), destination, requested_len, 0)
            .offset(file_offset)
            .build()
            .user_data(user_data);
        self.push_sqe(&entry);
    }

    fn push_write(
        &self,
        user_data: u64,
        fd_slot: u32,
        source: *const u8,
        file_offset: u64,
        requested_len: u32,
    ) {
        assert!(
            (fd_slot as usize) < MAX_FILES as usize,
            "write targets a table slot"
        );
        assert!(
            !source.is_null(),
            "a leased staging slot has a live pointer"
        );
        assert!(requested_len > 0, "a write transfers a non-empty slot");
        let entry = opcode::Write::new(types::Fixed(fd_slot), source, requested_len)
            .offset(file_offset)
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
