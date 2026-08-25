//! Product file retirement is deferred: new gets fail immediately, while every
//! capability admitted before retirement keeps enough state to finish safely.
//! Retirement and barrier tests share one chronological `MockIoEvent` stream
//! for backend attempts, completions, and physical closes.

#![cfg(feature = "mock")]
#![expect(
    clippy::similar_names,
    clippy::too_many_lines,
    reason = "the frozen retirement trace keeps its full capability lifecycle in one case"
)]

use std::path::Path;
use std::sync::Arc;
#[cfg(not(loom))]
use std::thread;
#[cfg(not(loom))]
use std::time::Duration;

use dios::testing::{
    FrameState, MockDriver, MockIoEvent, MockPoolObservation, MockPoolTestingExt,
    PoolBuilderTestingExt, PoolTestingExt, ReadFrameIdx,
};
use dios::{
    DirectIo, FileId, Get, GetError, PageId, Pool, PoolCompletion, PoolCompletionBatch,
    PoolSubmitError, PoolToken, ReadyResult, RetireStatus, SyncMode,
};

const FRAMES: u32 = 4;
const GRANULE: u32 = 4096;
const POLL_BOUND: u32 = 32;

fn pool_with_file(name: &str, fill: u8) -> (Pool<MockDriver>, FileId, Arc<MockPoolObservation>) {
    let mock = MockDriver::builder()
        .queue_capacity(4)
        .frames(FRAMES)
        .frame_bytes(GRANULE)
        .write_slots(2)
        .build();
    let file = mock
        .open(Path::new(name), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    mock.seed_page(&file, 0, fill);
    let pool = Pool::builder()
        .frame_count(FRAMES)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .write_slots(2)
        .max_inflight_product_ops(2)
        .build_on(mock)
        .expect("valid retirement pool");
    let observation = pool.observe();
    pool.register_file(file);
    (pool, file_id, observation)
}

fn assert_stale(pool: &Pool<MockDriver>, reader: &dios::ReaderCtx, page: PageId) {
    assert_eq!(
        pool.get(reader, page)
            .expect_err("retirement rejects new gets"),
        GetError::StaleFile { page }
    );
}

fn poll_until_closed(pool: &Pool<MockDriver>, file: FileId) {
    for _ in 0..POLL_BOUND {
        if pool.driver().is_closed(file) {
            return;
        }
        pool.poll();
    }
    assert!(
        pool.driver().is_closed(file),
        "deferred retirement closes within the bounded progress budget"
    );
}

fn assert_stays_open_while_capability_lives(pool: &Pool<MockDriver>, file: FileId) {
    for _ in 0..POLL_BOUND {
        pool.poll();
        assert!(
            !pool.driver().is_closed(file),
            "a live pre-retirement capability keeps its file open"
        );
    }
}

fn frames_in_state(pool: &Pool<MockDriver>, state: FrameState) -> u32 {
    (0..FRAMES)
        .filter(|&index| pool.frame_state(ReadFrameIdx::new(index)) == state)
        .count()
        .try_into()
        .expect("the fixed frame count fits u32")
}

fn assert_all_frames_free(pool: &Pool<MockDriver>) {
    assert_eq!(frames_in_state(pool, FrameState::Free), FRAMES);
}

fn read_completion_index(events: &[MockIoEvent], file: FileId) -> usize {
    events
        .iter()
        .position(|event| {
            matches!(
                event,
                MockIoEvent::ReadCompletion {
                    file: completed_file,
                    ..
                } if *completed_file == file
            )
        })
        .expect("the admitted read completion is observable")
}

fn close_index(events: &[MockIoEvent], file: FileId) -> usize {
    events
        .iter()
        .position(|event| {
            matches!(
                event,
                MockIoEvent::Close {
                    file: closed_file,
                    ..
                } if *closed_file == file
            )
        })
        .expect("the physical close is observable")
}

fn has_close(events: &[MockIoEvent], file: FileId) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            MockIoEvent::Close {
                file: closed_file,
                ..
            } if *closed_file == file
        )
    })
}

fn assert_stale_submit(error: PoolSubmitError, expected: FileId) {
    match error {
        PoolSubmitError::StaleFile { file } => assert_eq!(file, expected),
        other => panic!("retired file {expected:?} reported the wrong submit error: {other:?}"),
    }
}

fn completion_for(batch: &PoolCompletionBatch, expected: PoolToken) -> &PoolCompletion {
    batch
        .iter()
        .find(|completion| match completion {
            PoolCompletion::Write { token, .. } | PoolCompletion::Fsync { token, .. } => {
                *token == expected
            }
        })
        .expect("the exact pre-retirement operation completes")
}

fn only_completion_token(batch: &PoolCompletionBatch) -> PoolToken {
    match batch
        .iter()
        .next()
        .expect("exactly one result is delivered")
    {
        PoolCompletion::Write { token, .. } | PoolCompletion::Fsync { token, .. } => *token,
    }
}

#[test]
fn a_waiter_dropped_before_retirement_leaves_dma_to_drain_before_close() {
    let (pool, file, observation) = pool_with_file("pool-retire-dropped-waiter", 0xD1);
    let reader = pool.register_reader().expect("one reader slot");
    let page = PageId::new(file, 0);
    let Get::Pending(token) = pool.get(&reader, page).expect("the file is live") else {
        panic!("the cold page must admit a miss");
    };

    drop(token);
    assert_eq!(observation.live_pending_interests(), 0);
    assert_eq!(
        observation.backend_ops_in_flight(),
        1,
        "dropping requester interest does not cancel submitted DMA"
    );
    assert_eq!(pool.retire_file(file), RetireStatus::Retiring);
    assert_eq!(pool.retire_file(file), RetireStatus::Retiring);
    assert_stale(&pool, &reader, page);
    assert!(!pool.driver().is_closed(file));
    assert_eq!(observation.backend_ops_in_flight(), 1);

    poll_until_closed(&pool, file);

    let events = pool.driver().io_events_in_order();
    assert!(
        read_completion_index(&events, file) < close_index(&events, file),
        "physical close occurs strictly after the admitted DMA completion"
    );
    assert_eq!(observation.backend_ops_in_flight(), 0);
    assert_all_frames_free(&pool);
    assert_eq!(pool.retire_file(file), RetireStatus::Retired);
}

#[cfg(not(loom))]
#[test]
fn a_cold_get_rechecks_the_exact_generation_after_concurrent_retirement() {
    let (pool, file, observation) = pool_with_file("pool-retire-racing-cold-get", 0xC7);
    let pool = Arc::new(pool);
    let page = PageId::new(file, 0);
    let pause = pool.pause_next_cold_get();
    let getter_pool = Arc::clone(&pool);

    let getter = thread::spawn(move || {
        let reader = getter_pool.register_reader().expect("one reader slot");
        match getter_pool.get(&reader, page) {
            Ok(_) => Ok(()),
            Err(error) => Err(error),
        }
    });
    assert!(
        pause.wait_until_parked(Duration::from_secs(1)),
        "the cold get reaches the post-check, pre-admission pause"
    );
    assert_eq!(pool.retire_file(file), RetireStatus::Retiring);
    pause.release();

    assert_eq!(
        getter.join().expect("concurrent get must not panic"),
        Err(GetError::StaleFile { page })
    );
    assert_eq!(observation.backend_ops_in_flight(), 0);
    assert!(
        pool.driver()
            .io_events_in_order()
            .iter()
            .all(|event| !matches!(
                event,
                MockIoEvent::ReadAttempt {
                    file: attempted_file,
                    ..
                } if *attempted_file == file
            )),
        "retirement wins before cold admission, so no backend read is attempted"
    );
}

#[test]
fn a_pending_token_admitted_before_retirement_readies_before_physical_close() {
    let (pool, file, observation) = pool_with_file("pool-retire-live-pending", 0xD6);
    let reader = pool.register_reader().expect("one reader slot");
    let page = PageId::new(file, 0);
    let Get::Pending(mut token) = pool.get(&reader, page).expect("the file is live") else {
        panic!("the cold page must admit a miss");
    };
    assert_eq!(observation.live_pending_interests(), 1);
    assert_eq!(observation.backend_ops_in_flight(), 1);

    assert_eq!(pool.retire_file(file), RetireStatus::Retiring);
    assert_stale(&pool, &reader, page);
    assert!(
        !pool.driver().is_closed(file),
        "retirement cannot close over an admitted pending capability"
    );

    let mut polls_remaining = POLL_BOUND;
    let guard = 'ready: loop {
        assert!(
            polls_remaining > 0,
            "the admitted read readies within the bounded progress budget"
        );
        polls_remaining -= 1;
        match pool.ready(&reader, token) {
            ReadyResult::Ready(guard) => break 'ready guard,
            ReadyResult::NotYet(handed_back) => {
                token = handed_back;
                pool.poll();
                assert!(
                    !pool.driver().is_closed(file),
                    "the still-live token keeps the physical file open while DMA drains"
                );
            }
            ReadyResult::Err(error) => panic!("the admitted seeded read remains valid: {error}"),
        }
    };
    assert!(guard.iter().all(|&byte| byte == 0xD6));
    assert!(
        !pool.driver().is_closed(file),
        "the ready guard remains a pre-retirement live capability"
    );

    drop(guard);
    poll_until_closed(&pool, file);
    let events = pool.driver().io_events_in_order();
    assert!(
        read_completion_index(&events, file) < close_index(&events, file),
        "the admitted read completes before physical close"
    );
    assert_all_frames_free(&pool);
    assert_eq!(pool.retire_file(file), RetireStatus::Retired);
}

#[test]
fn a_completed_unconsumed_token_finishes_after_retirement() {
    let (pool, file, _observation) = pool_with_file("pool-retire-completed-token", 0xD2);
    let reader = pool.register_reader().expect("one reader slot");
    let page = PageId::new(file, 0);
    let Get::Pending(token) = pool.get(&reader, page).expect("the file is live") else {
        panic!("the cold page must admit a miss");
    };
    pool.poll();

    assert_eq!(pool.retire_file(file), RetireStatus::Retiring);
    assert_stale(&pool, &reader, page);
    assert_stays_open_while_capability_lives(&pool, file);

    match pool.ready(&reader, token) {
        ReadyResult::Ready(frame) => {
            assert!(frame.iter().all(|&byte| byte == 0xD2));
        }
        ReadyResult::NotYet(_) => panic!("the completion was drained before retirement"),
        ReadyResult::Err(error) => panic!("the admitted read remains valid: {error}"),
    }
    poll_until_closed(&pool, file);

    assert_all_frames_free(&pool);
    assert_eq!(pool.retire_file(file), RetireStatus::Retired);
}

#[test]
fn a_live_frame_guard_delays_reclamation_and_file_close() {
    let (pool, file, _observation) = pool_with_file("pool-retire-live-guard", 0xD3);
    let reader = pool.register_reader().expect("one reader slot");
    let page = PageId::new(file, 0);
    let Get::Pending(token) = pool.get(&reader, page).expect("the file is live") else {
        panic!("the cold page must admit a miss");
    };
    pool.poll();
    let guard = match pool.ready(&reader, token) {
        ReadyResult::Ready(frame) => frame,
        ReadyResult::NotYet(_) => panic!("the deterministic read completed"),
        ReadyResult::Err(error) => panic!("the seeded read cannot fail: {error}"),
    };

    assert_eq!(pool.retire_file(file), RetireStatus::Retiring);
    assert_stale(&pool, &reader, page);
    assert_eq!(
        frames_in_state(&pool, FrameState::Evicting),
        1,
        "retirement invalidates the resident mapping before deferred reclaim"
    );
    assert_stays_open_while_capability_lives(&pool, file);
    assert!(
        guard.iter().all(|&byte| byte == 0xD3),
        "the pre-retirement guard remains readable through deferred reclamation"
    );
    assert!(
        !has_close(&pool.driver().io_events_in_order(), file),
        "physical close is absent until the live guard is explicitly dropped"
    );

    drop(guard);
    poll_until_closed(&pool, file);

    assert!(
        has_close(&pool.driver().io_events_in_order(), file),
        "physical close appears only after the explicit guard drop"
    );
    assert_all_frames_free(&pool);
    assert_eq!(pool.retire_file(file), RetireStatus::Retired);
}

#[test]
fn an_already_resident_unpinned_page_retires_without_backend_progress() {
    let (pool, file, observation) = pool_with_file("pool-retire-resident", 0xD4);
    let reader = pool.register_reader().expect("one reader slot");
    let page = PageId::new(file, 0);
    pool.insert_resident_frame(page, 0xD4);
    assert_eq!(observation.backend_ops_in_flight(), 0);

    assert_eq!(pool.retire_file(file), RetireStatus::Retiring);
    assert_stale(&pool, &reader, page);
    assert_eq!(frames_in_state(&pool, FrameState::Evicting), 1);
    poll_until_closed(&pool, file);

    assert_eq!(observation.backend_completions(), 0);
    assert_all_frames_free(&pool);
    assert_eq!(pool.retire_file(file), RetireStatus::Retired);
}

#[test]
fn retirement_drains_preaccepted_writes_rejects_new_io_and_preserves_generation_staleness() {
    let path = Path::new("pool-retire-write-barrier");
    let (pool, file, _observation) =
        pool_with_file(path.to_str().expect("the fixture path is UTF-8"), 0xD5);
    let reader = pool.register_reader().expect("one reader slot");
    let mut write_slot = pool.write_arena().alloc().expect("first staging slot");
    write_slot.fill(0x5A);
    let write = pool
        .submit_write(file, write_slot, 0)
        .expect("write admits before retirement");
    let fsync = pool
        .submit_fsync(file, SyncMode::Full)
        .expect("barrier admits before retirement");

    assert_eq!(pool.retire_file(file), RetireStatus::Retiring);
    let mut rejected_slot = pool.write_arena().alloc().expect("second staging slot");
    rejected_slot.fill(0x7D);
    let (write_error, returned_slot) = pool
        .submit_write(file, rejected_slot, GRANULE.into())
        .expect_err("retirement rejects a new write and returns its staging slot");
    assert_stale_submit(write_error, file);
    assert_eq!(returned_slot.len(), GRANULE as usize);
    assert!(
        returned_slot.iter().all(|&byte| byte == 0x7D),
        "stale-file rejection preserves the caller's complete payload"
    );
    assert_stale_submit(
        pool.submit_fsync(file, SyncMode::Full)
            .expect_err("retirement rejects a new barrier"),
        file,
    );
    assert_stale(&pool, &reader, PageId::new(file, 0));
    assert!(!pool.driver().is_closed(file));

    let mut retain_all = PoolCompletionBatch::with_capacity(0);
    let mut backend_completions = 0u32;
    for _ in 0..POLL_BOUND {
        let report = pool.poll_report(&mut retain_all);
        backend_completions += report.backend_completions();
        assert_eq!(report.reclaimed_frames(), 0);
        assert_eq!(retain_all.iter().count(), 0);
        assert_eq!(pool.retire_file(file), RetireStatus::Retiring);
        assert!(!pool.driver().is_closed(file));
        if backend_completions == 2 {
            break;
        }
    }
    assert_eq!(backend_completions, 2);
    assert_stays_open_while_capability_lives(&pool, file);
    assert_eq!(pool.retire_file(file), RetireStatus::Retiring);

    let mut completions = PoolCompletionBatch::with_capacity(1);
    let first_report = pool.poll_report(&mut completions);
    assert_eq!(first_report.backend_completions(), 0);
    assert_eq!(first_report.reclaimed_frames(), 0);
    assert_eq!(completions.iter().count(), 1);
    let first_completed = only_completion_token(&completions);
    match completion_for(&completions, first_completed) {
        PoolCompletion::Write {
            token,
            result: Ok(bytes),
        } => {
            assert_eq!(*token, write);
            assert_eq!(*bytes, GRANULE);
        }
        PoolCompletion::Fsync {
            token,
            result: Ok(()),
        } => assert_eq!(*token, fsync),
        PoolCompletion::Write {
            result: Err(error), ..
        } => panic!("the preaccepted write failed: {error}"),
        PoolCompletion::Fsync {
            result: Err(error), ..
        } => panic!("the preaccepted barrier failed: {error}"),
    }
    assert_eq!(
        pool.retire_file(file),
        RetireStatus::Retiring,
        "one caller result remains retained after the first delivery"
    );
    assert_stays_open_while_capability_lives(&pool, file);

    let remaining = if first_completed == write {
        fsync
    } else {
        write
    };
    let second_report = pool.poll_report(&mut completions);
    assert_eq!(
        second_report.backend_completions(),
        0,
        "retained-result delivery is not a newly drained backend CQE"
    );
    assert_eq!(second_report.reclaimed_frames(), 0);
    assert_eq!(completions.iter().count(), 1);
    assert_eq!(only_completion_token(&completions), remaining);
    match completion_for(&completions, remaining) {
        PoolCompletion::Write {
            token,
            result: Ok(bytes),
        } => {
            assert_eq!(*token, write);
            assert_eq!(*bytes, GRANULE);
        }
        PoolCompletion::Fsync {
            token,
            result: Ok(()),
        } => assert_eq!(*token, fsync),
        PoolCompletion::Write {
            result: Err(error), ..
        } => panic!("the retained write failed: {error}"),
        PoolCompletion::Fsync {
            result: Err(error), ..
        } => panic!("the retained barrier failed: {error}"),
    }

    poll_until_closed(&pool, file);
    let events = pool.driver().io_events_in_order();
    let close = close_index(&events, file);
    let write_completion = events
        .iter()
        .position(|event| {
            matches!(
                event,
                MockIoEvent::WriteCompletion {
                    file: completed_file,
                    ..
                } if *completed_file == file
            )
        })
        .expect("the preaccepted write completion is observable");
    let fsync_completion = events
        .iter()
        .position(|event| {
            matches!(
                event,
                MockIoEvent::FsyncCompletion {
                    file: completed_file,
                    ..
                } if *completed_file == file
            )
        })
        .expect("the preaccepted fsync completion is observable");
    assert!(write_completion < close);
    assert!(fsync_completion < close);
    assert_eq!(pool.retire_file(file), RetireStatus::Retired);

    let reopened = pool
        .open(path, DirectIo::Disabled)
        .expect("the retired backend slot can be reused");
    assert!(
        file.aliases_slot(&reopened),
        "the fixture exercises generation change on the same backend slot"
    );
    assert_ne!(reopened, file, "slot reuse mints a fresh file generation");
    assert_stale(&pool, &reader, PageId::new(file, 0));
    assert_stale_submit(
        pool.submit_fsync(file, SyncMode::Full)
            .expect_err("the old generation remains stale after slot reuse"),
        file,
    );

    let (old_write_error, returned_slot) = pool
        .submit_write(file, returned_slot, GRANULE.into())
        .expect_err("the old generation also rejects writes after slot reuse");
    assert_stale_submit(old_write_error, file);
    assert_eq!(returned_slot.len(), GRANULE as usize);
    assert!(
        returned_slot.iter().all(|&byte| byte == 0x7D),
        "old-generation rejection returns the exact unchanged staging payload"
    );

    let reopened_write = pool
        .submit_write(reopened, returned_slot, GRANULE.into())
        .expect("the stale-rejected slot writes through the new generation");
    let reopened_fsync = pool
        .submit_fsync(reopened, SyncMode::Full)
        .expect("the new generation accepts IO");
    let mut reopened_completions = PoolCompletionBatch::with_capacity(2);
    let mut backend_completions = 0u32;
    let mut write_seen = false;
    let mut fsync_seen = false;
    for _ in 0..POLL_BOUND {
        let report = pool.poll_report(&mut reopened_completions);
        backend_completions += report.backend_completions();
        assert_eq!(report.reclaimed_frames(), 0);
        for completion in reopened_completions.iter() {
            match completion {
                PoolCompletion::Write {
                    token,
                    result: Ok(bytes),
                } => {
                    assert_eq!(*token, reopened_write);
                    assert_eq!(*bytes, GRANULE);
                    assert!(!write_seen);
                    write_seen = true;
                }
                PoolCompletion::Fsync {
                    token,
                    result: Ok(()),
                } => {
                    assert_eq!(*token, reopened_fsync);
                    assert!(!fsync_seen);
                    fsync_seen = true;
                }
                PoolCompletion::Write {
                    result: Err(error), ..
                } => panic!("the reopened generation's write failed: {error}"),
                PoolCompletion::Fsync {
                    result: Err(error), ..
                } => panic!("the reopened generation's barrier failed: {error}"),
            }
        }
        if write_seen && fsync_seen {
            break;
        }
    }
    assert_eq!(backend_completions, 2);
    assert!(write_seen);
    assert!(fsync_seen);
    let mut stored = [0u8; GRANULE as usize];
    assert_eq!(
        pool.driver()
            .copy_stored_bytes(reopened, u64::from(GRANULE), &mut stored),
        GRANULE as usize
    );
    assert!(
        stored.iter().all(|&byte| byte == 0x7D),
        "the stale-rejected payload survives and persists through slot reuse"
    );
}
