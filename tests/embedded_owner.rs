//! The pool's reader and miss capabilities must fit inside an embedding owner
//! that also owns the pool. Frame guards remain borrowed; only state retained
//! across owner-loop turns is lifetime-free.

#![cfg(feature = "mock")]

use std::path::Path;
use std::sync::Arc;
use std::thread;

use dios::testing::{
    MockDriver, MockPoolObservation, MockPoolTestingExt, PoolBuilderTestingExt, PoolTestingExt,
};
use dios::{DirectIo, Get, PageId, PendingToken, Pool, ReaderCtx, ReadyResult};

const GRANULE: u32 = 4096;

struct EmbeddedOwner {
    pool: Pool<MockDriver>,
    reader: ReaderCtx,
    pending: Option<PendingToken>,
}

fn embedded_owner() -> (EmbeddedOwner, Arc<MockPoolObservation>) {
    let mock = MockDriver::builder()
        .queue_capacity(4)
        .frames(4)
        .frame_bytes(GRANULE)
        .build();
    let file = mock
        .open(Path::new("embedded-owner"), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    mock.seed_page(&file, 0, 0xA5);

    let pool = Pool::builder()
        .frame_count(4)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .build_on(mock)
        .expect("valid embedded pool");
    let observation = pool.observe();
    pool.register_file(file);
    let reader = pool.register_reader().expect("one reader slot");
    let Get::Pending(pending) = pool
        .get(&reader, PageId::new(file_id, 0))
        .expect("the registered file is live")
    else {
        panic!("the registered cold page must admit a miss");
    };

    (
        EmbeddedOwner {
            pool,
            reader,
            pending: Some(pending),
        },
        observation,
    )
}

#[test]
fn an_embedding_owner_can_own_pool_reader_and_pending_token() {
    let (mut owner, _observation) = embedded_owner();
    let token = owner.pending.take().expect("the cold read is retained");

    owner.pool.poll();
    match owner.pool.ready(&owner.reader, token) {
        ReadyResult::Ready(frame) => {
            assert!(frame.iter().all(|&byte| byte == 0xA5));
        }
        ReadyResult::NotYet(_) => panic!("the deterministic read completed in one poll"),
        ReadyResult::Err(error) => panic!("the seeded read cannot fail: {error}"),
    }
}

#[test]
fn a_pending_token_moves_to_another_thread_and_readies_with_a_fresh_reader() {
    let mock = MockDriver::builder()
        .queue_capacity(4)
        .frames(4)
        .frame_bytes(GRANULE)
        .build();
    let file = mock
        .open(
            Path::new("pending-token-thread-handoff"),
            DirectIo::Disabled,
        )
        .expect("mock open");
    let file_id = file.file_id();
    mock.seed_page(&file, 0, 0xB6);
    let pool = Arc::new(
        Pool::builder()
            .frame_count(4)
            .granule(GRANULE)
            .max_concurrent_readers(1)
            .peak_guards_per_reader(1)
            .max_inflight_reads(1)
            .miss_headroom(3)
            .build_on(mock)
            .expect("valid handoff pool"),
    );
    pool.register_file(file);

    let origin_reader = pool.register_reader().expect("origin reader slot");
    let Get::Pending(token) = pool
        .get(&origin_reader, PageId::new(file_id, 0))
        .expect("the registered file is live")
    else {
        panic!("the seeded page starts cold");
    };
    drop(origin_reader);

    let destination_pool = Arc::clone(&pool);
    thread::spawn(move || {
        let destination_reader = destination_pool
            .register_reader()
            .expect("the origin reader released its slot");
        let mut token = token;
        for _ in 0..8 {
            destination_pool.poll();
            match destination_pool.ready(&destination_reader, token) {
                ReadyResult::Ready(frame) => {
                    assert!(frame.iter().all(|&byte| byte == 0xB6));
                    return;
                }
                ReadyResult::NotYet(handed_back) => token = handed_back,
                ReadyResult::Err(error) => panic!("the seeded handoff read failed: {error}"),
            }
        }
        panic!("the handed-off token did not ready within the bounded poll budget");
    })
    .join()
    .expect("the destination thread completes without moving its reader or guard");
}

#[test]
fn dropping_an_owner_releases_live_capabilities_and_quiesces_once() {
    let (owner, observation) = embedded_owner();
    let EmbeddedOwner {
        pool,
        reader,
        pending,
    } = owner;
    let pending = pending.expect("the owner retained its cold read");
    assert_eq!(observation.registered_readers(), 1);
    assert_eq!(observation.live_pending_interests(), 1);
    assert_eq!(observation.backend_ops_in_flight(), 1);

    drop(pool);

    assert_eq!(observation.registered_readers(), 1);
    assert_eq!(observation.reader_releases(), 0);
    assert_eq!(observation.live_pending_interests(), 1);
    assert_eq!(observation.pending_releases(), 0);
    assert_eq!(observation.backend_ops_in_flight(), 0);
    assert_eq!(observation.backend_completions(), 1);
    assert_eq!(observation.quiesce_calls(), 1);

    drop(reader);
    assert_eq!(observation.registered_readers(), 0);
    assert_eq!(observation.reader_releases(), 1);
    assert_eq!(observation.live_pending_interests(), 1);
    assert_eq!(observation.pending_releases(), 0);

    drop(pending);
    assert_eq!(observation.live_pending_interests(), 0);
    assert_eq!(observation.pending_releases(), 1);
    assert_eq!(observation.backend_ops_in_flight(), 0);
    assert_eq!(observation.backend_completions(), 1);
    assert_eq!(observation.quiesce_calls(), 1);
}
