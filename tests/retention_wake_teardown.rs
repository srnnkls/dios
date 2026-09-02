#![cfg(feature = "mock")]

use std::path::Path;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use dios::testing::{
    FrameState, MockDriver, MockIoEvent, MockPoolIoTestingExt, MockPoolTestingExt,
    MockWaitObservation, PoolBuilderTestingExt, PoolTestingExt, ReadFrameIdx,
};
use dios::{
    DirectIo, FileId, FrameGuard, Get, PageId, Pool, PoolCompletionBatch, ReaderCtx, RetainedFrame,
    RetireStatus, SyncMode,
};

const GRANULE: u32 = 4096;
const FRAME_COUNT: u32 = 6;
const HELD_POLLS: u32 = 4;
const OBSERVE_PARK: Duration = Duration::from_secs(1);
const PROMPT_WAKE: Duration = Duration::from_secs(2);
const WAIT_DEADLINE: Duration = Duration::from_secs(30);

type WaitReport = (u32, u32, usize);

fn mock_pool(name: &str) -> (Pool<MockDriver>, FileId) {
    let mock = MockDriver::builder()
        .queue_capacity(1)
        .frames(FRAME_COUNT)
        .frame_bytes(GRANULE)
        .build();
    let file = mock
        .open(Path::new(name), DirectIo::Disabled)
        .expect("mock file opens");
    let file_id = file.file_id();
    let pool = Pool::builder()
        .frame_count(FRAME_COUNT)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(2)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .max_retained_frames(1)
        .build_on(mock)
        .expect("retention wake fixture satisfies its watermark");
    pool.register_file(file);
    (pool, file_id)
}

fn resident_guard<'pool>(
    pool: &'pool Pool<MockDriver>,
    reader: &'pool ReaderCtx,
    page: PageId,
) -> FrameGuard<'pool> {
    match pool.get(reader, page).expect("registered page is live") {
        Get::Hit(guard) => guard,
        Get::Pending(_) => panic!("the fixture inserts its resident page"),
        Get::Busy => panic!("the fixture has spare frames"),
    }
}

fn retain_held<'pool>(
    pool: &'pool Pool<MockDriver>,
    reader: &'pool ReaderCtx,
    file: FileId,
) -> (ReadFrameIdx, RetainedFrame<'pool>) {
    let page = PageId::new(file, 0);
    let frame = pool.insert_resident_frame(page, 0xA5);
    let Ok(retained) = resident_guard(pool, reader, page).into_retained() else {
        panic!("the configured budget must admit one retained frame");
    };
    assert_eq!(pool.evict_frame(page), frame);
    let mut completions = PoolCompletionBatch::with_capacity(0);
    for _ in 0..HELD_POLLS {
        let report = pool.poll_report(&mut completions);
        assert_eq!(report.backend_completions(), 0);
        assert_eq!(report.reclaimed_frames(), 0);
    }
    assert_eq!(pool.frame_state(frame), FrameState::Evicting);
    (frame, retained)
}

fn spawn_mock_waiter(
    pool: Arc<Pool<MockDriver>>,
) -> (thread::JoinHandle<()>, mpsc::Receiver<WaitReport>) {
    let (report_tx, report_rx) = mpsc::sync_channel(1);
    let waiter = thread::spawn(move || {
        let mut completions = PoolCompletionBatch::with_capacity(0);
        let report = pool.poll_wait(&mut completions, WAIT_DEADLINE);
        report_tx
            .send((
                report.backend_completions(),
                report.reclaimed_frames(),
                completions.iter().count(),
            ))
            .expect("wait report receiver remains live");
    });
    (waiter, report_rx)
}

#[test]
fn held_release_published_before_mock_poll_wait_reclaims_without_parking() {
    let (pool, file) = mock_pool("retention-release-before-park");
    let observation: MockWaitObservation = pool.driver().observe_waits();
    let reader = pool.register_reader().expect("reader slot is available");
    let (frame, retained) = retain_held(&pool, &reader, file);
    drop(retained);
    let mut completions = PoolCompletionBatch::with_capacity(0);

    let report = pool.poll_wait(&mut completions, WAIT_DEADLINE);

    assert_eq!(report.backend_completions(), 0);
    assert_eq!(report.reclaimed_frames(), 1);
    assert_eq!(completions.iter().count(), 0);
    assert_eq!(pool.frame_state(frame), FrameState::Free);
    assert_eq!(observation.parks_entered(), 0);
    assert_eq!(observation.parks_in_progress(), 0);
    assert_eq!(observation.parks_exited(), 0);
    assert_eq!(observation.wake_exits(), 0);
    assert_eq!(observation.timeout_exits(), 0);
}

#[test]
fn held_release_wakes_parked_mock_poll_wait_and_reclaims() {
    let (pool, file) = mock_pool("retention-release-during-park");
    let observation: MockWaitObservation = pool.driver().observe_waits();
    let pool = Arc::new(pool);
    let reader = pool.register_reader().expect("reader slot is available");
    let (frame, retained) = retain_held(&pool, &reader, file);
    let cleanup_wake = pool.wake_handle();
    let (waiter, report_rx) = spawn_mock_waiter(Arc::clone(&pool));

    if !observation.wait_until_parked(OBSERVE_PARK) {
        drop(retained);
        cleanup_wake.wake();
        waiter.join().expect("cleanup joins the mock waiter");
        panic!("mock poll_wait never entered its blocking wait hook");
    }
    assert_eq!(observation.parks_entered(), 1);
    assert_eq!(observation.parks_in_progress(), 1);
    assert_eq!(observation.parks_exited(), 0);
    assert_eq!(observation.wake_exits(), 0);
    assert_eq!(observation.timeout_exits(), 0);
    drop(retained);

    let report = match report_rx.recv_timeout(PROMPT_WAKE) {
        Ok(report) => report,
        Err(error) => {
            cleanup_wake.wake();
            waiter.join().expect("cleanup joins the mock waiter");
            panic!("HELD final-drop did not wake mock poll_wait: {error}");
        }
    };
    waiter.join().expect("mock waiter joins after HELD release");
    assert_eq!(report, (0, 1, 0));
    assert_eq!(pool.frame_state(frame), FrameState::Free);
    assert_eq!(observation.parks_entered(), 1);
    assert_eq!(observation.parks_in_progress(), 0);
    assert_eq!(observation.parks_exited(), 1);
    assert_eq!(observation.wake_exits(), 1);
    assert_eq!(observation.timeout_exits(), 0);
}

#[test]
fn pool_teardown_drains_a_held_final_drop() {
    let outcome = std::panic::catch_unwind(|| {
        let (pool, file) = mock_pool("retention-release-teardown");
        let reader = pool.register_reader().expect("reader slot is available");
        let (frame, retained) = retain_held(&pool, &reader, file);
        drop(retained);
        assert_eq!(pool.frame_state(frame), FrameState::Evicting);
        drop(reader);
        drop(pool);
    });

    assert!(
        outcome.is_ok(),
        "pool teardown must consume a pending HELD release without panicking"
    );
}

#[test]
fn forgotten_retained_handle_panics_before_retiring_file_is_closed() {
    let (pool, file) = mock_pool("forgotten-retained-before-close");
    let io_observation = pool.observe_io();
    let reader = pool.register_reader().expect("reader slot is available");
    let (_frame, retained) = retain_held(&pool, &reader, file);
    assert_eq!(pool.retire_file(file), RetireStatus::Retiring);
    let _retained = std::mem::ManuallyDrop::new(retained);
    drop(reader);

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(pool)));

    assert!(
        outcome.is_err(),
        "forgetting a retained handle is a programmer error at pool teardown"
    );
    let events = io_observation.io_events_in_order();
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, MockIoEvent::Close { file: closed } if *closed == file)),
        "the programmer-error panic must precede physical close: {events:?}"
    );
}

#[test]
fn pool_teardown_drains_a_held_release_before_product_shutdown_progress() {
    let mock = MockDriver::builder()
        .queue_capacity(2)
        .frames(FRAME_COUNT)
        .frame_bytes(GRANULE)
        .build();
    let retained_file = mock
        .open(
            Path::new("retention-release-before-shutdown"),
            DirectIo::Disabled,
        )
        .expect("retained-frame file opens");
    let retained_file_id = retained_file.file_id();
    let product_file = mock
        .open(Path::new("retention-product-shutdown"), DirectIo::Disabled)
        .expect("product-I/O file opens");
    let product_file_id = product_file.file_id();
    let pool = Pool::builder()
        .frame_count(FRAME_COUNT)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(2)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .max_retained_frames(1)
        .max_inflight_product_ops(1)
        .build_on(mock)
        .expect("retention and one product operation fit the fixture");
    pool.register_file(retained_file);
    pool.register_file(product_file);
    let io_observation = pool.observe_io();
    let reader = pool.register_reader().expect("reader slot is available");
    let (frame, retained) = retain_held(&pool, &reader, retained_file_id);
    pool.submit_fsync(product_file_id, SyncMode::Full)
        .expect("product I/O is accepted before teardown");
    assert_eq!(pool.retire_file(retained_file_id), RetireStatus::Retiring);
    drop(retained);
    assert_eq!(pool.frame_state(frame), FrameState::Evicting);
    drop(reader);

    drop(pool);

    let events = io_observation.io_events_in_order();
    let release_completed = events
        .iter()
        .position(|event| {
            matches!(
                event,
                MockIoEvent::Close { file } if *file == retained_file_id
            )
        })
        .expect("retained-frame release allows its retiring file to close");
    let product_shutdown_progress = events
        .iter()
        .position(|event| {
            matches!(
                event,
                MockIoEvent::FsyncCompletion { file, .. } if *file == product_file_id
            )
        })
        .expect("teardown completes the accepted product I/O");
    assert!(
        release_completed < product_shutdown_progress,
        "pending retained-frame release must drain before product-I/O shutdown progress: {events:?}"
    );
}

#[cfg(target_os = "linux")]
mod linux_shipping {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use dios::testing::{ShippingWaitObservation, ShippingWaitTestingExt};

    use super::*;

    static UNIQUE: AtomicU32 = AtomicU32::new(0);

    fn shipping_pool() -> (Pool, FileId) {
        let sequence = UNIQUE.fetch_add(1, Ordering::Relaxed);
        let mut path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        std::fs::create_dir_all(&path).expect("target temp directory exists");
        path.push(format!(
            "retention-shipping-wake-{}-{sequence}",
            std::process::id()
        ));
        std::fs::write(&path, vec![0u8; GRANULE as usize]).expect("seed shipping fixture");
        let pool = Pool::builder()
            .frame_count(FRAME_COUNT)
            .granule(GRANULE)
            .max_concurrent_readers(1)
            .peak_guards_per_reader(2)
            .max_inflight_reads(1)
            .miss_headroom(3)
            .max_retained_frames(1)
            .build()
            .expect("shipping retention fixture initializes");
        let file = pool
            .open(&path, DirectIo::Disabled)
            .expect("shipping fixture opens");
        std::fs::remove_file(&path).expect("opened shipping fixture unlinks");
        (pool, file)
    }

    fn shipping_retain_held<'pool>(
        pool: &'pool Pool,
        reader: &'pool ReaderCtx,
        file: FileId,
    ) -> (ReadFrameIdx, RetainedFrame<'pool>) {
        let page = PageId::new(file, 0);
        let frame = pool.insert_resident_frame(page, 0xB6);
        let Get::Hit(guard) = pool.get(reader, page).expect("shipping page is live") else {
            panic!("the inserted shipping page must hit");
        };
        let Ok(retained) = guard.into_retained() else {
            panic!("the shipping pool must admit one retained frame");
        };
        assert_eq!(pool.evict_frame(page), frame);
        let mut completions = PoolCompletionBatch::with_capacity(0);
        for _ in 0..HELD_POLLS {
            let report = pool.poll_report(&mut completions);
            assert_eq!(report.backend_completions(), 0);
            assert_eq!(report.reclaimed_frames(), 0);
        }
        assert_eq!(pool.frame_state(frame), FrameState::Evicting);
        (frame, retained)
    }

    fn spawn_shipping_waiter(
        pool: Arc<Pool>,
    ) -> (thread::JoinHandle<()>, mpsc::Receiver<WaitReport>) {
        let (report_tx, report_rx) = mpsc::sync_channel(1);
        let waiter = thread::spawn(move || {
            let mut completions = PoolCompletionBatch::with_capacity(0);
            let report = pool.poll_wait(&mut completions, WAIT_DEADLINE);
            report_tx
                .send((
                    report.backend_completions(),
                    report.reclaimed_frames(),
                    completions.iter().count(),
                ))
                .expect("shipping wait report receiver remains live");
        });
        (waiter, report_rx)
    }

    #[test]
    fn held_release_wakes_shipping_platform_wait_and_reclaims() {
        let (pool, file) = shipping_pool();
        let observation: ShippingWaitObservation = pool.observe_shipping_waits();
        let pool = Arc::new(pool);
        let reader = pool.register_reader().expect("reader slot is available");
        let (frame, retained) = shipping_retain_held(&pool, &reader, file);
        let cleanup_wake = pool.wake_handle();
        let (waiter, report_rx) = spawn_shipping_waiter(Arc::clone(&pool));

        if !observation.wait_until_parked(OBSERVE_PARK) {
            drop(retained);
            cleanup_wake.wake();
            waiter.join().expect("cleanup joins the shipping waiter");
            panic!("shipping poll_wait never entered its platform wait hook");
        }
        assert_eq!(observation.parks_entered(), 1);
        assert_eq!(observation.parks_in_progress(), 1);
        assert_eq!(observation.parks_exited(), 0);
        drop(retained);

        let report = match report_rx.recv_timeout(PROMPT_WAKE) {
            Ok(report) => report,
            Err(error) => {
                cleanup_wake.wake();
                waiter.join().expect("cleanup joins the shipping waiter");
                panic!("HELD final-drop did not wake the platform wait: {error}");
            }
        };
        waiter
            .join()
            .expect("shipping waiter joins after HELD release");
        assert_eq!(report, (0, 1, 0));
        assert_eq!(pool.frame_state(frame), FrameState::Free);
        assert_eq!(observation.parks_entered(), 1);
        assert_eq!(observation.parks_in_progress(), 0);
        assert_eq!(observation.parks_exited(), 1);
        assert_eq!(observation.wake_exits(), 1);
        assert_eq!(observation.timeout_exits(), 0);
    }
}
