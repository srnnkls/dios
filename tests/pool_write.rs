//! Pool writes and barriers use product-level tokens and completions while the
//! composed backend retains staging, ordering, and fault routing internally.
//! `MockIoEvent` is the single chronological mock stream for backend attempts,
//! completions, and closes. The frozen `read_attempts_in_order` and
//! `write_attempts_in_order` accessors are typed projections of this stream;
//! no parallel attempt recorder exists.

#![cfg(feature = "mock")]
#![expect(
    clippy::similar_names,
    clippy::too_many_lines,
    reason = "the frozen adversarial schedule and precedence cases remain intentionally contiguous"
)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use dios::testing::{
    Injected, MockDriver, MockIoEvent, MockPoolIoTestingExt, MockPoolTestingExt,
    PoolBuilderTestingExt, PoolTestingExt, WriteAttempt,
};
use dios::{
    DirectIo, Pool, PoolCompletion, PoolCompletionBatch, PoolSubmitError, PoolToken, RetireStatus,
    SyncMode,
};

const GRANULE: u32 = 4096;
const POLL_BOUND: u32 = 64;

fn pool_with_file(name: &str, seed: u64) -> (Pool<MockDriver>, dios::FileId) {
    pool_with_limits(name, seed, 2, 1)
}

fn pool_with_limits(
    name: &str,
    seed: u64,
    max_inflight_product_ops: u32,
    write_slots: u32,
) -> (Pool<MockDriver>, dios::FileId) {
    let queue_capacity = 1u32
        .checked_add(max_inflight_product_ops)
        .expect("small fixture capacities add without overflow");
    let mock = MockDriver::builder()
        .seed(seed)
        .queue_capacity(queue_capacity)
        .frames(4)
        .frame_bytes(GRANULE)
        .write_slots(write_slots)
        .build();
    let file = mock
        .open(Path::new(name), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    let pool = Pool::builder()
        .frame_count(4)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .write_slots(write_slots)
        .max_inflight_product_ops(max_inflight_product_ops)
        .build_on(mock)
        .expect("valid write pool");
    pool.register_file(file);
    (pool, file_id)
}

fn find_completion(completions: &PoolCompletionBatch, expected: PoolToken) -> &PoolCompletion {
    completions
        .iter()
        .find(|completion| match completion {
            PoolCompletion::Write { token, .. } | PoolCompletion::Fsync { token, .. } => {
                *token == expected
            }
        })
        .expect("the exact pool token completes")
}

fn assert_write_success(completions: &PoolCompletionBatch, token: PoolToken, bytes: u32) {
    match find_completion(completions, token) {
        PoolCompletion::Write {
            token: completed,
            result: Ok(completed_bytes),
        } => {
            assert_eq!(*completed, token);
            assert_eq!(*completed_bytes, bytes);
        }
        PoolCompletion::Write {
            result: Err(error), ..
        } => {
            panic!("write unexpectedly failed: {error}")
        }
        PoolCompletion::Fsync { .. } => panic!("the write token cannot complete as fsync"),
    }
}

fn assert_fsync_success(completions: &PoolCompletionBatch, token: PoolToken) {
    match find_completion(completions, token) {
        PoolCompletion::Fsync {
            token: completed,
            result: Ok(()),
        } => assert_eq!(*completed, token),
        PoolCompletion::Fsync {
            result: Err(error), ..
        } => {
            panic!("fsync unexpectedly failed: {error}")
        }
        PoolCompletion::Write { .. } => panic!("the fsync token cannot complete as write"),
    }
}

fn assert_write_failure(completion: &PoolCompletion, token: PoolToken, errno: i32) {
    match completion {
        PoolCompletion::Write {
            token: completed,
            result: Err(error),
        } => {
            assert_eq!(*completed, token);
            assert_eq!(error.raw_os_error(), Some(errno));
        }
        PoolCompletion::Write {
            result: Ok(bytes), ..
        } => {
            panic!("the injected write unexpectedly transferred {bytes} bytes")
        }
        PoolCompletion::Fsync { .. } => panic!("the write token cannot complete as fsync"),
    }
}

fn assert_fsync_failure(completion: &PoolCompletion, token: PoolToken, errno: i32) {
    match completion {
        PoolCompletion::Fsync {
            token: completed,
            result: Err(error),
        } => {
            assert_eq!(*completed, token);
            assert_eq!(error.raw_os_error(), Some(errno));
        }
        PoolCompletion::Fsync { result: Ok(()), .. } => {
            panic!("the injected fsync unexpectedly succeeded")
        }
        PoolCompletion::Write { .. } => panic!("the fsync token cannot complete as write"),
    }
}

#[test]
fn dropping_pool_drains_a_write_and_its_held_fsync_before_close() {
    let (pool, file) = pool_with_limits("pool-drop-held-fsync", 0xD20F, 2, 1);
    let observation = pool.observe();
    let io_observation = pool.observe_io();
    let mut slot = pool.write_arena().alloc().expect("one staging slot");
    slot.fill(0xD2);
    let _write = pool
        .submit_write(file, slot, 0)
        .expect("write accepted before drop");
    let _fsync = pool
        .submit_fsync(file, SyncMode::Full)
        .expect("fsync accepted and held behind the write");

    drop(pool);

    assert_eq!(observation.backend_ops_in_flight(), 0);
    assert_eq!(observation.backend_completions(), 2);
    assert_eq!(observation.quiesce_calls(), 1);
    assert_eq!(
        io_observation.io_events_in_order(),
        vec![
            MockIoEvent::WriteAttempt {
                file,
                file_offset: 0,
                source_offset: 0,
                requested_len: GRANULE,
            },
            MockIoEvent::WriteCompletion {
                file,
                result: Ok(GRANULE),
            },
            MockIoEvent::FsyncAttempt { file },
            MockIoEvent::FsyncCompletion {
                file,
                result: Ok(()),
            },
            MockIoEvent::Close { file },
        ],
        "Pool drop drives every accepted product op terminal before closing the file"
    );
}

#[test]
fn writes_precede_their_fsync_barrier_under_adversarial_mock_schedules() {
    for seed in 0..8 {
        let name = format!("pool-write-barrier-seed-{seed}");
        let (pool, file) = pool_with_limits(&name, seed, 4, 3);
        let mut writes = Vec::with_capacity(3);
        for index in 0..3u32 {
            let mut slot = pool
                .write_arena()
                .alloc()
                .expect("three staging slots are reserved");
            slot.fill(0x50 + u8::try_from(index).expect("small write index"));
            writes.push(
                pool.submit_write(file, slot, u64::from(index * GRANULE))
                    .expect("each write is admitted before its barrier"),
            );
        }
        let fsync = pool
            .submit_fsync(file, SyncMode::Full)
            .expect("the barrier is admitted behind all three writes");

        let mut completions = PoolCompletionBatch::with_capacity(4);
        let mut seen_writes = [false; 3];
        let mut seen_fsync = false;
        let mut backend_completions = 0u32;
        for _ in 0..POLL_BOUND {
            let report = pool.poll_report(&mut completions);
            backend_completions += report.backend_completions();
            assert_eq!(report.reclaimed_frames(), 0);
            for completion in completions.iter() {
                match completion {
                    PoolCompletion::Write {
                        token,
                        result: Ok(bytes),
                    } => {
                        let index = writes
                            .iter()
                            .position(|expected| expected == token)
                            .expect("every write completion carries a submitted pool token");
                        assert!(!seen_writes[index], "a write token completes exactly once");
                        assert_eq!(*bytes, GRANULE);
                        seen_writes[index] = true;
                    }
                    PoolCompletion::Fsync {
                        token,
                        result: Ok(()),
                    } => {
                        assert_eq!(*token, fsync);
                        assert!(!seen_fsync, "the barrier token completes exactly once");
                        seen_fsync = true;
                    }
                    PoolCompletion::Write {
                        result: Err(error), ..
                    } => panic!("seed {seed}: write unexpectedly failed: {error}"),
                    PoolCompletion::Fsync {
                        result: Err(error), ..
                    } => panic!("seed {seed}: fsync unexpectedly failed: {error}"),
                }
            }
            if seen_writes.into_iter().all(|seen| seen) && seen_fsync {
                break;
            }
        }
        assert!(seen_writes.into_iter().all(|seen| seen));
        assert!(seen_fsync);
        assert_eq!(backend_completions, 4);

        let events = pool.driver().io_events_in_order();
        let projected_write_attempts: Vec<WriteAttempt> = events
            .iter()
            .filter_map(|event| match event {
                MockIoEvent::WriteAttempt {
                    file: attempted_file,
                    file_offset,
                    source_offset,
                    requested_len,
                } if *attempted_file == file => Some(WriteAttempt {
                    file_offset: *file_offset,
                    source_offset: *source_offset,
                    requested_len: *requested_len,
                }),
                _ => None,
            })
            .collect();
        assert_eq!(
            pool.driver().write_attempts_in_order(),
            projected_write_attempts,
            "the frozen write-attempt accessor is the typed projection of the unified event stream"
        );
        let fsync_attempt = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    MockIoEvent::FsyncAttempt {
                        file: attempted_file,
                    } if *attempted_file == file
                )
            })
            .expect("the fsync backend attempt is observable");
        let mut write_attempts = 0usize;
        let mut attempted_offsets = [false; 3];
        for (event_index, event) in events.iter().enumerate() {
            if let MockIoEvent::WriteAttempt {
                file: attempted_file,
                file_offset,
                requested_len,
                ..
            } = event
            {
                if *attempted_file != file {
                    continue;
                }
                assert!(
                    event_index < fsync_attempt,
                    "seed {seed}: every prior write attempt precedes the fsync attempt"
                );
                assert_eq!(*requested_len, GRANULE);
                assert_eq!(*file_offset % u64::from(GRANULE), 0);
                let slot = usize::try_from(*file_offset / u64::from(GRANULE))
                    .expect("small fixture offset");
                assert!(slot < attempted_offsets.len());
                attempted_offsets[slot] = true;
                write_attempts += 1;
            }
        }
        assert_eq!(write_attempts, 3, "seed {seed}: all writes were attempted");
        assert!(attempted_offsets.into_iter().all(|attempted| attempted));
        assert_eq!(
            events[..fsync_attempt]
                .iter()
                .filter(|event| matches!(event, MockIoEvent::WriteCompletion { file: completed_file, .. } if *completed_file == file))
                .count(),
            3,
            "seed {seed}: the fsync attempt stays withheld until all prior writes complete"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, MockIoEvent::WriteCompletion { file: completed_file, .. } if *completed_file == file))
                .count(),
            3,
            "seed {seed}: the unified stream contains every write completion"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, MockIoEvent::FsyncCompletion { file: completed_file, .. } if *completed_file == file))
                .count(),
            1,
            "seed {seed}: the unified stream contains the fsync completion"
        );

        for index in 0..3u32 {
            let mut stored = [0u8; GRANULE as usize];
            let copied =
                pool.driver()
                    .copy_stored_bytes(file, u64::from(index * GRANULE), &mut stored);
            assert_eq!(copied, GRANULE as usize);
            let fill = 0x50 + u8::try_from(index).expect("small write index");
            assert!(stored.iter().all(|&byte| byte == fill));
        }
        assert!(pool.write_arena().alloc().is_some(), "staging is reclaimed");
    }
}

#[test]
fn product_writes_are_disabled_by_default_even_if_the_backend_has_staging() {
    let mock = MockDriver::builder()
        .queue_capacity(2)
        .frames(4)
        .frame_bytes(GRANULE)
        .write_slots(1)
        .build();
    let file = mock
        .open(Path::new("pool-write-default-disabled"), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    let pool = Pool::builder()
        .frame_count(4)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .build_on(mock)
        .expect("the read-only default remains valid");
    pool.register_file(file);

    assert!(
        pool.write_arena().alloc().is_none(),
        "write_slots defaults to zero even when a supplied backend owns staging"
    );
    assert!(matches!(
        pool.submit_fsync(file_id, SyncMode::Full),
        Err(PoolSubmitError::Full)
    ));
}

#[test]
fn an_injected_write_failure_keeps_the_following_fsync_result_separate() {
    const ERRNO: i32 = 71;
    let (pool, file) = pool_with_file("pool-write-failure", 1);
    pool.driver().inject_next(Injected::Io(ERRNO));
    let arena = pool.write_arena();
    let slot = arena.alloc().expect("one staging slot");
    let write = pool.submit_write(file, slot, 0).expect("write admits");
    let mut completions = PoolCompletionBatch::with_capacity(2);
    let write_report = pool.poll_report(&mut completions);
    assert_eq!(write_report.backend_completions(), 1);
    assert_eq!(write_report.reclaimed_frames(), 0);
    assert_eq!(
        completions.iter().count(),
        1,
        "only the failed write completes"
    );
    assert_write_failure(find_completion(&completions, write), write, ERRNO);
    assert!(
        pool.write_arena().alloc().is_some(),
        "failure reclaims staging"
    );

    let fsync = pool
        .submit_fsync(file, SyncMode::Full)
        .expect("later fsync admits");
    let fsync_report = pool.poll_report(&mut completions);
    assert_eq!(fsync_report.backend_completions(), 1);
    assert_eq!(fsync_report.reclaimed_frames(), 0);
    assert_eq!(
        completions.iter().count(),
        1,
        "only the later fsync completes"
    );
    assert_fsync_success(&completions, fsync);
}

#[test]
fn an_injected_fsync_failure_does_not_poison_the_preceding_write() {
    const ERRNO: i32 = 72;
    let (pool, file) = pool_with_file("pool-fsync-failure", 2);
    let arena = pool.write_arena();
    let slot = arena.alloc().expect("one staging slot");
    let write = pool.submit_write(file, slot, 0).expect("write admits");
    let mut completions = PoolCompletionBatch::with_capacity(2);
    let write_report = pool.poll_report(&mut completions);
    assert_eq!(write_report.backend_completions(), 1);
    assert_eq!(write_report.reclaimed_frames(), 0);
    assert_eq!(completions.iter().count(), 1, "only the write completes");
    assert_write_success(&completions, write, GRANULE);

    pool.driver().inject_next(Injected::Io(ERRNO));
    let fsync = pool
        .submit_fsync(file, SyncMode::Full)
        .expect("fsync admits");
    let fsync_report = pool.poll_report(&mut completions);
    assert_eq!(fsync_report.backend_completions(), 1);
    assert_eq!(fsync_report.reclaimed_frames(), 0);
    assert_eq!(
        completions.iter().count(),
        1,
        "only the failed fsync completes"
    );
    assert_fsync_failure(find_completion(&completions, fsync), fsync, ERRNO);
    assert!(
        pool.write_arena().alloc().is_some(),
        "write staging remains reusable"
    );
}

#[test]
fn foreign_slot_identity_precedes_file_identity_and_product_capacity_checks() {
    let (source, source_file) = pool_with_limits("pool-write-slot-source", 3, 1, 1);
    let (target, target_file) = pool_with_limits("pool-write-slot-target", 4, 1, 1);
    let blocker = target
        .submit_fsync(target_file, SyncMode::Full)
        .expect("the target's sole product-op slot is occupied");
    let mut slot = source
        .write_arena()
        .alloc()
        .expect("the source pool mints one staging slot");
    slot.fill(0x6B);

    let (error, returned) = target
        .submit_write(source_file, slot, 0)
        .expect_err("slot identity is checked before foreign FileId or Full");
    assert!(matches!(error, PoolSubmitError::ForeignPool));
    assert_eq!(returned.len(), GRANULE as usize);
    assert!(returned.iter().all(|&byte| byte == 0x6B));

    let mut target_completions = PoolCompletionBatch::with_capacity(1);
    let target_report = target.poll_report(&mut target_completions);
    assert_eq!(target_report.backend_completions(), 1);
    assert_eq!(target_report.reclaimed_frames(), 0);
    assert_eq!(target_completions.iter().count(), 1);
    assert_fsync_success(&target_completions, blocker);

    let token = source
        .submit_write(source_file, returned, 0)
        .expect("the exact returned slot remains valid at its source pool");
    let mut source_completions = PoolCompletionBatch::with_capacity(1);
    let source_report = source.poll_report(&mut source_completions);
    assert_eq!(source_report.backend_completions(), 1);
    assert_eq!(source_report.reclaimed_frames(), 0);
    assert_eq!(source_completions.iter().count(), 1);
    assert_write_success(&source_completions, token, GRANULE);

    let mut stored = [0u8; GRANULE as usize];
    assert_eq!(
        source
            .driver()
            .copy_stored_bytes(source_file, 0, &mut stored),
        GRANULE as usize
    );
    assert!(
        stored.iter().all(|&byte| byte == 0x6B),
        "the exact foreign-rejected payload reaches its source file"
    );
}

#[test]
fn foreign_file_ids_panic_before_submit_admission_and_raii_reclaims_write_staging() {
    let (source, foreign_file) = pool_with_limits("pool-write-foreign-file-source", 30, 1, 1);
    let (target, target_file) = pool_with_limits("pool-write-foreign-file-target", 31, 1, 2);
    let mut blocker_slot = target
        .write_arena()
        .alloc()
        .expect("the target owns the first staging slot");
    blocker_slot.fill(0x11);
    let blocker = target
        .submit_write(target_file, blocker_slot, GRANULE.into())
        .expect("the target's sole product-op slot is occupied");

    let mut misuse_slot = target
        .write_arena()
        .alloc()
        .expect("the target owns a second staging slot for the misuse probe");
    misuse_slot.fill(0xA7);

    let write_panic = catch_unwind(AssertUnwindSafe(|| {
        let _ = target.submit_write(foreign_file, misuse_slot, 0);
    }));
    assert!(
        write_panic.is_err(),
        "foreign FileId misuse panics before the saturated-capacity check"
    );

    let mut reclaimed = target
        .write_arena()
        .alloc()
        .expect("unwinding drops the consumed RAII slot back into its arena");
    assert_eq!(reclaimed.len(), GRANULE as usize);
    reclaimed.fill(0x5C);
    assert!(
        target.write_arena().alloc().is_none(),
        "the in-flight blocker plus immediately reallocated RAII slot occupy both staging slots"
    );

    let fsync_panic = catch_unwind(AssertUnwindSafe(|| {
        let _ = target.submit_fsync(foreign_file, SyncMode::Full);
    }));
    assert!(
        fsync_panic.is_err(),
        "foreign FileId misuse panics before the saturated fsync-capacity check"
    );

    let mut source_results = PoolCompletionBatch::with_capacity(0);
    let mut source_retired = false;
    for _ in 0..POLL_BOUND {
        match source.retire_file(foreign_file) {
            RetireStatus::Retired => {
                source_retired = true;
                break;
            }
            RetireStatus::Retiring => {
                let report = source.poll_report(&mut source_results);
                assert_eq!(source_results.iter().count(), 0);
                assert_eq!(report.backend_completions(), 0);
            }
        }
    }
    assert!(
        source_retired,
        "the idle source file retires within the bound"
    );

    let retired_foreign_write = catch_unwind(AssertUnwindSafe(|| {
        let _ = target.submit_write(foreign_file, reclaimed, 0);
    }));
    assert!(
        retired_foreign_write.is_err(),
        "foreign driver identity is checked before the source generation's retired state"
    );
    let mut reclaimed = target
        .write_arena()
        .alloc()
        .expect("the retired-foreign panic also returns RAII staging capacity");
    reclaimed.fill(0x5C);

    let mut completions = PoolCompletionBatch::with_capacity(1);
    let blocker_report = target.poll_report(&mut completions);
    assert_eq!(blocker_report.backend_completions(), 1);
    assert_eq!(blocker_report.reclaimed_frames(), 0);
    assert_eq!(completions.iter().count(), 1);
    assert_write_success(&completions, blocker, GRANULE);

    let write = target
        .submit_write(target_file, reclaimed, 0)
        .expect("the immediately reallocated slot is reusable");
    let mut write_seen = false;
    let mut backend_completions = 0u32;
    for _ in 0..POLL_BOUND {
        let report = target.poll_report(&mut completions);
        backend_completions += report.backend_completions();
        if completions.iter().next().is_some() {
            assert_write_success(&completions, write, GRANULE);
            write_seen = true;
            break;
        }
    }
    assert!(write_seen);
    assert_eq!(backend_completions, 1);

    let fsync = target
        .submit_fsync(target_file, SyncMode::Full)
        .expect("both foreign panics consumed no admission capacity");
    let mut fsync_seen = false;
    let mut backend_completions = 0u32;
    for _ in 0..POLL_BOUND {
        let report = target.poll_report(&mut completions);
        backend_completions += report.backend_completions();
        if completions.iter().next().is_some() {
            assert_fsync_success(&completions, fsync);
            fsync_seen = true;
            break;
        }
    }
    assert!(fsync_seen);
    assert_eq!(backend_completions, 1);

    let mut stored = [0u8; GRANULE as usize];
    assert_eq!(
        target
            .driver()
            .copy_stored_bytes(target_file, 0, &mut stored),
        GRANULE as usize
    );
    assert!(stored.iter().all(|&byte| byte == 0x5C));
}

#[test]
fn full_returns_the_unchanged_write_slot_and_retry_persists_its_payload() {
    let (pool, file) = pool_with_limits("pool-write-full", 5, 1, 2);
    let mut first = pool.write_arena().alloc().expect("first staging slot");
    first.fill(0x18);
    let first_token = pool
        .submit_write(file, first, 0)
        .expect("the only submission slot admits the first write");

    let mut blocked = pool.write_arena().alloc().expect("second staging slot");
    blocked.fill(0x7C);
    assert!(
        pool.write_arena().alloc().is_none(),
        "write_slots(2) exposes exactly two staging capabilities"
    );
    let (error, returned) = pool
        .submit_write(file, blocked, GRANULE.into())
        .expect_err("the saturated queue returns the caller's staging slot");
    assert!(matches!(error, PoolSubmitError::Full));
    assert_eq!(returned.len(), GRANULE as usize);
    assert!(
        returned.iter().all(|&byte| byte == 0x7C),
        "Full returns the exact slot without replacing or clearing its payload"
    );
    assert!(matches!(
        pool.submit_fsync(file, SyncMode::Full),
        Err(PoolSubmitError::Full)
    ));

    let mut completions = PoolCompletionBatch::with_capacity(1);
    let first_report = pool.poll_report(&mut completions);
    assert_eq!(first_report.backend_completions(), 1);
    assert_eq!(first_report.reclaimed_frames(), 0);
    assert_eq!(completions.iter().count(), 1);
    assert_write_success(&completions, first_token, GRANULE);

    let retry = pool
        .submit_write(file, returned, GRANULE.into())
        .expect("the returned slot retries once capacity recovers");
    assert_ne!(
        retry, first_token,
        "capacity one reuses the product op slot with a fresh generation (ABA-safe)"
    );
    let retry_report = pool.poll_report(&mut completions);
    assert_eq!(retry_report.backend_completions(), 1);
    assert_eq!(retry_report.reclaimed_frames(), 0);
    assert_eq!(completions.iter().count(), 1);
    assert_write_success(&completions, retry, GRANULE);

    let mut stored = [0u8; GRANULE as usize];
    assert_eq!(
        pool.driver()
            .copy_stored_bytes(file, u64::from(GRANULE), &mut stored),
        GRANULE as usize
    );
    assert!(
        stored.iter().all(|&byte| byte == 0x7C),
        "retry persists every byte returned from the Full rejection"
    );
}
