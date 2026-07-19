//! T007 epoch pin-guard (EBR) pins — executes the design.md "Epoch reclamation
//! (EBR) — algorithm" section single-threaded. RED today because it names the
//! narrow T007 seams on `Pool` that do not exist yet; these seams are the
//! substrate the T008 `get()`/`poll()`/`ready()` entry points compose over.
//!
//! Seams this file defines (all provisional `#[doc(hidden)]` on `Pool`, sealed or
//! subsumed by T008):
//!   - `insert_resident_frame(&self, PageId, u8) -> ReadFrameIdx` — test substrate:
//!     claim a Free frame, fill its granule, mark it Resident, map the page. Stands
//!     in for the miss-completion path (T008) so a hit can be set up in isolation.
//!   - `pin(&self, &ReaderCtx, PageId) -> Option<FrameGuard<'_>>` — the hit-path
//!     guard mint: publish `local_epoch` BEFORE validating Resident + mapped, then
//!     hand back a guard, or `None` on an Evicting/removed page. Substrate of the
//!     T008 `get()` Hit arm.
//!   - `evict_frame(&self, PageId) -> ReadFrameIdx` — Resident -> Evicting, remove
//!     the table mapping, enqueue tagged with the current global epoch. Isolates the
//!     epoch tagging from T006's CLOCK victim selection (`Clock::evict_victim`) and
//!     the T008 busy-path wiring.
//!   - `poll(&self) -> usize` — the poll-boundary advance/reclaim pass: advance the
//!     global epoch iff every registered reader is quiescent or at the current epoch,
//!     then reclaim Evicting frames whose tag + 2 <= global (Evicting -> Free).
//!     Consistent with the contract `Pool::poll`; T008 extends it with driver drain.
//!   - `frame_state(&self, ReadFrameIdx) -> FrameState` — observation seam.
//!
//! Boundaries: publish-before-validate is pinned only for its single-threaded
//! observable (an evicted page never re-pins); the full probe/publish/evict
//! INTERLEAVING proof is T009 loom. The DIO-G1 warm-hit no-RMW/zero-alloc proof is
//! also T009 — here we pin only the observable shape (nested guards share one epoch,
//! release on the last drop; the epoch advances at poll boundaries, not per pin).

use std::sync::atomic::{AtomicU32, Ordering};

use dios::driver::Driver;
use dios::testing::{FrameState, PoolTestingExt};
use dios::{DirectIo, FileId, PageId, Pool};

static FILE_SEQ: AtomicU32 = AtomicU32::new(0);

const GRANULE: usize = 4096;

/// A pure hashable `FileId` for `PageId` keys — the opening handle may drop at
/// once, the id stays a valid table key (mirrors the `tests/pool.rs` helper).
fn a_file_id() -> FileId {
    let n = FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("dios_epoch_t007_{}_{n}.bin", std::process::id()));
    std::fs::write(&path, [0u8; 64]).expect("temp file writable");
    let driver = Driver::builder().build();
    let driver = driver.expect("the test driver initializes");
    driver
        .open(&path, DirectIo::Disabled)
        .expect("open temp file")
        .file_id()
}

/// A pool with `max_readers` slots, granule 4096, watermark-satisfying.
fn epoch_pool(max_readers: u32) -> Pool {
    let peak = 2u32;
    let inflight = 1u32;
    let headroom = 3u32;
    let watermark = max_readers * peak + headroom;
    Pool::builder()
        .frame_count(watermark + 8)
        .granule(4096)
        .max_concurrent_readers(max_readers)
        .peak_guards_per_reader(peak)
        .max_inflight_reads(inflight)
        .miss_headroom(headroom)
        .build()
        .expect("watermark-satisfying config builds")
}

#[test]
fn pin_of_a_resident_frame_borrows_the_whole_granule_of_its_content() {
    let pool = epoch_pool(1);
    let reader = pool.register_reader().expect("a reader slot");
    let page = PageId::new(a_file_id(), 7);

    let frame = pool.insert_resident_frame(page, 0xC3);
    assert_eq!(
        pool.frame_state(frame),
        FrameState::Resident,
        "an inserted frame is Resident"
    );

    let guard = pool
        .pin(&reader, page)
        .expect("a Resident, mapped page pins");
    assert_eq!(guard.len(), GRANULE, "a guard borrows the whole granule");
    assert!(
        guard.iter().all(|&b| b == 0xC3),
        "the guard sees this page's content, not another frame's"
    );
}

#[test]
fn a_held_guard_keeps_its_frame_unreclaimed_and_its_bytes_stable_across_polls() {
    let pool = epoch_pool(1);
    let reader = pool.register_reader().expect("a reader slot");
    let page = PageId::new(a_file_id(), 3);

    let frame = pool.insert_resident_frame(page, 0xAB);
    let guard = pool.pin(&reader, page).expect("resident page pins");

    let evicted = pool.evict_frame(page);
    assert_eq!(
        evicted, frame,
        "evicting the page targets its resident frame"
    );
    assert_eq!(
        pool.frame_state(frame),
        FrameState::Evicting,
        "eviction moves the frame to Evicting, not straight to Free"
    );

    for _ in 0..8u32 {
        let reclaimed = pool.poll();
        let _ = reclaimed;
        assert_eq!(
            pool.frame_state(frame),
            FrameState::Evicting,
            "a frame pinned by a live guard stays in Evicting limbo, never reclaimed, however many polls run"
        );
        assert_eq!(
            guard[0], 0xAB,
            "held bytes stay stable across an unrelated poll"
        );
        assert_eq!(
            guard[GRANULE - 1],
            0xAB,
            "the whole held granule stays stable"
        );
    }

    drop(guard);
    let mut freed = false;
    for _ in 0..4u32 {
        pool.poll();
        if pool.frame_state(frame) == FrameState::Free {
            freed = true;
            break;
        }
    }
    assert!(
        freed,
        "reclamation resumes once the pinning guard drops and the epoch advances"
    );
}

#[test]
fn reclamation_needs_two_epoch_advances_after_the_last_guard_drop() {
    let pool = epoch_pool(1);
    let reader = pool.register_reader().expect("a reader slot");
    let page = PageId::new(a_file_id(), 11);

    let frame = pool.insert_resident_frame(page, 0x5A);
    let guard = pool.pin(&reader, page).expect("resident page pins");
    let evicted = pool.evict_frame(page);
    assert_eq!(evicted, frame);

    drop(guard);
    assert_eq!(
        pool.frame_state(frame),
        FrameState::Evicting,
        "before any advance the evicted frame sits in Evicting limbo"
    );

    pool.poll();
    assert_eq!(
        pool.frame_state(frame),
        FrameState::Evicting,
        "one epoch advance is NOT enough: tag + 2 has not been reached"
    );

    pool.poll();
    assert_eq!(
        pool.frame_state(frame),
        FrameState::Free,
        "the second epoch advance reaches tag + 2 and reclaims the frame"
    );
}

#[test]
fn a_stalled_reader_blocks_reclamation_of_its_epochs_frames_until_it_drops() {
    let pool = epoch_pool(2);
    let stalled = pool.register_reader().expect("the stalled reader slot");
    let quiescent = pool
        .register_reader()
        .expect("a second, quiescent reader slot");
    let page = PageId::new(a_file_id(), 21);

    let frame = pool.insert_resident_frame(page, 0x77);
    let guard = pool.pin(&stalled, page).expect("resident page pins");
    let evicted = pool.evict_frame(page);
    assert_eq!(evicted, frame);

    for _ in 0..8u32 {
        pool.poll();
        assert_eq!(
            pool.frame_state(frame),
            FrameState::Evicting,
            "an unadvanced reader stalls global-epoch progress past tag + 1, so its \
             epoch's evicted frame stays in Evicting while it holds the guard"
        );
    }

    let _ = &quiescent;
    drop(guard);
    let mut freed = false;
    for _ in 0..4u32 {
        pool.poll();
        if pool.frame_state(frame) == FrameState::Free {
            freed = true;
            break;
        }
    }
    assert!(
        freed,
        "once the stalled reader releases its epoch, reclamation resumes"
    );
}

#[test]
fn an_evicted_page_never_mints_a_guard_over_its_reclaimable_bytes() {
    let pool = epoch_pool(1);
    let reader = pool.register_reader().expect("a reader slot");
    let page = PageId::new(a_file_id(), 30);

    pool.insert_resident_frame(page, 0x42);
    assert!(
        pool.pin(&reader, page).is_some(),
        "a Resident, mapped page pins before eviction"
    );

    pool.evict_frame(page);
    assert!(
        pool.pin(&reader, page).is_none(),
        "publish-before-validate: once the mapping is removed at evict, a fresh pin \
         observes Evicting/no-mapping and takes the miss path — it never hands back \
         the reclaimable frame's bytes"
    );
}

#[test]
fn nested_guards_share_one_epoch_and_release_only_on_the_last_drop() {
    let pool = epoch_pool(1);
    let reader = pool.register_reader().expect("a reader slot");
    let page = PageId::new(a_file_id(), 40);

    let frame = pool.insert_resident_frame(page, 0x9E);
    let outer = pool.pin(&reader, page).expect("first guard");
    let inner = pool
        .pin(&reader, page)
        .expect("nested guard shares the epoch");

    let evicted = pool.evict_frame(page);
    assert_eq!(evicted, frame);

    drop(outer);
    for _ in 0..4u32 {
        pool.poll();
        assert_eq!(
            pool.frame_state(frame),
            FrameState::Evicting,
            "the reader's epoch is still published while a nested guard lives, so the \
             frame stays Evicting after only the outer guard drops"
        );
        assert_eq!(
            inner[0], 0x9E,
            "the surviving nested guard's bytes stay stable"
        );
    }

    drop(inner);
    let mut freed = false;
    for _ in 0..4u32 {
        pool.poll();
        if pool.frame_state(frame) == FrameState::Free {
            freed = true;
            break;
        }
    }
    assert!(
        freed,
        "dropping the last of the nested guards releases the epoch and lets reclamation finish"
    );
}

#[test]
fn the_global_epoch_advances_at_poll_boundaries_not_per_pin() {
    let pool = epoch_pool(1);
    let reader = pool.register_reader().expect("a reader slot");
    let file = a_file_id();

    let resident = PageId::new(file, 50);
    let victim = PageId::new(file, 51);
    let victim_frame = pool.insert_resident_frame(victim, 0x01);
    pool.insert_resident_frame(resident, 0x02);
    pool.evict_frame(victim);

    for _ in 0..16u32 {
        let guard = pool
            .pin(&reader, resident)
            .expect("the resident page keeps hitting");
        assert_eq!(
            guard[0], 0x02,
            "repeat warm hits return the resident content"
        );
        drop(guard);
        assert_ne!(
            pool.frame_state(victim_frame),
            FrameState::Free,
            "with no poll in this loop the victim cannot be reclaimed whatever the epoch; \
             the per-pin-vs-per-poll distinction is pinned by the two polls after the loop"
        );
    }

    pool.poll();
    assert_eq!(
        pool.frame_state(victim_frame),
        FrameState::Evicting,
        "the first poll is the FIRST epoch advance (tag 0 -> global 1); an implementation \
         that advanced per pin would have global >= 16 and reclaim the victim on this very \
         poll, so Evicting proves the 16 warm pins moved the epoch by nothing"
    );
    pool.poll();
    assert_eq!(
        pool.frame_state(victim_frame),
        FrameState::Free,
        "the second poll reaches tag + 2 and reclaims — exactly two poll-boundary advances, \
         no per-pin churn (DIO-G1)"
    );
}

#[test]
fn a_nested_pin_taken_at_an_advanced_epoch_does_not_republish_the_readers_local_epoch() {
    let pool = epoch_pool(1);
    let reader = pool.register_reader().expect("a reader slot");
    let file = a_file_id();
    let page_a = PageId::new(file, 60);
    let page_b = PageId::new(file, 61);

    let frame_a = pool.insert_resident_frame(page_a, 0xA0);
    let outer = pool
        .pin(&reader, page_a)
        .expect("page A pins, publishing the reader's local epoch as 0");
    let evicted = pool.evict_frame(page_a);
    assert_eq!(
        evicted, frame_a,
        "evicting A targets its resident frame, tagged at the current global epoch 0"
    );

    pool.poll();
    assert_eq!(
        pool.frame_state(frame_a),
        FrameState::Evicting,
        "the first poll advances global to 1 (local == global permits it); A's tag + 2 = 2 \
         has not been reached, so A is still Evicting"
    );

    pool.insert_resident_frame(page_b, 0xB0);
    let inner = pool
        .pin(&reader, page_b)
        .expect("page B pins as a nested guard while global is 1");

    pool.poll();
    assert_eq!(
        pool.frame_state(frame_a),
        FrameState::Evicting,
        "the nested pin shares the epoch already published as 0 (per-thread guard count), so \
         local(0) != global(1) blocks the advance and A stays Evicting — a republish to \
         epoch 1 would let global reach 2 and free A while the outer guard still borrows it"
    );

    drop(outer);
    pool.poll();
    assert_eq!(
        pool.frame_state(frame_a),
        FrameState::Evicting,
        "dropping only the outer guard leaves the nested guard live, so the reader's epoch \
         stays published at 0 and A is not reclaimed"
    );

    drop(inner);
    let mut freed = false;
    for _ in 0..4u32 {
        pool.poll();
        if pool.frame_state(frame_a) == FrameState::Free {
            freed = true;
            break;
        }
    }
    assert!(
        freed,
        "with the last nested guard dropped the reader goes quiescent, global reaches A's \
         tag + 2, and A is finally reclaimed"
    );
}
