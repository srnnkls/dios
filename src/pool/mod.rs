//! Frame-pool contract shapes: the residency ADT (`Get`), the readiness
//! re-check ADT (`ReadyResult`), page identity, and the borrow guards.
//!
//! These are the SCOPE-CONTRACT names T006/T007/T008 fill in behind — the real
//! frames, page table, CLOCK, epoch guards, and singleflight land there. The
//! API-fit spike (T016) pins this call surface through an in-example `StubPool`.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::driver::{FileId, ReadFrameIdx};
use crate::error::IoError;

mod clock;
mod epoch;
mod frames;
mod table;
pub(crate) mod write_arena;

use epoch::{EvictQueue, ReaderSlot};

pub use clock::Clock;
pub use epoch::{FrameGuard, ReaderCtx};
pub use frames::{FrameState, Frames};
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
/// resident. The real waiter/epoch bookkeeping arrives with the pool (T006).
#[derive(Debug)]
pub struct PendingToken {
    page: PageId,
}

impl PendingToken {
    /// Provisional minting shim for the T016 spike, sealed at T007 (pending tokens
    /// are issued only by the pool's miss path).
    #[doc(hidden)]
    #[must_use]
    pub fn new(page: PageId) -> Self {
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

    /// Validates the configuration and preallocates the pool.
    ///
    /// # Errors
    ///
    /// [`PoolConfigError`]: a non-power-of-two or sub-sector granule, a
    /// `miss_headroom` below `3 × max_inflight_reads`, or a `frame_count` below
    /// the watermark `max_concurrent_readers × peak_guards_per_reader +
    /// miss_headroom` (INV-9).
    pub fn build(self) -> Result<Pool, PoolConfigError> {
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
        Ok(Pool::preallocated(self))
    }
}

/// The userspace frame pool: preallocated frames, the page table, CLOCK
/// eviction, and epoch reclamation, with reader registration capped at
/// `max_concurrent_readers`. The `Mutex<PageTable>` is the T007 substrate; the
/// lock-free packed probe that supersedes it on the warm-hit path is owned by the
/// miss path (T008).
#[derive(Debug)]
pub struct Pool {
    frames: Frames,
    table: Mutex<PageTable>,
    clock: Clock,
    global_epoch: AtomicU64,
    slots: Box<[ReaderSlot]>,
    evict_queue: Mutex<EvictQueue>,
    max_concurrent_readers: u32,
}

impl Pool {
    #[must_use]
    pub fn builder() -> PoolBuilder {
        PoolBuilder::default()
    }

    fn preallocated(config: PoolBuilder) -> Self {
        let slots = (0..config.max_concurrent_readers)
            .map(|_| ReaderSlot::vacant())
            .collect();
        Self {
            frames: Frames::preallocated(config.frame_count, config.granule),
            table: Mutex::new(PageTable::with_frame_count(config.frame_count)),
            clock: Clock::with_frame_count(config.frame_count),
            global_epoch: AtomicU64::new(0),
            slots,
            evict_queue: Mutex::new(EvictQueue::with_capacity(config.frame_count)),
            max_concurrent_readers: config.max_concurrent_readers,
        }
    }

    /// Provisional internal accessor for the T006 tests; the composed pool entry
    /// points (T008) subsume it and it leaves the documented surface.
    #[doc(hidden)]
    #[must_use]
    pub fn frames(&self) -> &Frames {
        &self.frames
    }

    /// Provisional internal accessor for the T006 tests; the composed pool entry
    /// points (T008) subsume it and it leaves the documented surface.
    #[doc(hidden)]
    #[must_use]
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    fn table(&self) -> MutexGuard<'_, PageTable> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn evict_queue(&self) -> MutexGuard<'_, EvictQueue> {
        self.evict_queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
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

    /// Makes `page` resident in a freshly claimed frame filled with `fill`,
    /// standing in for the miss-completion path (T008) so a hit can be set up in
    /// isolation.
    ///
    /// # Panics
    ///
    /// If no frame is `Free` — the watermark bounds a well-behaved caller below
    /// this.
    #[doc(hidden)]
    pub fn insert_resident_frame(&self, page: PageId, fill: u8) -> ReadFrameIdx {
        let frame = self.claim_free_frame();
        self.frames.advance(frame, FrameState::InFlight);
        // SAFETY: `frame` was just claimed Free and is not mapped in the page
        // table (inserted below, only after Resident), so no `pin` can hold a
        // guard over its bytes and this write aliases no live borrow of the
        // granule.
        unsafe { self.frames.fill(frame, fill) };
        self.frames.advance(frame, FrameState::Resident);
        self.table().insert(page, frame);
        frame
    }

    /// Mints an epoch-pinned guard over `page` for `reader`: publishes the
    /// reader's epoch BEFORE validating the frame is still Resident and mapped, so
    /// an eviction that removed the mapping is observed as a miss (`None`) rather
    /// than handing back reclaimable bytes. The guard borrows `reader`, so
    /// dropping a reader while one of its guards lives is a compile error — an
    /// epoch slot never vanishes under a live guard.
    #[doc(hidden)]
    pub fn pin<'ctx>(
        &'ctx self,
        reader: &'ctx ReaderCtx<'_>,
        page: PageId,
    ) -> Option<FrameGuard<'ctx>> {
        let slot = reader.slot();
        let first_guard = slot.begin_pin(self.global_epoch.load(Ordering::Acquire));
        let resident = self
            .table()
            .lookup(page)
            .filter(|&frame| self.frames.state(frame) == FrameState::Resident);
        let Some(frame) = resident else {
            if first_guard {
                slot.abort_pin();
            }
            return None;
        };
        slot.commit_pin();
        Some(FrameGuard::new(self.frames.frame_bytes(frame), slot))
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
        let frame = self
            .table()
            .remove(page)
            .expect("evict_frame targets a mapped page");
        self.frames.advance(frame, FrameState::Evicting);
        self.evict_queue()
            .push(frame, self.global_epoch.load(Ordering::Acquire));
        frame
    }

    /// The poll-boundary pass: advances the global epoch when every reader
    /// permits, then reclaims Evicting frames that have aged two epochs
    /// (Evicting -> Free). Returns the number reclaimed.
    #[doc(hidden)]
    pub fn poll(&self) -> usize {
        self.advance_epoch();
        let global_epoch = self.global_epoch.load(Ordering::Acquire);
        let frames = &self.frames;
        self.evict_queue().drain_matured(global_epoch, |frame| {
            frames.advance(frame, FrameState::Free);
        })
    }

    /// The residency state of `frame` — an observation seam for the epoch tests.
    #[doc(hidden)]
    #[must_use]
    pub fn frame_state(&self, frame: ReadFrameIdx) -> FrameState {
        self.frames.state(frame)
    }

    /// Race-free only under the AD-4 single-poll-caller discipline that
    /// serializes poll: the load-scan-store is not one atomic step. The
    /// CAS-or-lock enforcing it for concurrent pollers is owned by T008; T009
    /// loom models that lock.
    fn advance_epoch(&self) {
        let global_epoch = self.global_epoch.load(Ordering::Acquire);
        let permitted = self
            .slots
            .iter()
            .all(|slot| slot.permits_advance(global_epoch));
        if permitted {
            self.global_epoch.store(global_epoch + 1, Ordering::Release);
        }
    }

    fn claim_free_frame(&self) -> ReadFrameIdx {
        for index in 0..self.frames.count() {
            let frame = ReadFrameIdx::new(index);
            if self.frames.state(frame) == FrameState::Free {
                return frame;
            }
        }
        panic!("frame pool exhausted: no Free frame to make resident");
    }
}
