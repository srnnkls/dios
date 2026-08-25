use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use dios::{DirectIo, Get, PageId, PendingToken, Pool, ReaderCtx, ReadyResult};

#[path = "pinned_frame_retention/artifacts.rs"]
mod artifacts;
#[path = "pinned_frame_retention/common.rs"]
mod common;
#[path = "pinned_frame_retention/harness.rs"]
mod harness;
#[path = "pinned_frame_retention/platform.rs"]
mod platform;
#[path = "pinned_frame_retention/workloads.rs"]
mod workloads;

use common::{Lane, display_error, parse_number};

const SMOKE_OPERATIONS: u64 = 4;
const SMOKE_USEFUL_BYTES: u64 = 256;
const SMOKE_CHECKSUM: u64 = 8064;

static ALLOCATION_COUNT: AtomicU64 = AtomicU64::new(0);
static ALLOCATION_WINDOW: AtomicBool = AtomicBool::new(false);

struct CountingAllocator;

// SAFETY: every operation is forwarded unchanged to the system allocator.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ALLOCATION_WINDOW.load(Ordering::Relaxed) {
            ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: layout is forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if ALLOCATION_WINDOW.load(Ordering::Relaxed) {
            ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: all arguments are forwarded unchanged to the system allocator.
        unsafe { System.realloc(pointer, layout, size) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: both arguments are forwarded unchanged to the system allocator.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

fn main() -> ExitCode {
    let args = env::args()
        .skip(1)
        .filter(|argument| !argument.starts_with('-'))
        .collect::<Vec<_>>();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    match args {
        [command, lane, output] if command == "smoke" => {
            smoke(Lane::parse(lane)?, Path::new(output))
        }
        [command, lane, arm, input] if command == "probe" => {
            let lane = Lane::parse(lane)?;
            platform::pin_current(0)?;
            let measurement = workloads::measure(lane, arm, Path::new(input))?;
            println!(
                "lane={} arm={} operations={} elapsed_ns={} checksum={:016x}",
                lane.name(),
                arm,
                measurement.useful_operations,
                measurement.elapsed_ns,
                measurement.checksum,
            );
            Ok(())
        }
        [command, output] if command == "prepare-input" => {
            artifacts::prepare_input(Path::new(output))
        }
        [command, product, output] if command == "prepare-harness" => {
            harness::prepare(Path::new(product), Path::new(output))
        }
        [command] if command == "identity" => artifacts::print_identity(),
        [command, lane, input] if command == "host" => {
            artifacts::print_host(Lane::parse(lane)?, Path::new(input))
        }
        [command, lane, first, second, input, process, provenance] if command == "binding" => {
            artifacts::drive(
                Lane::parse(lane)?,
                Path::new(first),
                Path::new(second),
                Path::new(input),
                Path::new(process),
                Path::new(provenance),
            )
        }
        [command, lane, arm, pair, order, input, process, provenance] if command == "run" => {
            artifacts::run_process(
                Lane::parse(lane)?,
                arm,
                parse_number(pair, "pair")?,
                order,
                Path::new(input),
                Path::new(process),
                Path::new(provenance),
            )
        }
        [command, lane, process, paired, provenance] if command == "validate-pairs" => {
            artifacts::validate_pairs(
                Lane::parse(lane)?,
                Path::new(process),
                Path::new(paired),
                Path::new(provenance),
            )
        }
        [command, process, threshold, resamples] if command == "refusal-gate" => {
            artifacts::refusal_gate(
                Path::new(process),
                parse_number(threshold, "threshold")?,
                parse_number(resamples, "resamples")?,
            )
        }
        _ => Err(usage()),
    }
}

fn usage() -> String {
    "usage: pinned_frame_retention <smoke LANE OUTPUT|probe LANE ARM INPUT|prepare-input OUTPUT|prepare-harness PRODUCT OUTPUT|identity|host LANE INPUT|binding LANE FIRST_EXE SECOND_EXE INPUT PROCESS PROVENANCE|run LANE ARM PAIR ORDER INPUT PROCESS PROVENANCE|validate-pairs LANE PROCESS PAIRED PROVENANCE|refusal-gate PROCESS THRESHOLD RESAMPLES>".to_owned()
}

fn smoke(lane: Lane, output: &Path) -> Result<(), String> {
    let input = SmokeInput::create()?;
    let builder = Pool::builder()
        .frame_count(8)
        .granule(common::GRANULE_BYTES)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3);
    #[cfg(pfr_product_retention)]
    let builder = builder.max_retained_frames(0);
    let pool = builder
        .build()
        .map_err(|error| format!("build smoke pool: {error}"))?;
    let file = pool
        .open(&input.path, DirectIo::Disabled)
        .map_err(|error| format!("open smoke input: {error}"))?;
    let reader = pool.register_reader().map_err(display_error)?;
    let page = PageId::new(file, 0);
    drop(resolve_smoke_page(&pool, &reader, page)?);
    let (hits, checksum) = smoke_hits(&pool, &reader, page)?;
    validate_smoke(hits, checksum)?;
    let row = format!("{},smoke,4,256,4,8064", lane.name());
    fs::write(
        output,
        format!("lane,mode,operations,useful_bytes,hits,checksum\n{row}\n"),
    )
    .map_err(|error| format!("write {}: {error}", output.display()))
}

fn smoke_hits(pool: &Pool, reader: &ReaderCtx, page: PageId) -> Result<(u64, u64), String> {
    let mut checksum = 0_u64;
    let mut hits = 0_u64;
    for _ in 0..SMOKE_OPERATIONS {
        let Get::Hit(guard) = pool.get(reader, page).map_err(display_error)? else {
            return Err("warmed smoke page was not a hit".to_owned());
        };
        checksum += guard[..64].iter().map(|&byte| u64::from(byte)).sum::<u64>();
        hits += 1;
    }
    Ok((hits, checksum))
}

fn validate_smoke(hits: u64, checksum: u64) -> Result<(), String> {
    if hits != SMOKE_OPERATIONS
        || SMOKE_OPERATIONS * 64 != SMOKE_USEFUL_BYTES
        || checksum != SMOKE_CHECKSUM
    {
        return Err("PFR smoke witness differs from the frozen contract".to_owned());
    }
    Ok(())
}

fn resolve_smoke_page<'pool>(
    pool: &'pool Pool,
    reader: &'pool ReaderCtx,
    page: PageId,
) -> Result<dios::FrameGuard<'pool>, String> {
    match pool.get(reader, page).map_err(display_error)? {
        Get::Hit(guard) => Ok(guard),
        Get::Pending(token) => resolve_smoke_pending(pool, reader, token),
        Get::Busy => Err("smoke pool unexpectedly exhausted".to_owned()),
    }
}

fn resolve_smoke_pending<'pool>(
    pool: &'pool Pool,
    reader: &'pool ReaderCtx,
    mut token: PendingToken,
) -> Result<dios::FrameGuard<'pool>, String> {
    for _ in 0..1024 {
        match pool.ready(reader, token) {
            ReadyResult::Ready(guard) => return Ok(guard),
            ReadyResult::NotYet(returned) => token = returned,
            ReadyResult::Err(error) => return Err(format!("smoke read failed: {error}")),
        }
        pool.poll();
    }
    Err("smoke read exceeded its fixed poll bound".to_owned())
}

struct SmokeInput {
    path: PathBuf,
}

impl SmokeInput {
    fn create() -> Result<Self, String> {
        let path = env::temp_dir().join(format!("dios-pfr-smoke-{}", std::process::id()));
        let mut bytes = vec![0_u8; common::GRANULE_BYTES as usize];
        for (value, byte) in (0_u8..64).zip(&mut bytes) {
            *byte = value;
        }
        fs::write(&path, bytes).map_err(display_error)?;
        Ok(Self { path })
    }
}

impl Drop for SmokeInput {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn allocation_window_start() {
    ALLOCATION_COUNT.store(0, Ordering::Relaxed);
    ALLOCATION_WINDOW.store(true, Ordering::Release);
}

pub fn allocation_window_stop() -> u64 {
    ALLOCATION_WINDOW.store(false, Ordering::Release);
    allocation_count()
}

pub fn allocation_count_reset() {
    ALLOCATION_WINDOW.store(false, Ordering::Release);
    ALLOCATION_COUNT.store(0, Ordering::Relaxed);
}

pub fn allocation_window_enable() {
    ALLOCATION_WINDOW.store(true, Ordering::Release);
}

pub fn allocation_window_disable() {
    ALLOCATION_WINDOW.store(false, Ordering::Release);
}

pub fn allocation_count() -> u64 {
    ALLOCATION_COUNT.load(Ordering::Relaxed)
}
