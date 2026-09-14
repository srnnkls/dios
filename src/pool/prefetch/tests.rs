use std::path::Path;

use super::{Admission, Source};
use crate::testing::{MockDriver, PoolBuilderTestingExt, PoolTestingExt};
use crate::{DirectIo, FileId, Get, PageId, Pool, ReadyResult};

fn fixture() -> (Pool<MockDriver>, FileId) {
    let mock = MockDriver::builder()
        .seed(91)
        .frames(64)
        .frame_bytes(4096)
        .queue_capacity(1024)
        .retry_bound(0)
        .build();
    let file = mock
        .open(Path::new("feedback-boundary"), DirectIo::Disabled)
        .expect("file");
    let id = file.file_id();
    let pool = Pool::builder()
        .frame_count(64)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(2)
        .max_inflight_reads(16)
        .miss_headroom(48)
        .prefetch_headroom(4)
        .build_on(mock)
        .expect("pool");
    pool.register_file(file);
    (pool, id)
}

fn train(pool: &Pool<MockDriver>, reader: &crate::ReaderCtx, file: FileId) {
    for index in 0..3 {
        let Get::Pending(mut token) = pool.get(reader, PageId::new(file, index)).expect("get")
        else {
            panic!("cold page");
        };
        let mut complete = false;
        for _ in 0..128 {
            pool.poll();
            match pool.ready(reader, token) {
                ReadyResult::Ready(guard) => {
                    drop(guard);
                    complete = true;
                    break;
                }
                ReadyResult::NotYet(next) => token = next,
                ReadyResult::Err(error) => panic!("read error: {error}"),
            }
        }
        assert!(complete);
    }
    for _ in 0..8 {
        pool.poll();
    }
    assert_eq!(pool.prefetch_stats().occupied, 4);
}

#[test]
fn replacement_admission_harvests_earlier_consumption_before_later_feedback() {
    let (pool, file) = fixture();
    let reader = pool.register_reader().expect("reader");
    train(&pool, &reader, file);
    let mut control = pool.control();
    control.prefetch.reconcile(&pool.clock);
    // Pause replacement for [100, 3] after reconciliation, when page 3 is
    // protected and page 4 can be selected. Warm consumers need no control lock.
    for index in 3..5 {
        let Get::Hit(guard) = pool
            .get(&reader, PageId::new(file, index))
            .expect("warm get")
        else {
            panic!("resident speculation");
        };
        drop(guard);
    }
    let victim = pool.table.lookup(PageId::new(file, 4)).expect("victim");
    pool.evict_resident(&mut control, victim);
    assert!(matches!(
        pool.prefetch_admit(&mut control, PageId::new(file, 100), Source::Explicit),
        Admission::Admitted
    ));
    control.prefetch.reconcile(&pool.clock);
    assert!(
        control.prefetch.patterns[0].request(0).is_some(),
        "consecutive consumption preserves the stream"
    );
}

#[test]
fn automatic_request_is_revalidated_after_queued_feedback_resets_its_stream() {
    let (pool, file) = fixture();
    let reader = pool.register_reader().expect("reader");
    train(&pool, &reader, file);
    assert!(matches!(
        pool.get(&reader, PageId::new(file, 3)),
        Ok(Get::Hit(_))
    ));
    let mut control = pool.control();
    control.prefetch.reconcile(&pool.clock);
    let (page, source) = control.prefetch.patterns[0]
        .request(0)
        .expect("next prediction");
    assert!(matches!(
        pool.get(&reader, PageId::new(file, 6)),
        Ok(Get::Hit(_))
    ));
    let victim = pool.table.lookup(PageId::new(file, 6)).expect("victim");
    pool.evict_resident(&mut control, victim);
    let before = control.prefetch.stats.admitted;
    let outcome = pool.prefetch_admit(&mut control, page, source);
    assert!(!matches!(outcome, Admission::Admitted));
    assert_eq!(control.prefetch.stats.admitted, before);
}
