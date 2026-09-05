//! Cold-miss lookup cost versus pool frame count. The base arm drives one
//! cold `get` (`get` → `poll` → `ready`) over a 256-frame pool, the
//! candidate arm the same over a 32,768-frame pool; both pools are warmed
//! to full occupancy first so every miss claims through the CLOCK. The
//! miss table and the free-frame list are sized to the frame count, so any
//! per-miss walk over them shows up as a ratio that grows with the frame
//! count. Samples go to the shared compare harness (`mise run gate`); the
//! gate is never asserted in-bench.

use std::cell::Cell;
use std::hint::black_box;
use std::path::Path;

use dios::bench::{ratio_gate, run_paired, write_samples};
use dios::testing::{MockDriver, PoolBuilderTestingExt, PoolTestingExt};
use dios::{DirectIo, FileId, Get, PageId, PendingToken, Pool, ReaderCtx, ReadyResult};

const GRANULE: u32 = 4096;
const BASE_FRAMES: u32 = 256;
const CANDIDATE_FRAMES: u32 = 32_768;
const INFLIGHT: u32 = 64;
const MOCK_QUEUE: u32 = 8192;
const REPS: u32 = 40;
const ITERS_PER_REP: u32 = 64;
const BOOTSTRAP_RESAMPLES: u32 = 10_000;
const READY_POLLS_MAX: u32 = 4096;

struct Arm {
    pool: Pool<MockDriver>,
    reader: ReaderCtx,
    file: FileId,
    next_page: Cell<u32>,
}

impl Arm {
    fn new(frames: u32, name: &str) -> Self {
        let mock = MockDriver::builder()
            .seed(0x0016_0418)
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
            .max_inflight_reads(INFLIGHT)
            .miss_headroom(INFLIGHT * 3)
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
        match self
            .pool
            .get(&self.reader, self.next_page())
            .expect("the registered file is live")
        {
            Get::Pending(token) => {
                self.drive_ready(token).expect("a cold miss completes");
            }
            Get::Hit(_) => panic!("a never-read page cannot hit"),
            Get::Busy => {
                self.pool.poll();
            }
        }
    }
}

fn main() {
    let base = Arm::new(BASE_FRAMES, "miss-table-base");
    let candidate = Arm::new(CANDIDATE_FRAMES, "miss-table-candidate");

    let samples = run_paired(
        "miss_table_pending_index",
        REPS,
        ITERS_PER_REP,
        || base.cold_miss(),
        || candidate.cold_miss(),
    );

    let gate = ratio_gate(&samples, BOOTSTRAP_RESAMPLES);
    let path =
        write_samples(Path::new("target/bench-samples"), &samples).expect("write samples CSV");
    println!(
        "miss_table_pending_index: pairs {REPS}, frames {BASE_FRAMES} vs {CANDIDATE_FRAMES}, ratio geomean {:.4}, ci95 upper {:.4}, samples {}",
        gate.ratio_geomean,
        gate.ratio_ci95_upper,
        path.display()
    );
}
