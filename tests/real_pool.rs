//! Real-backend pool contract: file ownership and reads are observable only
//! through the residency ADTs and the borrowed frame.

#[cfg(not(target_os = "linux"))]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
#[cfg(feature = "mock")]
use std::sync::{Arc, mpsc};
#[cfg(feature = "mock")]
use std::thread;
use std::time::Duration;
#[cfg(feature = "mock")]
use std::time::Instant;

#[cfg(feature = "mock")]
use dios::PoolWakeHandle;
#[cfg(feature = "mock")]
use dios::testing::{ShippingWaitObservation, ShippingWaitTestingExt};
use dios::{
    DirectIo, FileId, FrameGuard, Get, GetError, PageId, PendingToken, Pool, PoolCompletion,
    PoolCompletionBatch, PoolSubmitError, ReaderCtx, ReadyResult,
};

const GRANULE: u32 = 4096;
const POLLS_MAX: u32 = 256;
const QUEUE_SUM_WAIT: Duration = Duration::from_millis(20);

static UNIQUE: AtomicU32 = AtomicU32::new(0);

fn temp_path(tag: &str) -> PathBuf {
    let sequence = UNIQUE.fetch_add(1, Ordering::Relaxed);
    let mut path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&path).expect("target temp directory");
    path.push(format!("real-pool-{tag}-{}-{sequence}", std::process::id()));
    path
}

fn pool() -> Pool {
    Pool::builder()
        .frame_count(4)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .build()
        .expect("a watermark-satisfying real pool")
}

fn product_pool_for_checked_queue_sum() -> Pool {
    Pool::builder()
        .frame_count(4)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .write_slots(3)
        .max_inflight_product_ops(2)
        .build()
        .expect("the shipping pool reserves read plus product queue capacity")
}

fn pending(outcome: Result<Get<'_>, GetError>) -> PendingToken {
    match outcome.expect("the registered file is live") {
        Get::Pending(token) => token,
        Get::Hit(_) => panic!("a first lookup of an uncached extent cannot hit"),
        Get::Busy => panic!("the configured pool has miss headroom"),
    }
}

#[cfg(not(target_os = "linux"))]
fn admit_pending(pool: &Pool, reader: &ReaderCtx, page: PageId) -> PendingToken {
    for _ in 0..POLLS_MAX {
        match pool.get(reader, page).expect("the registered file is live") {
            Get::Pending(token) => return token,
            Get::Hit(_) => panic!("a first lookup of an uncached extent cannot hit"),
            Get::Busy => {
                pool.poll();
            }
        }
    }
    panic!("bounded CLOCK reclamation did not admit the miss");
}

fn ready<'pool>(
    pool: &'pool Pool,
    reader: &'pool ReaderCtx,
    mut token: PendingToken,
) -> FrameGuard<'pool> {
    for _ in 0..POLLS_MAX {
        match pool.ready(reader, token) {
            ReadyResult::Ready(guard) => return guard,
            ReadyResult::NotYet(handed_back) => {
                token = handed_back;
                pool.poll();
            }
            ReadyResult::Err(error) => panic!("a complete file extent must read: {error}"),
        }
    }
    panic!("the real backend did not complete within the bounded poll budget");
}

fn patterned_extent(seed: u8) -> Vec<u8> {
    (0..GRANULE)
        .map(|index| seed.wrapping_add((index % 251) as u8))
        .collect()
}

fn assert_extent_eq(actual: &[u8], expected: &[u8], contract: &str) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "{contract}: exact extent length"
    );
    if let Some(index) = actual
        .iter()
        .zip(expected)
        .position(|(actual, expected)| actual != expected)
    {
        panic!(
            "{contract}: first mismatch at byte {index}: actual {:#04x}, expected {:#04x}",
            actual[index], expected[index]
        );
    }
}

fn open_file(pool: &Pool, path: &Path, direct_io: DirectIo) -> FileId {
    pool.open(path, direct_io)
        .expect("the pool opens and retains the fixture")
}

#[cfg(feature = "mock")]
struct WakeOnDrop(Option<PoolWakeHandle>);

#[cfg(feature = "mock")]
impl WakeOnDrop {
    fn wake(&self) {
        if let Some(wake) = &self.0 {
            wake.wake();
        }
    }

    fn disarm(&mut self) {
        self.0.take();
    }
}

#[cfg(feature = "mock")]
impl Drop for WakeOnDrop {
    fn drop(&mut self) {
        self.wake();
    }
}

#[cfg(feature = "mock")]
#[test]
fn shipping_pool_external_wake_interrupts_the_actual_backend_park() {
    const LONG_WAIT: Duration = Duration::from_secs(10);
    const OBSERVE_PARK: Duration = Duration::from_secs(1);
    const PROMPT_WAKE: Duration = Duration::from_secs(2);

    let pool = Arc::new(pool());
    let observation: ShippingWaitObservation = pool.observe_shipping_waits();
    let wake = pool.wake_handle();
    let mut cleanup = WakeOnDrop(Some(wake.clone()));
    let (report_tx, report_rx) = mpsc::sync_channel(1);
    let waiter = {
        let pool = Arc::clone(&pool);
        thread::spawn(move || {
            let mut completions = PoolCompletionBatch::with_capacity(0);
            let started = Instant::now();
            let report = pool.poll_wait(&mut completions, LONG_WAIT);
            report_tx
                .send((
                    report.backend_completions(),
                    report.reclaimed_frames(),
                    completions.iter().count(),
                    started.elapsed(),
                ))
                .expect("the shipping-wait report receiver remains live");
        })
    };

    if !observation.wait_until_parked(OBSERVE_PARK) {
        cleanup.wake();
        waiter
            .join()
            .expect("the cleanup wake releases the shipping waiter");
        panic!("the shipping backend never entered its actual blocking wait hook");
    }
    assert_eq!(observation.parks_entered(), 1);
    assert_eq!(observation.parks_in_progress(), 1);
    assert_eq!(observation.parks_exited(), 0);
    assert_eq!(observation.wake_exits(), 0);
    assert_eq!(observation.timeout_exits(), 0);
    assert!(matches!(
        report_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));

    let wake_started = Instant::now();
    wake.wake();
    let report = match report_rx.recv_timeout(PROMPT_WAKE) {
        Ok(report) => report,
        Err(error) => {
            cleanup.wake();
            waiter
                .join()
                .expect("the cleanup wake releases the shipping waiter");
            panic!("the real backend wait ignored PoolWakeHandle: {error}");
        }
    };
    let wake_elapsed = wake_started.elapsed();
    waiter.join().expect("the shipping waiter joins");
    cleanup.disarm();

    let (backend, reclaimed, delivered, elapsed) = report;
    assert_eq!(backend, 0, "an external wake is not a backend completion");
    assert_eq!(reclaimed, 0, "an idle external wake reclaims no frame");
    assert_eq!(delivered, 0, "an idle external wake delivers no result");
    assert!(
        wake_elapsed < PROMPT_WAKE,
        "the observed shipping park exits promptly after its signal: {wake_elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "signal-driven exit occurs well before the ten-second deadline: {elapsed:?}"
    );
    assert_eq!(observation.parks_entered(), 1);
    assert_eq!(observation.parks_in_progress(), 0);
    assert_eq!(observation.parks_exited(), 1);
    assert_eq!(observation.wake_exits(), 1);
    assert_eq!(observation.timeout_exits(), 0);
}

#[test]
fn shipping_pool_reserves_the_checked_sum_for_reads_and_product_writes() {
    let path = temp_path("checked-queue-sum");
    let read_extent = patterned_extent(0x37);
    let mut file_bytes = Vec::with_capacity((GRANULE * 3) as usize);
    file_bytes.extend_from_slice(&read_extent);
    file_bytes.resize((GRANULE * 3) as usize, 0);
    std::fs::write(&path, &file_bytes).expect("seed three complete extents");

    let pool = product_pool_for_checked_queue_sum();
    let file = open_file(&pool, &path, DirectIo::Disabled);
    let reader = pool.register_reader().expect("one reader slot");
    let read = pending(pool.get(&reader, PageId::new(file, 0)));

    let arena = pool.write_arena();
    let mut first_slot = arena.alloc().expect("first configured staging slot");
    first_slot.fill(0xA1);
    let mut second_slot = arena.alloc().expect("second configured staging slot");
    second_slot.fill(0xB2);
    let mut refused_slot = arena.alloc().expect("third configured staging slot");
    refused_slot.fill(0xC3);

    let first_write = pool
        .submit_write(file, first_slot, GRANULE.into())
        .expect("one read plus the first product write fit concurrently");
    let second_write = pool
        .submit_write(file, second_slot, u64::from(GRANULE) * 2)
        .expect("one read plus both configured product writes fit concurrently");
    let (error, returned) = pool
        .submit_write(file, refused_slot, 0)
        .expect_err("the next product operation reaches the exact configured bound");
    assert!(matches!(error, PoolSubmitError::Full));
    assert!(returned.iter().all(|&byte| byte == 0xC3));
    drop(returned);

    let mut completions = PoolCompletionBatch::with_capacity(2);
    let mut backend_completions = 0u32;
    let mut first_seen = false;
    let mut second_seen = false;
    for _ in 0..POLLS_MAX {
        let report = pool.poll_wait(&mut completions, QUEUE_SUM_WAIT);
        backend_completions += report.backend_completions();
        assert_eq!(report.reclaimed_frames(), 0);
        for completion in completions.iter() {
            match completion {
                PoolCompletion::Write {
                    token,
                    result: Ok(bytes),
                } if *token == first_write => {
                    assert_eq!(*bytes, GRANULE);
                    assert!(!first_seen, "the first write completes exactly once");
                    first_seen = true;
                }
                PoolCompletion::Write {
                    token,
                    result: Ok(bytes),
                } if *token == second_write => {
                    assert_eq!(*bytes, GRANULE);
                    assert!(!second_seen, "the second write completes exactly once");
                    second_seen = true;
                }
                PoolCompletion::Write {
                    token,
                    result: Ok(_),
                } => panic!("an unknown write token completed: {token:?}"),
                PoolCompletion::Write {
                    result: Err(error), ..
                } => panic!("a shipping queue-sum write failed: {error}"),
                PoolCompletion::Fsync { .. } => {
                    panic!("the queue-sum fixture admitted no fsync operation")
                }
            }
        }
        if backend_completions == 3 && first_seen && second_seen {
            break;
        }
    }
    assert_eq!(
        backend_completions, 3,
        "one read and two writes occupied the checked-sum shipping queue \
         (first write seen: {first_seen}, second write seen: {second_seen})"
    );
    assert!(first_seen);
    assert!(second_seen);

    match pool.ready(&reader, read) {
        ReadyResult::Ready(frame) => assert_extent_eq(
            &frame,
            &read_extent,
            "the concurrently admitted shipping read readies exactly",
        ),
        ReadyResult::NotYet(_) => panic!("the drained shipping read must be ready"),
        ReadyResult::Err(error) => panic!("the complete shipping read failed: {error}"),
    }

    let stored = std::fs::read(&path).expect("read the completed shipping writes");
    assert!(
        stored[GRANULE as usize..(GRANULE * 2) as usize]
            .iter()
            .all(|&byte| byte == 0xA1)
    );
    assert!(
        stored[(GRANULE * 2) as usize..(GRANULE * 3) as usize]
            .iter()
            .all(|&byte| byte == 0xB2)
    );
}

#[test]
fn real_pool_miss_borrows_the_exact_file_extent() {
    let path = temp_path("exact");
    let first = patterned_extent(0x11);
    let second = patterned_extent(0xA7);
    let mut file_bytes = Vec::with_capacity((GRANULE * 2) as usize);
    file_bytes.extend_from_slice(&first);
    file_bytes.extend_from_slice(&second);
    std::fs::write(&path, &file_bytes).expect("seed two exact extents");

    let pool = pool();
    let file = open_file(&pool, &path, DirectIo::Disabled);
    let reader = pool.register_reader().expect("one reader slot");
    let token = pending(pool.get(&reader, PageId::new(file, 1)));
    let guard = ready(&pool, &reader, token);

    assert_extent_eq(
        &guard,
        &second,
        "the real backend fills the pool-owned frame at the requested file extent",
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn eager_short_read_preserves_prefix_and_reads_only_the_extent_remainder() {
    let path = temp_path("short-remainder");
    let canary_path = temp_path("short-canary");
    let split = (GRANULE / 2) as usize;
    let prefix = vec![0x3Cu8; split];
    std::fs::write(&path, &prefix).expect("seed a nonzero short extent");
    let canary_extents: Vec<Vec<u8>> = (0..4).map(|index| patterned_extent(0x41 + index)).collect();
    let canary_file_bytes: Vec<u8> = canary_extents.iter().flatten().copied().collect();
    std::fs::write(&canary_path, canary_file_bytes).expect("seed the resident canary extents");

    let pool = pool();
    let file = open_file(&pool, &path, DirectIo::Disabled);
    let canary_file = open_file(&pool, &canary_path, DirectIo::Preferred);
    let reader = pool.register_reader().expect("one reader slot");
    for index in 0..4 {
        let page = PageId::new(canary_file, index);
        let guard = ready(&pool, &reader, pending(pool.get(&reader, page)));
        drop(guard);
    }
    let page = PageId::new(file, 0);
    let token = admit_pending(&pool, &reader, page);

    pool.poll();
    let token = match pool.ready(&reader, token) {
        ReadyResult::NotYet(token) => token,
        ReadyResult::Ready(_) => panic!("a nonzero short read must submit its remainder"),
        ReadyResult::Err(error) => panic!("a nonzero short read is continuable: {error}"),
    };

    let tail = vec![0xB5u8; split];
    let beyond_extent = vec![0xE9u8; GRANULE as usize];
    let mut writer = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("reopen the fixture for append between eager polls");
    writer.write_all(&tail).expect("append the requested tail");
    writer
        .write_all(&beyond_extent)
        .expect("append a sentinel extent that the continuation must not read");
    writer.flush().expect("publish the appended fixture bytes");

    let guard = ready(&pool, &reader, token);
    let mut expected = prefix;
    expected.extend_from_slice(&tail);
    assert_extent_eq(
        &guard,
        &expected,
        "the continuation starts at destination offset `filled`, preserves the prefix, and is bounded to the unfilled tail",
    );
    drop(guard);

    let canary = match pool
        .get(&reader, PageId::new(canary_file, 1))
        .expect("the canary file is live")
    {
        Get::Hit(guard) => guard,
        Get::Pending(_) => panic!("the continuation over-read into a resident neighbor"),
        Get::Busy => panic!("a resident canary lookup cannot backpressure"),
    };
    assert_extent_eq(
        &canary,
        &canary_extents[1],
        "the continuation never writes beyond the requested extent remainder",
    );
}

#[cfg(feature = "mock")]
mod portable_read_ranges {
    use std::path::Path;

    use dios::testing::{
        Injected, MockDriver, MockIoEvent, MockPoolTestingExt, PoolBuilderTestingExt,
        PoolTestingExt, ReadAttempt,
    };

    use super::*;

    const EIO: i32 = 5;

    fn mock_pool(mock: MockDriver) -> Pool<MockDriver> {
        Pool::builder()
            .frame_count(4)
            .granule(GRANULE)
            .max_concurrent_readers(1)
            .peak_guards_per_reader(1)
            .max_inflight_reads(1)
            .miss_headroom(3)
            .build_on(mock)
            .expect("the mock pool satisfies its watermark")
    }

    #[test]
    fn direct_io_unaligned_positive_short_read_is_terminal_without_remainder_submission() {
        const SHORT: u32 = 1500;

        let mock = MockDriver::builder()
            .seed(0xD357_1A7E)
            .queue_capacity(1)
            .frames(4)
            .frame_bytes(GRANULE)
            .retry_bound(0)
            .build();
        let handle = mock
            .open(Path::new("portable-direct-short-read"), DirectIo::Required)
            .expect("mock direct-I/O open");
        let file = handle.file_id();
        mock.inject_next(Injected::Short(SHORT));
        let pool = mock_pool(mock);
        pool.register_file(handle);
        let reader = pool.register_reader().expect("one reader slot");
        let mut token = pending(pool.get(&reader, PageId::new(file, 0)));

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for _ in 0..POLLS_MAX {
                pool.poll();
                match pool.ready(&reader, token) {
                    ReadyResult::NotYet(handed_back) => token = handed_back,
                    ReadyResult::Err(error) => return Some(error),
                    ReadyResult::Ready(_) => return None,
                }
            }
            None
        }));
        let error = match outcome {
            Ok(Some(error)) => error,
            Ok(None) => {
                let _pool = std::mem::ManuallyDrop::new(pool);
                panic!("an unaligned direct-I/O short count must terminate as an error");
            }
            Err(_) => {
                let _pool = std::mem::ManuallyDrop::new(pool);
                panic!("an unaligned direct-I/O short count must not panic while polling");
            }
        };
        let errno = error.raw_os_error();
        let attempts = pool.driver().read_attempts_in_order();

        let read_credit_released =
            matches!(pool.get(&reader, PageId::new(file, 1)), Ok(Get::Pending(_)));
        if !read_credit_released {
            let _pool = std::mem::ManuallyDrop::new(pool);
            panic!("the terminal direct-I/O error must release its logical read credit");
        }

        assert_eq!(errno, Some(EIO));
        assert_eq!(
            attempts,
            vec![ReadAttempt {
                file_offset: 0,
                destination_offset: 0,
                requested_len: GRANULE,
            }],
            "the invalid direct-I/O remainder must not be submitted"
        );
    }

    #[test]
    fn short_read_resubmits_the_exact_tail_at_the_filled_destination_offset() {
        let short = GRANULE / 2;
        let mock = MockDriver::builder()
            .seed(0xD357_1A7E)
            .queue_capacity(1)
            .frames(4)
            .frame_bytes(GRANULE)
            .retry_bound(0)
            .build();
        let handle = mock
            .open(Path::new("portable-read-ranges"), DirectIo::Disabled)
            .expect("mock open");
        let file = handle.file_id();
        mock.inject_next(Injected::Short(short));
        mock.inject_next(Injected::Io(EIO));
        let pool = mock_pool(mock);
        pool.register_file(handle);
        let reader = pool.register_reader().expect("one reader slot");
        let page = PageId::new(file, 2);
        let mut token = pending(pool.get(&reader, page));

        for _ in 0..POLLS_MAX {
            pool.poll();
            match pool.ready(&reader, token) {
                ReadyResult::NotYet(handed_back) => token = handed_back,
                ReadyResult::Err(_) => break,
                ReadyResult::Ready(_) => {
                    panic!("the injected remainder failure cannot ready the page")
                }
            }
        }

        let attempts = pool.driver().read_attempts_in_order();
        let projected_attempts: Vec<ReadAttempt> = pool
            .driver()
            .io_events_in_order()
            .iter()
            .filter_map(|event| match event {
                MockIoEvent::ReadAttempt {
                    file: attempted_file,
                    file_offset,
                    destination_offset,
                    requested_len,
                } if *attempted_file == file => Some(ReadAttempt {
                    file_offset: *file_offset,
                    destination_offset: *destination_offset,
                    requested_len: *requested_len,
                }),
                _ => None,
            })
            .collect();
        assert_eq!(
            attempts, projected_attempts,
            "the frozen read-attempt accessor is the typed projection of the unified event stream"
        );
        assert_eq!(attempts.len(), 2, "one full read and one exact remainder");
        let base = u64::from(GRANULE) * 2;
        assert_eq!(
            (
                attempts[0].file_offset,
                attempts[0].destination_offset,
                attempts[0].requested_len,
            ),
            (base, 0, GRANULE),
            "the first transfer spans the complete destination frame"
        );
        assert_eq!(
            (
                attempts[1].file_offset,
                attempts[1].destination_offset,
                attempts[1].requested_len,
            ),
            (base + u64::from(short), short, GRANULE - short),
            "the continuation advances file and destination offsets together and requests only the tail"
        );
    }
}
