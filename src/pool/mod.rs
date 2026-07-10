//! Frame-pool contract shapes: the residency ADT (`Get`), the readiness
//! re-check ADT (`ReadyResult`), page identity, and the borrow guards.
//!
//! These are the SCOPE-CONTRACT names T006/T007/T008 fill in behind — the real
//! frames, page table, CLOCK, epoch guards, and singleflight land there. The
//! API-fit spike (T016) pins this call surface through an in-example `StubPool`.

use std::cell::{Cell, Ref};
use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::driver::FileId;
use crate::error::IoError;

mod clock;
mod frames;
mod table;
pub(crate) mod write_arena;

pub use clock::Clock;
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

/// Per-reader epoch slot: `!Send` + `!Sync` and lifetime-bound to the pool, so
/// the EBR restrictions live in the type rather than a usage rule. The epoch
/// ticket this pins arrives with the pool's reclamation (T007); this shell only
/// carries the pool lifetime and the thread-bound marker.
#[derive(Debug)]
pub struct ReaderCtx<'pool> {
    _pool: PhantomData<&'pool ()>,
    _thread_bound: PhantomData<*const ()>,
}

impl ReaderCtx<'_> {
    /// Provisional minting shim for the T016 spike, sealed at T007 (readers are
    /// minted only via the pool's `register_reader`).
    #[doc(hidden)]
    #[must_use]
    pub fn new() -> Self {
        Self {
            _pool: PhantomData,
            _thread_bound: PhantomData,
        }
    }
}

/// Epoch-pinned read access to a resident frame: `Deref<Target = [u8]>` over the
/// whole granule, `!Send` (the borrow is thread-bound). The epoch ticket backing
/// arrives with the pool's EBR reclamation (T007); the spike borrows the frame
/// bytes directly.
#[derive(Debug)]
pub struct FrameGuard<'pool> {
    bytes: Ref<'pool, [u8]>,
}

impl<'pool> FrameGuard<'pool> {
    /// Provisional minting shim for the T016 spike, sealed at T007 (guards are
    /// minted only through the pool's epoch-pinned pin path).
    #[doc(hidden)]
    #[must_use]
    pub fn new(bytes: Ref<'pool, [u8]>) -> Self {
        Self { bytes }
    }
}

impl Deref for FrameGuard<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes
    }
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

/// The userspace frame pool: preallocated frames, the page table, and CLOCK
/// eviction, with reader registration capped at `max_concurrent_readers`. The
/// epoch guards (T007) and miss path (T008) land behind this surface.
#[derive(Debug)]
pub struct Pool {
    frames: Frames,
    table: PageTable,
    clock: Clock,
    max_concurrent_readers: u32,
    registered_readers: AtomicU32,
}

impl Pool {
    #[must_use]
    pub fn builder() -> PoolBuilder {
        PoolBuilder::default()
    }

    fn preallocated(config: PoolBuilder) -> Self {
        Self {
            frames: Frames::preallocated(config.frame_count, config.granule),
            table: PageTable::with_frame_count(config.frame_count),
            clock: Clock::with_frame_count(config.frame_count),
            max_concurrent_readers: config.max_concurrent_readers,
            registered_readers: AtomicU32::new(0),
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
    pub fn page_table(&self) -> &PageTable {
        &self.table
    }

    /// Provisional internal accessor for the T006 tests; the composed pool entry
    /// points (T008) subsume it and it leaves the documented surface.
    #[doc(hidden)]
    #[must_use]
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// Claims a reader registration slot.
    ///
    /// # Errors
    ///
    /// [`RegisterError::AtCapacity`] once `max_concurrent_readers` slots are
    /// held — registration beyond capacity fails rather than deadlocking.
    pub fn register_reader(&self) -> Result<ReaderCtx<'_>, RegisterError> {
        let prior = self.registered_readers.fetch_add(1, Ordering::AcqRel);
        if prior >= self.max_concurrent_readers {
            self.registered_readers.fetch_sub(1, Ordering::AcqRel);
            return Err(RegisterError::AtCapacity {
                max_concurrent_readers: self.max_concurrent_readers,
            });
        }
        Ok(ReaderCtx::new())
    }
}
