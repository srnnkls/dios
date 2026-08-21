//! `prefetch_window`: the plan-prefetch existence experiment. A 32-page cold
//! fragmented drain through the composed [`Pool`] over the real cfg-selected
//! driver: base arm is the pure demand cursor (`get` → `poll` → `ready`,
//! QD1 by construction); candidate arm runs the same cursor behind a 16-page
//! fire-and-forget lookahead — `get` with the [`PendingToken`] immediately
//! dropped, consumption coalescing via the miss-table singleflight. The
//! workload stays below the frame budget so no claim waits on epoch-matured
//! reclamation (the pressured regime is a recorded FAIL, not this gate —
//! see the plan); the end-of-run Busy assertions pin that precondition.
//! Registered buffers pin memory, so `FRAMES` must fit `RLIMIT_MEMLOCK`
//! (8 MiB on the pinned host). Binding on the pinned Linux `io_uring` host;
//! advisory on macOS, where the eager-inline backend executes every enqueued
//! read serially at `poll` and no overlap exists. The gate (ci95 upper
//! ≤ 0.5) is asserted by the shared compare harness, never in-bench.
//! Plan: `benches/plans/prefetch_window.md`.

use std::cell::Cell;
use std::hint::black_box;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use dios::bench::{ratio_gate, run_paired, write_samples};
use dios::driver::Driver;
use dios::{DirectIo, FileId, Get, PageId, PendingToken, Pool, ReaderCtx, ReadyResult};

const GRANULE: u32 = 4096;
const FILE_GRANULES: u32 = 65_536;
const FRAMES: u32 = 1_984;
const WINDOW: usize = 32;
const LOOKAHEAD: usize = 16;
const MAX_INFLIGHT_READS: u32 = 34;
const REPS: u32 = 30;
const ITERS_PER_REP: u32 = 1;
const BOOTSTRAP_RESAMPLES: u32 = 10_000;
const READY_POLLS_MAX: u32 = 1_000_000;
const BUSY_RETRIES_MAX: u32 = 1_000_000;

static PREFETCH_ISSUED: AtomicU64 = AtomicU64::new(0);
static PREFETCH_BUSY: AtomicU64 = AtomicU64::new(0);
static DEMAND_BUSY: AtomicU64 = AtomicU64::new(0);

fn granule_fill(granule_idx: u32) -> u8 {
    u8::try_from(granule_idx & 0xFF).expect("a masked byte fits u8")
}

fn next_page(state: &Cell<u64>, file: FileId) -> PageId {
    let drawn = state.get();
    state.set(drawn + 1);
    let granule_idx =
        u32::try_from((drawn * 75_193) % u64::from(FILE_GRANULES)).expect("index below span");
    PageId::new(file, granule_idx)
}

fn temp_path() -> PathBuf {
    let mut path =
        std::option_env!("CARGO_TARGET_TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
    std::fs::create_dir_all(&path).expect("target tmp dir");
    path.push(format!("dios-prefetch-window-{}", std::process::id()));
    path
}

fn preallocated_file(path: &Path) {
    const CHUNK_GRANULES: u32 = 256;
    let mut chunk = vec![0u8; (GRANULE * CHUNK_GRANULES) as usize];
    let file = std::fs::File::create(path).expect("create the prefetch-window file");
    for chunk_idx in 0..FILE_GRANULES / CHUNK_GRANULES {
        for k in 0..CHUNK_GRANULES {
            let start = (k * GRANULE) as usize;
            chunk[start..start + GRANULE as usize]
                .fill(granule_fill(chunk_idx * CHUNK_GRANULES + k));
        }
        file.write_all_at(
            &chunk,
            u64::from(chunk_idx) * u64::from(GRANULE) * u64::from(CHUNK_GRANULES),
        )
        .expect("write a chunk of granules");
    }
    file.sync_all().expect("fsync the preallocated file");
}

fn consume_landed(guard: &dios::FrameGuard<'_>, page: PageId) {
    assert_eq!(
        guard.len(),
        GRANULE as usize,
        "a frame guard borrows the whole granule"
    );
    assert_eq!(
        guard[0],
        granule_fill(page.granule_idx()),
        "the landed byte identifies the drained page"
    );
    black_box(guard[0]);
}

fn drive_ready<'pool>(
    pool: &'pool Pool<Driver>,
    reader: &'pool ReaderCtx,
    token: PendingToken,
) -> dios::FrameGuard<'pool> {
    let mut token = token;
    for _ in 0..READY_POLLS_MAX {
        match pool.ready(reader, token) {
            ReadyResult::Ready(guard) => return guard,
            ReadyResult::NotYet(handed_back) => {
                token = handed_back;
                pool.poll();
            }
            ReadyResult::Err(err) => panic!("a cold in-range read cannot fail: {err}"),
        }
    }
    panic!("a pending read never readied within the poll budget");
}

fn consume_page(pool: &Pool<Driver>, reader: &ReaderCtx, page: PageId) {
    for _ in 0..BUSY_RETRIES_MAX {
        match pool.get(reader, page).expect("the drained file stays live") {
            Get::Hit(guard) => {
                consume_landed(&guard, page);
                return;
            }
            Get::Pending(token) => {
                let guard = drive_ready(pool, reader, token);
                consume_landed(&guard, page);
                return;
            }
            Get::Busy => {
                DEMAND_BUSY.fetch_add(1, Ordering::Relaxed);
                pool.poll();
            }
        }
    }
    panic!("a demand get never admitted within the retry budget");
}

fn demand_drain(pool: &Pool<Driver>, reader: &ReaderCtx, stream: &Cell<u64>, file: FileId) {
    for _ in 0..WINDOW {
        consume_page(pool, reader, next_page(stream, file));
    }
}

fn prefetch_drain(pool: &Pool<Driver>, reader: &ReaderCtx, stream: &Cell<u64>, file: FileId) {
    let pages: [PageId; WINDOW] = std::array::from_fn(|_| next_page(stream, file));
    let mut issued = 0usize;
    for cursor in 0..WINDOW {
        while issued < WINDOW && issued < cursor + LOOKAHEAD {
            match pool
                .get(reader, pages[issued])
                .expect("the drained file stays live")
            {
                Get::Pending(token) => drop(token),
                Get::Hit(guard) => drop(guard),
                Get::Busy => {
                    PREFETCH_BUSY.fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
            PREFETCH_ISSUED.fetch_add(1, Ordering::Relaxed);
            issued += 1;
        }
        consume_page(pool, reader, pages[cursor]);
    }
}

fn main() {
    let path = temp_path();
    preallocated_file(&path);

    let pool = Pool::builder()
        .frame_count(FRAMES)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(2)
        .max_inflight_reads(MAX_INFLIGHT_READS)
        .miss_headroom(MAX_INFLIGHT_READS * 3)
        .build()
        .expect("watermark-satisfying pool composes over the real driver");
    let file = pool
        .open(&path, DirectIo::Preferred)
        .expect("open the drained file");
    let reader = pool.register_reader().expect("first reader slot");
    let stream = Cell::new(0u64);

    let samples = run_paired(
        "prefetch_window",
        REPS,
        ITERS_PER_REP,
        || demand_drain(&pool, &reader, &stream, file),
        || prefetch_drain(&pool, &reader, &stream, file),
    );

    let gate = ratio_gate(&samples, BOOTSTRAP_RESAMPLES);
    let out =
        write_samples(Path::new("target/bench-samples"), &samples).expect("write samples CSV");
    println!(
        "prefetch_window: pairs {REPS}, ratio geomean {:.4}, ci95 upper {:.4}, samples {}",
        gate.ratio_geomean,
        gate.ratio_ci95_upper,
        out.display()
    );

    let pages_per_arm = u64::from(REPS) * u64::from(ITERS_PER_REP) * WINDOW as u64;
    assert!(
        PREFETCH_ISSUED.load(Ordering::Relaxed) >= pages_per_arm,
        "the lookahead covered every candidate page"
    );
    assert!(
        PREFETCH_BUSY.load(Ordering::Relaxed) <= pages_per_arm / 8,
        "an unpressured claim rarely goes Busy — the workload outgrew the frame budget"
    );
    assert!(
        DEMAND_BUSY.load(Ordering::Relaxed) <= pages_per_arm / 8,
        "an unpressured demand get rarely goes Busy — the workload outgrew the frame budget"
    );

    let _ = std::fs::remove_file(&path);
}
