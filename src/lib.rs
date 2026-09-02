//! Completion-based async direct-IO driver and userspace frame pool.
//!
//! An ordinary warm [`Pool::get`] hit is the default no-hint path and performs
//! no hint-specific branch, load, store, or RMW. Callers with repeated access
//! to an exact file generation may opt into [`Pool::lease_file`],
//! [`ResidentFileLease`], [`Pool::resident_hint`], [`ResidentHint`], and
//! [`Pool::get_with_hint`]. A hint is advisory; `get_with_hint` owns fallback
//! to ordinary `Pool::get` behavior when the observation is absent, mismatched,
//! or stale.
//!
//! A [`ResidentFileLease`] protects the lifetime of one exact file generation,
//! but does not retain or pin frames. Its pages remain eligible for normal
//! eviction. The lease/hint API therefore makes no resident-set,
//! frame-retention, or other R8 guarantee. After pool construction, the
//! zero-allocation proof covers ordinary warm hits, hinted hits, stale-hint
//! fallback, lease acquire/drop, and retirement progress on both eager-inline
//! and `io_uring`.
//!
//! The binding R7 gates retained the current four-round page hash and selected
//! this opt-in API. The ordinary path remains the no-hint default.
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
        reason = "structural pool probes are public only through the feature-gated testing module"
    )
)]

mod alignment;
mod allocation;
mod backend;
mod completion;
pub mod driver;
mod error;
mod open;
mod pool;
mod product;
mod sync;

#[cfg(loom)]
#[doc(hidden)]
pub use pool::loom_model;

#[cfg(feature = "mock")]
#[doc(hidden)]
mod mock;

/// Deterministic backends and structural observation seams for tests.
#[cfg(any(feature = "mock", feature = "bench"))]
pub mod testing {
    use std::cell::Cell;
    use std::marker::PhantomData;
    #[cfg(feature = "mock")]
    use std::sync::Arc;
    #[cfg(feature = "mock")]
    use std::sync::atomic::Ordering;

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

        pub fn advance(&self, frame: ReadFrameIdx, to: FrameState) {
            self.frames.advance(frame, to);
        }

        #[must_use]
        pub fn state_word(&self, frame: ReadFrameIdx) -> u64 {
            self.frames.state_word(frame)
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
        /// Returns configuration or fixed-allocation failure.
        fn build_on(
            self,
            driver: MockDriver,
        ) -> Result<crate::pool::Pool<MockDriver>, crate::pool::PoolBuildError>;
    }

    #[cfg(feature = "mock")]
    impl PoolBuilderTestingExt for crate::pool::PoolBuilder {
        fn build_on(
            self,
            driver: MockDriver,
        ) -> Result<crate::pool::Pool<MockDriver>, crate::pool::PoolBuildError> {
            self.build_on_internal(driver)
        }
    }

    /// Mock-ring construction for Pool-level retry-CQE progress tests.
    #[cfg(feature = "mock")]
    pub trait MockRingPoolBuilderTestingExt {
        /// Builds a Pool over the mock ring's real retry/reap path.
        ///
        /// # Errors
        ///
        /// Returns configuration or fixed-allocation failure.
        fn build_on_ring(
            self,
            driver: MockRingDriver,
        ) -> Result<crate::pool::Pool<MockRingDriver>, crate::pool::PoolBuildError>;
    }

    #[cfg(feature = "mock")]
    impl MockRingPoolBuilderTestingExt for crate::pool::PoolBuilder {
        fn build_on_ring(
            self,
            driver: MockRingDriver,
        ) -> Result<crate::pool::Pool<MockRingDriver>, crate::pool::PoolBuildError> {
            self.build_on_ring_internal(driver)
        }
    }

    /// Narrow control-plane seams for deterministic residency tests.
    pub trait PoolTestingExt {
        fn register_file(&self, file: crate::driver::FileHandle);
        fn frame_state(&self, frame: ReadFrameIdx) -> FrameState;
        fn pin<'ctx>(
            &'ctx self,
            reader: &'ctx crate::pool::ReaderCtx,
            page: crate::pool::PageId,
        ) -> Option<crate::pool::FrameGuard<'ctx>>;
        fn insert_resident_frame(&self, page: crate::pool::PageId, fill: u8) -> ReadFrameIdx;
        fn evict_frame(&self, page: crate::pool::PageId) -> ReadFrameIdx;
        fn clock_reference_stores(&self) -> u64;
        #[cfg(feature = "bench")]
        fn global_epoch_observed(&self) -> u64;
        #[cfg(feature = "bench")]
        fn reclamation_epochs_observed(&self) -> Option<(u64, u64)>;
        fn file_is_retired_observed(&self, file: crate::driver::FileId) -> bool;
        #[cfg(feature = "mock")]
        fn control_acquisitions(&self) -> u64;
        fn pending_waiters(&self, token: &crate::pool::PendingToken) -> u32;
    }

    macro_rules! impl_pool_testing_ext {
        ($backend:ty) => {
            impl PoolTestingExt for crate::pool::Pool<$backend> {
                fn register_file(&self, file: crate::driver::FileHandle) {
                    self.register_file_internal(file);
                }

                fn frame_state(&self, frame: ReadFrameIdx) -> FrameState {
                    self.frame_state_internal(frame)
                }

                fn pin<'ctx>(
                    &'ctx self,
                    reader: &'ctx crate::pool::ReaderCtx,
                    page: crate::pool::PageId,
                ) -> Option<crate::pool::FrameGuard<'ctx>> {
                    self.pin_internal(reader, page)
                }

                fn insert_resident_frame(
                    &self,
                    page: crate::pool::PageId,
                    fill: u8,
                ) -> ReadFrameIdx {
                    self.insert_resident_frame_internal(page, fill)
                }

                fn evict_frame(&self, page: crate::pool::PageId) -> ReadFrameIdx {
                    self.evict_frame_internal(page)
                }

                fn clock_reference_stores(&self) -> u64 {
                    self.clock_reference_stores_internal()
                }

                #[cfg(feature = "bench")]
                fn global_epoch_observed(&self) -> u64 {
                    self.global_epoch_observed_internal()
                }

                #[cfg(feature = "bench")]
                fn reclamation_epochs_observed(&self) -> Option<(u64, u64)> {
                    self.reclamation_epochs_observed_internal()
                }

                fn file_is_retired_observed(&self, file: crate::driver::FileId) -> bool {
                    self.file_is_retired_observed_internal(file)
                }

                #[cfg(feature = "mock")]
                fn control_acquisitions(&self) -> u64 {
                    self.control_acquisitions_internal()
                }

                fn pending_waiters(&self, token: &crate::pool::PendingToken) -> u32 {
                    self.pending_waiters_internal(token)
                }
            }
        };
    }

    impl_pool_testing_ext!(crate::driver::Driver);
    #[cfg(feature = "mock")]
    impl_pool_testing_ext!(MockDriver);
    #[cfg(feature = "mock")]
    impl_pool_testing_ext!(MockRingDriver);

    /// Mock-only access to deterministic fault injection and observations.
    #[cfg(feature = "mock")]
    pub trait MockPoolTestingExt {
        fn driver(&self) -> &MockDriver;
        fn observe(&self) -> Arc<MockPoolObservation>;
        fn set_resident_lease_count(&self, file: crate::driver::FileId, count: u32);
        fn resident_lease_count(&self, file: crate::driver::FileId) -> u32;
        fn observe_resident_lease_count(
            &self,
            file: crate::driver::FileId,
        ) -> MockResidentLeaseCountObservation;
        #[cfg(not(loom))]
        fn pause_next_cold_get(&self) -> ColdGetPauseObservation;
    }

    #[cfg(feature = "mock")]
    impl MockPoolTestingExt for crate::pool::Pool<MockDriver> {
        fn driver(&self) -> &MockDriver {
            self.driver_internal()
        }

        fn observe(&self) -> Arc<MockPoolObservation> {
            Arc::new(MockPoolObservation {
                lifecycle: self.lifecycle_internal(),
            })
        }
        fn set_resident_lease_count(&self, file: crate::driver::FileId, count: u32) {
            self.set_resident_lease_count_internal(file, count);
        }

        fn resident_lease_count(&self, file: crate::driver::FileId) -> u32 {
            self.resident_lease_count_internal(file)
        }

        fn observe_resident_lease_count(
            &self,
            file: crate::driver::FileId,
        ) -> MockResidentLeaseCountObservation {
            MockResidentLeaseCountObservation {
                state: self.observe_resident_lease_count_internal(file),
            }
        }

        #[cfg(not(loom))]
        fn pause_next_cold_get(&self) -> ColdGetPauseObservation {
            ColdGetPauseObservation {
                state: self.pause_next_cold_get_internal(),
            }
        }
    }

    /// Arc-backed observation of one exact preallocated file-slot lease count.
    #[cfg(feature = "mock")]
    #[derive(Debug, Clone)]
    pub struct MockResidentLeaseCountObservation {
        state: Arc<crate::pool::ResidentLeaseState>,
    }

    #[cfg(feature = "mock")]
    impl MockResidentLeaseCountObservation {
        #[must_use]
        pub fn count(&self) -> u32 {
            self.state.count()
        }
    }

    /// One-shot mock-only control for the cold-get/retirement race.
    #[cfg(all(feature = "mock", not(loom)))]
    #[derive(Debug, Clone)]
    pub struct ColdGetPauseObservation {
        state: Arc<crate::pool::ColdGetPauseState>,
    }

    #[cfg(all(feature = "mock", not(loom)))]
    impl ColdGetPauseObservation {
        /// Waits until the get has passed its optimistic liveness check and is
        /// paused before taking the admission lock.
        #[must_use]
        pub fn wait_until_parked(&self, timeout: std::time::Duration) -> bool {
            self.state.wait_until_parked(timeout)
        }

        /// Releases the paused get to continue into cold admission.
        pub fn release(&self) {
            self.state.release();
        }
    }

    /// Arc-backed chronological I/O observation that remains readable after
    /// its Pool and mock driver have dropped.
    #[cfg(feature = "mock")]
    pub trait MockPoolIoTestingExt {
        fn observe_io(&self) -> MockIoObservation;
    }

    #[cfg(feature = "mock")]
    impl MockPoolIoTestingExt for crate::pool::Pool<MockDriver> {
        fn observe_io(&self) -> MockIoObservation {
            MockIoObservation {
                events: self.driver_internal().io_event_log_internal(),
            }
        }
    }

    #[cfg(feature = "mock")]
    #[derive(Debug, Clone)]
    pub struct MockIoObservation {
        events: Arc<std::sync::Mutex<Vec<MockIoEvent>>>,
    }

    #[cfg(feature = "mock")]
    impl MockIoObservation {
        #[must_use]
        pub fn io_events_in_order(&self) -> Vec<MockIoEvent> {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    /// Access to the mock ring retained by a product Pool.
    #[cfg(feature = "mock")]
    pub trait MockRingPoolTestingExt {
        fn ring_driver(&self) -> &MockRingDriver;
        #[cfg(target_os = "linux")]
        fn observe_ring_waits(&self) -> MockWaitObservation;
        #[cfg(target_os = "linux")]
        fn poll_wait_raw_progress(&self, timeout: std::time::Duration) -> u32;
    }

    #[cfg(feature = "mock")]
    impl MockRingPoolTestingExt for crate::pool::Pool<MockRingDriver> {
        fn ring_driver(&self) -> &MockRingDriver {
            self.driver_internal()
        }

        #[cfg(target_os = "linux")]
        fn observe_ring_waits(&self) -> MockWaitObservation {
            MockWaitObservation::from_state(self.wait_internal())
        }

        #[cfg(target_os = "linux")]
        fn poll_wait_raw_progress(&self, timeout: std::time::Duration) -> u32 {
            let mut completions = crate::completion::CompletionBatch::with_capacity(1);
            let progress = self
                .driver_internal()
                .poll_wait_for_pool_internal(&mut completions, timeout);
            assert_eq!(
                progress.caller_completions, 0,
                "the raw-progress test seam is only for retry or idle waits"
            );
            progress.backend_completions
        }
    }

    /// Exact Arc-backed product lifecycle observation.
    #[cfg(feature = "mock")]
    #[derive(Debug)]
    pub struct MockPoolObservation {
        lifecycle: Arc<crate::product::LifecycleCounters>,
    }

    #[cfg(feature = "mock")]
    impl MockPoolObservation {
        #[must_use]
        pub fn registered_readers(&self) -> u32 {
            self.lifecycle.registered_readers.load(Ordering::Acquire)
        }
        #[must_use]
        pub fn reader_releases(&self) -> u32 {
            self.lifecycle.reader_releases.load(Ordering::Acquire)
        }
        #[must_use]
        pub fn live_pending_interests(&self) -> u32 {
            self.lifecycle
                .live_pending_interests
                .load(Ordering::Acquire)
        }
        #[must_use]
        pub fn pending_releases(&self) -> u32 {
            self.lifecycle.pending_releases.load(Ordering::Acquire)
        }
        #[must_use]
        pub fn backend_ops_in_flight(&self) -> u32 {
            self.lifecycle.backend_ops_in_flight.load(Ordering::Acquire)
        }
        #[must_use]
        pub fn backend_completions(&self) -> u32 {
            self.lifecycle.backend_completions.load(Ordering::Acquire)
        }
        #[must_use]
        pub fn quiesce_calls(&self) -> u32 {
            self.lifecycle.quiesce_calls.load(Ordering::Acquire)
        }
    }

    #[cfg(feature = "mock")]
    macro_rules! wait_observation {
        ($name:ident) => {
            #[derive(Debug, Clone)]
            pub struct $name {
                inner: crate::product::WaitObservation,
            }

            impl $name {
                pub(crate) fn from_state(state: Arc<crate::product::WaitState>) -> Self {
                    Self {
                        inner: crate::product::WaitObservation { state },
                    }
                }

                #[must_use]
                pub fn wait_until_parked(&self, timeout: std::time::Duration) -> bool {
                    self.inner.wait_until_parked(timeout)
                }

                #[must_use]
                pub fn parks_entered(&self) -> u32 {
                    self.inner.parks_entered()
                }

                #[must_use]
                pub fn parks_in_progress(&self) -> u32 {
                    self.inner.parks_in_progress()
                }

                #[must_use]
                pub fn parks_exited(&self) -> u32 {
                    self.inner.parks_exited()
                }

                #[must_use]
                pub fn wake_exits(&self) -> u32 {
                    self.inner.wake_exits()
                }

                #[must_use]
                pub fn timeout_exits(&self) -> u32 {
                    self.inner.timeout_exits()
                }
            }
        };
    }

    #[cfg(feature = "mock")]
    wait_observation!(MockWaitObservation);
    #[cfg(feature = "mock")]
    wait_observation!(ShippingWaitObservation);

    #[cfg(feature = "mock")]
    pub trait ShippingWaitTestingExt {
        fn observe_shipping_waits(&self) -> ShippingWaitObservation;
    }

    #[cfg(feature = "mock")]
    impl ShippingWaitTestingExt for crate::pool::Pool<crate::driver::Driver> {
        fn observe_shipping_waits(&self) -> ShippingWaitObservation {
            ShippingWaitObservation::from_state(self.wait_internal())
        }
    }

    #[cfg(feature = "mock")]
    pub use crate::mock::{
        DirectIoSupport, Injected, MockDriver, MockDriverBuilder, MockIoEvent, MockRingDriver,
        MockRingDriverBuilder, MockRingObservation, MockWriteArena, ReadAttempt, WriteAttempt,
    };
    #[cfg(any(feature = "mock", feature = "bench"))]
    pub use crate::pool::{Clock, FrameState, PageTable, ReaderCounters};

    /// Calls the shipping four-round page hash for bench evidence generation.
    #[cfg(feature = "bench")]
    #[must_use]
    pub fn current_page_hash(driver: u64, slot: u32, generation: u32, granule: u32) -> u64 {
        let file = crate::FileId::new(driver, slot, generation);
        let page = crate::PageId::new(file, granule);
        crate::pool::page_hash(page)
    }
}

#[cfg(feature = "bench")]
pub mod bench;

pub use driver::{FileId, IoMode, SyncMode};
pub use error::{FileRegistrationError, IoError};
pub use open::DirectIo;
pub use pool::{
    FrameGuard, GRANULE_DEFAULT, Get, GetError, PageId, PendingToken, Pool, PoolBuildError,
    PoolBuilder, PoolConfigError, ReaderCtx, ReadyResult, RegisterError, ResidentFileLease,
    ResidentHint, ResidentLeaseError, RetainRefused, RetainRefusedReason, RetainedFrame,
    RetentionStats,
};
pub use product::{
    PollReport, PoolCompletion, PoolCompletionBatch, PoolSubmitError, PoolToken, PoolWakeHandle,
    PoolWriteArena, PoolWriteSlot, RetireStatus,
};
