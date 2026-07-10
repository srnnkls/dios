//! T007 reader-registration lifecycle pins (EBR slot accounting).
//!
//! These exercise ONLY the landed `Pool::register_reader` surface plus the NEW
//! `Drop for ReaderCtx` that T007 adds: dropping a `ReaderCtx` must release its
//! registration slot (design.md "Slots deregister via TLS destructor/RAII"; the
//! T007 task note "current cap counter never decrements"). This file compiles
//! against today's surface and is RED at RUNTIME — the drop-frees-a-slot and
//! cycle-to-capacity tests fail until the release-on-drop lands.
//!
//! The publish/validate/reclaim EBR machinery lives in `epoch_guards.rs`; the
//! !Send/!Sync marker pins live in `guard_compile_fail.rs`.

use dios::{Pool, RegisterError};

/// A watermark-satisfying pool with `max_readers` registration slots. The
/// registration cap is the only knob under test here, so the frame budget just
/// clears the INV-9 watermark for that reader count.
fn pool_with_reader_slots(max_readers: u32) -> Pool {
    let peak = 1u32;
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
fn registration_beyond_capacity_returns_the_typed_at_capacity_error() {
    let pool = pool_with_reader_slots(2);
    let _r1 = pool.register_reader().expect("first slot");
    let _r2 = pool.register_reader().expect("second slot");

    let err = pool
        .register_reader()
        .expect_err("a third registration exceeds the two-slot cap");
    assert_eq!(
        err,
        RegisterError::AtCapacity {
            max_concurrent_readers: 2,
        },
        "over-capacity registration is a typed error carrying the cap"
    );
}

#[test]
fn dropping_a_reader_context_reopens_its_registration_slot() {
    let pool = pool_with_reader_slots(1);
    let r1 = pool.register_reader().expect("the single slot");
    assert!(
        pool.register_reader().is_err(),
        "the one slot is occupied while r1 lives"
    );

    drop(r1);

    pool.register_reader()
        .expect("dropping the sole reader releases its slot for a fresh registration");
}

#[test]
fn register_drop_reregister_cycles_to_capacity_without_leaking_slots() {
    let max_readers = 3u32;
    let pool = pool_with_reader_slots(max_readers);

    for cycle in 0..12u32 {
        let mut held = Vec::with_capacity(max_readers as usize);
        for slot in 0..max_readers {
            let reader = pool.register_reader().unwrap_or_else(|err| {
                panic!("cycle {cycle} slot {slot} must re-register after prior drops, got {err:?}")
            });
            held.push(reader);
        }
        assert!(
            pool.register_reader().is_err(),
            "cycle {cycle}: capacity is reached once all {max_readers} slots are held"
        );
        drop(held);
    }
}
