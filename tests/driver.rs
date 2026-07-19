//! Driver-surface contract (T002) through the seeded mock — the only backend
//! under test (DST seam).
//!
//! Three ADT choices pinned here (T002 latitude): a stale/closed `FileHandle`
//! generation is rejected at submit as `SubmitError::StaleHandle`; the blocking
//! metadata wrappers take `&FileHandle` — no bare raw fd crosses the API (AGENTS
//! naming rule); and slot reuse is observed through the opaque predicate
//! `FileId::aliases_slot`, never a bare-`u32` slot/generation accessor.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use dios::driver::{CompletionBatch, OpKind, OpToken, SubmitError, SyncMode};
use dios::testing::{Injected, MockDriver, ReadFrameIdx};
use dios::DirectIo;

const FRAME_BYTES: u32 = 4096;
const EINTR: i32 = 4;
const EIO: i32 = 5;
#[cfg(target_os = "linux")]
const EAGAIN: i32 = 11;
#[cfg(not(target_os = "linux"))]
const EAGAIN: i32 = 35;

fn mock(seed: u64, queue_capacity: u32, frames: u32) -> MockDriver {
    mock_with_write_slots(seed, queue_capacity, frames, 1)
}

fn mock_with_write_slots(
    seed: u64,
    queue_capacity: u32,
    frames: u32,
    write_slots: u32,
) -> MockDriver {
    MockDriver::builder()
        .seed(seed)
        .queue_capacity(queue_capacity)
        .frames(frames)
        .frame_bytes(FRAME_BYTES)
        .write_slots(write_slots)
        .retry_bound(3)
        .build()
}

fn open(m: &MockDriver) -> dios::driver::FileHandle {
    m.open(Path::new("seg-000000"), DirectIo::Disabled)
        .expect("mock open never touches disk")
}

/// Drains completions across polls until `expected` are collected, returning
/// their tokens in drain order.
fn drain_tokens(m: &MockDriver, expected: usize) -> Vec<OpToken> {
    let mut out = CompletionBatch::with_capacity(expected.max(1));
    let mut tokens = Vec::with_capacity(expected);
    let mut polls = 0u32;
    while tokens.len() < expected {
        m.poll(&mut out);
        for c in &out {
            tokens.push(c.token());
        }
        polls += 1;
        assert!(polls < 64, "poll made no progress draining completions");
    }
    tokens
}

/// Polls exactly one completion and returns its result as `Copy` data:
/// `Ok(bytes)` or `Err(errno)`.
fn poll_one(m: &MockDriver) -> Result<u32, i32> {
    let mut out = CompletionBatch::with_capacity(1);
    let n = m.poll(&mut out);
    assert_eq!(n, 1, "expected exactly one ready completion");
    let c = out.iter().next().expect("one completion");
    match c.result() {
        Ok(bytes) => Ok(bytes),
        Err(e) => Err(e.raw_os_error().unwrap_or(-1)),
    }
}

#[test]
fn open_yields_a_live_generational_file_handle() {
    let m = mock(1, 4, 4);
    let fd = open(&m);
    assert!(
        !m.is_closed(fd.file_id()),
        "a freshly opened fd is not closed"
    );
}

#[test]
fn submit_read_token_is_echoed_in_the_drained_completion() {
    let m = mock(1, 4, 4);
    let fd = open(&m);
    let token = m
        .submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("submit within capacity");

    let mut out = CompletionBatch::with_capacity(4);
    let n = m.poll(&mut out);
    assert_eq!(n, 1);
    let c = out.iter().next().expect("one completion");
    assert_eq!(c.token(), token, "the driver-issued token is echoed back");
    assert_eq!(c.kind(), OpKind::Read);
    assert!(c.result().is_ok());
}

#[test]
fn write_and_fsync_completions_carry_their_op_kinds() {
    let m = mock(1, 4, 4);
    let fd = open(&m);
    let arena = m.write_arena();
    let slot = arena.alloc().expect("one staging slot");

    let Ok(write_token) = m.submit_write(&fd, slot, 0) else {
        panic!("write submits within capacity");
    };
    let fsync_token = m
        .submit_fsync(&fd, SyncMode::Full)
        .expect("fsync submits within capacity");

    let mut kind_of: HashMap<OpToken, OpKind> = HashMap::new();
    let mut out = CompletionBatch::with_capacity(4);
    let mut polls = 0u32;
    while kind_of.len() < 2 {
        m.poll(&mut out);
        for c in &out {
            kind_of.insert(c.token(), c.kind());
        }
        polls += 1;
        assert!(polls < 64, "poll made no progress draining completions");
    }
    assert_eq!(kind_of.get(&write_token), Some(&OpKind::Write));
    assert_eq!(kind_of.get(&fsync_token), Some(&OpKind::Fsync));
}

#[test]
fn queue_full_is_typed_backpressure_that_recovers_after_a_poll() {
    let m = mock(1, 1, 2);
    let fd = open(&m);

    let first = m
        .submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("one slot fits");
    let start = Instant::now();
    let refused = m.submit_read(&fd, ReadFrameIdx::new(1), u64::from(FRAME_BYTES));
    let elapsed = start.elapsed();
    assert!(
        matches!(refused, Err(SubmitError::Full)),
        "a fixed init-time capacity backpressures rather than blocking or growing"
    );
    assert!(
        elapsed < Duration::from_millis(100),
        "backpressure returns at once, never waiting on queue space: {elapsed:?}"
    );

    assert_eq!(drain_tokens(&m, 1), vec![first]);

    m.submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("slot reclaimed at drain, capacity recovered");
}

#[test]
fn a_reclaimed_slot_issues_a_fresh_non_aliasing_token() {
    let m = mock(1, 1, 2);
    let fd = open(&m);

    let first = m.submit_read(&fd, ReadFrameIdx::new(0), 0).expect("fits");
    assert!(matches!(
        m.submit_read(&fd, ReadFrameIdx::new(1), u64::from(FRAME_BYTES)),
        Err(SubmitError::Full)
    ));
    assert_eq!(drain_tokens(&m, 1), vec![first]);

    let reused = m
        .submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("same slot, reissued");
    assert_ne!(
        first, reused,
        "capacity 1 forces the same slot; a differing token can only mean a generation bump (ABA-safe)"
    );
}

/// Maps each completion back to its submission index via the driver-issued
/// tokens, yielding the drain-order permutation for a given seed.
fn completion_schedule(seed: u64) -> Vec<usize> {
    let m = mock(seed, 16, 16);
    let fd = open(&m);
    let mut index_of: HashMap<OpToken, usize> = HashMap::new();
    for i in 0..8u32 {
        let token = m
            .submit_read(
                &fd,
                ReadFrameIdx::new(i),
                u64::from(i) * u64::from(FRAME_BYTES),
            )
            .expect("submit within capacity");
        index_of.insert(token, i as usize);
    }
    drain_tokens(&m, 8)
        .into_iter()
        .map(|t| index_of[&t])
        .collect()
}

#[test]
fn seeded_completions_drain_out_of_submission_order() {
    let schedule = completion_schedule(0x00D5_7EED);
    let identity: Vec<usize> = (0..8).collect();
    assert_ne!(
        schedule, identity,
        "the mock reorders completions relative to submission order"
    );

    let mut sorted = schedule.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted, identity,
        "reordering is a permutation — nothing lost"
    );
}

#[test]
fn the_same_seed_reproduces_the_same_schedule() {
    assert_eq!(
        completion_schedule(0x00D5_7EED),
        completion_schedule(0x00D5_7EED),
        "a fixed seed makes the adversarial schedule deterministic"
    );
}

#[test]
fn different_seeds_produce_different_schedules() {
    assert_ne!(
        completion_schedule(0x00D5_7EED),
        completion_schedule(0x00B1_6B00),
        "the schedule is seed-derived, not a fixed shuffle (a PRNG collision across \
         8 ops / 40320 permutations is negligible; a real collision means picking another pair)"
    );
}

#[test]
fn close_is_deferred_until_the_fds_in_flight_ops_drain() {
    let m = mock(1, 4, 4);
    let fd = open(&m);
    let id = fd.file_id();

    let token = m.submit_read(&fd, ReadFrameIdx::new(0), 0).expect("submit");
    m.close(fd);
    assert!(
        !m.is_closed(id),
        "close(2) must not be issued while an op is in flight on the fd (INV-11)"
    );

    assert_eq!(
        drain_tokens(&m, 1),
        vec![token],
        "the op on the closing fd still completes"
    );
    assert!(
        m.is_closed(id),
        "close(2) is observable only after the fd drains"
    );
}

#[test]
fn submit_on_a_stale_handle_is_rejected_while_the_reused_slot_still_works() {
    let m = mock(1, 4, 4);
    let fd = open(&m);
    let ghost = m.duplicate_handle(&fd);
    let ghost_id = fd.file_id();

    m.close(fd);
    let mut idle = CompletionBatch::with_capacity(1);
    m.poll(&mut idle);
    assert!(m.is_closed(ghost_id), "close(2) issued once the fd drained");

    let reopened = open(&m);
    assert!(
        reopened.file_id().aliases_slot(&ghost_id),
        "the freed fd slot is reused"
    );

    let live = m
        .submit_read(&reopened, ReadFrameIdx::new(0), 0)
        .expect("the live generation on the reused slot submits fine");
    assert!(
        matches!(
            m.submit_read(&ghost, ReadFrameIdx::new(1), u64::from(FRAME_BYTES)),
            Err(SubmitError::StaleHandle)
        ),
        "the stale generation is rejected — generational, not mere closed-set membership"
    );
    assert_eq!(
        drain_tokens(&m, 1),
        vec![live],
        "only the live-handle op was ever issued"
    );
}

#[test]
fn a_consumed_write_slot_is_freed_only_at_completion_drain() {
    let m = mock(1, 4, 4);
    let fd = open(&m);
    let arena = m.write_arena();

    let slot = arena.alloc().expect("one slot");
    let Ok(token) = m.submit_write(&fd, slot, 0) else {
        panic!("submits within capacity");
    };
    assert!(
        arena.alloc().is_none(),
        "the slot stays leased while its write is in flight (INV-11)"
    );

    assert_eq!(drain_tokens(&m, 1), vec![token]);
    assert!(
        arena.alloc().is_some(),
        "the arena slot returns to Free at completion drain"
    );
}

#[test]
fn a_dropped_unsubmitted_write_slot_is_freed_immediately() {
    let m = mock(1, 4, 4);
    let arena = m.write_arena();
    {
        let _slot = arena.alloc().expect("one slot");
    }
    assert!(
        arena.alloc().is_some(),
        "dropping a slot without submitting returns it to Free at once"
    );
}

#[test]
fn a_full_queue_hands_the_write_slot_back_in_the_error_arm() {
    let m = mock_with_write_slots(1, 1, 2, 2);
    let fd = open(&m);
    let arena = m.write_arena();

    let blocker = m
        .submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("one slot fits");
    let slot = arena.alloc().expect("staging slot");

    let Err((err, slot)) = m.submit_write(&fd, slot, 0) else {
        panic!("the queue is at capacity and must reject");
    };
    assert_eq!(err, SubmitError::Full);

    assert_eq!(drain_tokens(&m, 1), vec![blocker]);
    let Ok(token) = m.submit_write(&fd, slot, 0) else {
        panic!("the recovered slot resubmits once capacity frees");
    };
    assert_eq!(drain_tokens(&m, 1), vec![token]);
}

#[test]
fn a_stale_handle_hands_the_write_slot_back_unchanged() {
    let m = mock(1, 2, 2);
    let fd = open(&m);
    let ghost = m.duplicate_handle(&fd);
    m.close(fd);
    let live = open(&m);
    let arena = m.write_arena();
    let slot = arena.alloc().expect("one staging slot");

    let Err((SubmitError::StaleHandle, slot)) = m.submit_write(&ghost, slot, 0) else {
        panic!("the stale generation returns its unconsumed staging slot");
    };
    let token = m
        .submit_write(&live, slot, 0)
        .expect("the returned slot submits unchanged on the live generation");
    assert_eq!(drain_tokens(&m, 1), vec![token]);
}

#[test]
fn a_foreign_driver_write_slot_panics_before_consumption() {
    let owner = mock(1, 1, 1);
    let foreign = mock(2, 1, 1);
    let fd = open(&foreign);
    let arena = owner.write_arena();
    let slot = arena.alloc().expect("owner staging slot");

    let rejected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = foreign.submit_write(&fd, slot, 0);
    }));
    assert!(rejected.is_err(), "foreign owner identity is rejected");
    assert!(
        arena.alloc().is_some(),
        "the panic path drops the still-unconsumed slot back to its owner"
    );
}

#[test]
fn eintr_is_resubmitted_internally_on_reads() {
    let m = mock(1, 4, 4);
    let fd = open(&m);
    m.inject_next(Injected::Eintr);
    m.inject_next(Injected::Io(EIO));

    m.submit_read(&fd, ReadFrameIdx::new(0), 0).expect("submit");
    assert_eq!(
        poll_one(&m),
        Err(EIO),
        "the read resubmitted past EINTR — the second attempt consumed the injected EIO"
    );
}

#[test]
fn eintr_is_resubmitted_internally_on_writes() {
    let m = mock(1, 4, 4);
    let fd = open(&m);
    let arena = m.write_arena();
    let slot = arena.alloc().expect("slot");
    m.inject_next(Injected::Eintr);
    m.inject_next(Injected::Io(EIO));

    assert!(m.submit_write(&fd, slot, 0).is_ok(), "submit");
    assert_eq!(
        poll_one(&m),
        Err(EIO),
        "the write resubmitted past EINTR — the second attempt consumed the injected EIO"
    );
}

#[test]
fn eintr_is_resubmitted_internally_on_fsync() {
    let m = mock(1, 4, 4);
    let fd = open(&m);
    m.inject_next(Injected::Eintr);
    m.inject_next(Injected::Io(EIO));

    m.submit_fsync(&fd, SyncMode::Full).expect("submit");
    assert_eq!(
        poll_one(&m),
        Err(EIO),
        "fsync resubmitted past EINTR — the second attempt consumed the injected EIO"
    );
}

#[test]
fn eintr_is_resubmitted_inside_the_blocking_wrappers() {
    let m = mock(1, 4, 4);
    let fd = open(&m);
    let payload = [0u8; 64];

    m.inject_next(Injected::Eintr);
    m.inject_next(Injected::Io(EIO));
    let write_err = m
        .write_all_blocking(&fd, &payload, 0)
        .expect_err("EINTR then EIO surfaces EIO, so the blocking write must have retried");
    assert_eq!(write_err.raw_os_error(), Some(EIO));

    m.inject_next(Injected::Eintr);
    m.inject_next(Injected::Io(EIO));
    let fsync_err = m
        .fsync_blocking(&fd, SyncMode::Full)
        .expect_err("EINTR then EIO surfaces EIO, so the blocking fsync must have retried");
    assert_eq!(fsync_err.raw_os_error(), Some(EIO));
}

#[test]
fn eintr_beyond_the_retry_bound_surfaces_the_eintr_error() {
    let m = mock(1, 4, 4);
    let retry_bound_exceeded = 4;
    let fd = open(&m);
    for _ in 0..retry_bound_exceeded {
        m.inject_next(Injected::Eintr);
    }

    m.submit_read(&fd, ReadFrameIdx::new(0), 0).expect("submit");
    assert_eq!(
        poll_one(&m),
        Err(EINTR),
        "a fixed init-time retry bound stops resubmitting and surfaces EINTR"
    );
}

#[test]
fn eagain_is_resubmitted_on_reads() {
    let m = mock(1, 4, 4);
    let fd = open(&m);
    m.inject_next(Injected::Eagain);
    m.inject_next(Injected::Io(EIO));

    m.submit_read(&fd, ReadFrameIdx::new(0), 0).expect("submit");
    assert_eq!(
        poll_one(&m),
        Err(EIO),
        "a blocking-file EAGAIN is retried on reads, so the second attempt's EIO surfaces"
    );
}

#[test]
fn eagain_surfaces_as_an_error_on_writes_without_retry() {
    let m = mock(1, 4, 4);
    let fd = open(&m);
    let arena = m.write_arena();
    let slot = arena.alloc().expect("slot");
    m.inject_next(Injected::Eagain);
    m.inject_next(Injected::Io(EIO));

    assert!(m.submit_write(&fd, slot, 0).is_ok(), "submit");
    assert_eq!(
        poll_one(&m),
        Err(EAGAIN),
        "EAGAIN on a write surfaces directly; the queued EIO is never reached, proving no retry"
    );
}

#[test]
fn a_short_read_is_surfaced_not_resubmitted_by_the_driver() {
    let m = mock(1, 4, 4);
    let fd = open(&m);
    m.inject_next(Injected::Short(512));

    let _ = m.submit_read(&fd, ReadFrameIdx::new(0), 0).expect("submit");
    assert_eq!(
        poll_one(&m),
        Ok(512),
        "the driver reports the partial byte count; reslicing is the pool's job, not the driver's"
    );
}

#[test]
fn blocking_metadata_wrappers_complete_and_surface_io_errors() {
    let m = mock(1, 4, 4);
    let fd = open(&m);
    let payload = [0xABu8; 64];

    m.write_all_blocking(&fd, &payload, 0)
        .expect("a clean blocking write completes synchronously");
    m.fsync_blocking(&fd, SyncMode::Full)
        .expect("a clean blocking fsync completes synchronously");

    m.inject_next(Injected::Io(EIO));
    let write_err = m
        .write_all_blocking(&fd, &payload, 0)
        .expect_err("an injected IO failure surfaces as IoError");
    assert_eq!(write_err.raw_os_error(), Some(EIO));

    m.inject_next(Injected::Io(EIO));
    let fsync_err = m
        .fsync_blocking(&fd, SyncMode::Full)
        .expect_err("fsync surfaces IO failures too");
    assert_eq!(fsync_err.raw_os_error(), Some(EIO));
}

#[test]
fn poll_is_non_blocking_and_poll_wait_bounds_its_idle_wait() {
    let m = mock(1, 4, 4);
    let mut out = CompletionBatch::with_capacity(4);

    let poll_start = Instant::now();
    let ready = m.poll(&mut out);
    let poll_elapsed = poll_start.elapsed();
    assert_eq!(ready, 0, "an idle poll drains nothing");
    assert!(
        poll_elapsed < Duration::from_millis(10),
        "poll never sleeps awaiting events: {poll_elapsed:?}"
    );

    let timeout = Duration::from_millis(100);
    let start = Instant::now();
    let drained = m.poll_wait(&mut out, timeout);
    let elapsed = start.elapsed();
    assert_eq!(drained, 0, "poll_wait drains nothing when idle");
    assert!(
        elapsed >= Duration::from_millis(60),
        "poll_wait honors its timeout instead of returning immediately: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "poll_wait returns promptly once the timeout elapses: {elapsed:?}"
    );
}

#[test]
fn submit_does_not_wait_on_a_poller_parked_in_poll_wait() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;

    let m = Arc::new(mock(1, 4, 4));
    let entered = Arc::new(AtomicBool::new(false));
    let parked = {
        let m = Arc::clone(&m);
        let entered = Arc::clone(&entered);
        thread::spawn(move || {
            let mut out = CompletionBatch::with_capacity(4);
            entered.store(true, Ordering::Release);
            m.poll_wait(&mut out, Duration::from_secs(2));
        })
    };
    while !entered.load(Ordering::Acquire) {
        std::hint::spin_loop();
    }
    thread::sleep(Duration::from_millis(50));

    let fd = open(&m);
    let start = Instant::now();
    let submitted = m.submit_read(&fd, ReadFrameIdx::new(0), 0);
    let elapsed = start.elapsed();

    assert!(submitted.is_ok());
    assert!(
        elapsed < Duration::from_millis(500),
        "submit's SQE-fill critical section is outside the poll_wait kernel wait (AD-4/INV-3): {elapsed:?}"
    );
    parked.join().expect("poller thread joins");
}
