//! Progress reporting for an embedding owner: backend completion and frame
//! reclamation are different facts, and waiting must not pin the pool lock.
//!
//! `MockWaitObservation` is a read-only observation of the mock backend's real
//! blocking wait hook. `wait_until_parked` returns true only while that hook is
//! currently blocked; observing it never releases or shortens the block.
//! `parks_entered`, `parks_in_progress`, and `parks_exited` are exact counters,
//! so a spin loop or short-timeout return cannot impersonate a parked wait.
//! `wake_exits` and `timeout_exits` partition completed parks by their actual
//! exit reason: I/O completion and `PoolWakeHandle` use the shared wake signal,
//! while deadline expiry is counted only as a timeout.

#![cfg(feature = "mock")]

use std::path::Path;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use dios::testing::{
    FrameState, MockDriver, MockPoolTestingExt, MockRingDriver, MockRingPoolBuilderTestingExt,
    MockRingPoolTestingExt, MockWaitObservation, PoolBuilderTestingExt, PoolTestingExt,
};
use dios::{
    DirectIo, Get, PageId, PollReport, Pool, PoolCompletion, PoolCompletionBatch, PoolToken,
    PoolWakeHandle, ReadyResult, SyncMode,
};

const GRANULE: u32 = 4096;
const POLL_BOUND: u32 = 128;
const WAIT_DEADLINE: Duration = Duration::from_secs(30);
const PROMPT_WAKE: Duration = Duration::from_secs(5);
const IDLE_WAIT: Duration = Duration::from_millis(120);
const LONG_IDLE_WAIT: Duration = Duration::from_millis(900);

fn pool_with_file(name: &str) -> (Pool<MockDriver>, dios::FileId) {
    pool_with_file_product_capacity(name, 0)
}

fn pool_with_file_product_capacity(
    name: &str,
    max_inflight_product_ops: u32,
) -> (Pool<MockDriver>, dios::FileId) {
    let queue_capacity = 1u32
        .checked_add(max_inflight_product_ops)
        .expect("small fixture capacities add without overflow");
    let mock = MockDriver::builder()
        .queue_capacity(queue_capacity)
        .frames(4)
        .frame_bytes(GRANULE)
        .build();
    let file = mock
        .open(Path::new(name), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    mock.seed_page(&file, 0, 0xC3);
    let pool = Pool::builder()
        .frame_count(4)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .write_slots(0)
        .max_inflight_product_ops(max_inflight_product_ops)
        .build_on(mock)
        .expect("valid progress pool");
    pool.register_file(file);
    (pool, file_id)
}

#[test]
fn read_credit_saturation_preserves_product_reservations_and_singleflight_joins() {
    let mock = MockDriver::builder()
        .seed(0x00C4_ED17)
        .queue_capacity(3)
        .frames(5)
        .frame_bytes(GRANULE)
        .write_slots(1)
        .build();
    let file = mock
        .open(
            Path::new("pool-progress-read-product-partition"),
            DirectIo::Disabled,
        )
        .expect("mock open");
    let file_id = file.file_id();
    mock.seed_page(&file, 0, 0x41);
    mock.seed_page(&file, 1, 0x42);
    let pool = Pool::builder()
        .frame_count(5)
        .granule(GRANULE)
        .max_concurrent_readers(2)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .write_slots(1)
        .max_inflight_product_ops(2)
        .build_on(mock)
        .expect("valid partitioned-capacity pool");
    pool.register_file(file);
    let first_reader = pool.register_reader().expect("first reader slot");
    let second_reader = pool.register_reader().expect("second reader slot");
    let page = PageId::new(file_id, 0);
    let Get::Pending(first_waiter) = pool.get(&first_reader, page).expect("live file") else {
        panic!("the first cold read must admit");
    };
    let Get::Pending(joined_waiter) = pool.get(&second_reader, page).expect("live file") else {
        panic!("a same-page join must not require another read credit");
    };
    assert!(matches!(
        pool.get(&first_reader, PageId::new(file_id, 1)),
        Ok(Get::Busy)
    ));

    let mut slot = pool.write_arena().alloc().expect("one write slot");
    slot.fill(0x5A);
    let write = pool
        .submit_write(file_id, slot, 0)
        .expect("the product reservation remains available beside a saturated read");
    let fsync = pool
        .submit_fsync(file_id, SyncMode::Full)
        .expect("the full product bound admits a held barrier");

    let mut completions = PoolCompletionBatch::with_capacity(2);
    let mut saw_write = false;
    let mut saw_fsync = false;
    for _ in 0..POLL_BOUND {
        let _ = pool.poll_report(&mut completions);
        for completion in completions.iter() {
            match completion {
                PoolCompletion::Write {
                    token,
                    result: Ok(bytes),
                } if *token == write => {
                    assert_eq!(*bytes, GRANULE);
                    saw_write = true;
                }
                PoolCompletion::Fsync {
                    token,
                    result: Ok(()),
                } if *token == fsync => saw_fsync = true,
                completion => panic!("unexpected product completion: {completion:?}"),
            }
        }
        if saw_write && saw_fsync {
            break;
        }
    }
    assert!(saw_write, "the admitted write drains");
    assert!(saw_fsync, "the held fsync cannot starve behind the read");
    assert!(matches!(
        pool.ready(&first_reader, first_waiter),
        ReadyResult::Ready(_)
    ));
    assert!(matches!(
        pool.ready(&second_reader, joined_waiter),
        ReadyResult::Ready(_)
    ));
    assert!(matches!(
        pool.get(&first_reader, PageId::new(file_id, 1)),
        Ok(Get::Pending(_))
    ));
}

#[test]
fn poll_report_counts_retry_cqes_before_the_terminal_product_result() {
    let ring = MockRingDriver::builder()
        .queue_capacity(2)
        .frames(4)
        .frame_bytes(GRANULE)
        .write_slots(1)
        .retry_bound(1)
        .build();
    let file = ring
        .open(Path::new("pool-progress-ring-retry"), DirectIo::Disabled)
        .expect("mock ring open");
    let file_id = file.file_id();
    ring.inject_for_next_submit(&[dios::testing::Injected::Eintr]);
    let pool = Pool::builder()
        .frame_count(4)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .write_slots(1)
        .max_inflight_product_ops(1)
        .build_on_ring(ring)
        .expect("valid mock-ring pool");
    pool.register_file(file);
    let token = pool
        .submit_fsync(file_id, SyncMode::Full)
        .expect("one barrier admits");
    let mut completions = PoolCompletionBatch::with_capacity(1);

    let retry = pool.poll_report(&mut completions);
    assert_eq!(retry.backend_completions(), 1, "the EINTR CQE was drained");
    assert_eq!(completions.iter().count(), 0, "retry is not terminal");

    let terminal = pool.poll_report(&mut completions);
    assert_eq!(
        terminal.backend_completions(),
        1,
        "the clean CQE was drained"
    );
    assert!(matches!(
        completions.iter().next(),
        Some(PoolCompletion::Fsync {
            token: completed,
            result: Ok(())
        }) if *completed == token
    ));
    assert_eq!(pool.ring_driver().observe().reaped(), 1);
}

#[cfg(target_os = "linux")]
#[test]
fn retry_cqe_early_return_is_progress_not_a_platform_wait_timeout() {
    let ring = MockRingDriver::builder()
        .queue_capacity(1)
        .frames(4)
        .frame_bytes(GRANULE)
        .write_slots(1)
        .retry_bound(1)
        .build();
    let file = ring
        .open(
            Path::new("pool-progress-ring-retry-wait-exit"),
            DirectIo::Disabled,
        )
        .expect("mock ring open");
    let submit_file = ring.duplicate_handle(&file);
    ring.inject_for_next_submit(&[dios::testing::Injected::Eintr]);
    let pool = Pool::builder()
        .frame_count(4)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .write_slots(1)
        .max_inflight_product_ops(1)
        .build_on_ring(ring)
        .expect("valid mock-ring pool");
    let wait_observation = pool.observe_ring_waits();
    pool.register_file(file);
    let token = pool
        .ring_driver()
        .submit_fsync(&submit_file, SyncMode::Full)
        .expect("one barrier admits");

    let started = Instant::now();
    let retry = pool.poll_wait_raw_progress(Duration::from_secs(1));
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "the injected retry CQE returns promptly rather than consuming the deadline"
    );
    assert_eq!(retry, 1, "the EINTR raw CQE counts as backend progress");
    assert_eq!(wait_observation.parks_entered(), 1);
    assert_eq!(wait_observation.parks_exited(), 1);
    assert_eq!(wait_observation.parks_in_progress(), 0);
    assert_eq!(wait_observation.wake_exits(), 1);
    assert_eq!(
        wait_observation.timeout_exits(),
        0,
        "a drained retry CQE is backend progress, never a deadline expiry"
    );

    let mut terminal = dios::driver::CompletionBatch::with_capacity(1);
    assert_eq!(pool.ring_driver().poll(&mut terminal), 1);
    assert_eq!(
        terminal.iter().next().expect("terminal fsync").token(),
        token
    );

    let remembered_wake = pool.poll_wait_raw_progress(Duration::from_millis(20));
    assert_eq!(remembered_wake, 0);
    assert_eq!(
        wait_observation.parks_entered(),
        1,
        "the terminal completion's remembered wake is consumed before parking"
    );

    let idle = pool.poll_wait_raw_progress(Duration::from_millis(20));
    assert_eq!(idle, 0);
    assert_eq!(wait_observation.parks_entered(), 2);
    assert_eq!(wait_observation.parks_exited(), 2);
    assert_eq!(wait_observation.parks_in_progress(), 0);
    assert_eq!(wait_observation.wake_exits(), 1);
    assert_eq!(
        wait_observation.timeout_exits(),
        1,
        "an idle wait that reaches its actual deadline remains a timeout"
    );
}

#[test]
fn poll_report_counts_retryable_eagain_read_cqe_without_releasing_its_credit() {
    let ring = MockRingDriver::builder()
        .queue_capacity(1)
        .frames(4)
        .frame_bytes(GRANULE)
        .write_slots(1)
        .retry_bound(1)
        .build();
    let file = ring
        .open(Path::new("pool-progress-ring-eagain"), DirectIo::Disabled)
        .expect("mock ring open");
    let file_id = file.file_id();
    ring.inject_for_next_submit(&[dios::testing::Injected::Eagain]);
    let pool = Pool::builder()
        .frame_count(4)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .build_on_ring(ring)
        .expect("valid mock-ring read pool");
    pool.register_file(file);
    let reader = pool.register_reader().expect("one reader");
    let Get::Pending(token) = pool
        .get(&reader, PageId::new(file_id, 0))
        .expect("live file")
    else {
        panic!("the cold read admits");
    };
    let mut completions = PoolCompletionBatch::with_capacity(0);

    let retry = pool.poll_report(&mut completions);
    assert_eq!(retry.backend_completions(), 1, "the EAGAIN CQE counts");
    let token = match pool.ready(&reader, token) {
        ReadyResult::NotYet(token) => token,
        ReadyResult::Ready(_) => panic!("a retry CQE cannot finish the logical read"),
        ReadyResult::Err(error) => panic!("retryable EAGAIN surfaced early: {error}"),
    };
    assert!(matches!(
        pool.get(&reader, PageId::new(file_id, 1)),
        Ok(Get::Busy)
    ));

    let terminal = pool.poll_report(&mut completions);
    assert_eq!(terminal.backend_completions(), 1, "the clean CQE counts");
    assert!(matches!(pool.ready(&reader, token), ReadyResult::Ready(_)));
}

#[test]
fn poll_report_separates_backend_completions_from_reclaimed_frames() {
    let (pool, file) = pool_with_file("pool-progress-report");
    let reader = pool.register_reader().expect("one reader slot");
    let Get::Pending(token) = pool
        .get(&reader, PageId::new(file, 0))
        .expect("the registered file is live")
    else {
        panic!("the registered cold page must admit a miss");
    };
    let mut completions = PoolCompletionBatch::with_capacity(4);

    let report: PollReport = pool.poll_report(&mut completions);

    let backend_completions: u32 = report.backend_completions();
    let reclaimed_frames: u32 = report.reclaimed_frames();
    assert_eq!(backend_completions, 1, "one read CQE drained internally");
    assert_eq!(
        reclaimed_frames, 0,
        "making a read resident is not frame reclamation"
    );
    assert_eq!(
        completions.iter().count(),
        0,
        "internal read tokens and CQEs never escape through the product batch"
    );
    match pool.ready(&reader, token) {
        ReadyResult::Ready(frame) => assert!(frame.iter().all(|&byte| byte == 0xC3)),
        ReadyResult::NotYet(_) => panic!("the reported read completion must be ready"),
        ReadyResult::Err(error) => panic!("the seeded read cannot fail: {error}"),
    }
}

#[test]
fn completion_driven_poll_wait_does_not_hold_the_pool_control_lock() {
    let (pool, file) = pool_with_file("pool-progress-wait");
    let wait_observation: MockWaitObservation = pool.driver().observe_waits();
    let cleanup_wake = pool.wake_handle();
    let pool = Arc::new(pool);
    let (report_tx, report_rx) = mpsc::sync_channel(1);

    let waiter = {
        let pool = Arc::clone(&pool);
        thread::spawn(move || {
            let mut completions = PoolCompletionBatch::with_capacity(4);
            let started = Instant::now();
            let report = pool.poll_wait(&mut completions, WAIT_DEADLINE);
            report_tx
                .send((
                    report.backend_completions(),
                    report.reclaimed_frames(),
                    completions.iter().count(),
                    started.elapsed(),
                ))
                .expect("the completion-wait report receiver remains live");
        })
    };

    if !wait_observation.wait_until_parked(Duration::from_secs(1)) {
        cleanup_wake.wake();
        waiter
            .join()
            .expect("waiter joins after observation timeout");
        panic!("poll_wait did not register an actual blocking wait");
    }
    assert_eq!(wait_observation.parks_entered(), 1);
    assert_eq!(wait_observation.parks_in_progress(), 1);
    assert_eq!(wait_observation.parks_exited(), 0);
    assert_eq!(wait_observation.wake_exits(), 0);
    assert_eq!(wait_observation.timeout_exits(), 0);

    let (admitted_tx, admitted_rx) = mpsc::sync_channel(1);
    let submitter = {
        let pool = Arc::clone(&pool);
        thread::spawn(move || {
            let reader = pool.register_reader().expect("one reader slot");
            let admitted = matches!(pool.get(&reader, PageId::new(file, 0)), Ok(Get::Pending(_)));
            admitted_tx.send(admitted).expect("receiver remains live");
        })
    };

    match admitted_rx.recv_timeout(Duration::from_secs(1)) {
        Ok(true) => submitter.join().expect("submitter joins"),
        outcome => {
            cleanup_wake.wake();
            submitter
                .join()
                .expect("submitter joins after cleanup wake");
            waiter.join().expect("waiter joins after cleanup wake");
            panic!(
                "future miss admission must proceed while poll_wait is actually parked: {outcome:?}"
            );
        }
    }

    let report = match report_rx.recv_timeout(PROMPT_WAKE) {
        Ok(report) => report,
        Err(error) => {
            cleanup_wake.wake();
            waiter.join().expect("fallback wake releases waiter");
            panic!("future IO did not wake the actually parked waiter: {error}");
        }
    };
    waiter.join().expect("waiter joins");
    assert_eq!(wait_observation.parks_entered(), 1);
    assert_eq!(wait_observation.parks_in_progress(), 0);
    assert_eq!(wait_observation.parks_exited(), 1);
    assert_eq!(wait_observation.wake_exits(), 1);
    assert_eq!(wait_observation.timeout_exits(), 0);
    let (backend_completions, reclaimed_frames, caller_completions, elapsed) = report;
    assert!(
        elapsed < PROMPT_WAKE,
        "a future submission wakes poll_wait far before its thirty-second deadline: {elapsed:?}"
    );
    assert_eq!(
        backend_completions, 1,
        "the wait returns on the submitted read"
    );
    assert_eq!(reclaimed_frames, 0, "the completed read was not a reclaim");
    assert_eq!(caller_completions, 0, "read CQEs remain pool-internal");

    let reader = pool
        .register_reader()
        .expect("the submitter released its reader");
    match pool
        .get(&reader, PageId::new(file, 0))
        .expect("the registered file is live")
    {
        Get::Hit(frame) => assert!(frame.iter().all(|&byte| byte == 0xC3)),
        Get::Pending(_) => panic!("the counted read CQE must have made its page resident"),
        Get::Busy => panic!("the completed page cannot be Busy"),
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the helper consumes the Arc transferred into the waiter thread"
)]
fn wake_only_wait(pool: Arc<Pool<MockDriver>>) -> (u32, u32, usize, Duration) {
    let mut completions = PoolCompletionBatch::with_capacity(1);
    let started = Instant::now();
    let report = pool.poll_wait(&mut completions, WAIT_DEADLINE);
    (
        report.backend_completions(),
        report.reclaimed_frames(),
        completions.iter().count(),
        started.elapsed(),
    )
}

fn assert_wake_only_report(report: (u32, u32, usize, Duration), schedule: &str) {
    let (backend, reclaimed, delivered, elapsed) = report;
    assert_eq!(backend, 0, "{schedule}: control wake is not a backend CQE");
    assert_eq!(reclaimed, 0, "{schedule}: control wake reclaims no frame");
    assert_eq!(delivered, 0, "{schedule}: control wake emits no completion");
    assert!(
        elapsed < PROMPT_WAKE,
        "{schedule}: control wake returns far before the thirty-second deadline: {elapsed:?}"
    );
}

#[test]
fn idle_product_poll_wait_tracks_distinct_deadlines_and_timeout_exits() {
    let (pool, _file) = pool_with_file("pool-progress-idle-timeout");
    let wait_observation: MockWaitObservation = pool.driver().observe_waits();
    let mut completions = PoolCompletionBatch::with_capacity(0);

    let started = Instant::now();
    let report = pool.poll_wait(&mut completions, IDLE_WAIT);
    let elapsed = started.elapsed();

    assert_eq!(report.backend_completions(), 0);
    assert_eq!(report.reclaimed_frames(), 0);
    assert_eq!(completions.iter().count(), 0);
    assert!(
        elapsed >= Duration::from_millis(80),
        "an idle product wait parks until approximately its deadline: {elapsed:?}"
    );
    assert!(
        elapsed < IDLE_WAIT * 5,
        "an idle product wait returns promptly after its deadline: {elapsed:?}"
    );
    assert_eq!(wait_observation.parks_entered(), 1);
    assert_eq!(wait_observation.parks_in_progress(), 0);
    assert_eq!(wait_observation.parks_exited(), 1);
    assert_eq!(wait_observation.timeout_exits(), 1);
    assert_eq!(wait_observation.wake_exits(), 0);

    let started = Instant::now();
    let report = pool.poll_wait(&mut completions, LONG_IDLE_WAIT);
    let elapsed = started.elapsed();

    assert_eq!(report.backend_completions(), 0);
    assert_eq!(report.reclaimed_frames(), 0);
    assert_eq!(completions.iter().count(), 0);
    assert!(
        elapsed >= Duration::from_millis(700),
        "the long wait's lower bound exceeds the short wait's entire allowed window: {elapsed:?}"
    );
    assert!(
        elapsed < LONG_IDLE_WAIT * 3,
        "the long idle product wait remains proportional to its supplied deadline: {elapsed:?}"
    );
    assert_eq!(wait_observation.parks_entered(), 2);
    assert_eq!(wait_observation.parks_in_progress(), 0);
    assert_eq!(wait_observation.parks_exited(), 2);
    assert_eq!(wait_observation.timeout_exits(), 2);
    assert_eq!(wait_observation.wake_exits(), 0);
}

#[test]
fn an_external_wake_interrupts_a_parked_poll_wait_without_backend_io() {
    let (pool, _file) = pool_with_file("pool-progress-external-wake");
    let wait_observation: MockWaitObservation = pool.driver().observe_waits();
    let wake: PoolWakeHandle = pool.wake_handle();
    let cleanup_wake = wake.clone();
    let pool = Arc::new(pool);
    let (report_tx, report_rx) = mpsc::sync_channel(1);
    let waiter = {
        let pool = Arc::clone(&pool);
        thread::spawn(move || {
            report_tx
                .send(wake_only_wait(pool))
                .expect("the wake report receiver remains live");
        })
    };

    if !wait_observation.wait_until_parked(Duration::from_secs(1)) {
        cleanup_wake.wake();
        waiter
            .join()
            .expect("waiter joins after observation timeout");
        panic!("poll_wait did not register an actual blocking wait");
    }
    assert!(matches!(
        report_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert_eq!(wait_observation.parks_entered(), 1);
    assert_eq!(wait_observation.parks_in_progress(), 1);
    assert_eq!(wait_observation.parks_exited(), 0);
    assert_eq!(wait_observation.wake_exits(), 0);
    assert_eq!(wait_observation.timeout_exits(), 0);
    thread::spawn(move || wake.wake())
        .join()
        .expect("external waker joins");

    let report = match report_rx.recv_timeout(PROMPT_WAKE) {
        Ok(report) => report,
        Err(error) => {
            cleanup_wake.wake();
            waiter.join().expect("fallback wake releases waiter");
            panic!("wake-during-park did not release the actual wait: {error}");
        }
    };
    waiter.join().expect("waiter joins");
    assert_eq!(wait_observation.parks_entered(), 1);
    assert_eq!(wait_observation.parks_in_progress(), 0);
    assert_eq!(wait_observation.parks_exited(), 1);
    assert_eq!(wait_observation.wake_exits(), 1);
    assert_eq!(wait_observation.timeout_exits(), 0);
    assert_wake_only_report(report, "wake-during-park");
}

#[test]
fn an_external_wake_before_poll_wait_is_not_lost() {
    let (pool, _file) = pool_with_file("pool-progress-prewake");
    let wait_observation: MockWaitObservation = pool.driver().observe_waits();
    let wake = pool.wake_handle();
    thread::spawn(move || wake.wake())
        .join()
        .expect("pre-waker joins before poll_wait starts");
    let pool = Arc::new(pool);
    let (report_tx, report_rx) = mpsc::sync_channel(1);
    let waiter = {
        let pool = Arc::clone(&pool);
        thread::spawn(move || {
            report_tx
                .send(wake_only_wait(pool))
                .expect("the report receiver remains live");
        })
    };

    let report = match report_rx.recv_timeout(PROMPT_WAKE) {
        Ok(report) => report,
        Err(error) => {
            pool.wake_handle().wake();
            waiter.join().expect("fallback wake releases waiter");
            panic!("wake-before-park was lost: {error}");
        }
    };
    waiter.join().expect("pre-woken waiter joins");
    let parks_entered = wait_observation.parks_entered();
    assert!(
        parks_entered <= 1,
        "a remembered wake cannot fall into a periodic re-park loop"
    );
    assert_eq!(wait_observation.parks_in_progress(), 0);
    assert_eq!(wait_observation.parks_exited(), parks_entered);
    assert_eq!(wait_observation.wake_exits(), parks_entered);
    assert_eq!(wait_observation.timeout_exits(), 0);
    assert_wake_only_report(report, "wake-before-park");
}

fn completion_token(completion: &PoolCompletion) -> PoolToken {
    match completion {
        PoolCompletion::Write { token, .. } | PoolCompletion::Fsync { token, .. } => *token,
    }
}

#[test]
fn a_bounded_pool_completion_batch_retains_the_remainder() {
    let (pool, file) = pool_with_file_product_capacity("pool-progress-partial-drain", 2);
    let first = pool
        .submit_fsync(file, SyncMode::Full)
        .expect("first barrier admits");
    let second = pool
        .submit_fsync(file, SyncMode::Full)
        .expect("second barrier admits");
    let mut retain_all = PoolCompletionBatch::with_capacity(0);
    let mut backend_completions = 0u32;
    for _ in 0..POLL_BOUND {
        let report: PollReport = pool.poll_report(&mut retain_all);
        backend_completions += report.backend_completions();
        assert_eq!(report.reclaimed_frames(), 0);
        assert_eq!(retain_all.iter().count(), 0);
        if backend_completions == 2 {
            break;
        }
    }
    assert_eq!(backend_completions, 2);
    assert!(matches!(
        pool.submit_fsync(file, SyncMode::Full),
        Err(dios::PoolSubmitError::Full)
    ));

    let mut completions = PoolCompletionBatch::with_capacity(1);
    let first_report: PollReport = pool.poll_report(&mut completions);
    assert_eq!(first_report.backend_completions(), 0);
    assert_eq!(first_report.reclaimed_frames(), 0);
    assert_eq!(completions.iter().count(), 1);
    let first_drained = completion_token(completions.iter().next().expect("one retained result"));

    let replacement = pool
        .submit_fsync(file, SyncMode::Full)
        .expect("delivering exactly one retained result releases one product-op slot");
    assert!(matches!(
        pool.submit_fsync(file, SyncMode::Full),
        Err(dios::PoolSubmitError::Full)
    ));

    assert!(first_drained == first || first_drained == second);
    let retained_original = if first_drained == first {
        second
    } else {
        first
    };
    let mut retained_seen = false;
    let mut replacement_seen = false;
    let mut cleanup_backend_completions = 0u32;
    for _ in 0..POLL_BOUND {
        let report: PollReport = pool.poll_report(&mut completions);
        cleanup_backend_completions += report.backend_completions();
        assert_eq!(report.reclaimed_frames(), 0);
        for completion in completions.iter() {
            let token = completion_token(completion);
            if token == retained_original {
                assert!(!retained_seen, "the retained result delivers exactly once");
                retained_seen = true;
            } else if token == replacement {
                assert!(
                    !replacement_seen,
                    "the replacement result delivers exactly once"
                );
                replacement_seen = true;
            } else {
                panic!("cleanup delivered an unknown product token: {token:?}");
            }
        }
        if retained_seen && replacement_seen {
            break;
        }
    }
    assert_eq!(
        cleanup_backend_completions, 1,
        "only the replacement reaches the backend during cleanup"
    );
    assert!(retained_seen);
    assert!(replacement_seen);
}

#[test]
fn caller_completion_saturation_never_starves_an_internal_read() {
    let (pool, file) = pool_with_file_product_capacity("pool-progress-internal-read", 2);
    let reader = pool.register_reader().expect("one reader slot");
    let Get::Pending(read) = pool
        .get(&reader, PageId::new(file, 0))
        .expect("the registered file is live")
    else {
        panic!("the seeded page starts cold");
    };
    let first = pool
        .submit_fsync(file, SyncMode::Full)
        .expect("first caller op admits");
    let second = pool
        .submit_fsync(file, SyncMode::Full)
        .expect("second caller op admits");
    let mut retain_all = PoolCompletionBatch::with_capacity(0);
    let mut backend_completions = 0u32;
    for _ in 0..POLL_BOUND {
        let report = pool.poll_report(&mut retain_all);
        backend_completions += report.backend_completions();
        assert_eq!(report.reclaimed_frames(), 0);
        assert_eq!(retain_all.iter().count(), 0);
        if backend_completions == 3 {
            break;
        }
    }
    assert_eq!(
        backend_completions, 3,
        "the internal read and both caller CQEs drain despite zero caller capacity"
    );
    match pool.ready(&reader, read) {
        ReadyResult::Ready(frame) => assert!(frame.iter().all(|&byte| byte == 0xC3)),
        ReadyResult::NotYet(_) => panic!("caller output saturation cannot starve read readiness"),
        ReadyResult::Err(error) => panic!("the seeded read cannot fail: {error}"),
    }

    let mut completions = PoolCompletionBatch::with_capacity(1);
    let first_report = pool.poll_report(&mut completions);
    assert_eq!(first_report.backend_completions(), 0);
    assert_eq!(first_report.reclaimed_frames(), 0);
    assert_eq!(completions.iter().count(), 1);
    let first_delivered = completion_token(completions.iter().next().expect("retained result"));
    let second_report = pool.poll_report(&mut completions);
    assert_eq!(second_report.backend_completions(), 0);
    assert_eq!(second_report.reclaimed_frames(), 0);
    assert_eq!(completions.iter().count(), 1);
    let second_delivered = completion_token(completions.iter().next().expect("retained result"));
    assert!(first_delivered == first || first_delivered == second);
    assert!(second_delivered == first || second_delivered == second);
    assert_ne!(
        first_delivered, second_delivered,
        "the second caller completion remains queued without delaying internal progress"
    );
}

#[test]
fn matured_eviction_reports_reclamation_without_a_backend_completion() {
    let (pool, file) = pool_with_file("pool-progress-reclaim-only");
    let page = PageId::new(file, 0);
    pool.insert_resident_frame(page, 0xE1);
    pool.evict_frame(page);
    let mut completions = PoolCompletionBatch::with_capacity(1);
    let mut reclaimed = 0u32;

    for _ in 0..16 {
        let report: PollReport = pool.poll_report(&mut completions);
        assert_eq!(report.backend_completions(), 0);
        assert!(completions.iter().next().is_none());
        reclaimed = reclaimed.saturating_add(report.reclaimed_frames());
        if reclaimed > 0 {
            break;
        }
    }

    assert_eq!(
        reclaimed, 1,
        "the matured frame is reclaimed within the bound"
    );
}

#[test]
fn retained_caller_completions_do_not_block_eviction_reclamation() {
    const MAX_INFLIGHT_PRODUCT_OPS: u32 = 16;
    let (pool, file) =
        pool_with_file_product_capacity("pool-progress-backlog-reclaim", MAX_INFLIGHT_PRODUCT_OPS);
    let page = PageId::new(file, 0);
    pool.insert_resident_frame(page, 0xE2);
    let evicting = pool.evict_frame(page);
    assert_eq!(pool.frame_state(evicting), FrameState::Evicting);
    for _ in 0..MAX_INFLIGHT_PRODUCT_OPS {
        pool.submit_fsync(file, SyncMode::Full)
            .expect("every configured product-op slot admits exactly once");
    }
    assert!(matches!(
        pool.submit_fsync(file, SyncMode::Full),
        Err(dios::PoolSubmitError::Full)
    ));

    let mut retain_all = PoolCompletionBatch::with_capacity(0);
    let mut backend_completions = 0u32;
    let mut reclaimed = 0u32;
    for _ in 0..POLL_BOUND {
        let report = pool.poll_report(&mut retain_all);
        backend_completions += report.backend_completions();
        reclaimed = reclaimed.saturating_add(report.reclaimed_frames());
        assert_eq!(retain_all.iter().count(), 0);
        if backend_completions == MAX_INFLIGHT_PRODUCT_OPS && reclaimed > 0 {
            break;
        }
    }
    assert_eq!(backend_completions, MAX_INFLIGHT_PRODUCT_OPS);
    assert_eq!(reclaimed, 1, "reclamation advances behind caller backlog");
    assert_eq!(pool.frame_state(evicting), FrameState::Free);

    let mut completions = PoolCompletionBatch::with_capacity(1);
    let retained = pool.poll_report(&mut completions);
    assert_eq!(retained.backend_completions(), 0);
    assert_eq!(retained.reclaimed_frames(), 0);
    assert_eq!(
        completions.iter().count(),
        1,
        "a caller completion was still retained when reclamation completed"
    );
}
