//! T005 fault injection at the syscall boundary through the `io_uring` REAP path.
//!
//! The eager-shaped fault rows (short read, `EAGAIN`, `EINTR`, stale-handle,
//! single-op deferred close) are already pinned against the eager `attempt` path
//! in `tests/driver.rs`. The scope's verification pillar makes syscall-boundary
//! fault injection apply to BOTH backends, but the landed `MockDriver` drives
//! only the eager path — the ring reap path (`fill_ring` /
//! `RingExecutor::reap` routing a `(user_data, raw)` into the finalize sink) has
//! no injectable seam and, off the bench host, no coverage. This file pins it on
//! ANY platform by naming a seam that does not exist yet, so the whole file is
//! COMPILE-RED on the missing surface until T005's implementer lands it.
//!
//! ## Seam contract (implementer's obligation)
//!
//! A public, seeded, cross-platform `dios::mock::MockRingDriver` (feature = "mock")
//! that composes the REAL shared `DriverCore` ring poll path — `poll_ring` /
//! `fill_ring` / `reap_ring` — over a mock `RingExecutor`. The mock supplies ONLY
//! seeded CQE ordering and injected raw CQE results; the completion routing, slab
//! reclaim, deferred-close progression, and `EAGAIN`/`EINTR` resubmit are the real
//! `DriverCore`'s, exercised unchanged. The implementer lifts the `cfg(linux)`
//! gate on `RingExecutor`/`poll_ring` for the mock build so this runs off-linux.
//! A mock that REPLICATES `reap_ring`'s finalize locally is NOT acceptable: a
//! correct replica passes while the real `reap_ring` is broken (a mock tautology).
//!
//! The seeded CQE ordering is CONTRACTUAL: the mock's `Executor::schedule` is the
//! same `splitmix64`-driven reorder the eager `MockExecutor` already ships (each
//! submit inserts at `splitmix64(rng) % (ready_len + 1)`), so the reap permutation
//! is a deterministic function of `(seed, op_count)`. The seed-determinism and
//! seed-divergence tests below rely on that contract; their reference permutations
//! were computed against it (e.g. seed `0x00D5_7EED`, 6 ops -> drain `[5,2,3,0,4,1]`).
//!
//! ## Fault binding — by submission identity, not reap order
//!
//! `inject_for_next_submit(&[Injected])` binds a fault SEQUENCE to the op the NEXT
//! submit creates, keyed to that op's `user_data`. A seed that reorders CQE
//! delivery must still land each op's result on its OWN token — faults are NOT
//! consumed in reap order (positional routing would pass a reap-order design).
//!
//! Per-attempt `Injected` -> raw CQE mapping on the bound op:
//!   - fault-free / exhausted sequence -> raw = `frame_bytes` (clean full-frame read)
//!   - `Injected::Short(n)`  -> raw = n       (>= 0)  short CQE, surfaced not resliced
//!   - `Injected::Io(errno)` -> raw = -errno  (< 0)   TERMINAL error CQE -> typed `IoError`
//!   - `Injected::Eagain`    -> raw = -EAGAIN (< 0)   RETRYABLE on reads (`DriverCore` refills + reaps)
//!   - `Injected::Eintr`     -> raw = -EINTR  (< 0)   RETRYABLE on every op
//!
//! A retryable CQE keeps the op LIVE (one token, no completion emitted) until a
//! terminal outcome or the init-time retry bound is hit. These rows pin the
//! scope.md:596 rule ("`EINTR` resubmitted internally on every op"; `EAGAIN`
//! resubmitted on reads) on the RING path — which today's `reap_ring` does not
//! honor (it maps `-errno` straight to `IoError`), the RED these tests exist for.
//!
//! ## Observation seam (survives the driver drop)
//!
//! `MockRingDriver::observe(&self) -> Arc<MockRingObservation>` exposing
//! `ops_in_flight()`, `reaped()`, and `retired()` (count of `Executor::retire_file`
//! calls). It pins exactly-one-retire on the final CQE of a closed fd (INV-11) and
//! drop-with-in-flight QUIESCE (INV-8): `MockRingDriver::drop` drains to zero
//! in-flight before the slab drops, so the `Arc` (which outlives the driver)
//! reports `ops_in_flight() == 0`.
//!
//! Newtype consideration (deferred from batch 5): the `pub(crate)` `RingExecutor`
//! trait threads `user_data: u64` and `fd_slot: u32` as bare integers. If the
//! implementer newtypes them while reshaping the mock, the public `MockRingDriver`
//! surface is unchanged — these tests bind only the observable.
//!
//! ## Deliberate coverage boundaries
//!   - Misaligned-`Direct` EINVAL stays a programmer-error PANIC (standing
//!     decision), pinned against a REAL `O_DIRECT` handle in `tests/uring.rs`
//!     (linux) and `tests/eager.rs` (macOS); the mock has no `Direct`-handle
//!     constructor, so it is not re-pinned here.
//!   - Ring `poll_wait` timeout / parked-submit (INV-3) against the REAL ring is
//!     `tests/uring_wait.rs`; deferred-close observation is `tests/ring_close.rs`.

#![cfg(feature = "mock")]

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use dios::mock::{Injected, MockRingDriver};
use dios::{Completion, CompletionBatch, FileHandle, OpToken, OpenHow, ReadFrameIdx, SubmitError};

const FRAME_BYTES: u32 = 4096;
const RETRY_BOUND: u32 = 3;
const EIO: i32 = 5;
const EMFILE: i32 = 24;
#[cfg(target_os = "linux")]
const EAGAIN: i32 = 11;
#[cfg(not(target_os = "linux"))]
const EAGAIN: i32 = 35;

const POLLS_MAX: u32 = 100_000;
const DEADLINE: Duration = Duration::from_secs(5);
const REOPEN_BACKOFF: Duration = Duration::from_micros(50);

fn ring(seed: u64, queue_capacity: u32, frames: u32) -> MockRingDriver {
    MockRingDriver::builder()
        .seed(seed)
        .queue_capacity(queue_capacity)
        .frames(frames)
        .frame_bytes(FRAME_BYTES)
        .retry_bound(RETRY_BOUND)
        .build()
}

fn open(m: &MockRingDriver) -> FileHandle {
    m.open(Path::new("seg-000000"), OpenHow::read_write())
        .expect("mock open never touches disk")
}

fn result_of(completion: &Completion) -> Result<u32, i32> {
    match completion.result() {
        Ok(bytes) => Ok(bytes),
        Err(err) => Err(err.raw_os_error().unwrap_or(-1)),
    }
}

/// Reaps exactly one completion, bounded by a wall-clock deadline and an
/// iteration cap.
fn reap_one(m: &MockRingDriver) -> (OpToken, Result<u32, i32>) {
    let mut out = CompletionBatch::with_capacity(1);
    let deadline = Instant::now() + DEADLINE;
    for _ in 0..POLLS_MAX {
        if m.poll(&mut out) > 0 {
            let completion = out.iter().next().expect("one reaped completion");
            return (completion.token(), result_of(completion));
        }
        assert!(
            Instant::now() < deadline,
            "no CQE reaped within the deadline"
        );
    }
    panic!("no CQE reaped within the poll iteration cap");
}

/// Reaps `expected` completions across polls into a `batch_capacity`-bounded
/// batch (a small capacity forces partial reaps), doubly bounded.
fn reap_results(
    m: &MockRingDriver,
    expected: usize,
    batch_capacity: usize,
) -> Vec<(OpToken, Result<u32, i32>)> {
    let mut out = CompletionBatch::with_capacity(batch_capacity.max(1));
    let mut drained = Vec::with_capacity(expected);
    let deadline = Instant::now() + DEADLINE;
    let mut polls = 0u32;
    while drained.len() < expected {
        m.poll(&mut out);
        for completion in &out {
            drained.push((completion.token(), result_of(completion)));
        }
        polls += 1;
        assert!(polls < POLLS_MAX, "partial reaps never converged (cap)");
        assert!(
            Instant::now() < deadline,
            "partial reaps never converged (deadline)"
        );
    }
    drained
}

/// Reaps until `wanted` appears, returning EVERY completion drained (so a
/// spurious early completion for `wanted` — e.g. a retryable CQE leaked as a
/// completion — is visible). Doubly bounded.
fn reap_until(m: &MockRingDriver, wanted: OpToken) -> Vec<(OpToken, Result<u32, i32>)> {
    let mut out = CompletionBatch::with_capacity(4);
    let mut seen = Vec::new();
    let deadline = Instant::now() + DEADLINE;
    let mut polls = 0u32;
    loop {
        m.poll(&mut out);
        for completion in &out {
            seen.push((completion.token(), result_of(completion)));
        }
        if seen.iter().any(|(token, _)| *token == wanted) {
            return seen;
        }
        polls += 1;
        assert!(polls < POLLS_MAX, "op never resolved (cap)");
        assert!(Instant::now() < deadline, "op never resolved (deadline)");
    }
}

fn results_for(seen: &[(OpToken, Result<u32, i32>)], token: OpToken) -> Vec<Result<u32, i32>> {
    seen.iter()
        .filter(|(reaped, _)| *reaped == token)
        .map(|(_, result)| *result)
        .collect()
}

#[test]
fn an_error_cqe_routes_to_a_typed_io_error_on_its_echoed_token() {
    let m = ring(1, 4, 4);
    let fd = open(&m);
    m.inject_for_next_submit(&[Injected::Io(EIO)]);

    let token = m
        .submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("submit within capacity");

    let (reaped_token, result) = reap_one(&m);
    assert_eq!(
        reaped_token, token,
        "the error CQE reaps on the op's own echoed user_data, not a positional slot"
    );
    assert_eq!(
        result,
        Err(EIO),
        "a negative raw CQE routes through the real reap sink as a typed IoError"
    );
}

#[test]
fn a_short_cqe_surfaces_its_partial_byte_count_without_reslicing() {
    let m = ring(1, 4, 4);
    let fd = open(&m);
    let short = 1500u32;
    m.inject_for_next_submit(&[Injected::Short(short)]);

    m.submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("submit within capacity");

    let (_token, result) = reap_one(&m);
    assert_eq!(
        result,
        Ok(short),
        "the ring reap surfaces the true partial count; reslicing a short read is the \
         pool's job (T008), never the driver's"
    );
}

#[test]
fn a_seeded_reorder_keeps_each_faults_result_bound_to_its_own_op_token() {
    const CLEAN: u32 = 5;
    let m = ring(0x00D5_7EED, 16, CLEAN + 1);
    let fd = open(&m);

    m.inject_for_next_submit(&[Injected::Io(EIO)]);
    let faulted = m
        .submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("the faulted op submits");
    let mut order_of: HashMap<OpToken, usize> = HashMap::new();
    order_of.insert(faulted, 0);
    for i in 1..=CLEAN {
        let token = m
            .submit_read(
                &fd,
                ReadFrameIdx::new(i),
                u64::from(i) * u64::from(FRAME_BYTES),
            )
            .expect("clean ops submit");
        order_of.insert(token, i as usize);
    }

    let reaped = reap_results(&m, (CLEAN + 1) as usize, (CLEAN + 1) as usize);
    let drain_order: Vec<usize> = reaped.iter().map(|(token, _)| order_of[token]).collect();
    let identity: Vec<usize> = (0..=CLEAN as usize).collect();
    assert_ne!(
        drain_order, identity,
        "the seed reorders CQE delivery, so binding-by-token is load-bearing here"
    );
    assert_ne!(
        drain_order[0], 0,
        "this seed reaps the faulted op (submission index 0) off the FIRST drain slot, so a \
         positional-routing impl — one that pins the injected error to whichever op reaps \
         first — would misroute it here and fail, regardless of the permutation"
    );

    let results: HashMap<OpToken, Result<u32, i32>> = reaped.into_iter().collect();
    assert_eq!(
        results[&faulted],
        Err(EIO),
        "the injected error lands on the ORIGINALLY-faulted op's token, not on whichever \
         op happened to reap in its CQE position"
    );
    for (token, index) in &order_of {
        if *index != 0 {
            assert_eq!(
                results[token],
                Ok(FRAME_BYTES),
                "every non-faulted op reaps its own clean full-frame result"
            );
        }
    }
}

#[test]
fn seeded_ring_completions_reap_out_of_submission_order_by_echoed_user_data() {
    const N: u32 = 8;
    let m = ring(0x00D5_7EED, 16, N);
    let fd = open(&m);

    let mut order_of: HashMap<OpToken, usize> = HashMap::new();
    for i in 0..N {
        let token = m
            .submit_read(
                &fd,
                ReadFrameIdx::new(i),
                u64::from(i) * u64::from(FRAME_BYTES),
            )
            .expect("submit within capacity");
        assert!(
            order_of.insert(token, i as usize).is_none(),
            "each submit mints a distinct token"
        );
    }

    let reaped = reap_results(&m, N as usize, N as usize);
    let drain_order: Vec<usize> = reaped.iter().map(|(token, _)| order_of[token]).collect();
    let identity: Vec<usize> = (0..N as usize).collect();
    assert_ne!(drain_order, identity, "the seed reorders CQE delivery");

    let mut sorted = drain_order.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted, identity,
        "reordering is a permutation routed by user_data — every op reaped exactly once"
    );
}

/// The seeded reap-order permutation for a given seed, ops mapped back to
/// submission index by their echoed tokens.
fn reap_schedule(seed: u64) -> Vec<usize> {
    const N: u32 = 8;
    let m = ring(seed, 16, N);
    let fd = open(&m);
    let mut order_of: HashMap<OpToken, usize> = HashMap::new();
    for i in 0..N {
        let token = m
            .submit_read(
                &fd,
                ReadFrameIdx::new(i),
                u64::from(i) * u64::from(FRAME_BYTES),
            )
            .expect("submit within capacity");
        order_of.insert(token, i as usize);
    }
    reap_results(&m, N as usize, N as usize)
        .into_iter()
        .map(|(token, _)| order_of[&token])
        .collect()
}

#[test]
fn the_same_seed_reproduces_the_same_reap_schedule() {
    assert_eq!(
        reap_schedule(0x00D5_7EED),
        reap_schedule(0x00D5_7EED),
        "a fixed seed makes the ring's adversarial CQE schedule deterministic"
    );
}

#[test]
fn different_seeds_produce_different_reap_schedules() {
    assert_ne!(
        reap_schedule(0x00D5_7EED),
        reap_schedule(0x00B1_6B00),
        "the ring reap schedule is seed-derived, not a fixed shuffle"
    );
}

#[test]
fn a_bounded_reap_leaves_the_rest_for_the_next_poll() {
    const N: u32 = 6;
    let m = ring(0x00B1_6B00, N + 2, N);
    let fd = open(&m);

    let mut submitted: HashSet<OpToken> = HashSet::new();
    for i in 0..N {
        let token = m
            .submit_read(
                &fd,
                ReadFrameIdx::new(i),
                u64::from(i) * u64::from(FRAME_BYTES),
            )
            .expect("submit within capacity");
        assert!(submitted.insert(token), "distinct token per submit");
    }

    let reaped = reap_results(&m, N as usize, 2);
    let tokens: HashSet<OpToken> = reaped.iter().map(|(token, _)| *token).collect();
    assert_eq!(
        tokens.len(),
        N as usize,
        "a reap bounded to 2 CQEs per poll still drains all N exactly once — a partial \
         reap defers the remainder, it never drops or double-counts a completion"
    );
    assert_eq!(
        tokens, submitted,
        "every submitted op reaps by echoed token"
    );
}

#[test]
fn an_eagain_read_cqe_is_resubmitted_on_the_ring_and_completes_with_the_success() {
    let m = ring(1, 4, 4);
    let fd = open(&m);
    m.inject_for_next_submit(&[Injected::Eagain]);

    let token = m
        .submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("submit within capacity");

    assert_eq!(
        results_for(&reap_until(&m, token), token),
        vec![Ok(FRAME_BYTES)],
        "an -EAGAIN CQE keeps the read live (one token, no early error) and the DriverCore \
         refills the SQE so the op completes with the SUCCESS result (scope.md:596)"
    );
}

#[test]
fn an_eintr_cqe_is_resubmitted_on_the_ring_and_completes_with_the_success() {
    let m = ring(1, 4, 4);
    let fd = open(&m);
    m.inject_for_next_submit(&[Injected::Eintr]);

    let token = m
        .submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("submit within capacity");

    assert_eq!(
        results_for(&reap_until(&m, token), token),
        vec![Ok(FRAME_BYTES)],
        "an -EINTR CQE is resubmitted internally on the ring and the op completes with the \
         success result — one token, no early error (scope.md:596)"
    );
}

#[test]
fn ring_retries_at_exactly_the_bound_still_complete_with_the_success() {
    let m = ring(1, 4, 4);
    let fd = open(&m);
    let at_bound: Vec<Injected> =
        std::iter::repeat_n(Injected::Eagain, RETRY_BOUND as usize).collect();
    m.inject_for_next_submit(&at_bound);

    let token = m
        .submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("submit within capacity");

    assert_eq!(
        results_for(&reap_until(&m, token), token),
        vec![Ok(FRAME_BYTES)],
        "exactly RETRY_BOUND retryable CQEs are ALL resubmitted and the op still completes with \
         success — the bound is the last retry that is honored, so an impl that stops one retry \
         short (off-by-one) would surface EAGAIN here and fail"
    );
}

#[test]
fn ring_retries_exhaust_the_init_time_bound_and_surface_the_typed_error() {
    let m = ring(1, 4, 4);
    let fd = open(&m);
    let over_bound: Vec<Injected> =
        std::iter::repeat_n(Injected::Eagain, (RETRY_BOUND + 1) as usize).collect();
    m.inject_for_next_submit(&over_bound);

    let token = m
        .submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("submit within capacity");

    assert_eq!(
        results_for(&reap_until(&m, token), token),
        vec![Err(EAGAIN)],
        "a fixed init-time retry bound stops resubmitting on the ring and surfaces the \
         typed EAGAIN once — the resubmit loop is bounded, never unbounded"
    );
}

#[test]
fn an_error_cqe_frees_its_slab_slot_for_reuse() {
    let m = ring(1, 1, 2);
    let fd = open(&m);
    m.inject_for_next_submit(&[Injected::Io(EIO)]);

    m.submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("the one slot fits");
    assert!(
        matches!(
            m.submit_read(&fd, ReadFrameIdx::new(1), u64::from(FRAME_BYTES)),
            Err(SubmitError::Full)
        ),
        "capacity 1: the second submit is refused while the first op is in flight"
    );

    let (_token, result) = reap_one(&m);
    assert_eq!(
        result,
        Err(EIO),
        "the first op reaps its injected error CQE"
    );

    m.submit_read(&fd, ReadFrameIdx::new(1), u64::from(FRAME_BYTES))
        .expect("the slot an errored op held is reclaimed at reap, so capacity recovers");
}

#[test]
fn fd_table_exhaustion_surfaces_emfile_and_recovers_after_a_close() {
    let m = ring(1, 4, 4);
    let mut open_handles: Vec<FileHandle> = Vec::new();
    let mut exhausted: Option<i32> = None;
    for _ in 0..POLLS_MAX {
        match m.open(Path::new("seg"), OpenHow::read_write()) {
            Ok(fd) => open_handles.push(fd),
            Err(err) => {
                exhausted = Some(err.raw_os_error().unwrap_or(-1));
                break;
            }
        }
    }
    assert_eq!(
        exhausted,
        Some(EMFILE),
        "the fixed fd table backpressures with EMFILE, never grows past its init bound"
    );
    assert!(
        !open_handles.is_empty(),
        "the table admits at least one fd before exhausting"
    );

    let freed = open_handles.pop().expect("at least one open handle");
    let freed_id = freed.file_id();
    m.close(freed);
    let mut idle = CompletionBatch::with_capacity(1);
    let deadline = Instant::now() + DEADLINE;
    let mut polls = 0u32;
    while !m.is_closed(freed_id) {
        m.poll(&mut idle);
        polls += 1;
        assert!(polls < POLLS_MAX, "the idle close(2) never drained (cap)");
        assert!(
            Instant::now() < deadline,
            "the idle close(2) never drained (deadline)"
        );
    }

    m.open(Path::new("seg"), OpenHow::read_write())
        .expect("closing an fd returns its table slot, so a fresh open succeeds again");
}

#[test]
fn close_retires_exactly_once_only_after_the_final_in_flight_cqe_reaps() {
    const N: u32 = 4;
    let m = ring(0x00D5_7EED, N + 2, N);
    let obs = m.observe();
    let fd = open(&m);
    let id = fd.file_id();

    let mut pending: HashSet<OpToken> = HashSet::new();
    for i in 0..N {
        let token = m
            .submit_read(
                &fd,
                ReadFrameIdx::new(i),
                u64::from(i) * u64::from(FRAME_BYTES),
            )
            .expect("submit within capacity");
        pending.insert(token);
    }

    m.close(fd);
    assert_eq!(
        obs.retired(),
        0,
        "with N ops in flight the deferred retire has not fired (INV-11)"
    );

    let mut out = CompletionBatch::with_capacity(1);
    let deadline = Instant::now() + DEADLINE;
    let mut polls = 0u32;
    let mut reaped = 0u32;
    while reaped < N {
        m.poll(&mut out);
        for completion in &out {
            assert!(
                pending.remove(&completion.token()),
                "each reap retires one distinct in-flight op on the closing fd"
            );
            reaped += 1;
        }
        if reaped < N {
            assert_eq!(
                obs.retired(),
                0,
                "retire is gated on the LAST drain — not fired while any op is in flight"
            );
        }
        polls += 1;
        assert!(
            polls < POLLS_MAX,
            "the closing fd never fully drained (cap)"
        );
        assert!(
            Instant::now() < deadline,
            "the closing fd never fully drained (deadline)"
        );
    }
    assert_eq!(
        obs.retired(),
        1,
        "the fd is retired EXACTLY once, and only after the final in-flight CQE reaps"
    );
    assert!(
        m.is_closed(id),
        "close(2) is observable after the final drain"
    );
}

#[test]
fn a_ghost_handle_stays_stale_through_a_concurrent_reopen_race() {
    let m = Arc::new(ring(1, 4, 4));
    let obs = m.observe();
    let anchor = open(&m);
    let ghost = m.duplicate_handle(&anchor);
    let ghost_id = anchor.file_id();

    let mut others: Vec<FileHandle> = Vec::new();
    for _ in 0..POLLS_MAX {
        match m.open(Path::new("seg"), OpenHow::read_write()) {
            Ok(handle) => others.push(handle),
            Err(_) => break,
        }
    }
    assert!(!others.is_empty(), "the table fills before the race");

    m.submit_read(&anchor, ReadFrameIdx::new(0), 0)
        .expect("one in-flight op holds the anchor's deferred close open");
    m.close(anchor);

    let raced = Arc::new(AtomicBool::new(false));
    let reopener = {
        let m = Arc::clone(&m);
        let raced = Arc::clone(&raced);
        thread::spawn(move || {
            let deadline = Instant::now() + DEADLINE;
            loop {
                match m.open(Path::new("seg"), OpenHow::read_write()) {
                    Ok(handle) => {
                        if handle.file_id().aliases_slot(&ghost_id) {
                            return handle.file_id();
                        }
                    }
                    Err(_) => raced.store(true, Ordering::Release),
                }
                assert!(
                    Instant::now() < deadline,
                    "the reopener never captured the released slot"
                );
                thread::sleep(REOPEN_BACKOFF);
            }
        })
    };

    let handshake = Instant::now() + DEADLINE;
    while !raced.load(Ordering::Acquire) {
        assert!(
            Instant::now() < handshake,
            "the reopener never observed an occupied-table EMFILE to race against"
        );
        thread::sleep(REOPEN_BACKOFF);
    }

    let mut out = CompletionBatch::with_capacity(1);
    let deadline = Instant::now() + DEADLINE;
    let mut polls = 0u32;
    while obs.retired() == 0 {
        assert!(
            matches!(
                m.submit_read(&ghost, ReadFrameIdx::new(1), u64::from(FRAME_BYTES)),
                Err(SubmitError::StaleHandle)
            ),
            "the ghost generation is rejected throughout the close, never issued an op"
        );
        m.poll(&mut out);
        polls += 1;
        assert!(polls < POLLS_MAX, "the anchor never retired (cap)");
        assert!(
            Instant::now() < deadline,
            "the anchor never retired (deadline)"
        );
    }

    let live_id = reopener.join().expect("the reopener joins");
    assert!(
        live_id.aliases_slot(&ghost_id),
        "the reopened handle took the released slot"
    );
    assert!(
        matches!(
            m.submit_read(&ghost, ReadFrameIdx::new(1), u64::from(FRAME_BYTES)),
            Err(SubmitError::StaleHandle)
        ),
        "even after a concurrent reopen recycled its slot, the ghost's stale generation is \
         rejected — generational identity, not closed-set membership"
    );
}

#[test]
fn dropping_the_driver_with_ops_in_flight_quiesces_to_zero() {
    const N: u32 = 3;
    let m = ring(1, N + 1, N);
    let fd = open(&m);

    for i in 0..N {
        m.submit_read(
            &fd,
            ReadFrameIdx::new(i),
            u64::from(i) * u64::from(FRAME_BYTES),
        )
        .expect("submit within capacity");
    }

    let obs = m.observe();
    assert_eq!(
        obs.ops_in_flight(),
        N,
        "all N submitted ops are in flight before any poll"
    );

    let (done_tx, done_rx) = mpsc::channel();
    let dropper = thread::spawn(move || {
        drop(m);
        let _ = done_tx.send(());
    });
    done_rx.recv_timeout(DEADLINE).expect(
        "dropping the driver with ops in flight must QUIESCE within the deadline — a timeout \
         means the drop hung draining in-flight ops (INV-8 deadlock), not a slow machine",
    );
    dropper.join().expect("the dropper thread joins");

    assert_eq!(
        obs.ops_in_flight(),
        0,
        "the drop drained to zero in-flight before the slab and registered buffers tore down \
         (INV-8), never abandoning kernel-visible ops"
    );
    assert_eq!(
        obs.reaped(),
        N,
        "every in-flight op was reaped during the quiesce, not discarded"
    );
}
