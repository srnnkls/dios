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
use std::mem::size_of;
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::Arc;
#[cfg(feature = "mock")]
use std::sync::atomic::{AtomicU64 as ObservationAtomicU64, Ordering as ObservationOrdering}; // control_acquisitions
use std::time::Duration;

use crate::completion::CompletionBatch;
use crate::driver::{Driver, FileHandle, FileId, MAX_FILES, OpKind, OpToken};
use crate::error::{IoError, SubmitError};
use crate::open::DirectIo;
use crate::product::{
    LifecycleCounters, PollReport, PoolCompletion, PoolCompletionBatch, PoolSubmitError, PoolToken,
    PoolWakeHandle, PoolWriteArena, PoolWriteSlot, RetireStatus, WaitState,
};
use crate::sync::{AtomicU32, AtomicU64, Mutex, MutexGuard, Ordering};

#[cfg(test)]
mod alias_guard;
mod clock;
mod epoch;
mod frames;
#[cfg(loom)]
pub mod loom_model;
mod miss;
mod table;
pub(crate) mod write_arena;

use epoch::{EvictQueue, ReaderRegistry, advance_epoch};
use miss::{MissEntry, MissInterests, MissOutcome, MissSlot, MissTable};

pub use clock::Clock;
pub use epoch::{FrameGuard, ReaderCtx};
pub(crate) use frames::Frames;
pub use frames::{FrameState, ReadFrameIdx};
pub(crate) use miss::PoolBackend;
pub(crate) use miss::sealed::Sealed as PoolBackendSealed;
pub use table::PageTable;
#[cfg(feature = "bench")]
pub(crate) use table::page_hash;

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
    /// Creates an extent identity from a file and granule index.
    #[must_use]
    pub fn new(file: FileId, granule_idx: u32) -> Self {
        Self { file, granule_idx }
    }

    /// Returns the opened file identity.
    #[must_use]
    pub fn file(self) -> FileId {
        self.file
    }

    /// Returns the zero-based granule index within the file.
    #[must_use]
    pub fn granule_idx(self) -> u32 {
        self.granule_idx
    }
}

/// Residency outcome of a `get`: a warm borrow, a submitted miss, or bounded
/// backpressure. `Busy` is retriable via `poll`, never a block.
#[derive(Debug)]
pub enum Get<'pool> {
    /// The extent is resident and borrowed for the guard lifetime.
    Hit(FrameGuard<'pool>),
    /// One read is in flight; the token can be checked with [`Pool::ready`].
    Pending(PendingToken),
    /// No frame can be admitted within the bounded reclaim pass.
    Busy,
}

/// Expected refusal of a residency lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GetError {
    /// The file generation is retired or has been replaced.
    StaleFile { page: PageId },
}

impl std::fmt::Display for GetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleFile { page } => write!(f, "stale file for page {page:?}"),
        }
    }
}

impl std::error::Error for GetError {}

/// Owned file capability reserved for resident-page observations.
#[derive(Debug)]
pub struct ResidentFileLease {
    pool_identity: u64,
    file: FileId,
    state: Arc<ResidentLeaseState>,
}

impl Drop for ResidentFileLease {
    fn drop(&mut self) {
        assert_eq!(
            self.pool_identity, self.state.pool_identity,
            "a resident file lease retains its originating pool identity"
        );
        assert_eq!(
            self.file.driver(),
            self.pool_identity,
            "a resident file lease retains its exact file identity"
        );
        let previous = self.state.count.fetch_sub(1, Ordering::AcqRel);
        assert!(previous > 0, "a resident file lease was acquired once");
        if previous == 1 {
            self.state.wake.wake();
        }
    }
}

#[derive(Debug)]
pub(crate) struct ResidentLeaseState {
    pool_identity: u64,
    generation: AtomicU32,
    count: AtomicU32,
    wake: Arc<WaitState>,
}

impl ResidentLeaseState {
    pub(crate) fn preallocated(pool_identity: u64, wake: Arc<WaitState>) -> Self {
        Self {
            pool_identity,
            generation: AtomicU32::new(0),
            count: AtomicU32::new(0),
            wake,
        }
    }

    pub(crate) fn count(&self) -> u32 {
        self.count.load(Ordering::Acquire)
    }

    fn publish_generation(&self, file: FileId) {
        assert_eq!(
            file.driver(),
            self.pool_identity,
            "lease state belongs to the registering pool"
        );
        assert_eq!(
            self.count(),
            0,
            "a file slot cannot publish a new generation with live leases"
        );
        self.generation.store(file.generation(), Ordering::Release);
    }

    fn acquire(state: &Arc<Self>, file: FileId) -> Result<ResidentFileLease, ResidentLeaseError> {
        assert_eq!(
            file.driver(),
            state.pool_identity,
            "file identity used with a foreign lease state"
        );
        if state.generation.load(Ordering::Acquire) != file.generation() {
            return Err(ResidentLeaseError::StaleFile { file });
        }
        let count = state.count();
        if count == u32::MAX {
            return Err(ResidentLeaseError::Exhausted { file });
        }
        let previous = state.count.fetch_add(1, Ordering::AcqRel);
        assert!(
            previous <= count,
            "concurrent drops only reduce the lease count"
        );
        Ok(ResidentFileLease {
            pool_identity: state.pool_identity,
            file,
            state: Arc::clone(state),
        })
    }

    #[cfg(feature = "mock")]
    fn set_count(&self, count: u32) {
        self.count.store(count, Ordering::Release);
    }
}

/// Volatile observation of one resident frame generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResidentHint {
    granule: u32,
    frame: u32,
    stamp: NonZeroU64,
}

const _: [(); 16] = [(); size_of::<ResidentHint>()];
const _: [(); 16] = [(); size_of::<Option<ResidentHint>>()];

/// Expected refusal to acquire a resident-file capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidentLeaseError {
    /// The file generation is retired or has been replaced.
    StaleFile { file: FileId },
    /// The file slot cannot admit another bounded lease.
    Exhausted { file: FileId },
}

/// Re-check outcome of a pending miss: `NotYet` hands the token back for a
/// non-consuming poll-again; `Err` frees the frame and surfaces the failure.
#[derive(Debug)]
pub enum ReadyResult<'pool> {
    /// The read completed and the extent is now borrowed.
    Ready(FrameGuard<'pool>),
    /// The read remains in flight and returns its waiter token.
    NotYet(PendingToken),
    /// The read failed and its frame was returned to the pool.
    Err(IoError),
}

/// Opaque waiter handle for a submitted miss. Dropping it cancels waiter
/// interest only — the in-flight read still completes and the page becomes
/// resident. Minted only by the pool's miss path.
#[derive(Debug)]
pub struct PendingToken {
    page: PageId,
    slot: MissSlot,
    generation: NonZeroU64,
    interests: Arc<MissInterests>,
    lifecycle: Arc<LifecycleCounters>,
    active: bool,
}

impl PendingToken {
    fn new(
        page: PageId,
        slot: MissSlot,
        generation: NonZeroU64,
        interests: Arc<MissInterests>,
        lifecycle: Arc<LifecycleCounters>,
    ) -> Self {
        lifecycle.register_pending();
        Self {
            page,
            slot,
            generation,
            interests,
            lifecycle,
            active: true,
        }
    }

    #[must_use]
    /// Returns the page this waiter observes.
    pub fn page(&self) -> PageId {
        self.page
    }

    fn consume(mut self) -> u32 {
        let remaining = self.interests.release(self.slot, self.generation);
        self.active = false;
        self.lifecycle.release_pending();
        remaining
    }
}

impl Drop for PendingToken {
    fn drop(&mut self) {
        if self.active {
            self.interests.release(self.slot, self.generation);
            self.active = false;
            self.lifecycle.release_pending();
        }
    }
}

/// Why a pool configuration is rejected at build, before any frame is allocated
/// — an open-time typed error, never a runtime deadlock (INV-9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolConfigError {
    /// `frame_count` is below the deadlock-freedom watermark.
    BelowWatermark {
        /// Configured frame count.
        frame_count: u32,
        /// Minimum frame count required by the watermark.
        watermark: u32,
    },
    /// `miss_headroom` is below `3 × max_inflight_reads` (one `InFlight` frame
    /// per concurrent miss plus two grace periods).
    MissHeadroomTooSmall {
        /// Configured miss headroom.
        miss_headroom: u32,
        /// Minimum headroom required by the in-flight bound.
        minimum: u32,
    },
    /// `granule` is not a power of two.
    GranuleNotPowerOfTwo {
        /// Rejected granule size.
        granule: u32,
    },
    /// `granule` is below the `sector` floor required by `O_DIRECT`.
    GranuleBelowSector {
        /// Rejected granule size.
        granule: u32,
        /// Minimum sector size.
        sector: u32,
    },
    /// Read and product queue reservations overflowed their fixed u32 bound.
    QueueCapacityOverflow {
        max_inflight_reads: u32,
        max_inflight_product_ops: u32,
    },
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
            Self::QueueCapacityOverflow {
                max_inflight_reads,
                max_inflight_product_ops,
            } => write!(
                f,
                "read capacity {max_inflight_reads} plus product capacity {max_inflight_product_ops} overflows"
            ),
        }
    }
}

impl std::error::Error for PoolConfigError {}

/// Failure to construct the default pool.
#[derive(Debug)]
pub enum PoolBuildError {
    /// The fixed capacities violate a pool invariant.
    Configuration(PoolConfigError),
    /// The selected driver could not initialize its operating resources.
    Driver(IoError),
}

impl std::fmt::Display for PoolBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Configuration(error) => write!(f, "invalid pool configuration: {error}"),
            Self::Driver(error) => write!(f, "driver initialization failed: {error}"),
        }
    }
}

impl std::error::Error for PoolBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Configuration(error) => Some(error),
            Self::Driver(error) => Some(error),
        }
    }
}

/// Why a reader registration is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterError {
    /// Every registration slot (`max_concurrent_readers`) is occupied.
    AtCapacity {
        /// Configured registration capacity.
        max_concurrent_readers: u32,
    },
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
    write_slots: u32,
    max_inflight_product_ops: u32,
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
            write_slots: 0,
            max_inflight_product_ops: 0,
        }
    }
}

impl PoolBuilder {
    /// Sets the total number of resident frames.
    #[must_use]
    pub fn frame_count(mut self, frame_count: u32) -> Self {
        self.frame_count = frame_count;
        self
    }

    /// Sets the power-of-two bytes per file extent and frame.
    #[must_use]
    pub fn granule(mut self, granule: u32) -> Self {
        self.granule = granule;
        self
    }

    /// Sets the fixed number of reader registration slots.
    #[must_use]
    pub fn max_concurrent_readers(mut self, max_concurrent_readers: u32) -> Self {
        self.max_concurrent_readers = max_concurrent_readers;
        self
    }

    /// Sets the maximum simultaneous guards held by one reader.
    #[must_use]
    pub fn peak_guards_per_reader(mut self, peak_guards_per_reader: u32) -> Self {
        self.peak_guards_per_reader = peak_guards_per_reader;
        self
    }

    /// Sets the maximum reads admitted concurrently.
    #[must_use]
    pub fn max_inflight_reads(mut self, max_inflight_reads: u32) -> Self {
        self.max_inflight_reads = max_inflight_reads;
        self
    }

    /// Sets the frames reserved for misses and epoch reclamation.
    #[must_use]
    pub fn miss_headroom(mut self, miss_headroom: u32) -> Self {
        self.miss_headroom = miss_headroom;
        self
    }

    /// Sets the exact number of product write staging slots.
    #[must_use]
    pub fn write_slots(mut self, write_slots: u32) -> Self {
        self.write_slots = write_slots;
        self
    }

    /// Sets the total admitted plus retained product-operation bound.
    #[must_use]
    pub fn max_inflight_product_ops(mut self, operations: u32) -> Self {
        self.max_inflight_product_ops = operations;
        self
    }

    /// Validates the granule and the deadlock-freedom watermark (INV-9).
    fn validate(self) -> Result<(), PoolConfigError> {
        if self
            .max_inflight_reads
            .checked_add(self.max_inflight_product_ops)
            .is_none()
        {
            return Err(PoolConfigError::QueueCapacityOverflow {
                max_inflight_reads: self.max_inflight_reads,
                max_inflight_product_ops: self.max_inflight_product_ops,
            });
        }
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
    ///
    /// # Panics
    ///
    /// If internally validated capacities are changed before driver construction.
    pub fn build(self) -> Result<Pool<Driver>, PoolBuildError> {
        self.validate().map_err(PoolBuildError::Configuration)?;
        let frames = Arc::new(Frames::preallocated(self.frame_count, self.granule));
        let driver = Driver::builder()
            .frames(self.frame_count)
            .frame_bytes(self.granule)
            .queue_capacity(
                self.max_inflight_reads
                    .saturating_add(self.max_inflight_product_ops)
                    .max(1),
            )
            .write_slots(self.write_slots.max(1))
            .build_with_frames(Arc::clone(&frames))
            .map_err(PoolBuildError::Driver)?;
        Ok(Pool::preallocated(self, driver, frames))
    }

    /// Preallocates a pool composed over the supplied `driver`, unifying its read
    /// target with the pool's frames.
    ///
    /// # Errors
    ///
    /// [`PoolConfigError`] on the same open-time checks as [`PoolBuilder::build`].
    #[cfg(feature = "mock")]
    pub(crate) fn build_on_internal(
        self,
        driver: crate::mock::MockDriver,
    ) -> Result<Pool<crate::mock::MockDriver>, PoolConfigError> {
        self.validate()?;
        let frames = Arc::new(Frames::preallocated(self.frame_count, self.granule));
        driver.share_frames_for_pool(Arc::clone(&frames));
        Ok(Pool::preallocated(self, driver, frames))
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
    files: Box<[Option<PoolFile>]>,
    batch: CompletionBatch,
    product_ops: Box<[ProductOpSlot]>,
    product_sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PoolFileState {
    Live,
    Retiring,
    Retired,
}

#[derive(Debug)]
struct PoolFile {
    id: FileId,
    handle: Option<FileHandle>,
    state: PoolFileState,
}

#[derive(Debug)]
struct ProductOpSlot {
    generation: u32,
    operation: Option<ProductOp>,
}

#[derive(Debug)]
enum ProductOp {
    Write {
        token: PoolToken,
        driver_token: OpToken,
        file: FileId,
        order: u64,
    },
    FsyncHeld {
        token: PoolToken,
        file: FileId,
        order: u64,
    },
    Fsync {
        token: PoolToken,
        driver_token: OpToken,
        file: FileId,
    },
    Completed {
        file: FileId,
        completion: PoolCompletion,
    },
}

impl ProductOp {
    fn file(&self) -> FileId {
        match self {
            Self::Write { file, .. }
            | Self::FsyncHeld { file, .. }
            | Self::Fsync { file, .. }
            | Self::Completed { file, .. } => *file,
        }
    }
}

/// The userspace frame pool: preallocated frames shared with the composed driver,
/// the lock-free packed page table, CLOCK eviction, epoch reclamation, and
/// per-`PageId` singleflight, with reader registration capped at
/// `max_concurrent_readers`. The CLOCK reference bits sit outside the control
/// mutex so a warm hit sets them lock-free; the sweep hand advances only under
/// the mutex.
///
/// Control-plane construction and mutation seams are not part of the product
/// API:
///
/// ```compile_fail
/// use dios::Pool;
/// let pool = Pool::builder()
///     .frame_count(8).granule(4096)
///     .max_concurrent_readers(1).peak_guards_per_reader(1)
///     .max_inflight_reads(1).miss_headroom(3)
///     .build().unwrap();
/// let _driver = pool.driver();
/// ```
#[derive(Debug)]
pub struct Pool<D = Driver> {
    frames: Arc<Frames>,
    table: PageTable,
    clock: Clock,
    global_epoch: AtomicU64,
    readers: Arc<ReaderRegistry>,
    miss_interests: Arc<MissInterests>,
    lifecycle: Arc<LifecycleCounters>,
    wake: Arc<WaitState>,
    wait_batch: Mutex<CompletionBatch>,
    control: Mutex<Control>,
    file_live_generations: Box<[AtomicU64]>,
    resident_lease_states: Box<[Arc<ResidentLeaseState>]>,
    #[cfg(feature = "mock")]
    control_acquisitions: ObservationAtomicU64,
    driver: D,
    granule: u32,
    frame_count: u32,
    max_concurrent_readers: u32,
    write_slots: u32,
    identity: u64,
}

impl Pool<Driver> {
    /// Returns a builder for the default platform driver.
    #[must_use]
    pub fn builder() -> PoolBuilder {
        PoolBuilder::default()
    }
}

#[expect(
    private_bounds,
    reason = "the private sealed bound preserves static dispatch for the two crate-owned pool backends"
)]
impl<D: PoolBackend> Pool<D> {
    fn preallocated(config: PoolBuilder, driver: D, frames: Arc<Frames>) -> Self {
        assert_eq!(
            frames.count(),
            config.frame_count,
            "pool and arena counts match"
        );
        assert_eq!(
            frames.granule(),
            config.granule,
            "pool and arena sizes match"
        );
        let lifecycle = Arc::new(LifecycleCounters::default());
        let readers = Arc::new(ReaderRegistry::with_capacity(
            config.max_concurrent_readers,
            Arc::clone(&lifecycle),
        ));
        let wake = Arc::new(WaitState::default());
        // Finish the platform's lazy wake initialization before hot-path lease drops.
        wake.wake();
        wake.consume_current();
        let pool_identity = driver.identity();
        driver.attach_pool_state(Arc::clone(&lifecycle), Arc::clone(&wake));
        let backend_capacity = config
            .max_inflight_reads
            .checked_add(config.max_inflight_product_ops)
            .expect("validated queue capacity")
            .max(1) as usize;
        let control = Control {
            evict_queue: EvictQueue::with_capacity(config.frame_count),
            miss: MissTable::with_capacity(config.frame_count),
            frame_pages: (0..config.frame_count).map(|_| None).collect(),
            files: (0..MAX_FILES).map(|_| None).collect(),
            batch: CompletionBatch::with_capacity(backend_capacity),
            product_ops: (0..config.max_inflight_product_ops)
                .map(|_| ProductOpSlot {
                    generation: 0,
                    operation: None,
                })
                .collect(),
            product_sequence: 0,
        };
        Self {
            frames,
            table: PageTable::with_frame_count(config.frame_count),
            clock: Clock::with_frame_count(config.frame_count),
            global_epoch: AtomicU64::new(0),
            readers,
            miss_interests: Arc::new(MissInterests::with_capacity(config.frame_count)),
            lifecycle,
            wake: Arc::clone(&wake),
            wait_batch: Mutex::new(CompletionBatch::with_capacity(backend_capacity)),
            control: Mutex::new(control),
            file_live_generations: (0..MAX_FILES).map(|_| AtomicU64::new(0)).collect(),
            resident_lease_states: (0..MAX_FILES)
                .map(|_| {
                    Arc::new(ResidentLeaseState::preallocated(
                        pool_identity,
                        Arc::clone(&wake),
                    ))
                })
                .collect(),
            #[cfg(feature = "mock")]
            control_acquisitions: ObservationAtomicU64::new(0),
            driver,
            granule: config.granule,
            frame_count: config.frame_count,
            max_concurrent_readers: config.max_concurrent_readers,
            write_slots: config.write_slots,
            identity: pool_identity,
        }
    }

    /// Borrows the composed driver — a test/observation seam.
    #[must_use]
    pub(crate) fn driver_internal(&self) -> &D {
        &self.driver
    }

    #[cfg(feature = "mock")]
    pub(crate) fn lifecycle_internal(&self) -> Arc<LifecycleCounters> {
        Arc::clone(&self.lifecycle)
    }

    #[cfg(feature = "mock")]
    pub(crate) fn wait_internal(&self) -> Arc<WaitState> {
        Arc::clone(&self.wake)
    }

    #[cfg(feature = "mock")]
    pub(crate) fn set_resident_lease_count_internal(&self, file: FileId, count: u32) {
        assert_eq!(
            file.driver(),
            self.identity,
            "file identity used with a foreign pool"
        );
        let index = file.slot() as usize;
        assert!(
            index < self.resident_lease_states.len(),
            "file slot is within the fixed lease-state table"
        );
        self.resident_lease_states[index].set_count(count);
    }

    #[cfg(feature = "mock")]
    pub(crate) fn resident_lease_count_internal(&self, file: FileId) -> u32 {
        assert_eq!(
            file.driver(),
            self.identity,
            "file identity used with a foreign pool"
        );
        let index = file.slot() as usize;
        assert!(
            index < self.resident_lease_states.len(),
            "file slot is within the fixed lease-state table"
        );
        self.resident_lease_states[index].count()
    }

    #[cfg(feature = "mock")]
    pub(crate) fn observe_resident_lease_count_internal(
        &self,
        file: FileId,
    ) -> Arc<ResidentLeaseState> {
        assert_eq!(
            file.driver(),
            self.identity,
            "file identity used with a foreign pool"
        );
        let index = file.slot() as usize;
        assert!(
            index < self.resident_lease_states.len(),
            "file slot is within the fixed lease-state table"
        );
        Arc::clone(&self.resident_lease_states[index])
    }

    #[cfg(not(loom))]
    fn control(&self) -> MutexGuard<'_, Control> {
        let control = self
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        #[cfg(feature = "mock")]
        self.control_acquisitions
            .fetch_add(1, ObservationOrdering::Relaxed);
        control
    }

    #[cfg(loom)]
    fn control(&self) -> MutexGuard<'_, Control> {
        let control = self.control.lock().expect("loom mutex is never poisoned");
        #[cfg(feature = "mock")]
        self.control_acquisitions
            .fetch_add(1, ObservationOrdering::Relaxed);
        control
    }

    #[cfg(not(loom))]
    fn wait_batch(&self) -> MutexGuard<'_, CompletionBatch> {
        self.wait_batch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(loom)]
    fn wait_batch(&self) -> MutexGuard<'_, CompletionBatch> {
        self.wait_batch
            .lock()
            .expect("loom mutex is never poisoned")
    }

    /// Claims a reader registration slot.
    ///
    /// # Errors
    ///
    /// [`RegisterError::AtCapacity`] once `max_concurrent_readers` slots are
    /// held — registration beyond capacity fails rather than deadlocking.
    pub fn register_reader(&self) -> Result<ReaderCtx, RegisterError> {
        self.readers.register().ok_or(RegisterError::AtCapacity {
            max_concurrent_readers: self.max_concurrent_readers,
        })
    }

    /// Routes every `PageId` naming `fd`'s file to this handle. Reads for such a
    /// page issue against `fd` at `granule_idx × granule`.
    pub(crate) fn register_file_internal(&self, fd: FileHandle) {
        let slot = fd.file_id().slot() as usize;
        let id = fd.file_id();
        let mut control = self.control();
        publish_live_file(
            &mut control.files,
            &self.file_live_generations[slot],
            &self.resident_lease_states[slot],
            id,
            Some(fd),
        );
    }

    /// Opens a data file and registers its owned handle with this pool.
    ///
    /// # Errors
    ///
    /// Returns the open, direct-I/O policy, or fixed-file registration failure.
    pub fn open(&self, path: &Path, direct_io: DirectIo) -> Result<FileId, IoError> {
        let handle = self.driver.open(path, direct_io)?;
        let file = handle.file_id();
        self.register_file_internal(handle);
        Ok(file)
    }

    /// Residency lookup: a warm hit borrows the frame; a miss submits a singleflight
    /// read (or joins one in flight); no evictable frame after one bounded reclaim
    /// attempt is `Busy`.
    ///
    /// # Errors
    ///
    /// Returns [`GetError::StaleFile`] for a retired or reused file generation.
    ///
    /// # Panics
    ///
    /// If `reader` or the page's file identity belongs to another pool.
    pub fn get<'pool>(
        &'pool self,
        reader: &'pool ReaderCtx,
        page: PageId,
    ) -> Result<Get<'pool>, GetError> {
        self.assert_reader_owner(reader);
        assert_eq!(
            page.file().driver(),
            self.identity,
            "file identity used with a foreign pool"
        );
        if !file_generation_is_live(
            &self.file_live_generations[page.file().slot() as usize],
            page.file(),
        ) {
            return Err(GetError::StaleFile { page });
        }
        if let Some(guard) = self.pin_internal(reader, page) {
            return Ok(Get::Hit(guard));
        }
        let mut control = self.control();
        if !file_is_live(&control.files, page.file(), self.identity) {
            return Err(GetError::StaleFile { page });
        }
        if self
            .table
            .lookup(page)
            .is_some_and(|frame| self.frames.state(frame) == FrameState::Resident)
        {
            drop(control);
            return Ok(match self.pin_internal(reader, page) {
                Some(guard) => Get::Hit(guard),
                None => Get::Busy,
            });
        }
        if let Some(index) = control.miss.find_pending(page) {
            let (slot, generation) = control.miss.join(index, &self.miss_interests);
            return Ok(Get::Pending(PendingToken::new(
                page,
                slot,
                generation,
                Arc::clone(&self.miss_interests),
                Arc::clone(&self.lifecycle),
            )));
        }
        let Some(miss_slot) = control.miss.admission_slot(&self.miss_interests) else {
            return Ok(Get::Busy);
        };
        let Some(frame) = self.claim_frame(&mut control) else {
            return Ok(Get::Busy);
        };
        self.frames.advance(frame, FrameState::InFlight);
        let Ok(token) = self.submit_page_read(&control, page, frame, 0) else {
            self.frames.abort_inflight(frame);
            return Ok(Get::Busy);
        };
        let generation = control
            .miss
            .admit(miss_slot, page, frame, token, &self.miss_interests);
        self.wake.wake();
        Ok(Get::Pending(PendingToken::new(
            page,
            miss_slot,
            generation,
            Arc::clone(&self.miss_interests),
            Arc::clone(&self.lifecycle),
        )))
    }

    /// Acquires one owned capability for an exact live file generation.
    ///
    /// # Errors
    ///
    /// Returns [`ResidentLeaseError::StaleFile`] when the exact generation is
    /// unavailable and [`ResidentLeaseError::Exhausted`] at the fixed count
    /// ceiling.
    ///
    /// # Panics
    ///
    /// If `file` belongs to another pool.
    pub fn lease_file(&self, file: FileId) -> Result<ResidentFileLease, ResidentLeaseError> {
        assert_eq!(
            file.driver(),
            self.identity,
            "file identity used with a foreign pool"
        );
        let index = file.slot() as usize;
        let control = self.control();
        if !file_is_live(&control.files, file, self.identity) {
            return Err(ResidentLeaseError::StaleFile { file });
        }
        acquire_resident_file_lease(&self.resident_lease_states[index], file)
    }

    /// Mints a lock-free advisory observation for one exact resident page.
    ///
    /// # Panics
    ///
    /// If `lease` does not belong to this pool or name `page`'s exact file.
    #[must_use]
    pub fn resident_hint(&self, lease: &ResidentFileLease, page: PageId) -> Option<ResidentHint> {
        self.assert_lease_owner(lease, page);
        let frame = self.table.lookup(page)?;
        let stamp = self.frames.state_word(frame);
        if !Frames::word_is_resident(stamp) {
            return None;
        }
        Some(ResidentHint {
            granule: page.granule_idx(),
            frame: frame.get(),
            stamp: NonZeroU64::new(stamp).expect("a Resident packed state word is nonzero"),
        })
    }

    /// Attempts one exact hinted residency lookup before falling back to
    /// [`Pool::get`] for a missing, mismatched, or stale observation.
    ///
    /// # Errors
    ///
    /// Returns the same error as [`Pool::get`].
    ///
    /// # Panics
    ///
    /// If `reader` or `lease` belongs to another pool, or `lease` does not name
    /// `page`'s exact file.
    pub fn get_with_hint<'pool>(
        &'pool self,
        reader: &'pool ReaderCtx,
        lease: &ResidentFileLease,
        page: PageId,
        hint: Option<ResidentHint>,
    ) -> Result<Get<'pool>, GetError> {
        self.assert_reader_owner(reader);
        self.assert_lease_owner(lease, page);
        if !file_generation_is_live(
            &self.file_live_generations[page.file().slot() as usize],
            page.file(),
        ) {
            return Err(GetError::StaleFile { page });
        }
        let Some(hint) = hint else {
            return self.get(reader, page);
        };
        let Some(frame) = pin_with_resident_hint(
            &self.frames,
            &self.clock,
            &self.global_epoch,
            reader.slot(),
            page,
            hint,
        ) else {
            return self.get(reader, page);
        };
        Ok(Get::Hit(FrameGuard::new(
            self.frames.frame_bytes(frame),
            reader.slot(),
        )))
    }

    /// Re-checks a pending miss: `Ready` once its page is resident, `Err` on a
    /// faulted or EOF-terminated read (frame already freed), else `NotYet` handing
    /// the token back.
    ///
    /// # Panics
    ///
    /// If `token` was minted by a different pool or no longer names its exact
    /// live miss generation.
    pub fn ready<'pool>(
        &'pool self,
        reader: &'pool ReaderCtx,
        token: PendingToken,
    ) -> ReadyResult<'pool> {
        self.assert_reader_owner(reader);
        assert!(
            Arc::ptr_eq(&token.interests, &self.miss_interests),
            "a pending capability cannot cross pool identity"
        );
        let mut control = self.control();
        let entry = control
            .miss
            .validate(token.slot, token.generation, token.page);
        match entry.outcome() {
            MissOutcome::Pending => ReadyResult::NotYet(token),
            MissOutcome::Failed(errno) => {
                let slot = token.slot;
                let generation = token.generation;
                let remaining = token.consume();
                debug_assert_eq!(
                    self.miss_interests.waiters(slot, generation),
                    Some(remaining),
                    "terminal consumption updates the exact generation"
                );
                control
                    .miss
                    .clean_terminal_zero(slot, generation, &self.miss_interests);
                ReadyResult::Err(IoError::from_raw(errno))
            }
            MissOutcome::Succeeded => {
                assert_eq!(
                    self.table.lookup(entry.page()),
                    Some(entry.frame()),
                    "a successful terminal record protects its exact mapped frame"
                );
                let guard = self
                    .pin_owned(reader, entry.page())
                    .expect("the protected successful frame remains pinnable");
                let slot = token.slot;
                let generation = token.generation;
                let _ = token.consume();
                control
                    .miss
                    .clean_terminal_zero(slot, generation, &self.miss_interests);
                ReadyResult::Ready(guard)
            }
        }
    }

    /// Borrows this pool's closed product staging arena.
    #[must_use]
    pub fn write_arena(&self) -> PoolWriteArena<'_> {
        PoolWriteArena {
            state: self.driver.write_arena_state(),
            pool_identity: self.identity,
            enabled_slots: self.write_slots,
        }
    }

    /// Admits a positional product write without blocking.
    ///
    /// # Errors
    ///
    /// Returns the unchanged slot with [`PoolSubmitError`] on fixed-capacity,
    /// stale-file, or foreign-slot refusal.
    ///
    /// # Panics
    ///
    /// If `file` belongs to another pool, or the operation generation exhausts.
    pub fn submit_write<'pool>(
        &'pool self,
        file: FileId,
        slot: PoolWriteSlot<'pool>,
        offset: u64,
    ) -> Result<PoolToken, (PoolSubmitError, PoolWriteSlot<'pool>)> {
        if slot.pool_identity != self.identity {
            return Err((PoolSubmitError::ForeignPool, slot));
        }
        let mut control = self.control();
        let handle = match live_file_handle(&control.files, file, self.identity) {
            Ok(handle) => handle,
            Err(error) => return Err((error, slot)),
        };
        let Some(index) = control
            .product_ops
            .iter()
            .position(|entry| entry.operation.is_none())
        else {
            return Err((PoolSubmitError::Full, slot));
        };
        let PoolWriteSlot {
            slot,
            pool_identity,
        } = slot;
        let driver_token = match self.driver.submit_write(handle, slot, offset) {
            Ok(token) => token,
            Err((SubmitError::Full, slot)) => {
                return Err((
                    PoolSubmitError::Full,
                    PoolWriteSlot {
                        slot,
                        pool_identity,
                    },
                ));
            }
            Err((SubmitError::StaleHandle, slot)) => {
                return Err((
                    PoolSubmitError::StaleFile { file },
                    PoolWriteSlot {
                        slot,
                        pool_identity,
                    },
                ));
            }
        };
        control.product_sequence = control
            .product_sequence
            .checked_add(1)
            .expect("product submission order exhausted before wraparound");
        let order = control.product_sequence;
        let product_slot = &mut control.product_ops[index];
        product_slot.generation = product_slot
            .generation
            .checked_add(1)
            .expect("product operation generation exhausted before ABA");
        let token = PoolToken::new(
            u32::try_from(index).expect("product op table indexes by u32"),
            product_slot.generation,
        );
        product_slot.operation = Some(ProductOp::Write {
            token,
            driver_token,
            file,
            order,
        });
        self.wake.wake();
        Ok(token)
    }

    /// Admits a durability barrier, withholding it behind prior writes to the
    /// same file.
    ///
    /// # Errors
    ///
    /// Returns [`PoolSubmitError::Full`] at fixed capacity or
    /// [`PoolSubmitError::StaleFile`] for a retired generation.
    ///
    /// # Panics
    ///
    /// If `file` belongs to another pool, or the operation generation exhausts.
    pub fn submit_fsync(
        &self,
        file: FileId,
        mode: crate::driver::SyncMode,
    ) -> Result<PoolToken, PoolSubmitError> {
        let crate::driver::SyncMode::Full = mode;
        let mut control = self.control();
        let _ = live_file_handle(&control.files, file, self.identity)?;
        let Some(index) = control
            .product_ops
            .iter()
            .position(|entry| entry.operation.is_none())
        else {
            return Err(PoolSubmitError::Full);
        };
        control.product_sequence = control
            .product_sequence
            .checked_add(1)
            .expect("product submission order exhausted before wraparound");
        let order = control.product_sequence;
        let product_slot = &mut control.product_ops[index];
        product_slot.generation = product_slot
            .generation
            .checked_add(1)
            .expect("product operation generation exhausted before ABA");
        let token = PoolToken::new(
            u32::try_from(index).expect("product op table indexes by u32"),
            product_slot.generation,
        );
        product_slot.operation = Some(ProductOp::FsyncHeld { token, file, order });
        self.submit_held_fsyncs(&mut control);
        self.wake.wake();
        Ok(token)
    }

    /// Drains and routes backend completions, advances reclamation, and delivers
    /// as many owned product results as the caller batch can hold.
    ///
    /// # Panics
    ///
    /// If a backend completion violates its admitted identity or capacity.
    pub fn poll_report(&self, out: &mut PoolCompletionBatch) -> PollReport {
        out.reset();
        let mut control = self.control();
        let backend = self.drain_completions(&mut control);
        let reclaimed = self.advance_and_reclaim(&mut control);
        Self::deliver_product_completions(&mut control, out);
        self.progress_retirements(&mut control);
        if backend > 0 || reclaimed > 0 || out.iter().next().is_some() {
            self.wake.consume_current();
        }
        PollReport::new(
            backend,
            u32::try_from(reclaimed).expect("reclaimed frame count fits u32"),
        )
    }

    /// Waits for I/O or external owner-loop ingress, then performs one truthful
    /// progress pass. The wait latch is independent of the pool control mutex.
    ///
    /// # Panics
    ///
    /// If a backend completion violates its admitted identity or capacity.
    pub fn poll_wait(&self, out: &mut PoolCompletionBatch, timeout: Duration) -> PollReport {
        let report = self.poll_report(out);
        if report.backend_completions() > 0
            || report.reclaimed_frames() > 0
            || out.iter().next().is_some()
        {
            return report;
        }
        let mut waited = self.wait_batch();
        let backend = self.driver.poll_wait(&mut waited, timeout);
        let mut control = self.control();
        while let Some(completion) = waited.pop() {
            control.batch.push(completion);
        }
        drop(waited);
        self.route_completion_batch(&mut control);
        let reclaimed = self.advance_and_reclaim(&mut control);
        Self::deliver_product_completions(&mut control, out);
        self.progress_retirements(&mut control);
        if backend > 0 || reclaimed > 0 || out.iter().next().is_some() {
            self.wake.consume_current();
        }
        PollReport::new(
            u32::try_from(backend).expect("backend completion count fits u32"),
            u32::try_from(reclaimed).expect("reclaimed frame count fits u32"),
        )
    }

    /// Clones the thread-safe external ingress wake capability.
    #[must_use]
    pub fn wake_handle(&self) -> PoolWakeHandle {
        PoolWakeHandle {
            state: Arc::clone(&self.wake),
        }
    }

    /// Starts or advances typed file retirement.
    ///
    /// # Panics
    ///
    /// If `file` belongs to another pool.
    pub fn retire_file(&self, file: FileId) -> RetireStatus {
        assert_eq!(
            file.driver(),
            self.identity,
            "file identity used with a foreign pool"
        );
        let mut control = self.control();
        let index = file.slot() as usize;
        let Some(entry) = control.files[index].as_mut() else {
            return RetireStatus::Retired;
        };
        if entry.id != file {
            return RetireStatus::Retired;
        }
        if entry.state == PoolFileState::Retired {
            return RetireStatus::Retired;
        }
        let started = begin_file_retirement(entry, &self.file_live_generations[index], file);
        self.progress_retirements(&mut control);
        if started {
            RetireStatus::Retiring
        } else if control.files[index]
            .as_ref()
            .is_some_and(|entry| entry.state == PoolFileState::Retired)
        {
            RetireStatus::Retired
        } else {
            RetireStatus::Retiring
        }
    }

    fn deliver_product_completions(control: &mut Control, out: &mut PoolCompletionBatch) {
        for slot in &mut control.product_ops {
            if out.remaining() == 0 {
                break;
            }
            let Some(operation) = slot.operation.take() else {
                continue;
            };
            match operation {
                ProductOp::Completed { completion, .. } => out.push(completion),
                operation => slot.operation = Some(operation),
            }
        }
    }

    fn progress_retirements(&self, control: &mut Control) {
        for index in 0..control.files.len() {
            let Some(file) = control.files[index]
                .as_ref()
                .filter(|entry| entry.state == PoolFileState::Retiring)
                .map(|entry| entry.id)
            else {
                continue;
            };
            if self.resident_lease_states[index].count() > 0 {
                continue;
            }
            self.retire_file_frames(control, file);
            let miss_live = control.miss.has_live_for_file(file, &self.miss_interests);
            let frames_live = control.frame_pages.iter().enumerate().any(|(frame, page)| {
                page.is_some_and(|page| page.file() == file)
                    && self.frames.state(ReadFrameIdx::new(
                        u32::try_from(frame).expect("frame index fits u32"),
                    )) != FrameState::Free
            });
            let product_live = control.product_ops.iter().any(|slot| {
                slot.operation
                    .as_ref()
                    .is_some_and(|operation| operation.file() == file)
            });
            if !miss_live && !frames_live && !product_live {
                let entry = control.files[index]
                    .as_mut()
                    .expect("the retiring file remains registered");
                if let Some(handle) = entry.handle.take() {
                    self.driver.close(handle);
                }
                if self.driver.is_closed(file) {
                    entry.state = PoolFileState::Retired;
                }
            }
        }
    }

    fn retire_file_frames(&self, control: &mut Control, file: FileId) {
        let epoch = self.global_epoch.load(Ordering::Acquire);
        for index in 0..control.frame_pages.len() {
            let Some(page) = control.frame_pages[index] else {
                continue;
            };
            if page.file() != file {
                continue;
            }
            let frame = ReadFrameIdx::new(u32::try_from(index).expect("frame index fits u32"));
            if self.frames.state(frame) != FrameState::Resident {
                continue;
            }
            if !control.miss.prepare_eviction(frame, &self.miss_interests) {
                continue;
            }
            let _ = self.table.remove_shared(page);
            self.frames.advance(frame, FrameState::Evicting);
            control.evict_queue.push(frame, epoch);
        }
    }

    /// The poll-boundary pass: drain the driver's completions (routing each into
    /// its frame, reslicing a short read, or failing the miss), then advance the
    /// global epoch and reclaim matured `Evicting` frames. Returns the number
    /// reclaimed.
    pub fn poll(&self) -> usize {
        let mut control = self.control();
        let _ = self.drain_completions(&mut control);
        let reclaimed = self.advance_and_reclaim(&mut control);
        self.progress_retirements(&mut control);
        debug_assert!(
            reclaimed <= self.frame_count as usize,
            "a poll reclaims at most every frame"
        );
        reclaimed
    }

    /// The residency state of `frame` — an observation seam for the epoch tests.
    #[must_use]
    pub(crate) fn frame_state_internal(&self, frame: ReadFrameIdx) -> FrameState {
        self.frames.state(frame)
    }

    #[cfg(feature = "bench")]
    pub(crate) fn global_epoch_observed_internal(&self) -> u64 {
        self.global_epoch.load(Ordering::Acquire)
    }

    #[cfg(feature = "bench")]
    pub(crate) fn reclamation_epochs_observed_internal(&self) -> Option<(u64, u64)> {
        let control = self.control();
        let tagged = control.evict_queue.oldest_tagged_epoch()?;
        let global = self.global_epoch.load(Ordering::Acquire);
        Some((tagged, global))
    }

    /// Mints an epoch-pinned guard over `page` for `reader`: publishes the
    /// reader's epoch BEFORE validating the exact mapping, so an eviction that
    /// removed the mapping is observed as a miss (`None`) rather than handing back
    /// reclaimable bytes.
    pub(crate) fn pin_internal<'ctx>(
        &'ctx self,
        reader: &'ctx ReaderCtx,
        page: PageId,
    ) -> Option<FrameGuard<'ctx>> {
        self.assert_reader_owner(reader);
        self.pin_owned(reader, page)
    }

    fn pin_owned<'ctx>(
        &'ctx self,
        reader: &'ctx ReaderCtx,
        page: PageId,
    ) -> Option<FrameGuard<'ctx>> {
        let slot = reader.slot();
        let first_guard = slot.begin_pin(self.global_epoch.load(Ordering::Acquire));
        let mapped = self.table.lookup(page);
        let Some(frame) = mapped else {
            if first_guard {
                slot.abort_pin();
            }
            return None;
        };
        let _ = self.clock.reference(frame);
        slot.commit_pin();
        Some(FrameGuard::new(self.frames.frame_bytes(frame), slot))
    }

    fn assert_reader_owner(&self, reader: &ReaderCtx) {
        assert!(
            reader.belongs_to(&self.readers),
            "a reader capability cannot cross pool identity"
        );
    }

    fn assert_lease_owner(&self, lease: &ResidentFileLease, page: PageId) {
        assert_eq!(
            page.file().driver(),
            self.identity,
            "file identity used with a foreign pool"
        );
        assert_eq!(
            lease.pool_identity, self.identity,
            "resident lease capability cannot cross pool identity"
        );
        assert_eq!(
            lease.file,
            page.file(),
            "resident lease names the exact page file"
        );
        let slot = page.file().slot() as usize;
        assert!(
            slot < self.resident_lease_states.len(),
            "file slot is within the lease table"
        );
        assert!(
            Arc::ptr_eq(&lease.state, &self.resident_lease_states[slot]),
            "resident lease state belongs to this pool's exact file slot"
        );
    }

    /// Makes `page` resident in a freshly claimed frame filled with `fill`,
    /// standing in for the miss-completion path so a hit can be set up in
    /// isolation.
    ///
    /// # Panics
    ///
    /// If no frame is `Free` — the watermark bounds a well-behaved caller below
    /// this.
    pub(crate) fn insert_resident_frame_internal(&self, page: PageId, fill: u8) -> ReadFrameIdx {
        let mut control = self.control();
        let frame = self
            .first_free_frame()
            .expect("frame pool exhausted: no Free frame to make resident");
        self.frames.advance(frame, FrameState::InFlight);
        self.frames.fill_inflight(frame, fill);
        self.frames.write_exact_page(frame, page);
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
    pub(crate) fn evict_frame_internal(&self, page: PageId) -> ReadFrameIdx {
        let mut control = self.control();
        let frame = self
            .table
            .lookup(page)
            .expect("evict_frame targets a mapped page");
        assert!(
            control.miss.prepare_eviction(frame, &self.miss_interests),
            "a testing eviction cannot bypass live pending-token protection"
        );
        assert_eq!(
            self.table.remove_shared(page),
            Some(frame),
            "the control lock keeps the eviction mapping stable"
        );
        self.frames.advance(frame, FrameState::Evicting);
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
        let fd = registered_file(&control.files, page);
        self.driver.submit_read(fd, frame, offset, filled, len)
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
        let _ = self.drain_completions(control);
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
            if !control.miss.prepare_eviction(victim, &self.miss_interests) {
                continue;
            }
            if let Some(page) = control.frame_pages[victim.get() as usize] {
                self.table.remove_shared(page);
            }
            self.frames.advance(victim, FrameState::Evicting);
            control.evict_queue.push(victim, epoch);
            return;
        }
    }

    fn drain_completions(&self, control: &mut Control) -> u32 {
        let drained = self.driver.poll(&mut control.batch);
        self.route_completion_batch(control);
        u32::try_from(drained).expect("backend completion count fits u32")
    }

    fn route_completion_batch(&self, control: &mut Control) {
        while let Some(completion) = control.batch.pop() {
            let (driver_token, kind, result) = completion.into_parts();
            if kind != OpKind::Read {
                Self::finish_product_completion(control, driver_token, kind, result);
                continue;
            }
            let Some(index) = control.miss.find_by_token(driver_token) else {
                continue;
            };
            let entry = control.miss.entry(index);
            match result {
                Ok(0) => {
                    self.drain_completions_finish_failure(
                        &mut control.miss,
                        index,
                        entry,
                        SHORT_READ_EOF_ERRNO,
                    );
                }
                Ok(bytes) => {
                    let filled = entry.filled() + bytes;
                    if filled >= self.granule {
                        self.drain_completions_finish_success(
                            &mut control.miss,
                            &mut control.frame_pages,
                            index,
                            entry,
                        );
                    } else {
                        let (offset, len) = read_span(entry.page(), self.granule, filled);
                        let fd = registered_file(&control.files, entry.page());
                        if let Ok(token) =
                            self.driver
                                .submit_read(fd, entry.frame(), offset, filled, len)
                        {
                            control.miss.advance_remainder(index, filled, token);
                        } else {
                            self.drain_completions_finish_failure(
                                &mut control.miss,
                                index,
                                entry,
                                SHORT_READ_EOF_ERRNO,
                            );
                        }
                    }
                }
                Err(err) => {
                    let errno = err.raw_os_error().unwrap_or(SHORT_READ_EOF_ERRNO);
                    self.drain_completions_finish_failure(&mut control.miss, index, entry, errno);
                }
            }
        }
        self.submit_held_fsyncs(control);
    }

    fn finish_product_completion(
        control: &mut Control,
        driver_token: OpToken,
        kind: OpKind,
        result: Result<u32, IoError>,
    ) {
        let slot = control
            .product_ops
            .iter_mut()
            .find(|slot| {
                slot.operation
                    .as_ref()
                    .is_some_and(|operation| match operation {
                        ProductOp::Write {
                            driver_token: expected,
                            ..
                        }
                        | ProductOp::Fsync {
                            driver_token: expected,
                            ..
                        } => *expected == driver_token,
                        ProductOp::FsyncHeld { .. } | ProductOp::Completed { .. } => false,
                    })
            })
            .expect("every product backend completion names an admitted operation");
        let operation = slot.operation.take().expect("the product slot is occupied");
        slot.operation = Some(match (operation, kind) {
            (ProductOp::Write { token, file, .. }, OpKind::Write) => ProductOp::Completed {
                file,
                completion: PoolCompletion::Write { token, result },
            },
            (ProductOp::Fsync { token, file, .. }, OpKind::Fsync) => ProductOp::Completed {
                file,
                completion: PoolCompletion::Fsync {
                    token,
                    result: result.map(|_| ()),
                },
            },
            _ => panic!("product completion kind matches its admitted operation"),
        });
    }

    fn submit_held_fsyncs(&self, control: &mut Control) {
        for index in 0..control.product_ops.len() {
            let Some(ProductOp::FsyncHeld { token, file, order }) =
                control.product_ops[index].operation.as_ref()
            else {
                continue;
            };
            let token = *token;
            let file = *file;
            let barrier_order = *order;
            let prior_write = control.product_ops.iter().any(|slot| {
                matches!(
                    slot.operation,
                    Some(ProductOp::Write {
                        file: write_file,
                        order: write_order,
                        ..
                    }) if write_file == file && write_order < barrier_order
                )
            });
            if prior_write {
                continue;
            }
            let handle = registered_file(&control.files, PageId::new(file, 0));
            match self
                .driver
                .submit_fsync(handle, crate::driver::SyncMode::Full)
            {
                Ok(driver_token) => {
                    control.product_ops[index].operation = Some(ProductOp::Fsync {
                        token,
                        driver_token,
                        file,
                    });
                    self.wake.wake();
                }
                Err(SubmitError::Full) => {}
                Err(SubmitError::StaleHandle) => {
                    panic!("a held product fsync retains its live backend file")
                }
            }
        }
    }

    fn drain_completions_finish_success(
        &self,
        miss: &mut MissTable,
        frame_pages: &mut [Option<PageId>],
        index: usize,
        entry: MissEntry,
    ) {
        self.frames.write_exact_page(entry.frame(), entry.page());
        self.frames.advance(entry.frame(), FrameState::Resident);
        self.table.insert_shared(entry.page(), entry.frame());
        frame_pages[entry.frame().get() as usize] = Some(entry.page());
        let _ = self.clock.reference(entry.frame());
        miss.succeed(index);
        miss.clean_terminal_zero(
            MissSlot::new(index),
            entry.generation(),
            &self.miss_interests,
        );
    }

    fn drain_completions_finish_failure(
        &self,
        miss: &mut MissTable,
        index: usize,
        entry: MissEntry,
        errno: i32,
    ) {
        self.frames.abort_inflight(entry.frame());
        miss.fail(index, errno);
        miss.clean_terminal_zero(
            MissSlot::new(index),
            entry.generation(),
            &self.miss_interests,
        );
    }

    fn advance_and_reclaim(&self, control: &mut Control) -> usize {
        let global_epoch = advance_epoch(&self.global_epoch, self.readers.slots());
        let frames = &self.frames;
        let frame_pages = &mut control.frame_pages;
        control.evict_queue.drain_matured(global_epoch, |frame| {
            frames.advance(frame, FrameState::Free);
            frame_pages[frame.get() as usize] = None;
        })
    }

    /// Cumulative clear→set CLOCK reference-bit stores across every `get()` hit
    /// path — the DIO-G1 store-elision observation seam (T009 zero-alloc gate).
    #[must_use]
    pub(crate) fn clock_reference_stores_internal(&self) -> u64 {
        self.clock.reference_stores()
    }

    #[must_use]
    pub(crate) fn file_is_retired_observed_internal(&self, file: FileId) -> bool {
        assert_eq!(
            file.driver(),
            self.identity,
            "file identity used with a foreign pool"
        );
        let control = self.control();
        let index = file.slot() as usize;
        assert!(
            index < control.files.len(),
            "file slot is within the fixed registry"
        );
        control.files[index]
            .as_ref()
            .is_some_and(|entry| entry.id == file && entry.state == PoolFileState::Retired)
    }

    #[cfg(feature = "mock")]
    #[must_use]
    pub(crate) fn control_acquisitions_internal(&self) -> u64 {
        self.control_acquisitions.load(ObservationOrdering::Relaxed)
    }

    #[must_use]
    pub(crate) fn pending_waiters_internal(&self, token: &PendingToken) -> u32 {
        assert!(
            Arc::ptr_eq(&token.interests, &self.miss_interests),
            "pending waiter observation uses its owning pool"
        );
        self.miss_interests
            .waiters(token.slot, token.generation)
            .expect("pending waiter observation names the exact generation")
    }
}

/// Publishes a reader epoch, then validates an advisory hint before committing a
/// normal frame pin. A failed first pin is returned to quiescence before callers
/// take their ordinary lookup fallback.
pub(crate) fn pin_with_resident_hint(
    frames: &Frames,
    clock: &Clock,
    global_epoch: &AtomicU64,
    slot: &epoch::ReaderSlot,
    page: PageId,
    hint: ResidentHint,
) -> Option<ReadFrameIdx> {
    if hint.granule != page.granule_idx() || hint.frame >= frames.count() {
        return None;
    }
    let frame = ReadFrameIdx::new(hint.frame);
    let first_guard = slot.begin_pin(global_epoch.load(Ordering::Acquire));
    let word = frames.state_word(frame);
    if word != hint.stamp.get() || !Frames::word_is_resident(word) {
        if first_guard {
            slot.abort_pin();
        }
        return None;
    }
    if frames.exact_page(frame) != page {
        if first_guard {
            slot.abort_pin();
        }
        return None;
    }
    let _ = clock.reference(frame);
    slot.commit_pin();
    Some(frame)
}

/// The file offset and length of the read that fills a page's granule from
/// `filled` bytes on: the whole granule at `filled == 0`, the reslice remainder
/// tail thereafter (scope.md:601).
fn read_span(page: PageId, granule: u32, filled: u32) -> (u64, u32) {
    let base = u64::from(page.granule_idx()) * u64::from(granule);
    (base + u64::from(filled), granule - filled)
}

fn registered_file(files: &[Option<PoolFile>], page: PageId) -> &FileHandle {
    let file = files[page.file().slot() as usize]
        .as_ref()
        .expect("a registered file backs every requested page");
    assert_eq!(
        file.id,
        page.file(),
        "a page capability must name the exact registered file identity"
    );
    file.handle
        .as_ref()
        .expect("an admitted read retains its backend handle")
}

fn file_is_live(files: &[Option<PoolFile>], file: FileId, pool_identity: u64) -> bool {
    assert_eq!(
        file.driver(),
        pool_identity,
        "file identity used with a foreign pool"
    );
    files[file.slot() as usize]
        .as_ref()
        .is_some_and(|entry| entry.id == file && entry.state == PoolFileState::Live)
}

pub(crate) fn acquire_resident_file_lease(
    state: &Arc<ResidentLeaseState>,
    file: FileId,
) -> Result<ResidentFileLease, ResidentLeaseError> {
    ResidentLeaseState::acquire(state, file)
}

fn file_live_word(file: FileId) -> u64 {
    (u64::from(file.generation()) << 1) | 1
}

fn publish_live_file(
    files: &mut [Option<PoolFile>],
    live_generation: &AtomicU64,
    resident_lease_state: &ResidentLeaseState,
    id: FileId,
    handle: Option<FileHandle>,
) {
    let slot = id.slot() as usize;
    assert!(slot < files.len(), "file slot is within the fixed registry");
    resident_lease_state.publish_generation(id);
    files[slot] = Some(PoolFile {
        id,
        handle,
        state: PoolFileState::Live,
    });
    live_generation.store(file_live_word(id), Ordering::Release);
}

fn file_generation_is_live(live_generation: &AtomicU64, file: FileId) -> bool {
    live_generation.load(Ordering::Acquire) == file_live_word(file)
}

fn begin_file_retirement(entry: &mut PoolFile, live_generation: &AtomicU64, file: FileId) -> bool {
    assert_eq!(entry.id, file, "retirement names the exact generation");
    assert_ne!(
        entry.state,
        PoolFileState::Retired,
        "a retired file cannot begin retirement"
    );
    let started = entry.state == PoolFileState::Live;
    entry.state = PoolFileState::Retiring;
    live_generation.store(0, Ordering::Release);
    started
}

fn live_file_handle(
    files: &[Option<PoolFile>],
    file: FileId,
    pool_identity: u64,
) -> Result<&FileHandle, PoolSubmitError> {
    assert_eq!(
        file.driver(),
        pool_identity,
        "file identity used with a foreign pool"
    );
    files[file.slot() as usize]
        .as_ref()
        .filter(|entry| entry.id == file && entry.state == PoolFileState::Live)
        .and_then(|entry| entry.handle.as_ref())
        .ok_or(PoolSubmitError::StaleFile { file })
}

impl PoolBackend for Driver {
    fn identity(&self) -> u64 {
        self.identity()
    }

    fn attach_pool_state(&self, _lifecycle: Arc<LifecycleCounters>, wake: Arc<WaitState>) {
        self.attach_pool_wait(wake);
    }

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
        self.submit_read_range(fd, frame, file_offset, destination_offset, len)
    }

    fn poll(&self, out: &mut CompletionBatch) -> usize {
        Driver::poll(self, out)
    }

    fn poll_wait(&self, out: &mut CompletionBatch, timeout: Duration) -> usize {
        self.poll_wait_for_pool(out, timeout)
    }

    fn write_arena_state(&self) -> &write_arena::ArenaState {
        self.write_arena_state()
    }

    fn submit_write<'arena>(
        &self,
        fd: &FileHandle,
        slot: write_arena::WriteSlot<'arena>,
        offset: u64,
    ) -> Result<OpToken, (SubmitError, write_arena::WriteSlot<'arena>)> {
        Driver::submit_write(self, fd, slot, offset)
    }

    fn submit_fsync(
        &self,
        fd: &FileHandle,
        mode: crate::driver::SyncMode,
    ) -> Result<OpToken, SubmitError> {
        Driver::submit_fsync(self, fd, mode)
    }

    fn close(&self, fd: FileHandle) {
        Driver::close(self, fd);
    }

    fn is_closed(&self, file: FileId) -> bool {
        Driver::is_closed(self, file)
    }
}

impl PoolBackendSealed for Driver {}
