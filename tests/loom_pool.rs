//! T009 pool concurrency proofs (loom). Compiled only under `--cfg loom`; the
//! normal build sees an empty crate, so the loom dev-dependency never touches
//! the shipping build (Cargo.toml gates it on `cfg(loom)`). Run with:
//!   `RUSTFLAGS="--cfg loom" cargo test --test loom_pool`.
//!
//! ── Why a `loom_model` seam, not `Pool::get`/`poll` directly ────────────────
//! loom explores interleavings only over ITS atomics. An integration test that
//! drove the whole `Pool<MockDriver>` (page table + slab + singleflight + mock
//! disk) would (a) blow up loom's state space past termination and (b) pass
//! VACUOUSLY today, because the shipping pool uses `std` atomics loom cannot see
//! — a false green, the exact anti-pattern this phase guards against. So the
//! proofs drive a bounded, purpose-built entry the implementer exposes over the
//! REAL lock-free machinery (frame-state atomic, EBR epoch slots incl.
//! `begin_pin`'s `SeqCst` fence, the AD-4 Control lock, the single-writer
//! `PageTable` seqlock) routed through the crate's `cfg(loom)` sync alias.
//!
//! CONTRACT (RED until it exists — every error must be missing-surface on this
//! seam; and each op MUST delegate to the production atomics/lock — a bespoke
//! reimplementation is a tautology that fails the review gate):
//!
//! ```text
//! #[cfg(loom)] #[doc(hidden)] pub mod loom_model {
//!   pub struct PoolModel;               // one shared control plane, N frames, 1 reader slot
//!   pub struct Guard;                   // a live epoch pin; unpins on Drop (last-guard -> quiescent)
//!   pub struct Snapshot;                // one committed seqlock read of a page->(frame,gen) cell
//!   impl PoolModel {
//!     pub fn new(frames: u32) -> loom::sync::Arc<Self>;
//!     // setup, single-threaded, before threads spawn: map `page` Resident in frame 0, granule filled `gen`
//!     pub fn make_resident(&self, page: u32, gen: u8);
//!     // reader: publish local_epoch (real begin_pin + SeqCst fence) THEN validate residency.
//!     // Some(g): a live guard whose frame currently reads content-generation g.
//!     // None: observed Evicting/unmapped -> miss path, never derefs.
//!     // >1 live Guard per thread share the published epoch via a per-thread count;
//!     // only the LAST drop republishes quiescent (design.md:132) — an inner drop under a
//!     // live outer guard must not.
//!     pub fn pin(&self, page: u32) -> Option<Guard>;
//!     // poller: one CLOCK pass that may take `page` Resident -> Evicting, unmap it, tag the epoch.
//!     pub fn evict(&self, page: u32);
//!     // poller under the AD-4 Control lock: advance the epoch iff every reader permits,
//!     // reclaim two-advance-expired Evicting frames (Evicting -> Free), and refill any frame
//!     // it frees by mapping `refill_page` Resident with content-generation `refill_gen`.
//!     pub fn poll_pass(&self, refill_page: u32, refill_gen: u8) -> DrainReport;
//!     // writer under the Control lock: remap `page` to a FRESH frame (frame 1) carrying `gen`,
//!     // committed as ONE seqlock transaction (version bump around the multi-field cell write).
//!     pub fn remap(&self, page: u32, gen: u8);
//!     // reader: a lock-free advisory seqlock read; None = empty/retry, Some = a non-torn snapshot.
//!     pub fn probe(&self, page: u32) -> Option<Snapshot>;
//!   }
//!   impl Guard    { pub fn generation(&self) -> u8; }   // re-reads the LIVE frame content, not a pin-time copy
//!   impl Snapshot { pub fn frame(&self) -> u32; pub fn generation(&self) -> u8; }
//! }
//! ```
//!
//! Frame convention the proofs rely on: `make_resident` installs in frame 0,
//! `remap` installs in frame 1, so a coupled (frame, gen) pair makes a torn
//! seqlock read directly observable.
//!
//! Deferred boundary: dead-thread reader-slot deregistration racing epoch advance
//! (design.md:124-125) is unmodeled — the bounded one-slot seam has no dereg
//! thread; the RAII/TLS-destructor drop path is T007 single-threaded coverage, and
//! a concurrent dereg-vs-advance model is future work if the reader-slot machinery
//! changes.

#![cfg(loom)]

use dios::loom_model::{DrainSource, PoolModel};
use dios::{ResidentLeaseError, RetainRefusedReason};
use loom::model::Builder;
use loom::sync::{Arc, mpsc};
use loom::thread;

const HELD_PAGE: u32 = 10;
const INTRUDER_PAGE: u32 = 20;
const HELD_GEN: u8 = 1;
const INTRUDER_GEN: u8 = 2;
const RETAINED_FILE_GENERATION: u32 = 1;
const REOPENED_FILE_GENERATION: u32 = 2;
const RETENTION_MAX_THREADS: usize = 4;
const RETENTION_MAX_BRANCHES: usize = 1_000;
const RETENTION_MAX_PERMUTATIONS: usize = 1_024;
const RETENTION_MAX_PREEMPTIONS: usize = 2;

fn retention_model<F>(scenario: F)
where
    F: Fn() + Send + Sync + 'static,
{
    let mut model = Builder::new();
    model.max_threads = RETENTION_MAX_THREADS;
    model.max_branches = RETENTION_MAX_BRANCHES;
    model.max_permutations = Some(RETENTION_MAX_PERMUTATIONS);
    model.preemption_bound = Some(RETENTION_MAX_PREEMPTIONS);
    model.check(scenario);
}

/// INV-1 + EBR: without `begin_pin`'s `SeqCst` fence the reader's `local_epoch`
/// publish can sit in the store buffer while the poller advances twice and
/// reclaims, refilling the frame under the live guard. loom must find that
/// interleaving absent the fence.
#[test]
fn a_live_guard_frame_is_never_reclaimed_or_refilled_by_a_concurrent_poller() {
    loom::model(|| {
        let pool = PoolModel::new(1);
        pool.make_resident(HELD_PAGE, HELD_GEN);

        let reader_pool = pool.clone();
        let reader = thread::spawn(move || {
            if let Some(guard) = reader_pool.pin(HELD_PAGE) {
                assert_eq!(
                    guard.generation(),
                    HELD_GEN,
                    "a validated pin observes its own page's content, never a mid-eviction frame"
                );
                assert_eq!(
                    guard.generation(),
                    HELD_GEN,
                    "the frame stays this page's until the guard drops — no reclaim/refill under a live pin (INV-1/EBR)"
                );
                drop(guard);
            }
        });

        pool.evict(HELD_PAGE);
        pool.poll_pass(INTRUDER_PAGE, INTRUDER_GEN);
        pool.poll_pass(INTRUDER_PAGE, INTRUDER_GEN);

        reader.join().expect("reader thread");
    });
}

/// The dual of the grace proof: once the guard drops, the two-advance reclaim
/// must complete and the frame must become reusable — the epoch gates reuse, it
/// does not permanently pin.
#[test]
fn a_frame_reclaims_and_refills_once_the_guard_has_dropped() {
    loom::model(|| {
        let pool = PoolModel::new(1);
        pool.make_resident(HELD_PAGE, HELD_GEN);

        let reader_pool = pool.clone();
        let reader = thread::spawn(move || {
            if let Some(guard) = reader_pool.pin(HELD_PAGE) {
                assert_eq!(guard.generation(), HELD_GEN);
                drop(guard);
            }
        });
        reader.join().expect("reader thread");

        pool.evict(HELD_PAGE);
        pool.poll_pass(INTRUDER_PAGE, INTRUDER_GEN);
        pool.poll_pass(INTRUDER_PAGE, INTRUDER_GEN);

        match pool.pin(INTRUDER_PAGE) {
            Some(guard) => assert_eq!(
                guard.generation(),
                INTRUDER_GEN,
                "after the guard dropped, the reclaimed frame carries the refilled page"
            ),
            None => panic!("the intruder was installed Resident; a fresh pin must observe it"),
        }
    });
}

/// Nested-guard race (design.md:132, INV-1): republishing the quiescent epoch on
/// ANY drop rather than the last frees the frame under a live outer guard — a
/// race no single-threaded T007 test sees.
#[test]
fn an_inner_nested_guard_drop_does_not_release_the_frame_under_the_outer_guard() {
    loom::model(|| {
        let pool = PoolModel::new(1);
        pool.make_resident(HELD_PAGE, HELD_GEN);

        let reader_pool = pool.clone();
        let reader = thread::spawn(move || {
            if let Some(outer) = reader_pool.pin(HELD_PAGE) {
                let inner = reader_pool
                    .pin(HELD_PAGE)
                    .expect("a nested pin on a live frame");
                assert_eq!(inner.generation(), HELD_GEN);
                drop(inner);
                assert_eq!(
                    outer.generation(),
                    HELD_GEN,
                    "dropping the inner nested guard must not republish quiescent — the outer pin still holds the frame"
                );
                drop(outer);
            }
        });

        pool.evict(HELD_PAGE);
        pool.poll_pass(INTRUDER_PAGE, INTRUDER_GEN);
        pool.poll_pass(INTRUDER_PAGE, INTRUDER_GEN);

        reader.join().expect("reader thread");
    });
}

/// `PageTable` single-writer seqlock: a torn read pairs one write's frame with the
/// other's generation. The two committed writes are (frame 0, gen A) and
/// (frame 1, gen B), so (0, B) and (1, A) are the torn outcomes the seqlock
/// version discipline must exclude.
#[test]
fn a_seqlock_read_racing_the_single_writer_never_returns_a_torn_snapshot() {
    const FRAME_A: u32 = 0;
    const FRAME_B: u32 = 1;
    const GEN_A: u8 = 0xA;
    const GEN_B: u8 = 0xB;

    loom::model(|| {
        let pool = PoolModel::new(2);
        pool.make_resident(HELD_PAGE, GEN_A);

        let writer_pool = pool.clone();
        let writer = thread::spawn(move || {
            writer_pool.remap(HELD_PAGE, GEN_B);
        });

        if let Some(snapshot) = pool.probe(HELD_PAGE) {
            let pair = (snapshot.frame(), snapshot.generation());
            assert!(
                pair == (FRAME_A, GEN_A) || pair == (FRAME_B, GEN_B),
                "a seqlock read returns one committed write's (frame, gen), never a torn mix: got {pair:?}"
            );
        }

        writer.join().expect("writer thread");
    });
}

#[test]
fn mirror_get_racing_retire_and_slot_reuse_never_admits_reused_bytes() {
    const OLD_FILE_GENERATION: u32 = 1;
    const NEW_FILE_GENERATION: u32 = 2;
    const PAGE: u32 = 7;
    const OLD_CONTENT: u8 = 0x31;
    const NEW_CONTENT: u8 = 0x72;

    loom::model(|| {
        let pool = PoolModel::new(1);
        pool.make_file_resident(OLD_FILE_GENERATION, PAGE, OLD_CONTENT);

        let reader_pool = pool.clone();
        let reader = thread::spawn(move || {
            if let Some(guard) = reader_pool.get_file(OLD_FILE_GENERATION, PAGE) {
                assert_eq!(
                    guard.generation(),
                    OLD_CONTENT,
                    "an old-generation get may return only bytes guarded before retirement"
                );
                assert_eq!(
                    reader_pool.locked_get_checks(),
                    0,
                    "a live mirror hit does not take the control lock"
                );
                drop(guard);
            }
        });

        pool.retire_file(OLD_FILE_GENERATION, PAGE);
        pool.poll_reopen(NEW_FILE_GENERATION, PAGE, NEW_CONTENT);
        pool.poll_reopen(NEW_FILE_GENERATION, PAGE, NEW_CONTENT);

        reader.join().expect("reader thread");
    });
}

#[test]
fn lease_acquire_racing_retirement_and_reuse_admits_exactly_one_generation() {
    const OLD_FILE_GENERATION: u32 = 1;
    const NEW_FILE_GENERATION: u32 = 2;
    const PAGE: u32 = 7;

    loom::model(|| {
        let pool = PoolModel::new(1);
        pool.make_file_resident(OLD_FILE_GENERATION, PAGE, 0x31);

        let acquiring_pool = pool.clone();
        let acquisition = thread::spawn(move || acquiring_pool.lease_file(OLD_FILE_GENERATION));

        pool.retire_file(OLD_FILE_GENERATION, PAGE);
        pool.poll_reopen(NEW_FILE_GENERATION, PAGE, 0x72);
        pool.poll_reopen(NEW_FILE_GENERATION, PAGE, 0x72);

        let old_won = match acquisition.join().expect("lease acquisition thread") {
            Ok(_old_lease) => {
                assert_eq!(pool.resident_lease_count(), 1);
                assert!(
                    pool.get_file(NEW_FILE_GENERATION, PAGE).is_none(),
                    "the old lease blocks same-slot content reuse independently of lease admission"
                );
                assert!(matches!(
                    pool.lease_file(NEW_FILE_GENERATION),
                    Err(ResidentLeaseError::StaleFile { .. })
                ));
                true
            }
            Err(ResidentLeaseError::StaleFile { .. }) => {
                assert_eq!(pool.resident_lease_count(), 0);
                false
            }
            Err(ResidentLeaseError::Exhausted { .. }) => {
                panic!("one modeled acquisition cannot exhaust the bounded count")
            }
        };

        if old_won {
            assert_eq!(pool.resident_lease_count(), 0);
            pool.poll_reopen(NEW_FILE_GENERATION, PAGE, 0x72);
            pool.poll_reopen(NEW_FILE_GENERATION, PAGE, 0x72);
        }

        assert!(matches!(
            pool.lease_file(OLD_FILE_GENERATION),
            Err(ResidentLeaseError::StaleFile { .. })
        ));
        assert!(
            pool.get_file(OLD_FILE_GENERATION, PAGE).is_none(),
            "the old capability cannot authorize the reused slot generation"
        );
        {
            let guard = pool
                .get_file(NEW_FILE_GENERATION, PAGE)
                .expect("bounded progress publishes the reopened generation");
            assert_eq!(guard.generation(), 0x72);
        }
        let _new_lease = pool
            .lease_file(NEW_FILE_GENERATION)
            .expect("after the winning order retires the old slot, only the new generation admits");
        assert_eq!(pool.resident_lease_count(), 1);
    });
}

#[test]
fn a_hint_racing_eviction_two_advances_and_reuse_never_reads_refilled_bytes() {
    const OLD_FILE_GENERATION: u32 = 1;
    const NEW_FILE_GENERATION: u32 = 2;
    const GRANULE: u32 = 7;
    const OLD_CONTENT: u8 = 0x31;
    const NEW_CONTENT: u8 = 0x72;

    loom::model(|| {
        let pool = PoolModel::new(1);
        pool.make_file_resident(OLD_FILE_GENERATION, GRANULE, OLD_CONTENT);
        let hint = pool
            .resident_hint(OLD_FILE_GENERATION, GRANULE)
            .expect("a resident exact page mints a hint before the race");

        let reader_pool = pool.clone();
        let reader = thread::spawn(move || {
            if let Some(guard) = reader_pool.get_with_hint(OLD_FILE_GENERATION, GRANULE, Some(hint))
            {
                assert_eq!(
                    guard.generation(),
                    OLD_CONTENT,
                    "a validated old hint may return only bytes guarded before eviction"
                );
                assert_eq!(
                    guard.generation(),
                    OLD_CONTENT,
                    "two epoch advances and reuse never change bytes observed through the old guard"
                );
                drop(guard);
            }
        });

        pool.evict_file(OLD_FILE_GENERATION, GRANULE);
        pool.poll_file_pass(NEW_FILE_GENERATION, GRANULE, NEW_CONTENT);
        pool.poll_file_pass(NEW_FILE_GENERATION, GRANULE, NEW_CONTENT);

        reader.join().expect("reader thread");
        for _ in 0..2 {
            pool.poll_file_pass(NEW_FILE_GENERATION, GRANULE, NEW_CONTENT);
        }
        assert!(
            pool.reader_is_quiescent(),
            "hint fallback or the last hinted guard drop leaves no phantom epoch pin"
        );
        match pool.get_with_hint(NEW_FILE_GENERATION, GRANULE, None) {
            Some(guard) => assert_eq!(
                guard.generation(),
                NEW_CONTENT,
                "the reused frame is reachable only through its new exact file generation"
            ),
            None => panic!("two passes refill the reused frame with the new exact page"),
        }
    });
}

fn retained_drop_producer(
    pool: Arc<PoolModel>,
    reader: u32,
    page: u32,
    ready_tx: mpsc::Sender<bool>,
    release_rx: mpsc::Receiver<()>,
) {
    let retained = pool
        .pin_reader(reader, page)
        .expect("the producer pins its resident frame")
        .into_retained()
        .ok();
    ready_tx
        .send(retained.is_some())
        .expect("report promotion outcome");
    release_rx.recv().expect("release retained handle");
    drop(retained);
}

fn nested_retained_drop_producer(
    pool: Arc<PoolModel>,
    ready_tx: mpsc::Sender<bool>,
    release_rx: mpsc::Receiver<()>,
) {
    let outer = pool
        .pin_reader(0, HELD_PAGE)
        .expect("the outer guard pins the resident frame");
    let retained = pool
        .pin_reader(0, HELD_PAGE)
        .expect("the nested guard shares the outer epoch")
        .into_retained()
        .ok();
    drop(outer);
    ready_tx
        .send(retained.is_some())
        .expect("report nested promotion");
    release_rx.recv().expect("release nested promotion");
    drop(retained);
}

fn promotion_maturity_producer(
    pool: Arc<PoolModel>,
    ready_tx: mpsc::Sender<()>,
    start_rx: mpsc::Receiver<()>,
    inspect_rx: mpsc::Receiver<()>,
    promoted_tx: mpsc::Sender<bool>,
    bytes_tx: mpsc::Sender<bool>,
) {
    let guard = pool
        .get_file(RETAINED_FILE_GENERATION, HELD_PAGE)
        .expect("the promoter pins before logical eviction");
    ready_tx.send(()).expect("report live guard");
    start_rx.recv().expect("start promotion race");
    let retained = guard.into_retained().ok();
    promoted_tx
        .send(retained.is_some())
        .expect("report promotion outcome");
    inspect_rx.recv().expect("inspect after maturity scans");
    let bytes_stable = retained.as_ref().is_some_and(|frame| frame[0] == HELD_GEN);
    bytes_tx.send(bytes_stable).expect("report retained bytes");
    drop(retained);
}

#[test]
fn promotion_publication_racing_maturity_never_reuses_retained_bytes() {
    retention_model(|| {
        let pool = PoolModel::with_retention(1, 1);
        pool.make_file_resident(RETAINED_FILE_GENERATION, HELD_PAGE, HELD_GEN);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (start_tx, start_rx) = mpsc::channel();
        let (inspect_tx, inspect_rx) = mpsc::channel();
        let (promoted_tx, promoted_rx) = mpsc::channel();
        let (bytes_tx, bytes_rx) = mpsc::channel();

        let producer_pool = pool.clone();
        let producer = thread::spawn(move || {
            promotion_maturity_producer(
                producer_pool,
                ready_tx,
                start_rx,
                inspect_rx,
                promoted_tx,
                bytes_tx,
            );
        });
        ready_rx.recv().expect("the pre-eviction guard is live");
        start_tx.send(()).expect("race promotion with eviction");

        pool.retire_file(RETAINED_FILE_GENERATION, HELD_PAGE);
        let first = pool.poll_reopen(REOPENED_FILE_GENERATION, HELD_PAGE, INTRUDER_GEN);
        let second = pool.poll_reopen(REOPENED_FILE_GENERATION, HELD_PAGE, INTRUDER_GEN);
        let promoted = promoted_rx.recv().expect("promotion outcome");
        let third = pool.poll_reopen(REOPENED_FILE_GENERATION, HELD_PAGE, INTRUDER_GEN);
        let matured = first.matured + second.matured + third.matured;
        let matured_freed = first.matured_freed + second.matured_freed + third.matured_freed;
        let held_before_drop = pool.frame_is_evicting(0);
        let reopened_while_retained = pool.get_file(REOPENED_FILE_GENERATION, HELD_PAGE).is_some();

        inspect_tx.send(()).expect("inspect retained bytes");
        let bytes_stable = bytes_rx.recv().expect("retained byte outcome");
        producer.join().expect("promotion producer");
        let _ = pool.poll_reopen(REOPENED_FILE_GENERATION, HELD_PAGE, INTRUDER_GEN);

        assert!(promoted, "the bounded first promotion must commit");
        assert_eq!(matured, 1, "the logical eviction reaches maturity once");
        assert_eq!(
            matured_freed, 0,
            "a retained matured frame never reaches Free"
        );
        assert!(held_before_drop, "the retained frame remains Evicting");
        assert!(
            !reopened_while_retained,
            "slot reuse waits until the retained owner drops"
        );
        assert!(
            bytes_stable,
            "maturity never reinstalls bytes under the retained owner"
        );
    });
}

#[test]
fn nested_promotion_and_concurrent_last_drops_have_one_release_owner() {
    retention_model(|| {
        let pool = PoolModel::with_retention(2, 1);
        pool.make_resident_in_frame(0, HELD_PAGE, HELD_GEN);
        pool.make_resident_in_frame(1, INTRUDER_PAGE, INTRUDER_GEN);
        let (first_ready_tx, first_ready_rx) = mpsc::channel();
        let (first_release_tx, first_release_rx) = mpsc::channel();
        let (second_ready_tx, second_ready_rx) = mpsc::channel();
        let (second_release_tx, second_release_rx) = mpsc::channel();

        let first_pool = pool.clone();
        let first = thread::spawn(move || {
            nested_retained_drop_producer(first_pool, first_ready_tx, first_release_rx);
        });
        assert!(first_ready_rx.recv().expect("nested promotion outcome"));

        let second_pool = pool.clone();
        let second = thread::spawn(move || {
            retained_drop_producer(
                second_pool,
                1,
                HELD_PAGE,
                second_ready_tx,
                second_release_rx,
            );
        });
        assert!(second_ready_rx.recv().expect("second promotion outcome"));
        pool.evict(HELD_PAGE);
        let first_maturity = pool.drain_matured_only();
        let second_maturity = pool.drain_matured_only();
        assert_eq!(first_maturity.matured + second_maturity.matured, 1);
        assert_eq!(
            first_maturity.matured_freed + second_maturity.matured_freed,
            0
        );

        first_release_tx.send(()).expect("release nested handle");
        second_release_tx.send(()).expect("release second handle");
        first.join().expect("nested producer");
        second.join().expect("second producer");

        let releases = pool.drain_releases_only();
        assert_eq!(releases, 1, "one count transition owns the release ticket");
        assert_eq!(pool.drain_releases_only(), 0, "no duplicate ticket remains");
    });
}

#[test]
fn concurrent_first_promotions_preserve_the_occupied_budget_floor() {
    retention_model(|| {
        let pool = PoolModel::with_retention(2, 1);
        pool.make_resident_in_frame(0, HELD_PAGE, HELD_GEN);
        pool.make_resident_in_frame(1, INTRUDER_PAGE, INTRUDER_GEN);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (first_release_tx, first_release_rx) = mpsc::channel();
        let (second_release_tx, second_release_rx) = mpsc::channel();

        let first_pool = pool.clone();
        let first_ready_tx = ready_tx.clone();
        let first = thread::spawn(move || {
            retained_drop_producer(first_pool, 0, HELD_PAGE, first_ready_tx, first_release_rx);
        });
        let second_pool = pool.clone();
        let second = thread::spawn(move || {
            retained_drop_producer(second_pool, 1, HELD_PAGE, ready_tx, second_release_rx);
        });

        let promotions = u32::from(ready_rx.recv().expect("one promotion outcome"))
            + u32::from(ready_rx.recv().expect("both promotion outcomes"));
        let distinct_guard = pool
            .pin_reader(0, INTRUDER_PAGE)
            .expect("the distinct frame remains resident");
        let distinct_attempt = distinct_guard.into_retained();
        let distinct_promoted = distinct_attempt.is_ok();
        drop(distinct_attempt);

        first_release_tx.send(()).expect("release first contender");
        second_release_tx
            .send(())
            .expect("release second contender");
        first.join().expect("first contender");
        second.join().expect("second contender");

        assert!(promotions > 0, "one concurrent first promotion must commit");
        assert!(
            !distinct_promoted,
            "a positive same-frame count keeps the sole budget unit occupied"
        );
    });
}

#[test]
fn two_ring_producers_overlap_consumer_and_turn_over_one_slot() {
    const TURNOVER_PAGE: u32 = 50;

    retention_model(|| {
        let pool = PoolModel::with_retention(2, 2);
        pool.make_resident_in_frame(0, HELD_PAGE, HELD_GEN);
        pool.make_resident_in_frame(1, INTRUDER_PAGE, INTRUDER_GEN);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (first_release_tx, first_release_rx) = mpsc::channel();
        let (second_release_tx, second_release_rx) = mpsc::channel();

        let first_pool = pool.clone();
        let first_ready_tx = ready_tx.clone();
        let first = thread::spawn(move || {
            retained_drop_producer(first_pool, 0, HELD_PAGE, first_ready_tx, first_release_rx);
        });
        let second_pool = pool.clone();
        let second = thread::spawn(move || {
            retained_drop_producer(second_pool, 1, INTRUDER_PAGE, ready_tx, second_release_rx);
        });

        assert!(ready_rx.recv().expect("one producer promotion"));
        assert!(ready_rx.recv().expect("both producer promotions"));
        pool.evict(HELD_PAGE);
        pool.evict(INTRUDER_PAGE);
        let first_maturity = pool.drain_matured_only();
        let second_maturity = pool.drain_matured_only();
        assert_eq!(first_maturity.matured + second_maturity.matured, 2);
        assert_eq!(
            first_maturity.matured_freed + second_maturity.matured_freed,
            0
        );

        first_release_tx.send(()).expect("start first ring push");
        second_release_tx.send(()).expect("start second ring push");
        let concurrent_scan = pool.drain_releases_only();
        first.join().expect("first ring producer");
        second.join().expect("second ring producer");
        let published_followup = pool.drain_releases_only();
        assert_eq!(concurrent_scan + published_followup, 2, "no ticket is lost");

        pool.make_resident_in_frame(0, TURNOVER_PAGE, 5);
        let turnover = pool
            .pin_reader(0, TURNOVER_PAGE)
            .expect("the turned-over frame pins")
            .into_retained()
            .ok()
            .expect("the consumed budget unit is reusable");
        pool.evict(TURNOVER_PAGE);
        let first_turnover_maturity = pool.drain_matured_only();
        let second_turnover_maturity = pool.drain_matured_only();
        assert_eq!(
            first_turnover_maturity.matured + second_turnover_maturity.matured,
            1
        );
        drop(turnover);
        assert_eq!(pool.drain_releases_only(), 1, "ticket two reuses slot zero");
    });
}

#[test]
fn poll_pass_drains_a_held_release_and_reuses_its_budget() {
    retention_model(|| {
        let pool = PoolModel::with_retention(1, 1);
        pool.make_resident(HELD_PAGE, HELD_GEN);
        let retained = pool
            .pin_reader(0, HELD_PAGE)
            .expect("the resident frame pins")
            .into_retained()
            .ok()
            .expect("the sole budget unit admits the retained frame");

        pool.evict(HELD_PAGE);
        let first_maturity = pool.poll_pass(INTRUDER_PAGE, INTRUDER_GEN);
        let second_maturity = pool.poll_pass(INTRUDER_PAGE, INTRUDER_GEN);
        assert_eq!(first_maturity.matured + second_maturity.matured, 1);
        assert_eq!(
            first_maturity.matured_freed + second_maturity.matured_freed,
            0
        );
        assert!(pool.frame_is_evicting(0));
        drop(retained);

        let release = pool.poll_pass(INTRUDER_PAGE, INTRUDER_GEN);
        let refilled = pool
            .pin_reader(0, INTRUDER_PAGE)
            .and_then(|guard| guard.into_retained().ok());
        let refilled_bytes = refilled
            .as_ref()
            .is_some_and(|frame| frame[0] == INTRUDER_GEN);
        drop(refilled);
        let cleanup_releases = pool.drain_releases_only();

        assert_eq!(release.first, Some(DrainSource::Release));
        assert_eq!(release.released, 1, "the full poll drains one release");
        assert_eq!(release.matured, 0, "the matured queue was already empty");
        assert_eq!(release.matured_freed, 0);
        assert!(
            refilled_bytes,
            "the directly freed frame is refilled and its budget is reusable"
        );
        assert_eq!(
            cleanup_releases, 0,
            "the full poll leaves no release behind"
        );
    });
}

#[test]
fn drain_driver_releases_before_maturity_and_direct_frees() {
    const PLAIN_PAGE: u32 = 30;
    const BLOCKER_PAGE: u32 = 40;

    retention_model(|| {
        let pool = PoolModel::with_retention(3, 1);
        pool.make_resident_in_frame(0, HELD_PAGE, HELD_GEN);
        pool.make_resident_in_frame(1, PLAIN_PAGE, 3);
        pool.make_resident_in_frame(2, BLOCKER_PAGE, 4);
        let retained = pool
            .pin_reader(0, HELD_PAGE)
            .expect("the retained frame pins")
            .into_retained()
            .ok()
            .expect("the sole budget unit admits the retained frame");

        pool.evict(HELD_PAGE);
        let first_maturity = pool.drain_matured_only();
        let second_maturity = pool.drain_matured_only();
        assert_eq!(first_maturity.matured + second_maturity.matured, 1);
        assert_eq!(
            first_maturity.matured_freed + second_maturity.matured_freed,
            0
        );

        pool.evict(PLAIN_PAGE);
        let _ = pool.advance_epoch_only();
        let _ = pool.advance_epoch_only();
        let blocker = pool
            .pin_reader(1, BLOCKER_PAGE)
            .expect("the unrelated guard blocks a second grace period");
        drop(retained);

        let report = pool.drain_driver();
        let released_directly = pool.frame_is_free(0);
        let plain_freed = pool.frame_is_free(1);
        let cleanup_releases = pool.drain_releases_only();
        drop(blocker);

        assert_eq!(report.first, Some(DrainSource::Release));
        assert_eq!(report.released, 1, "the release ring drains first");
        assert_eq!(cleanup_releases, 0, "the driver leaves no release behind");
        assert_eq!(report.matured_freed, 1, "maturity follows release drain");
        assert!(released_directly, "HELD release reaches Free directly");
        assert!(plain_freed, "the plain matured frame reaches Free");
    });
}

#[test]
fn promotion_after_model_retirement_observation_is_refused() {
    retention_model(|| {
        let pool = PoolModel::with_retention(1, 1);
        pool.make_resident(HELD_PAGE, HELD_GEN);
        let (guard_ready_tx, guard_ready_rx) = mpsc::channel();
        let (promote_tx, promote_rx) = mpsc::channel();
        let (outcome_tx, outcome_rx) = mpsc::channel();

        let promotion_pool = pool.clone();
        let promotion = thread::spawn(move || {
            let guard = promotion_pool
                .pin_reader(0, HELD_PAGE)
                .expect("a pre-retirement guard remains live");
            guard_ready_tx.send(()).expect("guard is ready");
            promote_rx.recv().expect("retirement is published");
            let refused_retiring = match guard.into_retained() {
                Ok(retained) => {
                    drop(retained);
                    false
                }
                Err(refusal) => {
                    let refused_retiring =
                        matches!(refusal.reason, RetainRefusedReason::FileRetiring);
                    drop(refusal.guard);
                    refused_retiring
                }
            };
            outcome_tx
                .send(refused_retiring)
                .expect("report promotion outcome");
        });

        guard_ready_rx.recv().expect("pre-retirement guard ready");
        let retirement_pool = pool.clone();
        thread::spawn(move || retirement_pool.begin_model_file_retirement())
            .join()
            .expect("retirement publisher");
        promote_tx
            .send(())
            .expect("start promotion after retirement");
        let refused_retiring = outcome_rx.recv().expect("promotion outcome");
        promotion.join().expect("promotion thread");

        assert!(
            refused_retiring,
            "an Acquire-observed retirement rolls promotion back as FileRetiring"
        );
    });
}

#[test]
fn held_file_release_refills_only_the_new_generation() {
    retention_model(|| {
        let pool = PoolModel::with_retention(1, 1);
        pool.make_file_resident(RETAINED_FILE_GENERATION, HELD_PAGE, HELD_GEN);
        let retained = pool
            .get_file(RETAINED_FILE_GENERATION, HELD_PAGE)
            .expect("the file-backed frame is resident")
            .into_retained()
            .ok()
            .expect("the sole budget unit admits the retained frame");

        pool.evict_file(RETAINED_FILE_GENERATION, HELD_PAGE);
        let first = pool.poll_file_pass(REOPENED_FILE_GENERATION, HELD_PAGE, INTRUDER_GEN);
        let second = pool.poll_file_pass(REOPENED_FILE_GENERATION, HELD_PAGE, INTRUDER_GEN);
        assert_eq!(first.matured + second.matured, 1);
        assert_eq!(first.matured_freed + second.matured_freed, 0);
        drop(retained);

        let release = pool.poll_file_pass(REOPENED_FILE_GENERATION, HELD_PAGE, INTRUDER_GEN);
        assert_eq!(release.first, Some(DrainSource::Release));
        assert_eq!(release.released, 1);
        let old_authoritative = pool.get_file(RETAINED_FILE_GENERATION, HELD_PAGE).is_some();
        let reopened_content = pool
            .get_file(REOPENED_FILE_GENERATION, HELD_PAGE)
            .map(|guard| guard.generation());

        assert_eq!(
            (old_authoritative, reopened_content),
            (false, Some(INTRUDER_GEN)),
            "release-path refill must replace old file authority with the reopened generation"
        );
    });
}
