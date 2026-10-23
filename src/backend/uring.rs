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
#[cfg(any(feature = "mock", feature = "bench"))]
use crate::pool::ReadFrameIdx;
use crate::pool::write_arena::ArenaState;
use crate::pool::{Frames, InFlightFrame, PoolConfigError};
use crate::product::{PlatformWake, WaitState};

const EINTR: i32 = 4;
const EAGAIN: i32 = 11;
const EBADF: i32 = 9;
const EIO: i32 = 5;
const ETIME: i32 = 62;
const POLLIN: u32 = 0x0001;
const WAKE_USER_DATA: u64 = u64::MAX;
/// `io_uring` refuses a registered buffer longer than one GiB (`EFAULT`), so
/// each arena registers as consecutive chunks of at most this size. A chunk
/// boundary never splits a frame or staging slot: both sizes are powers of two
/// no larger than a chunk.
const REGISTERED_CHUNK_BYTES: usize = 1 << 30;
/// The kernel's registered-buffer table limit (`IORING_MAX_REG_BUFFERS`).
const REGISTERED_BUFFERS_MAX: usize = 1 << 14;

/// Where each arena's chunks sit in the registered-buffer table: the frame
/// arena from index 0, the write arena right after its last chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BufferTable {
    frames_base: usize,
    write_base: usize,
    write_first_index: u16,
}

impl BufferTable {
    fn chunks(len: usize) -> usize {
        len.div_ceil(REGISTERED_CHUNK_BYTES)
    }

    fn index(base: usize, first: u16, address: *const u8) -> u16 {
        let offset = address
            .addr()
            .checked_sub(base)
            .expect("a fixed-buffer address lies inside its registered arena");
        let chunk = u16::try_from(offset / REGISTERED_CHUNK_BYTES)
            .expect("the chunk count was bounded at registration");
        first
            .checked_add(chunk)
            .expect("the chunk index was bounded at registration")
    }

    fn frame_index(self, destination: *const u8) -> u16 {
        Self::index(self.frames_base, 0, destination)
    }

    fn write_index(self, source: *const u8) -> u16 {
        Self::index(self.write_base, self.write_first_index, source)
    }
}

/// The posture table: which SQE shape each data-plane op takes. Under
/// `Registered` the SAFETY contract is the buffer table (every chunk index
/// stays valid while the ring lives); under `Unregistered` it is the arenas'
/// fixed addresses, which the ring outlives-in-reverse exactly as before (the
/// executor drops before the `Arc`s it holds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Posture {
    Registered(BufferTable),
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
            Self::Registered(table) => opcode::ReadFixed::new(
                types::Fixed(fd_slot),
                destination,
                requested_len,
                table.frame_index(destination),
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
            Self::Registered(table) => opcode::WriteFixed::new(
                types::Fixed(fd_slot),
                source,
                requested_len,
                table.write_index(source),
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
            Self::Registered(_) => RegistrationPosture::Registered,
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

        let mut backend = Self {
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

    fn push_sqe(&self, _access: &mut crate::driver::RingAccess<'_>, entry: &squeue::Entry) {
        // SAFETY: only the core that owns this ring mints a `RingAccess`, from
        // its own AD-4 guard, and it is borrowed exclusively here, so no second
        // submission handle exists.
        let mut sq = unsafe { self.ring.submission_shared() };
        Self::push_entry(&mut sq, entry);
    }

    fn push_entry(sq: &mut squeue::SubmissionQueue<'_>, entry: &squeue::Entry) {
        // SAFETY: every resource the SQE names outlives the op: a read or write
        // buffer is the posture's registered index or fixed-address arena, a
        // READV descriptor array is the vector storage the slab retains until
        // reap, the wake poll's eventfd is owned by this executor, fsync names no
        // buffer, and data-plane fds are fixed-file slots retired only after reap.
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

    fn wake_entry(&self) -> squeue::Entry {
        opcode::PollAdd::new(types::Fd(self.platform_wake.raw_fd()), POLLIN)
            .build()
            .user_data(WAKE_USER_DATA)
    }

    fn arm_wake(&mut self) {
        let entry = self.wake_entry();
        Self::push_entry(&mut self.ring.submission(), &entry);
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
    fn push_read_vector(
        &self,
        access: &mut crate::driver::RingAccess<'_>,
        user_data: u64,
        file: crate::driver::FileId,
        vector: &crate::driver::read_vector::ReadVector,
        file_offset: u64,
    ) {
        assert!(
            file.slot() < self.file_capacity,
            "vector targets a retained file"
        );
        let descriptors = vector.io();
        let entry = opcode::Readv::new(
            types::Fixed(file.slot()),
            descriptors.pointer().cast(),
            descriptors.count(),
        )
        .offset(file_offset)
        .build()
        .user_data(user_data);
        self.push_sqe(access, &entry);
    }

    fn push_read(
        &self,
        access: &mut crate::driver::RingAccess<'_>,
        user_data: u64,
        fd_slot: u32,
        token: &InFlightFrame,
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
            .transfer_ptr(token, destination_offset, requested_len);
        let entry = self
            .posture
            .read_entry(fd_slot, destination, requested_len, file_offset)
            .user_data(user_data);
        self.push_sqe(access, &entry);
    }

    fn push_write(
        &self,
        access: &mut crate::driver::RingAccess<'_>,
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
        self.push_sqe(access, &entry);
    }

    fn push_fsync(
        &self,
        access: &mut crate::driver::RingAccess<'_>,
        user_data: u64,
        fd_slot: u32,
        mode: crate::driver::SyncMode,
    ) {
        assert!(fd_slot < self.file_capacity, "fsync targets a table slot");
        let flags = match mode {
            crate::driver::SyncMode::Data => types::FsyncFlags::DATASYNC,
            crate::driver::SyncMode::Full => types::FsyncFlags::empty(),
        };
        let entry = opcode::Fsync::new(types::Fixed(fd_slot))
            .flags(flags)
            .build()
            .user_data(user_data);
        self.push_sqe(access, &entry);
    }

    fn submit(&self) {
        Self::check_enter(self.ring.submitter().submit());
    }

    fn submit_and_wait(&self, want: u32, timeout: Duration) {
        let timespec = types::Timespec::from(timeout);
        let args = types::SubmitArgs::new().timespec(&timespec);
        Self::check_enter(self.ring.submitter().submit_with_args(want as usize, &args));
    }

    fn reap<F: FnMut(u64, i32) -> bool>(
        &self,
        _access: &mut crate::driver::RingAccess<'_>,
        limit: u32,
        mut sink: F,
    ) -> RingReap {
        assert!(limit > 0, "a reap drains into a non-empty batch");
        // SAFETY: only the core that owns this ring mints a `RingAccess`, from
        // its own AD-4 guard, and it is borrowed exclusively here, so no second
        // completion handle exists.
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

    fn rearm_after_reap(&self, access: &mut crate::driver::RingAccess<'_>) {
        let entry = self.wake_entry();
        self.push_sqe(access, &entry);
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

    fn blocking_fsync(&self, fd_slot: u32, mode: crate::driver::SyncMode) -> Result<(), i32> {
        let files = self.lock_files();
        let Some(file) = files.get(fd_slot as usize).and_then(Option::as_ref) else {
            return Err(EBADF);
        };
        crate::open::sync_file(file, mode).map_err(|error| error.raw_os_error().unwrap_or(EIO))
    }
}

/// Selects the posture: `Unregistered` registers nothing; `Registered` and
/// `Auto` attempt the registration because the kernel's charge is
/// authoritative (`CAP_IPC_LOCK` exempts a ring outright, so the advisory
/// `getrlimit` reading alone decides nothing; the driver already downgraded
/// an `Auto` whose arena lock the same limit refused). `ENOMEM` degrades
/// `Auto` with the remediation and refuses an explicit `Registered` typed;
/// any other failure is the operating error it is.
fn select_posture(
    ring: &IoUring,
    frames: &Frames,
    write_arena: &ArenaState,
    registration_policy: RegistrationPolicy,
) -> Result<Posture, DriverBuildError> {
    if registration_policy == RegistrationPolicy::Unregistered {
        return Ok(Posture::Unregistered);
    }
    let error = match register_arenas(ring, frames, write_arena) {
        Ok(table) => return Ok(Posture::Registered(table)),
        Err(error) => error,
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

/// Registers both arenas as consecutive chunks of at most
/// [`REGISTERED_CHUNK_BYTES`]. An arena larger than the kernel's buffer table
/// can describe is refused like an over-limit charge (`ENOMEM`), so `Auto`
/// degrades to unregistered I/O and an explicit `Registered` is refused typed.
fn register_arenas(
    ring: &IoUring,
    frames: &Frames,
    write_arena: &ArenaState,
) -> std::io::Result<BufferTable> {
    let frame_chunks = BufferTable::chunks(frames.span_len());
    let write_chunks = BufferTable::chunks(write_arena.span_len());
    assert!(frame_chunks > 0, "the frame arena is non-empty");
    assert!(write_chunks > 0, "the write arena is non-empty");
    if frame_chunks + write_chunks > REGISTERED_BUFFERS_MAX {
        return Err(std::io::Error::from_raw_os_error(crate::memlock::ENOMEM));
    }
    let mut iov = Vec::new();
    iov.try_reserve_exact(frame_chunks + write_chunks)
        .map_err(|_| std::io::Error::from_raw_os_error(crate::memlock::ENOMEM))?;
    for (base, len) in [
        (frames.base_ptr(), frames.span_len()),
        (write_arena.base_ptr(), write_arena.span_len()),
    ] {
        for start in (0..len).step_by(REGISTERED_CHUNK_BYTES) {
            iov.push(Iovec {
                // SAFETY: `start < len`, so the chunk base stays inside the arena.
                iov_base: unsafe { base.add(start) }.cast(),
                iov_len: (len - start).min(REGISTERED_CHUNK_BYTES),
            });
        }
    }
    assert_eq!(
        iov.len(),
        frame_chunks + write_chunks,
        "one iovec per chunk"
    );
    // SAFETY: `Iovec` is layout-compatible with `libc::iovec` and the vector
    // is live for this slice borrow.
    let bufs = unsafe { std::slice::from_raw_parts(iov.as_ptr().cast(), iov.len()) };
    // SAFETY: both arenas keep fixed addresses and the ring drops before
    // them, so every registered chunk index remains valid for every op.
    unsafe { ring.submitter().register_buffers(bufs) }?;
    Ok(BufferTable {
        frames_base: frames.base_ptr().addr(),
        write_base: write_arena.base_ptr().addr(),
        write_first_index: u16::try_from(frame_chunks)
            .expect("bounded by the registered-buffer limit"),
    })
}

#[cfg(test)]
mod tests {
    use std::ptr::NonNull;

    use super::*;

    #[test]
    fn registered_posture_issues_fixed_buffer_opcodes() {
        let source = NonNull::<u8>::dangling().as_ptr();
        let table = BufferTable {
            frames_base: source.addr(),
            write_base: source.addr(),
            write_first_index: 1,
        };
        let write = Posture::Registered(table).write_entry(3, source, 4_096, 8_192);
        let read = Posture::Registered(table).read_entry(3, source, 4_096, 8_192);

        assert_eq!(
            write.get_opcode(),
            u32::from(opcode::WriteFixed::CODE),
            "ordinary WRITE does not prove the registered staging arena is used"
        );
        assert_eq!(read.get_opcode(), u32::from(opcode::ReadFixed::CODE));
    }

    #[test]
    fn chunk_indexes_follow_the_one_gibibyte_registration_boundary() {
        let table = BufferTable {
            frames_base: 1 << 40,
            write_base: 1 << 41,
            write_first_index: 3,
        };
        let frame = |offset: usize| std::ptr::without_provenance::<u8>((1 << 40) + offset);
        let slot = |offset: usize| std::ptr::without_provenance::<u8>((1 << 41) + offset);
        assert_eq!(BufferTable::chunks(4_096), 1);
        assert_eq!(BufferTable::chunks(REGISTERED_CHUNK_BYTES), 1);
        assert_eq!(BufferTable::chunks(REGISTERED_CHUNK_BYTES + 4_096), 2);
        assert_eq!(table.frame_index(frame(0)), 0);
        assert_eq!(table.frame_index(frame(REGISTERED_CHUNK_BYTES - 4_096)), 0);
        assert_eq!(table.frame_index(frame(REGISTERED_CHUNK_BYTES)), 1);
        assert_eq!(
            table.frame_index(frame(2 * REGISTERED_CHUNK_BYTES + 4_096)),
            2
        );
        assert_eq!(
            table.write_index(slot(0)),
            3,
            "the write arena follows the frame chunks"
        );
        assert_eq!(table.write_index(slot(REGISTERED_CHUNK_BYTES)), 4);
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

    #[test]
    #[cfg_attr(miri, ignore = "io_uring and mlock are foreign syscalls")]
    fn fsync_pushed_and_reaped_through_the_core_completes_its_token() {
        let driver = crate::driver::Driver::builder()
            .build()
            .expect("the io_uring driver initializes");
        let path = std::env::temp_dir().join(format!(
            "dios_uring_lock_witness_{}.bin",
            std::process::id()
        ));
        File::create(&path).expect("temp file");
        let fd = driver
            .open(&path, crate::DirectIo::Disabled)
            .expect("the driver opens the temp file");
        std::fs::remove_file(&path).expect("unlink temp file");
        let token = driver
            .submit_fsync(&fd, crate::driver::SyncMode::Data)
            .expect("submit within capacity");

        let mut out = crate::driver::CompletionBatch::with_capacity(1);
        assert_eq!(driver.poll_wait(&mut out, Duration::from_secs(5)), 1);
        let completion = out.iter().next().expect("one completion");
        assert_eq!(
            completion.token(),
            token,
            "the fsync CQE routes back to its submitted token"
        );
        assert_eq!(completion.result().ok(), Some(0));
    }
}
