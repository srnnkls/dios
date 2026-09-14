//! RC3 exercises the real pool span transaction before RC4 run classification.
//!
//! Required narrow seam: `PoolTestingExt::prefetch_span(&[PageId]) ->
//! PrefetchReport` admits one already classified contiguous absent run, using
//! the shared transaction that explicit and automatic admission will call.
//! It retains ordinary speculative accounting, completion routing and refusal.

#![cfg(feature = "mock")]

use std::path::Path;

use dios::driver::{CompletionBatch, FileHandle, OpKind, SubmitError};
use dios::testing::{
    FrameState, Injected, MockDriver, MockPoolTestingExt, MockRingDriver,
    MockRingPoolBuilderTestingExt, MockRingPoolTestingExt, PoolBuilderTestingExt, PoolTestingExt,
    ReadFrameIdx,
};
use dios::{
    DirectIo, FileId, Get, GetError, PageId, PendingToken, Pool, Readahead, ReaderCtx, ReadyResult,
    RetireStatus, SyncMode,
};

const FRAMES: u32 = 24;
const POLLS_MAX: u32 = 64;

fn fixture() -> (Pool<MockDriver>, FileHandle) {
    let driver = MockDriver::builder()
        .seed(91)
        .frames(FRAMES)
        .frame_bytes(4096)
        .queue_capacity(6)
        .build();
    let file = driver
        .open(Path::new("span-pages"), DirectIo::Disabled)
        .expect("mock file opens");
    for (page, fill) in [(7, 0xA1), (8, 0xB2), (9, 0xC3)] {
        driver.seed_page(&file, page, fill);
    }
    let borrowed = driver.duplicate_handle(&file);
    let pool = Pool::builder()
        .frame_count(FRAMES)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(3)
        .max_inflight_reads(4)
        .max_inflight_product_ops(2)
        .miss_headroom(12)
        .prefetch_headroom(4)
        .readahead(Readahead::Disabled)
        .build_on(driver)
        .expect("bounded span fixture");
    pool.register_file(file);
    (pool, borrowed)
}

fn pages(file: FileId, start: u32) -> [PageId; 3] {
    [start, start + 1, start + 2].map(|page| PageId::new(file, page))
}

fn pending(outcome: Get<'_>) -> PendingToken {
    match outcome {
        Get::Pending(token) => token,
        Get::Hit(_) => panic!("an unpublished page must remain pending"),
        Get::Busy => panic!("joining an admitted span needs no new read credit"),
    }
}

fn poll_until_reads(pool: &Pool<MockDriver>, expected: u32) {
    for _ in 0..POLLS_MAX {
        pool.poll();
        if pool.prefetch_stats().reads_in_flight == expected {
            return;
        }
    }
    panic!("span did not reach {expected} unfinished pages within the poll bound");
}

fn assert_bytes(pool: &Pool<MockDriver>, reader: &ReaderCtx, page: PageId, fill: u8) {
    let Get::Hit(guard) = pool.get(reader, page).expect("live page") else {
        panic!("a complete page must be published");
    };
    assert_eq!(&*guard, &[fill; 4096]);
}

fn count_frames(pool: &impl PoolTestingExt, state: FrameState) -> u32 {
    (0..FRAMES)
        .filter(|&frame| pool.frame_state(ReadFrameIdx::new(frame)) == state)
        .count()
        .try_into()
        .expect("fixed frame count fits u32")
}

#[test]
fn mid_iovec_short_publishes_only_whole_pages_and_preserves_exact_suffix_bytes() {
    let (pool, file) = fixture();
    let run = pages(file.file_id(), 7);
    let reader = pool.register_reader().expect("one reader");
    pool.driver().inject_next(Injected::Short(4609));
    assert_eq!(pool.prefetch_span(&run).admitted, 3);
    assert_eq!(pool.prefetch_stats().reads_in_flight, 3);
    assert_eq!(pool.prefetch_stats().occupied, 3);
    assert_eq!(count_frames(&pool, FrameState::InFlight), 3);
    assert!(pool.pin(&reader, run[0]).is_none());

    poll_until_reads(&pool, 2);
    assert_bytes(&pool, &reader, run[0], 0xA1);
    assert!(pool.pin(&reader, run[1]).is_none());
    assert!(pool.pin(&reader, run[2]).is_none());
    assert_eq!(count_frames(&pool, FrameState::InFlight), 2);
    assert_eq!(pool.prefetch_stats().occupied, 2);
    let middle = pending(pool.get(&reader, run[1]).expect("join middle page"));
    let tail = pending(pool.get(&reader, run[2]).expect("join last page"));
    drop(tail);
    assert_eq!(pool.prefetch_stats().reads_in_flight, 2);
    assert_eq!(pool.prefetch_stats().occupied, 0);

    // Changing the backing page makes rereading its already copied prefix visible.
    pool.driver().seed_page(&file, 7, 0x11);
    pool.driver().seed_page(&file, 8, 0x22);
    poll_until_reads(&pool, 0);
    let ReadyResult::Ready(guard) = pool.ready(&reader, middle) else {
        panic!("the remaining middle-page bytes must complete");
    };
    assert_eq!(&guard[..513], &[0xB2; 513]);
    assert_eq!(&guard[513..], &[0x22; 3583]);
    drop(guard);
    assert_bytes(&pool, &reader, run[0], 0xA1);
    assert_bytes(&pool, &reader, run[2], 0xC3);
    assert_eq!(count_frames(&pool, FrameState::Resident), 3);
    assert_eq!(count_frames(&pool, FrameState::Free), FRAMES - 3);
}

#[test]
fn eof_and_permanent_tail_error_preserve_prefix_and_return_each_page_credit() {
    for terminal in [Injected::Short(0), Injected::Io(5)] {
        let (pool, file) = fixture();
        let run = pages(file.file_id(), 7);
        let reader = pool.register_reader().expect("one reader");
        pool.driver().inject_next(Injected::Short(4609));
        pool.driver().inject_next(terminal);
        assert_eq!(pool.prefetch_span(&run).admitted, 3);
        let tails = [run[1], run[2]]
            .map(|page| pending(pool.get(&reader, page).expect("join unpublished page")));
        poll_until_reads(&pool, 2);
        assert_bytes(&pool, &reader, run[0], 0xA1);
        poll_until_reads(&pool, 0);
        for token in tails {
            let ReadyResult::Err(error) = pool.ready(&reader, token) else {
                panic!("every unpublished destination receives the terminal tail failure");
            };
            if terminal == Injected::Io(5) {
                assert_eq!(error.raw_os_error(), Some(5));
            }
        }
        assert_bytes(&pool, &reader, run[0], 0xA1);
        assert!(pool.pin(&reader, run[1]).is_none());
        assert!(pool.pin(&reader, run[2]).is_none());
        assert_eq!(pool.prefetch_stats().occupied, 0);
        assert_eq!(count_frames(&pool, FrameState::Resident), 1);
        assert_eq!(count_frames(&pool, FrameState::Free), FRAMES - 1);

        // A fresh route must have all three admission credits after the failed tail.
        let replacement = pages(file.file_id(), 12);
        assert_eq!(pool.prefetch_span(&replacement).admitted, 3);
        assert_eq!(pool.prefetch_stats().reads_in_flight, 3);
        poll_until_reads(&pool, 0);
        assert_eq!(pool.prefetch_stats().occupied, 3);
        for page in replacement {
            assert!(pool.pin(&reader, page).is_some());
        }
        assert_eq!(pool.prefetch_stats().occupied, 0);
    }
}

fn ring_fixture() -> (Pool<MockRingDriver>, FileId, FileHandle) {
    let driver = MockRingDriver::builder()
        .seed(91)
        .frames(FRAMES)
        .frame_bytes(4096)
        .queue_capacity(6)
        .retry_bound(32)
        .build();
    let file = driver
        .open(Path::new("span-retirement"), DirectIo::Disabled)
        .expect("target file opens");
    let file_id = file.file_id();
    for (page, fill) in [(7, 0xA1), (8, 0xB2), (9, 0xC3)] {
        driver.seed_page(&file, page, fill);
    }
    let blocker = driver
        .open(Path::new("unrelated-blockers"), DirectIo::Disabled)
        .expect("blocker file opens");
    let pool = Pool::builder()
        .frame_count(FRAMES)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(3)
        .max_inflight_reads(4)
        .max_inflight_product_ops(2)
        .miss_headroom(12)
        .prefetch_headroom(4)
        .readahead(Readahead::Disabled)
        .build_on_ring(driver)
        .expect("six backend slots include two product reservations");
    pool.register_file(file);
    (pool, file_id, blocker)
}

fn drain_blockers(pool: &Pool<MockRingDriver>, blocker: FileHandle) {
    pool.ring_driver().close(blocker);
    let mut batch = CompletionBatch::with_capacity(6);
    let mut completed = 0;
    for _ in 0..POLLS_MAX {
        pool.ring_driver().poll(&mut batch);
        for completion in &batch {
            assert_eq!(completion.kind(), OpKind::Fsync);
            assert_eq!(completion.result().expect("bounded retries succeed"), 0);
            completed += 1;
        }
        if completed == 5 {
            return;
        }
        assert!(completed < 5, "every unrelated blocker completes once");
    }
    panic!("five unrelated blockers did not drain within their fixed retry bound");
}

#[test]
fn original_slot_survives_full_backend_slab_and_retirement_across_later_shorts() {
    let (pool, file, blocker) = ring_fixture();
    // Raw bounded-retry barriers occupy the five slots unrelated to the pool span.
    // No barrier becomes terminal until pool-owned completions have been consumed.
    for _ in 0..5 {
        pool.ring_driver()
            .inject_for_next_submit(&[Injected::Eintr; 32]);
        pool.ring_driver()
            .submit_fsync(&blocker, SyncMode::Full)
            .expect("one of five unrelated operation slots");
    }
    pool.ring_driver()
        .inject_for_next_submit(&[Injected::Short(4609), Injected::Short(4096)]);
    let run = pages(file, 7);
    assert_eq!(pool.prefetch_span(&run).admitted, 3);
    assert!(matches!(
        pool.ring_driver().submit_fsync(&blocker, SyncMode::Full),
        Err(SubmitError::Full)
    ));
    let reader = pool.register_reader().expect("one reader");
    let mut tokens = run.map(|page| Some(pending(pool.get(&reader, page).expect("join run"))));
    assert_eq!(pool.retire_file(file), RetireStatus::Retiring);
    assert!(matches!(
        pool.get(&reader, PageId::new(file, 10)),
        Err(GetError::StaleFile { .. })
    ));
    for expected_remaining in [2, 1, 0] {
        pool.poll();
        assert_eq!(pool.prefetch_stats().reads_in_flight, expected_remaining);
        assert!(!pool.ring_driver().is_closed(file));
        let ordinal = usize::try_from(2 - expected_remaining).expect("three pages");
        let token = tokens[ordinal]
            .take()
            .expect("one terminal interest per page");
        let ReadyResult::Ready(guard) = pool.ready(&reader, token) else {
            panic!("retirement preserves the already admitted continuation");
        };
        assert_eq!(
            &*guard,
            &[[0xA1; 4096], [0xB2; 4096], [0xC3; 4096]][ordinal]
        );
        if expected_remaining > 0 {
            assert!(matches!(
                pool.ring_driver().submit_fsync(&blocker, SyncMode::Full),
                Err(SubmitError::Full)
            ));
        }
    }
    assert!(tokens.iter().all(Option::is_none));
    drain_blockers(&pool, blocker);
    drop(reader);
    for _ in 0..POLLS_MAX {
        pool.poll();
        if pool.retire_file(file) == RetireStatus::Retired {
            assert_eq!(count_frames(&pool, FrameState::Free), FRAMES);
            return;
        }
    }
    panic!("finished span did not release its file and every destination");
}

#[cfg(feature = "bench")]
mod rc4 {
    use super::*;
    use dios::testing::{PoolBuilderObservationExt, ReadObservationConfig};
    use serde_json::Value;

    fn fixture(mode: Readahead, credits: Option<u32>) -> (Pool<MockDriver>, FileId) {
        let driver = MockDriver::builder()
            .seed(91)
            .frames(2048)
            .frame_bytes(4096)
            .queue_capacity(1024)
            .build();
        let file = driver
            .open(Path::new("classified-readahead"), DirectIo::Disabled)
            .expect("mock file opens");
        for page in 0..2304 {
            driver.seed_page(&file, page, u8::try_from(page % 251).expect("fill byte"));
        }
        let id = file.file_id();
        let mut builder = Pool::builder()
            .frame_count(2048)
            .max_concurrent_readers(1)
            .peak_guards_per_reader(1)
            .max_inflight_reads(256)
            .miss_headroom(768)
            .readahead(mode)
            .read_observation(ReadObservationConfig {
                event_capacity: 32768,
                interval_start_page: 0,
                interval_pages: 0,
                consumer_stop_bytes: Some(2176 * 4096),
            });
        if let Some(credits) = credits {
            builder = builder.prefetch_headroom(credits);
        }
        let pool = builder.build_on(driver).expect("scan geometry fits");
        pool.register_file(file);
        (pool, id)
    }

    fn consume(pool: &Pool<MockDriver>, reader: &ReaderCtx, page: PageId) {
        let expected = u8::try_from(page.granule_idx() % 251).expect("fill byte");
        let mut token = match pool.get(reader, page).expect("live file") {
            Get::Hit(guard) => {
                assert_eq!(&*guard, &[expected; 4096]);
                return;
            }
            Get::Pending(token) => token,
            Get::Busy => panic!("one demand leaves configured read capacity"),
        };
        for _ in 0..POLLS_MAX {
            pool.poll();
            match pool.ready(reader, token) {
                ReadyResult::Ready(guard) => {
                    assert_eq!(&*guard, &[expected; 4096]);
                    return;
                }
                ReadyResult::NotYet(next) => token = next,
                ReadyResult::Err(error) => panic!("seeded demand failed: {error}"),
            }
        }
        panic!("demand exceeded the fixed poll bound");
    }

    fn initial_reads(snapshot: &Value) -> Vec<(u64, u64)> {
        let mut reads: Vec<_> = snapshot["read_events"]
            .as_array()
            .expect("bounded read observations")
            .iter()
            .filter(|event| event["event"] == "attempt" && event["kind"] != "continuation")
            .map(|event| {
                (
                    event["offset"].as_u64().expect("file offset"),
                    event["bytes"].as_u64().expect("requested bytes"),
                )
            })
            .collect();
        reads.sort_unstable();
        reads
    }

    #[test]
    fn explicit_runs_split_at_resident_and_pending_holes_and_deduplicate() {
        let (pool, file) = fixture(Readahead::Disabled, Some(8));
        let reader = pool.register_reader().expect("reader");
        consume(&pool, &reader, PageId::new(file, 3));
        assert_eq!(pool.prefetch(&[PageId::new(file, 5)]).admitted, 1);
        let requested = [1, 2, 3, 4, 5, 6, 7, 7].map(|page| PageId::new(file, page));
        let report = pool.prefetch(&requested);
        assert_eq!(
            (
                report.requested,
                report.resident,
                report.pending,
                report.admitted,
                report.deferred,
                report.rejected
            ),
            (8, 1, 2, 5, 0, 0)
        );
        poll_until_reads(&pool, 0);
        let snapshot = pool.read_observation().expect("capture").snapshot();
        assert_eq!(
            initial_reads(&snapshot),
            vec![
                (4096, 8192),
                (12288, 4096),
                (16384, 4096),
                (20480, 4096),
                (24576, 8192)
            ],
            "only contiguous absent runs share a read; neither hole is reread"
        );
        for page in requested {
            consume(&pool, &reader, page);
        }
        assert_eq!(pool.prefetch_stats().occupied, 0);
        assert_eq!(snapshot["io"]["terminal_read_credits"], 0);
        assert_eq!(snapshot["overflow"], 0);
    }

    #[test]
    fn automatic_steady_refills_keep_full_vectors_with_the_actual_default_budget() {
        let (pool, file) = fixture(Readahead::Automatic, None);
        let reader = pool.register_reader().expect("reader");
        let capture = pool.read_observation().expect("capture");
        for page in 0..2176 {
            capture.scan_position(0, page);
            consume(&pool, &reader, PageId::new(file, page));
        }
        poll_until_reads(&pool, 0);
        let snapshot = capture.snapshot();
        let reads: Vec<_> = snapshot["read_events"]
            .as_array()
            .expect("bounded read observations")
            .iter()
            .filter(|event| event["event"] == "attempt" && event["kind"] != "continuation")
            .map(|event| {
                (
                    event["offset"].as_u64().expect("file offset"),
                    event["bytes"].as_u64().expect("requested bytes"),
                    event["kind"].as_str().expect("initial read purpose"),
                )
            })
            .filter(|&(offset, bytes, _)| offset < 2048 * 4096 && offset + bytes > 1024 * 4096)
            .collect();
        assert!(
            reads.len() <= 33,
            "a steady 1024-page interval needs at most 33 initial reads, got {}",
            reads.len()
        );
        for &(_, bytes, kind) in &reads {
            match kind {
                "speculative" => assert_eq!(bytes, 32 * 4096),
                "demand" => assert_eq!(bytes, 4096),
                other => panic!("unexpected initial read purpose: {other}"),
            }
        }
        for page in 1024..2048 {
            let offset = page * 4096;
            assert!(
                reads
                    .iter()
                    .any(|&(start, bytes, _)| start <= offset && offset < start + bytes)
            );
        }
        assert_eq!(pool.prefetch_stats().capacity, 128);
        assert!(
            snapshot["confirmed_window_page"]
                .as_u64()
                .is_some_and(|page| page < 1024)
        );
        assert_eq!(snapshot["io"]["terminal_destinations"], 0);
        assert_eq!(snapshot["overflow"], 0);
    }

    #[test]
    fn consumption_after_the_last_completion_recovers_credits_and_idle_visits_are_zero() {
        for capacity in [32, 128, 256] {
            let (pool, file) = fixture(Readahead::Disabled, Some(capacity));
            let reader = pool.register_reader().expect("reader");
            let run: [_; 32] = std::array::from_fn(|index| {
                PageId::new(file, u32::try_from(index).expect("bounded index"))
            });
            assert_eq!(pool.prefetch(&run).admitted, 32);
            poll_until_reads(&pool, 0);
            assert_eq!(pool.prefetch_stats().occupied, 32);
            let capture = pool.read_observation().expect("capture");
            let completions = capture.snapshot()["io"]["cqes"].clone();
            for page in run {
                consume(&pool, &reader, page);
            }
            pool.poll();
            pool.poll();
            let snapshot = capture.snapshot();
            assert_eq!(snapshot["io"]["cqes"], completions);
            let events = snapshot["control"].as_array().expect(
                "RC4 must observe affected-entry work, idle polls and consumption without a new CQE",
            );
            let idle: Vec<_> = events
                .iter()
                .filter(|event| event["cause"] == "idle")
                .collect();
            assert!(!idle.is_empty(), "the final empty poll is observed");
            assert!(idle.iter().all(|event| event["entry_visits"] == 0));
            assert!(events.iter().any(|event| {
                event["cause"] == "consumption"
                    && event["after_last_cqe"] == true
                    && event["credits_recovered"]
                        .as_u64()
                        .is_some_and(|credits| credits > 0)
            }));
            for event in events {
                let affected = event["affected_entries"].as_u64().expect("affected pages");
                let cleanup = event["cleanup_entries"].as_u64().expect("one-time cleanup");
                assert!(affected <= 32);
                assert!(
                    event["entry_visits"].as_u64().expect("actual visits") <= affected + cleanup
                );
            }
            assert_eq!(pool.prefetch_stats().occupied, 0);
            assert_eq!(snapshot["overflow"], 0);
        }
    }
}
