//! T009 loom seam (`cfg(loom)`, doc-hidden): a bounded control plane over the REAL
//! lock-free pool machinery. Every op delegates to the production primitives —
//! `ReaderSlot::begin_pin` (with its `SeqCst` fence), `commit_pin`/`abort_pin`/
//! `release_guard`, `epoch::advance_epoch`, `FrameState::advance`, the `PageTable`
//! seqlock (`insert_shared`/`remove_shared`/`lookup`), and `Clock::reference` —
//! all routed through the `cfg(loom)` sync alias so loom explores their real
//! interleavings. A bespoke reimplementation would prove nothing.
//!
//! Frame convention the proofs rely on: `make_resident` installs in frame 0,
//! `remap` in frame 1, so a coupled (frame, generation) pair exposes a torn
//! seqlock read directly. Generation is the frame's content byte, published before
//! the seqlock write and read back after, so the seqlock's Release/Acquire pairing
//! is what excludes the torn coupling.

use std::sync::atomic as held_frame_atomic;

use crate::driver::FileId;
use crate::pool::ReadFrameIdx;
use crate::product::WaitState;
use crate::sync::{Arc, AtomicU32, AtomicU64, Mutex, MutexGuard, Ordering};

use super::epoch::{
    EvictQueue, FrameGuard as PoolFrameGuard, FrameOutcome, ReaderSlot, advance_epoch,
};
use super::retention::{RetainRefused, RetainedFrame, Retention};
use super::{
    Clock, FrameState, Frames, PageId, PageTable, PoolFile, PoolFileState, ResidentFileLease,
    ResidentHint, ResidentLeaseError, ResidentLeaseState, SECTOR_BYTES,
    acquire_resident_file_lease, begin_file_retirement, file_generation_is_live, file_is_live,
    pin_with_resident_hint, publish_live_file,
};

struct Control {
    evict_queue: EvictQueue,
    release_cursor: u64,
    files: Box<[Option<PoolFile>]>,
}

/// The transition that ran first in one bounded reclaim pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainSource {
    /// A zero-count HELD frame from the release ring.
    Release,
    /// A matured frame from the epoch queue.
    Matured,
}

/// Observable outcomes from the scoped drain-driver stand-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainReport {
    /// Frames freed from the release ring.
    pub released: u32,
    /// Matured queue entries processed, including entries that became HELD.
    pub matured: u32,
    /// Matured entries that reached Free.
    pub matured_freed: u32,
    /// The first transition performed by the pass.
    pub first: Option<DrainSource>,
}

/// One shared control plane, `N` frames, and two reader slots — the bounded
/// entry the Loom models drive.
pub struct PoolModel {
    frames: Frames,
    table: PageTable,
    clock: Clock,
    global_epoch: AtomicU64,
    slots: [ReaderSlot; 2],
    // Model scaffolding no loom proof reads: it bypasses `crate::sync` (aliasing it
    // would add loom state the proofs never use) through the diagnostics-only
    // `held_frame_atomic` allowlist entry (ARCH-3).
    held_frames: [held_frame_atomic::AtomicU32; 2],
    locked_get_checks: AtomicU32,
    file_live_generations: Box<[AtomicU64]>,
    resident_lease_states: Box<[std::sync::Arc<ResidentLeaseState>]>,
    retention: Retention,
    retention_enabled: bool,
    control: Mutex<Control>,
}

impl PoolModel {
    #[must_use]
    pub fn new(frames: u32) -> Arc<Self> {
        Self::with_retention(frames, 0)
    }

    /// Builds the bounded model with the production retention primitives enabled.
    #[must_use]
    pub fn with_retention(frames: u32, max_retained_frames: u32) -> Arc<Self> {
        assert!(frames > 0, "the bounded model has at least one frame");
        assert!(
            max_retained_frames <= frames,
            "the modeled budget does not exceed its frame arena"
        );
        let wake = std::sync::Arc::new(WaitState::default());
        Arc::new(Self {
            frames: Frames::preallocated(frames, SECTOR_BYTES),
            table: PageTable::with_frame_count(frames),
            clock: Clock::with_frame_count(frames),
            global_epoch: AtomicU64::new(0),
            slots: [ReaderSlot::vacant(), ReaderSlot::vacant()],
            held_frames: [
                held_frame_atomic::AtomicU32::new(0),
                held_frame_atomic::AtomicU32::new(0),
            ],
            locked_get_checks: AtomicU32::new(0),
            file_live_generations: Box::new([AtomicU64::new(0)]),
            resident_lease_states: Box::new([std::sync::Arc::new(
                ResidentLeaseState::preallocated(0, wake.clone()),
            )]),
            retention: Retention::preallocated(frames, max_retained_frames, 2, wake),
            retention_enabled: max_retained_frames > 0,
            control: Mutex::new(Control {
                evict_queue: EvictQueue::with_capacity(frames),
                release_cursor: 0,
                files: (0..crate::driver::MAX_FILES).map(|_| None).collect(),
            }),
        })
    }

    fn control(&self) -> MutexGuard<'_, Control> {
        self.control.lock().expect("loom mutex is never poisoned")
    }

    fn page_id(page: u32) -> PageId {
        Self::file_page_id(0, page)
    }

    fn file_page_id(file_generation: u32, page: u32) -> PageId {
        PageId::new(FileId::new(0, 0, file_generation), page)
    }

    /// Makes `page` resident in `frame` filled with content-generation
    /// `generation`, mapped through the seqlock — the shared install path.
    fn install(&self, frame: ReadFrameIdx, page: PageId, generation: u8) {
        self.frames.advance(frame, FrameState::InFlight);
        self.frames.fill_inflight(frame, generation);
        self.frames.write_exact_page(frame, page);
        self.frames.advance(frame, FrameState::Resident);
        self.table.insert_shared(page, frame);
        let _ = self.clock.reference(frame);
        debug_assert!(
            self.frames.state(frame) == FrameState::Resident,
            "install ends with the frame Resident"
        );
    }

    /// Setup, single-threaded before threads spawn: `page` resident in frame 0.
    pub fn make_resident(&self, page: u32, generation: u8) {
        self.make_resident_in_frame(0, page, generation);
    }

    /// Setup, single-threaded before threads spawn: installs one exact frame/page pair.
    pub fn make_resident_in_frame(&self, frame: u32, page: u32, generation: u8) {
        assert!(frame < self.frames.count(), "setup frame is in range");
        let _control = self.control();
        self.install(ReadFrameIdx::new(frame), Self::page_id(page), generation);
    }

    /// Publishes retirement through the production retention flag.
    pub fn begin_model_file_retirement(&self) {
        self.retention.mark_file_retiring(0);
    }

    /// Reader: publishes the local epoch (real `begin_pin` + `SeqCst` fence) THEN
    /// validates the exact mapping; `Some` is a live guard, `None` observed the
    /// mapping gone on the first pin and never derefs.
    ///
    /// A nested pin (this reader already holds a guard) re-pins the frame the outer
    /// guard proves live rather than re-validating the page. Production `Pool::pin`
    /// re-validates through the table and would re-MISS here — an eviction
    /// interleaved between the outer and inner pin unmaps the page. The held-frame
    /// shortcut exists solely to force the `guard_count == 2` state the nested-drop
    /// proof needs: dropping the inner guard must not republish quiescent while the
    /// outer holds the frame (the last-drop property of `release_guard`).
    pub fn pin(&self, page: u32) -> Option<Guard<'_>> {
        self.pin_reader(0, page)
    }

    pub fn pin_reader(&self, reader: u32, page: u32) -> Option<Guard<'_>> {
        self.pin_page(reader, Self::page_id(page))
    }

    fn pin_page(&self, reader: u32, page: PageId) -> Option<Guard<'_>> {
        let reader = reader as usize;
        assert!(reader < self.slots.len(), "reader index is in range");
        let slot = &self.slots[reader];
        let first = slot.begin_pin(self.global_epoch.load(Ordering::Acquire));
        let frame = if first {
            let mapped = self.table.lookup(page);
            let Some(frame) = mapped else {
                slot.abort_pin();
                return None;
            };
            self.held_frames[reader].store(frame.get(), Ordering::Relaxed);
            frame
        } else {
            ReadFrameIdx::new(self.held_frames[reader].load(Ordering::Relaxed))
        };
        debug_assert!(
            frame.get() < self.frames.count(),
            "a pinned frame — resolved or the held frame a nested pin reuses — is in range"
        );
        let _ = self.clock.reference(frame);
        slot.commit_pin();
        Some(Guard {
            inner: PoolFrameGuard::new(
                self.frames.frame_bytes(frame),
                slot,
                frame,
                0,
                &self.retention,
            ),
        })
    }

    /// Setup for the file-generation liveness model: installs one production
    /// live-file entry and makes its exact page resident in frame zero.
    pub fn make_file_resident(&self, file_generation: u32, page: u32, content_generation: u8) {
        let mut control = self.control();
        let id = FileId::new(0, 0, file_generation);
        publish_live_file(
            &mut control.files,
            &self.file_live_generations[0],
            &self.resident_lease_states[0],
            id,
            None,
        );
        self.install(
            ReadFrameIdx::new(0),
            PageId::new(id, page),
            content_generation,
        );
    }

    /// File-aware get through the generation-exact admission mirror, with an
    /// authoritative control-locked recheck after a page miss.
    pub fn get_file(&self, file_generation: u32, page: u32) -> Option<Guard<'_>> {
        let page = Self::file_page_id(file_generation, page);
        if !file_generation_is_live(&self.file_live_generations[0], page.file()) {
            return None;
        }
        if let Some(guard) = self.pin_page(0, page) {
            return Some(guard);
        }
        let control = self.control();
        self.locked_get_checks.fetch_add(1, Ordering::Relaxed);
        if !file_is_live(&control.files, page.file(), 0) {
            return None;
        }
        drop(control);
        self.pin_page(0, page)
    }

    #[must_use]
    pub fn resident_hint(&self, file_generation: u32, page: u32) -> Option<ResidentHint> {
        let page = Self::file_page_id(file_generation, page);
        let frame = self.table.lookup(page)?;
        let stamp = self.frames.state_word(frame);
        if !Frames::word_is_resident(stamp) {
            return None;
        }
        Some(ResidentHint {
            granule: page.granule_idx(),
            frame: frame.get(),
            stamp: std::num::NonZeroU64::new(stamp)
                .expect("a Resident packed state word is nonzero"),
        })
    }

    pub fn get_with_hint(
        &self,
        file_generation: u32,
        page: u32,
        hint: Option<ResidentHint>,
    ) -> Option<Guard<'_>> {
        let page = Self::file_page_id(file_generation, page);
        if !file_generation_is_live(&self.file_live_generations[0], page.file()) {
            return None;
        }
        let Some(hint) = hint else {
            return self.get_file(file_generation, page.granule_idx());
        };
        let Some(frame) = pin_with_resident_hint(
            &self.frames,
            &self.clock,
            &self.global_epoch,
            &self.slots[0],
            page,
            hint,
        ) else {
            return self.get_file(file_generation, page.granule_idx());
        };
        Some(Guard {
            inner: PoolFrameGuard::new(
                self.frames.frame_bytes(frame),
                &self.slots[0],
                frame,
                0,
                &self.retention,
            ),
        })
    }

    /// Attempts to acquire the production resident-file lease type for the
    /// modeled file generation.
    ///
    /// # Errors
    ///
    /// Returns the production typed refusal when the exact generation is not
    /// live or its fixed lease count is exhausted.
    pub fn lease_file(
        &self,
        file_generation: u32,
    ) -> Result<ResidentFileLease, ResidentLeaseError> {
        let file = FileId::new(0, 0, file_generation);
        let control = self.control();
        if !file_is_live(&control.files, file, 0) {
            return Err(ResidentLeaseError::StaleFile { file });
        }
        acquire_resident_file_lease(&self.resident_lease_states[0], file)
    }

    /// Returns the production lease count for the modeled file slot.
    #[must_use]
    pub fn resident_lease_count(&self) -> u32 {
        self.resident_lease_states[0].count()
    }

    /// Starts retirement of the exact production file entry and evicts its page.
    ///
    /// # Panics
    ///
    /// Panics unless setup installed the named live generation and resident page.
    pub fn retire_file(&self, file_generation: u32, page: u32) {
        let mut control = self.control();
        let page = Self::file_page_id(file_generation, page);
        let file = control.files[0]
            .as_mut()
            .expect("the modeled file is registered");
        assert!(
            begin_file_retirement(file, &self.file_live_generations[0], page.file()),
            "the modeled live file begins retirement"
        );
        self.retire_file_frame(&mut control, page);
    }

    /// Runs one bounded reclaim pass and reopens the reused file slot only when
    /// its frame reaches Free through retention-aware reclaim.
    ///
    /// # Panics
    ///
    /// Panics if the bounded one-frame model attempts to reopen more than once.
    pub fn poll_reopen(
        &self,
        new_file_generation: u32,
        page: u32,
        content_generation: u8,
    ) -> DrainReport {
        let mut control = self.control();
        let retiring = control.files[0]
            .as_ref()
            .filter(|file| file.state == PoolFileState::Retiring)
            .map(|file| PageId::new(file.id, page));
        if let Some(retiring_page) = retiring {
            self.retire_file_frame(&mut control, retiring_page);
        }

        let id = FileId::new(0, 0, new_file_generation);
        let mut reopened = 0u32;
        let Control {
            evict_queue,
            release_cursor,
            files,
        } = &mut *control;
        let report = self.advance_and_reclaim(evict_queue, release_cursor, |frame| {
            self.reopen_frame(files, frame, id, page, content_generation);
            reopened += 1;
        });
        assert!(
            reopened <= 1,
            "the one-frame file model reopens at most once"
        );
        assert!(
            report.matured_freed <= 1,
            "at most one matured frame reaches Free"
        );
        report
    }

    fn reopen_frame(
        &self,
        files: &mut [Option<PoolFile>],
        frame: ReadFrameIdx,
        id: FileId,
        page: u32,
        content_generation: u8,
    ) {
        self.frames.advance(frame, FrameState::Free);
        self.install(frame, PageId::new(id, page), content_generation);
        files[0]
            .as_mut()
            .expect("the modeled retiring file remains registered")
            .state = PoolFileState::Retired;
        publish_live_file(
            files,
            &self.file_live_generations[0],
            &self.resident_lease_states[0],
            id,
            None,
        );
    }

    fn retire_file_frame(&self, control: &mut Control, page: PageId) {
        if self.resident_lease_states[0].count() > 0 {
            return;
        }
        let Some(frame) = self.table.remove_shared(page) else {
            return;
        };
        self.frames.advance(frame, FrameState::Evicting);
        control
            .evict_queue
            .push(frame, self.global_epoch.load(Ordering::Acquire));
    }

    /// Number of authoritative control-locked checks performed by `get_file`.
    #[must_use]
    pub fn locked_get_checks(&self) -> u32 {
        self.locked_get_checks.load(Ordering::Relaxed)
    }

    /// Snapshot observation of whether this model's reader slot is quiescent.
    ///
    /// # Panics
    ///
    /// Panics if the bounded model exhausts every non-quiescent epoch value.
    #[must_use]
    pub fn reader_is_quiescent(&self) -> bool {
        let next_epoch = self
            .global_epoch
            .load(Ordering::Acquire)
            .checked_add(1)
            .expect("the bounded Loom epoch remains below the quiescent sentinel");
        self.slots
            .iter()
            .all(|slot| slot.permits_advance(next_epoch))
    }

    /// Poller: take `page` Resident → Evicting, unmap it, tag the eviction with the
    /// current global epoch.
    ///
    /// # Panics
    ///
    /// Panics if `page` is not mapped in the page table.
    pub fn evict(&self, page: u32) {
        self.evict_file(0, page);
    }

    /// Poller: take one exact `(file generation, page)` Resident mapping to
    /// Evicting, unmap it, and tag the eviction with the current global epoch.
    ///
    /// # Panics
    ///
    /// Panics if the exact page is not mapped in the page table.
    pub fn evict_file(&self, file_generation: u32, page: u32) {
        self.evict_page(Self::file_page_id(file_generation, page));
    }

    fn evict_page(&self, page: PageId) {
        let mut control = self.control();
        let frame = self
            .table
            .remove_shared(page)
            .expect("evict targets a mapped page");
        debug_assert!(
            frame.get() < self.frames.count(),
            "an evicted frame index is within the frame arena"
        );
        self.frames.advance(frame, FrameState::Evicting);
        debug_assert!(
            self.frames.state(frame) == FrameState::Evicting,
            "evict leaves the frame Evicting"
        );
        control
            .evict_queue
            .push(frame, self.global_epoch.load(Ordering::Acquire));
    }

    /// Poller under the control lock: advance the epoch iff every reader permits,
    /// reclaim two-advance-expired frames, and refill each freed frame by mapping
    /// `refill_page` resident with content-generation `refill_gen`.
    pub fn poll_pass(&self, refill_page: u32, refill_gen: u8) -> DrainReport {
        self.poll_file_pass(0, refill_page, refill_gen)
    }

    /// Poller under the control lock: advances the epoch and refills each matured
    /// frame with the exact `(file generation, page)` identity and content
    /// generation supplied by the bounded model.
    pub fn poll_file_pass(
        &self,
        file_generation: u32,
        refill_page: u32,
        refill_gen: u8,
    ) -> DrainReport {
        let mut control = self.control();
        let Control {
            evict_queue,
            release_cursor,
            ..
        } = &mut *control;
        let report = self.advance_and_reclaim(evict_queue, release_cursor, |frame| {
            self.frames.advance(frame, FrameState::Free);
            self.install(
                frame,
                Self::file_page_id(file_generation, refill_page),
                refill_gen,
            );
        });
        if (report.released > 0 || report.matured_freed > 0)
            && control.files[0]
                .as_ref()
                .is_some_and(|file| file.id.generation() != file_generation)
        {
            control.files[0]
                .as_mut()
                .expect("the modeled file remains registered")
                .state = PoolFileState::Retired;
            let id = FileId::new(0, 0, file_generation);
            publish_live_file(
                &mut control.files,
                &self.file_live_generations[0],
                &self.resident_lease_states[0],
                id,
                None,
            );
        }
        debug_assert!(
            report.matured_freed <= self.frames.count(),
            "a poll pass reclaims at most every frame"
        );
        report
    }

    /// Advances the epoch without consuming either reclaim queue.
    pub fn advance_epoch_only(&self) -> u64 {
        let _control = self.control();
        advance_epoch(&self.global_epoch, &self.slots)
    }

    /// Consumes matured epoch entries without invoking the drain-driver stand-in.
    #[must_use]
    pub fn drain_matured_only(&self) -> DrainReport {
        let mut control = self.control();
        let global_epoch = advance_epoch(&self.global_epoch, &self.slots);
        self.drain_matured_entries(&mut control.evict_queue, global_epoch, |frame| {
            self.frames.advance(frame, FrameState::Free);
        })
    }

    /// Runs the scoped advance-and-reclaim stand-in and reports transition order.
    #[must_use]
    pub fn drain_driver(&self) -> DrainReport {
        let mut control = self.control();
        let Control {
            evict_queue,
            release_cursor,
            ..
        } = &mut *control;
        self.advance_and_reclaim(evict_queue, release_cursor, |frame| {
            self.frames.advance(frame, FrameState::Free);
        })
    }

    fn advance_and_reclaim<F>(
        &self,
        evict_queue: &mut EvictQueue,
        release_cursor: &mut u64,
        mut on_free: F,
    ) -> DrainReport
    where
        F: FnMut(ReadFrameIdx),
    {
        let released =
            if self.retention_enabled && self.retention.release_drain_needed(*release_cursor) {
                let pass_start_epoch = self.global_epoch.load(Ordering::Acquire);
                self.drain_release_entries(release_cursor, pass_start_epoch, &mut on_free)
            } else {
                0
            };
        let global_epoch = advance_epoch(&self.global_epoch, &self.slots);
        if self.retention.occupied_budget.load(Ordering::Acquire) == 0 {
            let mut first = None;
            let mut matured = 0u32;
            let matured_freed = evict_queue.drain_matured(global_epoch, |frame, _tag| {
                if first.is_none() {
                    first = Some(DrainSource::Matured);
                }
                matured = matured
                    .checked_add(1)
                    .expect("a pass processes at most the bounded frame count");
                on_free(frame);
                FrameOutcome::Freed
            });
            return DrainReport {
                released,
                matured,
                matured_freed: u32::try_from(matured_freed)
                    .expect("the bounded frame count fits u32"),
                first: if released > 0 {
                    Some(DrainSource::Release)
                } else {
                    first
                },
            };
        }
        let mut report = self.drain_matured_entries(evict_queue, global_epoch, on_free);
        if released > 0 {
            report.first = Some(DrainSource::Release);
        }
        report.released = released;
        report
    }

    fn drain_matured_entries<F>(
        &self,
        evict_queue: &mut EvictQueue,
        global_epoch: u64,
        mut on_free: F,
    ) -> DrainReport
    where
        F: FnMut(ReadFrameIdx),
    {
        let mut first = None;
        let mut matured = 0u32;
        let matured_freed = evict_queue.drain_matured(global_epoch, |frame, tag| {
            if first.is_none() {
                first = Some(DrainSource::Matured);
            }
            matured = matured
                .checked_add(1)
                .expect("a pass processes at most the bounded frame count");
            let outcome = self.retention.matured_outcome(frame, tag);
            if matches!(outcome, FrameOutcome::Freed) {
                on_free(frame);
            }
            outcome
        });
        DrainReport {
            released: 0,
            matured,
            matured_freed: u32::try_from(matured_freed).expect("the bounded frame count fits u32"),
            first,
        }
    }

    /// Drains only the production release ring from the model's single-consumer cursor.
    pub fn drain_releases_only(&self) -> u32 {
        let mut control = self.control();
        let pass_start_epoch = self.global_epoch.load(Ordering::Acquire);
        let Control { release_cursor, .. } = &mut *control;
        self.drain_release_entries(release_cursor, pass_start_epoch, |frame| {
            self.frames.advance(frame, FrameState::Free);
        })
    }

    fn drain_release_entries<F>(
        &self,
        release_cursor: &mut u64,
        pass_start_epoch: u64,
        mut on_free: F,
    ) -> u32
    where
        F: FnMut(ReadFrameIdx),
    {
        if !self.retention_enabled {
            return 0;
        }
        let mut released = 0u32;
        self.retention
            .drain_releases(release_cursor, pass_start_epoch, |frame| {
                on_free(frame);
                released = released
                    .checked_add(1)
                    .expect("a release pass frees at most the bounded frame count");
            });
        released
    }

    /// Observes whether a frame has reached the direct-free terminal state.
    #[must_use]
    pub fn frame_is_free(&self, frame: u32) -> bool {
        assert!(frame < self.frames.count(), "observed frame is in range");
        self.frames.state(ReadFrameIdx::new(frame)) == FrameState::Free
    }

    /// Observes whether a matured retained frame remains physically held.
    #[must_use]
    pub fn frame_is_evicting(&self, frame: u32) -> bool {
        assert!(frame < self.frames.count(), "observed frame is in range");
        self.frames.state(ReadFrameIdx::new(frame)) == FrameState::Evicting
    }

    /// Writer under the control lock: remap `page` to a fresh frame (frame 1)
    /// carrying `generation` as one seqlock transaction.
    pub fn remap(&self, page: u32, generation: u8) {
        let _control = self.control();
        self.install(ReadFrameIdx::new(1), Self::page_id(page), generation);
    }

    /// Reader: a lock-free seqlock read of `page`'s cell coupled with the frame's
    /// content generation. `None` = unmapped.
    pub fn probe(&self, page: u32) -> Option<Snapshot> {
        let frame = self.table.lookup(Self::page_id(page))?;
        let generation = self.frames.frame_bytes(frame)[0];
        Some(Snapshot {
            frame: frame.get(),
            generation,
        })
    }
}

/// A live epoch pin over a resident frame; the reader goes quiescent when its last
/// guard drops (nested guards share the published epoch via the real per-thread
/// count).
pub struct Guard<'pool> {
    inner: PoolFrameGuard<'pool>,
}

impl<'pool> Guard<'pool> {
    /// Re-reads the LIVE frame content, not a pin-time copy.
    #[must_use]
    pub fn generation(&self) -> u8 {
        self.inner[0]
    }

    /// Promotes through the production retention word while the epoch guard is live.
    pub fn into_retained(self) -> Result<RetainedFrame<'pool>, RetainRefused<'pool>> {
        self.inner.into_retained()
    }
}

/// One committed seqlock read of a `page → (frame, generation)` cell.
pub struct Snapshot {
    frame: u32,
    generation: u8,
}

impl Snapshot {
    #[must_use]
    pub fn frame(&self) -> u32 {
        self.frame
    }

    #[must_use]
    pub fn generation(&self) -> u8 {
        self.generation
    }
}
