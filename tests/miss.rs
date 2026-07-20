//! T008 pool miss-path pins — `get`/`ready`/`poll` composed over a Driver,
//! driven deterministically over the seeded `MockDriver` so the whole file is
//! portable (no `O_DIRECT`, no Linux ring). RED today because it names the T008
//! composition + content seams that do not exist yet; every compile error must
//! be a missing-surface error on one of the signatures below.
//!
//! ── Composition + content contract (implementer fills; the exact surface
//!    this file compiles against) ─────────────────────────────────────────────
//!
//! T008 makes the pool OWN the driver it composes ("Pool composes over the
//! Driver", review.yaml batch-5 line 367) AND unifies the read target with the
//! pool's frames (the "arena-sharing" redesign). Frozen suites drive Pool with
//! no IO and use it as a bare type, so the frozen-safe shape is a driver type
//! parameter defaulted to the production `Driver`:
//!
//! ```text
//! pub struct Pool<D = Driver> { /* composes D + a file registry, AD-4 lock */ }
//!
//! pub trait PoolBackend { /* submit_read + poll — Driver and MockDriver satisfy inherently */ }
//! impl PoolBackend for Driver {…}         // production, cfg-selected
//! impl PoolBackend for MockDriver {…}      // deterministic tests (this file)
//!
//! impl PoolBuilder {
//!     pub fn build(self) -> Result<Pool<Driver>, PoolConfigError>;                    // FROZEN, internal driver
//!     pub fn build_on<D: PoolBackend>(self, driver: D) -> Result<Pool<D>, PoolConfigError>;
//! }
//! impl<D: PoolBackend> Pool<D> {
//!     pub fn register_file(&self, fd: FileHandle);   // route PageId.file() -> this handle (+ base offset)
//!     pub fn get<'p>(&'p self, reader: &'p ReaderCtx, page: PageId) -> Result<Get<'p>, GetError>;
//!     pub fn ready<'p>(&'p self, reader: &'p ReaderCtx, token: PendingToken) -> ReadyResult<'p>;
//!     pub fn poll(&self) -> usize;   // extends T007 poll: drain driver, InFlight->Resident + map, ready
//!                                    // tokens (or record fault/short), THEN epoch-advance/reclaim.
//!     pub fn driver(&self) -> &D;    // borrow the composed driver for test observation
//! }
//!
//! // Content seam that makes the arena-sharing ROUTING observable: the mock's
//! // simulated disk. A clean read completion for (fd, granule_idx) fills the
//! // destination pool frame with `fill` bytes; the pool's job is only to route
//! // that completion into the RIGHT frame for the RIGHT page.
//! pub struct ReadAttempt {
//!     pub file_offset: u64,
//!     pub destination_offset: u32,
//!     pub requested_len: u32,
//! }
//! pub enum MockIoEvent {
//!     ReadAttempt {
//!         file: FileId,
//!         file_offset: u64,
//!         destination_offset: u32,
//!         requested_len: u32,
//!     },
//!     // write/fsync attempts, all completions, and closes omitted here
//! }
//! impl MockDriver {
//!     pub fn seed_page(&self, fd: &FileHandle, granule_idx: u32, fill: u8);
//!     pub fn read_attempts_in_order(&self) -> Vec<ReadAttempt>;
//! }
//! ```
//! `MockIoEvent` is the sole chronological recorder. The frozen
//! `read_attempts_in_order` and `write_attempts_in_order` accessors are derived
//! typed projections of that stream; they do not maintain parallel logs.
//!
//! Setup order every test uses: build the mock, open its file(s), seed page
//! content, inject faults — all BEFORE `build_on` moves the mock into the pool —
//! then `register_file` each handle on the composed pool.
//!
//! ── Deliberate boundaries ─────────────────────────────────────────────────
//!   * ROUTING content IS pinned here (which page's seeded bytes land in which
//!     frame, keyed by file + granule offset, under adversarial completion
//!     reordering). What is NOT pinned here is raw DEVICE-content fidelity — that
//!     a real pread transfers the on-disk bytes verbatim — which stays with the
//!     real-backend IO suite (tests/uring.rs) and the DIO-G1/G3 benches. The mock
//!     substitutes a seeded fill for the device so routing is observable portably.
//!   * Byte content ACROSS a short-read reslice boundary is not asserted (the
//!     reslice's partial-fill mechanics are the implementer's); the short-read
//!     tests pin the POLICY branch (reslice->Ready vs EOF->Err), not partial bytes.
//!   * Concurrency / the `SeqCst` `begin_pin` publish fence / the packed-atomic
//!     `PageTable` cell are pinned only through single-threaded observable
//!     correctness; their interleaving proofs are T009 loom.
//!   * Async WRITE threading (`OpContext::write_buf` <- `WriteLease`): the only public
//!     async-write entry is `MockDriver::submit_write`, whose lease lifecycle
//!     already landed green in batch-5 (not new RED), and the mock's Write attempt
//!     observes no bytes; a Pool/`Driver` public async-write API — the seam that
//!     makes `write_buf` content observable end-to-end — does not exist and is
//!     T012/T013 write-plane territory. Noted, not invented.
//!   * The 64-concurrent-cold-gets overlap bench (<= 2.0x p50 single-miss) and its
//!     benches/plans/ entry are the IMPLEMENTER's per the bench-driven rule.

use std::panic::AssertUnwindSafe;
use std::path::Path;

use dios::testing::{
    FrameState, Injected, MockDriver, MockPoolTestingExt, PoolBuilderTestingExt, PoolTestingExt,
    ReadFrameIdx,
};
use dios::{
    DirectIo, FrameGuard, Get, GetError, PageId, PendingToken, Pool, ReaderCtx, ReadyResult,
};

const GRANULE: u32 = 4096;
const EIO: i32 = 5;
const READY_POLLS_MAX: u32 = 64;

/// A seeded in-memory driver sized to match the pool it will back (the miss path
/// unifies the pool's frames with the driver's read target, so frame count and
/// granule agree on both sides).
fn a_mock(seed: u64, frames: u32, queue_capacity: u32) -> MockDriver {
    MockDriver::builder()
        .seed(seed)
        .queue_capacity(queue_capacity)
        .frames(frames)
        .frame_bytes(GRANULE)
        .retry_bound(0)
        .build()
}

/// A watermark-satisfying single-reader pool composed over `mock`. `frames` must
/// equal the mock's frame count; the watermark is `peak + headroom` for one
/// reader.
fn pool_on(
    mock: MockDriver,
    frames: u32,
    peak: u32,
    inflight: u32,
    headroom: u32,
) -> Pool<MockDriver> {
    Pool::builder()
        .frame_count(frames)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(peak)
        .max_inflight_reads(inflight)
        .miss_headroom(headroom)
        .build_on(mock)
        .expect("a watermark-satisfying pool composes over the mock driver")
}

fn frames_in_state(pool: &Pool<MockDriver>, frames: u32, state: FrameState) -> u32 {
    (0..frames)
        .filter(|&index| pool.frame_state(ReadFrameIdx::new(index)) == state)
        .count()
        .try_into()
        .expect("frame count fits u32")
}

/// A guard whose borrowed granule carries `fill` in every byte identifies its
/// page: a wrong-frame, wrong-offset, or wrong-file completion routing would show
/// a different page's seeded fill, or none.
fn assert_page_bytes(guard: &FrameGuard<'_>, fill: u8) {
    assert_eq!(
        guard.len(),
        GRANULE as usize,
        "a guard borrows the whole granule"
    );
    assert!(
        guard.iter().all(|&byte| byte == fill),
        "the guard carries THIS page's seeded content (fill {fill:#04x}), not another page's"
    );
    assert_eq!(guard[0], fill, "leading byte identifies the page");
    assert_eq!(
        guard[GRANULE as usize - 1],
        fill,
        "trailing byte identifies the page"
    );
}

/// Polls until a submitted miss readies into a guard, handing the token back on
/// each `NotYet`. Panics on an unexpected fault — callers that expect a fault
/// assert `Err` directly.
fn drive_ready<'pool>(
    pool: &'pool Pool<MockDriver>,
    reader: &'pool ReaderCtx,
    token: PendingToken,
) -> FrameGuard<'pool> {
    let mut token = token;
    for _ in 0..READY_POLLS_MAX {
        match pool.ready(reader, token) {
            ReadyResult::Ready(guard) => return guard,
            ReadyResult::NotYet(handed_back) => {
                token = handed_back;
                pool.poll();
            }
            ReadyResult::Err(err) => panic!("a fault-free miss must not error: {err:?}"),
        }
    }
    panic!("a submitted miss never readied within the bounded poll budget");
}

fn expect_pending(outcome: Result<Get<'_>, GetError>, whose: &str) -> PendingToken {
    match outcome.unwrap_or_else(|error| panic!("{whose}: the registered file is live: {error}")) {
        Get::Pending(token) => token,
        Get::Hit(_) => panic!("{whose}: a cold page cannot hit"),
        Get::Busy => {
            panic!("{whose}: spare frames exist; a miss submits, it does not backpressure")
        }
    }
}

fn expect_live<'pool>(outcome: Result<Get<'pool>, GetError>, whose: &str) -> Get<'pool> {
    outcome.unwrap_or_else(|error| panic!("{whose}: the registered file is live: {error}"))
}

#[test]
fn a_cold_get_misses_then_readies_its_seeded_content_and_a_warm_re_get_hits() {
    let frames = 8u32;
    let mock = a_mock(0x0000_C01D, frames, 8);
    let file = mock
        .open(Path::new("miss-cold"), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    mock.seed_page(&file, 4, 0xC4);
    let pool = pool_on(mock, frames, 1, 1, 3);
    pool.register_file(file);
    let reader = pool.register_reader().expect("a reader slot");
    let page = PageId::new(file_id, 4);

    let token = expect_pending(pool.get(&reader, page), "cold get");
    assert_eq!(
        frames_in_state(&pool, frames, FrameState::InFlight),
        1,
        "a cold miss claims exactly one frame and submits one read"
    );

    let guard = drive_ready(&pool, &reader, token);
    assert_page_bytes(&guard, 0xC4);
    assert_eq!(
        frames_in_state(&pool, frames, FrameState::Resident),
        1,
        "poll transitioned the in-flight frame to Resident"
    );
    drop(guard);

    match expect_live(pool.get(&reader, page), "warm re-get") {
        Get::Hit(guard) => assert_page_bytes(&guard, 0xC4),
        Get::Pending(_) => panic!("a resident page must hit — re-submitting would block a worker"),
        Get::Busy => panic!("a resident page is never Busy"),
    }
}

#[test]
fn completions_route_each_pages_bytes_into_its_own_frame_across_offsets_and_files() {
    let frames = 12u32;
    let mock = a_mock(0xF00D_BEEF, frames, frames);
    let file_a = mock
        .open(Path::new("route-a"), DirectIo::Disabled)
        .expect("open a");
    let file_b = mock
        .open(Path::new("route-b"), DirectIo::Disabled)
        .expect("open b");
    let (id_a, id_b) = (file_a.file_id(), file_b.file_id());
    mock.seed_page(&file_a, 2, 0xA2);
    mock.seed_page(&file_a, 7, 0xA7);
    mock.seed_page(&file_b, 2, 0xB2);
    let pool = pool_on(mock, frames, 1, 3, 9);
    pool.register_file(file_a);
    pool.register_file(file_b);
    let reader = pool.register_reader().expect("a reader slot");

    let pages = [
        (PageId::new(id_a, 2), 0xA2u8),
        (PageId::new(id_a, 7), 0xA7u8),
        (PageId::new(id_b, 2), 0xB2u8),
    ];

    let mut waiters = Vec::with_capacity(pages.len());
    for (page, _) in pages {
        waiters.push(expect_pending(pool.get(&reader, page), "routing get"));
    }
    assert_eq!(
        frames_in_state(&pool, frames, FrameState::InFlight),
        3,
        "three distinct pages claim three distinct in-flight frames"
    );

    for ((_, fill), token) in pages.iter().zip(waiters) {
        let guard = drive_ready(&pool, &reader, token);
        assert_page_bytes(&guard, *fill);
    }
}

#[test]
fn n_gets_for_one_missing_page_issue_exactly_one_read_under_a_single_slot_queue() {
    let frames = 8u32;
    let waiters = 4u32;
    let mock = a_mock(0x51_1F_11_00, frames, 1);
    let file = mock
        .open(Path::new("miss-singleflight"), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    mock.seed_page(&file, 2, 0x2F);
    let pool = pool_on(mock, frames, 4, 1, 3);
    pool.register_file(file);
    let reader = pool.register_reader().expect("a reader slot");
    let page = PageId::new(file_id, 2);

    let mut tokens = Vec::with_capacity(waiters as usize);
    for waiter in 0..waiters {
        match expect_live(pool.get(&reader, page), "singleflight get") {
            Get::Pending(token) => tokens.push(token),
            Get::Hit(_) => panic!("waiter {waiter}: the page is still in flight, it cannot hit"),
            Get::Busy => panic!(
                "waiter {waiter}: a duplicate read exhausted the single queue slot — singleflight must issue ONE read"
            ),
        }
    }
    assert_eq!(
        frames_in_state(&pool, frames, FrameState::InFlight),
        1,
        "all {waiters} gets for the same missing page coalesce onto ONE in-flight frame"
    );

    let mut guards = Vec::with_capacity(waiters as usize);
    for token in tokens {
        guards.push(drive_ready(&pool, &reader, token));
    }
    assert_eq!(
        frames_in_state(&pool, frames, FrameState::Resident),
        1,
        "every waiter resolved from the single completion into the single shared frame"
    );
    for guard in &guards {
        assert_page_bytes(guard, 0x2F);
    }
}

#[test]
fn a_faulted_miss_fans_the_error_to_all_waiters_then_a_fresh_read_of_the_same_page_succeeds() {
    let frames = 8u32;
    let waiters = 3u32;
    let mock = a_mock(0xFA_17_ED_00, frames, 8);
    let file = mock
        .open(Path::new("miss-fault"), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    mock.seed_page(&file, 5, 0x55);
    mock.inject_next(Injected::Io(EIO));
    let pool = pool_on(mock, frames, 1, 1, 3);
    pool.register_file(file);
    let reader = pool.register_reader().expect("a reader slot");
    let page = PageId::new(file_id, 5);

    let mut tokens = Vec::with_capacity(waiters as usize);
    for _ in 0..waiters {
        tokens.push(expect_pending(pool.get(&reader, page), "faulting get"));
    }
    assert_eq!(
        frames_in_state(&pool, frames, FrameState::InFlight),
        1,
        "one injected fault backs one singleflight read"
    );

    pool.poll();

    for (waiter, token) in tokens.into_iter().enumerate() {
        match pool.ready(&reader, token) {
            ReadyResult::Err(err) => assert_eq!(
                err.raw_os_error(),
                Some(EIO),
                "waiter {waiter} receives the injected operating failure"
            ),
            ReadyResult::Ready(_) => {
                panic!("waiter {waiter} must see the fault, not a readied frame")
            }
            ReadyResult::NotYet(_) => panic!("waiter {waiter}: the fault is terminal, not NotYet"),
        }
    }
    assert_eq!(
        frames_in_state(&pool, frames, FrameState::Free),
        frames,
        "a faulted miss frees its frame — every frame is Free again, watermark intact"
    );

    let retry = expect_pending(pool.get(&reader, page), "same-page retry after fanout");
    let guard = drive_ready(&pool, &reader, retry);
    assert_page_bytes(&guard, 0x55);
    drop(guard);
    match expect_live(pool.get(&reader, page), "warm get after retry") {
        Get::Hit(guard) => assert_page_bytes(&guard, 0x55),
        Get::Pending(_) => panic!("the retried page is resident and must hit"),
        Get::Busy => panic!("a resident page is never Busy"),
    }
}

#[test]
fn an_early_retry_cannot_steal_the_terminal_error_from_old_waiters() {
    let frames = 8u32;
    let mock = a_mock(0xEA_12_1E_70, frames, frames);
    let file = mock
        .open(Path::new("miss-early-retry"), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    mock.seed_page(&file, 6, 0x66);
    mock.inject_next(Injected::Io(EIO));
    let pool = pool_on(mock, frames, 1, 1, 3);
    pool.register_file(file);
    let reader = pool.register_reader().expect("a reader slot");
    let page = PageId::new(file_id, 6);

    let old_a = expect_pending(pool.get(&reader, page), "old waiter A");
    let old_b = expect_pending(pool.get(&reader, page), "old waiter B");
    pool.poll();

    let retry = expect_pending(pool.get(&reader, page), "early same-page retry");
    pool.poll();

    for (whose, token) in [("old waiter A", old_a), ("old waiter B", old_b)] {
        match pool.ready(&reader, token) {
            ReadyResult::Err(error) => assert_eq!(
                error.raw_os_error(),
                Some(EIO),
                "{whose} remains bound to the original failed generation"
            ),
            ReadyResult::Ready(_) => panic!("{whose} stole the retry generation's success"),
            ReadyResult::NotYet(_) => panic!("{whose}'s original failure is terminal"),
        }
    }

    let guard = drive_ready(&pool, &reader, retry);
    assert_page_bytes(&guard, 0x66);
}

#[test]
fn retained_failures_saturate_the_fixed_miss_table_then_recover_after_interest_drops() {
    let frames = 4u32;
    let mock = a_mock(0x5A_70_AA_7E, frames, frames);
    let file = mock
        .open(Path::new("miss-retained-saturation"), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    for granule_idx in 0..=frames {
        mock.seed_page(
            &file,
            granule_idx,
            0x70 | u8::try_from(granule_idx).expect("small fixture index"),
        );
    }
    let pool = pool_on(mock, frames, 1, 1, 3);
    pool.register_file(file);
    let reader = pool.register_reader().expect("a reader slot");
    let mut retained = Vec::with_capacity(frames as usize);

    for granule_idx in 0..frames {
        pool.driver().inject_next(Injected::Io(EIO));
        retained.push(expect_pending(
            pool.get(&reader, PageId::new(file_id, granule_idx)),
            "retained failure",
        ));
        pool.poll();
    }

    let attempts_before = pool.driver().read_attempts_in_order().len();
    let recovery_page = PageId::new(file_id, frames);
    assert!(
        matches!(
            expect_live(pool.get(&reader, recovery_page), "saturated get"),
            Get::Busy
        ),
        "live terminal interests consume the fixed miss-record capacity"
    );
    assert_eq!(
        pool.driver().read_attempts_in_order().len(),
        attempts_before,
        "Busy is decided before claiming a frame or submitting a read"
    );

    drop(retained.pop());
    let recovery = expect_pending(
        pool.get(&reader, recovery_page),
        "capacity recovery after one interest drops",
    );
    let guard = drive_ready(&pool, &reader, recovery);
    assert_page_bytes(&guard, 0x74);

    for token in retained {
        match pool.ready(&reader, token) {
            ReadyResult::Err(error) => assert_eq!(error.raw_os_error(), Some(EIO)),
            ReadyResult::Ready(_) => panic!("an old retained failure observed a later success"),
            ReadyResult::NotYet(_) => panic!("a retained failure is terminal"),
        }
    }
}

#[test]
fn dropping_one_failed_waiter_preserves_the_other_waiters_error() {
    let frames = 4u32;
    let mock = a_mock(0xD0_0F_0A_E0, frames, frames);
    let file = mock
        .open(Path::new("miss-drop-one-failure"), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    mock.inject_next(Injected::Io(EIO));
    let pool = pool_on(mock, frames, 1, 1, 3);
    pool.register_file(file);
    let reader = pool.register_reader().expect("a reader slot");
    let page = PageId::new(file_id, 1);

    let dropped = expect_pending(pool.get(&reader, page), "dropped waiter");
    let survivor = expect_pending(pool.get(&reader, page), "surviving waiter");
    pool.poll();
    drop(dropped);

    match pool.ready(&reader, survivor) {
        ReadyResult::Err(error) => assert_eq!(error.raw_os_error(), Some(EIO)),
        ReadyResult::Ready(_) => panic!("the surviving waiter lost its terminal error"),
        ReadyResult::NotYet(_) => panic!("the surviving waiter's failure is terminal"),
    }
}

#[test]
fn repeated_not_yet_checks_preserve_exactly_one_waiter_interest() {
    let frames = 4u32;
    let mock = a_mock(0xA0_71_E7, frames, frames);
    let file = mock
        .open(Path::new("miss-not-yet-interest"), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    let pool = pool_on(mock, frames, 1, 1, 3);
    pool.register_file(file);
    let reader = pool.register_reader().expect("a reader slot");
    let page = PageId::new(file_id, 2);
    let mut token = expect_pending(pool.get(&reader, page), "not-yet miss");

    assert_eq!(pool.pending_waiters(&token), 1, "get minted one interest");
    for _ in 0..8 {
        token = match pool.ready(&reader, token) {
            ReadyResult::NotYet(handed_back) => handed_back,
            ReadyResult::Ready(_) => panic!("without poll, the miss cannot be ready"),
            ReadyResult::Err(_) => panic!("without poll, the miss cannot have failed"),
        };
        assert_eq!(
            pool.pending_waiters(&token),
            1,
            "NotYet returns the same capability without decrementing or rejoining"
        );
    }
}

#[test]
fn a_cross_pool_token_panics_before_the_target_pool_observes_its_page() {
    let frames = 4u32;
    let source_mock = a_mock(0x50_0A_CE, frames, frames);
    let source_file = source_mock
        .open(Path::new("miss-cross-source"), DirectIo::Disabled)
        .expect("source mock open");
    let source_id = source_file.file_id();
    let source = pool_on(source_mock, frames, 1, 1, 3);
    source.register_file(source_file);
    let source_reader = source.register_reader().expect("a source reader slot");
    let token = expect_pending(
        source.get(&source_reader, PageId::new(source_id, 1)),
        "source miss",
    );

    let target_mock = a_mock(0x7A_A6_E7, frames, frames);
    let target = pool_on(target_mock, frames, 1, 1, 3);
    let target_reader = target.register_reader().expect("a target reader slot");
    let target_attempts_before = target.driver().read_attempts_in_order().len();

    let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = target.ready(&target_reader, token);
    }));
    assert!(
        panic.is_err(),
        "a pending capability belongs to exactly one pool"
    );
    assert_eq!(
        target.driver().read_attempts_in_order().len(),
        target_attempts_before,
        "pool identity is rejected before target-pool miss state is observed or submitted"
    );
}

#[test]
fn a_cross_pool_reader_panics_before_get_touches_the_target_pool() {
    let frames = 4u32;
    let source = pool_on(a_mock(0x50_0C_7E, frames, frames), frames, 1, 1, 3);
    let source_reader = source.register_reader().expect("a source reader slot");

    let target_mock = a_mock(0x7A_26_E7, frames, frames);
    let target_file = target_mock
        .open(Path::new("reader-cross-target"), DirectIo::Disabled)
        .expect("target mock open");
    let target_id = target_file.file_id();
    target_mock.seed_page(&target_file, 1, 0x71);
    let target = pool_on(target_mock, frames, 1, 1, 3);
    target.register_file(target_file);

    let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = target.get(&source_reader, PageId::new(target_id, 1));
    }));
    assert!(
        panic.is_err(),
        "a reader capability belongs to exactly one pool"
    );
    assert_eq!(
        frames_in_state(&target, frames, FrameState::Free),
        frames,
        "reader ownership is rejected before the target claims or observes a frame"
    );
    assert!(
        target.driver().read_attempts_in_order().is_empty(),
        "reader ownership is rejected before target IO admission"
    );
}

#[test]
fn a_cross_pool_reader_panics_before_ready_pins_the_target_frame() {
    let frames = 4u32;
    let source = pool_on(a_mock(0x50_0C_75, frames, frames), frames, 1, 1, 3);
    let source_reader = source.register_reader().expect("a source reader slot");

    let target_mock = a_mock(0x7A_26_75, frames, frames);
    let target_file = target_mock
        .open(Path::new("reader-ready-target"), DirectIo::Disabled)
        .expect("target mock open");
    let target_id = target_file.file_id();
    target_mock.seed_page(&target_file, 2, 0x72);
    let target = pool_on(target_mock, frames, 1, 1, 3);
    target.register_file(target_file);
    let target_reader = target.register_reader().expect("a target reader slot");
    let page = PageId::new(target_id, 2);
    let token = expect_pending(target.get(&target_reader, page), "target miss");
    target.poll();

    let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = target.ready(&source_reader, token);
    }));
    assert!(panic.is_err(), "ready rejects a reader from another pool");
    match expect_live(target.get(&target_reader, page), "target re-get") {
        Get::Hit(guard) => assert_page_bytes(&guard, 0x72),
        Get::Pending(_) => panic!("reader rejection cannot lose the resident target frame"),
        Get::Busy => panic!("reader rejection cannot make a resident target page Busy"),
    }
}

#[test]
fn a_foreign_page_file_identity_panics_before_admission_or_frame_mutation() {
    let frames = 4u32;
    let foreign_mock = a_mock(0x00F0_2E16, frames, frames);
    let foreign_file = foreign_mock
        .open(Path::new("page-foreign-source"), DirectIo::Disabled)
        .expect("foreign mock open");
    let foreign_id = foreign_file.file_id();

    let target_mock = a_mock(0x7A_26_F1, frames, frames);
    let target_file = target_mock
        .open(Path::new("page-foreign-target"), DirectIo::Disabled)
        .expect("target mock open");
    let target = pool_on(target_mock, frames, 1, 1, 3);
    target.register_file(target_file);
    let target_reader = target.register_reader().expect("a target reader slot");

    let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = target.get(&target_reader, PageId::new(foreign_id, 1));
    }));
    assert!(
        panic.is_err(),
        "a PageId cannot route through an unrelated handle that happens to occupy the same fd slot"
    );
    assert_eq!(
        frames_in_state(&target, frames, FrameState::Free),
        frames,
        "foreign file identity is rejected before a frame leaves Free"
    );
    assert!(
        target.driver().read_attempts_in_order().is_empty(),
        "foreign file identity is rejected before a read attempt"
    );
}

#[test]
fn a_stale_page_file_generation_returns_the_exact_error_before_reusing_its_live_slot() {
    let frames = 4u32;
    let mock = a_mock(0x57_A1_E0, frames, frames);
    let stale_file = mock
        .open(Path::new("page-stale-generation"), DirectIo::Disabled)
        .expect("stale mock open");
    let stale_id = stale_file.file_id();
    mock.close(stale_file);
    let live_file = mock
        .open(Path::new("page-live-generation"), DirectIo::Disabled)
        .expect("live mock reopen");
    assert!(
        live_file.file_id().aliases_slot(&stale_id),
        "the regression requires fd-slot reuse with a distinct generation"
    );

    let pool = pool_on(mock, frames, 1, 1, 3);
    pool.register_file(live_file);
    let reader = pool.register_reader().expect("a reader slot");
    let page = PageId::new(stale_id, 1);

    assert_eq!(
        pool.get(&reader, page)
            .expect_err("a stale generation is an expected product error"),
        GetError::StaleFile { page },
        "the stale generation cannot route through the live handle in its reused slot"
    );
    assert_eq!(
        frames_in_state(&pool, frames, FrameState::Free),
        frames,
        "stale generation is rejected before a frame leaves Free"
    );
    assert!(
        pool.driver().read_attempts_in_order().is_empty(),
        "stale generation is rejected before a read attempt"
    );
}

#[test]
fn a_successful_unconsumed_token_protects_its_exact_frame_until_drop() {
    let frames = 4u32;
    let mock = a_mock(0x51_CC_E5_50, frames, frames);
    let file = mock
        .open(Path::new("miss-success-interest"), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    for granule_idx in 0..=(frames * 2) {
        mock.seed_page(
            &file,
            granule_idx,
            u8::try_from(0x40 + granule_idx).expect("small fixture fill"),
        );
    }
    let pool = pool_on(mock, frames, 1, 1, 3);
    pool.register_file(file);
    let reader = pool.register_reader().expect("a reader slot");
    let held_page = PageId::new(file_id, 0);
    let held = expect_pending(pool.get(&reader, held_page), "held successful miss");
    pool.poll();

    for granule_idx in 1..=frames {
        let page = PageId::new(file_id, granule_idx);
        let token = loop {
            match expect_live(pool.get(&reader, page), "pressure get") {
                Get::Pending(token) => break token,
                Get::Busy => {
                    pool.poll();
                }
                Get::Hit(_) => panic!("a new pressure page cannot hit"),
            }
        };
        drop(drive_ready(&pool, &reader, token));
    }

    match expect_live(pool.get(&reader, held_page), "protected-page get") {
        Get::Hit(guard) => assert_page_bytes(&guard, 0x40),
        Get::Pending(_) => panic!("CLOCK evicted a frame protected by terminal waiter interest"),
        Get::Busy => panic!("a protected resident page cannot be Busy"),
    }

    drop(held);
    for granule_idx in (frames + 1)..=(frames * 2) {
        let page = PageId::new(file_id, granule_idx);
        let token = loop {
            match expect_live(pool.get(&reader, page), "post-drop pressure get") {
                Get::Pending(token) => break token,
                Get::Busy => {
                    pool.poll();
                }
                Get::Hit(_) => panic!("a new post-drop pressure page cannot hit"),
            }
        };
        drop(drive_ready(&pool, &reader, token));
    }

    assert!(
        !matches!(
            expect_live(pool.get(&reader, held_page), "post-pressure held-page get"),
            Get::Hit(_)
        ),
        "after the last token drops, the successful frame returns to normal CLOCK eviction"
    );
}

#[test]
fn dropping_a_pending_token_cancels_interest_only_and_the_read_still_completes() {
    let frames = 8u32;
    let mock = a_mock(0x0D_00_09_ED, frames, 8);
    let file = mock
        .open(Path::new("miss-drop"), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    mock.seed_page(&file, 3, 0x3D);
    let pool = pool_on(mock, frames, 1, 1, 3);
    pool.register_file(file);
    let reader = pool.register_reader().expect("a reader slot");
    let page = PageId::new(file_id, 3);

    {
        let token = expect_pending(pool.get(&reader, page), "interest get");
        assert_eq!(token.page(), page, "the scoped waiter names this miss");
    }

    pool.poll();
    assert_eq!(
        frames_in_state(&pool, frames, FrameState::Resident),
        1,
        "dropping the token is waiter-interest only — the in-flight read still completed Resident"
    );

    match expect_live(pool.get(&reader, page), "completed dropped-interest get") {
        Get::Hit(guard) => assert_page_bytes(&guard, 0x3D),
        Get::Pending(_) => {
            panic!("dropping a PendingToken must not cancel the read — the page is resident")
        }
        Get::Busy => panic!("a resident page is never Busy"),
    }

    pool.poll();
    assert_eq!(
        frames_in_state(&pool, frames, FrameState::Resident),
        1,
        "the frame the dropped miss filled is mapped and stable, never leaked or reclaimed early"
    );
}

#[test]
fn a_miss_is_pending_within_the_watermark_and_busy_leaves_pinned_frames_untouched_then_recovers() {
    let frames = 4u32;
    let mock = a_mock(0x0000_B05E, frames, 8);
    let file = mock
        .open(Path::new("miss-busy"), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    for index in 0..frames {
        mock.seed_page(
            &file,
            index,
            0xF0 | u8::try_from(index).expect("index fits u8"),
        );
    }
    let pool = pool_on(mock, frames, 1, 1, 3);
    pool.register_file(file);
    let reader = pool.register_reader().expect("a reader slot");

    let mut guards: Vec<FrameGuard<'_>> = Vec::with_capacity(frames as usize);
    for index in 0..frames {
        let page = PageId::new(file_id, index);
        match expect_live(pool.get(&reader, page), "watermark get") {
            Get::Pending(token) => guards.push(drive_ready(&pool, &reader, token)),
            Get::Hit(_) => panic!("frame {index}: a distinct cold page cannot hit"),
            Get::Busy => panic!(
                "frame {index}: within the watermark a miss is Pending, never Busy (INV-9 positive space)"
            ),
        }
    }
    for (index, guard) in guards.iter().enumerate() {
        assert_page_bytes(guard, 0xF0 | u8::try_from(index).expect("index fits u8"));
    }
    assert_eq!(
        frames_in_state(&pool, frames, FrameState::Free),
        0,
        "every frame is now held and none is Free"
    );

    let absent = PageId::new(file_id, frames + 1);
    match expect_live(pool.get(&reader, absent), "pinned-capacity get") {
        Get::Busy => {}
        Get::Pending(_) => panic!(
            "with every frame pinned and none evictable, a further miss must backpressure Busy, not submit"
        ),
        Get::Hit(_) => panic!("an unfetched page cannot hit"),
    }

    assert_eq!(
        frames_in_state(&pool, frames, FrameState::Free),
        0,
        "the Busy call freed no frame — no pinned frame was reclaimed under a live guard"
    );
    assert_eq!(
        frames_in_state(&pool, frames, FrameState::InFlight),
        0,
        "the Busy call submitted no read — no held frame was recycled to InFlight for the refused miss"
    );
    for (index, guard) in guards.iter().enumerate() {
        assert_page_bytes(guard, 0xF0 | u8::try_from(index).expect("index fits u8"));
    }

    drop(guards);
    let mut recovered = false;
    for _ in 0..READY_POLLS_MAX {
        match expect_live(pool.get(&reader, absent), "recovery get") {
            Get::Pending(_) | Get::Hit(_) => {
                recovered = true;
                break;
            }
            Get::Busy => {
                pool.poll();
            }
        }
    }
    assert!(
        recovered,
        "Busy is retriable backpressure: once the pinning guards drop and reclamation runs, the miss admits — never a deadlock"
    );
}

#[test]
fn a_short_read_with_remaining_extent_resubmits_the_remainder_as_a_distinct_read() {
    // scope.md:601: short reads are resliced and resubmitted by the pool up to the extent, not terminal.
    let frames = 8u32;
    let mock = a_mock(0x0000_5401, frames, 8);
    let file = mock
        .open(Path::new("miss-short-remainder"), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    mock.seed_page(&file, 1, 0x1C);
    mock.inject_next(Injected::Short(GRANULE / 2));
    mock.inject_next(Injected::Io(EIO));
    let pool = pool_on(mock, frames, 1, 1, 3);
    pool.register_file(file);
    let reader = pool.register_reader().expect("a reader slot");
    let page = PageId::new(file_id, 1);

    let mut token = expect_pending(pool.get(&reader, page), "short-remainder get");
    let mut outcome = None;
    for _ in 0..READY_POLLS_MAX {
        match pool.ready(&reader, token) {
            ReadyResult::NotYet(handed_back) => {
                token = handed_back;
                pool.poll();
            }
            ReadyResult::Err(err) => {
                outcome = Some(err.raw_os_error());
                break;
            }
            ReadyResult::Ready(_) => panic!(
                "readiness before the remainder read is the bug: the pool completed on the first partial instead of resubmitting the remainder"
            ),
        }
    }
    assert_eq!(
        outcome,
        Some(Some(EIO)),
        "the fault injected only for the remainder surfaced — proving a distinct second read was submitted"
    );
    assert_eq!(
        frames_in_state(&pool, frames, FrameState::Free),
        frames,
        "the terminal remainder fault frees the frame, like any faulted miss"
    );

    let base_offset = u64::from(GRANULE);
    let short_count = GRANULE / 2;
    let attempts = pool.driver().read_attempts_in_order();
    assert_eq!(
        attempts.len(),
        2,
        "exactly two read attempts: the original granule read and its resubmitted remainder"
    );
    assert_eq!(
        (attempts[0].file_offset, attempts[0].requested_len),
        (base_offset, GRANULE),
        "the first attempt reads the whole granule at the page's base offset"
    );
    assert_eq!(
        attempts[1].file_offset,
        base_offset + u64::from(short_count),
        "the remainder read's offset advanced by the short count — reslice, not a full re-read of the extent"
    );
    assert_eq!(
        attempts[1].requested_len,
        GRANULE - short_count,
        "the remainder read requests only the unfilled tail, up to the extent length (scope.md:601)"
    );
}

#[test]
fn a_short_read_at_eof_is_terminal_and_fans_err_to_all_waiters_and_frees_the_frame() {
    // scope.md:570: a short-read-at-EOF fans Err to all waiters and frees the frame, like an IO error.
    let frames = 8u32;
    let waiters = 2u32;
    let mock = a_mock(0x0000_E0F0, frames, 8);
    let file = mock
        .open(Path::new("miss-short-eof"), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    mock.inject_next(Injected::Short(0));
    let pool = pool_on(mock, frames, 1, 1, 3);
    pool.register_file(file);
    let reader = pool.register_reader().expect("a reader slot");
    let page = PageId::new(file_id, 9);

    let mut tokens = Vec::with_capacity(waiters as usize);
    for _ in 0..waiters {
        tokens.push(expect_pending(pool.get(&reader, page), "short-eof get"));
    }

    pool.poll();

    for (waiter, token) in tokens.into_iter().enumerate() {
        match pool.ready(&reader, token) {
            ReadyResult::Err(_) => {}
            ReadyResult::Ready(_) => {
                panic!("waiter {waiter}: a short-read-at-EOF must not ready a frame")
            }
            ReadyResult::NotYet(_) => {
                panic!("waiter {waiter}: the EOF short is terminal, not NotYet")
            }
        }
    }
    assert_eq!(
        frames_in_state(&pool, frames, FrameState::Free),
        frames,
        "a terminal short-read-at-EOF frees its frame like an IO error"
    );
}
