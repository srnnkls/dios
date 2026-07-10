//! Seeded, deterministically-reordering in-memory backend — the DST seam.
//!
//! [`MockDriver`] is a thin newtype over the shared driver core, so it cannot
//! drift from the real driver: it adds only what a backend owns — fault
//! injection, seed-derived completion reordering, and simulated (instant)
//! execution. Admission, lease tracking, the retry policy, and finalization all
//! live in the core. Backend behaviour is never selected by matching
//! [`Backend`](crate::Backend) (AD-1); the mock is a distinct type.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::completion::CompletionBatch;
use crate::driver::{
    Attempt, CompletionSlab, DriverCore, Executor, FileHandle, FileId, OpContext, OpKind, OpToken,
    OpenHow, ReadFrameIdx, Shared, SyncMode, WriteArena, WriteSlot,
};
use crate::error::{IoError, SubmitError};

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
    retry_bound: u32,
}

impl Default for MockDriverBuilder {
    fn default() -> Self {
        Self {
            seed: 0,
            queue_capacity: 1,
            frames: 1,
            frame_bytes: 4096,
            retry_bound: 0,
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
    pub fn retry_bound(mut self, retry_bound: u32) -> Self {
        self.retry_bound = retry_bound;
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
        let executor = MockExecutor::new(self.seed, self.frame_bytes, self.queue_capacity);
        let shared = Shared::new(
            CompletionSlab::with_capacity(self.queue_capacity),
            self.queue_capacity,
        );
        MockDriver(DriverCore::new(
            shared,
            executor,
            self.frames,
            self.retry_bound,
        ))
    }
}

/// A seeded in-memory driver used as the deterministic test backend.
#[derive(Debug)]
pub struct MockDriver(DriverCore<MockExecutor>);

impl MockDriver {
    #[must_use]
    pub fn builder() -> MockDriverBuilder {
        MockDriverBuilder::default()
    }

    /// Opens a fresh generational handle. Never touches disk.
    ///
    /// # Errors
    ///
    /// Returns `EMFILE` when the fixed fd table is exhausted.
    pub fn open(&self, path: &Path, how: OpenHow) -> Result<FileHandle, IoError> {
        self.0.open(path, how)
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
        self.0.submit_read(fd, frame, offset)
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

    /// A fresh write-staging arena of `slot_count` granule-aligned slots.
    #[must_use]
    pub fn write_arena(&self, slot_count: u32) -> WriteArena {
        WriteArena::new(slot_count)
    }
}

/// The mock backend's own state: injected faults and the reordering PRNG behind
/// an internal mutex (the execute phase runs outside the core's submit lock),
/// plus the immutable clean transfer size. Admission, leases, retries, and
/// finalization are the core's, not the mock's.
#[derive(Debug)]
struct MockExecutor {
    state: Mutex<MockState>,
    frame_bytes: u32,
}

#[derive(Debug)]
struct MockState {
    injected: VecDeque<Injected>,
    rng: u64,
}

impl MockExecutor {
    fn new(seed: u64, frame_bytes: u32, injected_capacity: u32) -> Self {
        Self {
            state: Mutex::new(MockState {
                injected: VecDeque::with_capacity(injected_capacity as usize),
                rng: seed,
            }),
            frame_bytes,
        }
    }

    fn lock(&self) -> MutexGuard<'_, MockState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn inject(&self, fault: Injected) {
        self.lock().injected.push_back(fault);
    }
}

impl Executor for MockExecutor {
    fn attempt(&self, _kind: OpKind, clean_bytes: u32, _context: OpContext<'_>) -> Attempt {
        match self.lock().injected.pop_front() {
            None => Attempt::Done(clean_bytes),
            Some(Injected::Io(errno)) => Attempt::Failed(errno),
            Some(Injected::Short(bytes)) => Attempt::Done(bytes),
            Some(Injected::Eintr) => Attempt::Interrupted,
            Some(Injected::Eagain) => Attempt::WouldBlock,
        }
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
