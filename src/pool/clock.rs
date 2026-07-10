//! CLOCK second-chance eviction over a per-frame reference bit. A warm hit sets
//! the bit check-then-set (Relaxed load, Relaxed store only when clear), so a
//! repeat hit on an already-set bit performs no store — the DIO-G1 hot-path
//! invariant. Eviction sweeps a deterministic hand, clearing set bits (spending
//! their second chance) until it lands on a clear one.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::driver::ReadFrameIdx;

#[derive(Debug)]
pub struct Clock {
    reference_bits: Box<[AtomicBool]>,
    count: u32,
    hand: AtomicU32,
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
        assert!(frame_count > 0, "frame count must be positive");
        let mut bits = Vec::with_capacity(frame_count as usize);
        for _ in 0..frame_count {
            bits.push(AtomicBool::new(false));
        }
        Self {
            reference_bits: bits.into_boxed_slice(),
            count: frame_count,
            hand: AtomicU32::new(0),
        }
    }

    /// Records a touch of `frame`, returning `true` iff this call set a
    /// previously clear bit. A repeat touch on a set bit stores nothing.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range.
    #[must_use]
    pub fn reference(&self, frame: ReadFrameIdx) -> bool {
        let bit = &self.reference_bits[self.checked_index(frame)];
        if bit.load(Ordering::Relaxed) {
            false
        } else {
            bit.store(true, Ordering::Relaxed);
            true
        }
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

    fn checked_index(&self, frame: ReadFrameIdx) -> usize {
        let index = frame.get() as usize;
        assert!(
            index < self.reference_bits.len(),
            "frame index out of range"
        );
        index
    }
}
