//! T009 pool concurrency proofs (loom). Compiled only under `--cfg loom`; the
//! normal build sees an empty crate, so the loom dev-dependency never touches
//! the shipping build (Cargo.toml gates it on `cfg(loom)`). Run with:
//!   `RUSTFLAGS="--cfg loom" cargo test --features mock --test loom_pool`.
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
//!   #[derive(Clone, Copy)]
//!   pub struct ResidentHint;             // advisory exact-page mapping/stamp for the modeled file
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
//!     pub fn poll_pass(&self, refill_page: u32, refill_gen: u8);
//!     // writer under the Control lock: remap `page` to a FRESH frame (frame 1) carrying `gen`,
//!     // committed as ONE seqlock transaction (version bump around the multi-field cell write).
//!     pub fn remap(&self, page: u32, gen: u8);
//!     // reader: a lock-free advisory seqlock read; None = empty/retry, Some = a non-torn snapshot.
//!     pub fn probe(&self, page: u32) -> Option<Snapshot>;
//!     // These three hint/retirement operations delegate to the mock-gated real
//!     // production hint/file-liveness, table, frame-state, and EBR primitives;
//!     // they are not a parallel model-only implementation. The bounded model
//!     // assumes a live typed lease and represents its exact file/page identity
//!     // through `page`; it does not model lease guard counters or identity beyond
//!     // that PageId contract.
//!     // Hint admission closes when the modeled file retires.
//!     pub fn resident_hint(&self, page: u32) -> Option<ResidentHint>;
//!     // Models production pin validation of the typed lease/file, exact PageId
//!     // mapping, and frame stamp.
//!     pub fn pin_resident_hint(&self, page: u32, hint: ResidentHint) -> Option<Guard>;
//!     // An already-minted capability under a live lease remains pinnable while
//!     // its exact PageId mapping and stamp remain valid.
//!     pub fn retire_file(&self);
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

use dios::loom_model::PoolModel;
use loom::thread;

const HELD_PAGE: u32 = 10;
const INTRUDER_PAGE: u32 = 20;
const HELD_GEN: u8 = 1;
const INTRUDER_GEN: u8 = 2;

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
fn resident_hint_get_vs_retire() {
    loom::model(|| {
        let pool = PoolModel::new(1);
        pool.make_resident(HELD_PAGE, HELD_GEN);
        let hint = pool
            .resident_hint(HELD_PAGE)
            .expect("the resident page yields an advisory hint");
        let live = pool
            .pin_resident_hint(HELD_PAGE, hint)
            .expect("a live hint deterministically pins before retirement starts");
        assert_eq!(live.generation(), HELD_GEN);
        drop(live);

        let reader_pool = pool.clone();
        let reader = thread::spawn(move || {
            let guard = reader_pool
                .pin_resident_hint(HELD_PAGE, hint)
                .expect("a hint minted before retirement remains pinnable during the race");
            assert_eq!(
                guard.generation(),
                HELD_GEN,
                "a hint admitted before retirement keeps the exact old bytes stable"
            );
        });

        pool.retire_file();
        reader.join().expect("reader thread");
        assert!(
            pool.resident_hint(HELD_PAGE).is_none(),
            "retirement closes admission of new hints for the modeled file generation"
        );
        let retained = pool
            .pin_resident_hint(HELD_PAGE, hint)
            .expect("an already-minted hint remains a valid typed capability after retirement");
        assert_eq!(
            retained.generation(),
            HELD_GEN,
            "a minted capability under its live lease retains the exact-page generation after retirement"
        );
    });
}

#[test]
fn resident_hint_eviction_reuse() {
    loom::model(|| {
        let pool = PoolModel::new(1);
        pool.make_resident(HELD_PAGE, HELD_GEN);
        let stale = pool
            .resident_hint(HELD_PAGE)
            .expect("the original residency yields a hint");

        let reader_pool = pool.clone();
        let reader = thread::spawn(move || {
            if let Some(guard) = reader_pool.pin_resident_hint(HELD_PAGE, stale) {
                assert_eq!(
                    guard.generation(),
                    HELD_GEN,
                    "a racing pin sees the original generation, never reused frame bytes"
                );
            }
        });

        pool.evict(HELD_PAGE);
        pool.poll_pass(HELD_PAGE, INTRUDER_GEN);
        pool.poll_pass(HELD_PAGE, INTRUDER_GEN);
        reader.join().expect("reader thread");
        pool.poll_pass(HELD_PAGE, INTRUDER_GEN);
        pool.poll_pass(HELD_PAGE, INTRUDER_GEN);

        assert!(
            pool.pin_resident_hint(HELD_PAGE, stale).is_none(),
            "a stale hint cannot alias a frame after eviction and reuse"
        );
        let fresh = pool
            .resident_hint(HELD_PAGE)
            .expect("the refilled page yields a fresh stamp for its new generation");
        let guard = pool
            .pin_resident_hint(HELD_PAGE, fresh)
            .expect("the fresh stamp pins the refilled page");
        assert_eq!(guard.generation(), INTRUDER_GEN);
    });
}
