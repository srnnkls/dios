//! Preallocated, sector-aligned, non-moving read-frame arena and the per-frame
//! residency state machine (INV-1). All capacity is fixed at construction; a
//! frame base never moves for the arena's lifetime. Writer uniqueness is the
//! [`InFlightFrame`] token, visibility is the residency word, reuse is EBR.

#[cfg(all(target_os = "linux", not(miri)))]
use core::ffi::{c_int, c_void};
use std::alloc::{Layout, handle_alloc_error};
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::num::NonZeroU32;
use std::ptr::NonNull;

use crate::pool::epoch::{FrameGuard, PinBegun, PinCommit};
use crate::pool::{Control, PageId, SECTOR_BYTES};
use crate::sync::{AtomicU64, Ordering};

#[cfg(all(test, target_os = "linux", not(miri)))]
const SECTOR: usize = SECTOR_BYTES as usize;

const HUGEPAGE_BYTES: usize = 2 * 1024 * 1024;
const FRAME_STATE_BITS: u32 = 2;
const FRAME_STATE_MASK: u64 = (1 << FRAME_STATE_BITS) - 1;
const FRAME_GENERATION_MAX: u64 = u64::MAX >> FRAME_STATE_BITS;
const FRAME_STATE_TAG_MAX: u64 = 3;
const _: () = assert!(FRAME_STATE_TAG_MAX <= FRAME_STATE_MASK);

/// One preallocated full-page identity per frame, written under the frame's
/// [`InFlightFrame`] token and read under one of three witnesses: a validated
/// reader pin, a live guard, or the pool control lock over a mapped frame.
#[derive(Debug)]
struct ExactPageCells {
    cells: crate::allocation::MappedSlice<UnsafeCell<MaybeUninit<PageId>>>,
}

// SAFETY: the token serializes every cell write against readers (a token exists
// only while the frame is unpublished) and EBR defers reuse until all validated
// readers exit.
unsafe impl Sync for ExactPageCells {}

impl ExactPageCells {
    fn try_preallocated(count: u32) -> Option<Self> {
        Some(Self {
            cells: crate::allocation::MappedSlice::try_vacant(count)?,
        })
    }

    fn write(&self, token: &mut InFlightFrame, page: PageId) {
        // SAFETY: the unique token holds the frame unpublished, so no reader
        // has validated this frame's Resident word and none can until publish.
        unsafe { (*self.cells[token.index()].get()).write(page) };
    }

    fn read(&self, index: usize) -> PageId {
        // SAFETY: every caller holds a witness (validated pin, live guard, or the
        // control lock over a mapped frame) that a publish preceded this read and
        // that no token can exist until the witness is gone.
        let page = unsafe { &*self.cells[index].get() };
        // SAFETY: `page` was initialized under the token before publish.
        unsafe { page.assume_init_read() }
    }
}

/// Which read-pool frame an internal read lands in. This type is reachable only
/// through the feature-gated structural testing surface; product callers name
/// pages and receive [`crate::FrameGuard`] leases instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReadFrameIdx(u32);

impl ReadFrameIdx {
    #[must_use]
    pub fn new(frame: u32) -> Self {
        Self(frame)
    }

    pub(crate) fn get(self) -> u32 {
        self.0
    }
}

/// Distinguishes frame arenas within the process so a token minted by one is
/// rejected by another whose frame indexes coincide. Handed out once per arena
/// construction — never on the hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArenaId(u32);

impl ArenaId {
    fn next() -> Self {
        let id = crate::allocation::next_arena_id();
        assert!(id > 0, "frame arena ids do not wrap within one process");
        Self(id)
    }
}

/// The unique authority to write one frame's bytes and identity, held from
/// `claim` until `publish` or `abort`. Not `Clone`, not `Copy`, no `Drop`: a
/// token outlives whatever async operation writes the frame and dies only in
/// the consuming call, never when a Rust value goes out of scope. The token
/// names the arena that minted it as well as the frame, so a token cannot
/// address another arena's same-index frame (INV-1).
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct InFlightFrame {
    arena: ArenaId,
    frame: NonZeroU32,
}

impl InFlightFrame {
    fn new(arena: ArenaId, frame: ReadFrameIdx) -> Self {
        Self {
            arena,
            frame: NonZeroU32::new(
                frame
                    .get()
                    .checked_add(1)
                    .expect("frame index is below the u32 capacity"),
            )
            .expect("index plus one is nonzero"),
        }
    }

    #[must_use]
    pub(crate) fn frame(&self) -> ReadFrameIdx {
        ReadFrameIdx::new(self.frame.get() - 1)
    }

    fn index(&self) -> usize {
        (self.frame.get() - 1) as usize
    }
}

// madvise(2) declared to match glibc's C ABI on the linux build targets; the
// signature follows the man page. MADV_HUGEPAGE is arch-uniform in the linux uapi
// (asm-generic/mman-common.h): 14.
#[cfg(all(target_os = "linux", not(miri)))]
unsafe extern "C" {
    fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
}

#[cfg(all(target_os = "linux", not(miri)))]
const MADV_HUGEPAGE: c_int = 14;

/// Residency of one frame. [`FrameState::advance`] admits the residency cycle
/// `Free → InFlight → Resident → Evicting → Free` (INV-1) plus the miss-abort edge
/// `InFlight → Free`, and panics on any other edge. The edges into and out of
/// `InFlight` are taken only through an `InFlightFrame` token
/// (`Frames::claim`, `Frames::publish`, `Frames::abort`); `Frames::advance`
/// drives the two reclamation edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameState {
    Free,
    InFlight,
    Resident,
    Evicting,
}

impl FrameState {
    // A zeroed state word is a free frame, which `MappedSlice::try_vacant` relies on.
    const FREE_TAG_IS_ZERO: () = assert!(FrameState::Free.to_tag() == 0);

    /// Advances to `to`, returning it.
    ///
    /// # Panics
    ///
    /// If `self → to` is not a legal edge of the residency cycle or the miss-abort
    /// edge `InFlight → Free` — the frame state machine admits no other transition
    /// (INV-1).
    #[must_use]
    pub fn advance(self, to: FrameState) -> FrameState {
        let legal = matches!(
            (self, to),
            (FrameState::Free, FrameState::InFlight)
                | (
                    FrameState::InFlight,
                    FrameState::Resident | FrameState::Free
                )
                | (FrameState::Resident, FrameState::Evicting)
                | (FrameState::Evicting, FrameState::Free)
        );
        assert!(legal, "illegal frame transition {self:?} -> {to:?}");
        to
    }

    const fn to_tag(self) -> u8 {
        match self {
            FrameState::Free => 0,
            FrameState::InFlight => 1,
            FrameState::Resident => 2,
            FrameState::Evicting => 3,
        }
    }

    fn from_tag(tag: u8) -> FrameState {
        match tag {
            0 => FrameState::Free,
            1 => FrameState::InFlight,
            2 => FrameState::Resident,
            3 => FrameState::Evicting,
            other => panic!("frame state tag {other} out of range"),
        }
    }

    fn from_word(word: u64) -> FrameState {
        Self::from_tag((word & FRAME_STATE_MASK) as u8)
    }
}

/// A fixed set of `count` granule-sized frames in one sector-aligned, non-moving
/// mapping. Frame `i` owns the byte region `[i * granule, (i + 1) * granule)`;
/// its base is sector-aligned because the mapping is and `granule` is a
/// multiple of a sector. Construction reserves the span without touching it:
/// a frame's page materialises when its first read lands, so an open charges
/// the pool's capacity, not its footprint. Locking or registering the arena
/// populates it as the kernel's part of that posture.
#[derive(Debug)]
pub(crate) struct Frames {
    base: NonNull<u8>,
    arena: crate::allocation::MappedArena,
    states: crate::allocation::MappedSlice<AtomicU64>,
    pages: ExactPageCells,
    count: u32,
    granule: u32,
    id: ArenaId,
}

// SAFETY: `base` addresses a heap allocation owned solely by this `Frames`. A
// byte write needs `&mut InFlightFrame`, which exists only while the frame is
// `InFlight`; a shared byte borrow needs a validated pin or the exclusive
// arena, and a frame reaches `InFlight` again only after EBR has retired every
// such pin. No write ever aliases a live shared borrow of the same granule.
unsafe impl Send for Frames {}
// SAFETY: see the `Send` impl — every frame's residency state lives in an
// `AtomicU64` and its bytes are gated by the token and pin witnesses.
unsafe impl Sync for Frames {}

impl Frames {
    pub(crate) fn try_preallocated(count: u32, granule: u32) -> Option<Self> {
        assert!(count > 0, "frame count must be positive");
        assert!(granule.is_power_of_two(), "granule must be a power of two");
        assert!(
            granule >= SECTOR_BYTES,
            "granule must not fall below the sector floor"
        );
        let span = (count as usize).checked_mul(granule as usize)?;
        let () = FrameState::FREE_TAG_IS_ZERO;
        let states = crate::allocation::MappedSlice::<AtomicU64>::try_vacant(count)?;
        let pages = ExactPageCells::try_preallocated(count)?;
        let arena = crate::allocation::MappedArena::try_map(span, arena_alignment(span, granule))?;
        advise_hugepage(arena.base(), span);
        Some(Self {
            base: arena.base(),
            arena,
            states,
            pages,
            count,
            granule,
            id: ArenaId::next(),
        })
    }

    /// Preallocates `count` frames of `granule` bytes each, sector-aligned and
    /// all `Free`. The whole span is allocated once and never grows.
    ///
    /// # Panics
    ///
    /// If `count` is zero, if `granule` is not a power of two or falls below the
    /// sector floor, or the total span overflows a `Layout`.
    #[must_use]
    pub(crate) fn preallocated(count: u32, granule: u32) -> Self {
        assert!(count > 0, "frame count must be positive");
        assert!(granule.is_power_of_two(), "granule must be a power of two");
        assert!(
            granule >= SECTOR_BYTES,
            "granule must not fall below the sector floor"
        );
        Self::try_preallocated(count, granule).unwrap_or_else(|| {
            let span = (count as usize)
                .checked_mul(granule as usize)
                .expect("frame arena span within isize::MAX");
            let layout = Layout::from_size_align(span, arena_alignment(span, granule))
                .expect("valid frame arena layout");
            handle_alloc_error(layout)
        })
    }

    #[must_use]
    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    pub(crate) fn granule(&self) -> u32 {
        self.granule
    }

    pub(crate) fn span_len(&self) -> usize {
        self.arena.len()
    }

    pub(crate) fn base_ptr(&self) -> *mut u8 {
        self.base.as_ptr()
    }

    /// Touches every frame so the whole span is resident before any read —
    /// the eager fill construction no longer performs, kept as the base arm of
    /// the population bench and for tests that need a resident arena.
    ///
    /// # Panics
    ///
    /// If any frame has left `Free` — population precedes first use.
    pub(crate) fn populate(&mut self) {
        for index in 0..self.count as usize {
            assert_eq!(
                FrameState::from_word(self.states[index].load(Ordering::Acquire)),
                FrameState::Free,
                "population precedes first use"
            );
            // SAFETY: `&mut self` excludes every live borrow of the arena, so
            // the zero fill aliases no shared reference.
            unsafe { std::ptr::write_bytes(self.granule_ptr(index), 0, self.granule as usize) };
        }
    }

    /// Takes `frame` from `Free` to `InFlight` and writes `page` as its exact
    /// identity, minting the unique write token. `None` when the frame is not
    /// `Free`, including when another claimant won the compare-exchange.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range.
    pub(crate) fn claim(&self, frame: ReadFrameIdx, page: PageId) -> Option<InFlightFrame> {
        let mut token = self.claim_unidentified(frame)?;
        self.pages.write(&mut token, page);
        Some(token)
    }

    /// Takes `frame` from `Free` to `InFlight` without writing an exact identity,
    /// minting the unique write token for a lease that aborts at completion and
    /// never publishes. `None` on the same contended and non-`Free` cases as
    /// [`Frames::claim`].
    ///
    /// # Panics
    ///
    /// If `frame` is out of range.
    pub(crate) fn claim_unidentified(&self, frame: ReadFrameIdx) -> Option<InFlightFrame> {
        let index = self.checked_index(frame);
        let current_word = self.states[index].load(Ordering::Acquire);
        let current = FrameState::from_word(current_word);
        if current != FrameState::Free {
            return None;
        }
        let next = current.advance(FrameState::InFlight);
        let next_word = (current_word & !FRAME_STATE_MASK) | u64::from(next.to_tag());
        self.states[index]
            .compare_exchange(current_word, next_word, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(InFlightFrame::new(self.id, frame))
    }

    /// Fills the token's whole granule with `byte`, standing in for a read
    /// completion writing the frame's contents.
    pub(crate) fn fill(&self, token: &mut InFlightFrame, byte: u8) {
        self.transfer_mut(token, 0, self.granule).fill(byte);
    }

    /// The exclusive destination range `[offset, offset + len)` of the token's
    /// frame, borrowed for as long as the token is.
    ///
    /// # Panics
    ///
    /// If the range leaves the frame.
    pub(crate) fn transfer_mut<'token>(
        &'token self,
        token: &'token mut InFlightFrame,
        destination_offset: u32,
        requested_len: u32,
    ) -> &'token mut [u8] {
        let start = self.transfer_ptr(token, destination_offset, requested_len);
        // SAFETY: `transfer_ptr` keeps the range inside the arena. `token` is
        // the unique write authority for this frame, the exclusive reborrow
        // pins it for the slice's life, and a shared byte borrow of an
        // `InFlight` frame is unreachable (every reader witness proves a publish
        // that has not happened, or a pin EBR retired before the claim).
        unsafe { std::slice::from_raw_parts_mut(start, requested_len as usize) }
    }

    /// The raw destination pointer for a backend that writes without a Rust
    /// reference (an `io_uring` SQE); the token stays with the op until its
    /// completion.
    ///
    /// # Panics
    ///
    /// If the range leaves the frame.
    pub(crate) fn transfer_ptr(
        &self,
        token: &InFlightFrame,
        destination_offset: u32,
        requested_len: u32,
    ) -> *mut u8 {
        let index = self.owned_index(token);
        debug_assert_eq!(
            FrameState::from_word(self.states[index].load(Ordering::Acquire)),
            FrameState::InFlight,
            "a live token names an InFlight frame"
        );
        assert!(
            destination_offset <= self.granule,
            "destination offset lies within the frame"
        );
        assert!(
            requested_len <= self.granule - destination_offset,
            "requested range lies within the frame"
        );
        // SAFETY: the frame index and range assertions keep the offset within
        // this allocation; no reference is created here.
        unsafe { self.granule_ptr(index).add(destination_offset as usize) }
    }

    /// Consumes the token, Release-publishing the frame `Resident` under a new
    /// residency generation. Returns the frame so the caller can map it.
    pub(crate) fn publish(&self, token: InFlightFrame) -> ReadFrameIdx {
        self.consume(token, FrameState::Resident)
    }

    /// Consumes the token, returning the frame to `Free` with its residency
    /// generation intact — the miss-abort edge for a faulted, EOF-terminated or
    /// refused read that was never published.
    pub(crate) fn abort(&self, token: InFlightFrame) -> ReadFrameIdx {
        self.consume(token, FrameState::Free)
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "the token is linear: publish and abort consume it so no writer survives them"
    )]
    fn consume(&self, token: InFlightFrame, to: FrameState) -> ReadFrameIdx {
        let index = self.owned_index(&token);
        let current_word = self.states[index].load(Ordering::Acquire);
        let current = FrameState::from_word(current_word);
        assert_eq!(
            current,
            FrameState::InFlight,
            "a live token names an InFlight frame"
        );
        let next = current.advance(to);
        let generation = Self::advance_generation(current_word, next);
        let next_word = (generation << FRAME_STATE_BITS) | u64::from(next.to_tag());
        // The token is the only writer of an InFlight word, so a plain Release
        // store publishes; the reclamation edges never touch an InFlight frame.
        self.states[index].store(next_word, Ordering::Release);
        token.frame()
    }

    /// The `granule`-byte region backing `frame`, readable under a committed
    /// pin: the pin's published epoch keeps EBR from freeing the frame, so no
    /// token can be minted for it while the borrow lives.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range for the configured count.
    #[must_use]
    pub(crate) fn frame_bytes(&self, frame: ReadFrameIdx, _pin: &PinCommit) -> &[u8] {
        let index = self.checked_index(frame);
        // SAFETY: the committed pin proves a validated Resident/Evicting frame
        // whose reuse EBR defers past this borrow, so no token writes it.
        unsafe { self.granule_slice(index) }
    }

    /// The `granule`-byte region backing `frame` under exclusive arena
    /// ownership: no token, pin or backend can exist alongside `&mut self`.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range for the configured count.
    #[must_use]
    pub(crate) fn frame_bytes_exclusive(&mut self, frame: ReadFrameIdx) -> &[u8] {
        let index = self.checked_index(frame);
        // SAFETY: `&mut self` excludes every other borrow of the arena.
        unsafe { self.granule_slice(index) }
    }

    #[cfg(any(feature = "mock", feature = "bench"))]
    pub(crate) fn copy_frame(&self, frame: ReadFrameIdx, out: &mut [u8]) -> usize {
        let index = self.checked_index(frame);
        assert_ne!(
            self.state(frame),
            FrameState::InFlight,
            "a test observation never aliases an in-flight frame write"
        );
        // SAFETY: a frame that is not `InFlight` has no token, so nothing
        // writes it; the caller holds the driver submit lock, which excludes a
        // new claim while the copy runs.
        let source = unsafe { self.granule_slice(index) };
        let copied = out.len().min(source.len());
        out[..copied].copy_from_slice(&source[..copied]);
        copied
    }

    /// The residency state of `frame`.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range for the configured count.
    #[must_use]
    pub(crate) fn state(&self, frame: ReadFrameIdx) -> FrameState {
        FrameState::from_word(self.state_word(frame))
    }

    #[must_use]
    pub(crate) fn state_word(&self, frame: ReadFrameIdx) -> u64 {
        self.states[self.checked_index(frame)].load(Ordering::Acquire)
    }

    pub(crate) fn word_is_resident(word: u64) -> bool {
        FrameState::from_word(word) == FrameState::Resident
    }

    /// The exact identity of a frame that a begun pin has just validated: the
    /// frame's word still equals `stamp` and is `Resident`, so the publish that
    /// wrote the identity happens-before this read and the pin's epoch defers
    /// reuse. `None` when the frame moved on since the stamp was taken.
    #[must_use]
    pub(crate) fn validate_resident(
        &self,
        frame: ReadFrameIdx,
        stamp: u64,
        _pin: &PinBegun,
    ) -> Option<PageId> {
        let index = self.checked_index(frame);
        let word = self.states[index].load(Ordering::Acquire);
        if word != stamp || !Self::word_is_resident(word) {
            return None;
        }
        Some(self.pages.read(index))
    }

    /// The exact identity under a live guard: the guard's pin keeps the frame
    /// mapped or held and its identity unwritten.
    #[must_use]
    pub(crate) fn exact_page_guarded(&self, guard: &FrameGuard<'_>) -> PageId {
        let index = self.checked_index(guard.frame);
        assert!(
            matches!(
                FrameState::from_word(self.states[index].load(Ordering::Acquire)),
                FrameState::Resident | FrameState::Evicting
            ),
            "a guarded frame is published"
        );
        self.pages.read(index)
    }

    /// The exact identity of a published frame under exclusive arena ownership.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range or not published.
    #[cfg(test)]
    #[must_use]
    pub(super) fn exact_page_exclusive(&mut self, frame: ReadFrameIdx) -> PageId {
        let index = self.checked_index(frame);
        assert!(
            matches!(
                FrameState::from_word(self.states[index].load(Ordering::Acquire)),
                FrameState::Resident | FrameState::Evicting
            ),
            "an exclusive identity read names a published frame"
        );
        self.pages.read(index)
    }

    /// The exact identity of a mapped `Resident`/`Evicting` frame under the
    /// pool control lock, which excludes both reclamation and a new claim.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range or not published.
    #[must_use]
    pub(super) fn exact_page_locked(&self, frame: ReadFrameIdx, _control: &Control) -> PageId {
        let index = self.checked_index(frame);
        assert!(
            matches!(
                FrameState::from_word(self.states[index].load(Ordering::Acquire)),
                FrameState::Resident | FrameState::Evicting
            ),
            "a locked identity read names a published frame"
        );
        self.pages.read(index)
    }

    /// Advances `frame` along a reclamation edge, `Resident → Evicting` or
    /// `Evicting → Free`, storing the new state. Takes `&self` so the composed
    /// pool drives the state machine through a shared borrow while guards hold
    /// read borrows of other frames' bytes. The load then store is not one
    /// atomic RMW: callers serialize it under the pool's AD-4 lock, which T009
    /// loom models; a bare interleave here would be a race.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range, or the edge is not a reclamation edge — the
    /// edges into and out of `InFlight` belong to the token (INV-1).
    pub(crate) fn advance(&self, frame: ReadFrameIdx, to: FrameState) {
        let index = self.checked_index(frame);
        let current_word = self.states[index].load(Ordering::Acquire);
        let current = FrameState::from_word(current_word);
        let reclamation_edge = matches!(
            (current, to),
            (FrameState::Resident, FrameState::Evicting) | (FrameState::Evicting, FrameState::Free)
        );
        assert!(
            reclamation_edge,
            "illegal frame transition {current:?} -> {to:?} outside a token"
        );
        let next = current.advance(to);
        let generation = Self::advance_generation(current_word, next);
        let next_word = (generation << FRAME_STATE_BITS) | u64::from(next.to_tag());
        self.states[index].store(next_word, Ordering::Release);
    }

    fn advance_generation(current_word: u64, next: FrameState) -> u64 {
        let generation = current_word >> FRAME_STATE_BITS;
        if next == FrameState::Resident {
            generation
                .checked_add(1)
                .filter(|&next| next <= FRAME_GENERATION_MAX)
                .expect("frame residency generation exhausted before ABA")
        } else {
            generation
        }
    }

    fn granule_ptr(&self, index: usize) -> *mut u8 {
        debug_assert!(index < self.count as usize, "granule index in range");
        // SAFETY: `index * granule + granule <= span` because `index < count`.
        unsafe { self.base.as_ptr().add(index * self.granule as usize) }
    }

    /// # Safety
    ///
    /// No token may write frame `index` while the returned borrow lives: the
    /// caller holds a committed pin, the exclusive arena, or the not-`InFlight`
    /// observation under the driver lock.
    unsafe fn granule_slice(&self, index: usize) -> &[u8] {
        // SAFETY: the pointer lies within the arena and the mapping is zeroed
        // at construction, so every byte is initialized; the caller contract
        // excludes a concurrent write.
        unsafe { std::slice::from_raw_parts(self.granule_ptr(index), self.granule as usize) }
    }

    fn checked_index(&self, frame: ReadFrameIdx) -> usize {
        let index = frame.get() as usize;
        assert!(index < self.states.len(), "frame index out of range");
        index
    }

    fn owned_index(&self, token: &InFlightFrame) -> usize {
        assert_eq!(
            token.arena, self.id,
            "a token addresses only the arena that minted it"
        );
        let index = token.index();
        assert!(index < self.count as usize, "token frame index in range");
        index
    }
}

/// Transparent hugepages install only at a 2 MiB-aligned virtual address, so a
/// Linux arena at least a hugepage large is 2 MiB-aligned (or granule-aligned if
/// that is larger); a sector-aligned start would strand the head and tail.
fn arena_alignment(span: usize, granule: u32) -> usize {
    if cfg!(target_os = "linux") && span >= HUGEPAGE_BYTES {
        HUGEPAGE_BYTES.max(granule as usize)
    } else {
        granule as usize
    }
}

/// Must run before the arena's pages are first touched: fault-time THP (defrag
/// `madvise`) backs only untouched ranges, so the hint has to precede the first
/// read into any frame.
fn advise_hugepage(base: NonNull<u8>, len: usize) {
    #[cfg(all(target_os = "linux", not(miri)))]
    {
        if len >= HUGEPAGE_BYTES {
            // SAFETY: `base`/`len` name the live mapping just created and
            // owned solely here; `madvise` only sets a VMA hint
            // and reads or writes no user bytes. The result is ignored on purpose:
            // the hint is best-effort — a `THP=never` kernel returns `EINVAL` and
            // the arena runs correctly on 4 KiB pages, so a rejected hint must
            // never fail construction.
            let _ = unsafe { madvise(base.as_ptr().cast(), len, MADV_HUGEPAGE) };
        }
    }
    // Miri models no `madvise`, and the hint carries no semantics to model.
    #[cfg(not(all(target_os = "linux", not(miri))))]
    {
        let _ = (base, len);
    }
}

#[cfg(all(test, target_os = "linux", not(miri)))]
mod tests {
    use super::*;

    #[test]
    fn a_hugepage_sized_arena_starts_2mib_aligned() {
        let frame_count =
            u32::try_from(HUGEPAGE_BYTES / SECTOR).expect("hugepage frame count fits u32");
        let mut frames = Frames::preallocated(frame_count, SECTOR_BYTES);
        let base = frames
            .frame_bytes_exclusive(ReadFrameIdx::new(0))
            .as_ptr()
            .addr();
        assert_eq!(
            base % HUGEPAGE_BYTES,
            0,
            "a >= 2 MiB arena is 2 MiB-aligned so THP can back it from byte zero"
        );
    }

    fn vma_anon_huge_kib(addr: usize) -> u64 {
        vma_field_kib(addr, "AnonHugePages:")
    }

    fn vma_field_kib(addr: usize, field: &str) -> u64 {
        let smaps = std::fs::read_to_string("/proc/self/smaps").expect("smaps is readable");
        let mut in_vma = false;
        for line in smaps.lines() {
            if let Some((range, _)) = line.split_once(' ')
                && let Some((start, end)) = range.split_once('-')
                && let (Ok(start), Ok(end)) = (
                    usize::from_str_radix(start, 16),
                    usize::from_str_radix(end, 16),
                )
            {
                in_vma = (start..end).contains(&addr);
                continue;
            }
            if in_vma && let Some(rest) = line.strip_prefix(field) {
                return rest
                    .trim()
                    .trim_end_matches(" kB")
                    .parse()
                    .expect("smaps field parses");
            }
        }
        panic!("no smaps VMA contains the arena base");
    }

    #[test]
    fn a_fresh_arena_is_not_resident_until_touched() {
        let span = 64 * HUGEPAGE_BYTES;
        let frame_count = u32::try_from(span / SECTOR).expect("frame count fits u32");
        let mut frames = Frames::preallocated(frame_count, SECTOR_BYTES);
        let base = frames
            .frame_bytes_exclusive(ReadFrameIdx::new(0))
            .as_ptr()
            .addr();
        assert_eq!(
            vma_field_kib(base, "Rss:"),
            0,
            "construction reserves the span without touching a page"
        );
        frames.populate();
        assert_eq!(
            vma_field_kib(base, "Rss:"),
            (span / 1024) as u64,
            "population makes the whole span resident"
        );
    }

    fn thp_fault_backing_available() -> bool {
        let enabled = std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled")
            .unwrap_or_default();
        enabled.contains("[always]") || enabled.contains("[madvise]")
    }

    #[test]
    fn a_hugepage_sized_arena_is_hugepage_backed_when_thp_is_available() {
        if !thp_fault_backing_available() {
            eprintln!("skipped: kernel offers no fault-time THP");
            return;
        }
        let span = 4 * HUGEPAGE_BYTES;
        let frame_count = u32::try_from(span / SECTOR).expect("frame count fits u32");
        let mut frames = Frames::preallocated(frame_count, SECTOR_BYTES);
        frames.populate();
        let base = frames
            .frame_bytes_exclusive(ReadFrameIdx::new(0))
            .as_ptr()
            .addr();
        let resident_kib = vma_anon_huge_kib(base);
        assert!(
            resident_kib >= (HUGEPAGE_BYTES / 1024) as u64,
            "the first touch happens after the MADV_HUGEPAGE hint, so at \
             least one of the arena's {} possible hugepages is resident (got {} KiB)",
            span / HUGEPAGE_BYTES,
            resident_kib,
        );
    }
}

#[cfg(test)]
mod alignment_tests {
    use super::*;

    #[test]
    fn a_small_arena_still_honors_a_granule_above_the_sector_floor() {
        assert_eq!(
            arena_alignment(8192, 8192),
            8192,
            "each frame base must satisfy a direct device whose alignment exceeds 4096 bytes"
        );
    }
}

/// The `Free` frames as a fixed stack, so a claim pops in constant time
/// instead of walking the frame states. Every `→ Free` transition on the
/// control path pushes; the stack never exceeds the frame count.
#[derive(Debug)]
pub(crate) struct FreeFrames {
    frames: Box<[ReadFrameIdx]>,
    len: u32,
}

impl FreeFrames {
    /// Holds every frame, ordered so the first pop claims frame 0.
    pub(crate) fn try_with_all(frame_count: u32) -> Option<Self> {
        let mut next = frame_count;
        let frames = crate::allocation::try_boxed_slice_with(frame_count, || {
            next -= 1;
            ReadFrameIdx::new(next)
        })?;
        assert_eq!(next, 0, "the stack holds every frame exactly once");
        Some(Self {
            frames,
            len: frame_count,
        })
    }

    pub(crate) fn push(&mut self, frame: ReadFrameIdx) {
        let index = self.len as usize;
        assert!(
            index < self.frames.len(),
            "a frame frees at most once between claims"
        );
        self.frames[index] = frame;
        self.len += 1;
    }

    pub(crate) fn pop(&mut self) -> Option<ReadFrameIdx> {
        let index = self.len.checked_sub(1)?;
        self.len = index;
        Some(self.frames[index as usize])
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> u32 {
        self.len
    }
}

#[cfg(all(test, not(loom)))]
mod free_frames_tests {
    use super::{FreeFrames, ReadFrameIdx};

    #[test]
    fn claims_start_at_frame_zero_and_freed_frames_return_last_in_first_out() {
        let mut free = FreeFrames::try_with_all(4).expect("allocation");
        assert_eq!(free.len(), 4);
        assert_eq!(free.pop(), Some(ReadFrameIdx::new(0)));
        assert_eq!(free.pop(), Some(ReadFrameIdx::new(1)));
        free.push(ReadFrameIdx::new(0));
        assert_eq!(free.pop(), Some(ReadFrameIdx::new(0)));
        assert_eq!(free.pop(), Some(ReadFrameIdx::new(2)));
        assert_eq!(free.pop(), Some(ReadFrameIdx::new(3)));
        assert_eq!(free.pop(), None);
        assert_eq!(free.len(), 0);
    }

    #[test]
    #[should_panic(expected = "a frame frees at most once between claims")]
    fn pushing_past_the_frame_count_is_a_programmer_error() {
        let mut free = FreeFrames::try_with_all(1).expect("allocation");
        free.push(ReadFrameIdx::new(0));
    }
}

#[cfg(all(test, not(loom)))]
mod token_tests {
    use super::*;
    use crate::driver::FileId;

    trait AmbiguousIfClone<A> {
        fn resolve() {}
    }
    impl<T: ?Sized> AmbiguousIfClone<()> for T {}
    impl<T: Clone> AmbiguousIfClone<SecondImpl> for T {}
    struct SecondImpl;

    fn page(granule: u32) -> PageId {
        PageId::new(FileId::new(7, 3, 1), granule)
    }

    #[test]
    fn a_write_token_is_affine() {
        let _ = <InFlightFrame as AmbiguousIfClone<_>>::resolve;
        assert_eq!(size_of::<Option<InFlightFrame>>(), 8);
    }

    #[test]
    fn a_second_claim_of_the_same_frame_is_refused() {
        let frames = Frames::preallocated(2, SECTOR_BYTES);
        let frame = ReadFrameIdx::new(1);
        let token = frames.claim(frame, page(0)).expect("a Free frame claims");
        assert!(frames.claim(frame, page(0)).is_none());
        assert_eq!(frames.state(frame), FrameState::InFlight);
        assert_eq!(frames.abort(token), frame);
        assert_eq!(frames.state(frame), FrameState::Free);
        assert!(frames.claim(frame, page(0)).is_some());
    }

    #[test]
    fn publish_bumps_the_generation_and_abort_keeps_it() {
        let frames = Frames::preallocated(1, SECTOR_BYTES);
        let frame = ReadFrameIdx::new(0);
        let generation = |frames: &Frames| frames.state_word(frame) >> FRAME_STATE_BITS;
        let initial = generation(&frames);
        let token = frames.claim(frame, page(4)).expect("claim");
        assert_eq!(generation(&frames), initial);
        frames.abort(token);
        assert_eq!(generation(&frames), initial);
        let token = frames.claim(frame, page(4)).expect("claim");
        assert_eq!(frames.publish(token), frame);
        assert_eq!(frames.state(frame), FrameState::Resident);
        assert_eq!(generation(&frames), initial + 1);
    }

    #[test]
    fn the_identity_written_at_claim_is_the_published_identity() {
        let frames = Frames::preallocated(1, SECTOR_BYTES);
        let frame = ReadFrameIdx::new(0);
        let mut token = frames.claim(frame, page(9)).expect("claim");
        frames.fill(&mut token, 0x5A);
        frames.transfer_mut(&mut token, 8, 4).fill(0xC3);
        let stamp = frames.state_word(frame);
        frames.publish(token);
        let mut frames = frames;
        let bytes = frames.frame_bytes_exclusive(frame);
        assert_eq!(
            &bytes[..12],
            &[0x5A; 8]
                .iter()
                .chain(&[0xC3; 4])
                .copied()
                .collect::<Vec<_>>()[..]
        );
        assert_ne!(stamp, frames.state_word(frame));
    }

    #[test]
    #[should_panic(expected = "illegal frame transition")]
    fn advance_refuses_the_token_owned_edges() {
        let frames = Frames::preallocated(1, SECTOR_BYTES);
        frames.advance(ReadFrameIdx::new(0), FrameState::InFlight);
    }

    #[test]
    #[should_panic(expected = "a token addresses only the arena that minted it")]
    fn one_arena_refuses_another_arenas_same_index_token() {
        let first = Frames::preallocated(1, SECTOR_BYTES);
        let second = Frames::preallocated(1, SECTOR_BYTES);
        let frame = ReadFrameIdx::new(0);
        let _held = first
            .claim(frame, page(0))
            .expect("claim in the first arena");
        let borrowed = second
            .claim(frame, page(0))
            .expect("claim in the second arena");
        first.publish(borrowed);
    }

    #[test]
    fn an_unidentified_claim_takes_the_frame_in_flight_without_a_page() {
        let frames = Frames::preallocated(1, SECTOR_BYTES);
        let frame = ReadFrameIdx::new(0);
        let token = frames
            .claim_unidentified(frame)
            .expect("a Free frame claims");
        assert_eq!(frames.state(frame), FrameState::InFlight);
        assert!(frames.claim_unidentified(frame).is_none());
        assert_eq!(frames.abort(token), frame);
        assert_eq!(frames.state(frame), FrameState::Free);
    }
}
