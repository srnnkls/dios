#![cfg(feature = "mock")]

use std::mem::size_of;
use std::path::Path;

use dios::testing::{
    FrameState, MockDriver, MockPoolTestingExt, MockResidentLeaseCountObservation,
    PoolBuilderTestingExt, PoolTestingExt, ReadFrameIdx, TestFrames,
};
use dios::{
    DirectIo, FileId, Get, GetError, PageId, Pool, ReaderCtx, ResidentFileLease, ResidentHint,
    ResidentLeaseError, RetireStatus,
};

const FRAME_COUNT: u32 = 4;
const GRANULE: u32 = 4096;
const POLL_BOUND: u32 = 32;

fn pool_with_file(name: &str) -> (Pool<MockDriver>, FileId) {
    pool_with_file_capacity(name, FRAME_COUNT, 1)
}

fn pool_with_file_capacity(
    name: &str,
    frame_count: u32,
    peak_guards_per_reader: u32,
) -> (Pool<MockDriver>, FileId) {
    let driver = MockDriver::builder()
        .queue_capacity(1)
        .frames(frame_count)
        .frame_bytes(GRANULE)
        .build();
    let file = driver
        .open(Path::new(name), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    let pool = Pool::builder()
        .frame_count(frame_count)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(peak_guards_per_reader)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .build_on(driver)
        .expect("valid resident-lease pool");
    pool.register_file(file);
    (pool, file_id)
}

fn minted_hint(pool: &Pool<MockDriver>, lease: &ResidentFileLease, page: PageId) -> ResidentHint {
    pool.resident_hint(lease, page)
        .expect("a resident exact page mints an opaque advisory hint")
}

fn assert_hit_bytes(outcome: Result<Get<'_>, GetError>, expected: u8, message: &str) {
    match outcome.expect("the supplied lease remains live") {
        Get::Hit(guard) => {
            assert!(guard.iter().all(|&byte| byte == expected), "{message}");
        }
        Get::Pending(_) => panic!("{message}: a resident ordinary fallback must hit"),
        Get::Busy => panic!("{message}: a resident ordinary fallback is never Busy"),
    }
}

fn assert_pending_then_ready(
    pool: &Pool<MockDriver>,
    reader: &ReaderCtx,
    outcome: Result<Get<'_>, GetError>,
    expected: u8,
    message: &str,
) {
    let mut token = match outcome.expect("the supplied lease remains live") {
        Get::Pending(token) => token,
        Get::Hit(_) => panic!("{message}: an unrelated resident frame must not authorize a hit"),
        Get::Busy => panic!("{message}: a cold ordinary fallback is within the fixed capacity"),
    };

    for _ in 0..POLL_BOUND {
        match pool.ready(reader, token) {
            dios::ReadyResult::Ready(guard) => {
                assert!(guard.iter().all(|&byte| byte == expected), "{message}");
                return;
            }
            dios::ReadyResult::NotYet(returned) => {
                token = returned;
                pool.poll();
            }
            dios::ReadyResult::Err(error) => {
                panic!("{message}: fault-free fallback failed: {error:?}")
            }
        }
    }
    panic!("{message}: ordinary fallback did not become ready within the poll bound");
}

fn same_granule_source_hints(
    pool: &Pool<MockDriver>,
    first_file: FileId,
) -> Vec<(ReadFrameIdx, ResidentHint)> {
    let mut files = vec![first_file];
    for name in [
        "resident-hint-source-one",
        "resident-hint-source-two",
        "resident-hint-source-three",
        "resident-hint-source-four",
        "resident-hint-source-five",
    ] {
        let file = pool
            .driver()
            .open(Path::new(name), DirectIo::Disabled)
            .expect("additional source mock file");
        let file_id = file.file_id();
        pool.register_file(file);
        files.push(file_id);
    }

    let mut hints = Vec::new();
    for file in files {
        let page = PageId::new(file, 0);
        let frame = pool.insert_resident_frame(page, 0x60);
        let lease = pool.lease_file(file).expect("source pool file lease");
        hints.push((frame, minted_hint(pool, &lease, page)));
    }
    hints
}

fn assert_stale(result: &Result<ResidentFileLease, ResidentLeaseError>, file: FileId) {
    assert!(
        matches!(result, Err(ResidentLeaseError::StaleFile { file: stale }) if *stale == file),
        "the exact unavailable generation must be returned as typed stale"
    );
}

fn poll_until_closed(pool: &Pool<MockDriver>, file: FileId) {
    for _ in 0..POLL_BOUND {
        if pool.driver().is_closed(file) {
            return;
        }
        pool.poll();
    }
    assert!(
        pool.driver().is_closed(file),
        "last lease drop wakes bounded retirement progress"
    );
}

fn lease_after_pool_drop() -> (ResidentFileLease, MockResidentLeaseCountObservation) {
    let (pool, file) = pool_with_file("resident-lease-pool-first-drop");
    let observation = pool.observe_resident_lease_count(file);
    assert_eq!(observation.count(), 0);
    let lease = pool
        .lease_file(file)
        .expect("a live registered file admits one owned lease");
    assert_eq!(observation.count(), 1);
    drop(pool);
    (lease, observation)
}

#[test]
fn a_live_file_lease_is_owned_and_safe_to_drop_after_the_pool() {
    let observation = {
        let (lease, observation) = lease_after_pool_drop();
        assert_eq!(observation.count(), 1);
        std::hint::black_box(lease);
        observation
    };
    assert_eq!(observation.count(), 0);
}

#[test]
fn a_lease_serializes_retirement_and_same_slot_reopen() {
    let (pool, old_file) = pool_with_file("resident-lease-retire-old");
    let old_page = PageId::new(old_file, 0);
    let resident_frame = pool.insert_resident_frame(old_page, 0x61);
    let unregistered = pool
        .driver()
        .open(Path::new("resident-lease-absent"), DirectIo::Disabled)
        .expect("an unregistered backend file provides an absent pool generation");
    let absent_file = unregistered.file_id();

    assert_stale(&pool.lease_file(absent_file), absent_file);
    assert_eq!(pool.resident_lease_count(old_file), 0);
    {
        let _lease = pool
            .lease_file(old_file)
            .expect("a live registered generation admits a lease");
        assert_eq!(pool.resident_lease_count(old_file), 1);
        assert_eq!(pool.retire_file(old_file), RetireStatus::Retiring);
        assert_stale(&pool.lease_file(old_file), old_file);

        for _ in 0..POLL_BOUND {
            pool.poll();
            assert_eq!(
                pool.frame_state(resident_frame),
                FrameState::Resident,
                "a live lease delays file-frame retirement"
            );
            assert!(
                !pool.driver().is_closed(old_file),
                "a live lease delays physical close and slot reuse"
            );
        }
    }

    assert_eq!(pool.resident_lease_count(old_file), 0);
    poll_until_closed(&pool, old_file);
    assert_eq!(pool.retire_file(old_file), RetireStatus::Retired);
    assert_stale(&pool.lease_file(old_file), old_file);

    let reopened = pool
        .open(Path::new("resident-lease-retire-new"), DirectIo::Disabled)
        .expect("the released backend slot can reopen");
    assert!(old_file.aliases_slot(&reopened));
    assert_ne!(old_file, reopened);
    assert_stale(&pool.lease_file(old_file), old_file);
    let _new_lease = pool
        .lease_file(reopened)
        .expect("the later generation in the same slot admits a fresh lease");
}

#[test]
fn lease_count_exhaustion_is_typed_and_does_not_mutate_the_count() {
    let (pool, file) = pool_with_file("resident-lease-exhausted");
    pool.set_resident_lease_count(file, u32::MAX);

    assert!(
        matches!(
            pool.lease_file(file),
            Err(ResidentLeaseError::Exhausted { file: exhausted }) if exhausted == file
        ),
        "the bounded lease count reports exact typed exhaustion"
    );
    assert_eq!(
        pool.resident_lease_count(file),
        u32::MAX,
        "rejected acquisition must not wrap or otherwise mutate the count"
    );
}

#[test]
fn packed_frame_word_changes_generation_only_when_residency_is_published() {
    const STATE_MASK: u64 = 0b11;
    const GENERATION_SHIFT: u32 = 2;
    let frames = TestFrames::preallocated(2, GRANULE);
    let refill = ReadFrameIdx::new(0);
    let cancelled = ReadFrameIdx::new(1);
    let free_word = frames.state_word(refill);
    let initial_generation = free_word >> GENERATION_SHIFT;
    frames.advance(refill, FrameState::InFlight);
    let first_inflight_word = frames.state_word(refill);
    assert_eq!(first_inflight_word >> GENERATION_SHIFT, initial_generation);
    frames.advance(refill, FrameState::Resident);
    let first_resident_word = frames.state_word(refill);
    assert_eq!(
        first_resident_word >> GENERATION_SHIFT,
        initial_generation + 1
    );
    frames.advance(refill, FrameState::Evicting);
    let first_evicting_word = frames.state_word(refill);
    assert_eq!(
        first_evicting_word >> GENERATION_SHIFT,
        initial_generation + 1
    );
    frames.advance(refill, FrameState::Free);
    let reclaimed_word = frames.state_word(refill);
    assert_eq!(reclaimed_word >> GENERATION_SHIFT, initial_generation + 1);
    frames.advance(refill, FrameState::InFlight);
    let second_inflight_word = frames.state_word(refill);
    assert_eq!(
        second_inflight_word >> GENERATION_SHIFT,
        initial_generation + 1
    );
    frames.advance(refill, FrameState::Resident);
    let second_resident_word = frames.state_word(refill);
    assert_eq!(
        second_resident_word >> GENERATION_SHIFT,
        initial_generation + 2
    );

    let cancelled_generation = frames.state_word(cancelled) >> GENERATION_SHIFT;
    frames.advance(cancelled, FrameState::InFlight);
    frames.advance(cancelled, FrameState::Free);
    assert_eq!(
        frames.state_word(cancelled) >> GENERATION_SHIFT,
        cancelled_generation,
        "a cancelled fill preserves the residency generation"
    );

    let tags = [
        free_word & STATE_MASK,
        first_inflight_word & STATE_MASK,
        first_resident_word & STATE_MASK,
        first_evicting_word & STATE_MASK,
    ];
    for (index, tag) in tags.iter().enumerate() {
        assert!(
            !tags[..index].contains(tag),
            "each legal FrameState has one distinct two-bit tag"
        );
    }
    assert_eq!(reclaimed_word & STATE_MASK, free_word & STATE_MASK);
    assert_eq!(
        second_resident_word & STATE_MASK,
        first_resident_word & STATE_MASK
    );
}

#[test]
fn an_opaque_resident_hint_is_niche_sized_and_mints_a_normal_guard() {
    assert_eq!(size_of::<ResidentHint>(), 16);
    assert_eq!(size_of::<Option<ResidentHint>>(), 16);

    let (pool, file) = pool_with_file("resident-hint-mint-hit");
    let reader = pool.register_reader().expect("one reader slot");
    let page = PageId::new(file, 3);
    pool.insert_resident_frame(page, 0xA3);
    let lease = pool
        .lease_file(file)
        .expect("a live file admits an exact generation lease");
    let hint = minted_hint(&pool, &lease, page);

    assert_hit_bytes(
        pool.get_with_hint(&reader, &lease, page, Some(hint)),
        0xA3,
        "a matching opaque hint returns the ordinary guard over its exact page",
    );
}

#[test]
fn stale_and_missing_hints_fall_back_then_a_refreshed_hint_hits() {
    let (pool, source_file) = pool_with_file("resident-hint-stale-refresh-source");
    let target_file = pool
        .driver()
        .open(
            Path::new("resident-hint-stale-refresh-target"),
            DirectIo::Disabled,
        )
        .expect("target mock file");
    let target_id = target_file.file_id();
    pool.register_file(target_file);
    let reader = pool.register_reader().expect("one reader slot");
    let stale_page = PageId::new(source_file, 0);
    let refreshed_page = PageId::new(target_id, 0);
    pool.insert_resident_frame(stale_page, 0x31);
    let source_lease = pool.lease_file(source_file).expect("source file lease");
    let target_lease = pool.lease_file(target_id).expect("target file lease");
    let stale_hint = minted_hint(&pool, &source_lease, stale_page);

    pool.evict_frame(stale_page);
    pool.poll();
    pool.poll();
    let refreshed_frame = pool.insert_resident_frame(refreshed_page, 0x72);

    assert_hit_bytes(
        pool.get_with_hint(&reader, &target_lease, refreshed_page, Some(stale_hint)),
        0x72,
        "a stale same-granule stamp must take ordinary get instead of reading another file's bytes",
    );
    assert_hit_bytes(
        pool.get_with_hint(&reader, &target_lease, refreshed_page, None),
        0x72,
        "a missing hint must take the same ordinary hit path",
    );

    let refreshed_hint = minted_hint(&pool, &target_lease, refreshed_page);
    assert_hit_bytes(
        pool.get_with_hint(&reader, &target_lease, refreshed_page, Some(refreshed_hint)),
        0x72,
        "ordinary fallback permits a newly minted hint for the current residency",
    );
    pool.evict_frame(refreshed_page);
    pool.poll();
    pool.poll();
    assert_eq!(
        pool.frame_state(refreshed_frame),
        FrameState::Free,
        "a stale-hint fallback with no returned guard leaves no phantom epoch pin"
    );
}

#[test]
fn a_wrong_granule_hint_falls_back_without_releasing_an_outer_guard() {
    let (pool, file) = pool_with_file_capacity("resident-hint-nested-fallback", 5, 2);
    let reader = pool.register_reader().expect("one reader slot");
    let outer_page = PageId::new(file, 0);
    let fallback_page = PageId::new(file, 1);
    pool.insert_resident_frame(outer_page, 0x40);
    pool.insert_resident_frame(fallback_page, 0x41);
    let lease = pool
        .lease_file(file)
        .expect("a live file admits an exact generation lease");
    let wrong_granule_hint = minted_hint(&pool, &lease, outer_page);
    let outer = match pool
        .get(&reader, outer_page)
        .expect("the outer page remains live")
    {
        Get::Hit(guard) => guard,
        Get::Pending(_) => panic!("the inserted outer page must hit"),
        Get::Busy => panic!("the inserted outer page is never Busy"),
    };

    assert_hit_bytes(
        pool.get_with_hint(&reader, &lease, fallback_page, Some(wrong_granule_hint)),
        0x41,
        "a wrong-granule hint falls back through a nested normal guard",
    );
    assert!(
        outer.iter().all(|&byte| byte == 0x40),
        "the failed nested hint path must leave the outer epoch guard live"
    );
}

#[test]
fn a_same_granule_hint_from_another_file_falls_back_to_that_files_bytes() {
    let (pool, source_file) = pool_with_file("resident-hint-wrong-file-source");
    let target_file = pool
        .driver()
        .open(
            Path::new("resident-hint-wrong-file-target"),
            DirectIo::Disabled,
        )
        .expect("second mock file");
    let target_id = target_file.file_id();
    pool.driver().seed_page(&target_file, 0, 0xB0);
    pool.register_file(target_file);
    let reader = pool.register_reader().expect("one reader slot");
    let source_page = PageId::new(source_file, 0);
    let target_page = PageId::new(target_id, 0);
    pool.insert_resident_frame(source_page, 0xA0);
    let source_lease = pool.lease_file(source_file).expect("source file lease");
    let target_lease = pool.lease_file(target_id).expect("target file lease");
    let source_hint = minted_hint(&pool, &source_lease, source_page);

    assert_pending_then_ready(
        &pool,
        &reader,
        pool.get_with_hint(&reader, &target_lease, target_page, Some(source_hint)),
        0xB0,
        "same-granule metadata from another file must not expose that file's frame",
    );
}

#[test]
fn a_valid_hint_cannot_bypass_retirement_while_its_lease_keeps_the_frame_resident() {
    let (pool, file) = pool_with_file("resident-hint-retiring-file");
    let reader = pool.register_reader().expect("one reader slot");
    let page = PageId::new(file, 0);
    let frame = pool.insert_resident_frame(page, 0x91);
    let lease = pool
        .lease_file(file)
        .expect("a live file admits a pre-retirement lease");
    let hint = minted_hint(&pool, &lease, page);

    assert_eq!(pool.retire_file(file), RetireStatus::Retiring);
    assert_eq!(
        pool.frame_state(frame),
        FrameState::Resident,
        "the pre-retirement lease delays physical frame retirement"
    );
    assert!(
        matches!(
            pool.get_with_hint(&reader, &lease, page, Some(hint)),
            Err(GetError::StaleFile { page: stale_page }) if stale_page == page
        ),
        "retirement clears hinted admission even while the old lease retains the frame"
    );
}

#[test]
fn a_same_granule_hint_from_another_pool_falls_back_before_frame_indexing() {
    let (source_pool, source_file) = pool_with_file_capacity("resident-hint-source-pool", 6, 1);
    let source_hints = same_granule_source_hints(&source_pool, source_file);

    let (target_pool, target_file) = pool_with_file_capacity("resident-hint-target-pool", 5, 1);
    let target_decoy_file = target_pool
        .driver()
        .open(Path::new("resident-hint-target-decoy"), DirectIo::Disabled)
        .expect("target decoy mock file");
    let target_decoy_id = target_decoy_file.file_id();
    target_pool.register_file(target_decoy_file);
    let target_reader = target_pool.register_reader().expect("one reader slot");
    let target_page = PageId::new(target_file, 0);
    let target_decoy_page = PageId::new(target_decoy_id, 0);
    let target_decoy_frame = target_pool.insert_resident_frame(target_decoy_page, 0xC0);
    target_pool.insert_resident_frame(target_page, 0xD0);
    let target_lease = target_pool
        .lease_file(target_file)
        .expect("target pool file lease");
    let out_of_range_hint = source_hints
        .iter()
        .find(|(frame, _)| *frame == ReadFrameIdx::new(5))
        .map(|(_, hint)| *hint)
        .expect("six resident source frames include one frame outside the five-frame target arena");
    let in_range_decoy_hint = source_hints
        .iter()
        .find(|(frame, _)| *frame == target_decoy_frame)
        .map(|(_, hint)| *hint)
        .expect("the source has the target decoy's in-range frame at the same granule");

    assert_hit_bytes(
        target_pool.get_with_hint(
            &target_reader,
            &target_lease,
            target_page,
            Some(out_of_range_hint),
        ),
        0xD0,
        "a same-granule foreign hint must fall back before an out-of-range frame can be indexed",
    );
    assert_hit_bytes(
        target_pool.get_with_hint(
            &target_reader,
            &target_lease,
            target_page,
            Some(in_range_decoy_hint),
        ),
        0xD0,
        "an in-range same-granule/frame/stamp decoy must fall back from another file's bytes",
    );
}
