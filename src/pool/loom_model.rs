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

use crate::driver::{FileId, ReadFrameIdx};
use crate::sync::{Arc, AtomicU64, Mutex, MutexGuard, Ordering};

use super::epoch::{advance_epoch, EvictQueue, ReaderSlot};
use super::{Clock, FrameState, Frames, PageId, PageTable, SECTOR_BYTES};

struct Control {
    evict_queue: EvictQueue,
}

/// One shared control plane, `N` frames, and a single reader slot — the bounded
/// entry the T009 loom models drive.
pub struct PoolModel {
    frames: Frames,
    table: PageTable,
    clock: Clock,
    global_epoch: AtomicU64,
    slot: ReaderSlot,
    // Model scaffolding no loom proof reads: it bypasses `crate::sync` (aliasing it
    // would add loom state the proofs never use) and is fully qualified so the
    // sync-alias regression guard allowlists it by name (ARCH-3).
    held_frame: std::sync::atomic::AtomicU32,
    control: Mutex<Control>,
}

impl PoolModel {
    #[must_use]
    pub fn new(frames: u32) -> Arc<Self> {
        Arc::new(Self {
            frames: Frames::preallocated(frames, SECTOR_BYTES),
            table: PageTable::with_frame_count(frames),
            clock: Clock::with_frame_count(frames),
            global_epoch: AtomicU64::new(0),
            slot: ReaderSlot::vacant(),
            held_frame: std::sync::atomic::AtomicU32::new(0),
            control: Mutex::new(Control {
                evict_queue: EvictQueue::with_capacity(frames),
            }),
        })
    }

    fn control(&self) -> MutexGuard<'_, Control> {
        self.control.lock().expect("loom mutex is never poisoned")
    }

    fn page_id(page: u32) -> PageId {
        PageId::new(FileId::new(0, 0, 0), page)
    }

    /// Makes `page` resident in `frame` filled with content-generation
    /// `generation`, mapped through the seqlock — the shared install path.
    fn install(&self, frame: ReadFrameIdx, page: u32, generation: u8) {
        self.frames.advance(frame, FrameState::InFlight);
        self.frames.fill_inflight(frame, generation);
        self.frames.advance(frame, FrameState::Resident);
        self.table.insert_shared(Self::page_id(page), frame);
        let _ = self.clock.reference(frame);
        debug_assert!(
            self.frames.state(frame) == FrameState::Resident,
            "install ends with the frame Resident"
        );
    }

    /// Setup, single-threaded before threads spawn: `page` resident in frame 0.
    pub fn make_resident(&self, page: u32, generation: u8) {
        let _control = self.control();
        self.install(ReadFrameIdx::new(0), page, generation);
    }

    /// Reader: publishes the local epoch (real `begin_pin` + `SeqCst` fence) THEN
    /// validates residency; `Some` is a live guard, `None` observed the mapping gone
    /// on the first pin and never derefs.
    ///
    /// A nested pin (this reader already holds a guard) re-pins the frame the outer
    /// guard proves live rather than re-validating the page. Production `Pool::pin`
    /// re-validates through the table and would re-MISS here — an eviction
    /// interleaved between the outer and inner pin unmaps the page. The held-frame
    /// shortcut exists solely to force the `guard_count == 2` state the nested-drop
    /// proof needs: dropping the inner guard must not republish quiescent while the
    /// outer holds the frame (the last-drop property of `release_guard`).
    pub fn pin(&self, page: u32) -> Option<Guard<'_>> {
        let first = self
            .slot
            .begin_pin(self.global_epoch.load(Ordering::Acquire));
        let frame = if first {
            let resident = self
                .table
                .lookup(Self::page_id(page))
                .filter(|&frame| self.frames.state(frame) == FrameState::Resident);
            let Some(frame) = resident else {
                self.slot.abort_pin();
                return None;
            };
            self.held_frame.store(frame.get(), Ordering::Relaxed);
            frame
        } else {
            ReadFrameIdx::new(self.held_frame.load(Ordering::Relaxed))
        };
        debug_assert!(
            frame.get() < self.frames.count(),
            "a pinned frame — resolved or the held frame a nested pin reuses — is in range"
        );
        let _ = self.clock.reference(frame);
        self.slot.commit_pin();
        Some(Guard {
            slot: &self.slot,
            frames: &self.frames,
            frame,
        })
    }

    /// Poller: take `page` Resident → Evicting, unmap it, tag the eviction with the
    /// current global epoch.
    ///
    /// # Panics
    ///
    /// Panics if `page` is not mapped in the page table.
    pub fn evict(&self, page: u32) {
        let mut control = self.control();
        let frame = self
            .table
            .remove_shared(Self::page_id(page))
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
    pub fn poll_pass(&self, refill_page: u32, refill_gen: u8) {
        let mut control = self.control();
        let global_epoch = advance_epoch(&self.global_epoch, std::slice::from_ref(&self.slot));
        let reclaimed = control.evict_queue.drain_matured(global_epoch, |frame| {
            self.frames.advance(frame, FrameState::Free);
            self.install(frame, refill_page, refill_gen);
        });
        debug_assert!(
            reclaimed <= self.frames.count() as usize,
            "a poll pass reclaims at most every frame"
        );
    }

    /// Writer under the control lock: remap `page` to a fresh frame (frame 1)
    /// carrying `generation` as one seqlock transaction.
    pub fn remap(&self, page: u32, generation: u8) {
        let _control = self.control();
        self.install(ReadFrameIdx::new(1), page, generation);
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
    slot: &'pool ReaderSlot,
    frames: &'pool Frames,
    frame: ReadFrameIdx,
}

impl Guard<'_> {
    /// Re-reads the LIVE frame content, not a pin-time copy.
    #[must_use]
    pub fn generation(&self) -> u8 {
        self.frames.frame_bytes(self.frame)[0]
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.slot.release_guard();
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
