//! Completion-based async direct-IO driver and userspace frame pool.
#![cfg_attr(
    not(feature = "mock"),
    expect(
        dead_code,
        reason = "parts of the shared driver core (DST seams, observation surfaces) are reachable only through the mock backend, so a mock-less build sees them as dead"
    )
)]

mod alignment;
mod backend;
mod completion;
mod driver;
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

#[cfg(feature = "bench")]
pub mod bench;

pub use alignment::{Alignment, Unaligned};
pub use completion::{Completion, CompletionBatch};
pub use driver::{
    Backend, Driver, DriverBuilder, FileHandle, FileId, IoMode, OpKind, OpToken, OpenHow,
    ReadFrameIdx, SyncMode,
};
pub use error::{IoError, SubmitError};
pub use pool::{
    FrameGuard, GRANULE_DEFAULT, Get, PageId, PendingToken, Pool, PoolBuilder, PoolConfigError,
    ReaderCtx, ReadyResult, RegisterError, WriteArena, WriteSlot,
};

/// Internal pool building blocks the T006 tests reach through the crate root.
/// They are not part of the documented pool surface; the composed pool entry
/// points (T007/T008) subsume them.
#[doc(hidden)]
pub use pool::{Clock, FrameState, Frames, PageTable, PoolBackend, ReaderCounters};
