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
    /// Read-frame observation used by backend tests and read-bracketing benches.
    pub trait DriverObservation {
        /// Copies one completed frame into `out`.
        fn copy_frame(&self, frame: crate::driver::ReadFrameIdx, out: &mut [u8]) -> usize;
    }

    impl DriverObservation for crate::driver::Driver {
        fn copy_frame(&self, frame: crate::driver::ReadFrameIdx, out: &mut [u8]) -> usize {
            self.copy_frame_testing(frame, out)
        }
    }

    #[cfg(feature = "mock")]
    pub use crate::mock::{
        DirectIoSupport, Injected, MockDriver, MockDriverBuilder, MockRingDriver,
        MockRingDriverBuilder, MockRingObservation, ReadAttempt,
    };
    #[cfg(any(feature = "mock", feature = "bench"))]
    pub use crate::pool::{Clock, FrameState, Frames, PageTable, PoolBackend, ReaderCounters};
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
