//! Preallocated, sector-aligned, non-moving read-frame arena and the per-frame
//! residency state machine (INV-1). All capacity is fixed at construction; a
//! frame base never moves for the arena's lifetime.

#[cfg(target_os = "linux")]
use core::ffi::{c_int, c_void};
use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::ptr::NonNull;

use crate::driver::ReadFrameIdx;
use crate::pool::SECTOR_BYTES;
use crate::sync::{AtomicU8, Ordering};

const SECTOR: usize = SECTOR_BYTES as usize;

const HUGEPAGE_BYTES: usize = 2 * 1024 * 1024;

// madvise(2) declared to match glibc's C ABI on the linux build targets; the
// signature follows the man page. MADV_HUGEPAGE is arch-uniform in the linux uapi
// (asm-generic/mman-common.h): 14.
#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
}

#[cfg(target_os = "linux")]
const MADV_HUGEPAGE: c_int = 14;

/// Residency of one frame. [`FrameState::advance`] admits the residency cycle
/// `Free → InFlight → Resident → Evicting → Free` (INV-1) plus the miss-abort edge
/// `InFlight → Free`, and panics on any other edge. [`Frames::abort_inflight`]
/// takes the abort edge for a faulted or EOF-terminated read whose frame was never
/// published `Resident` nor mapped, so no guard borrows it and the reclamation
/// stages do not apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameState {
    Free,
    InFlight,
    Resident,
    Evicting,
}

impl FrameState {
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

    fn to_tag(self) -> u8 {
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
}

/// A fixed set of `count` granule-sized frames in one sector-aligned, non-moving
/// allocation. Frame `i` owns the byte region `[i * granule, (i + 1) * granule)`;
/// its base is sector-aligned because the allocation is and `granule` is a
/// multiple of a sector.
#[derive(Debug)]
pub struct Frames {
    base: NonNull<u8>,
    layout: Layout,
    states: Box<[AtomicU8]>,
    count: u32,
    granule: u32,
}

// SAFETY: `base` addresses a heap allocation owned solely by this `Frames`. A
// read borrow (`frame_bytes`) is handed out only for a Resident/Evicting frame
// and a byte write (`fill`) targets only a Free frame; the residency state
// machine keeps those two disjoint per frame, so no write ever aliases a live
// shared borrow of the same granule. Sharing the arena is thus no less sound
// than sharing any `Box<[u8]>` behind that discipline.
unsafe impl Send for Frames {}
// SAFETY: see the `Send` impl — every frame's residency state lives in an
// `AtomicU8` and its bytes are gated by that state machine.
unsafe impl Sync for Frames {}

impl Frames {
    /// Preallocates `count` frames of `granule` bytes each, sector-aligned and
    /// all `Free`. The whole span is allocated once and never grows.
    ///
    /// # Panics
    ///
    /// If `count` is zero, if `granule` is not a power of two or falls below the
    /// sector floor, or the total span overflows a `Layout`.
    #[must_use]
    pub fn preallocated(count: u32, granule: u32) -> Self {
        assert!(count > 0, "frame count must be positive");
        assert!(granule.is_power_of_two(), "granule must be a power of two");
        assert!(
            granule >= SECTOR_BYTES,
            "granule must not fall below the sector floor"
        );
        let span = (count as usize)
            .checked_mul(granule as usize)
            .expect("frame arena span within isize::MAX");
        let layout = Layout::from_size_align(span, arena_alignment(span, granule))
            .expect("valid frame arena layout");
        // SAFETY: `layout` has a non-zero size (count, granule both positive); a
        // null return is routed to `handle_alloc_error`, so `base` is a live,
        // sector-aligned allocation of `span` bytes.
        let ptr = unsafe { alloc(layout) };
        let base = NonNull::new(ptr).unwrap_or_else(|| handle_alloc_error(layout));
        advise_hugepage(base, span);
        // SAFETY: `base` addresses `span` writable bytes owned solely here; no
        // reference to them exists yet.
        unsafe { std::ptr::write_bytes(base.as_ptr(), 0, span) };
        Self {
            base,
            layout,
            states: (0..count)
                .map(|_| AtomicU8::new(FrameState::Free.to_tag()))
                .collect(),
            count,
            granule,
        }
    }

    #[must_use]
    pub fn count(&self) -> u32 {
        self.count
    }

    /// The `granule`-byte region backing `frame`. The base never moves.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range for the configured count.
    #[must_use]
    pub fn frame_bytes(&self, frame: ReadFrameIdx) -> &[u8] {
        let index = self.checked_index(frame);
        let offset = index * self.granule as usize;
        // SAFETY: `offset + granule <= span` because `index < count`.
        let start = unsafe { self.base.as_ptr().add(offset) };
        // SAFETY: the region is initialised (zeroed at construction) and lives as
        // long as `&self`.
        unsafe { std::slice::from_raw_parts(start, self.granule as usize) }
    }

    /// The residency state of `frame`.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range for the configured count.
    #[must_use]
    pub fn state(&self, frame: ReadFrameIdx) -> FrameState {
        FrameState::from_tag(self.states[self.checked_index(frame)].load(Ordering::Relaxed))
    }

    /// Advances `frame` through the residency cycle, storing the new state. Takes
    /// `&self` so the composed pool drives the state machine through a shared
    /// borrow while guards hold read borrows of other frames' bytes. The load
    /// then store is not one atomic RMW: callers serialize it under the pool's
    /// AD-4 lock (poll) or single-writer discipline (miss completion), which T009
    /// loom models; a bare interleave here would be a race.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range, or the transition is illegal (INV-1).
    pub fn advance(&self, frame: ReadFrameIdx, to: FrameState) {
        let index = self.checked_index(frame);
        let current = FrameState::from_tag(self.states[index].load(Ordering::Relaxed));
        self.states[index].store(current.advance(to).to_tag(), Ordering::Relaxed);
    }

    /// Fills `frame`'s whole granule with `byte`, standing in for a read
    /// completion writing the frame's contents.
    ///
    /// # Safety
    ///
    /// No live `frame_bytes` borrow of `frame`'s granule may exist, and no other
    /// thread may touch it, for the duration of the write: `frame` must be
    /// unmapped in the page table (pre-Resident in the miss-completion path), so
    /// `pin` cannot have handed out a guard over these bytes. This raw seam is
    /// superseded by T008's state-gated fill lease.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range.
    pub unsafe fn fill(&self, frame: ReadFrameIdx, byte: u8) {
        let index = self.checked_index(frame);
        let offset = index * self.granule as usize;
        // SAFETY: `offset + granule <= span` because `index < count`.
        let start = unsafe { self.base.as_ptr().add(offset) };
        // SAFETY: the caller contract guarantees no live borrow of this granule,
        // so the write aliases no shared reference.
        unsafe { std::ptr::write_bytes(start, byte, self.granule as usize) };
    }

    /// Fills `frame`'s whole granule with `byte` while it is `InFlight` — the
    /// state-gated fill seam (T008) that discharges [`Frames::fill`]'s
    /// no-live-borrow contract for the miss-completion path. An `InFlight` frame
    /// is unmapped in the page table, so `pin` cannot have handed out a guard over
    /// its bytes and this write aliases no live borrow.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range, or the frame is not `InFlight` — only an
    /// unpublished in-flight frame accepts a fill.
    pub fn fill_inflight(&self, frame: ReadFrameIdx, byte: u8) {
        assert_eq!(
            self.state(frame),
            FrameState::InFlight,
            "a filled frame is InFlight — unmapped, so no guard borrows its bytes"
        );
        // SAFETY: the `InFlight` state gate guarantees `frame` is unmapped in the
        // page table, so `pin` never handed out a guard over these bytes; the write
        // aliases no live shared borrow of the granule.
        unsafe { self.fill(frame, byte) };
    }

    /// Returns an `InFlight` frame directly to `Free` — the miss-abort seam for a
    /// faulted or EOF-terminated read whose frame was never published `Resident`
    /// and never mapped, so no guard borrows it and the residency cycle's
    /// `Resident`/`Evicting` reclamation stages do not apply.
    ///
    /// # Panics
    ///
    /// If `frame` is out of range, or the frame is not `InFlight` — only an
    /// unpublished in-flight frame aborts.
    pub fn abort_inflight(&self, frame: ReadFrameIdx) {
        let index = self.checked_index(frame);
        assert_eq!(
            FrameState::from_tag(self.states[index].load(Ordering::Relaxed)),
            FrameState::InFlight,
            "only an unpublished InFlight frame aborts back to Free"
        );
        self.advance(frame, FrameState::Free);
    }

    fn checked_index(&self, frame: ReadFrameIdx) -> usize {
        let index = frame.get() as usize;
        assert!(index < self.states.len(), "frame index out of range");
        index
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        // SAFETY: `base`/`layout` are exactly the pair returned by `alloc` in
        // `preallocated`, freed once here at end of life.
        unsafe { dealloc(self.base.as_ptr(), self.layout) }
    }
}

/// Transparent hugepages install only at a 2 MiB-aligned virtual address, so a
/// Linux arena at least a hugepage large is 2 MiB-aligned (or granule-aligned if
/// that is larger); a sector-aligned start would strand the head and tail.
fn arena_alignment(span: usize, granule: u32) -> usize {
    if cfg!(target_os = "linux") && span >= HUGEPAGE_BYTES {
        HUGEPAGE_BYTES.max(granule as usize)
    } else {
        SECTOR
    }
}

/// Must run before the arena's pages are first touched: fault-time THP (defrag
/// `madvise`) backs only untouched ranges, so the hint has to precede the warmup
/// fill.
fn advise_hugepage(base: NonNull<u8>, len: usize) {
    #[cfg(target_os = "linux")]
    {
        if len >= HUGEPAGE_BYTES {
            // SAFETY: `base`/`len` name the live allocation just returned by
            // `alloc_zeroed` and owned solely here; `madvise` only sets a VMA hint
            // and reads or writes no user bytes. The result is ignored on purpose:
            // the hint is best-effort — a `THP=never` kernel returns `EINVAL` and
            // the arena runs correctly on 4 KiB pages, so a rejected hint must
            // never fail construction.
            let _ = unsafe { madvise(base.as_ptr().cast(), len, MADV_HUGEPAGE) };
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (base, len);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn a_hugepage_sized_arena_starts_2mib_aligned() {
        let frame_count =
            u32::try_from(HUGEPAGE_BYTES / SECTOR).expect("hugepage frame count fits u32");
        let frames = Frames::preallocated(frame_count, SECTOR_BYTES);
        let base = frames.frame_bytes(ReadFrameIdx::new(0)).as_ptr().addr();
        assert_eq!(
            base % HUGEPAGE_BYTES,
            0,
            "a >= 2 MiB arena is 2 MiB-aligned so THP can back it from byte zero"
        );
    }

    fn vma_anon_huge_kib(addr: usize) -> u64 {
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
            if in_vma && let Some(rest) = line.strip_prefix("AnonHugePages:") {
                return rest
                    .trim()
                    .trim_end_matches(" kB")
                    .parse()
                    .expect("AnonHugePages field parses");
            }
        }
        panic!("no smaps VMA contains the arena base");
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
        let frames = Frames::preallocated(frame_count, SECTOR_BYTES);
        let base = frames.frame_bytes(ReadFrameIdx::new(0)).as_ptr().addr();
        let resident_kib = vma_anon_huge_kib(base);
        assert!(
            resident_kib >= (HUGEPAGE_BYTES / 1024) as u64,
            "construction's first touch happens after the MADV_HUGEPAGE hint, so at \
             least one of the arena's {} possible hugepages is resident (got {} KiB)",
            span / HUGEPAGE_BYTES,
            resident_kib,
        );
    }
}
