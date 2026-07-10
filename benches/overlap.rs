//! DIO-G3 overlap bench (T008): 64 concurrent cold `get`s against a single cold
//! miss over the composed pool. Advisory on the container (the eager/mock backend
//! executes reads inline at `poll`, so there is no kernel overlap); the binding
//! 2.0× ratio gate runs at T014 on the pinned Linux `io_uring` host. The bench
//! writes samples for the shared compare harness (`mise run gate`) and never
//! asserts the gate in-bench.

use std::cell::Cell;
use std::hint::black_box;
use std::path::Path;

use dios::bench::{ratio_gate, run_paired, write_samples};
use dios::mock::MockDriver;
use dios::{FileId, Get, PageId, PendingToken, Pool, ReaderCtx, ReadyResult};

const GRANULE: u32 = 4096;
const FRAMES: u32 = 256;
const CONCURRENT: u32 = 64;
const PAGE_RANGE: u32 = 4096;
const REPS: u32 = 40;
const ITERS_PER_REP: u32 = 8;
const BOOTSTRAP_RESAMPLES: u32 = 10_000;
const READY_POLLS_MAX: u32 = 4096;

fn next_page(counter: &Cell<u32>, file: FileId) -> PageId {
    let n = counter.get();
    counter.set(n.wrapping_add(1));
    PageId::new(file, n % PAGE_RANGE)
}

fn drive_ready<'pool>(
    pool: &'pool Pool<MockDriver>,
    reader: &'pool ReaderCtx<'pool>,
    token: PendingToken,
) -> Option<()> {
    let mut token = token;
    for _ in 0..READY_POLLS_MAX {
        match pool.ready(reader, token) {
            ReadyResult::Ready(guard) => {
                black_box(guard.len());
                return Some(());
            }
            ReadyResult::NotYet(handed_back) => {
                token = handed_back;
                pool.poll();
            }
            ReadyResult::Err(_) => return None,
        }
    }
    None
}

fn cold_miss(pool: &Pool<MockDriver>, reader: &ReaderCtx<'_>, counter: &Cell<u32>, file: FileId) {
    match pool.get(reader, next_page(counter, file)) {
        Get::Pending(token) => {
            drive_ready(pool, reader, token);
        }
        Get::Hit(guard) => {
            black_box(guard.len());
        }
        Get::Busy => {
            pool.poll();
        }
    }
}

fn overlapped_misses(
    pool: &Pool<MockDriver>,
    reader: &ReaderCtx<'_>,
    counter: &Cell<u32>,
    file: FileId,
) {
    let mut tokens = Vec::with_capacity(CONCURRENT as usize);
    for _ in 0..CONCURRENT {
        match pool.get(reader, next_page(counter, file)) {
            Get::Pending(token) => tokens.push(token),
            Get::Hit(guard) => {
                black_box(guard.len());
            }
            Get::Busy => {
                pool.poll();
            }
        }
    }
    for token in tokens {
        drive_ready(pool, reader, token);
    }
}

fn main() {
    let mock = MockDriver::builder()
        .seed(0x0016_0417)
        .queue_capacity(CONCURRENT)
        .frames(FRAMES)
        .frame_bytes(GRANULE)
        .retry_bound(0)
        .build();
    let file = mock
        .open(Path::new("overlap"), dios::OpenHow::read_write())
        .expect("mock file opens");
    let file_id = file.file_id();
    let pool = Pool::builder()
        .frame_count(FRAMES)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(CONCURRENT)
        .miss_headroom(CONCURRENT * 3)
        .build_on(mock)
        .expect("watermark-satisfying overlap pool composes over the mock");
    pool.register_file(file);
    let reader = pool.register_reader().expect("first reader slot");
    let counter = Cell::new(0u32);

    let samples = run_paired(
        "overlap",
        REPS,
        ITERS_PER_REP,
        || cold_miss(&pool, &reader, &counter, file_id),
        || overlapped_misses(&pool, &reader, &counter, file_id),
    );

    let gate = ratio_gate(&samples, BOOTSTRAP_RESAMPLES);
    let path =
        write_samples(Path::new("target/bench-samples"), &samples).expect("write samples CSV");
    println!(
        "overlap: pairs {REPS}, ratio geomean {:.4}, ci95 upper {:.4}, samples {}",
        gate.ratio_geomean,
        gate.ratio_ci95_upper,
        path.display()
    );
}
