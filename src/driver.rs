//! The shared driver core and its public surface.
//!
//! [`DriverCore`] owns everything a backend must not diverge on: the fd table
//! and its deferred-close-past-drain (INV-11), op-slot lease tracking, the
//! `EINTR`/`EAGAIN` retry policy and its init-time bound, submit admission
//! (`is_live` → reserve → fill → `on_submit`), and completion finalization
//! (reclaim, close progression, lease release, publish). It composes an
//! [`Executor`] backend that lives outside the submit mutex, so the execute
//! phase (a real syscall on the eager/uring backends) runs without the lock
//! held; the mock synchronises its own injected state internally. Op routing is
//! never selected by matching a runtime tag (AD-1). Both [`Driver`] and the mock
//! compose the same core.

use std::collections::VecDeque;
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::alignment::Alignment;
use crate::backend;
use crate::completion::{Completion, CompletionBatch};
use crate::error::{IoError, SubmitError};
use crate::pool::write_arena::{WriteLease, WriteSlot};

pub(crate) const MAX_FILES: u32 = 64;
const EINTR: i32 = 4;
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

/// Builds a [`Driver`] with every capacity fixed up front. Capacities are the
/// only inputs; a backend that opens a ring asserts its init at build, so the
/// signature stays infallible.
#[derive(Debug, Clone, Copy)]
pub struct DriverBuilder {
    queue_capacity: u32,
    frames: u32,
    frame_bytes: u32,
    retry_bound: u32,
}

impl Default for DriverBuilder {
    fn default() -> Self {
        Self {
            queue_capacity: 1,
            frames: 1,
            frame_bytes: 4096,
            retry_bound: 0,
        }
    }
}

impl DriverBuilder {
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
    pub fn build(self) -> Driver {
        assert!(self.queue_capacity > 0, "queue capacity must be positive");
        assert!(self.frames > 0, "frame count must be positive");
        assert!(self.frame_bytes > 0, "frame size must be positive");
        let executor = backend::Impl::new(self.frames, self.frame_bytes, self.queue_capacity);
        let shared = Shared::new(
            CompletionSlab::with_capacity(self.queue_capacity),
            self.queue_capacity,
        );
        Driver(DriverCore::new(
            shared,
            executor,
            self.frames,
            self.retry_bound,
            self.queue_capacity,
        ))
    }
}

impl Driver {
    /// The backend selected for the target platform.
    pub const BACKEND: Backend = backend::Impl::KIND;

    #[must_use]
    pub fn builder() -> DriverBuilder {
        DriverBuilder::default()
    }

    /// Opens an existing file read-write, probing the direct-IO mode `how`
    /// requests; the outcome rides in the handle's [`FileHandle::io_mode`] as an
    /// observable enum (scope Constraints). No create mode in v1.
    ///
    /// # Errors
    ///
    /// The open syscall's operating failure (`ENOENT`, `EACCES`, …), or `EMFILE`
    /// when the fixed fd table is exhausted.
    pub fn open(&self, path: &Path, how: OpenHow) -> Result<FileHandle, IoError> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        let io_mode = crate::open::probe(&file, how.io_request())?;
        let id = self.0.reserve_file()?;
        if let Err(error) = self.0.executor().register_file(id.slot(), file) {
            self.0.abort_file(id);
            return Err(error);
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
    pub fn submit_read(
        &self,
        fd: &FileHandle,
        frame: ReadFrameIdx,
        offset: u64,
    ) -> Result<OpToken, SubmitError> {
        self.0.submit_read(fd, frame, offset)
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

    /// Drains completions like [`Driver::poll`], parking in the kernel for up to
    /// `timeout` when none are ready. The wait runs outside the AD-4 submit mutex
    /// (INV-3), so a concurrent [`Driver::submit_read`] never blocks on the poller.
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

    /// Copies a drained read's frame bytes out of the eager slab into `out`,
    /// returning the byte count.
    ///
    /// Provisional pre-T007 read-observation seam, subsumed by [`FrameGuard`](crate::FrameGuard)
    /// once the pool's epoch pins land.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range for the configured frame count.
    #[doc(hidden)]
    pub fn copy_frame(&self, frame: ReadFrameIdx, out: &mut [u8]) -> usize {
        self.0.executor().copy_frame(frame, out)
    }
}

/// The ring backend drains its kernel-visible ops before teardown (INV-8). The
/// eager backend has no async op outstanding at drop — `poll` executes inline —
/// so it needs no quiesce.
#[cfg(target_os = "linux")]
impl Drop for Driver {
    fn drop(&mut self) {
        self.0.quiesce_ring();
    }
}

/// The three op kinds the driver issues; echoed in each completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind {
    Read,
    Write,
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

/// Which read-pool frame a read lands in. Reuse is governed by the pool's frame
/// state machine (INV-1), not by this index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReadFrameIdx(u32);

impl ReadFrameIdx {
    #[must_use]
    pub fn new(frame: u32) -> Self {
        Self(frame)
    }

    pub(crate) fn get(self) -> u32 {
        self.0
    }
}

/// Durability barrier requested by an fsync op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyncMode {
    Full,
}

/// How a file is opened. `read_write` is the only access mode v1 issues; the
/// `IoRequest` selects the buffered or direct-IO data plane, probed at
/// [`Driver::open`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpenHow {
    access: Access,
    io_request: crate::open::IoRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Access {
    ReadWrite,
}

impl OpenHow {
    #[must_use]
    pub fn read_write() -> Self {
        Self {
            access: Access::ReadWrite,
            io_request: crate::open::IoRequest::Buffered,
        }
    }

    /// Requests direct IO (`O_DIRECT` on Linux, `F_NOCACHE` on darwin). The probe
    /// reports the outcome as [`IoMode`]; a filesystem without direct support
    /// falls back to [`IoMode::Buffered`] (scope Constraints).
    #[must_use]
    pub fn direct(mut self) -> Self {
        self.io_request = crate::open::IoRequest::Direct;
        self
    }

    pub(crate) fn io_request(self) -> crate::open::IoRequest {
        self.io_request
    }
}

/// How a file's data plane transfers: direct with a probed sector [`Alignment`],
/// or buffered through the page cache. An observable enum, never a silent bool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IoMode {
    Direct(Alignment),
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

/// A driver-owned open file. `!Copy`: `close` consumes it, and only
/// [`MockDriver::duplicate_handle`](crate::mock::MockDriver::duplicate_handle)-style
/// minting produces a second reference.
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
    pub(crate) fn with_capacity(capacity: u32) -> Self {
        let mut slots = Vec::with_capacity(capacity as usize);
        for _ in 0..capacity {
            slots.push(SlabSlot {
                generation: 0,
                payload: None,
            });
        }
        let mut free = Vec::with_capacity(capacity as usize);
        for slot in (0..capacity).rev() {
            free.push(slot);
        }
        Self {
            slots: slots.into_boxed_slice(),
            free,
        }
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

    /// The position at which a freshly admitted op joins the ready order given
    /// the current queue length (seeded reordering in the mock; a real backend
    /// appends at `ready_len`).
    fn schedule(&self, ready_len: usize) -> usize;

    /// Releases the backend state an fd slot holds, called once the core advances
    /// it `Closing → Closed` in publish (its last in-flight op drained). Eager
    /// drops the retained `File`, closing the descriptor and freeing the slot for
    /// reuse; the mock keeps no per-file state and no-ops.
    fn retire_file(&self, slot: u32);
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
    /// The transfer length a clean full-frame read reports (registered-buffer
    /// size).
    fn read_len(&self) -> u32;

    /// Fills one read SQE addressing the registered buffer for `frame` at
    /// `offset`, tagged with `user_data`. Called under the AD-4 mutex.
    fn push_read(&self, user_data: u64, fd_slot: u32, frame: ReadFrameIdx, offset: u64, len: u32);

    /// Fills one fsync SQE, tagged with `user_data`. Called under the AD-4 mutex.
    fn push_fsync(&self, user_data: u64, fd_slot: u32);

    /// Submits filled SQEs without blocking on completions (`min_complete = 0`),
    /// so poll never sleeps awaiting events. Runs OUTSIDE the AD-4 mutex.
    fn submit(&self);

    /// Submits filled SQEs and parks up to `timeout` for at least `want`
    /// completions via the `EXT_ARG` kernel wait. Runs OUTSIDE the AD-4 mutex.
    fn submit_and_wait(&self, want: u32, timeout: Duration);

    /// Drains at most `limit` ready CQEs, routing each `(user_data, raw_result)`
    /// to `sink`, and returns the count. Called under the AD-4 mutex.
    fn reap<F: FnMut(u64, i32)>(&self, limit: u32, sink: F) -> u32;

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

#[derive(Debug)]
pub(crate) struct OpEntry {
    kind: OpKind,
    fd: FileId,
    offset: u64,
    frame: ReadFrameIdx,
    lease: Option<WriteLease>,
    retries: u32,
}

/// Per-op parameters the execute phase hands a backend: which file, at what
/// offset, into (reads) or out of (writes) which resource. The mock ignores it
/// and replays injected faults; the eager backend performs the real syscall.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OpContext<'buf> {
    pub(crate) fd: FileId,
    pub(crate) offset: u64,
    pub(crate) frame: ReadFrameIdx,
    pub(crate) write_buf: &'buf [u8],
}

impl<'buf> OpContext<'buf> {
    fn new(fd: FileId, offset: u64, frame: ReadFrameIdx, write_buf: &'buf [u8]) -> Self {
        Self {
            fd,
            offset,
            frame,
            write_buf,
        }
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
}

impl Shared {
    pub(crate) fn new(slab: CompletionSlab<OpEntry>, ready_capacity: u32) -> Self {
        Self {
            slab,
            files: FileTable::new(),
            ready: VecDeque::with_capacity(ready_capacity as usize),
        }
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
    executor: E,
    frames: u32,
    retry_bound: u32,
    queue_capacity: u32,
    id: u64,
}

impl<E> DriverCore<E> {
    pub(crate) fn new(
        shared: Shared,
        executor: E,
        frames: u32,
        retry_bound: u32,
        queue_capacity: u32,
    ) -> Self {
        Self {
            inner: Mutex::new(shared),
            executor,
            frames,
            retry_bound,
            queue_capacity,
            id: NEXT_DRIVER_ID.fetch_add(1, Ordering::Relaxed),
        }
    }

    fn inflight_total(&self) -> u32 {
        self.lock().files.total_inflight()
    }

    pub(crate) fn executor(&self) -> &E {
        &self.executor
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

    pub(crate) fn open(&self, path: &Path, how: OpenHow) -> Result<FileHandle, IoError> {
        let _ = how;
        debug_assert!(!path.as_os_str().is_empty(), "open path must be non-empty");
        let mut shared = self.lock();
        match shared.files.open(self.id) {
            Some(id) => Ok(FileHandle::from_id(id)),
            None => Err(IoError::from_raw(EMFILE)),
        }
    }

    pub(crate) fn reserve_file(&self) -> Result<FileId, IoError> {
        let mut shared = self.lock();
        shared
            .files
            .open(self.id)
            .ok_or_else(|| IoError::from_raw(EMFILE))
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
    }

    pub(crate) fn submit_read(
        &self,
        fd: &FileHandle,
        frame: ReadFrameIdx,
        offset: u64,
    ) -> Result<OpToken, SubmitError> {
        assert!(frame.get() < self.frames, "read frame index out of range");
        if let IoMode::Direct(alignment) = fd.io_mode() {
            assert!(
                alignment.check(offset).is_ok(),
                "offset {offset} is not aligned to {} bytes",
                alignment.get()
            );
        }
        let mut shared = self.lock();
        let slot = self.admit(&mut shared, fd.file_id())?;
        let entry = OpEntry {
            kind: OpKind::Read,
            fd: fd.file_id(),
            offset,
            frame,
            lease: None,
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
                let entry = OpEntry {
                    kind: OpKind::Write,
                    fd: fd.file_id(),
                    offset,
                    frame: ReadFrameIdx::new(0),
                    lease: Some(buf.into_lease()),
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
            offset: 0,
            frame: ReadFrameIdx::new(0),
            lease: None,
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
            shared.slab.reserve().ok_or(SubmitError::Full)
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
        let mut drained = 0usize;
        while drained < out.capacity() {
            let Some((slot, kind, fd, offset, frame)) = self.prepare_next() else {
                break;
            };
            let clean_bytes = self.executor.clean_bytes(kind);
            let context = OpContext::new(fd, offset, frame, &[]);
            let outcome = self.execute(kind, clean_bytes, context);
            self.publish(slot, outcome, out);
            drained += 1;
        }
        drained
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

    /// Prepare (locked): take the next ready slot and read its op kind. The slab
    /// slot stays occupied — it is reclaimed only in [`DriverCore::publish`].
    fn prepare_next(&self) -> Option<(u32, OpKind, FileId, u64, ReadFrameIdx)> {
        let mut shared = self.lock();
        let slot = shared.ready.pop_front()?;
        let entry = shared.slab.peek(slot);
        Some((slot, entry.kind, entry.fd, entry.offset, entry.frame))
    }

    /// Execute (submit mutex *not* held): the retry policy and its fixed bound
    /// over the backend's simulated or real syscall. `EINTR` resubmits on every
    /// op, `EAGAIN` resubmits on reads but surfaces on writes and fsync.
    fn execute(&self, kind: OpKind, clean_bytes: u32, context: OpContext<'_>) -> Result<u32, i32> {
        let retry_would_block = matches!(kind, OpKind::Read);
        let mut retries = 0u32;
        loop {
            match self.executor.attempt(kind, clean_bytes, context) {
                Attempt::Done(bytes) => return Ok(bytes),
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
            if let Some(lease) = entry.lease {
                lease.release();
            }
            let result = outcome.map_err(IoError::from_raw);
            out.push(Completion::new(token, entry.kind, result));
            retire_due.then_some(entry.fd)
        };
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
            let context = OpContext::new(
                fd.file_id(),
                offset + u64::from(written),
                ReadFrameIdx::new(0),
                &buf[written as usize..],
            );
            match self.execute(OpKind::Write, remaining, context) {
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
        let context = OpContext::new(fd.file_id(), 0, ReadFrameIdx::new(0), &[]);
        match self.execute(OpKind::Fsync, 0, context) {
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
        assert!(
            out.capacity() > 0,
            "poll requires a non-empty completion batch"
        );
        out.reset();
        let cap = u32::try_from(out.capacity()).expect("batch capacity fits u32");
        let filled = self.fill_ring(cap);
        if filled > 0 {
            self.executor.submit();
        }
        self.reap_ring(out)
    }

    pub(crate) fn poll_wait_ring(&self, out: &mut CompletionBatch, timeout: Duration) -> usize {
        assert!(
            out.capacity() > 0,
            "poll requires a non-empty completion batch"
        );
        out.reset();
        let cap = u32::try_from(out.capacity()).expect("batch capacity fits u32");
        let filled = self.fill_ring(cap);
        self.executor.submit_and_wait(filled.max(1), timeout);
        self.reap_ring(out)
    }

    /// Prepare (locked): drain ready slots into SQEs, tagging each with its slab
    /// slot as `user_data`. Slots stay occupied — reclaimed only in reap.
    fn fill_ring(&self, cap: u32) -> u32 {
        let len = self.executor.read_len();
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
                    self.executor
                        .push_read(user_data, fd_slot, entry.frame, entry.offset, len);
                }
                OpKind::Fsync => self.executor.push_fsync(user_data, fd_slot),
                OpKind::Write => {
                    unreachable!(
                        "the ring poll path issues reads and fsyncs; async writes land in T006"
                    )
                }
            }
            filled += 1;
        }
        filled
    }

    /// Reap/finalize (locked): drain CQEs, routing each by echoed slot id. A
    /// retryable `-EAGAIN`/`-EINTR` result under the init-time bound keeps the op
    /// live — its slot is re-queued for a fresh SQE, not reclaimed, and no
    /// completion is emitted (scope.md:596). A terminal result reclaims the slot,
    /// drops the in-flight count (progressing a deferred close), releases the
    /// write lease, and emits the completion. Retires run after the lock is
    /// released.
    fn reap_ring(&self, out: &mut CompletionBatch) -> usize {
        let limit = u32::try_from(out.capacity()).expect("batch capacity fits u32");
        let retry_bound = self.retry_bound;
        let mut retire = [FileId::new(0, 0, 0); MAX_FILES as usize];
        let mut retire_len = 0usize;
        let mut drained = 0usize;
        {
            let mut shared = self.lock();
            let shared = &mut *shared;
            self.executor.reap(limit, |user_data, raw| {
                let slot = u32::try_from(user_data).expect("uring user_data is a slab slot");
                let entry = shared.slab.peek_mut(slot);
                if ring_should_retry(entry.kind, raw, entry.retries, retry_bound) {
                    entry.retries += 1;
                    shared.ready.push_back(slot);
                    return;
                }
                let (token, entry) = shared.slab.reclaim(slot);
                let retire_due = shared.files.on_complete(entry.fd);
                if let Some(lease) = entry.lease {
                    lease.release();
                }
                out.push(Completion::new(token, entry.kind, ring_result(raw)));
                if retire_due {
                    assert!(retire_len < retire.len(), "at most one retire per open fd");
                    retire[retire_len] = entry.fd;
                    retire_len += 1;
                }
                self.executor.on_op_finalized();
                drained += 1;
            });
        }
        for fd in &retire[..retire_len] {
            self.retire(*fd);
        }
        drained
    }

    /// Drains every in-flight op before teardown so no kernel-visible op is
    /// abandoned (INV-8). Bounded: each poll that makes no progress counts against
    /// a fixed cap.
    pub(crate) fn quiesce_ring(&self) {
        if self.inflight_total() == 0 {
            return;
        }
        let capacity = self.queue_capacity.max(1) as usize;
        let mut out = CompletionBatch::with_capacity(capacity);
        let mut idle = 0u32;
        while self.inflight_total() > 0 {
            if self.poll_ring(&mut out) > 0 {
                idle = 0;
            } else {
                idle += 1;
                assert!(idle < QUIESCE_IDLE_MAX, "drop quiesce made no progress");
            }
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
    fn new() -> Self {
        let slot = FileSlot {
            generation: 0,
            state: FileState::Free,
            inflight: 0,
        };
        Self {
            slots: vec![slot; MAX_FILES as usize].into_boxed_slice(),
        }
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
        slot.generation == id.generation() && matches!(slot.state, FileState::Closed)
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
