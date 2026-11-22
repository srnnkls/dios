//! Copy-out read (FR-9 to FR-11): one optimistic attempt that copies a byte
//! range of one resident granule into caller memory under no epoch pin, and at
//! most one ordinary `get` behind it.

#[cfg(all(feature = "mock", not(loom)))]
use std::sync::Arc;

use crate::sync::AtomicU64;
#[cfg(all(feature = "mock", not(loom)))]
use crate::sync::{AtomicBool, Ordering};

#[cfg(any(loom, feature = "mock"))]
use super::ReadFrameIdx;
#[cfg(loom)]
use super::frames::MODEL_PAYLOAD_WORDS;
use super::{Frames, GetError, PageId, PendingToken, Pool, PoolBackend, ReaderCtx, Translation};

/// Outcome of [`Pool::read_copy`]; the output holds the requested bytes only
/// on `Copied`.
#[derive(Debug)]
pub enum ReadCopy {
    /// The output holds exactly the requested bytes of one publication of
    /// the page.
    Copied,
    /// The page is absent and one read is in flight; the token can be checked
    /// with [`Pool::ready`].
    Pending(PendingToken),
    /// No frame can be admitted within the bounded reclaim pass.
    Busy,
}

/// Outcome of one optimistic copy attempt.
#[cfg_attr(
    not(loom),
    expect(
        unreachable_pub,
        reason = "the outcome is public only through the Loom model seam"
    )
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptimisticCopy {
    /// The output holds the requested bytes of one publication of the page.
    #[cfg_attr(
        not(loom),
        expect(dead_code, reason = "the scaffolded attempt always falls back")
    )]
    Accepted,
    /// The attempt proved nothing; the caller takes one ordinary `get`.
    Fallback,
}

#[expect(
    private_bounds,
    reason = "the private sealed bound preserves static dispatch for the two crate-owned pool backends"
)]
impl<D: PoolBackend> Pool<D> {
    /// Copies `output.len()` bytes of `page` starting at `offset_bytes` into
    /// `output` through one optimistic attempt and at most one ordinary
    /// [`Pool::get`] behind it. `output` is valid only on
    /// [`ReadCopy::Copied`].
    ///
    /// # Errors
    ///
    /// Returns the [`GetError`] of the ordinary `get`.
    ///
    /// # Panics
    ///
    /// If `reader` or `page` belongs to another pool, `output` is empty or
    /// longer than `u32::MAX`, or the range ends past the granule.
    pub fn read_copy(
        &self,
        reader: &ReaderCtx,
        page: PageId,
        offset_bytes: u32,
        output: &mut [u8],
    ) -> Result<ReadCopy, GetError> {
        self.assert_reader_owner(reader);
        assert!(
            page.file().driver() == self.identity,
            "file identity used with a foreign pool"
        );
        copy_range_assert(self.granule, offset_bytes, output.len());
        match optimistic_copy(
            &self.table,
            &self.frames,
            &self.file_live_generations,
            page,
            offset_bytes,
            output,
        ) {
            OptimisticCopy::Accepted => Ok(ReadCopy::Copied),
            OptimisticCopy::Fallback => Ok(ReadCopy::Busy),
        }
    }

    /// Arms the forced failure of every optimistic attempt and restarts its
    /// counts.
    #[cfg(all(feature = "mock", not(loom)))]
    pub(crate) fn force_optimistic_copy_failures_internal(&self) -> Arc<OptimisticCopyFailures> {
        self.optimistic_copy_failures.arm();
        Arc::clone(&self.optimistic_copy_failures)
    }

    /// Relaxed atomic loads of every payload word of `frame` from the
    /// optimistic seam's pointer, taking no lock and bypassing the volatile
    /// loads, so Miri reports any payload write that races them without being
    /// a same-width atomic store.
    #[cfg(all(feature = "mock", not(loom)))]
    pub(crate) fn load_payload_words_atomic_internal(
        &self,
        frame: ReadFrameIdx,
        words: &mut [u64],
    ) {
        assert_eq!(
            words.len(),
            self.frames.granule() as usize / size_of::<u64>(),
            "one output word per payload word"
        );
        let first = self.frames.optimistic_payload_ptr(frame);
        for (index, word) in words.iter_mut().enumerate() {
            let pointer = first.wrapping_add(index).cast_mut();
            // SAFETY: the seam bound-checks `frame` and asserts the first word
            // aligned, and `index < granule / 8`, so the word lies inside the
            // live arena, which outlives `&self`. A payload write racing this
            // load that is not a same-width atomic store is the defect Miri
            // reports here.
            let cell = unsafe { AtomicU64::from_ptr(pointer) };
            *word = cell.load(Ordering::Relaxed);
        }
    }
}

/// The optimistic attempt of FR-10 over the parts of a pool, shared with the
/// Loom seam: it takes no epoch pin, sets no CLOCK reference and never falls
/// back itself.
pub(crate) fn optimistic_copy(
    _translation: &Translation,
    frames: &Frames,
    _file_live_generations: &[AtomicU64],
    _page: PageId,
    offset_bytes: u32,
    output: &mut [u8],
) -> OptimisticCopy {
    copy_range_assert(frames.granule(), offset_bytes, output.len());
    OptimisticCopy::Fallback
}

/// The copy step of the optimistic attempt over the Loom model's labeled
/// payload, from offset zero: each word is its own load, so a refill racing
/// the copy can tear it between words.
///
/// # Panics
///
/// If `output` is empty or longer than the labeled payload.
#[cfg(loom)]
pub(crate) fn copy_payload_words(frames: &Frames, frame: ReadFrameIdx, output: &mut [u8]) {
    assert!(!output.is_empty(), "a copy names at least one byte");
    assert!(
        output.len() <= MODEL_PAYLOAD_WORDS * size_of::<u64>(),
        "the copy lies in the labeled payload"
    );
    for (word_index, chunk) in (0..).zip(output.chunks_mut(size_of::<u64>())) {
        let word = frames.model_payload_word(frame, word_index).to_ne_bytes();
        chunk.copy_from_slice(&word[..chunk.len()]);
    }
}

fn copy_range_assert(granule: u32, offset_bytes: u32, output_len: usize) {
    assert!(output_len > 0, "a copy names at least one byte");
    let len = u32::try_from(output_len).expect("a copy's length fits u32");
    assert!(
        offset_bytes
            .checked_add(len)
            .is_some_and(|end| end <= granule),
        "the copy range [{offset_bytes}, {offset_bytes} + {len}) ends within the {granule}-byte granule"
    );
}

/// Mock test hook: while armed every optimistic attempt fails; the counts
/// run from the last arming, also after release. A hook never armed counts
/// nothing, so `get` pays one relaxed load and a branch for it.
#[cfg(all(feature = "mock", not(loom)))]
#[derive(Debug, Default)]
pub(crate) struct OptimisticCopyFailures {
    armed: AtomicBool,
    counting: AtomicBool,
    attempts: AtomicU64,
    fallback_gets: AtomicU64,
}

#[cfg(all(feature = "mock", not(loom)))]
impl OptimisticCopyFailures {
    fn arm(&self) {
        self.attempts.store(0, Ordering::Relaxed);
        self.fallback_gets.store(0, Ordering::Relaxed);
        self.counting.store(true, Ordering::Release);
        self.armed.store(true, Ordering::Release);
    }

    pub(crate) fn release(&self) {
        self.armed.store(false, Ordering::Release);
    }

    pub(crate) fn attempts(&self) -> u64 {
        self.attempts.load(Ordering::Acquire)
    }

    /// Counts one entry into the ordinary `get` path once the hook was armed.
    #[inline]
    pub(crate) fn count_get(&self) {
        if self.counting.load(Ordering::Relaxed) {
            self.fallback_gets.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn fallback_gets(&self) -> u64 {
        self.fallback_gets.load(Ordering::Acquire)
    }
}
