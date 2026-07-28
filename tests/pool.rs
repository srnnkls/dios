//! T006 frame-pool core pins: frame state machine, open-addressed `PageTable`,
//! CLOCK reference bit and second-chance eviction, watermark/config open-fail
//! (INV-9), the GRANULE default contract (AD-6), the relocated `WriteArena`, and
//! per-reader hit/eviction counters. Epoch guards (T007) and the miss path (T008)
//! are out of scope, so nothing here drives `Pool::get`/`poll`/`ready`. The
//! CLOCK store-elision property (no per-frame write on a repeat hit — the DIO-G1
//! hot-path invariant) is deferred to T009's instrumented/loom coverage; here we
//! pin only the `reference()`/`is_referenced()` return contract and sweep behavior.
//!
//! Surface the implementer must expose to this integration target:
//!   `FrameState` (Copy+Eq+Debug) with `advance(self, FrameState) -> FrameState`
//!     (panics on an illegal edge);
//!   `Frames::preallocated(count, granule)` -> `count()`/`frame_bytes(idx)`/`state(idx)`;
//!   `PageTable::with_frame_count(frames)` -> `capacity()`/`len()`/`insert`/`lookup`/`remove`
//!     keyed by `PageId` valued by `ReadFrameIdx`, plus a test-visible
//!     `home_slot(&self, PageId) -> u32` (linear-probe home slot) so a deterministic
//!     collision chain can be constructed; `insert` accepts at least `frames`
//!     distinct keys and asserts (no rehash/growth) before exceeding `capacity()`;
//!   `Clock::with_frame_count(n)` -> `reference(idx) -> bool` (true iff it set a
//!     clear bit), `is_referenced(idx)`, `evict_victim() -> ReadFrameIdx`;
//!   `Pool::builder()` with `frame_count`/`granule`/`max_concurrent_readers`/
//!     `peak_guards_per_reader`/`max_inflight_reads`/`miss_headroom` -> `build()`
//!     -> `Result<Pool, PoolConfigError>` (`BelowWatermark`/`MissHeadroomTooSmall`/
//!     granule variants), `register_reader() -> Result<_, _>`;
//!   `GRANULE_DEFAULT: u32`; `DriverBuilder::write_slots(slot_count)` and
//!   `Driver::write_arena()`;
//!   `WriteSlot: DerefMut<Target = [u8]>`; `ReaderCounters::new()` with
//!     `record_hit(&self)`/`record_eviction(&self)`/`hits()`/`evictions()`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};

use dios::driver::Driver;
use dios::testing::{
    Clock, FrameState, PageTable, ReadFrameIdx, ReaderCounters, TestFrames as Frames,
};
use dios::{DirectIo, FileId, GRANULE_DEFAULT, PageId, Pool, PoolConfigError};

const SECTOR: usize = 4096;

static FILE_SEQ: AtomicU32 = AtomicU32::new(0);

/// The `FileId` is a pure hashable key here, so the opening handle and driver may
/// drop at once without invalidating it.
fn a_file_id() -> FileId {
    let n = FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("dios_pool_t006_{}_{n}.bin", std::process::id()));
    std::fs::write(&path, [0u8; 64]).expect("temp file writable");
    let driver = Driver::builder().build();
    let driver = driver.expect("the test driver initializes");
    driver
        .open(&path, DirectIo::Disabled)
        .expect("open temp file")
        .file_id()
}

fn page(file: FileId, idx: u32) -> PageId {
    PageId::new(file, idx)
}

/// The first `len` pages (by granule index) that share one linear-probe home
/// slot — a provable collision chain, independent of the hash function.
fn colliding_pages(table: &PageTable, file: FileId, len: usize) -> Vec<PageId> {
    let mut buckets: HashMap<u32, Vec<PageId>> = HashMap::new();
    for idx in 0..1_000_000u32 {
        let p = page(file, idx);
        let bucket = buckets.entry(table.home_slot(p)).or_default();
        bucket.push(p);
        if bucket.len() == len {
            return bucket.clone();
        }
    }
    panic!("no collision chain of length {len} found");
}

fn a_page_with_home(table: &PageTable, file: FileId, home: u32, exclude: &[PageId]) -> PageId {
    for idx in 0..1_000_000u32 {
        let p = page(file, idx);
        if table.home_slot(p) == home && !exclude.contains(&p) {
            return p;
        }
    }
    panic!("no additional page hashing to home slot {home}");
}

fn try_pool(
    frame_count: u32,
    readers: u32,
    peak: u32,
    inflight: u32,
    headroom: u32,
) -> Result<Pool, dios::PoolBuildError> {
    Pool::builder()
        .frame_count(frame_count)
        .granule(4096)
        .max_concurrent_readers(readers)
        .peak_guards_per_reader(peak)
        .max_inflight_reads(inflight)
        .miss_headroom(headroom)
        .build()
}

#[test]
fn frame_state_transition_matrix_permits_exactly_the_legal_edges() {
    let states = [
        FrameState::Free,
        FrameState::InFlight,
        FrameState::Resident,
        FrameState::Evicting,
    ];
    let legal = |from, to| {
        matches!(
            (from, to),
            (FrameState::Free, FrameState::InFlight)
                | (
                    FrameState::InFlight,
                    FrameState::Resident | FrameState::Free
                )
                | (FrameState::Resident, FrameState::Evicting)
                | (FrameState::Evicting, FrameState::Free)
        )
    };
    for from in states {
        for to in states {
            let outcome = std::panic::catch_unwind(|| from.advance(to));
            if legal(from, to) {
                assert_eq!(
                    outcome.ok(),
                    Some(to),
                    "legal edge {from:?}->{to:?} advances to the target state"
                );
            } else {
                assert!(outcome.is_err(), "illegal edge {from:?}->{to:?} must panic");
            }
        }
    }
}

#[test]
fn frames_are_preallocated_sector_aligned_and_non_moving() {
    let granule = 8192u32;
    let frames = Frames::preallocated(4, granule);
    assert_eq!(frames.count(), 4);

    let mut bases = HashSet::new();
    for i in 0..frames.count() {
        let idx = ReadFrameIdx::new(i);
        let bytes = frames.frame_bytes(idx);
        assert_eq!(bytes.len(), granule as usize, "a frame spans one granule");
        let base = bytes.as_ptr().addr();
        assert_eq!(base % SECTOR, 0, "frame base is sector-aligned (O_DIRECT)");
        assert_eq!(frames.state(idx), FrameState::Free, "a fresh frame is Free");
        assert_eq!(
            frames.frame_bytes(idx).as_ptr().addr(),
            base,
            "frame base does not move between calls"
        );
        bases.insert(base);
    }
    assert_eq!(bases.len(), 4, "distinct frames occupy distinct addresses");
}

#[test]
fn page_table_capacity_is_two_times_frames_rounded_up_to_a_power_of_two() {
    for (frames, expected) in [(1u32, 2u32), (3, 8), (5, 16), (6, 16), (8, 16), (100, 256)] {
        assert_eq!(
            PageTable::with_frame_count(frames).capacity(),
            expected,
            "capacity for {frames} frames"
        );
    }
}

#[test]
fn page_table_insert_lookup_remove_roundtrip() {
    let file = a_file_id();
    let mut table = PageTable::with_frame_count(8);
    let key = page(file, 42);
    let frame = ReadFrameIdx::new(3);

    assert_eq!(table.lookup(key), None, "absent key resolves to None");
    table.insert(key, frame);
    assert_eq!(table.lookup(key), Some(frame), "inserted key resolves");
    assert_eq!(table.remove(key), Some(frame), "remove returns the mapping");
    assert_eq!(table.lookup(key), None, "removed key no longer resolves");
}

#[test]
fn page_table_insert_updates_an_existing_page_in_place() {
    let file = a_file_id();
    let mut table = PageTable::with_frame_count(8);
    let key = page(file, 7);

    table.insert(key, ReadFrameIdx::new(1));
    table.insert(key, ReadFrameIdx::new(2));
    assert_eq!(
        table.lookup(key),
        Some(ReadFrameIdx::new(2)),
        "re-inserting a page updates its frame without a duplicate entry"
    );
}

#[test]
fn page_table_updates_an_existing_key_at_full_occupancy_without_growing() {
    let file = a_file_id();
    let frames = 8u32;
    let mut table = PageTable::with_frame_count(frames);
    for i in 0..frames {
        table.insert(page(file, i), ReadFrameIdx::new(i));
    }
    assert_eq!(
        table.len(),
        frames,
        "the table is at its documented occupancy bound"
    );

    table.insert(page(file, 3), ReadFrameIdx::new(99));
    assert_eq!(
        table.len(),
        frames,
        "an in-place update at full occupancy adds no entry"
    );
    assert_eq!(
        table.lookup(page(file, 3)),
        Some(ReadFrameIdx::new(99)),
        "the existing key resolves to its new frame after a full-occupancy update"
    );
}

#[test]
fn page_table_backward_shift_delete_keeps_the_chain_tail_resolvable() {
    let file = a_file_id();
    let mut table = PageTable::with_frame_count(8);
    let chain = colliding_pages(&table, file, 3);
    let (head, middle, tail) = (chain[0], chain[1], chain[2]);
    table.insert(head, ReadFrameIdx::new(0));
    table.insert(middle, ReadFrameIdx::new(1));
    table.insert(tail, ReadFrameIdx::new(2));

    assert_eq!(table.remove(middle), Some(ReadFrameIdx::new(1)));
    assert_eq!(
        table.lookup(head),
        Some(ReadFrameIdx::new(0)),
        "chain head resolves"
    );
    assert_eq!(
        table.lookup(tail),
        Some(ReadFrameIdx::new(2)),
        "chain tail still resolves after a mid-chain delete"
    );
    assert_eq!(table.lookup(middle), None, "the removed middle key is gone");
}

#[test]
fn page_table_negative_probe_terminates_at_the_end_of_a_collision_chain() {
    let file = a_file_id();
    let mut table = PageTable::with_frame_count(8);
    let chain = colliding_pages(&table, file, 3);
    for (i, key) in chain.iter().enumerate() {
        table.insert(*key, ReadFrameIdx::new(u32::try_from(i).unwrap()));
    }
    let home = table.home_slot(chain[0]);
    let absent = a_page_with_home(&table, file, home, &chain);
    assert_eq!(
        table.lookup(absent),
        None,
        "a negative probe walks the full chain and terminates at the trailing empty slot"
    );
}

#[test]
fn page_table_backward_shift_respects_home_slots_across_the_wrap() {
    let file = a_file_id();
    let table = PageTable::with_frame_count(8);
    let last = table.capacity() - 1;

    let wrap_head = a_page_with_home(&table, file, last, &[]);
    let wrap_tail = a_page_with_home(&table, file, last, &[wrap_head]);
    let barrier = a_page_with_home(&table, file, 1, &[wrap_head, wrap_tail]);

    let mut table = table;
    table.insert(wrap_head, ReadFrameIdx::new(0));
    table.insert(wrap_tail, ReadFrameIdx::new(1));
    table.insert(barrier, ReadFrameIdx::new(2));

    assert_eq!(table.remove(wrap_head), Some(ReadFrameIdx::new(0)));

    assert_eq!(
        table.lookup(wrap_tail),
        Some(ReadFrameIdx::new(1)),
        "the wrapped entry shifts back to its home at the last slot and stays findable"
    );
    assert_eq!(
        table.lookup(barrier),
        Some(ReadFrameIdx::new(2)),
        "a naive shift-everything delete pulls the barrier from slot 1 across its own home into slot 0, stranding it; backward-shift must stop at its home slot"
    );
    assert_eq!(table.lookup(wrap_head), None, "the removed head is gone");
}

#[test]
fn page_table_refuses_growth_beyond_capacity_no_rehash() {
    let file = a_file_id();
    let frames = 8u32;
    let mut table = PageTable::with_frame_count(frames);
    let capacity = table.capacity();

    let mut inserted = 0u32;
    for i in 0..=capacity {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            table.insert(page(file, i), ReadFrameIdx::new(0));
        }));
        if outcome.is_err() {
            break;
        }
        inserted += 1;
    }

    assert!(
        inserted >= frames,
        "the table must accept at least frame_count ({frames}) live entries, accepted {inserted}"
    );
    assert!(
        inserted <= capacity,
        "the table asserts before exceeding fixed capacity ({capacity}) — no rehash/growth, accepted {inserted}"
    );
}

#[test]
fn clock_reference_reports_first_set_and_is_idempotent_on_repeat() {
    let clock = Clock::with_frame_count(4);
    let idx = ReadFrameIdx::new(1);

    assert!(
        clock.reference(idx),
        "first touch on a clear bit reports a set"
    );
    assert!(clock.is_referenced(idx), "the bit is now set");
    assert!(
        !clock.reference(idx),
        "a repeat touch on a set bit reports no set (idempotent)"
    );
    assert!(clock.is_referenced(idx), "the bit stays set");
}

#[test]
fn clock_only_unreferenced_frame_is_the_victim() {
    let mut clock = Clock::with_frame_count(4);
    for i in [0u32, 1, 3] {
        assert!(clock.reference(ReadFrameIdx::new(i)));
    }
    assert_eq!(
        clock.evict_victim(),
        ReadFrameIdx::new(2),
        "the lone unreferenced frame is evicted regardless of hand position"
    );
}

#[test]
fn clock_gives_a_referenced_frame_a_second_chance() {
    let mut clock = Clock::with_frame_count(4);
    let all: HashSet<ReadFrameIdx> = (0..4).map(ReadFrameIdx::new).collect();
    for &idx in &all {
        assert!(clock.reference(idx));
    }
    let victim = clock.evict_victim();
    assert!(all.contains(&victim), "the victim is one of the frames");
    for &idx in &all {
        assert!(
            !clock.is_referenced(idx),
            "one full sweep clears every reference bit (second chance consumed)"
        );
    }
}

#[test]
fn clock_eviction_order_is_deterministic_for_a_constructed_state() {
    let victims_of = || {
        let mut clock = Clock::with_frame_count(4);
        (0..4).map(|_| clock.evict_victim()).collect::<Vec<_>>()
    };
    let first = victims_of();
    assert_eq!(first, victims_of(), "identical state evicts identically");
    let distinct: HashSet<ReadFrameIdx> = first.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        4,
        "one no-reference rotation visits every frame"
    );
}

#[test]
fn watermark_is_readers_times_peak_plus_configured_headroom() {
    assert!(
        try_pool(16, 3, 4, 1, 5).is_err(),
        "16 < 3*4+5=17 fails open"
    );
    assert!(
        try_pool(17, 3, 4, 1, 5).is_ok(),
        "17 == 3*4+5 watermark builds"
    );

    assert!(
        try_pool(15, 2, 3, 2, 10).is_err(),
        "15 < 2*3+10=16 fails open; a 3*inflight substitution would wrongly admit it"
    );
    assert!(
        try_pool(16, 2, 3, 2, 10).is_ok(),
        "16 == 2*3+10 watermark builds"
    );
}

#[test]
fn pool_below_watermark_fails_open_with_a_typed_config_error() {
    let err = try_pool(11, 2, 3, 2, 6).expect_err("11 < 2*3+6=12 must fail open");
    assert!(
        matches!(
            err,
            dios::PoolBuildError::Configuration(PoolConfigError::BelowWatermark { .. })
        ),
        "open-fail is a typed config error, not a runtime deadlock: {err:?}"
    );
}

#[test]
fn pool_miss_headroom_below_three_times_inflight_is_rejected() {
    let err = try_pool(100, 2, 3, 2, 5).expect_err("headroom 5 < 3*inflight(2)=6 must fail");
    assert!(
        matches!(
            err,
            dios::PoolBuildError::Configuration(PoolConfigError::MissHeadroomTooSmall { .. })
        ),
        "insufficient miss headroom is a typed config error: {err:?}"
    );
}

#[test]
fn granule_default_is_a_power_of_two_at_least_one_sector() {
    assert!(
        GRANULE_DEFAULT.is_power_of_two(),
        "GRANULE default must be a power of two, got {GRANULE_DEFAULT}"
    );
    assert!(
        GRANULE_DEFAULT as usize >= SECTOR,
        "GRANULE default must be at least one 4096-byte sector, got {GRANULE_DEFAULT}"
    );
}

#[test]
fn pool_rejects_a_non_power_of_two_granule() {
    Pool::builder()
        .frame_count(64)
        .granule(6144)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .build()
        .expect_err("a non-power-of-two granule is invalid");
}

#[test]
fn pool_rejects_a_granule_below_the_sector_floor() {
    Pool::builder()
        .frame_count(64)
        .granule(256)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .build()
        .expect_err("a sub-sector granule is invalid");
}

#[test]
fn pool_accepts_a_valid_granule() {
    let pool = Pool::builder()
        .frame_count(64)
        .granule(8192)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .build();
    assert!(
        pool.is_ok(),
        "a power-of-two granule at/above the floor is accepted"
    );
}

#[test]
fn write_arena_allocs_to_capacity_then_exhausts_then_frees_on_drop() {
    let driver = Driver::builder()
        .queue_capacity(2)
        .write_slots(2)
        .build()
        .expect("driver init");
    let arena = driver.write_arena();
    let a = arena.alloc().expect("first slot");
    let b = arena.alloc().expect("second slot");
    assert!(
        arena.alloc().is_none(),
        "a two-slot arena exhausts after two"
    );
    drop(a);
    assert!(
        arena.alloc().is_some(),
        "dropping a slot returns it to the free set"
    );
    drop(b);
}

#[test]
fn write_slot_is_sector_aligned_granule_sized_and_deref_mut_roundtrips() {
    let granule = 4096u32;
    let driver = Driver::builder()
        .frame_bytes(granule)
        .write_slots(1)
        .build()
        .expect("driver init");
    let arena = driver.write_arena();
    let mut slot = arena.alloc().expect("a slot");

    assert_eq!(
        slot.len(),
        granule as usize,
        "a staging slot spans one granule"
    );
    assert_eq!(
        slot.as_ptr().addr() % SECTOR,
        0,
        "a staging slot base is sector-aligned (O_DIRECT)"
    );

    slot[0] = 0xAB;
    slot[granule as usize - 1] = 0xCD;
    assert_eq!(slot[0], 0xAB, "DerefMut writes are observable");
    assert_eq!(slot[granule as usize - 1], 0xCD);
}

#[test]
fn reader_counters_count_hits_and_evictions_locally() {
    let counters = ReaderCounters::new();
    for _ in 0..5 {
        counters.record_hit();
    }
    for _ in 0..2 {
        counters.record_eviction();
    }
    assert_eq!(counters.hits(), 5, "N local hits are observable as N");
    assert_eq!(
        counters.evictions(),
        2,
        "M local evictions are observable as M"
    );
}

#[test]
fn reader_counters_are_independent_across_readers() {
    let a = ReaderCounters::new();
    let b = ReaderCounters::new();
    for _ in 0..3 {
        a.record_hit();
    }
    assert_eq!(a.hits(), 3);
    assert_eq!(b.hits(), 0, "a second reader's counter is untouched");
}

#[test]
fn a_watermark_sized_pool_registers_a_reader() {
    let pool = try_pool(64, 1, 1, 1, 3).expect("watermark-satisfying config builds");
    assert!(
        pool.register_reader().is_ok(),
        "the first reader slot is available"
    );
}
