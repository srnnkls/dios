//! The pool miss path (T008): the backend seam the pool composes over and the
//! per-`PageId` singleflight table.
//!
//! [`PoolBackend`] is the read-submit + drain seam. Both the production
//! [`Driver`](crate::Driver) and the deterministic mock driver
//! satisfy it inherently — the pool owns the driver it composes and never selects
//! a backend by matching a runtime tag (AD-1). The read target unifies with the
//! pool's frames at construction: a completed read fills the pool frame directly
//! rather than a private slab.
//!
//! [`MissTable`] coalesces every `get` for one missing page onto a single
//! in-flight read (singleflight). A completion resolves the page `Resident`, a
//! short read reslices the remainder, and an IO error or short-read-at-EOF fans
//! the failure to every waiter and frees the frame.

use crate::completion::CompletionBatch;
use crate::driver::{BackendProgress, FileHandle, FileId, OpToken, RegistrationPosture, SyncMode};
use crate::error::FileRegistrationError;
use crate::error::SubmitError;
use crate::open::DirectIo;
use crate::pool::write_arena::{ArenaState, WriteSlot};
use crate::pool::{PageId, ReadFrameIdx};
use crate::product::{LifecycleCounters, WaitState};
use crate::sync::{AtomicU32, AtomicU64, Ordering};
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub(super) mod sealed {
    pub(crate) trait Sealed {}
}

/// The read-submit + drain seam the pool composes over. Sealed to the crate's own
/// driver types; carried `#[doc(hidden)]` so it is not documented public API.
pub(crate) trait PoolBackend: sealed::Sealed {
    fn identity(&self) -> u64;

    /// The buffer-registration posture the backend runs; mocks have none.
    fn registration_posture(&self) -> RegistrationPosture {
        RegistrationPosture::Unregistered
    }

    /// Whether the backend holds both arenas locked in memory.
    fn arena_locked(&self) -> bool {
        false
    }

    fn attach_pool_state(&self, lifecycle: Arc<LifecycleCounters>, wake: Arc<WaitState>);

    /// Opens and retains a data file according to `direct_io`.
    fn open(&self, path: &Path, direct_io: DirectIo) -> Result<FileHandle, FileRegistrationError>;

    fn create(&self, path: &Path, direct_io: DirectIo)
    -> Result<FileHandle, FileRegistrationError>;

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

    /// Drains ready completions and separately reports raw backend progress.
    fn poll_progress(&self, out: &mut CompletionBatch) -> BackendProgress;

    /// Parks through the backend's real wait source, outside pool control, then
    /// drains ready completions into `out`.
    fn poll_wait_progress(&self, out: &mut CompletionBatch, timeout: Duration) -> BackendProgress;

    fn write_arena_state(&self) -> &ArenaState;

    fn submit_write<'arena>(
        &self,
        fd: &FileHandle,
        slot: WriteSlot<'arena>,
        offset: u64,
    ) -> Result<OpToken, (SubmitError, WriteSlot<'arena>)>;

    fn submit_fsync(&self, fd: &FileHandle, mode: SyncMode) -> Result<OpToken, SubmitError>;

    fn close(&self, fd: FileHandle);

    fn is_closed(&self, file: FileId) -> bool;
}

/// One fixed miss-record index carried by every waiter capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MissSlot(u32);

impl MissSlot {
    pub(crate) fn new(index: usize) -> Self {
        Self(u32::try_from(index).expect("the fixed miss table indexes by u32"))
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

/// Permanent generation and waiter-interest atomics for one fixed miss slot.
#[derive(Debug)]
struct MissInterest {
    generation: AtomicU64,
    waiters: AtomicU32,
}

/// Pool-owned waiter accounting. Its address is the structural pool identity
/// carried by a [`PendingToken`](super::PendingToken).
#[derive(Debug)]
pub(crate) struct MissInterests {
    slots: Box<[MissInterest]>,
}

impl MissInterests {
    #[cfg(test)]
    pub(crate) fn with_capacity(capacity: u32) -> Self {
        Self::try_with_capacity(capacity)
            .unwrap_or_else(|| panic!("miss-interest allocation failed for {capacity} slots"))
    }

    pub(crate) fn try_with_capacity(capacity: u32) -> Option<Self> {
        Some(Self {
            slots: crate::allocation::try_boxed_slice_with(capacity, || MissInterest {
                generation: AtomicU64::new(0),
                waiters: AtomicU32::new(0),
            })?,
        })
    }

    fn begin(&self, slot: MissSlot) -> NonZeroU64 {
        let interest = &self.slots[slot.index()];
        assert_eq!(
            interest.waiters.load(Ordering::Acquire),
            0,
            "a miss slot is never recycled with live waiter interest"
        );
        let previous = interest.generation.load(Ordering::Acquire);
        let generation = previous
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .expect("miss-slot generation exhausted before ABA");
        interest
            .generation
            .store(generation.get(), Ordering::Release);
        interest.waiters.store(1, Ordering::Release);
        generation
    }

    fn join(&self, slot: MissSlot, generation: NonZeroU64) {
        let interest = &self.slots[slot.index()];
        assert_eq!(
            interest.generation.load(Ordering::Acquire),
            generation.get(),
            "singleflight joins only the current miss generation"
        );
        interest
            .waiters
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |waiters| {
                waiters.checked_add(1)
            })
            .expect("miss waiter count exhausted its fixed u32 bound");
    }

    pub(crate) fn release(&self, slot: MissSlot, generation: NonZeroU64) -> u32 {
        let interest = &self.slots[slot.index()];
        assert_eq!(
            interest.generation.load(Ordering::Acquire),
            generation.get(),
            "a live capability's miss slot cannot be recycled"
        );
        let previous = interest
            .waiters
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |waiters| {
                waiters.checked_sub(1)
            })
            .expect("a pending capability consumes interest once");
        previous - 1
    }

    pub(crate) fn waiters(&self, slot: MissSlot, generation: NonZeroU64) -> Option<u32> {
        let interest = &self.slots[slot.index()];
        if interest.generation.load(Ordering::Acquire) != generation.get() {
            return None;
        }
        let waiters = interest.waiters.load(Ordering::Acquire);
        (interest.generation.load(Ordering::Acquire) == generation.get()).then_some(waiters)
    }
}

/// A submitted miss's current or terminal disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MissOutcome {
    /// A read is in flight (the original granule read or a resubmitted remainder).
    Pending,
    /// The read failed — an IO error or a short-read-at-EOF — carrying its errno.
    /// The frame is already freed; the errno fans out to every waiter.
    Failed(i32),
    /// The exact frame is resident and remains non-evictable while this
    /// generation has waiter interest.
    Succeeded,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MissEntry {
    page: PageId,
    frame: ReadFrameIdx,
    token: OpToken,
    generation: NonZeroU64,
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

    pub(crate) fn generation(&self) -> NonZeroU64 {
        self.generation
    }
}

/// The pending subset of the miss table, kept beside the frame-sized slots so a
/// cold `get` or a completion resolves its miss in the in-flight bound rather
/// than in the frame count.
#[derive(Debug, Clone, Copy)]
struct PendingMiss {
    page: PageId,
    token: OpToken,
    slot: MissSlot,
}

/// Fixed-capacity singleflight registry: one pending entry per missing `PageId`,
/// plus retained terminal generations with live waiter interest. It is sized to
/// the frame count and never grows after construction. The pending index is
/// sized to the read in-flight bound: every pending miss holds one read credit.
#[derive(Debug)]
pub(crate) struct MissTable {
    slots: Box<[Option<MissEntry>]>,
    success_frames: Box<[Option<(MissSlot, NonZeroU64)>]>,
    pending: Box<[Option<PendingMiss>]>,
    pending_len: u32,
}

impl MissTable {
    #[cfg(test)]
    pub(crate) fn with_capacity(capacity: u32) -> Self {
        Self::try_with_capacity(capacity, capacity)
            .unwrap_or_else(|| panic!("miss-table allocation failed for {capacity} slots"))
    }

    pub(crate) fn try_with_capacity(capacity: u32, pending_capacity: u32) -> Option<Self> {
        assert!(
            pending_capacity <= capacity,
            "pending misses occupy miss slots, so their bound fits the slot count"
        );
        Some(Self {
            slots: crate::allocation::try_boxed_slice_with(capacity, || None)?,
            success_frames: crate::allocation::try_boxed_slice_with(capacity, || None)?,
            pending: crate::allocation::try_boxed_slice_with(pending_capacity, || None)?,
            pending_len: 0,
        })
    }

    fn pending_live(&self) -> &[Option<PendingMiss>] {
        &self.pending[..self.pending_len as usize]
    }

    pub(crate) fn find_pending(&self, page: PageId) -> Option<usize> {
        let found = self
            .pending_live()
            .iter()
            .find_map(|pending| pending.filter(|pending| pending.page == page))
            .map(|pending| pending.slot.index());
        debug_assert_eq!(
            found,
            self.slots.iter().position(|slot| {
                slot.is_some_and(|entry| {
                    entry.page == page && entry.outcome == MissOutcome::Pending
                })
            }),
            "the pending index mirrors the pending slots"
        );
        found
    }

    pub(crate) fn find_by_token(&self, token: OpToken) -> Option<usize> {
        self.pending_live()
            .iter()
            .find_map(|pending| pending.filter(|pending| pending.token == token))
            .map(|pending| pending.slot.index())
    }

    fn pending_position(&self, slot: MissSlot) -> usize {
        self.pending_live()
            .iter()
            .position(|pending| pending.is_some_and(|pending| pending.slot == slot))
            .expect("a pending miss is indexed")
    }

    fn pending_push(&mut self, pending: PendingMiss) {
        let index = self.pending_len as usize;
        assert!(
            index < self.pending.len(),
            "pending misses stay within the read in-flight bound"
        );
        assert!(self.pending[index].is_none(), "the pending tail is free");
        self.pending[index] = Some(pending);
        self.pending_len += 1;
    }

    fn pending_remove(&mut self, slot: MissSlot) {
        let index = self.pending_position(slot);
        let last = self.pending_len as usize - 1;
        let moved = self.pending[last].take();
        if index < last {
            self.pending[index] = moved;
        }
        self.pending_len -= 1;
        assert!(
            self.pending[last].is_none(),
            "removal frees the pending tail"
        );
    }

    pub(crate) fn entry(&self, index: usize) -> MissEntry {
        self.slots[index].expect("an occupied miss slot")
    }

    /// Finds an empty or zero-interest terminal slot without mutating it. The
    /// caller holds the pool control lock through submit and installation.
    pub(crate) fn admission_slot(&self, interests: &MissInterests) -> Option<MissSlot> {
        self.slots.iter().enumerate().find_map(|(index, entry)| {
            let reusable = entry.is_none_or(|entry| {
                entry.outcome != MissOutcome::Pending
                    && interests.waiters(MissSlot::new(index), entry.generation) == Some(0)
            });
            reusable.then(|| MissSlot::new(index))
        })
    }

    pub(crate) fn admit(
        &mut self,
        slot: MissSlot,
        page: PageId,
        frame: ReadFrameIdx,
        token: OpToken,
        interests: &MissInterests,
    ) -> NonZeroU64 {
        debug_assert!(
            !self.slots.is_empty(),
            "the miss table is sized to the frames"
        );
        debug_assert!(
            self.find_pending(page).is_none(),
            "admit installs a fresh miss — a pending duplicate would break singleflight"
        );
        if let Some(previous) = self.slots[slot.index()] {
            self.clean_terminal_zero(slot, previous.generation, interests);
        }
        assert!(
            self.slots[slot.index()].is_none(),
            "admission reuses only an empty or cleaned terminal slot"
        );
        let generation = interests.begin(slot);
        let entry = MissEntry {
            page,
            frame,
            token,
            generation,
            filled: 0,
            outcome: MissOutcome::Pending,
        };
        self.slots[slot.index()] = Some(entry);
        self.pending_push(PendingMiss { page, token, slot });
        generation
    }

    pub(crate) fn join(&self, index: usize, interests: &MissInterests) -> (MissSlot, NonZeroU64) {
        let entry = self.entry(index);
        assert_eq!(
            entry.outcome,
            MissOutcome::Pending,
            "only pending misses join"
        );
        let slot = MissSlot::new(index);
        interests.join(slot, entry.generation);
        (slot, entry.generation)
    }

    pub(crate) fn advance_remainder(&mut self, index: usize, filled: u32, token: OpToken) {
        let entry = self.slots[index].as_mut().expect("an occupied miss slot");
        assert_eq!(
            entry.outcome,
            MissOutcome::Pending,
            "only pending misses reslice"
        );
        entry.filled = filled;
        entry.token = token;
        let position = self.pending_position(MissSlot::new(index));
        let pending = self.pending[position]
            .as_mut()
            .expect("a pending position names an indexed miss");
        pending.token = token;
    }

    pub(crate) fn fail(&mut self, index: usize, errno: i32) {
        let entry = self.slots[index].as_mut().expect("an occupied miss slot");
        assert_ne!(errno, 0, "a terminal miss failure carries a real errno");
        assert_eq!(entry.outcome, MissOutcome::Pending, "a miss fails once");
        entry.outcome = MissOutcome::Failed(errno);
        self.pending_remove(MissSlot::new(index));
    }

    pub(crate) fn succeed(&mut self, index: usize) {
        let entry = self.slots[index].as_mut().expect("an occupied miss slot");
        assert_eq!(entry.outcome, MissOutcome::Pending, "a miss succeeds once");
        entry.outcome = MissOutcome::Succeeded;
        let entry = *entry;
        self.pending_remove(MissSlot::new(index));
        let frame_index = entry.frame.get() as usize;
        assert!(
            self.success_frames[frame_index].is_none(),
            "one successful terminal generation protects a frame"
        );
        self.success_frames[frame_index] = Some((MissSlot::new(index), entry.generation));
    }

    pub(crate) fn validate(
        &self,
        slot: MissSlot,
        generation: NonZeroU64,
        page: PageId,
    ) -> MissEntry {
        let entry = self.slots[slot.index()].expect("pending token names a live miss record");
        assert_eq!(
            entry.generation, generation,
            "pending token generation is exact"
        );
        assert_eq!(entry.page, page, "pending token page is exact");
        entry
    }

    pub(crate) fn clean_terminal_zero(
        &mut self,
        slot: MissSlot,
        generation: NonZeroU64,
        interests: &MissInterests,
    ) {
        let entry = self.slots[slot.index()];
        if entry.is_some_and(|entry| {
            entry.generation == generation
                && entry.outcome != MissOutcome::Pending
                && interests.waiters(slot, generation) == Some(0)
        }) {
            let entry = entry.expect("the terminal-zero predicate observed an entry");
            if entry.outcome == MissOutcome::Succeeded {
                let frame_index = entry.frame.get() as usize;
                assert_eq!(
                    self.success_frames[frame_index],
                    Some((slot, generation)),
                    "successful protection names its exact terminal generation"
                );
                self.success_frames[frame_index] = None;
            }
            self.slots[slot.index()] = None;
        }
    }

    pub(crate) fn prepare_eviction(
        &mut self,
        frame: ReadFrameIdx,
        interests: &MissInterests,
    ) -> bool {
        let Some((slot, generation)) = self.success_frames[frame.get() as usize] else {
            return true;
        };
        if interests.waiters(slot, generation) != Some(0) {
            return false;
        }
        self.clean_terminal_zero(slot, generation, interests);
        assert!(
            self.success_frames[frame.get() as usize].is_none(),
            "zero-interest success protection clears before frame reuse"
        );
        true
    }

    pub(crate) fn has_live_for_file(&self, file: FileId, interests: &MissInterests) -> bool {
        self.slots.iter().enumerate().any(|(index, entry)| {
            entry.is_some_and(|entry| {
                entry.page.file() == file
                    && (entry.outcome == MissOutcome::Pending
                        || interests.waiters(MissSlot::new(index), entry.generation) != Some(0))
            })
        })
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::{MissInterests, MissSlot, MissTable};
    use crate::driver::{FileId, OpToken};
    use crate::pool::{PageId, ReadFrameIdx};

    #[test]
    fn pending_lookups_follow_admit_reslice_and_terminal_transitions() {
        let interests = MissInterests::with_capacity(100_000);
        let mut miss = MissTable::try_with_capacity(100_000, 2).expect("allocation");
        let file = FileId::new(1, 0, 1);
        let first_page = PageId::new(file, 7);
        let second_page = PageId::new(file, 9);
        let first_slot = MissSlot::new(60_000);
        let second_slot = MissSlot::new(70_000);
        assert_eq!(miss.find_pending(first_page), None);

        miss.admit(
            first_slot,
            first_page,
            ReadFrameIdx::new(0),
            OpToken::new(0, 1),
            &interests,
        );
        miss.admit(
            second_slot,
            second_page,
            ReadFrameIdx::new(1),
            OpToken::new(1, 1),
            &interests,
        );
        assert_eq!(miss.find_pending(first_page), Some(first_slot.index()));
        assert_eq!(miss.find_pending(second_page), Some(second_slot.index()));
        assert_eq!(
            miss.find_by_token(OpToken::new(1, 1)),
            Some(second_slot.index())
        );

        miss.advance_remainder(first_slot.index(), 512, OpToken::new(2, 1));
        assert_eq!(miss.find_by_token(OpToken::new(0, 1)), None);
        assert_eq!(
            miss.find_by_token(OpToken::new(2, 1)),
            Some(first_slot.index())
        );

        miss.succeed(first_slot.index());
        assert_eq!(miss.find_pending(first_page), None);
        assert_eq!(miss.find_by_token(OpToken::new(2, 1)), None);
        assert_eq!(miss.find_pending(second_page), Some(second_slot.index()));

        miss.fail(second_slot.index(), 5);
        assert_eq!(miss.find_pending(second_page), None);
        assert_eq!(miss.find_by_token(OpToken::new(1, 1)), None);
        assert_eq!(miss.pending_len, 0);
    }

    #[test]
    #[should_panic(expected = "pending misses stay within the read in-flight bound")]
    fn admitting_past_the_in_flight_bound_is_a_programmer_error() {
        let interests = MissInterests::with_capacity(4);
        let mut miss = MissTable::try_with_capacity(4, 1).expect("allocation");
        let file = FileId::new(1, 0, 1);
        miss.admit(
            MissSlot::new(0),
            PageId::new(file, 0),
            ReadFrameIdx::new(0),
            OpToken::new(0, 1),
            &interests,
        );
        miss.admit(
            MissSlot::new(1),
            PageId::new(file, 1),
            ReadFrameIdx::new(1),
            OpToken::new(1, 1),
            &interests,
        );
    }

    #[test]
    fn zero_interest_success_mapping_clears_before_another_slot_reuses_the_frame() {
        let interests = MissInterests::with_capacity(2);
        let mut miss = MissTable::with_capacity(2);
        let old_slot = MissSlot::new(1);
        let old_generation = miss.admit(
            old_slot,
            PageId::new(FileId::new(1, 0, 1), 0),
            ReadFrameIdx::new(0),
            OpToken::new(0, 1),
            &interests,
        );
        miss.succeed(old_slot.index());
        assert_eq!(interests.release(old_slot, old_generation), 0);

        assert!(miss.prepare_eviction(ReadFrameIdx::new(0), &interests));

        let reused_slot = MissSlot::new(0);
        miss.admit(
            reused_slot,
            PageId::new(FileId::new(1, 0, 1), 1),
            ReadFrameIdx::new(0),
            OpToken::new(1, 1),
            &interests,
        );
        miss.succeed(reused_slot.index());
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use super::{MissInterests, MissSlot};
    use loom::sync::Arc;
    use loom::thread;

    #[test]
    fn concurrent_exact_generation_drops_reach_zero_without_underflow() {
        loom::model(|| {
            let interests = Arc::new(MissInterests::with_capacity(1));
            let slot = MissSlot::new(0);
            let generation = interests.begin(slot);
            interests.join(slot, generation);

            let first_interests = Arc::clone(&interests);
            let first = thread::spawn(move || first_interests.release(slot, generation));
            let second_interests = Arc::clone(&interests);
            let second = thread::spawn(move || second_interests.release(slot, generation));

            let remaining_sum = first.join().expect("first waiter drop")
                + second.join().expect("second waiter drop");
            assert_eq!(
                remaining_sum, 1,
                "the two exact drops observe one then zero"
            );
            assert_eq!(
                interests.waiters(slot, generation),
                Some(0),
                "the exact generation has no waiter interest after both drops"
            );
        });
    }
}
