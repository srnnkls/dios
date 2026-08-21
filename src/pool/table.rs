//! Open-addressed `PageId → ReadFrameIdx` map with linear probing, fixed
//! capacity `2 × frame_count` rounded up to a power of two (≤ 50 % load at a
//! full pool bounds negative-probe length), backward-shift deletion (no
//! tombstones), and no rehash or growth ever.
//!
//! Each slot is a per-slot single-writer seqlock over atomics, so a `pin` warm
//! probe loads a cell lock-free without a data race while a completion under the
//! AD-4 control-plane lock is the sole writer. The single-threaded observable
//! correctness of the seqlock is pinned by the T006 table tests and the T008 miss
//! path; the concurrent read/write interleaving proof is T009 loom.

use crate::driver::FileId;
use crate::pool::PageId;
use crate::pool::ReadFrameIdx;
use crate::sync::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence, spin_loop};

/// Under the AD-4 single-writer discipline a read that never observes a stable
/// even version is a stuck writer — a bug to crash on, not to spin on forever.
const SEQLOCK_READ_SPINS_MAX: u32 = 1_000_000;

#[derive(Debug)]
struct Cell {
    seq: AtomicU64,
    occupied: AtomicBool,
    driver: AtomicU64,
    file_slot: AtomicU32,
    file_generation: AtomicU32,
    granule_idx: AtomicU32,
    frame: AtomicU32,
}

impl Cell {
    fn vacant() -> Self {
        Self {
            seq: AtomicU64::new(0),
            occupied: AtomicBool::new(false),
            driver: AtomicU64::new(0),
            file_slot: AtomicU32::new(0),
            file_generation: AtomicU32::new(0),
            granule_idx: AtomicU32::new(0),
            frame: AtomicU32::new(0),
        }
    }

    fn write(&self, entry: Option<(PageId, ReadFrameIdx)>) {
        let version = self.seq.load(Ordering::Relaxed);
        debug_assert!(
            version & 1 == 0,
            "a seqlock write begins from an even, unlocked version (single writer)"
        );
        self.seq.store(version + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        match entry {
            Some((page, frame)) => {
                let file = page.file();
                self.driver.store(file.driver(), Ordering::Relaxed);
                self.file_slot.store(file.slot(), Ordering::Relaxed);
                self.file_generation
                    .store(file.generation(), Ordering::Relaxed);
                self.granule_idx
                    .store(page.granule_idx(), Ordering::Relaxed);
                self.frame.store(frame.get(), Ordering::Relaxed);
                self.occupied.store(true, Ordering::Relaxed);
            }
            None => self.occupied.store(false, Ordering::Relaxed),
        }
        self.seq.store(version + 2, Ordering::Release);
    }

    fn read(&self) -> Option<(PageId, ReadFrameIdx)> {
        for _ in 0..SEQLOCK_READ_SPINS_MAX {
            let before = self.seq.load(Ordering::Acquire);
            if before & 1 != 0 {
                spin_loop();
                continue;
            }
            let occupied = self.occupied.load(Ordering::Relaxed);
            let snapshot = occupied.then(|| {
                let file = FileId::new(
                    self.driver.load(Ordering::Relaxed),
                    self.file_slot.load(Ordering::Relaxed),
                    self.file_generation.load(Ordering::Relaxed),
                );
                let page = PageId::new(file, self.granule_idx.load(Ordering::Relaxed));
                (page, ReadFrameIdx::new(self.frame.load(Ordering::Relaxed)))
            });
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == before {
                return snapshot;
            }
        }
        panic!("seqlock read exceeded its retry bound — a stuck single writer");
    }
}

#[derive(Debug)]
pub struct PageTable {
    slots: Box<[Cell]>,
    capacity: u32,
    mask: u32,
    len: AtomicU32,
}

impl PageTable {
    /// Builds a table sized `2 × frame_count` rounded up to a power of two.
    ///
    /// # Panics
    ///
    /// If `frame_count` is zero, or the power-of-two-rounded doubled capacity
    /// overflows `u32` (`frame_count` above `2^30`).
    #[must_use]
    pub fn with_frame_count(frame_count: u32) -> Self {
        assert!(frame_count > 0, "frame count must be positive");
        let capacity = frame_count
            .checked_mul(2)
            .and_then(u32::checked_next_power_of_two)
            .expect("page-table capacity within u32");
        Self {
            slots: (0..capacity).map(|_| Cell::vacant()).collect(),
            capacity,
            mask: capacity - 1,
            len: AtomicU32::new(0),
        }
    }

    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    #[must_use]
    pub fn len(&self) -> u32 {
        self.len.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The linear-probe home slot for `page` — the deterministic start of its
    /// probe chain, exposed so a collision chain can be constructed in tests.
    #[must_use]
    pub fn home_slot(&self, page: PageId) -> u32 {
        let masked = page_hash(page) & u64::from(self.mask);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "masked below a power-of-two capacity (≤ 2^31), so the value fits u32"
        )]
        let slot = masked as u32;
        slot
    }

    #[must_use]
    pub fn lookup(&self, page: PageId) -> Option<ReadFrameIdx> {
        let capacity = self.capacity();
        let mut slot = self.home_slot(page);
        for _ in 0..capacity {
            debug_assert!(slot <= self.mask, "probe slot within mask");
            match self.slots[slot as usize].read() {
                Some((key, frame)) if key == page => return Some(frame),
                Some(_) => slot = (slot + 1) & self.mask,
                None => return None,
            }
        }
        None
    }

    /// Inserts or updates `page → frame` through an exclusive borrow — the
    /// standalone-table entry point.
    ///
    /// # Panics
    ///
    /// If inserting a new key would exceed the fixed `capacity()`.
    pub fn insert(&mut self, page: PageId, frame: ReadFrameIdx) {
        self.insert_shared(page, frame);
    }

    /// Removes `page` through an exclusive borrow — the standalone-table entry
    /// point.
    pub fn remove(&mut self, page: PageId) -> Option<ReadFrameIdx> {
        self.remove_shared(page)
    }

    /// Inserts or updates `page → frame`. Updating an existing key never grows
    /// the table. Callers serialize inserts under the AD-4 control-plane lock
    /// (single-writer seqlock discipline).
    ///
    /// # Panics
    ///
    /// If inserting a new key would exceed the fixed `capacity()` — the table
    /// never rehashes or grows.
    pub(crate) fn insert_shared(&self, page: PageId, frame: ReadFrameIdx) {
        let capacity = self.capacity();
        let mut slot = self.home_slot(page);
        for _ in 0..capacity {
            debug_assert!(slot <= self.mask, "probe slot within mask");
            match self.slots[slot as usize].read() {
                Some((key, _)) if key == page => {
                    self.slots[slot as usize].write(Some((page, frame)));
                    return;
                }
                Some(_) => slot = (slot + 1) & self.mask,
                None => {
                    debug_assert!(
                        self.len() < capacity,
                        "a new key lands only in a below-capacity table, so the probe reaches this None before the panic"
                    );
                    self.slots[slot as usize].write(Some((page, frame)));
                    self.len.store(self.len() + 1, Ordering::Relaxed);
                    return;
                }
            }
        }
        panic!("page table exceeded fixed capacity {capacity} — no rehash or growth");
    }

    /// Removes `page`, returning its frame if present, and backward-shifts the
    /// tail of its probe chain so no lookup stops early (no tombstones).
    pub(crate) fn remove_shared(&self, page: PageId) -> Option<ReadFrameIdx> {
        let capacity = self.capacity();
        let mut gap = self.home_slot(page);
        let mut removed = None;
        for _ in 0..capacity {
            debug_assert!(gap <= self.mask, "probe slot within mask");
            match self.slots[gap as usize].read() {
                Some((key, frame)) if key == page => {
                    removed = Some(frame);
                    break;
                }
                Some(_) => gap = (gap + 1) & self.mask,
                None => return None,
            }
        }
        removed?;
        self.slots[gap as usize].write(None);
        debug_assert!(
            !self.is_empty(),
            "removing a present key from a non-empty table"
        );
        self.len.store(self.len() - 1, Ordering::Relaxed);
        self.backward_shift(gap);
        removed
    }

    fn backward_shift(&self, mut gap: u32) {
        let capacity = self.capacity();
        let mut scan = (gap + 1) & self.mask;
        for _ in 0..capacity {
            debug_assert!(
                self.slots[gap as usize].read().is_none(),
                "the gap slot is empty"
            );
            let Some((key, _)) = self.slots[scan as usize].read() else {
                return;
            };
            let home = self.home_slot(key);
            if fills_gap(home, gap, scan, self.mask) {
                debug_assert!(
                    gap != scan,
                    "an entry only moves back to an earlier gap on its probe chain"
                );
                self.slots[gap as usize].write(self.slots[scan as usize].read());
                self.slots[scan as usize].write(None);
                gap = scan;
            }
            scan = (scan + 1) & self.mask;
        }
    }
}

/// Whether an entry homed at `home`, currently at `scan`, may move back to fill
/// `gap` — true iff `gap` lies on that entry's probe chain, i.e. `home` is
/// cyclically outside the open interval `(gap, scan]` that must stay contiguous.
fn fills_gap(home: u32, gap: u32, scan: u32, mask: u32) -> bool {
    debug_assert!(
        home <= mask && gap <= mask && scan <= mask,
        "slots within mask"
    );
    let scan_from_gap = scan.wrapping_sub(gap) & mask;
    let home_from_gap = home.wrapping_sub(gap) & mask;
    debug_assert!(
        scan_from_gap >= 1,
        "scan lies strictly after the gap on the probe chain"
    );
    home_from_gap == 0 || home_from_gap > scan_from_gap
}

pub(crate) fn page_hash(page: PageId) -> u64 {
    let file: FileId = page.file();
    let mut hash = 0u64;
    hash = mix(hash ^ file.driver());
    hash = mix(hash ^ u64::from(file.slot()));
    hash = mix(hash ^ u64::from(file.generation()));
    mix(hash ^ u64::from(page.granule_idx()))
}

fn mix(mut word: u64) -> u64 {
    word = (word ^ (word >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    word = (word ^ (word >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    word ^ (word >> 31)
}
