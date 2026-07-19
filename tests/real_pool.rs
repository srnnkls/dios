//! Real-backend pool contract: file ownership and reads are observable only
//! through the residency ADTs and the borrowed frame.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use dios::{DirectIo, FileId, FrameGuard, Get, PageId, PendingToken, Pool, ReaderCtx, ReadyResult};

const GRANULE: u32 = 4096;
const POLLS_MAX: u32 = 256;

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

fn pending(outcome: Get<'_>) -> PendingToken {
    match outcome {
        Get::Pending(token) => token,
        Get::Hit(_) => panic!("a first lookup of an uncached extent cannot hit"),
        Get::Busy => panic!("the configured pool has miss headroom"),
    }
}

fn admit_pending(pool: &Pool, reader: &ReaderCtx<'_>, page: PageId) -> PendingToken {
    for _ in 0..POLLS_MAX {
        match pool.get(reader, page) {
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
    reader: &'pool ReaderCtx<'pool>,
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

    let canary = match pool.get(&reader, PageId::new(canary_file, 1)) {
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
        Injected, MockDriver, MockPoolTestingExt, PoolBuilderTestingExt, PoolTestingExt,
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
