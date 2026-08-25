//! The shared driver core and its public surface.
//!
//! The internal driver core owns everything a backend must not diverge on: the fd table
//! and its deferred-close-past-drain (INV-11), op-slot lease tracking, the
//! `EINTR`/`EAGAIN` retry policy and its init-time bound, submit admission
//! (`is_live` → reserve → fill → `on_submit`), and completion finalization
//! (reclaim, close progression, lease release, publish). It composes an
//! backend executor that lives outside the submit mutex, so the execute
//! phase (a real syscall on the eager/uring backends) runs without the lock
//! held; the mock synchronises its own injected state internally. Op routing is
//! never selected by matching a runtime tag (AD-1). Both [`Driver`] and the mock
//! compose the same core.
//!
//! Read placement is owned by [`crate::Pool`], not exposed as a caller-chosen
//! frame index on the advanced driver:
//!
//! ```compile_fail
//! use dios::driver::{Driver, ReadFrameIdx};
//! use dios::DirectIo;
//! let driver = Driver::builder().build().unwrap();
//! let path = std::env::temp_dir().join("dios_raw_read_surface.bin");
//! std::fs::write(&path, [0u8; 4096]).unwrap();
//! let file = driver.open(&path, DirectIo::Disabled).unwrap();
//! driver.submit_read(&file, ReadFrameIdx::new(0), 0).unwrap();
//! ```

use std::collections::VecDeque;
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

pub use crate::alignment::{Alignment, Unaligned};
use crate::backend;
pub use crate::completion::{Completion, CompletionBatch};
use crate::error::FileRegistrationError;
pub use crate::error::{IoError, SubmitError};
use crate::open::DirectIo;
use crate::pool::write_arena::{ArenaState, try_shared as try_shared_write_arena};
pub use crate::pool::write_arena::{WriteArena, WriteSlot};
use crate::pool::{Frames, ReadFrameIdx};
#[cfg(target_os = "linux")]
use crate::product::PlatformWaitOutcome;
use crate::product::WaitState;

pub(crate) const DEFAULT_REGISTERED_FILE_CAPACITY: u32 = 64;
const EINTR: i32 = 4;
const EIO: i32 = 5;
const EBADF: i32 = 9;
const EMFILE: i32 = 24;
#[cfg(target_os = "linux")]
const EAGAIN: i32 = 11;
#[cfg(not(target_os = "linux"))]
const EAGAIN: i32 = 35;
const POLL_WAIT_QUANTUM: Duration = Duration::from_millis(5);
const QUIESCE_IDLE_MAX: u32 = 1_000_000;

/// Distinguishes driver instances so a [`FileHandle`] minted by one core is
/// rejected by another whose fd slots happen to coincide. Bumped once per
/// [`DriverCore`] construction — never on the hot path.
static NEXT_DRIVER_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) fn next_driver_id() -> u64 {
    NEXT_DRIVER_ID.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn file_registration_error_into_io(error: FileRegistrationError) -> IoError {
    match error {
        FileRegistrationError::AtCapacity => IoError::from_raw(EMFILE),
        FileRegistrationError::Io(error) => error,
    }
}

/// A diagnostic probe of the compiled backend. Op routing binds to the
/// cfg-selected concrete type (AD-1), never by matching this at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    /// Portable backend: `submit` enqueues, `poll` runs the syscall inline.
    Eager,
    /// Linux `io_uring` backend.
    Uring,
}

/// The public driver over the cfg-selected backend, composing the same driver
/// core the mock uses so the two cannot structurally drift.
#[derive(Debug)]
pub struct Driver(DriverCore<backend::Impl>);

/// Builds a [`Driver`] with every capacity fixed up front.
#[derive(Debug, Clone, Copy)]
pub struct DriverBuilder {
    queue_capacity: u32,
    frames: u32,
    frame_bytes: u32,
    write_slots: u32,
    retry_bound: u32,
    registered_file_capacity: u32,
}

#[derive(Debug)]
pub(crate) enum DriverBuildError {
    Allocation,
    #[cfg(target_os = "linux")]
    Driver(IoError),
}

impl Default for DriverBuilder {
    fn default() -> Self {
        Self {
            queue_capacity: 1,
            frames: 1,
            frame_bytes: 4096,
            write_slots: 1,
            retry_bound: 0,
            registered_file_capacity: DEFAULT_REGISTERED_FILE_CAPACITY,
        }
    }
}

impl DriverBuilder {
    /// Sets the maximum number of operations in flight.
    #[must_use]
    pub fn queue_capacity(mut self, queue_capacity: u32) -> Self {
        self.queue_capacity = queue_capacity;
        self
    }

    /// Sets the number of fixed read frames.
    #[must_use]
    pub fn frames(mut self, frames: u32) -> Self {
        self.frames = frames;
        self
    }

    /// Sets each read frame's byte length.
    #[must_use]
    pub fn frame_bytes(mut self, frame_bytes: u32) -> Self {
        self.frame_bytes = frame_bytes;
        self
    }

    /// Sets the fixed number of granule-sized write staging slots.
    #[must_use]
    pub fn write_slots(mut self, write_slots: u32) -> Self {
        self.write_slots = write_slots;
        self
    }

    /// Sets the maximum retries for interrupted or retryable operations.
    #[must_use]
    pub fn retry_bound(mut self, retry_bound: u32) -> Self {
        self.retry_bound = retry_bound;
        self
    }

    pub(crate) fn registered_file_capacity(mut self, registered_file_capacity: u32) -> Self {
        self.registered_file_capacity = registered_file_capacity;
        self
    }

    /// # Errors
    ///
    /// Returns [`std::io::ErrorKind::OutOfMemory`] if fixed storage cannot be
    /// allocated, or the operating failure if the selected backend cannot
    /// initialize or register its fixed resources.
    ///
    /// # Panics
    ///
    /// If any capacity is zero — capacities are fixed and positive at init.
    pub fn build(self) -> Result<Driver, IoError> {
        assert!(self.queue_capacity > 0, "queue capacity must be positive");
        assert!(self.frames > 0, "frame count must be positive");
        assert!(self.frame_bytes > 0, "frame size must be positive");
        assert!(self.write_slots > 0, "write slot count must be positive");
        let result = match Frames::try_preallocated(self.frames, self.frame_bytes) {
            Some(frames) => self.build_with_frames(Arc::new(frames)),
            None => Err(DriverBuildError::Allocation),
        };
        match result {
            Ok(driver) => Ok(driver),
            Err(DriverBuildError::Allocation) => Err(IoError::from(std::io::Error::from(
                std::io::ErrorKind::OutOfMemory,
            ))),
            #[cfg(target_os = "linux")]
            Err(DriverBuildError::Driver(error)) => Err(error),
        }
    }

    pub(crate) fn build_with_frames(self, frames: Arc<Frames>) -> Result<Driver, DriverBuildError> {
        assert_eq!(
            frames.count(),
            self.frames,
            "driver and arena frame counts match"
        );
        assert_eq!(
            frames.granule(),
            self.frame_bytes,
            "driver and arena frame sizes match"
        );
        let id = next_driver_id();
        let write_arena = try_shared_write_arena(self.write_slots, self.frame_bytes, id)
            .ok_or(DriverBuildError::Allocation)?;
        let shared = Shared::try_new(self.queue_capacity, self.registered_file_capacity)
            .ok_or(DriverBuildError::Allocation)?;
        let executor = backend::Impl::new(
            frames,
            Arc::clone(&write_arena),
            self.queue_capacity,
            self.registered_file_capacity,
        )?;
        let core = DriverCore::try_new(
            shared,
            executor,
            write_arena,
            self.frames,
            self.retry_bound,
            self.queue_capacity,
            id,
        )
        .ok_or(DriverBuildError::Allocation)?;
        Ok(Driver(core))
    }
}

impl Driver {
    /// The backend selected for the target platform.
    pub const BACKEND: Backend = backend::Impl::KIND;

    /// Returns a builder with minimal fixed capacities.
    #[must_use]
    pub fn builder() -> DriverBuilder {
        DriverBuilder::default()
    }

    /// Borrows this driver's fixed, registered write-staging arena.
    #[must_use]
    pub fn write_arena(&self) -> WriteArena<'_> {
        WriteArena::new(self)
    }

    pub(crate) fn alloc_write_slot(&self) -> Option<WriteSlot<'_>> {
        self.0.write_arena.alloc()
    }

    pub(crate) fn identity(&self) -> u64 {
        self.0.id
    }

    pub(crate) fn write_arena_state(&self) -> &ArenaState {
        self.0.write_arena_state()
    }

    pub(crate) fn attach_pool_wait(&self, wait: Arc<WaitState>) {
        self.0.attach_pool_wait(wait);
    }

    pub(crate) fn alloc_write_slot_wait(&self, timeout: Duration) -> Option<WriteSlot<'_>> {
        #[cfg(target_os = "linux")]
        {
            self.0.alloc_write_slot_wait_ring(timeout)
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.0.alloc_write_slot_wait(timeout)
        }
    }

    /// Opens an existing file read-write, applying the requested direct-I/O
    /// policy; the outcome rides in the handle's [`FileHandle::io_mode`] as an
    /// observable enum (scope Constraints). No create mode in v1.
    ///
    /// # Errors
    ///
    /// The open syscall's operating failure (`ENOENT`, `EACCES`, …), or `EMFILE`
    /// when the fixed fd table is exhausted.
    pub fn open(&self, path: &Path, direct_io: DirectIo) -> Result<FileHandle, IoError> {
        self.open_with_direct_io(path, direct_io)
            .map_err(file_registration_error_into_io)
    }

    pub(crate) fn open_with_direct_io(
        &self,
        path: &Path,
        direct_io: DirectIo,
    ) -> Result<FileHandle, FileRegistrationError> {
        let id = self
            .0
            .reserve_file()
            .ok_or(FileRegistrationError::AtCapacity)?;
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
        {
            Ok(file) => file,
            Err(error) => {
                self.0.abort_file(id);
                return Err(FileRegistrationError::Io(IoError::from(error)));
            }
        };
        let arena_granule = self.0.executor().clean_bytes(OpKind::Read);
        let io_mode = match crate::open::probe(&file, direct_io, arena_granule) {
            Ok(mode) => mode,
            Err(error) => {
                self.0.abort_file(id);
                return Err(FileRegistrationError::Io(error));
            }
        };
        if let Err(error) = self.0.executor().register_file(id.slot(), file) {
            self.0.abort_file(id);
            return Err(FileRegistrationError::Io(error));
        }
        Ok(FileHandle::from_parts(id, io_mode))
    }

    pub(crate) fn create_with_direct_io(
        &self,
        path: &Path,
        direct_io: DirectIo,
    ) -> Result<FileHandle, FileRegistrationError> {
        let id = self
            .0
            .reserve_file()
            .ok_or(FileRegistrationError::AtCapacity)?;
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(file) => file,
            Err(error) => {
                self.0.abort_file(id);
                return Err(FileRegistrationError::Io(IoError::from(error)));
            }
        };
        let arena_granule = self.0.executor().clean_bytes(OpKind::Read);
        let io_mode = match crate::open::probe(&file, direct_io, arena_granule) {
            Ok(mode) => mode,
            Err(error) => {
                self.0.abort_file(id);
                return Err(FileRegistrationError::Io(error));
            }
        };
        if let Err(error) = self.0.executor().register_file(id.slot(), file) {
            self.0.abort_file(id);
            return Err(FileRegistrationError::Io(error));
        }
        Ok(FileHandle::from_parts(id, io_mode))
    }

    /// Enqueues a read into `frame` at `offset`; the eager backend performs the
    /// pread inline at [`Driver::poll`] (AD-7).
    ///
    /// # Errors
    ///
    /// [`SubmitError::StaleHandle`] for a stale fd, [`SubmitError::Full`] at
    /// capacity.
    ///
    /// # Panics
    ///
    /// If `fd` was minted by a different driver, if `frame` is out of range, or a
    /// direct handle receives a misaligned `offset` — each rejected before the op
    /// is issued.
    pub(crate) fn submit_read_internal(
        &self,
        fd: &FileHandle,
        frame: ReadFrameIdx,
        offset: u64,
    ) -> Result<OpToken, SubmitError> {
        let len = self.0.executor().clean_bytes(OpKind::Read);
        self.0.submit_read(fd, frame, offset, 0, len, true)
    }

    pub(crate) fn submit_read_range(
        &self,
        fd: &FileHandle,
        frame: ReadFrameIdx,
        file_offset: u64,
        destination_offset: u32,
        requested_len: u32,
    ) -> Result<OpToken, SubmitError> {
        self.0.submit_read(
            fd,
            frame,
            file_offset,
            destination_offset,
            requested_len,
            false,
        )
    }

    /// Enqueues a write from `buf`, transferring its ownership to the driver
    /// until completion drain.
    ///
    /// # Errors
    ///
    /// Returns the refusal and the unchanged slot when the handle is stale or
    /// the bounded submission queue is full.
    pub fn submit_write<'arena>(
        &self,
        fd: &FileHandle,
        buf: WriteSlot<'arena>,
        offset: u64,
    ) -> Result<OpToken, (SubmitError, WriteSlot<'arena>)> {
        self.0.submit_write(fd, buf, offset)
    }

    /// Enqueues an fsync barrier; the eager backend runs it inline at
    /// [`Driver::poll`].
    ///
    /// # Errors
    ///
    /// [`SubmitError::StaleHandle`] for a stale fd, [`SubmitError::Full`] at
    /// capacity.
    ///
    /// # Panics
    ///
    /// If `fd` was minted by a different driver.
    pub fn submit_fsync(&self, fd: &FileHandle, mode: SyncMode) -> Result<OpToken, SubmitError> {
        self.0.submit_fsync(fd, mode)
    }

    /// Executes queued ops inline on the calling thread (AD-7) and drains their
    /// completions into `out`, returning the count.
    ///
    /// # Panics
    ///
    /// If `out` has zero capacity.
    pub fn poll(&self, out: &mut CompletionBatch) -> usize {
        #[cfg(target_os = "linux")]
        {
            self.0.poll_ring(out)
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.0.poll(out)
        }
    }

    pub(crate) fn poll_progress_for_pool(&self, out: &mut CompletionBatch) -> BackendProgress {
        #[cfg(target_os = "linux")]
        {
            self.0.poll_ring_progress(out)
        }
        #[cfg(not(target_os = "linux"))]
        {
            BackendProgress::from_terminal(self.0.poll(out))
        }
    }

    /// Drains completions like [`Driver::poll`], parking in the kernel for up to
    /// `timeout` when none are ready. The wait runs outside the AD-4 submit mutex
    /// (INV-3), so a concurrent pool read admission never blocks on the poller.
    ///
    /// # Panics
    ///
    /// If `out` has zero capacity.
    pub fn poll_wait(&self, out: &mut CompletionBatch, timeout: Duration) -> usize {
        #[cfg(target_os = "linux")]
        {
            self.0.poll_wait_ring(out, timeout)
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.0.poll_wait(out, timeout)
        }
    }

    pub(crate) fn poll_wait_for_pool(
        &self,
        out: &mut CompletionBatch,
        timeout: Duration,
    ) -> BackendProgress {
        #[cfg(target_os = "linux")]
        {
            self.0.poll_wait_ring_for_pool(out, timeout)
        }
        #[cfg(not(target_os = "linux"))]
        {
            BackendProgress::from_terminal(self.0.poll_wait_eager_for_pool(out, timeout))
        }
    }

    /// Closes `fd`, deferring the underlying `close(2)` past the drain of its
    /// in-flight ops (INV-11). Consumes the handle so it cannot be reused.
    pub fn close(&self, fd: FileHandle) {
        self.0.close(fd);
    }

    /// Whether the fd named by `id` has reached the closed state — its deferred
    /// close completed after the last in-flight op drained. Takes the retained
    /// [`FileId`] because [`Driver::close`] consumes the handle.
    ///
    /// # Panics
    ///
    /// If `id` was minted by a different driver.
    #[must_use]
    pub fn is_closed(&self, id: FileId) -> bool {
        self.0.is_closed(id)
    }

    /// Blocking metadata-plane write (AD-3): loops past short transfers until the
    /// whole buffer lands, retrying `EINTR` up to the init-time bound.
    ///
    /// # Errors
    ///
    /// `EBADF` on a stale/closed handle, or the write syscall's operating failure.
    ///
    /// # Panics
    ///
    /// If `fd` was minted by a different driver, or the buffer length exceeds
    /// `u32::MAX`.
    pub fn write_all_blocking(
        &self,
        fd: &FileHandle,
        buf: &[u8],
        offset: u64,
    ) -> Result<(), IoError> {
        #[cfg(target_os = "linux")]
        {
            self.0.write_all_blocking_ring(fd, buf, offset)
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.0.write_all_blocking(fd, buf, offset)
        }
    }

    /// Blocking metadata-plane fsync barrier (`F_FULLFSYNC` on darwin).
    ///
    /// # Errors
    ///
    /// `EBADF` on a stale/closed handle, or the fsync syscall's operating failure.
    ///
    /// # Panics
    ///
    /// If `fd` was minted by a different driver.
    pub fn fsync_blocking(&self, fd: &FileHandle, mode: SyncMode) -> Result<(), IoError> {
        #[cfg(target_os = "linux")]
        {
            self.0.fsync_blocking_ring(fd, mode)
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.0.fsync_blocking(fd, mode)
        }
    }

    #[cfg(any(feature = "mock", feature = "bench"))]
    pub(crate) fn copy_frame_testing(&self, frame: ReadFrameIdx, out: &mut [u8]) -> usize {
        self.0.copy_frame_testing(frame, out)
    }
}

/// Drains every admitted operation before backend resources are released
/// (INV-8). Ring operations may already be kernel-visible; eager operations
/// remain in the bounded ready queue until this teardown poll executes them.
impl Drop for Driver {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        self.0.quiesce_ring();
        #[cfg(not(target_os = "linux"))]
        self.0.quiesce();
    }
}

/// The three op kinds the driver issues; echoed in each completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind {
    /// A positional read into a fixed frame.
    Read,
    /// A positional write from a leased staging slot.
    Write,
    /// A durability barrier.
    Fsync,
}

/// A completion-slab slot plus its generation, issued by submit and echoed in
/// the completion. A reused slot carries a bumped generation, so a stale token
/// never aliases a live op (ABA-safe).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpToken(u64);

impl OpToken {
    pub(crate) fn new(slot: u32, generation: u32) -> Self {
        Self((u64::from(generation) << 32) | u64::from(slot))
    }

    /// The completion-slab slot this token names — the ring's `user_data`.
    pub(crate) fn slot(self) -> u32 {
        u32::try_from(self.0 & u64::from(u32::MAX)).expect("the low word is a u32 slot")
    }
}

/// Durability barrier requested by an fsync op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyncMode {
    /// Flush file data and metadata through the device barrier.
    Full,
}

/// How a file's data plane transfers: direct with a probed sector [`Alignment`],
/// or buffered through the page cache. An observable enum, never a silent bool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IoMode {
    /// Direct I/O with its probed alignment.
    Direct(Alignment),
    /// Buffered I/O through the operating-system page cache.
    Buffered,
}

/// A generational file identity: the originating driver's instance id, an
/// fd-table slot, and the generation live when the handle was minted. The
/// representation stays opaque; slot reuse is only observable through
/// [`FileId::aliases_slot`], and only within the driver that minted both ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId {
    driver: u64,
    slot: u32,
    generation: u32,
}

impl FileId {
    pub(crate) fn new(driver: u64, slot: u32, generation: u32) -> Self {
        Self {
            driver,
            slot,
            generation,
        }
    }

    pub(crate) fn driver(self) -> u64 {
        self.driver
    }

    pub(crate) fn slot(self) -> u32 {
        self.slot
    }

    pub(crate) fn generation(self) -> u32 {
        self.generation
    }

    /// Whether `other` names the same fd-table slot, regardless of generation —
    /// the predicate that makes slot reuse observable without exposing the
    /// representation.
    ///
    /// # Panics
    ///
    /// If `self` and `other` were minted by different drivers — a cross-driver
    /// comparison is a caller bug.
    #[must_use]
    pub fn aliases_slot(&self, other: &FileId) -> bool {
        assert_eq!(
            self.driver, other.driver,
            "aliases_slot across different drivers is a caller bug"
        );
        self.slot == other.slot
    }
}

/// A driver-owned open file. `!Copy`: `close` consumes it, and test-only fault
/// injection is the sole source of duplicate handles.
#[derive(Debug)]
pub struct FileHandle {
    id: FileId,
    io_mode: IoMode,
}

impl FileHandle {
    pub(crate) fn from_id(id: FileId) -> Self {
        Self {
            id,
            io_mode: IoMode::Buffered,
        }
    }

    pub(crate) fn from_parts(id: FileId, io_mode: IoMode) -> Self {
        Self { id, io_mode }
    }

    #[must_use]
    /// Returns the generational identity used by [`crate::PageId`].
    pub fn file_id(&self) -> FileId {
        self.id
    }

    /// The direct/buffered mode probed for this file at open (scope Constraints).
    #[must_use]
    pub fn io_mode(&self) -> IoMode {
        self.io_mode
    }
}

/// Fixed-capacity op slab shared by every backend: slots acquired at submit
/// (minting an [`OpToken`] = slot + generation), reclaimed at completion drain.
/// Generation bumps on each fill, so a reused slot never aliases a stale token
/// (INV-11). Capacity is set at construction; there is no growth path.
#[derive(Debug)]
pub(crate) struct CompletionSlab<T> {
    slots: Box<[SlabSlot<T>]>,
    free: Vec<u32>,
}

#[derive(Debug)]
struct SlabSlot<T> {
    generation: u32,
    payload: Option<T>,
}

impl<T> CompletionSlab<T> {
    pub(crate) fn try_with_capacity(capacity: u32) -> Option<Self> {
        let slots = crate::allocation::try_boxed_slice_with(capacity, || SlabSlot {
            generation: 0,
            payload: None,
        })?;
        let mut free = crate::allocation::try_vec_with_exact_capacity(capacity)?;
        for slot in (0..capacity).rev() {
            free.push(slot);
        }
        Some(Self { slots, free })
    }

    pub(crate) fn reserve(&mut self) -> Option<u32> {
        self.free.pop()
    }

    pub(crate) fn fill(&mut self, slot: u32, payload: T) -> OpToken {
        let entry = &mut self.slots[slot as usize];
        debug_assert!(entry.payload.is_none(), "free slot must be empty");
        assert!(entry.generation < u32::MAX, "op slot generation exhausted");
        entry.generation += 1;
        entry.payload = Some(payload);
        OpToken::new(slot, entry.generation)
    }

    /// Borrows an occupied slot's payload without reclaiming it — the prepare
    /// phase reads the op kind while the slot stays live through execution.
    pub(crate) fn peek(&self, slot: u32) -> &T {
        self.slots[slot as usize]
            .payload
            .as_ref()
            .expect("peek of an occupied slot")
    }

    /// Mutably borrows an occupied slot's payload without reclaiming it — the ring
    /// reap path bumps an op's retry count while the op stays live across CQEs.
    pub(crate) fn peek_mut(&mut self, slot: u32) -> &mut T {
        self.slots[slot as usize]
            .payload
            .as_mut()
            .expect("peek of an occupied slot")
    }

    pub(crate) fn reclaim(&mut self, slot: u32) -> (OpToken, T) {
        let entry = &mut self.slots[slot as usize];
        let token = OpToken::new(slot, entry.generation);
        let payload = entry.payload.take().expect("reclaim of an occupied slot");
        self.free.push(slot);
        (token, payload)
    }
}

/// One raw syscall attempt a backend simulates or performs. The core's retry
/// loop, not the backend, decides whether an interruption or would-block is
/// resubmitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Attempt {
    /// A transfer of `bytes` — a clean completion or a short one.
    Done(u32),
    /// A hard operating failure carrying `errno`.
    Failed(i32),
    /// `EINTR`: resubmit on every op up to the bound.
    Interrupted,
    /// `EAGAIN`: resubmit on reads up to the bound, surface on writes.
    WouldBlock,
}

/// The lifecycle seam every backend shares (generics only, no dyn — AD-1): op
/// sizing, ready-order placement, and fd retirement. The per-op execute path is
/// split out — [`EagerExecutor`] for the attempt-based backends, [`RingExecutor`]
/// for `io_uring` — so no backend conforms to an execute shape it never runs.
/// All methods take `&self`.
pub(crate) trait Executor {
    /// The transfer byte count a clean `kind` op reports.
    fn clean_bytes(&self, kind: OpKind) -> u32;

    /// Retains `file` in the fd `slot`, registering it with any backend resource
    /// reads address it through (the ring's fixed-file table on `io_uring`).
    ///
    /// # Errors
    ///
    /// A backend registration failure, surfaced rather than retaining an unusable
    /// descriptor. The eager and mock backends never fail.
    fn register_file(&self, slot: u32, file: File) -> Result<(), IoError>;

    fn try_reconfigure_file_capacity(&mut self, _file_capacity: u32) -> Option<()> {
        Some(())
    }

    /// The position at which a freshly admitted op joins the ready order given
    /// the current queue length (seeded reordering in the mock; a real backend
    /// appends at `ready_len`).
    fn schedule(&self, ready_len: usize) -> usize;

    /// Releases the backend state an fd slot holds, called once the core advances
    /// it `Closing → Closed` in publish (its last in-flight op drained). Eager
    /// drops the retained `File`, closing the descriptor and freeing the slot for
    /// reuse; the mock keeps no per-file state and no-ops.
    fn retire_file(&self, slot: u32);

    fn on_op_submitted(&self) {}

    fn on_op_completed(&self, _file: FileId, _kind: OpKind, _result: &Result<u32, IoError>) {}

    fn on_file_closed(&self, _file: FileId) {}

    fn on_quiesce(&self) {}

    #[cfg(target_os = "linux")]
    fn attach_pool_wait(&self, _wait: &Arc<WaitState>) {}

    #[cfg(any(feature = "mock", feature = "bench"))]
    fn copy_frame(&self, frame: ReadFrameIdx, out: &mut [u8]) -> usize;
}

/// The eager-shaped execute seam: a per-op syscall attempt the shared retry loop
/// drives outside the submit mutex. The eager backend performs the real syscall;
/// the mock replays seeded faults. The `io_uring` backend does not implement it —
/// it fills SQEs through [`RingExecutor`] — so no backend fakes an attempt path.
pub(crate) trait EagerExecutor: Executor {
    /// One attempt at the op named by `kind`; `clean_bytes` is the transfer a
    /// fault-free op reports and `context` carries the file, offset, and target
    /// resource. Runs in the execute phase, no submit lock held.
    fn attempt(&self, kind: OpKind, clean_bytes: u32, context: OpContext<'_>) -> Attempt;
}

/// The batched ring seam the `io_uring` backend composes over: SQE fill and CQE
/// reap run under the AD-4 mutex, the kernel wait runs outside it (INV-3). The
/// eager per-op [`EagerExecutor::attempt`] path is bypassed entirely. Portable so
/// the seeded mock ring composes the same `DriverCore` reap path off-linux; only
/// the `io_uring` implementation is Linux-gated.
pub(crate) trait RingExecutor: Executor {
    /// Fills one read SQE addressing the registered buffer for `frame` at
    /// `file_offset`, tagged with `user_data`. Called under the AD-4 mutex.
    fn push_read(
        &self,
        user_data: u64,
        fd_slot: u32,
        frame: ReadFrameIdx,
        file_offset: u64,
        destination_offset: u32,
        requested_len: u32,
    );

    /// Fills one write SQE from a leased staging slot. The lease remains in the
    /// completion slab until this op is terminally reaped.
    fn push_write(
        &self,
        user_data: u64,
        fd_slot: u32,
        source: *const u8,
        source_offset: u32,
        file_offset: u64,
        requested_len: u32,
    );

    /// Fills one fsync SQE, tagged with `user_data`. Called under the AD-4 mutex.
    fn push_fsync(&self, user_data: u64, fd_slot: u32);

    /// Submits filled SQEs without blocking on completions (`min_complete = 0`),
    /// so poll never sleeps awaiting events. Runs OUTSIDE the AD-4 mutex.
    fn submit(&self);

    /// Submits filled SQEs and parks up to `timeout` for at least `want`
    /// completions via the `EXT_ARG` kernel wait. Runs OUTSIDE the AD-4 mutex.
    fn submit_and_wait(&self, want: u32, timeout: Duration);

    /// Drains at most `limit` ready operation CQEs, routing each
    /// `(user_data, raw_result)` to `sink`. The private wake CQE is excluded
    /// from `backend_completions` and reported only through `rearm_needed`.
    /// Called under the AD-4 mutex and must not enter the kernel.
    fn reap<F: FnMut(u64, i32) -> bool>(&self, limit: u32, sink: F) -> RingReap;

    /// Enqueues a fresh private wake poll after reap consumed the prior one.
    /// Called under the AD-4 mutex; the core submits it only after unlocking.
    fn rearm_after_reap(&self) {}

    /// Fires once per op finalized in reap — never for a resubmitted
    /// `EAGAIN`/`EINTR` CQE, which keeps the op live. Defaults to a no-op.
    fn on_op_finalized(&self) {}

    /// Metadata-plane blocking write on the retained file (AD-3): `Ok(bytes)` or
    /// `Err(errno)`. Linux-only — the metadata plane rides only the real ring.
    #[cfg(target_os = "linux")]
    fn blocking_write(&self, fd_slot: u32, buf: &[u8], offset: u64) -> Result<u32, i32>;

    /// Metadata-plane blocking fsync barrier on the retained file (AD-3).
    #[cfg(target_os = "linux")]
    fn blocking_fsync(&self, fd_slot: u32) -> Result<(), i32>;
}

/// Raw progress from one ring reap. Retryable CQEs count even though they do
/// not emit a terminal caller completion; the private wake CQE never counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RingReap {
    pub(crate) backend_completions: u32,
    pub(crate) rearm_needed: bool,
}

/// Private Pool-facing progress. The advanced Driver API continues to report
/// only terminal caller completions, while Pool reports every backend CQE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BackendProgress {
    pub(crate) caller_completions: usize,
    pub(crate) backend_completions: u32,
}

impl BackendProgress {
    pub(crate) fn from_terminal(caller_completions: usize) -> Self {
        Self {
            caller_completions,
            backend_completions: u32::try_from(caller_completions)
                .expect("completion batch capacity fits u32"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct OpEntry {
    kind: OpKind,
    fd: FileId,
    file_offset: u64,
    frame: ReadFrameIdx,
    destination_offset: u32,
    requested_len: u32,
    raw_frame_lease: bool,
    write_slot: Option<u32>,
    retries: u32,
}

/// Per-op parameters the execute phase hands a backend: which file, at what
/// offset, into (reads) or out of (writes) which resource. `destination_offset`
/// is the read destination or write source offset within the fixed buffer. The
/// eager backend performs the real syscall; mocks replay injected outcomes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OpContext<'buf> {
    pub(crate) fd: FileId,
    pub(crate) file_offset: u64,
    pub(crate) frame: ReadFrameIdx,
    pub(crate) destination_offset: u32,
    pub(crate) requested_len: u32,
    pub(crate) write_buf: &'buf [u8],
}

impl<'buf> OpContext<'buf> {
    fn read(
        fd: FileId,
        file_offset: u64,
        frame: ReadFrameIdx,
        destination_offset: u32,
        requested_len: u32,
    ) -> Self {
        Self {
            fd,
            file_offset,
            frame,
            destination_offset,
            requested_len,
            write_buf: &[],
        }
    }

    fn write(fd: FileId, file_offset: u64, source_offset: u32, write_buf: &'buf [u8]) -> Self {
        let requested_len =
            u32::try_from(write_buf.len()).expect("write length fits the driver bound");
        Self {
            fd,
            file_offset,
            frame: ReadFrameIdx::new(0),
            destination_offset: source_offset,
            requested_len,
            write_buf,
        }
    }

    fn fsync(fd: FileId) -> Self {
        Self {
            fd,
            file_offset: 0,
            frame: ReadFrameIdx::new(0),
            destination_offset: 0,
            requested_len: 0,
            write_buf: &[],
        }
    }

    fn advance_write(self, bytes: u32) -> Self {
        assert!(bytes > 0, "a write tail advances by positive progress");
        assert!(bytes < self.requested_len, "only a short write has a tail");
        let start = bytes as usize;
        Self::write(
            self.fd,
            self.file_offset + u64::from(bytes),
            self.destination_offset + bytes,
            &self.write_buf[start..],
        )
    }
}

/// State guarded by the submit mutex: the op slab, the fd table, and the
/// seed-ordered ready queue. The backend executor is deliberately *not* here —
/// it sits on [`DriverCore`] so the execute phase never holds this lock.
#[derive(Debug)]
pub(crate) struct Shared {
    slab: CompletionSlab<OpEntry>,
    files: FileTable,
    ready: VecDeque<u32>,
    completion_backlog: VecDeque<Completion>,
}

impl Shared {
    pub(crate) fn try_new(ready_capacity: u32, file_capacity: u32) -> Option<Self> {
        Some(Self {
            slab: CompletionSlab::try_with_capacity(ready_capacity)?,
            files: FileTable::try_with_capacity(file_capacity)?,
            ready: crate::allocation::try_vec_deque_with_exact_capacity(ready_capacity)?,
            completion_backlog: crate::allocation::try_vec_deque_with_exact_capacity(
                ready_capacity,
            )?,
        })
    }
}

/// The shared driver composition. The AD-4 lock boundary is structural and the
/// phase order is fixed: locked prepare (pop ready work, no reclaim) → execute
/// (retry loop over the backend, submit mutex *not* held) → locked publish
/// (reclaim the slab slot, progress a deferred close, release the lease, emit).
/// A slab slot is reclaimed only in publish, after its final execution attempt,
/// so unlocking around execute never permits premature slot reuse. The idle
/// wait in [`DriverCore::poll_wait`] also parks outside the lock (INV-3).
#[derive(Debug)]
pub(crate) struct DriverCore<E> {
    inner: Mutex<Shared>,
    retire: Mutex<Vec<FileId>>,
    shutdown_batch: Mutex<CompletionBatch>,
    executor: E,
    write_arena: Arc<ArenaState>,
    frames: u32,
    retry_bound: u32,
    queue_capacity: u32,
    raw_read_inflight: Box<[AtomicBool]>,
    pool_wait: OnceLock<Arc<WaitState>>,
    id: u64,
}

impl<E> DriverCore<E> {
    pub(crate) fn try_new(
        shared: Shared,
        executor: E,
        write_arena: Arc<ArenaState>,
        frames: u32,
        retry_bound: u32,
        queue_capacity: u32,
        id: u64,
    ) -> Option<Self> {
        assert!(frames > 0, "driver core frame count must be positive");
        assert!(
            queue_capacity > 0,
            "driver core queue capacity must be positive"
        );
        let file_capacity =
            u32::try_from(shared.files.slots.len()).expect("the configured file capacity fits u32");
        Some(Self {
            inner: Mutex::new(shared),
            retire: Mutex::new(crate::allocation::try_vec_with_exact_capacity(
                file_capacity,
            )?),
            shutdown_batch: Mutex::new(CompletionBatch::try_with_capacity(queue_capacity)?),
            executor,
            write_arena,
            frames,
            retry_bound,
            queue_capacity,
            raw_read_inflight: crate::allocation::try_boxed_slice_with(frames, || {
                AtomicBool::new(false)
            })?,
            pool_wait: OnceLock::new(),
            id,
        })
    }

    fn inflight_total(&self) -> u32 {
        self.lock().files.total_inflight()
    }

    pub(crate) fn executor(&self) -> &E {
        &self.executor
    }

    pub(crate) fn identity(&self) -> u64 {
        self.id
    }

    pub(crate) fn write_arena_state(&self) -> &ArenaState {
        &self.write_arena
    }

    pub(crate) fn attach_pool_wait(&self, wait: Arc<WaitState>)
    where
        E: Executor,
    {
        #[cfg(target_os = "linux")]
        self.executor.attach_pool_wait(&wait);
        if let Err(wait) = self.pool_wait.set(wait) {
            assert!(
                Arc::ptr_eq(
                    self.pool_wait
                        .get()
                        .expect("an occupied pool wait slot has a value"),
                    &wait,
                ),
                "one Driver can be attached to only one Pool"
            );
        }
    }

    fn signal_pool_wait(&self) {
        if let Some(wait) = self.pool_wait.get() {
            wait.wake();
        }
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn assert_own(&self, id: FileId) {
        assert_eq!(
            id.driver(),
            self.id,
            "file handle used with a foreign driver"
        );
    }

    pub(crate) fn reserve_file(&self) -> Option<FileId> {
        self.lock().files.open(self.id)
    }

    pub(crate) fn is_closed(&self, id: FileId) -> bool {
        self.assert_own(id);
        self.lock().files.is_closed(id)
    }

    /// Releases a slot reserved by [`DriverCore::reserve_file`] whose backend
    /// registration then failed, returning it to `Free` for reuse.
    pub(crate) fn abort_file(&self, id: FileId) {
        self.assert_own(id);
        self.lock().files.abort_open(id);
    }
}

impl<E: Executor> DriverCore<E> {
    pub(crate) fn try_reconfigure_file_capacity(&mut self, file_capacity: u32) -> Option<()> {
        let files = self
            .inner
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .files
            .try_resized(file_capacity)?;
        let retire = crate::allocation::try_vec_with_exact_capacity(file_capacity)?;
        self.executor.try_reconfigure_file_capacity(file_capacity)?;
        self.inner
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .files = files;
        let prior_retire = self
            .retire
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        assert!(
            prior_retire.is_empty(),
            "construction has no pending retirements"
        );
        *prior_retire = retire;
        Some(())
    }

    #[cfg(any(feature = "mock", feature = "bench"))]
    fn copy_frame_testing(&self, frame: ReadFrameIdx, out: &mut [u8]) -> usize {
        let shared = self.lock();
        let mutating = shared.slab.slots.iter().any(|slot| {
            slot.payload
                .as_ref()
                .is_some_and(|entry| entry.kind == OpKind::Read && entry.frame == frame)
        });
        assert!(
            !mutating,
            "a test observation never aliases an in-flight frame mutation"
        );
        let copied = self.executor.copy_frame(frame, out);
        drop(shared);
        copied
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "close consumes the handle by value so it cannot be reused (INV-11)"
    )]
    pub(crate) fn close(&self, fd: FileHandle) {
        let id = fd.file_id();
        self.assert_own(id);
        let retire_due = {
            let mut shared = self.lock();
            shared.files.close(id)
        };
        if retire_due {
            self.retire(id);
        }
    }

    fn retire(&self, fd: FileId) {
        self.executor.retire_file(fd.slot());
        self.lock().files.finish_retire(fd);
        self.executor.on_file_closed(fd);
    }

    pub(crate) fn submit_read(
        &self,
        fd: &FileHandle,
        frame: ReadFrameIdx,
        file_offset: u64,
        destination_offset: u32,
        requested_len: u32,
        raw_frame_lease: bool,
    ) -> Result<OpToken, SubmitError> {
        assert!(frame.get() < self.frames, "read frame index out of range");
        let frame_bytes = self.executor.clean_bytes(OpKind::Read);
        assert!(requested_len > 0, "read length must be positive");
        assert!(
            destination_offset <= frame_bytes,
            "read destination starts within its frame"
        );
        assert!(
            requested_len <= frame_bytes - destination_offset,
            "read destination range lies within its frame"
        );
        if let IoMode::Direct(alignment) = fd.io_mode() {
            assert!(
                alignment.check(file_offset).is_ok(),
                "offset {file_offset} is not aligned to {} bytes",
                alignment.get()
            );
        }
        if raw_frame_lease {
            let was_inflight =
                self.raw_read_inflight[frame.get() as usize].swap(true, Ordering::AcqRel);
            assert!(
                !was_inflight,
                "a raw read frame has at most one op in flight"
            );
        }
        let mut shared = self.lock();
        let slot = match self.admit(&mut shared, fd.file_id()) {
            Ok(slot) => slot,
            Err(error) => {
                if raw_frame_lease {
                    self.raw_read_inflight[frame.get() as usize].store(false, Ordering::Release);
                }
                return Err(error);
            }
        };
        let entry = OpEntry {
            kind: OpKind::Read,
            fd: fd.file_id(),
            file_offset,
            frame,
            destination_offset,
            requested_len,
            raw_frame_lease,
            write_slot: None,
            retries: 0,
        };
        Ok(self.commit(&mut shared, slot, entry))
    }

    pub(crate) fn submit_write<'arena>(
        &self,
        fd: &FileHandle,
        buf: WriteSlot<'arena>,
        offset: u64,
    ) -> Result<OpToken, (SubmitError, WriteSlot<'arena>)> {
        buf.assert_owner(self.id);
        if let IoMode::Direct(alignment) = fd.io_mode() {
            assert!(
                alignment.check(offset).is_ok(),
                "offset {offset} is not aligned to {} bytes",
                alignment.get()
            );
        }
        let mut shared = self.lock();
        match self.admit(&mut shared, fd.file_id()) {
            Ok(slot) => {
                let requested_len =
                    u32::try_from(buf.len()).expect("write slot length fits the driver bound");
                let entry = OpEntry {
                    kind: OpKind::Write,
                    fd: fd.file_id(),
                    file_offset: offset,
                    frame: ReadFrameIdx::new(0),
                    destination_offset: 0,
                    requested_len,
                    raw_frame_lease: false,
                    write_slot: Some(buf.into_index()),
                    retries: 0,
                };
                Ok(self.commit(&mut shared, slot, entry))
            }
            Err(err) => Err((err, buf)),
        }
    }

    pub(crate) fn submit_fsync(
        &self,
        fd: &FileHandle,
        mode: SyncMode,
    ) -> Result<OpToken, SubmitError> {
        let SyncMode::Full = mode;
        let mut shared = self.lock();
        let slot = self.admit(&mut shared, fd.file_id())?;
        let entry = OpEntry {
            kind: OpKind::Fsync,
            fd: fd.file_id(),
            file_offset: 0,
            frame: ReadFrameIdx::new(0),
            destination_offset: 0,
            requested_len: 0,
            raw_frame_lease: false,
            write_slot: None,
            retries: 0,
        };
        Ok(self.commit(&mut shared, slot, entry))
    }

    /// Admission (locked prepare): own-driver check, then liveness, then slot
    /// reservation — all before any [`WriteSlot`] is consumed, so a rejection
    /// hands the slot back intact.
    fn admit(&self, shared: &mut Shared, fd: FileId) -> Result<u32, SubmitError> {
        self.assert_own(fd);
        if shared.files.is_live(fd) {
            let retained = u32::try_from(shared.completion_backlog.len())
                .expect("completion backlog length fits u32");
            if shared.files.total_inflight() + retained >= self.queue_capacity {
                Err(SubmitError::Full)
            } else {
                shared.slab.reserve().ok_or(SubmitError::Full)
            }
        } else {
            Err(SubmitError::StaleHandle)
        }
    }

    /// Fill the reserved slot, count the op in flight, then place it in the
    /// backend's ready order — the `fill → on_submit` half of admission.
    fn commit(&self, shared: &mut Shared, slot: u32, entry: OpEntry) -> OpToken {
        let fd = entry.fd;
        let token = shared.slab.fill(slot, entry);
        shared.files.on_submit(fd);
        let position = self.executor.schedule(shared.ready.len());
        debug_assert!(
            position <= shared.ready.len(),
            "schedule position in bounds"
        );
        shared.ready.insert(position, slot);
        self.executor.on_op_submitted();
        token
    }
}

impl<E: EagerExecutor> DriverCore<E> {
    pub(crate) fn poll(&self, out: &mut CompletionBatch) -> usize {
        assert!(
            out.capacity() > 0,
            "poll requires a non-empty completion batch"
        );
        out.reset();
        let mut drained = self.drain_completion_backlog(out);
        while drained < out.capacity() {
            let Some((slot, kind, context)) = self.prepare_next() else {
                break;
            };
            let outcome = self.execute(kind, context);
            self.publish(slot, outcome, out);
            drained += 1;
        }
        drained
    }

    pub(crate) fn poll_wait_eager_for_pool(
        &self,
        out: &mut CompletionBatch,
        timeout: Duration,
    ) -> usize {
        self.pool_wait
            .get()
            .expect("the shipping or mock driver is attached before Pool wait")
            .wait(timeout);
        self.poll(out)
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn alloc_write_slot_wait(&self, timeout: Duration) -> Option<WriteSlot<'_>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(slot) = self.write_arena.alloc() {
                return Some(slot);
            }
            if Instant::now() >= deadline {
                return None;
            }
            if self.pump_one_to_backlog() {
                continue;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            self.write_arena.wait_for_release(remaining);
        }
    }

    pub(crate) fn poll_wait(&self, out: &mut CompletionBatch, timeout: Duration) -> usize {
        let deadline = Instant::now() + timeout;
        loop {
            let drained = self.poll(out);
            if drained > 0 {
                return drained;
            }
            let now = Instant::now();
            if now >= deadline {
                return 0;
            }
            std::thread::sleep((deadline - now).min(POLL_WAIT_QUANTUM));
        }
    }

    /// Drains every admitted eager op before teardown (INV-8). A non-empty
    /// queue makes progress on each pass; the idle bound catches a broken
    /// backend rather than permitting an unbounded shutdown loop.
    pub(crate) fn quiesce(&self) {
        if self.inflight_total() == 0 {
            return;
        }
        let mut out = self
            .shutdown_batch
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut idle = 0u32;
        while self.inflight_total() > 0 {
            if self.poll(&mut out) > 0 {
                idle = 0;
            } else {
                idle += 1;
                assert!(idle < QUIESCE_IDLE_MAX, "drop quiesce made no progress");
            }
        }
    }

    /// Prepare (locked): take the next ready slot and read its op kind. The slab
    /// slot stays occupied — it is reclaimed only in [`DriverCore::publish`].
    fn prepare_next(&self) -> Option<(u32, OpKind, OpContext<'static>)> {
        let mut shared = self.lock();
        let slot = shared.ready.pop_front()?;
        let entry = shared.slab.peek(slot);
        let context = match entry.kind {
            OpKind::Read => OpContext::read(
                entry.fd,
                entry.file_offset,
                entry.frame,
                entry.destination_offset,
                entry.requested_len,
            ),
            OpKind::Fsync => OpContext::fsync(entry.fd),
            OpKind::Write => {
                let write_slot = entry
                    .write_slot
                    .expect("an async write retains a staging slot index");
                let source = self.write_arena.region(
                    write_slot,
                    entry.destination_offset,
                    entry.requested_len,
                );
                // SAFETY: the occupied slab entry retains the slot's free bit
                // until publish, and the driver-owned arena outlives execution.
                let write_buf =
                    unsafe { std::slice::from_raw_parts(source, entry.requested_len as usize) };
                OpContext::write(
                    entry.fd,
                    entry.file_offset,
                    entry.destination_offset,
                    write_buf,
                )
            }
        };
        Some((slot, entry.kind, context))
    }

    /// Execute (submit mutex *not* held): the retry policy and its fixed bound
    /// over the backend's simulated or real syscall. `EINTR` resubmits on every
    /// op, `EAGAIN` resubmits on reads but surfaces on writes and fsync.
    fn execute(&self, kind: OpKind, context: OpContext<'_>) -> Result<u32, i32> {
        let retry_would_block = matches!(kind, OpKind::Read);
        let mut retries = 0u32;
        let logical_len = context.requested_len;
        let mut next = context;
        loop {
            match self.executor.attempt(kind, next.requested_len, next) {
                Attempt::Done(bytes) => {
                    assert!(
                        bytes <= next.requested_len,
                        "executor reported more bytes than requested"
                    );
                    if !matches!(kind, OpKind::Write) {
                        return Ok(bytes);
                    }
                    if bytes == next.requested_len {
                        return Ok(logical_len);
                    }
                    if bytes > 0 {
                        next = next.advance_write(bytes);
                    } else if retries >= self.retry_bound {
                        return Err(EIO);
                    } else {
                        retries += 1;
                    }
                }
                Attempt::Failed(errno) => return Err(errno),
                Attempt::Interrupted => {
                    if retries >= self.retry_bound {
                        return Err(EINTR);
                    }
                    retries += 1;
                }
                Attempt::WouldBlock => {
                    if !retry_would_block {
                        return Err(EAGAIN);
                    }
                    if retries >= self.retry_bound {
                        return Err(EAGAIN);
                    }
                    retries += 1;
                }
            }
        }
    }

    /// Publish/finalize (locked): reclaim the slot now that its final attempt is
    /// done, drop the in-flight count (progressing a deferred close), release the
    /// write lease, and emit the completion.
    fn publish(&self, slot: u32, outcome: Result<u32, i32>, out: &mut CompletionBatch) {
        let retiring = {
            let mut shared = self.lock();
            let (token, entry) = shared.slab.reclaim(slot);
            let retire_due = shared.files.on_complete(entry.fd);
            self.release_raw_frame(&entry);
            self.release_write_slot(&entry);
            let result = outcome.map_err(IoError::from_raw);
            self.executor.on_op_completed(entry.fd, entry.kind, &result);
            out.push(Completion::new(token, entry.kind, result));
            retire_due.then_some(entry.fd)
        };
        self.signal_pool_wait();
        if let Some(fd) = retiring {
            self.retire(fd);
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn pump_one_to_backlog(&self) -> bool {
        let Some((slot, kind, context)) = self.prepare_next() else {
            return false;
        };
        let outcome = self.execute(kind, context);
        self.publish_to_backlog(slot, outcome);
        true
    }

    #[cfg(not(target_os = "linux"))]
    fn publish_to_backlog(&self, slot: u32, outcome: Result<u32, i32>) {
        let retiring = {
            let mut shared = self.lock();
            let (token, entry) = shared.slab.reclaim(slot);
            let retire_due = shared.files.on_complete(entry.fd);
            self.release_raw_frame(&entry);
            self.release_write_slot(&entry);
            assert!(
                shared.completion_backlog.len() < self.queue_capacity as usize,
                "completion backlog stays within the init-time admission bound"
            );
            let result = outcome.map_err(IoError::from_raw);
            self.executor.on_op_completed(entry.fd, entry.kind, &result);
            shared
                .completion_backlog
                .push_back(Completion::new(token, entry.kind, result));
            retire_due.then_some(entry.fd)
        };
        self.signal_pool_wait();
        if let Some(fd) = retiring {
            self.retire(fd);
        }
    }

    pub(crate) fn write_all_blocking(
        &self,
        fd: &FileHandle,
        buf: &[u8],
        offset: u64,
    ) -> Result<(), IoError> {
        let total = u32::try_from(buf.len()).expect("metadata write length within u32 bound");
        self.assert_own(fd.file_id());
        {
            let shared = self.lock();
            if !shared.files.is_live(fd.file_id()) {
                return Err(IoError::from_raw(EBADF));
            }
        }
        let mut written = 0u32;
        while written < total {
            let remaining = total - written;
            let context = OpContext::write(
                fd.file_id(),
                offset + u64::from(written),
                written,
                &buf[written as usize..],
            );
            match self.execute(OpKind::Write, context) {
                Ok(bytes) => {
                    assert!(
                        bytes <= remaining,
                        "executor reported more bytes than requested"
                    );
                    assert!(bytes > 0, "blocking write made no forward progress");
                    written += bytes;
                }
                Err(errno) => return Err(IoError::from_raw(errno)),
            }
        }
        Ok(())
    }

    pub(crate) fn fsync_blocking(&self, fd: &FileHandle, mode: SyncMode) -> Result<(), IoError> {
        let SyncMode::Full = mode;
        self.assert_own(fd.file_id());
        {
            let shared = self.lock();
            if !shared.files.is_live(fd.file_id()) {
                return Err(IoError::from_raw(EBADF));
            }
        }
        let context = OpContext::fsync(fd.file_id());
        match self.execute(OpKind::Fsync, context) {
            Ok(_) => Ok(()),
            Err(errno) => Err(IoError::from_raw(errno)),
        }
    }
}

/// The `io_uring` poll seam. Prepare fills SQEs under the AD-4 mutex, the kernel
/// wait runs outside it (INV-3), reap drains CQEs under the mutex. A completion
/// routes to its slab slot by echoed `user_data`, not prepare order, because
/// CQEs arrive unordered relative to submission.
impl<E: RingExecutor> DriverCore<E> {
    pub(crate) fn poll_ring(&self, out: &mut CompletionBatch) -> usize {
        self.poll_ring_progress(out).caller_completions
    }

    pub(crate) fn poll_ring_progress(&self, out: &mut CompletionBatch) -> BackendProgress {
        assert!(
            out.capacity() > 0,
            "poll requires a non-empty completion batch"
        );
        out.reset();
        let backlog = self.drain_completion_backlog(out);
        if backlog == out.capacity() {
            return BackendProgress {
                caller_completions: backlog,
                backend_completions: 0,
            };
        }
        let cap = u32::try_from(out.capacity() - backlog).expect("batch capacity fits u32");
        let filled = self.fill_ring(cap);
        if filled > 0 {
            self.executor.submit();
        }
        let reaped = self.reap_ring(out, cap);
        BackendProgress {
            caller_completions: backlog + reaped.caller_completions,
            backend_completions: reaped.backend_completions,
        }
    }

    pub(crate) fn poll_wait_ring(&self, out: &mut CompletionBatch, timeout: Duration) -> usize {
        self.poll_wait_ring_progress(out, timeout)
            .caller_completions
    }

    pub(crate) fn poll_wait_ring_progress(
        &self,
        out: &mut CompletionBatch,
        timeout: Duration,
    ) -> BackendProgress {
        assert!(
            out.capacity() > 0,
            "poll requires a non-empty completion batch"
        );
        out.reset();
        let backlog = self.drain_completion_backlog(out);
        if backlog > 0 {
            return BackendProgress {
                caller_completions: backlog,
                backend_completions: 0,
            };
        }
        let cap = u32::try_from(out.capacity()).expect("batch capacity fits u32");
        let filled = self.fill_ring(cap);
        self.executor.submit_and_wait(filled.max(1), timeout);
        self.reap_ring(out, cap)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn poll_wait_ring_for_pool(
        &self,
        out: &mut CompletionBatch,
        timeout: Duration,
    ) -> BackendProgress {
        let wait = self
            .pool_wait
            .get()
            .expect("the shipping driver is attached before Pool wait");
        let Some(armed) = wait.begin_platform_wait() else {
            return self.poll_ring_progress(out);
        };
        let deadline = Instant::now() + timeout;
        let drained = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break BackendProgress {
                    caller_completions: 0,
                    backend_completions: 0,
                };
            }
            let drained = self.poll_wait_ring_one(out, remaining);
            if drained.backend_completions > 0
                || drained.caller_completions > 0
                || wait.platform_woken(armed)
            {
                break drained;
            }
        };
        let outcome = if drained.backend_completions > 0 || drained.caller_completions > 0 {
            PlatformWaitOutcome::Progress
        } else {
            PlatformWaitOutcome::Deadline
        };
        wait.finish_platform_wait(armed, outcome);
        drained
    }

    #[cfg(target_os = "linux")]
    fn poll_wait_ring_one(&self, out: &mut CompletionBatch, timeout: Duration) -> BackendProgress {
        assert!(
            out.capacity() > 0,
            "pool wait requires a non-empty private completion batch"
        );
        out.reset();
        let backlog = self.drain_completion_backlog(out);
        if backlog > 0 {
            return BackendProgress {
                caller_completions: backlog,
                backend_completions: 0,
            };
        }
        let cap = u32::try_from(out.capacity()).expect("batch capacity fits u32");
        let _ = self.fill_ring(cap);
        self.executor.submit_and_wait(1, timeout);
        self.reap_ring(out, cap)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn alloc_write_slot_wait_ring(&self, timeout: Duration) -> Option<WriteSlot<'_>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(slot) = self.write_arena.alloc() {
                return Some(slot);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            self.write_arena.wait_for_release(remaining);
        }
    }

    /// Prepare (locked): drain ready slots into SQEs, tagging each with its slab
    /// slot as `user_data`. Slots stay occupied — reclaimed only in reap.
    fn fill_ring(&self, cap: u32) -> u32 {
        let mut shared = self.lock();
        let mut filled = 0u32;
        while filled < cap {
            let Some(slot) = shared.ready.pop_front() else {
                break;
            };
            let entry = shared.slab.peek(slot);
            let user_data = u64::from(slot);
            let fd_slot = entry.fd.slot();
            match entry.kind {
                OpKind::Read => {
                    self.executor.push_read(
                        user_data,
                        fd_slot,
                        entry.frame,
                        entry.file_offset,
                        entry.destination_offset,
                        entry.requested_len,
                    );
                }
                OpKind::Fsync => self.executor.push_fsync(user_data, fd_slot),
                OpKind::Write => {
                    let write_slot = entry
                        .write_slot
                        .expect("an async write retains a staging slot index");
                    let source = self.write_arena.region(
                        write_slot,
                        entry.destination_offset,
                        entry.requested_len,
                    );
                    self.executor.push_write(
                        user_data,
                        fd_slot,
                        source,
                        entry.destination_offset,
                        entry.file_offset,
                        entry.requested_len,
                    );
                }
            }
            filled += 1;
        }
        filled
    }

    fn reap_ring_locked<F>(
        &self,
        shared: &mut Shared,
        out: &mut CompletionBatch,
        limit: u32,
        retry_bound: u32,
        caller_completions: &mut usize,
        mut retire_due: F,
    ) -> RingReap
    where
        F: FnMut(FileId) -> bool,
    {
        self.executor.reap(limit, |user_data, raw| {
            let slot = u32::try_from(user_data).expect("uring user_data is a slab slot");
            let entry = shared.slab.peek_mut(slot);
            let RingProgress::Terminal(result) = ring_progress(entry, raw, retry_bound) else {
                shared.ready.push_back(slot);
                return true;
            };
            let (token, entry) = shared.slab.reclaim(slot);
            let file_retire_due = shared.files.on_complete(entry.fd);
            self.release_raw_frame(&entry);
            self.release_write_slot(&entry);
            self.executor.on_op_completed(entry.fd, entry.kind, &result);
            out.push(Completion::new(token, entry.kind, result));
            let keep_reaping = !file_retire_due || retire_due(entry.fd);
            self.executor.on_op_finalized();
            *caller_completions += 1;
            keep_reaping
        })
    }

    fn reap_ring_record_retire(scratch: &mut Vec<FileId>, file: FileId) {
        assert!(
            scratch.len() < scratch.capacity(),
            "one retire per file slot"
        );
        scratch.push(file);
    }

    /// Reap/finalize (locked): drain CQEs, routing each by echoed slot id. A
    /// retryable `-EAGAIN`/`-EINTR` result under the init-time bound keeps the op
    /// live — its slot is re-queued for a fresh SQE, not reclaimed, and no
    /// completion is emitted (scope.md:596). A terminal result reclaims the slot,
    /// drops the in-flight count (progressing a deferred close), releases the
    /// write lease, and emits the completion. Retires run after the lock is
    /// released.
    fn reap_ring(&self, out: &mut CompletionBatch, limit: u32) -> BackendProgress {
        assert!(limit > 0, "ring reap limit is positive");
        let mut caller_completions = 0usize;
        let mut first_retire = None;
        let first_reap;
        {
            let mut shared = self.lock();
            first_reap = self.reap_ring_locked(
                &mut shared,
                out,
                limit,
                self.retry_bound,
                &mut caller_completions,
                |file| {
                    assert!(first_retire.replace(file).is_none());
                    false
                },
            );
            if first_reap.rearm_needed {
                self.executor.rearm_after_reap();
            }
        }
        let mut backend_completions = first_reap.backend_completions;
        let mut rearm_needed = first_reap.rearm_needed;
        let mut retire_scratch = first_retire.map(|file| {
            let mut scratch = self.retire.lock().unwrap_or_else(PoisonError::into_inner);
            scratch.clear();
            Self::reap_ring_record_retire(&mut scratch, file);
            scratch
        });
        if let Some(scratch) = retire_scratch.as_mut() {
            let remaining = limit
                .checked_sub(backend_completions)
                .expect("the first reap respects its limit");
            if remaining > 0 {
                let mut shared = self.lock();
                let additional = self.reap_ring_locked(
                    &mut shared,
                    out,
                    remaining,
                    self.retry_bound,
                    &mut caller_completions,
                    |file| {
                        Self::reap_ring_record_retire(scratch, file);
                        true
                    },
                );
                if additional.rearm_needed {
                    self.executor.rearm_after_reap();
                }
                backend_completions += additional.backend_completions;
                rearm_needed |= additional.rearm_needed;
            }
        }
        if rearm_needed {
            self.executor.submit();
        }
        if caller_completions > 0 {
            self.signal_pool_wait();
        }
        if let Some(mut scratch) = retire_scratch {
            for fd in scratch.drain(..) {
                self.retire(fd);
            }
        }
        BackendProgress {
            caller_completions,
            backend_completions,
        }
    }

    /// Drains every in-flight op before teardown so no kernel-visible op is
    /// abandoned (INV-8). Bounded: each poll that makes no progress counts against
    /// a fixed cap.
    pub(crate) fn quiesce_ring(&self) {
        if self.inflight_total() == 0 {
            return;
        }
        let mut out = self
            .shutdown_batch
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut idle = 0u32;
        while self.inflight_total() > 0 {
            let progress = self.poll_ring_progress(&mut out);
            if progress.backend_completions > 0 || progress.caller_completions > 0 {
                idle = 0;
            } else {
                idle += 1;
                assert!(idle < QUIESCE_IDLE_MAX, "drop quiesce made no progress");
            }
        }
    }
}

impl<E> DriverCore<E> {
    fn drain_completion_backlog(&self, out: &mut CompletionBatch) -> usize {
        let mut shared = self.lock();
        let mut drained = 0usize;
        while drained < out.capacity() {
            let Some(completion) = shared.completion_backlog.pop_front() else {
                break;
            };
            out.push(completion);
            drained += 1;
        }
        drained
    }

    fn release_write_slot(&self, entry: &OpEntry) {
        if let Some(slot) = entry.write_slot {
            self.write_arena.release(slot);
        }
    }

    fn release_raw_frame(&self, entry: &OpEntry) {
        if entry.raw_frame_lease {
            let was_inflight =
                self.raw_read_inflight[entry.frame.get() as usize].swap(false, Ordering::AcqRel);
            assert!(
                was_inflight,
                "a completed raw read releases its frame lease"
            );
        }
    }
}

/// The ring metadata plane (AD-3): blocking `pwrite`/`fsync` on the retained
/// file, never the ring. Linux-only — the mock ring has no metadata plane.
#[cfg(target_os = "linux")]
impl<E: RingExecutor> DriverCore<E> {
    pub(crate) fn write_all_blocking_ring(
        &self,
        fd: &FileHandle,
        buf: &[u8],
        offset: u64,
    ) -> Result<(), IoError> {
        let total = u32::try_from(buf.len()).expect("metadata write length within u32 bound");
        self.check_live(fd.file_id())?;
        let mut written = 0u32;
        while written < total {
            match self.executor.blocking_write(
                fd.file_id().slot(),
                &buf[written as usize..],
                offset + u64::from(written),
            ) {
                Ok(bytes) => {
                    assert!(
                        bytes <= total - written,
                        "backend reported more bytes than requested"
                    );
                    assert!(bytes > 0, "blocking write made no forward progress");
                    written += bytes;
                }
                Err(errno) => return Err(IoError::from_raw(errno)),
            }
        }
        Ok(())
    }

    pub(crate) fn fsync_blocking_ring(
        &self,
        fd: &FileHandle,
        mode: SyncMode,
    ) -> Result<(), IoError> {
        let SyncMode::Full = mode;
        self.check_live(fd.file_id())?;
        self.executor
            .blocking_fsync(fd.file_id().slot())
            .map_err(IoError::from_raw)
    }

    fn check_live(&self, fd: FileId) -> Result<(), IoError> {
        self.assert_own(fd);
        if self.lock().files.is_live(fd) {
            Ok(())
        } else {
            Err(IoError::from_raw(EBADF))
        }
    }
}

/// Maps a raw CQE result — non-negative byte count or `-errno` — to the op
/// outcome the completion carries.
fn ring_result(raw: i32) -> Result<u32, IoError> {
    if raw < 0 {
        Err(IoError::from_raw(-raw))
    } else {
        Ok(u32::try_from(raw).expect("a non-negative CQE result fits u32"))
    }
}

#[derive(Debug)]
enum RingProgress {
    Resubmit,
    Terminal(Result<u32, IoError>),
}

fn ring_progress(entry: &mut OpEntry, raw: i32, retry_bound: u32) -> RingProgress {
    if ring_should_retry(entry.kind, raw, entry.retries, retry_bound) {
        entry.retries += 1;
        return RingProgress::Resubmit;
    }
    if !matches!(entry.kind, OpKind::Write) || raw < 0 {
        return RingProgress::Terminal(ring_result(raw));
    }
    let bytes = u32::try_from(raw).expect("a non-negative CQE result fits u32");
    assert!(
        bytes <= entry.requested_len,
        "write CQE reported more bytes than requested"
    );
    if bytes == entry.requested_len {
        return RingProgress::Terminal(Ok(entry.destination_offset + bytes));
    }
    if bytes == 0 {
        if entry.retries >= retry_bound {
            return RingProgress::Terminal(Err(IoError::from_raw(EIO)));
        }
        entry.retries += 1;
        return RingProgress::Resubmit;
    }
    entry.file_offset += u64::from(bytes);
    entry.destination_offset += bytes;
    entry.requested_len -= bytes;
    RingProgress::Resubmit
}

/// Whether a raw CQE result is a resubmittable transient under the init-time
/// bound: `EINTR` on every op, `EAGAIN` on reads only, mirroring the eager retry
/// policy so both backends honour scope.md:596 identically.
fn ring_should_retry(kind: OpKind, raw: i32, retries: u32, retry_bound: u32) -> bool {
    if raw >= 0 || retries >= retry_bound {
        return false;
    }
    match -raw {
        EINTR => true,
        EAGAIN => matches!(kind, OpKind::Read),
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileState {
    Free,
    Open,
    Closing,
    Closed,
}

#[derive(Debug, Clone, Copy)]
struct FileSlot {
    generation: u32,
    state: FileState,
    inflight: u32,
}

#[derive(Debug)]
struct FileTable {
    slots: Box<[FileSlot]>,
}

impl FileTable {
    #[cfg(test)]
    fn new() -> Self {
        Self::try_with_capacity(DEFAULT_REGISTERED_FILE_CAPACITY)
            .expect("the test file table capacity allocates")
    }

    fn try_with_capacity(capacity: u32) -> Option<Self> {
        let slot = FileSlot {
            generation: 0,
            state: FileState::Free,
            inflight: 0,
        };
        Some(Self {
            slots: crate::allocation::try_boxed_slice_with(capacity, || slot)?,
        })
    }

    fn try_resized(&self, capacity: u32) -> Option<Self> {
        let empty = FileSlot {
            generation: 0,
            state: FileState::Free,
            inflight: 0,
        };
        let mut slots = crate::allocation::try_boxed_slice_with(capacity, || empty)?;
        let copied = slots.len().min(self.slots.len());
        slots[..copied].copy_from_slice(&self.slots[..copied]);
        if self.slots[copied..]
            .iter()
            .any(|slot| slot.generation != 0 || slot.state != FileState::Free)
        {
            return None;
        }
        Some(Self { slots })
    }

    fn open(&mut self, driver: u64) -> Option<FileId> {
        for index in 0..self.slots.len() {
            let slot = &mut self.slots[index];
            if matches!(slot.state, FileState::Free | FileState::Closed) {
                assert!(slot.generation < u32::MAX, "fd generation exhausted");
                slot.generation += 1;
                slot.state = FileState::Open;
                slot.inflight = 0;
                let id_slot = u32::try_from(index).ok()?;
                return Some(FileId::new(driver, id_slot, slot.generation));
            }
        }
        None
    }

    fn is_live(&self, id: FileId) -> bool {
        let slot = &self.slots[id.slot() as usize];
        slot.generation == id.generation() && matches!(slot.state, FileState::Open)
    }

    fn total_inflight(&self) -> u32 {
        self.slots.iter().map(|slot| slot.inflight).sum()
    }

    fn is_closed(&self, id: FileId) -> bool {
        let slot = &self.slots[id.slot() as usize];
        slot.generation > id.generation()
            || slot.generation == id.generation() && matches!(slot.state, FileState::Closed)
    }

    fn on_submit(&mut self, id: FileId) {
        let slot = &mut self.slots[id.slot() as usize];
        debug_assert_eq!(
            slot.generation,
            id.generation(),
            "submit on a stale fd generation"
        );
        debug_assert!(slot.inflight < u32::MAX, "in-flight count overflow");
        slot.inflight += 1;
    }

    fn on_complete(&mut self, id: FileId) -> bool {
        let slot = &mut self.slots[id.slot() as usize];
        debug_assert_eq!(
            slot.generation,
            id.generation(),
            "completion for a stale fd generation"
        );
        debug_assert!(slot.inflight > 0, "completion without a matching submit");
        slot.inflight -= 1;
        slot.inflight == 0 && matches!(slot.state, FileState::Closing)
    }

    /// A slot reaches `Closed` only here, after the backend retired its file: an
    /// `open` landing between `close`/`on_complete` and this point sees `Closing`,
    /// skips the slot, and so cannot register into an eager file entry the retire
    /// has not yet cleared.
    fn finish_retire(&mut self, id: FileId) {
        let slot = &mut self.slots[id.slot() as usize];
        debug_assert_eq!(
            slot.generation,
            id.generation(),
            "retire of a stale fd generation"
        );
        assert_eq!(
            slot.state,
            FileState::Closing,
            "finish_retire expects a slot left Closing by close/on_complete"
        );
        assert_eq!(slot.inflight, 0, "finish_retire with ops still in flight");
        slot.state = FileState::Closed;
    }

    fn close(&mut self, id: FileId) -> bool {
        let slot = &mut self.slots[id.slot() as usize];
        assert_eq!(
            slot.generation,
            id.generation(),
            "close of a stale fd generation is a driver state bug (EBADF)"
        );
        assert_eq!(
            slot.state,
            FileState::Open,
            "double close of an fd is a driver state bug (EBADF)"
        );
        slot.state = FileState::Closing;
        slot.inflight == 0
    }

    fn abort_open(&mut self, id: FileId) {
        let slot = &mut self.slots[id.slot() as usize];
        assert_eq!(
            slot.generation,
            id.generation(),
            "abort of a stale fd generation"
        );
        assert_eq!(
            slot.state,
            FileState::Open,
            "abort_open expects a slot still Open from its reservation"
        );
        assert_eq!(slot.inflight, 0, "abort_open before any op is submitted");
        slot.state = FileState::Free;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retiring_slot_is_not_reused_until_its_file_is_retired() {
        let mut files = FileTable::new();
        let driver = 7;
        let id = files.open(driver).expect("first slot opens");

        files.on_submit(id);
        assert!(
            !files.close(id),
            "a close with an op in flight is not retire-due yet"
        );

        assert!(
            files.on_complete(id),
            "the last drain under Closing makes the retire due"
        );
        assert!(
            !files.is_closed(id),
            "the slot stays Closing until finish_retire"
        );

        let reopened = files.open(driver).expect("a fresh slot is available");
        assert!(
            !reopened.aliases_slot(&id),
            "a retiring slot is never reused before its file is retired"
        );

        files.finish_retire(id);
        assert!(
            files.is_closed(id),
            "finish_retire completes the deferred close"
        );
    }

    #[test]
    fn a_zero_inflight_close_retires_through_finish_retire_not_a_direct_flip() {
        let mut files = FileTable::new();
        let id = files.open(3).expect("first slot opens");

        assert!(files.close(id), "closing an idle fd is retire-due at once");
        assert!(
            !files.is_closed(id),
            "an idle close still awaits finish_retire before Closed"
        );

        files.finish_retire(id);
        assert!(files.is_closed(id), "finish_retire flips it Closed");
    }
}
