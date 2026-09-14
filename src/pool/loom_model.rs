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

use std::num::NonZeroU64;

use crate::driver::read_vector::{ReadVector, VECTOR_FRAMES_MAX, VectorStorage};
use crate::driver::{CompletionSlab, FileId, OpToken};
use crate::pool::ReadFrameIdx;
use crate::product::WaitState;
use crate::sync::{Arc, AtomicU32, AtomicU64, Mutex, MutexGuard, Ordering};

use super::epoch::{
    EvictQueue, FrameGuard as PoolFrameGuard, FrameOutcome, ReaderRegistry, advance_epoch,
};
use super::miss::{MissEntry, MissInterests, MissOutcome, MissSlot, MissTable};
use super::prefetch::state::{Prefetch, Terminal};
use super::prefetch::{PrefetchStats, Readahead, Source};
use super::read_spans::{ReadSpan, ReadSpans};
use super::retention::{RetainRefused, RetainedFrame, Retention};
use super::{
    Clock, FrameState, Frames, InFlightFrame, PageId, PageTable, PoolFile, PoolFileState,
    ResidentFileLease, ResidentHint, ResidentLeaseError, ResidentLeaseState, SECTOR_BYTES,
    acquire_resident_file_lease, begin_file_retirement, file_generation_is_live, file_is_live,
    pin_with_resident_hint, publish_live_file,
};

struct Control {
    evict_queue: EvictQueue,
    release_cursor: u64,
    files: Box<[Option<PoolFile>]>,
    readahead: Option<ReadaheadControl>,
}

struct ReadaheadControl {
    miss: MissTable,
    spans: ReadSpans,
    prefetch: Prefetch,
    operations: CompletionSlab<SpanRead>,
    descriptors: VectorStorage,
    reads_in_flight: u32,
}

impl ReadaheadControl {
    fn new(frames: u32, capacity: u32) -> Self {
        assert!((1..=frames).contains(&capacity));
        Self {
            miss: MissTable::try_with_capacity(frames, frames, 1).expect("loom miss table"),
            spans: ReadSpans::try_new(frames, 1).expect("loom span routes"),
            prefetch: Prefetch::try_with_geometry(
                capacity,
                2,
                frames,
                Readahead::Automatic,
                frames,
                SECTOR_BYTES,
            )
            .expect("loom prefetch ledger"),
            operations: CompletionSlab::try_with_capacity(1).expect("loom operation slab"),
            descriptors: VectorStorage::try_new(1).expect("loom iovec storage"),
            reads_in_flight: 0,
        }
    }
}

/// Source bytes stand in for the disk; observations always borrow the arena.
struct SpanRead {
    vector: ReadVector,
    contents: [u8; VECTOR_FRAMES_MAX as usize],
    pages: u32,
}

impl SpanRead {
    fn complete(&mut self, bytes: u32) {
        assert!(bytes > 0, "this model injects positive byte-count CQEs");
        let offset = self.pages * SECTOR_BYTES - self.vector.remaining();
        self.vector
            .transfer_prefix(bytes, |transferred, destination| {
                let ordinal = (offset + transferred) / SECTOR_BYTES;
                destination.fill(self.contents[ordinal as usize]);
            });
        self.vector.record_completion(bytes);
    }
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
    frames: std::sync::Arc<Frames>,
    table: PageTable,
    clock: Clock,
    global_epoch: AtomicU64,
    readers: std::sync::Arc<ReaderRegistry>,
    // Model scaffolding no loom proof reads; modelling it would add
    // interleavings the proofs never use.
    held_frames: [crate::pool::diagnostics::DiagnosticSlot; 2],
    locked_get_checks: AtomicU32,
    file_live_generations: Box<[AtomicU64]>,
    resident_lease_states: Box<[std::sync::Arc<ResidentLeaseState>]>,
    retention: Retention,
    retention_enabled: bool,
    miss_interests: Option<std::sync::Arc<MissInterests>>,
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
        Self::with_geometry(frames, max_retained_frames, None)
    }

    /// Enables actual span ownership and speculative bookkeeping in this model.
    ///
    /// # Panics
    ///
    /// Panics unless the speculative capacity is positive and fits the arena.
    #[must_use]
    pub fn with_readahead(frames: u32, capacity: u32) -> Arc<Self> {
        assert!((1..=frames).contains(&capacity));
        Self::with_geometry(frames, 0, Some(capacity))
    }

    fn with_geometry(
        frames: u32,
        max_retained_frames: u32,
        readahead_capacity: Option<u32>,
    ) -> Arc<Self> {
        assert!(frames > 0, "the bounded model has at least one frame");
        assert!(
            max_retained_frames <= frames,
            "the modeled budget does not exceed its frame arena"
        );
        let wake = std::sync::Arc::new(WaitState::default());
        let registered_file_capacity = 1;
        Arc::new(Self {
            frames: std::sync::Arc::new(Frames::preallocated(frames, SECTOR_BYTES)),
            table: PageTable::with_frame_count(frames),
            clock: Clock::with_frame_count(frames),
            global_epoch: AtomicU64::new(0),
            readers: std::sync::Arc::new(
                ReaderRegistry::try_with_capacity(
                    2,
                    2,
                    std::sync::Arc::new(crate::product::LifecycleCounters::default()),
                )
                .expect("loom reader registry"),
            ),
            held_frames: [
                crate::pool::diagnostics::DiagnosticSlot::new(),
                crate::pool::diagnostics::DiagnosticSlot::new(),
            ],
            locked_get_checks: AtomicU32::new(0),
            file_live_generations: crate::allocation::try_boxed_slice_with(
                registered_file_capacity,
                || AtomicU64::new(0),
            )
            .expect("loom file-generation allocation succeeds"),
            resident_lease_states: crate::allocation::try_boxed_slice_with(
                registered_file_capacity,
                || std::sync::Arc::new(ResidentLeaseState::preallocated(0, wake.clone())),
            )
            .expect("loom lease-state allocation succeeds"),
            retention: Retention::try_preallocated_with_file_capacity(
                frames,
                max_retained_frames,
                2,
                registered_file_capacity,
                wake,
            )
            .expect("loom retention allocation succeeds"),
            retention_enabled: max_retained_frames > 0,
            miss_interests: readahead_capacity.map(|_| {
                std::sync::Arc::new(
                    MissInterests::try_with_capacity(frames).expect("loom miss interests"),
                )
            }),
            control: Mutex::new(Control {
                evict_queue: EvictQueue::with_capacity(frames),
                release_cursor: 0,
                files: crate::allocation::try_boxed_slice_with(registered_file_capacity, || None)
                    .expect("loom file-table allocation succeeds"),
                readahead: readahead_capacity
                    .map(|capacity| ReadaheadControl::new(frames, capacity)),
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

    fn miss_interests(&self) -> &MissInterests {
        self.miss_interests
            .as_ref()
            .expect("readahead model enabled")
    }

    /// Admits one bounded vector through the real route, miss and operation slabs.
    ///
    /// # Panics
    ///
    /// Panics unless readahead is enabled and all two-to-32 source pages are
    /// absent, fit the page namespace, and have available credits and storage.
    pub fn admit_span(&self, first: u32, contents: &[u8]) -> OpToken {
        let count = u32::try_from(contents.len()).expect("bounded source pages");
        assert!((2..=VECTOR_FRAMES_MAX).contains(&count));
        let mut control = self.control();
        let readahead = control.readahead.as_mut().expect("readahead model enabled");
        readahead.prefetch.reconcile(&self.clock);
        let stats = readahead.prefetch.model_stats();
        assert!(count <= stats.capacity - stats.occupied);
        assert!(count <= self.frames.count() - readahead.reads_in_flight);
        let route = readahead.spans.reserve().expect("free span route");
        let mut slots = [None; VECTOR_FRAMES_MAX as usize];
        assert!(
            readahead
                .miss
                .admission_slots(self.miss_interests(), &mut slots[..contents.len()],)
        );
        let (mut read, frames) = self.admit_span_claim(readahead, first, contents);
        let operation = readahead
            .operations
            .reserve()
            .expect("one free driver slot");
        readahead.descriptors.prepare(operation, &mut read.vector);
        let token = readahead.operations.fill(operation, read);
        let mut span = ReadSpan::new(Self::page_id(first), token, count, SECTOR_BYTES);
        for (ordinal, slot) in slots[..contents.len()].iter().enumerate() {
            let page = Self::page_id(first + u32::try_from(ordinal).expect("bounded ordinal"));
            let frame = frames[ordinal].expect("claimed frame");
            let slot = slot.expect("reserved miss slot");
            let generation =
                readahead
                    .miss
                    .admit_speculative(slot, page, frame, token, self.miss_interests());
            span.install(ordinal, slot, generation);
            readahead
                .prefetch
                .admit(&self.clock, page, frame, Source::Explicit);
        }
        readahead.reads_in_flight += count;
        readahead.spans.commit(route, span);
        token
    }

    fn admit_span_claim(
        &self,
        readahead: &ReadaheadControl,
        first: u32,
        contents: &[u8],
    ) -> (SpanRead, [Option<ReadFrameIdx>; VECTOR_FRAMES_MAX as usize]) {
        let mut frames = [None; VECTOR_FRAMES_MAX as usize];
        let mut count = 0;
        for index in 0..self.frames.count() {
            let frame = ReadFrameIdx::new(index);
            if self.frames.state(frame) == FrameState::Free {
                frames[count] = Some(frame);
                count += 1;
                if count == contents.len() {
                    break;
                }
            }
        }
        assert_eq!(
            count,
            contents.len(),
            "all destinations are reserved before claim"
        );
        let mut vector = ReadVector::new(&self.frames);
        for (ordinal, frame) in frames[..count].iter().enumerate() {
            let page = Self::page_id(
                first
                    .checked_add(u32::try_from(ordinal).expect("bounded ordinal"))
                    .expect("modeled page range fits u32"),
            );
            assert!(self.table.lookup(page).is_none());
            assert!(readahead.miss.find_pending(page).is_none());
            vector.push(
                self.frames
                    .claim(frame.expect("free destination"), page)
                    .expect("the control lock preserves exclusive claim"),
            );
        }
        let mut source = [0; VECTOR_FRAMES_MAX as usize];
        source[..count].copy_from_slice(contents);
        (
            SpanRead {
                vector,
                contents: source,
                pages: u32::try_from(count).expect("bounded pages"),
            },
            frames,
        )
    }

    /// Transfers the CQE's bytes and publishes only its newly complete prefix.
    ///
    /// # Panics
    ///
    /// Panics unless `token` names the current retained span and `bytes` is a
    /// positive count within its remaining destinations.
    pub fn complete_span(&self, token: OpToken, bytes: u32) {
        let mut control = self.control();
        let ReadaheadControl {
            miss,
            spans,
            operations,
            descriptors,
            reads_in_flight,
            ..
        } = control.readahead.as_mut().expect("readahead model enabled");
        let (route, mut span) = spans.completing(token).expect("current span route");
        let read = operations.peek_mut(token.slot());
        read.complete(bytes);
        span.publish_completed(
            &mut read.vector,
            bytes,
            SECTOR_BYTES,
            |span, ordinal, write| {
                let (index, entry) = span.entry(miss, ordinal, &write);
                self.complete_span_publish(miss, reads_in_flight, index, entry, write);
            },
        );
        if span.is_complete() {
            assert_eq!(read.vector.remaining(), 0);
            spans.finish(route, token);
            let (completed, read) = operations.reclaim(token.slot());
            assert_eq!(completed, token, "the retained driver generation is exact");
            drop(read);
        } else {
            descriptors.prepare(token.slot(), &mut read.vector);
            spans.resume(route, span);
        }
    }

    fn complete_span_publish(
        &self,
        miss: &mut MissTable,
        reads_in_flight: &mut u32,
        index: usize,
        entry: MissEntry,
        write: InFlightFrame,
    ) {
        *reads_in_flight = reads_in_flight
            .checked_sub(1)
            .expect("one terminal read credit");
        assert_eq!(self.frames.publish(write), entry.frame());
        self.table.insert_shared(entry.page(), entry.frame());
        if self.clock.is_speculative(entry.frame()) {
            self.clock.completed.notify(entry.frame());
        } else {
            let _ = self.clock.reference(entry.frame());
        }
        miss.succeed(index);
        miss.clean_terminal_zero(
            MissSlot::new(index),
            entry.generation(),
            self.miss_interests(),
        );
    }

    /// Retains an exact pending miss interest or an already resident page identity.
    ///
    /// # Panics
    ///
    /// Panics unless readahead is enabled and the page is pending or resident.
    pub fn join_span_page(&self, page: u32) -> JoinedSpan {
        let page = Self::page_id(page);
        let mut control = self.control();
        let readahead = control.readahead.as_mut().expect("readahead model enabled");
        let interest = if self.table.lookup(page).is_some() {
            None
        } else {
            let index = readahead
                .miss
                .find_pending(page)
                .expect("one admitted page miss");
            let interest = readahead.miss.join(index, self.miss_interests());
            readahead.prefetch.finish(
                &self.clock,
                readahead.miss.entry(index).frame(),
                Terminal::Promoted(Some(0)),
            );
            Some(interest)
        };
        JoinedSpan {
            interests: std::sync::Arc::clone(
                self.miss_interests
                    .as_ref()
                    .expect("readahead model enabled"),
            ),
            page,
            interest,
        }
    }

    /// Reads the current whole frame under the existing EBR pin protocol.
    ///
    /// # Panics
    ///
    /// Panics if the join belongs to another model or its retained identity changed.
    #[must_use]
    pub fn joined_span_bytes(&self, joined: &JoinedSpan) -> Option<[u8; SECTOR_BYTES as usize]> {
        assert!(std::sync::Arc::ptr_eq(
            self.miss_interests
                .as_ref()
                .expect("readahead model enabled"),
            &joined.interests,
        ));
        if let Some((slot, generation)) = joined.interest {
            let control = self.control();
            let readahead = control.readahead.as_ref().expect("readahead model enabled");
            let entry = readahead.miss.validate(slot, generation, joined.page);
            if entry.outcome() != MissOutcome::Succeeded {
                return None;
            }
        }
        let guard = self.pin_readahead_page(0, joined.page)?;
        let bytes: &[u8; SECTOR_BYTES as usize] = (&*guard)
            .try_into()
            .expect("the model returns a whole granule");
        Some(*bytes)
    }

    /// Observes the actual driver's slot component, including after route reuse.
    ///
    /// # Panics
    ///
    /// Panics if the token names a slot outside the one-slot model.
    #[must_use]
    pub fn span_operation_slot(&self, token: OpToken) -> u32 {
        assert_eq!(token.slot(), 0, "one bounded operation slot");
        token.slot()
    }

    /// Counts admitted page destinations that have not reached their terminal outcome.
    ///
    /// # Panics
    ///
    /// Panics unless readahead is enabled.
    #[must_use]
    pub fn read_credits_used(&self) -> u32 {
        self.control()
            .readahead
            .as_ref()
            .expect("readahead model enabled")
            .reads_in_flight
    }

    /// Supplies an ordinary demand observation to the production predictor.
    ///
    /// # Panics
    ///
    /// Panics unless readahead is enabled.
    pub fn observe_readahead(&self, reader: u32, page: u32) {
        self.control()
            .readahead
            .as_mut()
            .expect("readahead model enabled")
            .prefetch
            .observe(&self.clock, reader, Self::page_id(page));
    }

    /// Installs one automatic prediction, reclaiming any previous identity through EBR.
    ///
    /// # Panics
    ///
    /// Panics unless the frame is in range, has no live guard or miss interest,
    /// and the next automatic prediction has available speculative capacity.
    pub fn make_speculative_resident(&self, reader: u32, frame: u32, page: u32, content: u8) {
        let frame = ReadFrameIdx::new(frame);
        assert!(frame.get() < self.frames.count());
        let mut control = self.control();
        if self.frames.state(frame) == FrameState::Resident {
            self.make_speculative_resident_reclaim(&mut control, frame);
        }
        assert_eq!(self.frames.state(frame), FrameState::Free);
        let page = Self::page_id(page);
        assert!(self.table.lookup(page).is_none());
        let mut write = self
            .frames
            .claim(frame, page)
            .expect("free modeled destination");
        self.frames.fill(&mut write, content);
        control
            .readahead
            .as_mut()
            .expect("readahead model enabled")
            .prefetch
            .admit_model_automatic(&self.clock, reader, page, frame);
        self.frames.publish(write);
        self.table.insert_shared(page, frame);
        self.clock.completed.notify(frame);
    }

    fn make_speculative_resident_reclaim(&self, control: &mut Control, frame: ReadFrameIdx) {
        let slot = &self.readers.slots()[1];
        let begun = slot.begin_pin(self.global_epoch.load(Ordering::Acquire));
        assert_eq!(self.frames.state(frame), FrameState::Resident);
        let pin = slot.commit_pin(begun);
        let guard = PoolFrameGuard::new(
            self.frames.frame_bytes(frame, &pin),
            slot,
            frame,
            0,
            &self.retention,
        );
        let page = self.frames.exact_page_guarded(&guard);
        drop(guard);
        let readahead = control.readahead.as_mut().expect("readahead model enabled");
        assert!(
            readahead
                .miss
                .prepare_eviction(frame, self.miss_interests())
        );
        assert_eq!(self.table.remove_shared(page), Some(frame));
        readahead
            .prefetch
            .finish(&self.clock, frame, Terminal::Evicted);
        self.frames.advance(frame, FrameState::Evicting);
        control
            .evict_queue
            .push(frame, self.global_epoch.load(Ordering::Acquire));
        for _ in 0..2 {
            self.advance_and_reclaim(
                &mut control.evict_queue,
                &mut control.release_cursor,
                |frame| {
                    self.frames.advance(frame, FrameState::Free);
                },
            );
        }
        assert_eq!(
            self.frames.state(frame),
            FrameState::Free,
            "reuse follows the real grace period"
        );
    }

    /// Consumes real resident bytes through the first speculative-reference CAS.
    ///
    /// # Panics
    ///
    /// Panics unless `reader` is in range and the prediction is resident.
    #[must_use]
    pub fn consume_readahead(&self, reader: u32, page: u32) -> [u8; SECTOR_BYTES as usize] {
        let guard = self
            .pin_readahead_page(reader, Self::page_id(page))
            .expect("resident prediction");
        let bytes: &[u8; SECTOR_BYTES as usize] = (&*guard)
            .try_into()
            .expect("the model returns a whole granule");
        *bytes
    }

    fn pin_readahead_page(&self, reader: u32, page: PageId) -> Option<PoolFrameGuard<'_>> {
        let slot = &self.readers.slots()[reader as usize];
        let begun = slot.begin_pin(self.global_epoch.load(Ordering::Acquire));
        let Some(frame) = self.table.lookup(page) else {
            slot.abort_pin(begun);
            return None;
        };
        let _ = self.clock.reference_from(frame, slot);
        let pin = slot.commit_pin(begun);
        let guard = PoolFrameGuard::new(
            self.frames.frame_bytes(frame, &pin),
            slot,
            frame,
            0,
            &self.retention,
        );
        assert_eq!(self.frames.exact_page_guarded(&guard), page);
        Some(guard)
    }

    /// Runs production reconciliation and reports visits from its actual control events.
    ///
    /// # Panics
    ///
    /// Panics unless readahead is enabled.
    pub fn reconcile_readahead(&self) -> u64 {
        self.control()
            .readahead
            .as_mut()
            .expect("readahead model enabled")
            .prefetch
            .reconcile_model(&self.clock)
    }

    /// Snapshots the production speculative ledger.
    ///
    /// # Panics
    ///
    /// Panics unless readahead is enabled.
    #[must_use]
    pub fn readahead_stats(&self) -> PrefetchStats {
        let control = self.control();
        let readahead = control.readahead.as_ref().expect("readahead model enabled");
        PrefetchStats {
            reads_in_flight: readahead.reads_in_flight,
            ..readahead.prefetch.model_stats()
        }
    }

    /// Observes the production predictor's next eligible page without changing it.
    ///
    /// # Panics
    ///
    /// Panics unless readahead is enabled and `reader` is in range.
    #[must_use]
    pub fn next_readahead_page(&self, reader: u32) -> Option<u32> {
        self.control()
            .readahead
            .as_ref()
            .expect("readahead model enabled")
            .prefetch
            .model_next_page(reader)
            .map(PageId::granule_idx)
    }

    /// Makes `page` resident in `frame` filled with content-generation
    /// `generation`, mapped through the seqlock — the shared install path.
    fn install(&self, frame: ReadFrameIdx, page: PageId, generation: u8) {
        let mut token = self
            .frames
            .claim(frame, page)
            .expect("the model installs into a Free frame");
        self.frames.fill(&mut token, generation);
        self.frames.publish(token);
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
    ///
    /// # Panics
    ///
    /// Panics unless the frame is in range and free.
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
        assert!(
            reader < self.readers.slots().len(),
            "reader index is in range"
        );
        let slot = &self.readers.slots()[reader];
        let begun = slot.begin_pin(self.global_epoch.load(Ordering::Acquire));
        let frame = if begun.is_first() {
            let mapped = self.table.lookup(page);
            let Some(frame) = mapped else {
                slot.abort_pin(begun);
                return None;
            };
            self.held_frames[reader].set(frame.get());
            frame
        } else {
            ReadFrameIdx::new(self.held_frames[reader].get())
        };
        debug_assert!(
            frame.get() < self.frames.count(),
            "a pinned frame — resolved or the held frame a nested pin reuses — is in range"
        );
        let _ = self.clock.reference(frame);
        let pin = slot.commit_pin(begun);
        Some(Guard {
            inner: PoolFrameGuard::new(
                self.frames.frame_bytes(frame, &pin),
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

    /// Observes a generation-exact hint for a currently resident page.
    ///
    /// # Panics
    ///
    /// Panics if a Resident state word violates its nonzero invariant.
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
            stamp: NonZeroU64::new(stamp).expect("a Resident packed state word is nonzero"),
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
        let Some((frame, pin)) = pin_with_resident_hint(
            &self.frames,
            &self.clock,
            &self.global_epoch,
            &self.readers.slots()[0],
            page,
            hint,
        ) else {
            return self.get_file(file_generation, page.granule_idx());
        };
        Some(Guard {
            inner: PoolFrameGuard::new(
                self.frames.frame_bytes(frame, &pin),
                &self.readers.slots()[0],
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
            ..
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
        self.readers
            .slots()
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
    ///
    /// # Panics
    ///
    /// Panics if a previously observed modeled file disappears before reopening.
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
        advance_epoch(&self.global_epoch, self.readers.slots())
    }

    /// Consumes matured epoch entries without invoking the drain-driver stand-in.
    #[must_use]
    pub fn drain_matured_only(&self) -> DrainReport {
        let mut control = self.control();
        let global_epoch = advance_epoch(&self.global_epoch, self.readers.slots());
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
        let (retention_enabled, released) =
            match self.retention.release_drain_needed(*release_cursor) {
                Some(true) => {
                    let pass_start_epoch = self.global_epoch.load(Ordering::Acquire);
                    let released =
                        self.drain_release_entries(release_cursor, pass_start_epoch, &mut on_free);
                    (true, released)
                }
                Some(false) => (true, 0),
                None => (false, 0),
            };
        let global_epoch = advance_epoch(&self.global_epoch, self.readers.slots());
        let retention_occupied =
            retention_enabled && self.retention.occupied_budget.load(Ordering::Acquire) != 0;
        let mut report = if retention_occupied {
            self.drain_matured_entries(evict_queue, global_epoch, on_free)
        } else {
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
            DrainReport {
                released: 0,
                matured,
                matured_freed: u32::try_from(matured_freed)
                    .expect("the bounded frame count fits u32"),
                first,
            }
        };
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
        if !self.retention_enabled {
            return 0;
        }
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
    ///
    /// # Panics
    ///
    /// Panics if `frame` is outside the arena.
    #[must_use]
    pub fn frame_is_free(&self, frame: u32) -> bool {
        assert!(frame < self.frames.count(), "observed frame is in range");
        self.frames.state(ReadFrameIdx::new(frame)) == FrameState::Free
    }

    /// Observes whether a matured retained frame remains physically held.
    ///
    /// # Panics
    ///
    /// Panics if `frame` is outside the arena.
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
    /// content generation, read under a pin on reader 0. `None` = unmapped.
    pub fn probe(&self, page: u32) -> Option<Snapshot> {
        let slot = &self.readers.slots()[0];
        let begun = slot.begin_pin(self.global_epoch.load(Ordering::Acquire));
        let Some(frame) = self.table.lookup(Self::page_id(page)) else {
            slot.abort_pin(begun);
            return None;
        };
        let pin = slot.commit_pin(begun);
        let generation = self.frames.frame_bytes(frame, &pin)[0];
        slot.release_guard();
        Some(Snapshot {
            frame: frame.get(),
            generation,
        })
    }
}

/// An owned exact miss interest or resident identity, with no saved frame contents.
pub struct JoinedSpan {
    interests: std::sync::Arc<MissInterests>,
    page: PageId,
    interest: Option<(MissSlot, NonZeroU64)>,
}

impl Drop for JoinedSpan {
    fn drop(&mut self) {
        if let Some((slot, generation)) = self.interest.take() {
            self.interests.release(slot, generation);
        }
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
    ///
    /// # Errors
    ///
    /// Returns the production retention refusal with the original guard preserved.
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
