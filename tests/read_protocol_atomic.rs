#![cfg(feature = "mock")]

use std::path::Path;

use dios::testing::{MockDriver, PoolBuilderTestingExt, PoolTestingExt};
use dios::{DirectIo, Get, PageId, Pool};

const FRAME_COUNT: u32 = 4;
const GRANULE: u32 = 4096;

fn pool_with_warm_page() -> (Pool<MockDriver>, dios::ReaderCtx, PageId) {
    let driver = MockDriver::builder()
        .queue_capacity(1)
        .frames(FRAME_COUNT)
        .frame_bytes(GRANULE)
        .build();
    let file = driver
        .open(Path::new("read-protocol-atomic-warm"), DirectIo::Disabled)
        .expect("mock open");
    let page = PageId::new(file.file_id(), 0);
    let pool = Pool::builder()
        .frame_count(FRAME_COUNT)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .build_on(driver)
        .expect("valid fixed pool");
    pool.register_file(file);
    let reader = pool.register_reader().expect("one reader slot");
    pool.insert_resident_frame(page, 0xA5);
    (pool, reader, page)
}

#[test]
fn a_warm_get_does_not_enter_the_control_plane() {
    let (pool, reader, page) = pool_with_warm_page();
    let before = pool.control_acquisitions();

    let outcome = pool.get(&reader, page).expect("the exact file is live");
    let Get::Hit(guard) = outcome else {
        panic!("the installed resident page must be a warm hit");
    };

    assert!(guard.iter().all(|&byte| byte == 0xA5));
    assert_eq!(
        pool.control_acquisitions(),
        before,
        "a generation-exact live mirror admits a warm hit without AD-4"
    );
}
