use std::cell::Cell;
use std::hint::black_box;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use dios::testing::{MockDriver, PoolBuilderTestingExt, PoolTestingExt};
use dios::{DirectIo, FileId, Get, PageId, PendingToken, Pool, ReaderCtx, ReadyResult};

const GRANULE: u32 = 4096;
const HIT_FRAMES: u32 = 64;
const HIT_ITERS: u32 = 4096 * 4096;
const MISS_FRAMES: u32 = 256;
const MISS_ITERS: u32 = 64 * 2048;
const HIT_INFLIGHT: u32 = 1;
const MISS_INFLIGHT: u32 = 64;
const MOCK_QUEUE: u32 = 65_536;
const READY_POLLS_MAX: u32 = 4096;
const BUSY_POLLS_MAX: u32 = 4096;

struct Arm {
    pool: Pool<MockDriver>,
    reader: ReaderCtx,
    file: FileId,
    next_page: Cell<u32>,
}

impl Arm {
    fn new(frames: u32, inflight: u32, name: &str) -> Self {
        let mock = MockDriver::builder()
            .seed(0x0016_0419)
            .queue_capacity(MOCK_QUEUE)
            .frames(frames)
            .frame_bytes(GRANULE)
            .retry_bound(0)
            .build();
        let handle = mock
            .open(Path::new(name), DirectIo::Disabled)
            .expect("mock file opens");
        let file = handle.file_id();
        let pool = Pool::builder()
            .frame_count(frames)
            .granule(GRANULE)
            .max_concurrent_readers(1)
            .peak_guards_per_reader(1)
            .max_inflight_reads(inflight)
            .miss_headroom(inflight * 3)
            .build_on(mock)
            .expect("watermark-satisfying pool composes over the mock");
        pool.register_file(handle);
        let reader = pool.register_reader().expect("first reader slot");
        let arm = Self {
            pool,
            reader,
            file,
            next_page: Cell::new(0),
        };
        for _ in 0..frames {
            arm.cold_miss();
        }
        arm
    }

    fn next_page(&self) -> PageId {
        let n = self.next_page.get();
        self.next_page
            .set(n.checked_add(1).expect("the page counter never wraps"));
        PageId::new(self.file, n)
    }

    fn drive_ready(&self, token: PendingToken) -> Option<()> {
        let mut token = token;
        for _ in 0..READY_POLLS_MAX {
            match self.pool.ready(&self.reader, token) {
                ReadyResult::Ready(guard) => {
                    black_box(guard.len());
                    return Some(());
                }
                ReadyResult::NotYet(handed_back) => {
                    token = handed_back;
                    self.pool.poll();
                }
                ReadyResult::Err(_) => return None,
            }
        }
        None
    }

    fn cold_miss(&self) {
        let page = self.next_page();
        for _ in 0..BUSY_POLLS_MAX {
            match self
                .pool
                .get(&self.reader, page)
                .expect("the registered file is live")
            {
                Get::Pending(token) => {
                    self.drive_ready(token).expect("a cold miss completes");
                    return;
                }
                Get::Hit(_) => panic!("a never-read page cannot hit"),
                Get::Busy => {
                    self.pool.poll();
                }
            }
        }
        panic!("a cold miss admits within the bounded busy retries");
    }

    fn warm_hit(&self, page: u32) {
        match self
            .pool
            .get(&self.reader, PageId::new(self.file, page))
            .expect("the registered file is live")
        {
            Get::Hit(guard) => black_box(guard.len()),
            Get::Pending(_) | Get::Busy => panic!("a resident page always hits"),
        };
    }
}

fn time_hits() -> u128 {
    let arm = Arm::new(HIT_FRAMES, HIT_INFLIGHT, "frame-write-path-hits");
    let mut state = 0x9E37_79B9_u32;
    let started = Instant::now();
    for _ in 0..HIT_ITERS {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        arm.warm_hit(state % HIT_FRAMES);
    }
    started.elapsed().as_nanos()
}

fn time_misses() -> u128 {
    let arm = Arm::new(MISS_FRAMES, MISS_INFLIGHT, "frame-write-path-misses");
    let started = Instant::now();
    for _ in 0..MISS_ITERS {
        arm.cold_miss();
    }
    started.elapsed().as_nanos()
}

fn main() -> ExitCode {
    let case = std::env::args()
        .skip(1)
        .find(|argument| !argument.starts_with('-'));
    let elapsed_ns = match case.as_deref() {
        Some("hits") => time_hits(),
        Some("misses") => time_misses(),
        _ => {
            println!("usage: frame_write_path <hits|misses>");
            return ExitCode::SUCCESS;
        }
    };
    println!("{elapsed_ns}");
    ExitCode::SUCCESS
}
