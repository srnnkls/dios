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
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::backend;
use crate::completion::{Completion, CompletionBatch};
use crate::error::{IoError, SubmitError};

const MAX_FILES: u32 = 64;
const EINTR: i32 = 4;
const EBADF: i32 = 9;
const EMFILE: i32 = 24;
#[cfg(target_os = "linux")]
const EAGAIN: i32 = 11;
#[cfg(not(target_os = "linux"))]
const EAGAIN: i32 = 35;
const POLL_WAIT_QUANTUM: Duration = Duration::from_millis(5);

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

/// The public driver over the cfg-selected backend. Its real submit/poll surface
/// arrives with the eager and uring backends (T003/T004); it already composes
/// the same driver core the mock uses, so the two cannot structurally drift.
#[derive(Debug)]
pub struct Driver(
    #[expect(
        dead_code,
        reason = "the submit/poll surface that reads the composed core arrives with the eager and uring backends (T003/T004)"
    )]
    DriverCore<backend::Impl>,
);

impl Driver {
    /// The backend selected for the target platform.
    pub const BACKEND: Backend = backend::Impl::KIND;
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

/// How a file is opened. `read_write` is the only access mode v1 issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpenHow {
    access: Access,
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
        }
    }
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
}

impl FileHandle {
    pub(crate) fn from_id(id: FileId) -> Self {
        Self { id }
    }

    #[must_use]
    pub fn file_id(&self) -> FileId {
        self.id
    }
}

/// Granule-aligned write staging (the `O_DIRECT` data plane), separate from the
/// read pool. Slots are leased by [`WriteArena::alloc`] and freed on drop or at
/// the completion drain of the write that consumed them (INV-11). The aligned
/// buffer backing is T006's task; this holds only the slot bookkeeping.
#[derive(Debug)]
pub struct WriteArena {
    state: Arc<ArenaState>,
}

#[derive(Debug)]
struct ArenaState {
    free: Box<[AtomicBool]>,
}

impl WriteArena {
    pub(crate) fn new(slot_count: u32) -> Self {
        let mut free = Vec::with_capacity(slot_count as usize);
        for _ in 0..slot_count {
            free.push(AtomicBool::new(true));
        }
        Self {
            state: Arc::new(ArenaState {
                free: free.into_boxed_slice(),
            }),
        }
    }

    /// Leases a free staging slot, or `None` when every slot is in use. The
    /// lease borrows the arena; no refcount is taken until the slot is consumed
    /// by a submit.
    #[must_use]
    pub fn alloc(&self) -> Option<WriteSlot<'_>> {
        for (index, cell) in self.state.free.iter().enumerate() {
            let was_free = cell.swap(false, Ordering::AcqRel);
            if was_free {
                let slot = u32::try_from(index).ok()?;
                return Some(WriteSlot {
                    arena: &self.state,
                    slot,
                    consumed: false,
                });
            }
        }
        None
    }
}

/// A leased write-staging slot, borrowed from its [`WriteArena`]. Dropped
/// unsubmitted, it frees at once; consumed by `submit_write`, it frees only when
/// the write's completion drains.
#[derive(Debug)]
pub struct WriteSlot<'arena> {
    arena: &'arena Arc<ArenaState>,
    slot: u32,
    consumed: bool,
}

impl WriteSlot<'_> {
    pub(crate) fn into_lease(mut self) -> WriteLease {
        self.consumed = true;
        WriteLease {
            state: Arc::clone(self.arena),
            slot: self.slot,
            released: false,
        }
    }
}

impl Drop for WriteSlot<'_> {
    fn drop(&mut self) {
        if !self.consumed {
            self.arena.free[self.slot as usize].store(true, Ordering::Release);
        }
    }
}

/// Ownership of a leased slot moved into an in-flight write. The slot is freed
/// exactly once — at completion drain via [`WriteLease::release`], or as a
/// teardown net through `Drop` if the write is never drained.
#[derive(Debug)]
pub(crate) struct WriteLease {
    state: Arc<ArenaState>,
    slot: u32,
    released: bool,
}

impl WriteLease {
    pub(crate) fn release(mut self) {
        self.free_once();
    }

    fn free_once(&mut self) {
        if !self.released {
            self.state.free[self.slot as usize].store(true, Ordering::Release);
            self.released = true;
        }
    }
}

impl Drop for WriteLease {
    fn drop(&mut self) {
        self.free_once();
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

/// The backend-execution seam the shared core composes over (generics only, no
/// dyn — AD-1). The backend lives outside the submit mutex, so `attempt` runs
/// without the lock held (a real syscall on the eager/uring backends); the mock
/// synchronises its own injected state internally. All methods take `&self`.
pub(crate) trait Executor {
    /// One attempt at the op named by `kind`; `clean_bytes` is the transfer a
    /// fault-free op reports. Runs in the execute phase, no submit lock held.
    fn attempt(&self, kind: OpKind, clean_bytes: u32) -> Attempt;

    /// The transfer byte count a clean `kind` op reports.
    fn clean_bytes(&self, kind: OpKind) -> u32;

    /// The position at which a freshly admitted op joins the ready order given
    /// the current queue length (seeded reordering in the mock; a real backend
    /// appends at `ready_len`).
    fn schedule(&self, ready_len: usize) -> usize;
}

#[derive(Debug)]
pub(crate) struct OpEntry {
    kind: OpKind,
    fd: FileId,
    lease: Option<WriteLease>,
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
    id: u64,
}

impl<E> DriverCore<E> {
    pub(crate) fn new(shared: Shared, executor: E, frames: u32, retry_bound: u32) -> Self {
        Self {
            inner: Mutex::new(shared),
            executor,
            frames,
            retry_bound,
            id: NEXT_DRIVER_ID.fetch_add(1, Ordering::Relaxed),
        }
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

    #[expect(
        clippy::needless_pass_by_value,
        reason = "close consumes the handle by value so it cannot be reused (INV-11)"
    )]
    pub(crate) fn close(&self, fd: FileHandle) {
        self.assert_own(fd.file_id());
        let mut shared = self.lock();
        shared.files.close(fd.file_id());
    }

    pub(crate) fn is_closed(&self, id: FileId) -> bool {
        self.assert_own(id);
        self.lock().files.is_closed(id)
    }
}

impl<E: Executor> DriverCore<E> {
    pub(crate) fn submit_read(
        &self,
        fd: &FileHandle,
        frame: ReadFrameIdx,
        offset: u64,
    ) -> Result<OpToken, SubmitError> {
        let _ = offset;
        assert!(frame.get() < self.frames, "read frame index out of range");
        let mut shared = self.lock();
        let slot = self.admit(&mut shared, fd.file_id())?;
        let entry = OpEntry {
            kind: OpKind::Read,
            fd: fd.file_id(),
            lease: None,
        };
        Ok(self.commit(&mut shared, slot, entry))
    }

    pub(crate) fn submit_write<'arena>(
        &self,
        fd: &FileHandle,
        buf: WriteSlot<'arena>,
        offset: u64,
    ) -> Result<OpToken, (SubmitError, WriteSlot<'arena>)> {
        let _ = offset;
        let mut shared = self.lock();
        match self.admit(&mut shared, fd.file_id()) {
            Ok(slot) => {
                let entry = OpEntry {
                    kind: OpKind::Write,
                    fd: fd.file_id(),
                    lease: Some(buf.into_lease()),
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
            lease: None,
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

    pub(crate) fn poll(&self, out: &mut CompletionBatch) -> usize {
        assert!(
            out.capacity() > 0,
            "poll requires a non-empty completion batch"
        );
        out.reset();
        let mut drained = 0usize;
        while drained < out.capacity() {
            let Some((slot, kind)) = self.prepare_next() else {
                break;
            };
            let clean_bytes = self.executor.clean_bytes(kind);
            let outcome = self.execute(kind, clean_bytes);
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
    fn prepare_next(&self) -> Option<(u32, OpKind)> {
        let mut shared = self.lock();
        let slot = shared.ready.pop_front()?;
        let kind = shared.slab.peek(slot).kind;
        Some((slot, kind))
    }

    /// Execute (submit mutex *not* held): the retry policy and its fixed bound
    /// over the backend's simulated or real syscall. `EINTR` resubmits on every
    /// op, `EAGAIN` resubmits on reads but surfaces on writes and fsync.
    fn execute(&self, kind: OpKind, clean_bytes: u32) -> Result<u32, i32> {
        let retry_would_block = matches!(kind, OpKind::Read);
        let mut retries = 0u32;
        loop {
            match self.executor.attempt(kind, clean_bytes) {
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
        let mut shared = self.lock();
        let (token, entry) = shared.slab.reclaim(slot);
        shared.files.on_complete(entry.fd);
        if let Some(lease) = entry.lease {
            lease.release();
        }
        let result = outcome.map_err(IoError::from_raw);
        out.push(Completion::new(token, entry.kind, result));
    }

    pub(crate) fn write_all_blocking(
        &self,
        fd: &FileHandle,
        buf: &[u8],
        offset: u64,
    ) -> Result<(), IoError> {
        let _ = offset;
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
            match self.execute(OpKind::Write, remaining) {
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
        match self.execute(OpKind::Fsync, 0) {
            Ok(_) => Ok(()),
            Err(errno) => Err(IoError::from_raw(errno)),
        }
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

    fn on_complete(&mut self, id: FileId) {
        let slot = &mut self.slots[id.slot() as usize];
        debug_assert_eq!(
            slot.generation,
            id.generation(),
            "completion for a stale fd generation"
        );
        debug_assert!(slot.inflight > 0, "completion without a matching submit");
        slot.inflight -= 1;
        if slot.inflight == 0 && matches!(slot.state, FileState::Closing) {
            slot.state = FileState::Closed;
        }
    }

    fn close(&mut self, id: FileId) {
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
        if slot.inflight == 0 {
            slot.state = FileState::Closed;
        } else {
            slot.state = FileState::Closing;
        }
    }
}
