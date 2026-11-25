//! Copy-out read (FR-9 to FR-11): one optimistic attempt that copies a byte
//! range of one resident granule into caller memory under no epoch pin, and at
//! most one ordinary `get` behind it.

#[cfg(all(feature = "mock", not(loom)))]
use std::sync::Arc;

#[cfg(all(feature = "mock", not(loom)))]
use crate::sync::AtomicBool;
use crate::sync::{AtomicU64, Ordering, fence};

#[cfg(loom)]
use super::frames::MODEL_PAYLOAD_WORDS;
use super::translation::entry_frame;
use super::{
    FileId, Frames, Get, GetError, PageId, PendingToken, Pool, PoolBackend, ReadFrameIdx,
    ReaderCtx, Translation, file_generation_is_live,
};

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
    #[inline]
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
        #[cfg(all(feature = "mock", not(loom)))]
        if self
            .optimistic_copy_failures
            .record_attempt_forced_to_fail()
        {
            return self.read_copy_fallback(
                reader,
                page.file(),
                page.granule_idx(),
                offset_bytes,
                output,
            );
        }
        match optimistic_copy(
            &self.table,
            &self.frames,
            &self.file_live_generations,
            page,
            offset_bytes,
            output,
        ) {
            OptimisticCopy::Accepted => Ok(ReadCopy::Copied),
            OptimisticCopy::Fallback => self.read_copy_fallback(
                reader,
                page.file(),
                page.granule_idx(),
                offset_bytes,
                output,
            ),
        }
    }

    /// The one ordinary `get` behind a failed optimistic attempt. It takes
    /// the page's parts, which fit registers, so the inlined fast path keeps
    /// no page in memory for it.
    #[cold]
    #[inline(never)]
    fn read_copy_fallback(
        &self,
        reader: &ReaderCtx,
        file: FileId,
        granule_idx: u32,
        offset_bytes: u32,
        output: &mut [u8],
    ) -> Result<ReadCopy, GetError> {
        match self.get(reader, PageId::new(file, granule_idx))? {
            Get::Hit(guard) => {
                let start = offset_bytes as usize;
                output.copy_from_slice(&guard[start..start + output.len()]);
                Ok(ReadCopy::Copied)
            }
            Get::Pending(token) => Ok(ReadCopy::Pending(token)),
            Get::Busy => Ok(ReadCopy::Busy),
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
/// back itself. An equal stamp around the copy holds one publication of the
/// frame, and the entry loaded inside that bracket names it as `page`'s
/// (AD-7, INV-2). Outside Loom the caller has run [`copy_range_assert`].
#[inline]
pub(crate) fn optimistic_copy(
    translation: &Translation,
    frames: &Frames,
    file_live_generations: &[AtomicU64],
    page: PageId,
    offset_bytes: u32,
    output: &mut [u8],
) -> OptimisticCopy {
    #[cfg(loom)]
    copy_range_assert(frames.granule(), offset_bytes, output.len());
    let Some(entry) = translation.bounded_entry(page) else {
        return OptimisticCopy::Fallback;
    };
    let live_generation = &file_live_generations[page.file().slot() as usize];
    let Some(frame) = entry_frame(entry) else {
        return OptimisticCopy::Fallback;
    };
    let stamp = frames.state_word(frame);
    if !Frames::word_is_resident(stamp) {
        return OptimisticCopy::Fallback;
    }
    if entry_frame(entry) != Some(frame) {
        return OptimisticCopy::Fallback;
    }
    copy_payload(frames, frame, offset_bytes, output);
    fence(Ordering::Acquire);
    if frames.state_word(frame) != stamp {
        return OptimisticCopy::Fallback;
    }
    if !file_generation_is_live(live_generation, page.file()) {
        return OptimisticCopy::Fallback;
    }
    OptimisticCopy::Accepted
}

/// The copy step: one volatile load per payload word the range covers,
/// through the `Frames::optimistic_payload_ptr` seam (TR-6).
#[cfg(not(loom))]
#[inline]
fn copy_payload(frames: &Frames, frame: ReadFrameIdx, offset_bytes: u32, output: &mut [u8]) {
    const WORD_BYTES: usize = size_of::<u64>();
    let payload = frames.optimistic_payload_ptr(frame);
    let offset = offset_bytes as usize;
    let mut word_index = offset / WORD_BYTES;
    let load_word = |word_index: usize| {
        // SAFETY: the range assertion keeps every covered word inside the
        // granule of the frame the seam bound-checked and aligned, in the
        // arena that outlives `frames`, and every caller below names a
        // covered word. A writer racing this load stores whole words
        // atomically behind its claim, and the stamp bracket rejects any
        // copy it overlapped (audits.md, ordering audit).
        unsafe { payload.wrapping_add(word_index).read_volatile() }
    };
    let mut rest = output;
    let skip = offset % WORD_BYTES;
    if skip != 0 {
        let count = (WORD_BYTES - skip).min(rest.len());
        let (head, tail) = rest.split_at_mut(count);
        head.copy_from_slice(&load_word(word_index).to_ne_bytes()[skip..skip + count]);
        rest = tail;
        word_index += 1;
    }
    let (pairs, rest) = rest.as_chunks_mut::<{ 2 * WORD_BYTES }>();
    for pair in pairs {
        copy_payload_store_pair(pair, load_word(word_index), load_word(word_index + 1));
        word_index += 2;
    }
    let last = match rest.split_first_chunk_mut::<WORD_BYTES>() {
        Some((word, tail)) => {
            *word = load_word(word_index).to_ne_bytes();
            word_index += 1;
            tail
        }
        None => rest,
    };
    if !last.is_empty() {
        last.copy_from_slice(&load_word(word_index).to_ne_bytes()[..last.len()]);
    }
}

/// Writes two loaded payload words with one 16-byte store, so a caller's
/// vector load of the output forwards from it instead of stalling on two
/// 8-byte stores.
#[cfg(not(loom))]
#[inline]
fn copy_payload_store_pair(pair: &mut [u8; 16], low: u64, high: u64) {
    cfg_select! {
        target_arch = "x86_64" => {
            use std::arch::x86_64::{__m128i, _mm_set_epi64x, _mm_storeu_si128};
            // SAFETY: SSE2 is part of the x86-64 baseline.
            let words = unsafe { _mm_set_epi64x(high.cast_signed(), low.cast_signed()) };
            #[expect(
                clippy::cast_ptr_alignment,
                reason = "the unaligned store has no alignment precondition"
            )]
            let destination = pair.as_mut_ptr().cast::<__m128i>();
            // SAFETY: `destination` names the 16 writable bytes of `pair`.
            unsafe { _mm_storeu_si128(destination, words) };
        }
        _ => {
            pair.copy_from_slice([low.to_ne_bytes(), high.to_ne_bytes()].as_flattened());
        }
    }
}

#[cfg(loom)]
fn copy_payload(frames: &Frames, frame: ReadFrameIdx, offset_bytes: u32, output: &mut [u8]) {
    assert_eq!(offset_bytes, 0, "the Loom model copies from offset zero");
    copy_payload_words(frames, frame, output);
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

/// Asserts that the copy names at least one byte and ends within the
/// granule, which bounds its length to `u32`, behind one inline test.
#[inline]
fn copy_range_assert(granule: u32, offset_bytes: u32, output_len: usize) {
    let end = u64::from(offset_bytes) + output_len as u64;
    if output_len == 0 || end > u64::from(granule) {
        copy_range_rejected(granule, offset_bytes, output_len);
    }
}

#[cold]
#[inline(never)]
fn copy_range_rejected(granule: u32, offset_bytes: u32, output_len: usize) -> ! {
    assert!(output_len > 0, "a copy names at least one byte");
    let len = u32::try_from(output_len).expect("a copy's length fits u32");
    panic!(
        "the copy range [{offset_bytes}, {offset_bytes} + {len}) ends within the {granule}-byte granule"
    )
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

    /// Counts one optimistic attempt once the hook was armed; `true` while
    /// armed, when the attempt fails without running.
    #[inline]
    fn record_attempt_forced_to_fail(&self) -> bool {
        if self.counting.load(Ordering::Relaxed) {
            self.attempts.fetch_add(1, Ordering::Relaxed);
        }
        self.armed.load(Ordering::Acquire)
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
