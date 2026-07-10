//! T005 batch-5 follow-up: real-ring pins for the `Driver::poll_wait` surface T004
//! forwarded — the `EXT_ARG` idle wait, INV-3 submit-while-parked. `tests/uring.rs`
//! deferred these because they reached the public `Driver` only once forwarding
//! landed; that has happened, so the ring-level pins follow here.
//!
//! The deferred-close-through-the-ring pin lives in `tests/ring_close.rs` — it is
//! compile-RED on a `Driver::close`/`is_closed` wiring gap, so it is isolated
//! there rather than blocking this file's runnable `poll_wait` pins.
//!
//! Linux-only — the ring exists only there; the file is empty on the darwin dev
//! machine. Every wait loop is bounded (wall-clock deadline + idle backoff), the
//! pattern from `tests/uring.rs`, so a driver that never completes fails loudly
//! instead of hanging.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use dios::{CompletionBatch, Driver, OpenHow, ReadFrameIdx};

const FRAME_BYTES: u32 = 4096;
const DRAIN_DEADLINE: Duration = Duration::from_secs(5);

static UNIQUE: AtomicU32 = AtomicU32::new(0);

fn temp_path(tag: &str) -> PathBuf {
    let n = UNIQUE.fetch_add(1, Ordering::Relaxed);
    let mut path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&path).expect("target tmp dir");
    path.push(format!("uringwait-{tag}-{}-{n}", std::process::id()));
    path
}

fn driver(frames: u32, frame_bytes: u32, queue_capacity: u32) -> Driver {
    Driver::builder()
        .queue_capacity(queue_capacity)
        .frames(frames)
        .frame_bytes(frame_bytes)
        .retry_bound(3)
        .build()
}

fn seed_frame(tag: &str) -> PathBuf {
    let path = temp_path(tag);
    std::fs::write(&path, vec![0x2Cu8; FRAME_BYTES as usize]).expect("seed a full frame");
    path
}

#[test]
fn poll_wait_returns_zero_without_events_after_its_timeout_expires() {
    let drv = driver(1, FRAME_BYTES, 4);
    let mut out = CompletionBatch::with_capacity(4);

    let timeout = Duration::from_millis(120);
    let start = Instant::now();
    let drained = drv.poll_wait(&mut out, timeout);
    let elapsed = start.elapsed();

    assert_eq!(drained, 0, "an idle ring reaps nothing from poll_wait");
    assert!(
        elapsed >= Duration::from_millis(80),
        "poll_wait parks in the kernel for its EXT_ARG timeout instead of spinning back: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "poll_wait returns promptly once the timeout expires: {elapsed:?}"
    );
}

#[test]
fn poll_wait_reaps_a_ready_completion_without_burning_the_full_timeout() {
    let path = seed_frame("ready");
    let drv = driver(1, FRAME_BYTES, 4);
    let fd = drv
        .open(&path, OpenHow::read_write())
        .expect("open the seeded file");

    drv.submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("submit within capacity");

    let mut out = CompletionBatch::with_capacity(4);
    let start = Instant::now();
    let drained = drv.poll_wait(&mut out, Duration::from_secs(5));
    let elapsed = start.elapsed();

    assert!(drained >= 1, "poll_wait reaps the ready read");
    assert!(
        elapsed < Duration::from_secs(4),
        "a ready CQE wakes the EXT_ARG wait well before the timeout: {elapsed:?}"
    );
    let completion = out.iter().next().expect("one completion");
    assert_eq!(
        completion.result().map_err(dios::IoError::raw_os_error),
        Ok(FRAME_BYTES),
        "the reaped completion carries the full-frame transfer"
    );
}

const PARKED_WAIT: Duration = Duration::from_secs(3);
const SUBMIT_BOUND: Duration = Duration::from_millis(500);

const SPIN_MAX: u32 = 100_000_000;

#[test]
fn a_submit_from_another_thread_completes_while_a_poller_is_parked_in_poll_wait() {
    let path = seed_frame("parked");
    let drv = Arc::new(driver(2, FRAME_BYTES, 4));
    let entered = Arc::new(AtomicBool::new(false));
    let reaped = Arc::new(AtomicU32::new(0));

    let fd = drv
        .open(&path, OpenHow::read_write())
        .expect("open the seeded file");

    let parked = {
        let drv = Arc::clone(&drv);
        let entered = Arc::clone(&entered);
        let reaped = Arc::clone(&reaped);
        thread::spawn(move || {
            let mut out = CompletionBatch::with_capacity(4);
            let deadline = Instant::now() + DRAIN_DEADLINE;
            entered.store(true, Ordering::Release);
            while reaped.load(Ordering::Acquire) == 0 {
                let drained = drv.poll_wait(&mut out, PARKED_WAIT);
                if drained > 0 {
                    reaped.fetch_add(u32::try_from(drained).unwrap_or(0), Ordering::AcqRel);
                }
                assert!(
                    Instant::now() < deadline,
                    "the parked poller reaps the cross-thread submit within the deadline"
                );
            }
        })
    };

    let deadline = Instant::now() + DRAIN_DEADLINE;
    let mut spins = 0u32;
    while !entered.load(Ordering::Acquire) {
        std::hint::spin_loop();
        spins += 1;
        assert!(spins < SPIN_MAX, "the poller never entered poll_wait (cap)");
        assert!(
            Instant::now() < deadline,
            "the poller never entered poll_wait (deadline)"
        );
    }
    thread::sleep(Duration::from_millis(200));

    let start = Instant::now();
    let submitted = drv.submit_read(&fd, ReadFrameIdx::new(0), 0);
    let elapsed = start.elapsed();

    assert!(submitted.is_ok(), "the submit is admitted");
    // One-sided: losing the pre-park race passes vacuously, never false-fails.
    assert!(
        elapsed < SUBMIT_BOUND,
        "submit's SQE-fill critical section runs OUTSIDE the poll_wait kernel wait, so it \
         never blocks on the parked poller's {PARKED_WAIT:?} wait (AD-4/INV-3): {elapsed:?}"
    );

    parked
        .join()
        .expect("the parked poller joins after reaping");
    assert_eq!(
        reaped.load(Ordering::Acquire),
        1,
        "the op submitted from another thread was reaped by the parked poll_wait"
    );
}
