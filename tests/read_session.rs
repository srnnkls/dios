//! Session reads (FR-7, FR-8): one epoch pin for many resident-only reads,
//! each validating the file-generation mirror after its translation entry.

#![cfg(feature = "mock")]

use std::path::Path;

use dios::testing::{MockDriver, MockPoolTestingExt, PoolBuilderTestingExt, PoolTestingExt};
use dios::{DirectIo, FileId, Get, GetError, PageId, Pool, ReaderCtx, RetireStatus, SessionRead};

const GRANULE: u32 = 4096;
const FRAMES: u32 = 4;
const POLL_BOUND: u32 = 32;
// Spaced granules keep sequential readahead out of the CLOCK order.
const FILLED: [u32; 4] = [0, 9, 18, 27];
const FIRST_PRESSURE: u32 = 36;
const SECOND_PRESSURE: u32 = 45;

fn session_pool() -> Pool<MockDriver> {
    let mock = MockDriver::builder()
        .queue_capacity(4)
        .frames(FRAMES)
        .frame_bytes(GRANULE)
        .build();
    Pool::builder()
        .frame_count(FRAMES)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .build_on(mock)
        .expect("valid session pool")
}

fn poll_until_closed(pool: &Pool<MockDriver>, file: FileId) {
    for _ in 0..POLL_BOUND {
        if pool.driver().is_closed(file) {
            return;
        }
        pool.poll();
    }
    panic!("the retired file did not close within the bounded progress budget");
}

fn fill_of(granule_idx: u32) -> u8 {
    u8::try_from(granule_idx % 251).expect("fill fits a byte") + 1
}

fn clock_pool() -> (Pool<MockDriver>, FileId) {
    let mock = MockDriver::builder()
        .queue_capacity(4)
        .frames(FRAMES)
        .frame_bytes(GRANULE)
        .build();
    let file = mock
        .open(Path::new("session-clock"), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    for granule_idx in FILLED.into_iter().chain([FIRST_PRESSURE, SECOND_PRESSURE]) {
        mock.seed_page(&file, granule_idx, fill_of(granule_idx));
    }
    let pool = Pool::builder()
        .frame_count(FRAMES)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .build_on(mock)
        .expect("valid clock pool");
    pool.register_file(file);
    (pool, file_id)
}

fn resolve(pool: &Pool<MockDriver>, reader: &ReaderCtx, page: PageId) {
    for _ in 0..POLL_BOUND {
        match pool.get(reader, page).expect("the file is live") {
            Get::Hit(guard) => {
                assert_eq!(guard[0], fill_of(page.granule_idx()));
                return;
            }
            Get::Pending(_) | Get::Busy => {
                pool.poll();
            }
        }
    }
    panic!("the page did not become resident within the bounded progress budget");
}

/// Fills every frame, then applies one pressure miss so the CLOCK sweep
/// clears every surviving reference bit.
fn fill_and_clear_references(pool: &Pool<MockDriver>, reader: &ReaderCtx, file: FileId) {
    for granule_idx in FILLED {
        resolve(pool, reader, PageId::new(file, granule_idx));
    }
    resolve(pool, reader, PageId::new(file, FIRST_PRESSURE));
}

/// Drives one pressure miss and returns the first candidate whose entry
/// clears; each step evicts at most one frame.
fn first_victim(
    pool: &Pool<MockDriver>,
    reader: &ReaderCtx,
    pressure: PageId,
    candidates: &[PageId],
) -> PageId {
    for _ in 0..POLL_BOUND {
        let resolved = matches!(
            pool.get(reader, pressure).expect("the file is live"),
            Get::Hit(_)
        );
        let evicted: Vec<PageId> = candidates
            .iter()
            .copied()
            .filter(|&page| pool.translation_lookup(page).is_none())
            .collect();
        assert!(
            evicted.len() <= 1,
            "one pressure step evicts at most one candidate: {evicted:?}"
        );
        if let Some(&victim) = evicted.first() {
            return victim;
        }
        assert!(
            !resolved,
            "the pressure miss resolved without evicting a candidate"
        );
        pool.poll();
    }
    panic!("the pressure miss evicted nothing within the bounded progress budget");
}

#[test]
fn a_session_reads_resident_bytes_absent_pages_and_a_retired_or_reused_slot_as_stale() {
    let pool = session_pool();
    let live = pool
        .open(Path::new("session-live"), DirectIo::Disabled)
        .expect("mock open");
    let retired = pool
        .open(Path::new("session-retired"), DirectIo::Disabled)
        .expect("mock open");
    pool.insert_resident_frame(PageId::new(live, 0), 0xA7);
    let reader = pool.register_reader().expect("one reader slot");
    let retired_page = PageId::new(retired, 0);

    let session = pool.session(&reader);
    let live_view = session
        .file(live)
        .expect("a live file opens in the session");
    let retired_view = session
        .file(retired)
        .expect("the file is live when the session opens it");
    match live_view.page(0).expect("the file is live") {
        SessionRead::Hit(bytes) => {
            assert_eq!(
                bytes.len(),
                GRANULE as usize,
                "a hit borrows the whole granule"
            );
            assert!(
                bytes.iter().all(|&byte| byte == 0xA7),
                "a hit returns the resident page's bytes"
            );
        }
        SessionRead::Absent => panic!("a resident page must read as a hit"),
    }
    assert!(
        matches!(live_view.page(1), Ok(SessionRead::Absent)),
        "a page without an entry reads Absent"
    );

    let _ = pool.retire_file(retired);
    poll_until_closed(&pool, retired);
    assert_eq!(
        pool.retire_file(retired),
        RetireStatus::Retired,
        "a file with no resident frames retires without waiting for the session"
    );
    assert!(
        matches!(retired_view.page(0), Err(GetError::StaleFile { page }) if page == retired_page),
        "a retired file reads StaleFile even where its entry is absent"
    );

    let successor = pool
        .open(Path::new("session-successor"), DirectIo::Disabled)
        .expect("the retired slot can be reused");
    assert!(successor.aliases_slot(&retired));
    pool.insert_resident_frame(PageId::new(successor, 0), 0x5C);
    let successor_view = session.file(successor).expect("the successor is live");
    assert!(
        matches!(successor_view.page(0), Ok(SessionRead::Hit(bytes)) if bytes[0] == 0x5C),
        "the successor's page is resident through the reused slot's entry"
    );
    assert!(
        matches!(retired_view.page(0), Err(GetError::StaleFile { page }) if page == retired_page),
        "after the epoch pin and before the entry load, slot reuse never admits the successor's bytes under the retired identity"
    );
    assert!(
        pool.driver().read_attempts_in_order().is_empty(),
        "a session read submits no I/O"
    );
}

#[test]
fn a_session_read_gives_its_unreferenced_frame_a_second_chance_without_driver_reads() {
    let (control, control_file) = clock_pool();
    let control_reader = control.register_reader().expect("one reader slot");
    fill_and_clear_references(&control, &control_reader, control_file);
    let candidates: Vec<PageId> = FILLED
        .into_iter()
        .map(|granule_idx| PageId::new(control_file, granule_idx))
        .filter(|&page| control.translation_lookup(page).is_some())
        .collect();
    let victim = first_victim(
        &control,
        &control_reader,
        PageId::new(control_file, SECOND_PRESSURE),
        &candidates,
    );

    let (pool, file) = clock_pool();
    let reader = pool.register_reader().expect("one reader slot");
    fill_and_clear_references(&pool, &reader, file);
    let victim = PageId::new(file, victim.granule_idx());
    let candidates: Vec<PageId> = candidates
        .iter()
        .map(|page| PageId::new(file, page.granule_idx()))
        .collect();
    let reads_before = pool.driver().read_attempts_in_order().len();
    {
        let session = pool.session(&reader);
        let view = session.file(file).expect("the file is live");
        assert!(
            matches!(view.page(victim.granule_idx()), Ok(SessionRead::Hit(bytes)) if bytes[0] == fill_of(victim.granule_idx())),
            "the unreferenced resident page hits in the session"
        );
    }
    assert_eq!(
        pool.driver().read_attempts_in_order().len(),
        reads_before,
        "a session read records no driver read"
    );

    let first = first_victim(
        &pool,
        &reader,
        PageId::new(file, SECOND_PRESSURE),
        &candidates,
    );
    assert_ne!(
        first, victim,
        "the session read set the frame's CLOCK reference, so it is not the first victim"
    );
    assert!(
        pool.translation_lookup(victim).is_some(),
        "the referenced page stays resident through the first eviction"
    );
}
