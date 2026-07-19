//! Completion-based async direct-IO driver and userspace frame pool.
#![cfg_attr(
    not(feature = "mock"),
    expect(
        dead_code,
        reason = "parts of the shared driver core (DST seams, observation surfaces) are reachable only through the mock backend, so a mock-less build sees them as dead"
    )
)]
#![cfg_attr(
    not(any(feature = "mock", feature = "bench")),
    expect(
        unreachable_pub,
        unnameable_types,
        reason = "structural pool probes are public only through the feature-gated testing module"
    )
)]

mod alignment;
mod backend;
mod completion;
pub mod driver;
mod error;
mod open;
mod pool;
mod sync;

#[cfg(loom)]
#[doc(hidden)]
pub use pool::loom_model;

#[cfg(feature = "mock")]
#[doc(hidden)]
pub mod mock;

/// Deterministic backends and structural observation seams for tests.
#[cfg(any(feature = "mock", feature = "bench"))]
pub mod testing {
    use std::cell::Cell;
    use std::marker::PhantomData;

    pub use crate::pool::ReadFrameIdx;

    /// Feature-gated raw-read admission for backend tests and driver benches.
    pub trait DriverReadTestingExt {
        /// Enqueues a whole-frame raw read under the driver's exclusive frame
        /// lease. Product reads use [`crate::Pool`] instead.
        ///
        /// # Errors
        ///
        /// Returns [`crate::driver::SubmitError::Full`] when admission is full,
        /// or [`crate::driver::SubmitError::StaleHandle`] for a stale handle.
        fn submit_read(
            &self,
            file: &crate::driver::FileHandle,
            frame: ReadFrameIdx,
            offset: u64,
        ) -> Result<crate::driver::OpToken, crate::driver::SubmitError>;
    }

    impl DriverReadTestingExt for crate::driver::Driver {
        fn submit_read(
            &self,
            file: &crate::driver::FileHandle,
            frame: ReadFrameIdx,
            offset: u64,
        ) -> Result<crate::driver::OpToken, crate::driver::SubmitError> {
            self.submit_read_internal(file, frame, offset)
        }
    }

    /// Read-frame observation used by backend tests and read-bracketing benches.
    pub trait DriverObservation {
        /// Copies one completed frame into `out` while holding admission
        /// exclusively and rejecting every frame with a live read slab entry.
        fn copy_frame(&self, frame: ReadFrameIdx, out: &mut [u8]) -> usize;
    }

    impl DriverObservation for crate::driver::Driver {
        fn copy_frame(&self, frame: ReadFrameIdx, out: &mut [u8]) -> usize {
            self.copy_frame_testing(frame, out)
        }
    }

    /// Single-threaded owner of a standalone frame arena for structural tests.
    /// Mutation is unavailable through shared references, and the wrapper is
    /// `!Sync`, so a safe byte view cannot race a test fill or backend DMA.
    #[derive(Debug)]
    pub struct TestFrames {
        frames: crate::pool::Frames,
        _thread_bound: PhantomData<Cell<()>>,
    }

    impl TestFrames {
        #[must_use]
        pub fn preallocated(count: u32, granule: u32) -> Self {
            Self {
                frames: crate::pool::Frames::preallocated(count, granule),
                _thread_bound: PhantomData,
            }
        }

        #[must_use]
        pub fn count(&self) -> u32 {
            self.frames.count()
        }

        #[must_use]
        pub fn frame_bytes(&self, frame: ReadFrameIdx) -> &[u8] {
            self.frames.frame_bytes(frame)
        }

        #[must_use]
        pub fn state(&self, frame: ReadFrameIdx) -> FrameState {
            self.frames.state(frame)
        }
    }

    /// Feature-gated deterministic backend construction for pool tests. The
    /// concrete mock parameter keeps the production driver's fixed arena from
    /// entering this post-construction test seam.
    #[cfg(feature = "mock")]
    pub trait PoolBuilderTestingExt {
        /// Builds a pool over the supplied deterministic driver.
        ///
        /// # Errors
        ///
        /// Returns the pool configuration error when the fixed capacities do
        /// not satisfy the residency invariants.
        fn build_on(
            self,
            driver: MockDriver,
        ) -> Result<crate::pool::Pool<MockDriver>, crate::pool::PoolConfigError>;
    }

    #[cfg(feature = "mock")]
    impl PoolBuilderTestingExt for crate::pool::PoolBuilder {
        fn build_on(
            self,
            driver: MockDriver,
        ) -> Result<crate::pool::Pool<MockDriver>, crate::pool::PoolConfigError> {
            self.build_on_internal(driver)
        }
    }

    /// Narrow control-plane seams for deterministic residency tests.
    pub trait PoolTestingExt<D: PoolBackend> {
        fn register_file(&self, file: crate::driver::FileHandle);
        fn frame_state(&self, frame: ReadFrameIdx) -> FrameState;
        fn pin<'ctx>(
            &'ctx self,
            reader: &'ctx crate::pool::ReaderCtx<'_>,
            page: crate::pool::PageId,
        ) -> Option<crate::pool::FrameGuard<'ctx>>;
        fn insert_resident_frame(&self, page: crate::pool::PageId, fill: u8) -> ReadFrameIdx;
        fn evict_frame(&self, page: crate::pool::PageId) -> ReadFrameIdx;
        fn clock_reference_stores(&self) -> u64;
    }

    impl<D: PoolBackend> PoolTestingExt<D> for crate::pool::Pool<D> {
        fn register_file(&self, file: crate::driver::FileHandle) {
            self.register_file_internal(file);
        }

        fn frame_state(&self, frame: ReadFrameIdx) -> FrameState {
            self.frame_state_internal(frame)
        }

        fn pin<'ctx>(
            &'ctx self,
            reader: &'ctx crate::pool::ReaderCtx<'_>,
            page: crate::pool::PageId,
        ) -> Option<crate::pool::FrameGuard<'ctx>> {
            self.pin_internal(reader, page)
        }

        fn insert_resident_frame(&self, page: crate::pool::PageId, fill: u8) -> ReadFrameIdx {
            self.insert_resident_frame_internal(page, fill)
        }

        fn evict_frame(&self, page: crate::pool::PageId) -> ReadFrameIdx {
            self.evict_frame_internal(page)
        }

        fn clock_reference_stores(&self) -> u64 {
            self.clock_reference_stores_internal()
        }
    }

    /// Mock-only access to deterministic fault injection and observations.
    #[cfg(feature = "mock")]
    pub trait MockPoolTestingExt {
        fn driver(&self) -> &MockDriver;
    }

    #[cfg(feature = "mock")]
    impl MockPoolTestingExt for crate::pool::Pool<MockDriver> {
        fn driver(&self) -> &MockDriver {
            self.driver_internal()
        }
    }

    #[cfg(feature = "mock")]
    pub use crate::mock::{
        DirectIoSupport, Injected, MockDriver, MockDriverBuilder, MockRingDriver,
        MockRingDriverBuilder, MockRingObservation, ReadAttempt,
    };
    #[cfg(any(feature = "mock", feature = "bench"))]
    pub use crate::pool::{Clock, FrameState, PageTable, PoolBackend, ReaderCounters};
}

#[cfg(feature = "bench")]
pub mod bench;

pub use driver::FileId;
pub use error::IoError;
pub use open::DirectIo;
pub use pool::{
    FrameGuard, Get, PageId, PendingToken, Pool, PoolBuildError, PoolBuilder, PoolConfigError,
    ReaderCtx, ReadyResult, RegisterError, GRANULE_DEFAULT,
};
