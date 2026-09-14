use std::path::Path;

use dios::testing::{
    Injected, MockDriver, MockPoolTestingExt, PoolBuilderTestingExt, PoolTestingExt,
};
use dios::{DirectIo, FileId, Get, PageId, Pool, Readahead, ReaderCtx, ReadyResult};

const FRAMES: u32 = 64;
const POLLS_MAX: u32 = 128;

fn fixture(mode: Option<Readahead>, credits: u32) -> (Pool<MockDriver>, FileId) {
    fixture_with_readers(mode, credits, 1)
}

fn fixture_with_readers(
    mode: Option<Readahead>,
    credits: u32,
    readers: u32,
) -> (Pool<MockDriver>, FileId) {
    let mock = MockDriver::builder()
        .seed(91)
        .frames(FRAMES)
        .frame_bytes(4096)
        .queue_capacity(1024)
        .retry_bound(0)
        .build();
    let file = mock
        .open(Path::new("prefetch-pages"), DirectIo::Disabled)
        .expect("mock file");
    for page in 0..256 {
        mock.seed_page(&file, page, u8::try_from(page).expect("fill byte"));
    }
    let id = file.file_id();
    let mut builder = Pool::builder()
        .frame_count(FRAMES)
        .max_concurrent_readers(readers)
        .peak_guards_per_reader(2)
        .max_inflight_reads(16)
        .miss_headroom(48)
        .prefetch_headroom(credits);
    if let Some(mode) = mode {
        builder = builder.readahead(mode);
    }
    let pool = builder.build_on(mock).expect("bounded pool");
    pool.register_file(file);
    (pool, id)
}

fn consume(pool: &Pool<MockDriver>, reader: &ReaderCtx, page: PageId) {
    let expected = u8::try_from(page.granule_idx()).expect("seeded page");
    let mut pending = match pool.get(reader, page).expect("live file") {
        Get::Hit(guard) => {
            assert_eq!(guard[0], expected);
            assert_eq!(guard[4095], expected);
            return;
        }
        Get::Pending(token) => token,
        Get::Busy => panic!("fixture leaves demand capacity"),
    };
    for _ in 0..POLLS_MAX {
        pool.poll();
        match pool.ready(reader, pending) {
            ReadyResult::Ready(guard) => {
                assert_eq!(guard[0], expected);
                assert_eq!(guard[4095], expected);
                return;
            }
            ReadyResult::NotYet(token) => pending = token,
            ReadyResult::Err(error) => panic!("unexpected demand failure: {error}"),
        }
    }
    panic!("demand did not complete within the fixed bound");
}

#[test]
fn explicit_prefetch_is_queued_and_coalesces_with_demand() {
    let (pool, file) = fixture(Some(Readahead::Disabled), 8);
    let pages = [
        PageId::new(file, 0),
        PageId::new(file, 1),
        PageId::new(file, 0),
    ];
    let report = pool.prefetch(&pages);
    assert_eq!(
        (report.requested, report.admitted, report.pending),
        (3, 2, 1)
    );
    assert_eq!(pool.driver().read_attempts_in_order().len(), 2);
    let observer = pool.register_reader().expect("observer");
    assert!(
        pool.pin(&observer, pages[0]).is_none(),
        "submission does not publish bytes before poll"
    );
    drop(observer);
    assert_eq!(pool.prefetch_stats().occupied, 2);
    let reader = pool.register_reader().expect("reader");
    consume(&pool, &reader, pages[0]);
    consume(&pool, &reader, pages[1]);
    assert_eq!(pool.driver().read_attempts_in_order().len(), 2);
    assert_eq!(pool.prefetch_stats().demand_promoted, 2);
    assert_eq!(pool.prefetch_stats().occupied, 0);
    let report = pool.prefetch(&pages);
    assert_eq!((report.resident, report.admitted), (3, 0));
}

#[test]
fn speculative_credits_bound_windows_and_recover_after_abandonment() {
    let (pool, file) = fixture(Some(Readahead::Disabled), 4);
    let pages: [_; 12] =
        std::array::from_fn(|index| PageId::new(file, u32::try_from(index).expect("index")));
    let report = pool.prefetch(&pages);
    assert_eq!(
        (report.requested, report.admitted, report.deferred),
        (12, 4, 8)
    );
    for _ in 0..8 {
        pool.poll();
    }
    assert_eq!(pool.prefetch_stats().occupied, 4);
    let next: [_; 4] =
        std::array::from_fn(|index| PageId::new(file, 16 + u32::try_from(index).expect("index")));
    let mut admitted = 0;
    for _ in 0..POLLS_MAX {
        let report = pool.prefetch(&next);
        admitted += report.admitted;
        assert!(pool.prefetch_stats().occupied <= 4);
        pool.poll();
        if admitted == 4 {
            break;
        }
    }
    assert_eq!(
        admitted, 4,
        "abandoned speculation cannot hold credits forever"
    );
    assert_eq!(pool.prefetch_stats().evicted_unused, 4);
    let reader = pool.register_reader().expect("reader");
    for page in next {
        consume(&pool, &reader, page);
    }
    assert_eq!(pool.prefetch_stats().occupied, 0);
}

#[test]
fn a_failed_speculative_read_returns_its_credit_and_can_be_retried() {
    let (pool, file) = fixture(Some(Readahead::Disabled), 4);
    pool.driver().inject_next(Injected::Io(5));
    assert_eq!(pool.prefetch(&[PageId::new(file, 7)]).admitted, 1);
    for _ in 0..POLLS_MAX {
        pool.poll();
        if pool.prefetch_stats().failed == 1 {
            break;
        }
    }
    assert_eq!(pool.prefetch_stats().failed, 1);
    assert_eq!(pool.prefetch_stats().occupied, 0);
    let reader = pool.register_reader().expect("reader");
    consume(&pool, &reader, PageId::new(file, 7));
    assert_eq!(pool.driver().read_attempts_in_order().len(), 2);
}

#[test]
fn automatic_readahead_is_default_and_can_be_disabled() {
    for mode in [None, Some(Readahead::Disabled)] {
        let (pool, file) = fixture(mode, 8);
        let reader = pool.register_reader().expect("reader");
        for page in 0..3 {
            consume(&pool, &reader, PageId::new(file, page));
        }
        for _ in 0..8 {
            pool.poll();
        }
        if mode.is_none() {
            assert!(pool.prefetch_stats().automatic_admitted > 0);
        } else {
            assert_eq!(pool.prefetch_stats().automatic_admitted, 0);
            assert_eq!(pool.driver().read_attempts_in_order().len(), 3);
        }
        assert!(pool.prefetch_stats().occupied <= 8);
    }
}

#[test]
fn non_sequential_demand_does_not_train_a_sequential_stream() {
    let (pool, file) = fixture(None, 8);
    let reader = pool.register_reader().expect("reader");
    for page in [3, 41, 7, 99, 17, 65, 9, 31] {
        consume(&pool, &reader, PageId::new(file, page));
    }
    assert_eq!(pool.prefetch_stats().automatic_admitted, 0);
    assert_eq!(pool.driver().read_attempts_in_order().len(), 8);
}

#[test]
fn another_reader_promotes_a_credit_without_advancing_the_originating_stream() {
    let (pool, file) = fixture_with_readers(None, 8, 2);
    let first = pool.register_reader().expect("first reader");
    let second = pool.register_reader().expect("second reader");
    for page in 0..3 {
        consume(&pool, &first, PageId::new(file, page));
    }
    for _ in 0..8 {
        pool.poll();
    }
    assert_eq!(pool.prefetch_stats().automatic_admitted, 4);
    consume(&pool, &second, PageId::new(file, 3));
    for _ in 0..8 {
        pool.poll();
    }
    let stats = pool.prefetch_stats();
    assert_eq!(stats.demand_promoted, 1);
    assert_eq!(stats.automatic_admitted, 4);
}

#[test]
fn skipping_ahead_in_a_resident_window_resets_automatic_training() {
    let (pool, file) = fixture(None, 8);
    let reader = pool.register_reader().expect("reader");
    for page in 0..3 {
        consume(&pool, &reader, PageId::new(file, page));
    }
    for _ in 0..8 {
        pool.poll();
    }
    assert_eq!(pool.prefetch_stats().automatic_admitted, 4);
    consume(&pool, &reader, PageId::new(file, 6));
    for _ in 0..8 {
        pool.poll();
    }
    let stats = pool.prefetch_stats();
    assert_eq!(stats.automatic_admitted, 4);
    assert_eq!(stats.occupied, 0);
    assert_eq!(stats.evicted_unused, 3);
}

#[test]
fn an_abandoned_automatic_window_returns_credits_without_demand_pressure() {
    let (pool, file) = fixture(None, 4);
    let reader = pool.register_reader().expect("reader");
    for page in 0..3 {
        consume(&pool, &reader, PageId::new(file, page));
    }
    for _ in 0..8 {
        pool.poll();
    }
    assert_eq!(pool.prefetch_stats().automatic_admitted, 4);
    for page in 100..104 {
        consume(&pool, &reader, PageId::new(file, page));
    }
    for _ in 0..POLLS_MAX {
        pool.poll();
    }
    let stats = pool.prefetch_stats();
    assert!(stats.automatic_admitted > 4);
    assert_eq!(stats.evicted_unused, 4);
    assert!(stats.occupied <= 4);
}

#[test]
fn sequential_feedback_survives_batched_consumption_and_recycled_metadata_slots() {
    let (pool, file) = fixture(None, 8);
    let reader = pool.register_reader().expect("reader");
    for page in 0..4 {
        consume(&pool, &reader, PageId::new(file, page));
        for _ in 0..8 {
            pool.poll();
        }
    }
    for start in (4..188).step_by(8) {
        for page in start..start + 8 {
            consume(&pool, &reader, PageId::new(file, page));
        }
        for _ in 0..8 {
            pool.poll();
        }
    }
    let stats = pool.prefetch_stats();
    assert!(stats.automatic_admitted > 180);
    assert_eq!(stats.evicted_unused, 0);
    let mut reads = pool.driver().read_attempts_in_order();
    reads.sort_by_key(|attempt| attempt.file_offset);
    assert!(reads.windows(2).all(|pair| pair[0] != pair[1]));
}

#[test]
fn obsolete_automatic_reads_complete_before_their_credits_are_recovered() {
    let (pool, file) = fixture(None, 4);
    let reader = pool.register_reader().expect("reader");
    for page in 0..3 {
        consume(&pool, &reader, PageId::new(file, page));
    }
    assert_eq!(pool.prefetch_stats().reads_in_flight, 4);
    let Get::Pending(token) = pool
        .get(&reader, PageId::new(file, 100))
        .expect("live file")
    else {
        panic!("new cold demand");
    };
    assert_eq!(pool.prefetch_stats().occupied, 4);
    drop(token);
    for _ in 0..POLLS_MAX {
        pool.poll();
    }
    let stats = pool.prefetch_stats();
    assert_eq!(stats.reads_in_flight, 0);
    assert_eq!(stats.evicted_unused, 4);
    assert_eq!(stats.occupied, 0);
    assert_eq!(pool.driver().read_attempts_in_order().len(), 8);
}

#[test]
fn speculative_short_reads_complete_through_the_existing_reslice_path() {
    let (pool, file) = fixture(Some(Readahead::Disabled), 4);
    pool.driver().inject_next(Injected::Short(2048));
    let page = PageId::new(file, 9);
    assert_eq!(pool.prefetch(&[page]).admitted, 1);
    let reader = pool.register_reader().expect("reader");
    consume(&pool, &reader, page);
    let attempts = pool.driver().read_attempts_in_order();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[1].destination_offset, 2048);
    assert_eq!(attempts[1].requested_len, 2048);
    assert_eq!(pool.prefetch_stats().occupied, 0);
}

#[test]
fn resident_hints_do_not_displace_existing_speculation() {
    let (pool, file) = fixture(Some(Readahead::Disabled), 4);
    let reader = pool.register_reader().expect("reader");
    consume(&pool, &reader, PageId::new(file, 7));
    let pages = std::array::from_fn::<_, 4, _>(|index| {
        PageId::new(file, 100 + u32::try_from(index).expect("index"))
    });
    assert_eq!(pool.prefetch(&pages).admitted, 4);
    for _ in 0..8 {
        pool.poll();
    }
    let before = pool.prefetch_stats();
    assert_eq!(pool.prefetch(&[PageId::new(file, 7)]).resident, 1);
    assert_eq!(
        pool.prefetch_stats(),
        before,
        "a resident hint needs no replacement credit"
    );
}

#[test]
fn retirement_drains_speculation_and_rejects_further_hints() {
    let (pool, file) = fixture(Some(Readahead::Disabled), 4);
    let pages = std::array::from_fn::<_, 4, _>(|index| {
        PageId::new(file, u32::try_from(index).expect("index"))
    });
    assert_eq!(pool.prefetch(&pages).admitted, 4);
    assert_eq!(pool.retire_file(file), dios::RetireStatus::Retiring);
    for _ in 0..POLLS_MAX {
        pool.poll();
        if pool.retire_file(file) == dios::RetireStatus::Retired {
            break;
        }
    }
    assert_eq!(pool.retire_file(file), dios::RetireStatus::Retired);
    assert_eq!(pool.prefetch_stats().occupied, 0);
    assert_eq!(pool.prefetch_stats().evicted_unused, 4);
    let before = pool.prefetch_stats();
    assert_eq!(pool.prefetch(&pages).rejected, 4);
    assert_eq!(pool.prefetch_stats(), before);
}

#[test]
fn reader_registration_and_file_changes_reset_training() {
    let (pool, file) = fixture(None, 8);
    let reader = pool.register_reader().expect("reader");
    for page in 0..2 {
        consume(&pool, &reader, PageId::new(file, page));
    }
    drop(reader);
    let reader = pool.register_reader().expect("replacement reader");
    consume(&pool, &reader, PageId::new(file, 2));
    assert_eq!(pool.prefetch_stats().automatic_admitted, 0);
    consume(&pool, &reader, PageId::new(file, 3));
    let other = pool
        .open(Path::new("another-prefetch-file"), DirectIo::Disabled)
        .expect("another mock file");
    consume(&pool, &reader, PageId::new(other, 0));
    consume(&pool, &reader, PageId::new(file, 4));
    assert_eq!(pool.prefetch_stats().automatic_admitted, 0);
}

#[test]
fn a_full_demand_queue_defers_speculation_without_consuming_credits() {
    let (pool, file) = fixture(Some(Readahead::Disabled), 4);
    let reader = pool.register_reader().expect("reader");
    let mut pending = Vec::with_capacity(16);
    for page in 0..16 {
        let Get::Pending(token) = pool
            .get(&reader, PageId::new(file, page))
            .expect("live file")
        else {
            panic!("the demand read fits its configured queue");
        };
        pending.push(token);
    }
    let page = PageId::new(file, 99);
    assert_eq!(pool.prefetch(&[page]).deferred, 1);
    assert_eq!(pool.prefetch_stats().occupied, 0);
    drop(pending);
    for _ in 0..8 {
        pool.poll();
    }
    assert_eq!(pool.prefetch(&[page]).admitted, 1);
    consume(&pool, &reader, page);
    assert_eq!(pool.prefetch_stats().occupied, 0);
}

#[test]
fn zero_credits_disable_explicit_and_automatic_admission() {
    let (pool, file) = fixture(None, 0);
    let reader = pool.register_reader().expect("reader");
    for page in 0..4 {
        consume(&pool, &reader, PageId::new(file, page));
    }
    assert_eq!(pool.prefetch(&[PageId::new(file, 10)]).deferred, 1);
    for _ in 0..8 {
        pool.poll();
    }
    assert_eq!(pool.prefetch_stats().admitted, 0);
    assert_eq!(pool.driver().read_attempts_in_order().len(), 4);
}

#[test]
fn a_joined_speculative_failure_reaches_demand_and_releases_resources_once() {
    let (pool, file) = fixture(Some(Readahead::Disabled), 4);
    pool.driver().inject_next(Injected::Io(5));
    let page = PageId::new(file, 11);
    assert_eq!(pool.prefetch(&[page]).admitted, 1);
    let reader = pool.register_reader().expect("reader");
    let Get::Pending(mut token) = pool.get(&reader, page).expect("live file") else {
        panic!("pending read");
    };
    for _ in 0..POLLS_MAX {
        pool.poll();
        match pool.ready(&reader, token) {
            ReadyResult::Err(error) => {
                assert_eq!(error.raw_os_error(), Some(5));
                assert_eq!(pool.prefetch_stats().demand_promoted, 1);
                assert_eq!(pool.prefetch_stats().failed, 0);
                assert_eq!(pool.prefetch_stats().occupied, 0);
                return;
            }
            ReadyResult::NotYet(next) => token = next,
            ReadyResult::Ready(_) => panic!("the injected read must fail"),
        }
    }
    panic!("the failed read must complete within the fixed poll bound");
}

#[test]
fn extending_a_partially_consumed_window_preserves_useful_lookahead_when_recycling() {
    let (pool, file) = fixture(Some(Readahead::Disabled), 8);
    let reader = pool.register_reader().expect("reader");
    for position in 0..192 {
        let pages = std::array::from_fn::<_, 8, _>(|index| {
            PageId::new(file, position + u32::try_from(index).expect("index"))
        });
        let mut covered = false;
        for _ in 0..POLLS_MAX {
            let report = pool.prefetch(&pages);
            pool.poll();
            if report.deferred == 0 {
                covered = true;
                break;
            }
        }
        assert!(covered, "recycling must replenish the reserve");
        consume(&pool, &reader, pages[0]);
        pool.poll();
    }
    assert_eq!(
        pool.prefetch_stats().evicted_unused,
        0,
        "reserve top-up must preserve the useful window"
    );
    let attempts = pool.driver().read_attempts_in_order();
    assert_eq!(
        attempts.len(),
        199,
        "each unique hinted page is read exactly once"
    );
}

#[test]
fn abandoned_wrong_hints_preserve_a_hot_set_that_leaves_the_credit_budget_free() {
    let (pool, file) = fixture(Some(Readahead::Disabled), 4);
    let reader = pool.register_reader().expect("reader");
    for page in 0..60 {
        consume(&pool, &reader, PageId::new(file, page));
    }
    for start in (100..200).step_by(4) {
        let pages = std::array::from_fn::<_, 4, _>(|index| {
            PageId::new(file, start + u32::try_from(index).expect("index"))
        });
        for _ in 0..POLLS_MAX {
            let report = pool.prefetch(&pages);
            pool.poll();
            if report.deferred == 0 {
                break;
            }
        }
        for page in 0..60 {
            let Get::Hit(guard) = pool
                .get(&reader, PageId::new(file, page))
                .expect("live file")
            else {
                panic!("wrong speculation evicted a declared demand-hot page");
            };
            assert_eq!(guard[0], u8::try_from(page).expect("fill byte"));
        }
    }
    assert_eq!(pool.prefetch_stats().occupied, 4);
    assert_eq!(pool.prefetch_stats().evicted_unused, 96);
}
