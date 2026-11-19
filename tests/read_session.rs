//! Session reads (FR-7, FR-8): one epoch pin for many resident-only reads,
//! with file liveness validated once, when a view of the file opens.

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
const SESSION_GRANULES_MAX: u32 = 64;

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
        .file_granules_max(SESSION_GRANULES_MAX)
        .build_on(mock)
        .expect("valid session pool")
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
        .file_granules_max(SESSION_GRANULES_MAX)
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

/// A read through a view opened before its file retired: the file's own
/// granule, or `Absent`, and never `StaleFile` (AT-D21).
fn assert_own_bytes_or_absent(read: Result<SessionRead<'_>, GetError>, own_fill: u8) {
    match read {
        Ok(SessionRead::Hit(bytes)) => assert!(
            bytes.iter().all(|&byte| byte == own_fill),
            "a view opened before retirement returns only its own file's bytes"
        ),
        Ok(SessionRead::Absent) => {}
        Err(error) => panic!(
            "a view opened before retirement reads its own bytes or Absent until the session drops, not {error:?}"
        ),
    }
}

#[test]
fn a_session_reads_resident_and_absent_pages_and_rejects_only_a_file_retired_before_its_view() {
    let pool = session_pool();
    let live = pool
        .open(Path::new("session-live"), DirectIo::Disabled)
        .expect("mock open");
    let retired_early = pool
        .open(Path::new("session-retired-early"), DirectIo::Disabled)
        .expect("mock open");
    let retired_late = pool
        .open(Path::new("session-retired-late"), DirectIo::Disabled)
        .expect("mock open");
    pool.insert_resident_frame(PageId::new(live, 0), 0xA7);
    pool.insert_resident_frame(PageId::new(retired_late, 0), 0xB4);
    let reader = pool.register_reader().expect("one reader slot");
    assert_eq!(pool.retire_file(retired_early), RetireStatus::Retiring);

    {
        let session = pool.session(&reader);
        let live_view = session
            .file(live)
            .expect("a live file opens in the session");
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
        assert!(
            matches!(
                session.file(retired_early),
                Err(GetError::StaleFile { page }) if page.file() == retired_early
            ),
            "a file retired before its view opens is StaleFile"
        );

        let late_view = session
            .file(retired_late)
            .expect("the file is live when the session opens it");
        assert!(
            matches!(late_view.page(0), Ok(SessionRead::Hit(bytes)) if bytes[0] == 0xB4),
            "the view reads its resident page before retirement"
        );
        assert_eq!(pool.retire_file(retired_late), RetireStatus::Retiring);
        let successor = pool
            .open(Path::new("session-successor"), DirectIo::Disabled)
            .expect("a free file slot remains");
        pool.insert_resident_frame(PageId::new(successor, 0), 0x5C);
        for _ in 0..POLL_BOUND {
            pool.poll();
            assert_own_bytes_or_absent(late_view.page(0), 0xB4);
            assert_own_bytes_or_absent(late_view.page(1), 0xB4);
        }
    }

    assert!(
        pool.driver().read_attempts_in_order().is_empty(),
        "a session read submits no I/O"
    );
    let session = pool.session(&reader);
    assert!(
        matches!(
            session.file(retired_late),
            Err(GetError::StaleFile { page }) if page.file() == retired_late
        ),
        "a view opened after retirement is StaleFile"
    );
}

#[test]
fn a_file_session_rejects_a_file_retired_before_it_opens_and_reads_its_own_bytes_after() {
    let pool = session_pool();
    let retired_early = pool
        .open(Path::new("file-session-retired-early"), DirectIo::Disabled)
        .expect("mock open");
    let retired_late = pool
        .open(Path::new("file-session-retired-late"), DirectIo::Disabled)
        .expect("mock open");
    pool.insert_resident_frame(PageId::new(retired_late, 0), 0xC3);
    let reader = pool.register_reader().expect("one reader slot");
    assert_eq!(pool.retire_file(retired_early), RetireStatus::Retiring);

    assert!(
        matches!(
            pool.session_file(&reader, retired_early),
            Err(GetError::StaleFile { page }) if page.file() == retired_early
        ),
        "a file retired before its view opens is StaleFile"
    );

    {
        let view = pool
            .session_file(&reader, retired_late)
            .expect("the file is live when its view opens");
        assert!(
            matches!(view.page(0), Ok(SessionRead::Hit(bytes)) if bytes[0] == 0xC3),
            "the view reads its resident page before retirement"
        );
        assert_eq!(pool.retire_file(retired_late), RetireStatus::Retiring);
        let successor = pool
            .open(Path::new("file-session-successor"), DirectIo::Disabled)
            .expect("a free file slot remains");
        pool.insert_resident_frame(PageId::new(successor, 0), 0x5C);
        for _ in 0..POLL_BOUND {
            pool.poll();
            assert_own_bytes_or_absent(view.page(0), 0xC3);
            assert_own_bytes_or_absent(view.page(1), 0xC3);
        }
    }

    assert!(
        matches!(
            pool.session_file(&reader, retired_late),
            Err(GetError::StaleFile { page }) if page.file() == retired_late
        ),
        "a view opened after retirement is StaleFile"
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
