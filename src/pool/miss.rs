//! The pool miss path (T008): the backend seam the pool composes over and the
//! per-`PageId` singleflight table.
//!
//! [`PoolBackend`] is the read-submit + drain seam. Both the production
//! [`Driver`](crate::Driver) and the deterministic [`MockDriver`](crate::mock::MockDriver)
//! satisfy it inherently — the pool owns the driver it composes and never selects
//! a backend by matching a runtime tag (AD-1). The read target unifies with the
//! pool's frames through [`PoolBackend::share_frames`]: a completed read fills the
//! pool frame directly rather than a private slab.
//!
//! [`MissTable`] coalesces every `get` for one missing page onto a single
//! in-flight read (singleflight). A completion resolves the page `Resident`, a
//! short read reslices the remainder, and an IO error or short-read-at-EOF fans
//! the failure to every waiter and frees the frame.

use std::path::Path;
use std::sync::Arc;

use crate::completion::CompletionBatch;
use crate::driver::{FileHandle, OpToken, ReadFrameIdx};
use crate::error::IoError;
use crate::error::SubmitError;
use crate::open::DirectIo;
use crate::pool::frames::Frames;
use crate::pool::PageId;

pub(super) mod sealed {
    #[expect(
        unnameable_types,
        reason = "the unnameable supertrait deliberately seals the test backend seam"
    )]
    pub trait Sealed {}
}

/// The read-submit + drain seam the pool composes over. Sealed to the crate's own
/// driver types; carried `#[doc(hidden)]` so it is not documented public API.
#[doc(hidden)]
pub trait PoolBackend: sealed::Sealed {
    /// Opens and retains a data file according to `direct_io`.
    fn open(&self, path: &Path, direct_io: DirectIo) -> Result<FileHandle, IoError>;

    /// Enqueues a read of `len` bytes at `file_offset` into `frame`. The pool
    /// always requests the whole granule first, then the remainder tail after a
    /// short read (reslice, scope.md:601).
    ///
    /// # Errors
    ///
    /// [`SubmitError::Full`] when the queue is saturated, [`SubmitError::StaleHandle`]
    /// for a stale fd — backpressure, never a block.
    fn submit_read(
        &self,
        fd: &FileHandle,
        frame: ReadFrameIdx,
        file_offset: u64,
        destination_offset: u32,
        len: u32,
    ) -> Result<OpToken, SubmitError>;

    /// Drains ready completions into `out`, returning the count.
    fn poll(&self, out: &mut CompletionBatch) -> usize;

    /// Hands the backend the pool's frame arena so a completed read lands in the
    /// pool frame. The default no-op fits the mock (which fills the shared frame
    /// directly) and any backend that reads into its own slab; registering the
    /// arena as the ring's fixed read buffers is the T014 unification.
    fn share_frames(&self, frames: Arc<Frames>);
}

/// A submitted miss's terminal disposition. A successful completion removes the
/// entry (the page is `Resident`, observable through the table), so success is not
/// a variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MissOutcome {
    /// A read is in flight (the original granule read or a resubmitted remainder).
    Pending,
    /// The read failed — an IO error or a short-read-at-EOF — carrying its errno.
    /// The frame is already freed; the errno fans out to every waiter.
    Failed(i32),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MissEntry {
    page: PageId,
    frame: ReadFrameIdx,
    token: OpToken,
    filled: u32,
    outcome: MissOutcome,
}

impl MissEntry {
    pub(crate) fn page(&self) -> PageId {
        self.page
    }

    pub(crate) fn frame(&self) -> ReadFrameIdx {
        self.frame
    }

    pub(crate) fn filled(&self) -> u32 {
        self.filled
    }

    pub(crate) fn outcome(&self) -> MissOutcome {
        self.outcome
    }
}

/// Fixed-capacity singleflight registry: one live entry per missing `PageId`. Sized
/// to the frame count — the most distinct pages that can hold or have just released
/// a frame — so it never grows after construction.
#[derive(Debug)]
pub(crate) struct MissTable {
    slots: Box<[Option<MissEntry>]>,
}

impl MissTable {
    pub(crate) fn with_capacity(capacity: u32) -> Self {
        Self {
            slots: (0..capacity).map(|_| None).collect(),
        }
    }

    pub(crate) fn find(&self, page: PageId) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.is_some_and(|entry| entry.page == page))
    }

    pub(crate) fn find_by_token(&self, token: OpToken) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.is_some_and(|entry| entry.token == token))
    }

    pub(crate) fn entry(&self, index: usize) -> MissEntry {
        self.slots[index].expect("an occupied miss slot")
    }

    /// Admits a fresh pending miss, preferring an empty slot and otherwise
    /// recycling a `Failed` slot (its frame is already freed). Returns `None` when
    /// every slot holds a live pending miss.
    pub(crate) fn admit(&mut self, page: PageId, frame: ReadFrameIdx, token: OpToken) -> bool {
        debug_assert!(
            !self.slots.is_empty(),
            "the miss table is sized to the frames"
        );
        debug_assert!(
            self.find(page).is_none(),
            "admit installs a fresh miss — a pending duplicate would break singleflight"
        );
        let entry = MissEntry {
            page,
            frame,
            token,
            filled: 0,
            outcome: MissOutcome::Pending,
        };
        let free = self.slots.iter().position(Option::is_none);
        let recyclable = || {
            self.slots
                .iter()
                .position(|slot| slot.is_some_and(|e| matches!(e.outcome, MissOutcome::Failed(_))))
        };
        let Some(slot) = free.or_else(recyclable) else {
            return false;
        };
        self.slots[slot] = Some(entry);
        true
    }

    pub(crate) fn advance_remainder(&mut self, index: usize, filled: u32, token: OpToken) {
        let entry = self.slots[index].as_mut().expect("an occupied miss slot");
        entry.filled = filled;
        entry.token = token;
    }

    pub(crate) fn fail(&mut self, index: usize, errno: i32) {
        let entry = self.slots[index].as_mut().expect("an occupied miss slot");
        entry.outcome = MissOutcome::Failed(errno);
    }

    pub(crate) fn resolve(&mut self, index: usize) {
        self.slots[index] = None;
    }
}
