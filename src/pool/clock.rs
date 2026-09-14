//! CLOCK second-chance eviction over a per-frame reference bit. A warm hit sets
//! the bit check-then-set (Relaxed load, Relaxed store only when clear), so a
//! repeat hit on an already-set bit performs no store — the DIO-G1 hot-path
//! invariant. Eviction sweeps a deterministic hand, clearing set bits (spending
//! their second chance) until it lands on a clear one.

use crate::pool::ReadFrameIdx;
use crate::sync::{AtomicBool, AtomicU32, AtomicU64, Ordering};

#[derive(Debug)]
pub struct Clock {
    reference_bits: crate::allocation::MappedSlice<AtomicBool>,
    count: u32,
    hand: AtomicU32,
    reference_stores: super::diagnostics::DiagnosticCounter,
    speculation: crate::allocation::MappedSlice<AtomicU64>,
    pub(super) consumed: super::prefetch::notifications::DirtyFrames,
    pub(super) completed: super::prefetch::notifications::DirtyFrames,
    #[cfg(feature = "bench")]
    visits: super::diagnostics::DiagnosticCounter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SpeculationOutcome {
    Absent,
    Unconsumed,
    Consumed(Option<u32>),
}

impl Clock {
    /// Builds a clock with `frame_count` clear reference bits and the hand at
    /// frame zero.
    ///
    /// # Panics
    ///
    /// If `frame_count` is zero.
    #[must_use]
    pub fn with_frame_count(frame_count: u32) -> Self {
        Self::try_with_frame_count(frame_count)
            .unwrap_or_else(|| panic!("clock allocation failed for {frame_count} frames"))
    }

    pub(crate) fn try_with_frame_count(frame_count: u32) -> Option<Self> {
        assert!(frame_count > 0, "frame count must be positive");
        Some(Self {
            reference_bits: crate::allocation::MappedSlice::try_vacant(frame_count)?,
            count: frame_count,
            hand: AtomicU32::new(0),
            reference_stores: super::diagnostics::DiagnosticCounter::new(),
            speculation: crate::allocation::MappedSlice::try_vacant(frame_count)?,
            consumed: super::prefetch::notifications::DirtyFrames::try_new(frame_count)?,
            completed: super::prefetch::notifications::DirtyFrames::try_new(frame_count)?,
            #[cfg(feature = "bench")]
            visits: super::diagnostics::DiagnosticCounter::new(),
        })
    }

    /// Records a touch of `frame`, returning `true` iff this call set a
    /// previously clear bit. A repeat touch on a set bit stores nothing.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range.
    #[must_use]
    #[inline]
    pub fn reference(&self, frame: ReadFrameIdx) -> bool {
        self.reference_observed(frame, || None)
    }

    #[inline]
    pub(super) fn reference_from(
        &self,
        frame: ReadFrameIdx,
        reader: &super::epoch::ReaderSlot,
    ) -> bool {
        self.reference_observed(frame, || reader.index())
    }

    #[inline]
    fn reference_observed(
        &self,
        frame: ReadFrameIdx,
        reader: impl FnOnce() -> Option<u32>,
    ) -> bool {
        let bit = &self.reference_bits[self.checked_index(frame)];
        if bit.load(Ordering::Relaxed) {
            false
        } else {
            self.consume_speculation(frame, reader);
            bit.store(true, Ordering::Relaxed);
            self.reference_stores.increment();
            true
        }
    }

    fn consume_speculation(&self, frame: ReadFrameIdx, reader: impl FnOnce() -> Option<u32>) {
        // This marker only arbitrates credit accounting. The page table and
        // EBR publish bytes and prevent frame reuse until this pin quiesces.
        let marker = &self.speculation[self.checked_index(frame)];
        if marker.load(Ordering::Relaxed) == 1 {
            let consumed = reader().map_or(u64::MAX, |reader| u64::from(reader) + 2);
            // Release orders this reader's earlier speculative observations for
            // bounded, ordered feedback reconciliation; EBR still owns bytes.
            if marker
                .compare_exchange(1, consumed, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                self.consumed.notify(frame);
            }
        }
    }

    pub(super) fn begin_speculation(&self, frame: ReadFrameIdx) {
        let index = self.checked_index(frame);
        assert_eq!(self.speculation[index].load(Ordering::Relaxed), 0);
        self.reference_bits[index].store(false, Ordering::Relaxed);
        self.speculation[index].store(1, Ordering::Relaxed);
    }

    pub(super) fn speculation_consumed(&self, frame: ReadFrameIdx) -> bool {
        self.speculation[self.checked_index(frame)].load(Ordering::Acquire) >= 2
    }

    pub(super) fn is_speculative(&self, frame: ReadFrameIdx) -> bool {
        self.speculation[self.checked_index(frame)].load(Ordering::Relaxed) != 0
    }

    pub(super) fn take_speculation(&self, frame: ReadFrameIdx) -> SpeculationOutcome {
        match self.speculation[self.checked_index(frame)].swap(0, Ordering::AcqRel) {
            0 => SpeculationOutcome::Absent,
            1 => SpeculationOutcome::Unconsumed,
            u64::MAX => SpeculationOutcome::Consumed(None),
            marker => SpeculationOutcome::Consumed(Some(
                u32::try_from(marker - 2).expect("the marker names a reader index"),
            )),
        }
    }

    /// Cumulative count of clear→set reference-bit stores; a repeat hit on a set
    /// bit leaves it unchanged (DIO-G1 store-elision observation seam).
    #[doc(hidden)]
    #[must_use]
    pub fn reference_stores(&self) -> u64 {
        self.reference_stores.get()
    }

    /// Whether `frame`'s reference bit is set.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range.
    #[must_use]
    pub fn is_referenced(&self, frame: ReadFrameIdx) -> bool {
        self.reference_bits[self.checked_index(frame)].load(Ordering::Relaxed)
    }

    /// Advances the hand until it lands on a clear bit, clearing every set bit
    /// it passes (each spending one second chance), and evicts that frame — the
    /// standalone-clock entry point.
    pub fn evict_victim(&mut self) -> ReadFrameIdx {
        self.evict_victim_shared()
    }

    /// The sweep over the shared reference bits and the atomic hand. Callers
    /// serialize it under the pool's AD-4 control-plane lock so the hand advances
    /// single-writer even though the signature is `&self` (the reference bits stay
    /// lock-free for the warm-hit path).
    pub(crate) fn evict_victim_shared(&self) -> ReadFrameIdx {
        let mut hand = self.hand.load(Ordering::Relaxed);
        for _ in 0..=self.count {
            let index = hand;
            hand = (hand + 1) % self.count;
            #[cfg(feature = "bench")]
            self.visits.increment();
            let bit = &self.reference_bits[index as usize];
            if bit.load(Ordering::Relaxed) {
                bit.store(false, Ordering::Relaxed);
            } else {
                self.hand.store(hand, Ordering::Relaxed);
                return ReadFrameIdx::new(index);
            }
        }
        self.hand.store(hand, Ordering::Relaxed);
        ReadFrameIdx::new(hand)
    }

    #[cfg(feature = "bench")]
    pub(super) fn visits(&self) -> u64 {
        self.visits.get()
    }

    #[cfg(feature = "bench")]
    pub(super) fn notification_bytes(&self) -> u64 {
        self.consumed.metadata_bytes() + self.completed.metadata_bytes()
    }

    fn checked_index(&self, frame: ReadFrameIdx) -> usize {
        let index = frame.get() as usize;
        assert!(
            index < self.reference_bits.len(),
            "frame index out of range"
        );
        index
    }
}
