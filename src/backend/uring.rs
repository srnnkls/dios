//! `io_uring` backend (T004). Reads land in the preallocated frame slab through
//! fixed files — `READ_FIXED` against buffer index 0 under the `Registered`
//! posture, plain `READ` by pointer under `Unregistered` — and async fsync
//! rides the ring. One posture table ([`Posture`]) owns the dispatch; no submit
//! path names a buffer index. The batched
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

use io_uring::{IoUring, opcode, squeue, types};

use crate::driver::{
    Backend, DriverBuildError, Executor, OpKind, RegistrationPolicy, RegistrationPosture,
    RingExecutor, RingReap,
};
use crate::error::IoError;
use crate::pool::write_arena::ArenaState;
use crate::pool::{Frames, PoolConfigError, ReadFrameIdx};
use crate::product::{PlatformWake, WaitState};

const EINTR: i32 = 4;
const EAGAIN: i32 = 11;
const EBADF: i32 = 9;
const EIO: i32 = 5;
const ETIME: i32 = 62;
const POLLIN: u32 = 0x0001;
const WAKE_USER_DATA: u64 = u64::MAX;
const FRAMES_BUF_INDEX: u16 = 0;
const WRITE_ARENA_BUF_INDEX: u16 = 1;

/// The posture table: which SQE shape each data-plane op takes. Under
/// `Registered` the SAFETY contract is the buffer table (indexes 0 and 1 stay
/// valid while the ring lives); under `Unregistered` it is the arenas' fixed
/// addresses, which the ring outlives-in-reverse exactly as before (the
/// executor drops before the `Arc`s it holds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Posture {
    Registered,
    Unregistered,
}

impl Posture {
    fn read_entry(
        self,
        fd_slot: u32,
        destination: *mut u8,
        requested_len: u32,
        file_offset: u64,
    ) -> squeue::Entry {
        match self {
            Self::Registered => opcode::ReadFixed::new(
                types::Fixed(fd_slot),
                destination,
                requested_len,
                FRAMES_BUF_INDEX,
            )
            .offset(file_offset)
            .build(),
            Self::Unregistered => {
                opcode::Read::new(types::Fixed(fd_slot), destination, requested_len)
                    .offset(file_offset)
                    .build()
            }
        }
    }

    fn write_entry(
        self,
        fd_slot: u32,
        source: *const u8,
        requested_len: u32,
        file_offset: u64,
    ) -> squeue::Entry {
        match self {
            Self::Registered => opcode::WriteFixed::new(
                types::Fixed(fd_slot),
                source,
                requested_len,
                WRITE_ARENA_BUF_INDEX,
            )
            .offset(file_offset)
            .build(),
            Self::Unregistered => opcode::Write::new(types::Fixed(fd_slot), source, requested_len)
                .offset(file_offset)
                .build(),
        }
    }

    fn readback(self) -> RegistrationPosture {
        match self {
            Self::Registered => RegistrationPosture::Registered,
            Self::Unregistered => RegistrationPosture::Unregistered,
        }
    }
}

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
    _write_arena: Arc<ArenaState>,
    files: Mutex<Box<[Option<File>]>>,
    platform_wake: Arc<PlatformWake>,
    posture: Posture,
    frame_bytes: u32,
    file_capacity: u32,
}

impl std::fmt::Debug for Uring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Uring")
            .field("frame_bytes", &self.frame_bytes)
            .field("posture", &self.posture)
            .finish_non_exhaustive()
    }
}

impl Uring {
    pub(crate) const KIND: Backend = Backend::Uring;

    pub(crate) fn new(
        frames: Arc<Frames>,
        write_arena: Arc<ArenaState>,
        queue_capacity: u32,
        file_capacity: u32,
        registration_policy: RegistrationPolicy,
    ) -> Result<Self, DriverBuildError> {
        assert!(frames.count() > 0, "frame count must be positive");
        let frame_bytes = frames.granule();
        assert!(frame_bytes > 0, "frame size must be positive");
        assert!(queue_capacity > 0, "queue capacity must be positive");
        let files = crate::allocation::try_boxed_slice_with(file_capacity, || None)
            .ok_or(DriverBuildError::Allocation)?;
        let ring_entries = queue_capacity
            .checked_add(1)
            .expect("the product queue sum leaves room for one private wake request")
            .next_power_of_two();
        let ring = IoUring::new(ring_entries)
            .map_err(IoError::from)
            .map_err(DriverBuildError::Driver)?;
        let platform_wake = PlatformWake::new()
            .map_err(IoError::from)
            .map_err(DriverBuildError::Driver)?;

        if file_capacity > 0 {
            ring.submitter()
                .register_files_sparse(file_capacity)
                .map_err(IoError::from)
                .map_err(DriverBuildError::Driver)?;
        }
        let posture = select_posture(&ring, &frames, &write_arena, registration_policy)?;

        let backend = Self {
            ring,
            frames,
            _write_arena: write_arena,
            files: Mutex::new(files),
            platform_wake,
            posture,
            frame_bytes,
            file_capacity,
        };
        backend.arm_wake();
        Ok(backend)
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

    fn arm_wake(&self) {
        let entry = opcode::PollAdd::new(types::Fd(self.platform_wake.raw_fd()), POLLIN)
            .build()
            .user_data(WAKE_USER_DATA);
        self.push_sqe(&entry);
    }
}

impl Executor for Uring {
    fn register_file(&self, slot: u32, file: File) -> Result<(), IoError> {
        let raw = file.as_raw_fd();
        assert!(raw >= 0, "a retained File yields a valid descriptor");
        assert!(slot < self.file_capacity, "fd slot within the table");
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
        assert!(slot < self.file_capacity, "retire slot within the table");
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

    fn attach_pool_wait(&self, wait: &Arc<WaitState>) {
        wait.attach_platform(Arc::clone(&self.platform_wake));
    }

    #[cfg(any(feature = "mock", feature = "bench"))]
    fn copy_frame(&self, frame: ReadFrameIdx, out: &mut [u8]) -> usize {
        self.frames.copy_frame(frame, out)
    }

    fn registration_posture(&self) -> RegistrationPosture {
        self.posture.readback()
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
        assert!(fd_slot < self.file_capacity, "read targets a table slot");
        assert!(
            requested_len <= self.frame_bytes,
            "a read spans at most one frame"
        );
        let destination = self
            .frames
            .frame_ptr(frame, destination_offset, requested_len);
        let entry = self
            .posture
            .read_entry(fd_slot, destination, requested_len, file_offset)
            .user_data(user_data);
        self.push_sqe(&entry);
    }

    fn push_write(
        &self,
        user_data: u64,
        fd_slot: u32,
        source: *const u8,
        source_offset: u32,
        file_offset: u64,
        requested_len: u32,
    ) {
        assert!(fd_slot < self.file_capacity, "write targets a table slot");
        assert!(
            !source.is_null(),
            "a leased staging slot has a live pointer"
        );
        assert!(requested_len > 0, "a write transfers a non-empty slot");
        assert!(
            source_offset < self.frame_bytes,
            "a write source starts within one staging granule"
        );
        assert!(
            requested_len <= self.frame_bytes - source_offset,
            "a write source tail stays within its registered granule"
        );
        let entry = self
            .posture
            .write_entry(fd_slot, source, requested_len, file_offset)
            .user_data(user_data);
        self.push_sqe(&entry);
    }

    fn push_fsync(&self, user_data: u64, fd_slot: u32) {
        assert!(fd_slot < self.file_capacity, "fsync targets a table slot");
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

    fn reap<F: FnMut(u64, i32) -> bool>(&self, limit: u32, mut sink: F) -> RingReap {
        assert!(limit > 0, "a reap drains into a non-empty batch");
        // SAFETY: CQ userspace access is serialised by the caller holding the AD-4
        // mutex; no second completion handle exists concurrently.
        let mut cq = unsafe { self.ring.completion_shared() };
        let mut reaped = 0u32;
        let mut woke = false;
        while reaped < limit {
            let Some(cqe) = cq.next() else { break };
            if cqe.user_data() == WAKE_USER_DATA {
                woke = true;
                continue;
            }
            let keep_reaping = sink(cqe.user_data(), cqe.result());
            reaped += 1;
            if !keep_reaping {
                break;
            }
        }
        drop(cq);
        if woke {
            self.platform_wake.drain();
        }
        RingReap {
            backend_completions: reaped,
            rearm_needed: woke,
        }
    }

    fn rearm_after_reap(&self) {
        self.arm_wake();
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

/// Selects the posture: `Unregistered` registers nothing; `Registered` and
/// `Auto` attempt the registration because the kernel's charge is
/// authoritative (`CAP_IPC_LOCK` exempts a ring outright, so the advisory
/// `getrlimit` reading alone decides nothing). `ENOMEM` degrades `Auto` with
/// the remediation and refuses an explicit `Registered` typed; any other
/// failure is the operating error it is.
fn select_posture(
    ring: &IoUring,
    frames: &Frames,
    write_arena: &ArenaState,
    registration_policy: RegistrationPolicy,
) -> Result<Posture, DriverBuildError> {
    if registration_policy == RegistrationPolicy::Unregistered {
        return Ok(Posture::Unregistered);
    }
    let Err(error) = register_arenas(ring, frames, write_arena) else {
        return Ok(Posture::Registered);
    };
    if error.raw_os_error() != Some(crate::memlock::ENOMEM) {
        return Err(DriverBuildError::Driver(IoError::from(error)));
    }
    let arena_bytes = crate::driver::arena_bytes(frames, write_arena);
    let memlock_limit_bytes = crate::memlock::memlock_limit_bytes();
    if registration_policy == RegistrationPolicy::Registered {
        return Err(DriverBuildError::Configuration(
            PoolConfigError::RegistrationRefused {
                arena_bytes,
                memlock_limit_bytes,
            },
        ));
    }
    eprintln!(
        "dios: buffer registration refused ({arena_bytes} bytes against RLIMIT_MEMLOCK \
         {memlock_limit_bytes}); running unregistered — grant CAP_IPC_LOCK, raise the limit, \
         or select the posture explicitly"
    );
    Ok(Posture::Unregistered)
}

fn register_arenas(
    ring: &IoUring,
    frames: &Frames,
    write_arena: &ArenaState,
) -> std::io::Result<()> {
    let iov = [
        Iovec {
            iov_base: frames.base_ptr().cast(),
            iov_len: frames.span_len(),
        },
        Iovec {
            iov_base: write_arena.base_ptr().cast(),
            iov_len: write_arena.span_len(),
        },
    ];
    assert_eq!(
        usize::from(FRAMES_BUF_INDEX),
        0,
        "the frame slab registers first"
    );
    assert_eq!(
        usize::from(WRITE_ARENA_BUF_INDEX),
        1,
        "the write arena registers second"
    );
    // SAFETY: `Iovec` is layout-compatible with `libc::iovec` and the array
    // is live for this slice borrow.
    let bufs = unsafe { std::slice::from_raw_parts(iov.as_ptr().cast(), iov.len()) };
    // SAFETY: both arenas keep fixed addresses and the ring drops before
    // them, so registered buffer indexes 0 and 1 remain valid for every op.
    unsafe { ring.submitter().register_buffers(bufs) }
}

#[cfg(test)]
mod tests {
    use std::ptr::NonNull;

    use super::*;

    #[test]
    fn registered_posture_issues_fixed_buffer_opcodes() {
        let source = NonNull::<u8>::dangling().as_ptr();
        let write = Posture::Registered.write_entry(3, source, 4_096, 8_192);
        let read = Posture::Registered.read_entry(3, source, 4_096, 8_192);

        assert_eq!(
            write.get_opcode(),
            u32::from(opcode::WriteFixed::CODE),
            "ordinary WRITE does not prove the registered staging arena is used"
        );
        assert_eq!(read.get_opcode(), u32::from(opcode::ReadFixed::CODE));
    }

    #[test]
    fn unregistered_posture_issues_plain_opcodes() {
        let source = NonNull::<u8>::dangling().as_ptr();
        let write = Posture::Unregistered.write_entry(3, source, 4_096, 8_192);
        let read = Posture::Unregistered.read_entry(3, source, 4_096, 8_192);

        assert_eq!(
            write.get_opcode(),
            u32::from(opcode::Write::CODE),
            "an unregistered arena must never name a buffer index"
        );
        assert_eq!(read.get_opcode(), u32::from(opcode::Read::CODE));
        assert_eq!(
            read.user_data(7).get_user_data(),
            7,
            "the slab slot routes the CQE"
        );
    }
}
