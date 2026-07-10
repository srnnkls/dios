//! Frame-pool contract shapes and the composed pool: the residency ADT (`Get`),
//! the readiness re-check ADT (`ReadyResult`), page identity, borrow guards, and
//! the `Pool` that composes a driver behind `&self` entry points.
//!
//! The pool owns the driver it composes ([`PoolBackend`]) and unifies the read
//! target with its own frames. The warm-hit `pin` probes the lock-free
//! packed-atomic [`PageTable`]; every mutation — miss admission, completion
//! routing, eviction, epoch advance/reclaim — runs under the AD-4 control-plane
//! [`Mutex`]. The miss singleflight and the backend seam live in [`miss`].

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::completion::CompletionBatch;
use crate::driver::{Driver, FileHandle, FileId, MAX_FILES, OpKind, OpToken, ReadFrameIdx};
use crate::error::{IoError, SubmitError};

mod clock;
mod epoch;
mod frames;
mod miss;
mod table;
pub(crate) mod write_arena;

use epoch::{EvictQueue, ReaderSlot};
use miss::{MissOutcome, MissTable};

pub use clock::Clock;
pub use epoch::{FrameGuard, ReaderCtx};
pub use frames::{FrameState, Frames};
pub use miss::PoolBackend;
pub use table::PageTable;
pub use write_arena::{WriteArena, WriteSlot};

/// Default frame granule, fixed per store at open and never below the `O_DIRECT`
/// sector floor. AD-6 bounds the encoded BLOCK size (keys plus headers), not raw
/// value size: the S003 value-size evidence (the gestalt store's 36,423 rows cap
/// at a 758 B value, 36,421 of them ≤ 16 B) together with sira's ~4 KiB writer
/// block-size target justify 4096 as the default. T011/T014 re-validate this
/// against real encoded segments, and a per-store override remains available.
pub const GRANULE_DEFAULT: u32 = 4096;

/// The `O_DIRECT` sector floor: every frame and staging slot is aligned to this,
/// and no granule may fall below it.
pub(crate) const SECTOR_BYTES: u32 = 4096;

/// A zero-progress short read (EOF mid-extent) surfaces to every waiter as an
/// operating failure, like an IO error (scope.md:570). No POSIX errno names EOF,
/// so the pool synthesizes `EIO`.
const SHORT_READ_EOF_ERRNO: i32 = 5;

/// Stable address of an aligned file extent: a generational file id and the
/// granule index within that file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageId {
    file: FileId,
    granule_idx: u32,
}

impl PageId {
    #[must_use]
    pub fn new(file: FileId, granule_idx: u32) -> Self {
        Self { file, granule_idx }
    }

    #[must_use]
    pub fn file(self) -> FileId {
        self.file
    }

    #[must_use]
    pub fn granule_idx(self) -> u32 {
        self.granule_idx
    }
}

/// Residency outcome of a `get`: a warm borrow, a submitted miss, or bounded
/// backpressure. `Busy` is retriable via `poll`, never a block.
#[derive(Debug)]
pub enum Get<'pool> {
    Hit(FrameGuard<'pool>),
    Pending(PendingToken),
    Busy,
}

/// Re-check outcome of a pending miss: `NotYet` hands the token back for a
/// non-consuming poll-again; `Err` frees the frame and surfaces the failure.
#[derive(Debug)]
pub enum ReadyResult<'pool> {
    Ready(FrameGuard<'pool>),
    NotYet(PendingToken),
    Err(IoError),
}

/// Opaque waiter handle for a submitted miss. Dropping it cancels waiter
/// interest only — the in-flight read still completes and the page becomes
/// resident. Minted only by the pool's miss path.
#[derive(Debug)]
pub struct PendingToken {
    page: PageId,
}

impl PendingToken {
    pub(crate) fn new(page: PageId) -> Self {
        Self { page }
    }

    #[must_use]
    pub fn page(&self) -> PageId {
        self.page
    }
}

impl Drop for PendingToken {
    fn drop(&mut self) {}
}

/// Why a pool configuration is rejected at build, before any frame is allocated
/// — an open-time typed error, never a runtime deadlock (INV-9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolConfigError {
    /// `frame_count` is below the deadlock-freedom watermark.
    BelowWatermark { frame_count: u32, watermark: u32 },
    /// `miss_headroom` is below `3 × max_inflight_reads` (one `InFlight` frame
    /// per concurrent miss plus two grace periods).
    MissHeadroomTooSmall { miss_headroom: u32, minimum: u32 },
    /// `granule` is not a power of two.
    GranuleNotPowerOfTwo { granule: u32 },
    /// `granule` is below the `sector` floor required by `O_DIRECT`.
    GranuleBelowSector { granule: u32, sector: u32 },
}

impl std::fmt::Display for PoolConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BelowWatermark {
                frame_count,
                watermark,
            } => write!(
                f,
                "frame count {frame_count} below the deadlock-freedom watermark {watermark}"
            ),
            Self::MissHeadroomTooSmall {
                miss_headroom,
                minimum,
            } => write!(
                f,
                "miss headroom {miss_headroom} below the minimum {minimum}"
            ),
            Self::GranuleNotPowerOfTwo { granule } => {
                write!(f, "granule {granule} is not a power of two")
            }
            Self::GranuleBelowSector { granule, sector } => {
                write!(f, "granule {granule} below the sector floor {sector}")
            }
        }
    }
}

impl std::error::Error for PoolConfigError {}

/// Why a reader registration is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterError {
    /// Every registration slot (`max_concurrent_readers`) is occupied.
    AtCapacity { max_concurrent_readers: u32 },
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AtCapacity {
                max_concurrent_readers,
            } => write!(
                f,
                "reader registration at capacity {max_concurrent_readers}"
            ),
        }
    }
}

impl std::error::Error for RegisterError {}

/// Per-reader hit and eviction tallies. Plain thread-local cells — no shared
/// RMW on the warm path; the poll-boundary aggregation is a later consumer.
#[derive(Debug, Default)]
pub struct ReaderCounters {
    hits: Cell<u32>,
    evictions: Cell<u32>,
}

impl ReaderCounters {
    #[must_use]
    pub fn new() -> Self {
        Self {
            hits: Cell::new(0),
            evictions: Cell::new(0),
        }
    }

    pub fn record_hit(&self) {
        self.hits.set(self.hits.get().saturating_add(1));
    }

    pub fn record_eviction(&self) {
        self.evictions.set(self.evictions.get().saturating_add(1));
    }

    #[must_use]
    pub fn hits(&self) -> u32 {
        self.hits.get()
    }

    #[must_use]
    pub fn evictions(&self) -> u32 {
        self.evictions.get()
    }
}

/// Fixes every pool capacity up front, then validates the granule and the
/// deadlock-freedom watermark (INV-9) at [`PoolBuilder::build`].
#[derive(Debug, Clone, Copy)]
pub struct PoolBuilder {
    frame_count: u32,
    granule: u32,
    max_concurrent_readers: u32,
    peak_guards_per_reader: u32,
    max_inflight_reads: u32,
    miss_headroom: u32,
}

impl Default for PoolBuilder {
    fn default() -> Self {
        Self {
            frame_count: 0,
            granule: GRANULE_DEFAULT,
            max_concurrent_readers: 0,
            peak_guards_per_reader: 0,
            max_inflight_reads: 0,
            miss_headroom: 0,
        }
    }
}

impl PoolBuilder {
    #[must_use]
    pub fn frame_count(mut self, frame_count: u32) -> Self {
        self.frame_count = frame_count;
        self
    }

    #[must_use]
    pub fn granule(mut self, granule: u32) -> Self {
        self.granule = granule;
        self
    }

    #[must_use]
    pub fn max_concurrent_readers(mut self, max_concurrent_readers: u32) -> Self {
        self.max_concurrent_readers = max_concurrent_readers;
        self
    }

    #[must_use]
    pub fn peak_guards_per_reader(mut self, peak_guards_per_reader: u32) -> Self {
        self.peak_guards_per_reader = peak_guards_per_reader;
        self
    }

    #[must_use]
    pub fn max_inflight_reads(mut self, max_inflight_reads: u32) -> Self {
        self.max_inflight_reads = max_inflight_reads;
        self
    }

    #[must_use]
    pub fn miss_headroom(mut self, miss_headroom: u32) -> Self {
        self.miss_headroom = miss_headroom;
        self
    }

    /// Validates the granule and the deadlock-freedom watermark (INV-9).
    fn validate(self) -> Result<(), PoolConfigError> {
        if !self.granule.is_power_of_two() {
            return Err(PoolConfigError::GranuleNotPowerOfTwo {
                granule: self.granule,
            });
        }
        if self.granule < SECTOR_BYTES {
            return Err(PoolConfigError::GranuleBelowSector {
                granule: self.granule,
                sector: SECTOR_BYTES,
            });
        }
        let Some(minimum) = self.max_inflight_reads.checked_mul(3) else {
            return Err(PoolConfigError::MissHeadroomTooSmall {
                miss_headroom: self.miss_headroom,
                minimum: u32::MAX,
            });
        };
        if self.miss_headroom < minimum {
            return Err(PoolConfigError::MissHeadroomTooSmall {
                miss_headroom: self.miss_headroom,
                minimum,
            });
        }
        let watermark = (u64::from(self.max_concurrent_readers)
            * u64::from(self.peak_guards_per_reader)
            + u64::from(self.miss_headroom))
        .max(1);
        if u64::from(self.frame_count) < watermark {
            return Err(PoolConfigError::BelowWatermark {
                frame_count: self.frame_count,
                watermark: u32::try_from(watermark).unwrap_or(u32::MAX),
            });
        }
        Ok(())
    }

    /// Validates the configuration and preallocates a pool over an internal,
    /// cfg-selected [`Driver`].
    ///
    /// # Errors
    ///
    /// [`PoolConfigError`]: a non-power-of-two or sub-sector granule, a
    /// `miss_headroom` below `3 × max_inflight_reads`, or a `frame_count` below
    /// the watermark `max_concurrent_readers × peak_guards_per_reader +
    /// miss_headroom` (INV-9).
    pub fn build(self) -> Result<Pool<Driver>, PoolConfigError> {
        self.validate()?;
        let driver = Driver::builder()
            .frames(self.frame_count)
            .frame_bytes(self.granule)
            .queue_capacity(self.max_inflight_reads.max(1))
            .build();
        Ok(Pool::preallocated(self, driver))
    }

    /// Preallocates a pool composed over the supplied `driver`, unifying its read
    /// target with the pool's frames.
    ///
    /// # Errors
    ///
    /// [`PoolConfigError`] on the same open-time checks as [`PoolBuilder::build`].
    #[doc(hidden)]
    pub fn build_on<D: PoolBackend>(self, driver: D) -> Result<Pool<D>, PoolConfigError> {
        self.validate()?;
        Ok(Pool::preallocated(self, driver))
    }
}

/// Control-plane state guarded by the AD-4 pool mutex: the CLOCK sweep (its
/// reference bits live lock-free on the pool for the warm-hit path), the
/// epoch-tagged eviction ring, the per-`PageId` singleflight table, the frame →
/// page reverse map, the file registry, and the reused completion batch.
#[derive(Debug)]
struct Control {
    evict_queue: EvictQueue,
    miss: MissTable,
    frame_pages: Box<[Option<PageId>]>,
    files: Box<[Option<FileHandle>]>,
    batch: CompletionBatch,
}

/// The userspace frame pool: preallocated frames shared with the composed driver,
/// the lock-free packed page table, CLOCK eviction, epoch reclamation, and
/// per-`PageId` singleflight, with reader registration capped at
/// `max_concurrent_readers`. The CLOCK reference bits sit outside the control
/// mutex so a warm hit sets them lock-free; the sweep hand advances only under
/// the mutex.
#[derive(Debug)]
pub struct Pool<D = Driver> {
    frames: Arc<Frames>,
    table: PageTable,
    clock: Clock,
    global_epoch: AtomicU64,
    slots: Box<[ReaderSlot]>,
    control: Mutex<Control>,
    driver: D,
    granule: u32,
    frame_count: u32,
    max_concurrent_readers: u32,
}

impl Pool<Driver> {
    #[must_use]
    pub fn builder() -> PoolBuilder {
        PoolBuilder::default()
    }
}

impl<D: PoolBackend> Pool<D> {
    fn preallocated(config: PoolBuilder, driver: D) -> Self {
        let frames = Arc::new(Frames::preallocated(config.frame_count, config.granule));
        driver.share_frames(Arc::clone(&frames));
        let slots = (0..config.max_concurrent_readers)
            .map(|_| ReaderSlot::vacant())
            .collect();
        let control = Control {
            evict_queue: EvictQueue::with_capacity(config.frame_count),
            miss: MissTable::with_capacity(config.frame_count),
            frame_pages: (0..config.frame_count).map(|_| None).collect(),
            files: (0..MAX_FILES).map(|_| None).collect(),
            batch: CompletionBatch::with_capacity(config.frame_count as usize),
        };
        Self {
            frames,
            table: PageTable::with_frame_count(config.frame_count),
            clock: Clock::with_frame_count(config.frame_count),
            global_epoch: AtomicU64::new(0),
            slots,
            control: Mutex::new(control),
            driver,
            granule: config.granule,
            frame_count: config.frame_count,
            max_concurrent_readers: config.max_concurrent_readers,
        }
    }

    /// Borrows the composed driver — a test/observation seam.
    #[doc(hidden)]
    #[must_use]
    pub fn driver(&self) -> &D {
        &self.driver
    }

    fn control(&self) -> MutexGuard<'_, Control> {
        self.control.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Claims a reader registration slot.
    ///
    /// # Errors
    ///
    /// [`RegisterError::AtCapacity`] once `max_concurrent_readers` slots are
    /// held — registration beyond capacity fails rather than deadlocking.
    pub fn register_reader(&self) -> Result<ReaderCtx<'_>, RegisterError> {
        for slot in &self.slots {
            if slot.try_occupy() {
                return Ok(ReaderCtx::new(slot));
            }
        }
        Err(RegisterError::AtCapacity {
            max_concurrent_readers: self.max_concurrent_readers,
        })
    }

    /// Routes every `PageId` naming `fd`'s file to this handle. Reads for such a
    /// page issue against `fd` at `granule_idx × granule`.
    pub fn register_file(&self, fd: FileHandle) {
        let slot = fd.file_id().slot() as usize;
        self.control().files[slot] = Some(fd);
    }

    /// Residency lookup: a warm hit borrows the frame; a miss submits a singleflight
    /// read (or joins one in flight); no evictable frame after one bounded reclaim
    /// attempt is `Busy`.
    pub fn get<'pool>(&'pool self, reader: &'pool ReaderCtx<'pool>, page: PageId) -> Get<'pool> {
        if let Some(guard) = self.pin(reader, page) {
            return Get::Hit(guard);
        }
        let mut control = self.control();
        if self
            .table
            .lookup(page)
            .is_some_and(|frame| self.frames.state(frame) == FrameState::Resident)
        {
            drop(control);
            return match self.pin(reader, page) {
                Some(guard) => Get::Hit(guard),
                None => Get::Busy,
            };
        }
        if let Some(index) = control.miss.find(page) {
            match control.miss.entry(index).outcome() {
                MissOutcome::Pending => return Get::Pending(PendingToken::new(page)),
                MissOutcome::Failed(_) => control.miss.resolve(index),
            }
        }
        let Some(frame) = self.claim_frame(&mut control) else {
            return Get::Busy;
        };
        self.frames.advance(frame, FrameState::InFlight);
        let Ok(token) = self.submit_page_read(&control, page, frame, 0) else {
            self.frames.abort_inflight(frame);
            return Get::Busy;
        };
        let admitted = control.miss.admit(page, frame, token);
        debug_assert!(
            admitted,
            "the miss table admits within the frame-count bound"
        );
        Get::Pending(PendingToken::new(page))
    }

    /// Re-checks a pending miss: `Ready` once its page is resident, `Err` on a
    /// faulted or EOF-terminated read (frame already freed), else `NotYet` handing
    /// the token back.
    pub fn ready<'pool>(
        &'pool self,
        reader: &'pool ReaderCtx<'pool>,
        token: PendingToken,
    ) -> ReadyResult<'pool> {
        let page = token.page();
        if let Some(guard) = self.pin(reader, page) {
            return ReadyResult::Ready(guard);
        }
        let control = self.control();
        let outcome = control
            .miss
            .find(page)
            .map(|i| control.miss.entry(i).outcome());
        match outcome {
            Some(MissOutcome::Failed(errno)) => {
                debug_assert!(errno != 0, "a fanned-out miss failure carries a real errno");
                ReadyResult::Err(IoError::from_raw(errno))
            }
            Some(MissOutcome::Pending) | None => ReadyResult::NotYet(token),
        }
    }

    /// The poll-boundary pass: drain the driver's completions (routing each into
    /// its frame, reslicing a short read, or failing the miss), then advance the
    /// global epoch and reclaim matured `Evicting` frames. Returns the number
    /// reclaimed.
    pub fn poll(&self) -> usize {
        let mut control = self.control();
        self.drain_completions(&mut control);
        let reclaimed = self.advance_and_reclaim(&mut control);
        debug_assert!(
            reclaimed <= self.frame_count as usize,
            "a poll reclaims at most every frame"
        );
        reclaimed
    }

    /// The residency state of `frame` — an observation seam for the epoch tests.
    #[doc(hidden)]
    #[must_use]
    pub fn frame_state(&self, frame: ReadFrameIdx) -> FrameState {
        self.frames.state(frame)
    }

    /// Mints an epoch-pinned guard over `page` for `reader`: publishes the
    /// reader's epoch BEFORE validating the frame is still Resident and mapped, so
    /// an eviction that removed the mapping is observed as a miss (`None`) rather
    /// than handing back reclaimable bytes.
    #[doc(hidden)]
    pub fn pin<'ctx>(
        &'ctx self,
        reader: &'ctx ReaderCtx<'_>,
        page: PageId,
    ) -> Option<FrameGuard<'ctx>> {
        let slot = reader.slot();
        let first_guard = slot.begin_pin(self.global_epoch.load(Ordering::Acquire));
        let resident = self
            .table
            .lookup(page)
            .filter(|&frame| self.frames.state(frame) == FrameState::Resident);
        let Some(frame) = resident else {
            if first_guard {
                slot.abort_pin();
            }
            return None;
        };
        let _ = self.clock.reference(frame);
        slot.commit_pin();
        Some(FrameGuard::new(self.frames.frame_bytes(frame), slot))
    }

    /// Makes `page` resident in a freshly claimed frame filled with `fill`,
    /// standing in for the miss-completion path so a hit can be set up in
    /// isolation.
    ///
    /// # Panics
    ///
    /// If no frame is `Free` — the watermark bounds a well-behaved caller below
    /// this.
    #[doc(hidden)]
    pub fn insert_resident_frame(&self, page: PageId, fill: u8) -> ReadFrameIdx {
        let mut control = self.control();
        let frame = self
            .first_free_frame()
            .expect("frame pool exhausted: no Free frame to make resident");
        self.frames.advance(frame, FrameState::InFlight);
        self.frames.fill_inflight(frame, fill);
        self.frames.advance(frame, FrameState::Resident);
        self.table.insert_shared(page, frame);
        control.frame_pages[frame.get() as usize] = Some(page);
        let _ = self.clock.reference(frame);
        frame
    }

    /// Evicts `page`: removes its table mapping (so no new guard can pin the old
    /// contents), moves the frame Resident -> Evicting, and enqueues it tagged
    /// with the current global epoch. Isolated from CLOCK victim selection.
    ///
    /// # Panics
    ///
    /// If `page` has no mapping — a caller only evicts a resident page.
    #[doc(hidden)]
    pub fn evict_frame(&self, page: PageId) -> ReadFrameIdx {
        let mut control = self.control();
        let frame = self
            .table
            .remove_shared(page)
            .expect("evict_frame targets a mapped page");
        self.frames.advance(frame, FrameState::Evicting);
        control.frame_pages[frame.get() as usize] = None;
        control
            .evict_queue
            .push(frame, self.global_epoch.load(Ordering::Acquire));
        frame
    }

    fn first_free_frame(&self) -> Option<ReadFrameIdx> {
        (0..self.frame_count)
            .map(ReadFrameIdx::new)
            .find(|&frame| self.frames.state(frame) == FrameState::Free)
    }

    fn submit_page_read(
        &self,
        control: &Control,
        page: PageId,
        frame: ReadFrameIdx,
        filled: u32,
    ) -> Result<OpToken, SubmitError> {
        let (offset, len) = read_span(page, self.granule, filled);
        let fd = control.files[page.file().slot() as usize]
            .as_ref()
            .expect("a registered file backs every requested page");
        self.driver.submit_read(fd, frame, offset, len)
    }

    /// Finds a `Free` frame, or runs one bounded reclaim attempt (drain, advance,
    /// reclaim, one CLOCK eviction) before conceding `Busy` (design.md Busy path).
    fn claim_frame(&self, control: &mut Control) -> Option<ReadFrameIdx> {
        let claimed = self.claim_frame_bounded(control);
        debug_assert!(
            claimed.is_none_or(|frame| self.frames.state(frame) == FrameState::Free),
            "a claimed frame is Free before the caller advances it InFlight"
        );
        claimed
    }

    fn claim_frame_bounded(&self, control: &mut Control) -> Option<ReadFrameIdx> {
        if let Some(frame) = self.first_free_frame() {
            return Some(frame);
        }
        self.drain_completions(control);
        self.advance_and_reclaim(control);
        if let Some(frame) = self.first_free_frame() {
            return Some(frame);
        }
        self.evict_one_victim(control);
        self.advance_and_reclaim(control);
        self.first_free_frame()
    }

    fn evict_one_victim(&self, control: &mut Control) {
        let epoch = self.global_epoch.load(Ordering::Acquire);
        for _ in 0..=self.frame_count.saturating_mul(2) {
            let victim = self.clock.evict_victim_shared();
            if self.frames.state(victim) != FrameState::Resident {
                continue;
            }
            if let Some(page) = control.frame_pages[victim.get() as usize].take() {
                self.table.remove_shared(page);
            }
            self.frames.advance(victim, FrameState::Evicting);
            control.evict_queue.push(victim, epoch);
            return;
        }
    }

    fn drain_completions(&self, control: &mut Control) {
        self.driver.poll(&mut control.batch);
        let Control {
            batch,
            miss,
            frame_pages,
            files,
            ..
        } = control;
        for completion in batch.iter() {
            if completion.kind() != OpKind::Read {
                continue;
            }
            let Some(index) = miss.find_by_token(completion.token()) else {
                continue;
            };
            let entry = miss.entry(index);
            match completion.result() {
                Ok(0) => {
                    self.frames.abort_inflight(entry.frame());
                    miss.fail(index, SHORT_READ_EOF_ERRNO);
                }
                Ok(bytes) => {
                    let filled = entry.filled() + bytes;
                    if filled >= self.granule {
                        self.frames.advance(entry.frame(), FrameState::Resident);
                        self.table.insert_shared(entry.page(), entry.frame());
                        frame_pages[entry.frame().get() as usize] = Some(entry.page());
                        let _ = self.clock.reference(entry.frame());
                        miss.resolve(index);
                    } else {
                        let (offset, len) = read_span(entry.page(), self.granule, filled);
                        let fd = files[entry.page().file().slot() as usize]
                            .as_ref()
                            .expect("a registered file backs every in-flight miss");
                        if let Ok(token) = self.driver.submit_read(fd, entry.frame(), offset, len) {
                            miss.advance_remainder(index, filled, token);
                        } else {
                            self.frames.abort_inflight(entry.frame());
                            miss.fail(index, SHORT_READ_EOF_ERRNO);
                        }
                    }
                }
                Err(err) => {
                    let errno = err.raw_os_error().unwrap_or(SHORT_READ_EOF_ERRNO);
                    self.frames.abort_inflight(entry.frame());
                    miss.fail(index, errno);
                }
            }
        }
    }

    fn advance_and_reclaim(&self, control: &mut Control) -> usize {
        let epoch = self.global_epoch.load(Ordering::Acquire);
        let permitted = self.slots.iter().all(|slot| slot.permits_advance(epoch));
        if permitted {
            self.global_epoch.store(epoch + 1, Ordering::Release);
        }
        let global_epoch = self.global_epoch.load(Ordering::Acquire);
        let frames = &self.frames;
        control.evict_queue.drain_matured(global_epoch, |frame| {
            frames.advance(frame, FrameState::Free);
        })
    }
}

/// The file offset and length of the read that fills a page's granule from
/// `filled` bytes on: the whole granule at `filled == 0`, the reslice remainder
/// tail thereafter (scope.md:601).
fn read_span(page: PageId, granule: u32, filled: u32) -> (u64, u32) {
    let base = u64::from(page.granule_idx()) * u64::from(granule);
    (base + u64::from(filled), granule - filled)
}

impl PoolBackend for Driver {
    fn submit_read(
        &self,
        fd: &FileHandle,
        frame: ReadFrameIdx,
        file_offset: u64,
        _len: u32,
    ) -> Result<OpToken, SubmitError> {
        // The production driver reads the whole registered granule; the reslice
        // `len` binds only once the ring reads into the shared arena (T014).
        Driver::submit_read(self, fd, frame, file_offset)
    }

    fn poll(&self, out: &mut CompletionBatch) -> usize {
        Driver::poll(self, out)
    }
}
