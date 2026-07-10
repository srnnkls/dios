//! Frame-pool contract shapes: the residency ADT (`Get`), the readiness
//! re-check ADT (`ReadyResult`), page identity, and the borrow guards.
//!
//! These are the SCOPE-CONTRACT names T006/T007/T008 fill in behind — the real
//! frames, page table, CLOCK, epoch guards, and singleflight land there. The
//! API-fit spike (T016) pins this call surface through an in-example `StubPool`.

use std::cell::Ref;
use std::marker::PhantomData;
use std::ops::Deref;

use crate::driver::FileId;
use crate::error::IoError;

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
