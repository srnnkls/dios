//! Seeded, deterministically-reordering in-memory backend — the DST seam.
//!
//! [`MockDriver`] is a thin newtype over the shared driver core, so it cannot
//! drift from the real driver: it adds only what a backend owns — fault
//! injection, seed-derived completion reordering, and simulated (instant)
//! execution. Admission, lease tracking, the retry policy, and finalization all
//! live in the core. Backend behaviour is never selected by matching
//! [`Backend`](crate::driver::Backend) (AD-1); the mock is a distinct type.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use crate::completion::CompletionBatch;
use crate::driver::{
    next_driver_id, Attempt, CompletionSlab, DriverCore, EagerExecutor, Executor, FileHandle,
    FileId, OpContext, OpKind, OpToken, RingExecutor, Shared, SyncMode, MAX_FILES,
};
use crate::error::{IoError, SubmitError};
use crate::open::DirectIo;
use crate::pool::write_arena::{shared as shared_write_arena, ArenaState, WriteSlot};
use crate::pool::{Frames, PoolBackend, ReadFrameIdx};

const EINTR: i32 = 4;
#[cfg(target_os = "linux")]
const EAGAIN: i32 = 11;
#[cfg(not(target_os = "linux"))]
const EAGAIN: i32 = 35;

/// One read the pool issued against the mock: the file offset and the requested
/// byte count. Recorded in submission order so the reslice remainder read
/// (offset advanced by the short count, length the unfilled tail) is observable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadAttempt {
    pub file_offset: u64,
    pub destination_offset: u32,
    pub requested_len: u32,
}

/// One asynchronous write attempt: the file offset, source offset within the
/// retained staging slot, and exact tail length presented to the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteAttempt {
    pub file_offset: u64,
    pub source_offset: u32,
    pub requested_len: u32,
}

/// Direct-I/O capability reported by a deterministic mock file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectIoSupport {
    /// The mock file supports direct transfers.
    Supported,
    /// The mock file supports buffered transfers only.
    Unsupported,
}

/// A fault the mock injects into the next resolved op, mimicking a syscall
/// return the real backends handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Injected {
    /// `EINTR`: retried on every op up to the init-time bound.
    Eintr,
    /// `EAGAIN`: retried on reads, surfaced as an error on writes.
    Eagain,
    /// A short transfer of `bytes`, surfaced with its partial count.
    Short(u32),
    /// An operating failure carrying `errno`.
    Io(i32),
}

/// Builds a [`MockDriver`] with all capacities fixed up front.
#[derive(Debug, Clone, Copy)]
pub struct MockDriverBuilder {
    seed: u64,
    queue_capacity: u32,
    frames: u32,
    frame_bytes: u32,
    write_slots: u32,
    retry_bound: u32,
    direct_io_support: DirectIoSupport,
}

impl Default for MockDriverBuilder {
    fn default() -> Self {
        Self {
            seed: 0,
            queue_capacity: 1,
            frames: 1,
            frame_bytes: 4096,
            write_slots: 1,
            retry_bound: 0,
            direct_io_support: DirectIoSupport::Supported,
        }
    }
}

impl MockDriverBuilder {
    #[must_use]
    pub fn seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    #[must_use]
    pub fn queue_capacity(mut self, queue_capacity: u32) -> Self {
        self.queue_capacity = queue_capacity;
        self
    }

    #[must_use]
    pub fn frames(mut self, frames: u32) -> Self {
        self.frames = frames;
        self
    }

    #[must_use]
    pub fn frame_bytes(mut self, frame_bytes: u32) -> Self {
        self.frame_bytes = frame_bytes;
        self
    }

    #[must_use]
    pub fn write_slots(mut self, write_slots: u32) -> Self {
        self.write_slots = write_slots;
        self
    }

    #[must_use]
    pub fn retry_bound(mut self, retry_bound: u32) -> Self {
        self.retry_bound = retry_bound;
        self
    }

    /// Selects the direct-I/O capability exposed by newly opened mock files.
    #[must_use]
    pub fn direct_io_support(mut self, support: DirectIoSupport) -> Self {
        self.direct_io_support = support;
        self
    }

    /// # Panics
    ///
    /// If any capacity is zero — capacities are fixed and positive at init.
    #[must_use]
    pub fn build(self) -> MockDriver {
        assert!(self.queue_capacity > 0, "queue capacity must be positive");
        assert!(self.frames > 0, "frame count must be positive");
        assert!(self.frame_bytes > 0, "frame size must be positive");
        assert!(self.write_slots > 0, "write slot count must be positive");
        let id = next_driver_id();
        let write_arena = shared_write_arena(self.write_slots, self.frame_bytes, id);
        let executor = MockExecutor::new(
            self.seed,
            self.frames,
            self.frame_bytes,
            self.queue_capacity,
            self.direct_io_support,
        );
        let shared = Shared::new(
            CompletionSlab::with_capacity(self.queue_capacity),
            self.queue_capacity,
        );
        MockDriver(DriverCore::new(
            shared,
            executor,
            write_arena,
            self.frames,
            self.retry_bound,
            self.queue_capacity,
            id,
        ))
    }
}

/// A seeded in-memory driver used as the deterministic test backend.
#[derive(Debug)]
pub struct MockDriver(DriverCore<MockExecutor>);

impl Drop for MockDriver {
    fn drop(&mut self) {
        self.0.quiesce();
    }
}

impl MockDriver {
    #[must_use]
    pub fn builder() -> MockDriverBuilder {
        MockDriverBuilder::default()
    }

    pub(crate) fn share_frames_for_pool(&self, frames: Arc<Frames>) {
        self.0.executor().set_arena(frames);
    }

    /// Opens a fresh generational handle. Never touches disk.
    ///
    /// # Errors
    ///
    /// Returns `EMFILE` when the fixed fd table is exhausted.
    pub fn open(&self, path: &Path, direct_io: DirectIo) -> Result<FileHandle, IoError> {
        self.open_with_direct_io(path, direct_io)
    }

    fn open_with_direct_io(&self, path: &Path, direct_io: DirectIo) -> Result<FileHandle, IoError> {
        if direct_io == DirectIo::Required
            && self.0.executor().direct_io_support == DirectIoSupport::Unsupported
        {
            let error = std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "direct IO is unsupported for this mock file",
            );
            return Err(IoError::from(error));
        }
        let handle = self.0.open_mock(path)?;
        let io_mode = if direct_io != DirectIo::Disabled
            && self.0.executor().direct_io_support == DirectIoSupport::Supported
        {
            let alignment = crate::alignment::Alignment::new(4096)
                .expect("the mock direct alignment is a power of two");
            crate::driver::IoMode::Direct(alignment)
        } else {
            crate::driver::IoMode::Buffered
        };
        Ok(FileHandle::from_parts(handle.file_id(), io_mode))
    }

    /// Consumes the handle; the deferred close(2) is observable once the fd's
    /// in-flight ops drain (INV-11).
    ///
    /// # Panics
    ///
    /// If `fd` was minted by a different driver.
    pub fn close(&self, fd: FileHandle) {
        self.0.close(fd);
    }

    /// # Errors
    ///
    /// [`SubmitError::StaleHandle`] if `fd`'s generation is stale,
    /// [`SubmitError::Full`] if the slab is at capacity.
    ///
    /// # Panics
    ///
    /// If `fd` was minted by a different driver, or `frame` is out of range for
    /// the configured frame count.
    pub fn submit_read(
        &self,
        fd: &FileHandle,
        frame: ReadFrameIdx,
        offset: u64,
    ) -> Result<OpToken, SubmitError> {
        self.0
            .submit_read(fd, frame, offset, 0, self.0.executor().frame_bytes, true)
    }

    /// # Errors
    ///
    /// The tuple hands the unconsumed [`WriteSlot`] back on rejection:
    /// [`SubmitError::StaleHandle`] for a stale fd, [`SubmitError::Full`] at
    /// capacity.
    ///
    /// # Panics
    ///
    /// If `fd` was minted by a different driver.
    pub fn submit_write<'arena>(
        &self,
        fd: &FileHandle,
        buf: WriteSlot<'arena>,
        offset: u64,
    ) -> Result<OpToken, (SubmitError, WriteSlot<'arena>)> {
        self.0.submit_write(fd, buf, offset)
    }

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

    /// Drains ready completions into `out` (never sleeps) and returns the count.
    ///
    /// # Panics
    ///
    /// If `out` has zero capacity.
    pub fn poll(&self, out: &mut CompletionBatch) -> usize {
        self.0.poll(out)
    }

    /// Drains completions, waiting up to `timeout` for the first, parking
    /// outside the submit lock so a submit never waits on a parked poller
    /// (AD-4/INV-3).
    pub fn poll_wait(&self, out: &mut CompletionBatch, timeout: Duration) -> usize {
        self.0.poll_wait(out, timeout)
    }

    /// Blocking metadata-plane write: loops the remainder past short transfers
    /// until the whole buffer is written or an operating failure surfaces;
    /// retries `EINTR` up to the init-time bound.
    ///
    /// # Errors
    ///
    /// `EBADF` on a stale/closed handle, or an injected operating failure.
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
        self.0.write_all_blocking(fd, buf, offset)
    }

    /// Blocking metadata-plane fsync; retries `EINTR` up to the init-time bound.
    ///
    /// # Errors
    ///
    /// `EBADF` on a stale/closed handle, or an injected operating failure.
    ///
    /// # Panics
    ///
    /// If `fd` was minted by a different driver.
    pub fn fsync_blocking(&self, fd: &FileHandle, mode: SyncMode) -> Result<(), IoError> {
        self.0.fsync_blocking(fd, mode)
    }

    /// Queues `fault` for the next resolved op (submitted or blocking).
    pub fn inject_next(&self, fault: Injected) {
        self.0.executor().inject(fault);
    }

    /// Whether the fd named by `id` has issued its deferred close(2).
    ///
    /// # Panics
    ///
    /// If `id` was minted by a different driver.
    #[must_use]
    pub fn is_closed(&self, id: FileId) -> bool {
        self.0.is_closed(id)
    }

    /// Mints a second reference to the same fd — the stale-handle test's ghost.
    #[must_use]
    pub fn duplicate_handle(&self, fd: &FileHandle) -> FileHandle {
        FileHandle::from_id(fd.file_id())
    }

    /// Borrows the mock driver's fixed write-staging arena.
    #[must_use]
    pub fn write_arena(&self) -> MockWriteArena<'_> {
        MockWriteArena {
            state: self.0.write_arena_state(),
        }
    }

    /// Seeds the simulated disk: a clean read of `fd`'s `granule_idx` granule
    /// fills the destination pool frame with `fill` in every byte. Called before
    /// the mock is composed into a pool.
    pub fn seed_page(&self, fd: &FileHandle, granule_idx: u32, fill: u8) {
        self.0.executor().seed(fd.file_id(), granule_idx, fill);
    }

    /// The reads the composed pool issued against this mock, in submission order.
    #[must_use]
    pub fn read_attempts_in_order(&self) -> Vec<ReadAttempt> {
        self.0.executor().attempts()
    }

    /// The write tails presented to the eager-shaped executor, in attempt order.
    #[must_use]
    pub fn write_attempts_in_order(&self) -> Vec<WriteAttempt> {
        self.0.executor().write_attempts()
    }
}

/// A borrowing view of a [`MockDriver`]'s fixed staging slots.
#[derive(Debug, Clone, Copy)]
pub struct MockWriteArena<'driver> {
    state: &'driver ArenaState,
}

impl MockWriteArena<'_> {
    /// Leases a mock staging slot, or `None` when all fixed slots are held.
    #[must_use]
    pub fn alloc(&self) -> Option<WriteSlot<'_>> {
        self.state.alloc()
    }
}

impl PoolBackend for MockDriver {
    fn open(&self, path: &Path, direct_io: DirectIo) -> Result<FileHandle, IoError> {
        self.open_with_direct_io(path, direct_io)
    }

    fn submit_read(
        &self,
        fd: &FileHandle,
        frame: ReadFrameIdx,
        file_offset: u64,
        destination_offset: u32,
        len: u32,
    ) -> Result<OpToken, SubmitError> {
        self.0.executor().record_attempt(ReadAttempt {
            file_offset,
            destination_offset,
            requested_len: len,
        });
        self.0
            .submit_read(fd, frame, file_offset, destination_offset, len, false)
    }

    fn poll(&self, out: &mut CompletionBatch) -> usize {
        self.0.poll(out)
    }
}

impl crate::pool::PoolBackendSealed for MockDriver {}

/// The mock backend's own state: injected faults and the reordering PRNG behind
/// an internal mutex (the execute phase runs outside the core's submit lock),
/// plus the immutable clean transfer size. Admission, leases, retries, and
/// finalization are the core's, not the mock's.
#[derive(Debug)]
struct MockExecutor {
    state: Mutex<MockState>,
    arena: OnceLock<Arc<Frames>>,
    frames: u32,
    frame_bytes: u32,
    direct_io_support: DirectIoSupport,
}

#[derive(Debug)]
struct MockState {
    injected: VecDeque<Injected>,
    seeds: HashMap<(FileId, u32), u8>,
    attempts: Vec<ReadAttempt>,
    write_attempts: Vec<WriteAttempt>,
    rng: u64,
}

impl MockExecutor {
    fn new(
        seed: u64,
        frames: u32,
        frame_bytes: u32,
        injected_capacity: u32,
        direct_io_support: DirectIoSupport,
    ) -> Self {
        Self {
            state: Mutex::new(MockState {
                injected: VecDeque::with_capacity(injected_capacity as usize),
                seeds: HashMap::new(),
                attempts: Vec::new(),
                write_attempts: Vec::with_capacity(injected_capacity as usize),
                rng: seed,
            }),
            arena: OnceLock::new(),
            frames,
            frame_bytes,
            direct_io_support,
        }
    }

    fn lock(&self) -> MutexGuard<'_, MockState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn inject(&self, fault: Injected) {
        self.lock().injected.push_back(fault);
    }

    fn seed(&self, file_id: FileId, granule_idx: u32, fill: u8) {
        self.lock().seeds.insert((file_id, granule_idx), fill);
    }

    fn attempts(&self) -> Vec<ReadAttempt> {
        self.lock().attempts.clone()
    }

    fn record_attempt(&self, attempt: ReadAttempt) {
        self.lock().attempts.push(attempt);
    }

    fn write_attempts(&self) -> Vec<WriteAttempt> {
        self.lock().write_attempts.clone()
    }

    fn set_arena(&self, arena: Arc<Frames>) {
        let _ = self.arena.set(arena);
    }

    /// Fills the destination pool frame with the seeded byte for a clean read,
    /// modelling the disk transferring the granule's contents into the buffer.
    fn fill_read(&self, context: &OpContext<'_>) {
        let granule_idx = u32::try_from(context.file_offset / u64::from(self.frame_bytes))
            .expect("granule index fits u32");
        let fill = self.lock().seeds.get(&(context.fd, granule_idx)).copied();
        if let (Some(arena), Some(fill)) = (self.arena.get(), fill) {
            arena.with_transfer_range_mut(
                context.frame,
                context.destination_offset,
                context.requested_len,
                |destination| destination.fill(fill),
            );
        }
    }
}

impl EagerExecutor for MockExecutor {
    fn attempt(&self, kind: OpKind, clean_bytes: u32, context: OpContext<'_>) -> Attempt {
        debug_assert!(
            context.fd.slot() < MAX_FILES,
            "the driver admits ops only on live fd-table slots"
        );
        debug_assert!(
            context.frame.get() < self.frames,
            "an op's frame indexes within the configured pool"
        );
        match kind {
            OpKind::Read => debug_assert!(
                context.write_buf.is_empty(),
                "a read attempt carries no write payload"
            ),
            OpKind::Fsync => {
                debug_assert!(
                    context.file_offset == 0,
                    "an fsync attempt carries no offset"
                );
                debug_assert!(
                    context.write_buf.is_empty(),
                    "an fsync attempt carries no payload"
                );
            }
            OpKind::Write => {}
        }
        let injected = {
            let mut state = self.lock();
            if matches!(kind, OpKind::Write) {
                state.write_attempts.push(WriteAttempt {
                    file_offset: context.file_offset,
                    source_offset: context.destination_offset,
                    requested_len: context.requested_len,
                });
            }
            state.injected.pop_front()
        };
        match injected {
            None => {
                if matches!(kind, OpKind::Read) {
                    self.fill_read(&context);
                }
                Attempt::Done(clean_bytes)
            }
            Some(Injected::Io(errno)) => Attempt::Failed(errno),
            Some(Injected::Short(bytes)) => Attempt::Done(bytes),
            Some(Injected::Eintr) => Attempt::Interrupted,
            Some(Injected::Eagain) => Attempt::WouldBlock,
        }
    }
}

impl Executor for MockExecutor {
    fn register_file(&self, _slot: u32, _file: File) -> Result<(), IoError> {
        Ok(())
    }

    fn clean_bytes(&self, kind: OpKind) -> u32 {
        match kind {
            OpKind::Fsync => 0,
            OpKind::Read | OpKind::Write => self.frame_bytes,
        }
    }

    fn schedule(&self, ready_len: usize) -> usize {
        rng_below(&mut self.lock().rng, ready_len + 1)
    }

    fn retire_file(&self, _slot: u32) {}

    #[cfg(any(feature = "mock", feature = "bench"))]
    fn copy_frame(&self, frame: ReadFrameIdx, out: &mut [u8]) -> usize {
        self.arena
            .get()
            .expect("the mock is composed with a frame arena before reads")
            .copy_frame(frame, out)
    }
}

/// Builds a [`MockRingDriver`] with all capacities fixed up front.
#[derive(Debug, Clone, Copy)]
pub struct MockRingDriverBuilder {
    seed: u64,
    queue_capacity: u32,
    frames: u32,
    frame_bytes: u32,
    write_slots: u32,
    retry_bound: u32,
}

impl Default for MockRingDriverBuilder {
    fn default() -> Self {
        Self {
            seed: 0,
            queue_capacity: 1,
            frames: 1,
            frame_bytes: 4096,
            write_slots: 1,
            retry_bound: 0,
        }
    }
}

impl MockRingDriverBuilder {
    #[must_use]
    pub fn seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    #[must_use]
    pub fn queue_capacity(mut self, queue_capacity: u32) -> Self {
        self.queue_capacity = queue_capacity;
        self
    }

    #[must_use]
    pub fn frames(mut self, frames: u32) -> Self {
        self.frames = frames;
        self
    }

    #[must_use]
    pub fn frame_bytes(mut self, frame_bytes: u32) -> Self {
        self.frame_bytes = frame_bytes;
        self
    }

    #[must_use]
    pub fn write_slots(mut self, write_slots: u32) -> Self {
        self.write_slots = write_slots;
        self
    }

    #[must_use]
    pub fn retry_bound(mut self, retry_bound: u32) -> Self {
        self.retry_bound = retry_bound;
        self
    }

    /// # Panics
    ///
    /// If any capacity is zero — capacities are fixed and positive at init.
    #[must_use]
    pub fn build(self) -> MockRingDriver {
        assert!(self.queue_capacity > 0, "queue capacity must be positive");
        assert!(self.frames > 0, "frame count must be positive");
        assert!(self.frame_bytes > 0, "frame size must be positive");
        assert!(self.write_slots > 0, "write slot count must be positive");
        let id = next_driver_id();
        let write_arena = shared_write_arena(self.write_slots, self.frame_bytes, id);
        let executor = MockRingExecutor::new(self.seed, self.frame_bytes, self.queue_capacity);
        let shared = Shared::new(
            CompletionSlab::with_capacity(self.queue_capacity),
            self.queue_capacity,
        );
        MockRingDriver(DriverCore::new(
            shared,
            executor,
            write_arena,
            self.frames,
            self.retry_bound,
            self.queue_capacity,
            id,
        ))
    }
}

/// A seeded in-memory driver over the REAL `DriverCore` ring poll path
/// (`poll_ring`/`fill_ring`/`reap_ring`), used to inject faults and adversarial
/// CQE orderings at the `io_uring` reap seam off the bench host. The mock supplies
/// only seeded CQE ordering and injected raw results; routing, slab reclaim,
/// deferred close, and the `EAGAIN`/`EINTR` resubmit are the core's, unchanged.
#[derive(Debug)]
pub struct MockRingDriver(DriverCore<MockRingExecutor>);

impl Drop for MockRingDriver {
    fn drop(&mut self) {
        self.0.quiesce_ring();
    }
}

impl MockRingDriver {
    #[must_use]
    pub fn builder() -> MockRingDriverBuilder {
        MockRingDriverBuilder::default()
    }

    /// Opens a fresh generational handle. Never touches disk.
    ///
    /// # Errors
    ///
    /// Returns `EMFILE` when the fixed fd table is exhausted.
    pub fn open(&self, path: &Path, _direct_io: DirectIo) -> Result<FileHandle, IoError> {
        self.0.open_mock(path)
    }

    /// Consumes the handle; the deferred close(2) is observable once the fd's
    /// in-flight ops drain (INV-11).
    ///
    /// # Panics
    ///
    /// If `fd` was minted by a different driver.
    pub fn close(&self, fd: FileHandle) {
        self.0.close(fd);
    }

    /// Whether the fd named by `id` has issued its deferred close(2).
    ///
    /// # Panics
    ///
    /// If `id` was minted by a different driver.
    #[must_use]
    pub fn is_closed(&self, id: FileId) -> bool {
        self.0.is_closed(id)
    }

    /// Mints a second reference to the same fd — the stale-handle test's ghost.
    #[must_use]
    pub fn duplicate_handle(&self, fd: &FileHandle) -> FileHandle {
        FileHandle::from_id(fd.file_id())
    }

    /// Binds a per-attempt fault SEQUENCE to the op the NEXT submit creates, keyed
    /// to that op's `user_data`, so a seeded reorder still lands each result on its
    /// own token. Each element maps to one CQE attempt on the bound op.
    pub fn inject_for_next_submit(&self, faults: &[Injected]) {
        self.0.executor().inject_for_next_submit(faults);
    }

    /// Enqueues a read; the next [`MockRingDriver::poll`] fills its SQE and reaps
    /// the seeded (possibly injected) CQE.
    ///
    /// # Errors
    ///
    /// [`SubmitError::StaleHandle`] for a stale fd, [`SubmitError::Full`] at
    /// capacity.
    ///
    /// # Panics
    ///
    /// If `fd` was minted by a different driver, or `frame` is out of range.
    pub fn submit_read(
        &self,
        fd: &FileHandle,
        frame: ReadFrameIdx,
        offset: u64,
    ) -> Result<OpToken, SubmitError> {
        let token =
            self.0
                .submit_read(fd, frame, offset, 0, self.0.executor().frame_bytes, true)?;
        self.0.executor().bind_pending(u64::from(token.slot()));
        Ok(token)
    }

    /// Enqueues a write from a retained staging slot through the real ring reap
    /// path, binding any pending injected CQE sequence to its slab slot.
    pub fn submit_write<'arena>(
        &self,
        fd: &FileHandle,
        buf: WriteSlot<'arena>,
        offset: u64,
    ) -> Result<OpToken, (SubmitError, WriteSlot<'arena>)> {
        let token = self.0.submit_write(fd, buf, offset)?;
        self.0.executor().bind_pending(u64::from(token.slot()));
        Ok(token)
    }

    /// Enqueues an fsync barrier.
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
        let token = self.0.submit_fsync(fd, mode)?;
        self.0.executor().bind_pending(u64::from(token.slot()));
        Ok(token)
    }

    /// Fills ready SQEs and reaps their CQEs through the real ring path (never
    /// sleeps); returns the completion count.
    ///
    /// # Panics
    ///
    /// If `out` has zero capacity.
    pub fn poll(&self, out: &mut CompletionBatch) -> usize {
        self.0.poll_ring(out)
    }

    /// Reaps like [`MockRingDriver::poll`], parking up to `timeout` when idle.
    pub fn poll_wait(&self, out: &mut CompletionBatch, timeout: Duration) -> usize {
        self.0.poll_wait_ring(out, timeout)
    }

    /// A shared observation of the ring state that survives the driver drop.
    #[must_use]
    pub fn observe(&self) -> Arc<MockRingObservation> {
        self.0.executor().observe()
    }

    /// Borrows the mock ring driver's fixed write-staging arena.
    #[must_use]
    pub fn write_arena(&self) -> MockWriteArena<'_> {
        MockWriteArena {
            state: self.0.write_arena_state(),
        }
    }

    /// The write tails filled into mock SQEs, in submission order.
    #[must_use]
    pub fn write_attempts_in_order(&self) -> Vec<WriteAttempt> {
        self.0.executor().write_attempts()
    }
}

/// Counters shared with a [`MockRingDriver`] through an [`Arc`] that outlives it,
/// so a test can assert the post-drop quiesce state (INV-8) and exactly-one-retire
/// per closed fd (INV-11).
#[derive(Debug, Default)]
pub struct MockRingObservation {
    submitted: AtomicU32,
    reaped: AtomicU32,
    retired: AtomicU32,
}

impl MockRingObservation {
    /// Ops submitted but not yet terminally reaped.
    #[must_use]
    pub fn ops_in_flight(&self) -> u32 {
        self.submitted
            .load(Ordering::Acquire)
            .saturating_sub(self.reaped.load(Ordering::Acquire))
    }

    /// Ops finalized in reap — a resubmitted `EAGAIN`/`EINTR` CQE is not counted.
    #[must_use]
    pub fn reaped(&self) -> u32 {
        self.reaped.load(Ordering::Acquire)
    }

    /// `retire_file` calls — one per fully-drained closed fd.
    #[must_use]
    pub fn retired(&self) -> u32 {
        self.retired.load(Ordering::Acquire)
    }
}

/// The mock ring backend: a seeded reorder PRNG, per-op injected fault sequences
/// keyed by `user_data`, and a ready-CQE queue produced at SQE-fill. It holds no
/// per-file state — routing, reclaim, and retire progression are the core's.
#[derive(Debug)]
struct MockRingExecutor {
    state: Mutex<MockRingState>,
    frame_bytes: u32,
    observation: Arc<MockRingObservation>,
}

#[derive(Debug)]
struct MockRingState {
    rng: u64,
    pending: Option<Vec<Injected>>,
    faults: HashMap<u64, VecDeque<Injected>>,
    cqes: VecDeque<(u64, i32)>,
    write_attempts: Vec<WriteAttempt>,
}

impl MockRingExecutor {
    fn new(seed: u64, frame_bytes: u32, queue_capacity: u32) -> Self {
        let capacity = queue_capacity as usize;
        Self {
            state: Mutex::new(MockRingState {
                rng: seed,
                pending: None,
                faults: HashMap::with_capacity(capacity),
                cqes: VecDeque::with_capacity(capacity),
                write_attempts: Vec::with_capacity(capacity),
            }),
            frame_bytes,
            observation: Arc::new(MockRingObservation::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, MockRingState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn observe(&self) -> Arc<MockRingObservation> {
        Arc::clone(&self.observation)
    }

    fn write_attempts(&self) -> Vec<WriteAttempt> {
        self.lock().write_attempts.clone()
    }

    fn inject_for_next_submit(&self, faults: &[Injected]) {
        self.lock().pending = Some(faults.to_vec());
    }

    /// Binds any queued injection to the just-submitted op's `user_data` and
    /// counts the op in flight. Called once per admitted submit.
    fn bind_pending(&self, user_data: u64) {
        let mut state = self.lock();
        if let Some(faults) = state.pending.take() {
            state.faults.insert(user_data, faults.into());
        }
        drop(state);
        self.observation.submitted.fetch_add(1, Ordering::AcqRel);
    }

    /// The raw CQE the next attempt on `user_data` reports: the bound sequence's
    /// next element, or a clean `clean_bytes` transfer once it is exhausted.
    fn next_raw(state: &mut MockRingState, user_data: u64, clean_bytes: u32) -> i32 {
        let fault = state
            .faults
            .get_mut(&user_data)
            .and_then(VecDeque::pop_front);
        match fault {
            None => i32::try_from(clean_bytes).expect("a clean transfer fits i32"),
            Some(Injected::Short(bytes)) => i32::try_from(bytes).expect("a short count fits i32"),
            Some(Injected::Io(errno)) => -errno,
            Some(Injected::Eagain) => -EAGAIN,
            Some(Injected::Eintr) => -EINTR,
        }
    }
}

impl Executor for MockRingExecutor {
    fn register_file(&self, _slot: u32, _file: File) -> Result<(), IoError> {
        Ok(())
    }

    fn clean_bytes(&self, kind: OpKind) -> u32 {
        match kind {
            OpKind::Fsync => 0,
            OpKind::Read | OpKind::Write => self.frame_bytes,
        }
    }

    fn schedule(&self, ready_len: usize) -> usize {
        rng_below(&mut self.lock().rng, ready_len + 1)
    }

    fn retire_file(&self, _slot: u32) {
        self.observation.retired.fetch_add(1, Ordering::AcqRel);
    }

    #[cfg(any(feature = "mock", feature = "bench"))]
    fn copy_frame(&self, _frame: ReadFrameIdx, _out: &mut [u8]) -> usize {
        panic!("the ring mock carries completion ordering, not frame bytes")
    }
}

impl RingExecutor for MockRingExecutor {
    fn push_read(
        &self,
        user_data: u64,
        _fd_slot: u32,
        _frame: ReadFrameIdx,
        _file_offset: u64,
        _destination_offset: u32,
        len: u32,
    ) {
        let mut state = self.lock();
        let raw = Self::next_raw(&mut state, user_data, len);
        state.cqes.push_back((user_data, raw));
    }

    fn push_write(
        &self,
        user_data: u64,
        _fd_slot: u32,
        source: *const u8,
        source_offset: u32,
        file_offset: u64,
        requested_len: u32,
    ) {
        assert!(
            !source.is_null(),
            "a mock write receives a live staging slot"
        );
        let mut state = self.lock();
        state.write_attempts.push(WriteAttempt {
            file_offset,
            source_offset,
            requested_len,
        });
        let raw = Self::next_raw(&mut state, user_data, requested_len);
        state.cqes.push_back((user_data, raw));
    }

    fn push_fsync(&self, user_data: u64, _fd_slot: u32) {
        let mut state = self.lock();
        let raw = Self::next_raw(&mut state, user_data, 0);
        state.cqes.push_back((user_data, raw));
    }

    fn submit(&self) {}

    fn submit_and_wait(&self, _want: u32, _timeout: Duration) {}

    fn reap<F: FnMut(u64, i32)>(&self, limit: u32, mut sink: F) -> u32 {
        assert!(limit > 0, "a reap drains into a non-empty batch");
        let mut state = self.lock();
        let mut reaped = 0u32;
        while reaped < limit {
            let Some((user_data, raw)) = state.cqes.pop_front() else {
                break;
            };
            sink(user_data, raw);
            reaped += 1;
        }
        reaped
    }

    fn on_op_finalized(&self) {
        self.observation.reaped.fetch_add(1, Ordering::AcqRel);
    }

    #[cfg(target_os = "linux")]
    fn blocking_write(&self, _fd_slot: u32, _buf: &[u8], _offset: u64) -> Result<u32, i32> {
        unreachable!("the mock ring exposes no metadata plane")
    }

    #[cfg(target_os = "linux")]
    fn blocking_fsync(&self, _fd_slot: u32) -> Result<(), i32> {
        unreachable!("the mock ring exposes no metadata plane")
    }
}

fn rng_below(state: &mut u64, bound: usize) -> usize {
    let next = splitmix64(state);
    let bound_u64 = u64::try_from(bound).unwrap_or(u64::MAX).max(1);
    usize::try_from(next % bound_u64).unwrap_or(0)
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}
