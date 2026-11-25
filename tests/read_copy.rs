//! Copy-out read (FR-9 to FR-11): `Pool::read_copy` copies a byte range of one
//! resident granule into caller memory through one optimistic attempt and at
//! most one ordinary `get`; the output is valid only on `Copied`.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use dios::{DirectIo, FileId, Get, PageId, Pool, ReadCopy, Readahead, ReaderCtx, ReadyResult};

const GRANULE: u32 = 4096;
const POLLS_MAX: u32 = 256;
const SENTINEL: u8 = 0xEE;
const RESIDENT_GRANULE: u32 = 1;
const MISSED_GRANULE: u32 = 9;
const SATURATING_GRANULE: u32 = 18;
const FILE_GRANULES: u32 = 19;

static UNIQUE: AtomicU32 = AtomicU32::new(0);

/// The byte at `offset` of the fixture file; a period of 251 makes every
/// granule and every offset inside one distinguishable.
fn file_byte(offset: u64) -> u8 {
    u8::try_from(offset % 251).expect("a residue below 251 fits a byte")
}

fn patterned_file(tag: &str) -> PathBuf {
    let sequence = UNIQUE.fetch_add(1, Ordering::Relaxed);
    let mut path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&path).expect("target temp directory");
    path.push(format!("read-copy-{tag}-{}-{sequence}", std::process::id()));
    let bytes: Vec<u8> = (0..u64::from(FILE_GRANULES * GRANULE))
        .map(file_byte)
        .collect();
    std::fs::write(&path, bytes).expect("write the patterned fixture");
    path
}

fn expected_bytes(page: PageId, offset_bytes: u32, len: u32) -> Vec<u8> {
    let start = u64::from(page.granule_idx()) * u64::from(GRANULE) + u64::from(offset_bytes);
    (start..start + u64::from(len)).map(file_byte).collect()
}

fn resolve(pool: &Pool, reader: &ReaderCtx, page: PageId) {
    for _ in 0..POLLS_MAX {
        let mut token = match pool.get(reader, page).expect("the fixture file is live") {
            Get::Hit(_) => return,
            Get::Pending(token) => token,
            Get::Busy => {
                pool.poll();
                continue;
            }
        };
        for _ in 0..POLLS_MAX {
            match pool.ready(reader, token) {
                ReadyResult::Ready(_) => return,
                ReadyResult::NotYet(returned) => {
                    token = returned;
                    pool.poll();
                }
                ReadyResult::Err(error) => panic!("a complete fixture granule must read: {error}"),
            }
        }
        panic!("the warmup read did not complete within the poll bound");
    }
    panic!("the warmup read stayed Busy beyond the poll bound");
}

fn copy_pool() -> Pool {
    Pool::builder()
        .frame_count(4)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .readahead(Readahead::Disabled)
        .build()
        .expect("a watermark-satisfying shipping pool")
}

fn open_patterned(pool: &Pool, tag: &str) -> FileId {
    pool.open(&patterned_file(tag), DirectIo::Disabled)
        .expect("the shipping pool opens the fixture")
}

#[test]
#[cfg_attr(miri, ignore = "real file I/O is outside the Miri abstract machine")]
fn read_copy_copies_exact_resident_bytes_up_to_the_tail_and_reports_a_miss_and_saturation() {
    let pool = copy_pool();
    let file = open_patterned(&pool, "outcomes");
    let reader = pool.register_reader().expect("one reader slot");
    let resident = PageId::new(file, RESIDENT_GRANULE);
    resolve(&pool, &reader, resident);

    let ranges: [(u32, u32); 5] = [
        (0, 64),
        (13, 64),
        (GRANULE - 64, 64),
        (GRANULE - 13, 13),
        (0, GRANULE),
    ];
    for (offset_bytes, len) in ranges {
        let mut output = vec![SENTINEL; len as usize];
        match pool.read_copy(&reader, resident, offset_bytes, &mut output) {
            Ok(ReadCopy::Copied) => assert_eq!(
                output,
                expected_bytes(resident, offset_bytes, len),
                "Copied holds exactly the bytes of [{offset_bytes}, {offset_bytes} + {len}) of the granule"
            ),
            Ok(ReadCopy::Pending(_)) => panic!("a resident page copies, it is not Pending"),
            Ok(ReadCopy::Busy) => panic!("a resident page copies, it is not Busy"),
            Err(error) => panic!("the fixture file is live: {error:?}"),
        }
    }

    let mut output = [SENTINEL; 64];
    let Ok(ReadCopy::Pending(token)) =
        pool.read_copy(&reader, PageId::new(file, MISSED_GRANULE), 0, &mut output)
    else {
        panic!("a copy of an absent page admits its miss and returns Pending");
    };
    assert_eq!(token.page(), PageId::new(file, MISSED_GRANULE));
    assert!(
        matches!(
            pool.read_copy(
                &reader,
                PageId::new(file, SATURATING_GRANULE),
                0,
                &mut output
            ),
            Ok(ReadCopy::Busy)
        ),
        "with the one read credit taken, a copy of another absent page is Busy"
    );
    drop(token);

    let past_tail: [(u32, u32); 2] = [(GRANULE - 63, 64), (GRANULE, 1)];
    for (offset_bytes, len) in past_tail {
        let mut output = vec![SENTINEL; len as usize];
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let _ = pool.read_copy(&reader, resident, offset_bytes, &mut output);
        }));
        assert!(
            outcome.is_err(),
            "a range past the granule's tail at offset {offset_bytes} for {len} bytes asserts as a programmer error"
        );
    }
}

#[cfg(feature = "mock")]
mod mock {
    use std::path::Path;

    use dios::testing::{
        Injected, MockDriver, MockPoolTestingExt, PoolBuilderTestingExt, PoolTestingExt,
        ReadFrameIdx,
    };
    use dios::{DirectIo, Get, PageId, Pool, ReadCopy, Readahead, ReadyResult};

    use super::GRANULE;

    const FRAMES: u32 = 6;
    const FILE_GRANULES_MAX: u32 = 64;
    const RESIDENT_FILL: u8 = 0xA7;

    fn mock_pool() -> Pool<MockDriver> {
        let mock = MockDriver::builder()
            .queue_capacity(4)
            .frames(FRAMES)
            .frame_bytes(GRANULE)
            .build();
        Pool::builder()
            .frame_count(FRAMES)
            .granule(GRANULE)
            .max_concurrent_readers(1)
            .peak_guards_per_reader(1)
            .max_inflight_reads(1)
            .miss_headroom(3)
            .file_granules_max(FILE_GRANULES_MAX)
            .build_on(mock)
            .expect("valid copy pool")
    }

    #[test]
    fn a_failed_optimistic_attempt_takes_exactly_one_ordinary_get() {
        let pool = mock_pool();
        let file = pool
            .open(Path::new("read-copy-fallback"), DirectIo::Disabled)
            .expect("mock open");
        let resident = PageId::new(file, 0);
        pool.insert_resident_frame(resident, RESIDENT_FILL);
        let reader = pool.register_reader().expect("one reader slot");

        let observation = pool.force_optimistic_copy_failures();
        let mut output = [0u8; 64];
        assert!(
            matches!(
                pool.read_copy(&reader, resident, 128, &mut output),
                Ok(ReadCopy::Copied)
            ),
            "the ordinary get behind a failed optimistic attempt hits and copies"
        );
        assert!(
            output.iter().all(|&byte| byte == RESIDENT_FILL),
            "the fallback copies the resident page's bytes"
        );
        assert_eq!(
            observation.optimistic_attempts(),
            1,
            "one optimistic attempt per call, never retried"
        );
        assert_eq!(
            observation.fallback_gets(),
            1,
            "a failed optimistic attempt takes exactly one ordinary get"
        );

        assert!(
            matches!(
                pool.read_copy(&reader, PageId::new(file, 9), 0, &mut output),
                Ok(ReadCopy::Pending(_))
            ),
            "the single get behind a failed attempt admits a miss"
        );
        assert_eq!(observation.optimistic_attempts(), 2);
        assert_eq!(
            observation.fallback_gets(),
            2,
            "the miss takes one get and no second one"
        );

        observation.release();
        output.fill(0);
        assert!(
            matches!(
                pool.read_copy(&reader, resident, 128, &mut output),
                Ok(ReadCopy::Copied)
            ),
            "the optimistic attempt copies a stable resident page"
        );
        assert!(output.iter().all(|&byte| byte == RESIDENT_FILL));
        assert_eq!(observation.optimistic_attempts(), 3);
        assert_eq!(
            observation.fallback_gets(),
            2,
            "an accepted optimistic copy takes no ordinary get"
        );
    }

    #[test]
    fn a_busy_fallback_takes_exactly_one_ordinary_get() {
        let pool = mock_pool();
        let file = pool
            .open(Path::new("read-copy-busy"), DirectIo::Disabled)
            .expect("mock open");
        let reader = pool.register_reader().expect("one reader slot");
        let Ok(Get::Pending(_credit)) = pool.get(&reader, PageId::new(file, 9)) else {
            panic!("an absent page admits its miss and takes the one read credit");
        };

        let observation = pool.force_optimistic_copy_failures();
        let mut output = [0u8; 64];
        assert!(
            matches!(
                pool.read_copy(&reader, PageId::new(file, 18), 0, &mut output),
                Ok(ReadCopy::Busy)
            ),
            "with the one read credit taken, a copy of another absent page is Busy"
        );
        assert_eq!(
            observation.fallback_gets(),
            1,
            "a Busy fallback takes exactly one ordinary get, never a poll and a second get"
        );
        assert_eq!(
            observation.optimistic_attempts(),
            1,
            "one optimistic attempt per call, never retried"
        );
    }

    const FILL_FRAMES: u32 = 24;
    const FILL_ROUNDS: u32 = 2;
    const FILL_POLLS_MAX: u32 = 64;
    const WORDS_PER_GRANULE: usize = (GRANULE / 8) as usize;
    const INSERTED_FILL: u8 = 0x11;
    const POINT_GRANULE: u32 = 9;
    const POINT_FILL: u8 = 0x29;
    const SPAN: [(u32, u8); 3] = [(20, 0x41), (21, 0x42), (22, 0x43)];
    // 4,609 bytes end the first transfer 513 bytes into the span's second
    // granule, so its remainder starts inside a payload word.
    const SHORT_TRANSFER_BYTES: u32 = 4_609;

    fn fill_pool() -> (Pool<MockDriver>, dios::FileId) {
        let mock = MockDriver::builder()
            .queue_capacity(6)
            .frames(FILL_FRAMES)
            .frame_bytes(GRANULE)
            .build();
        let file = mock
            .open(Path::new("read-copy-fills"), DirectIo::Disabled)
            .expect("mock open");
        let file_id = file.file_id();
        mock.seed_page(&file, POINT_GRANULE, POINT_FILL);
        for (granule_idx, fill) in SPAN {
            mock.seed_page(&file, granule_idx, fill);
        }
        let pool = Pool::builder()
            .frame_count(FILL_FRAMES)
            .granule(GRANULE)
            .max_concurrent_readers(1)
            .peak_guards_per_reader(3)
            .max_inflight_reads(4)
            .max_inflight_product_ops(2)
            .miss_headroom(12)
            .prefetch_headroom(4)
            .readahead(Readahead::Disabled)
            .file_granules_max(FILE_GRANULES_MAX)
            .build_on(mock)
            .expect("valid fill pool");
        pool.register_file(file);
        (pool, file_id)
    }

    fn payload_bytes(pool: &Pool<MockDriver>, frame: ReadFrameIdx) -> Vec<u8> {
        let mut words = [0u64; WORDS_PER_GRANULE];
        pool.load_payload_words_atomic(frame, &mut words);
        words.iter().flat_map(|word| word.to_ne_bytes()).collect()
    }

    /// The in-process fills of the writer inventory: `Frames::fill`, the mock
    /// point read and the mock vector read, the latter split mid-word by a
    /// short transfer.
    fn run_in_process_fills(pool: &Pool<MockDriver>, file: dios::FileId) {
        let reader = pool.register_reader().expect("one reader slot");
        pool.insert_resident_frame(PageId::new(file, 0), INSERTED_FILL);

        let Ok(Get::Pending(mut token)) = pool.get(&reader, PageId::new(file, POINT_GRANULE))
        else {
            panic!("the point granule is absent and admits a read");
        };
        let mut completed = false;
        for _ in 0..FILL_POLLS_MAX {
            match pool.ready(&reader, token) {
                ReadyResult::Ready(_) => {
                    completed = true;
                    break;
                }
                ReadyResult::NotYet(returned) => {
                    token = returned;
                    pool.poll();
                }
                ReadyResult::Err(error) => panic!("a seeded point read completes: {error}"),
            }
        }
        assert!(completed, "the point read completes within the poll bound");

        pool.driver()
            .inject_next(Injected::Short(SHORT_TRANSFER_BYTES));
        let run = SPAN.map(|(granule_idx, _)| PageId::new(file, granule_idx));
        assert_eq!(pool.prefetch_span(&run).admitted, 3);
        for _ in 0..FILL_POLLS_MAX {
            if pool.prefetch_stats().reads_in_flight == 0 {
                break;
            }
            pool.poll();
        }
        assert_eq!(
            pool.prefetch_stats().reads_in_flight,
            0,
            "the span completes within the poll bound"
        );
    }

    /// AC-13, decided under `cargo miri test`: an atomic-load stand-in reads
    /// every frame's payload while the in-process fills run on another thread,
    /// with no synchronization between the two, so Miri's race detector
    /// rejects any payload write that is not a same-width atomic store.
    ///
    /// Model limits (audits.md, Miri model limits): the stand-in checks the
    /// writers only; it is not the production volatile copy, which Miri would
    /// report as a mixed volatile and atomic race and so does not run here.
    /// The `Release` fence after the claim is invisible to the race detector;
    /// the ordering audit and the Loom cases carry it. Kernel writes through
    /// `io_uring` and the eager syscalls lie outside the abstract machine.
    #[test]
    fn in_process_payload_fills_racing_an_atomic_payload_reader_raise_no_data_race() {
        let (pool, file) = fill_pool();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..FILL_ROUNDS {
                    for frame in 0..FILL_FRAMES {
                        let _ = payload_bytes(&pool, ReadFrameIdx::new(frame));
                    }
                }
            });
            run_in_process_fills(&pool, file);
        });

        let filled = [(0, INSERTED_FILL), (POINT_GRANULE, POINT_FILL)]
            .into_iter()
            .chain(SPAN);
        for (granule_idx, fill) in filled {
            let frame = pool
                .translation_lookup(PageId::new(file, granule_idx))
                .expect("every filled granule is resident");
            assert!(
                payload_bytes(&pool, frame).iter().all(|&byte| byte == fill),
                "the stand-in reads granule {granule_idx}'s payload as its fill wrote it"
            );
        }
    }
}
