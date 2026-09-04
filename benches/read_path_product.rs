use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;

use dios::PoolCompletionBatch;
use dios::{DirectIo, Get, PageId, PendingToken, Pool, ReaderCtx, ReadyResult};
#[cfg(dios_resident_hint)]
use dios::{ResidentFileLease, ResidentHint};
use serde_json::json;
use sha2::{Digest as _, Sha256};

const GRANULE_BYTES: u32 = 4096;
const POLLS_MAX: u32 = 1_000_000;
const PROCESS_HEADER: &str = "gate,lane,pair,order,arm,workload,iterations,checksum,allocations,source_commit,executable_sha256,cpu_set,manifest_sha256,runner_source_sha256,runner_build_sha256,elapsed_ns,ns_per_op";
const SMOKE_FOLD: &str = "xor_le_u64_rotate_v1";
const WARM_FRAME_COUNT: u32 = 520;
const CYCLING_FRAME_COUNT: u32 = 64;
const CYCLING_WORKING_SET: u32 = 96;
const RESIDENT_PAGE_COUNT: u32 = 512;
const BINDING_ITERATIONS: u64 = 32_768;
const INPUT_PAGE_PERIOD: u32 = 64;
const INPUT_PAGE_FACTOR: u32 = 17;
const INPUT_OFFSET_FACTOR: u32 = 73;
const INPUT_SEED: u32 = 41;
const INPUT_MODULUS: u32 = 251;

static ALLOCATION_COUNT: AtomicU64 = AtomicU64::new(0);
static ALLOCATION_WINDOW: AtomicBool = AtomicBool::new(false);

struct CountingAllocator;

// SAFETY: every allocation operation is forwarded unchanged to the system
// allocator; the fixed atomics only observe calls while the benchmark is armed.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ALLOCATION_WINDOW.load(Ordering::Relaxed) {
            ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: `layout` is forwarded unchanged to the system allocator.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Access {
    Ordinary,
    Hinted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunKind {
    Smoke,
    Binding,
}

#[derive(Clone, Copy, Debug)]
struct Lane {
    gate: &'static str,
    name: &'static str,
    arm: &'static str,
    workload: &'static str,
    access: Access,
    threads: u32,
    cycling: bool,
}

#[derive(Debug)]
struct RunRequest {
    kind: RunKind,
    lane: Lane,
    pair: u32,
    order: String,
    iterations: u64,
    schedule: Vec<u32>,
    input: PathBuf,
    output: PathBuf,
    manifest: PathBuf,
    proof: Option<PathBuf>,
}

#[derive(Debug)]
struct ProcessRow {
    lane: Lane,
    pair: u32,
    order: String,
    iterations: u64,
    checksum: u64,
    allocations: u64,
    source_commit: String,
    executable_sha256: String,
    cpu_set: String,
    manifest_sha256: String,
    runner_source_sha256: String,
    runner_build_sha256: String,
    elapsed_ns: u64,
}

#[derive(Debug)]
struct CyclingProof {
    actual_reads: u64,
    completed_reads: u64,
    evicted_frames: u64,
    reclaimed_frames: u64,
    reused_frames: u64,
    reuse_cycles: [ReuseCycle; 32],
}

#[derive(Clone, Copy, Debug)]
struct ReuseCycle {
    page: u32,
    reclaimed_frames: u32,
    backend_completions: u32,
    reused_frame: u64,
}

const EMPTY_REUSE_CYCLE: ReuseCycle = ReuseCycle {
    page: 0,
    reclaimed_frames: 0,
    backend_completions: 0,
    reused_frame: 0,
};

#[repr(align(128))]
struct ThreadAccumulator(AtomicU64);

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
    let request = parse_request(args)?;
    if request.kind == RunKind::Binding {
        require_clean_product_source()?;
    }
    if request.lane.access == Access::Hinted && !cfg!(dios_resident_hint) {
        return Err("this product source does not provide the candidate hint API".to_owned());
    }
    let source_commit = product_source_commit(request.kind)?;
    let executable_sha256 = sha256_path(&env::current_exe().map_err(display_error)?)?;
    let cpu_set = observed_cpu_set()?;
    let manifest_sha256 = sha256_path(&request.manifest)?;
    let runner_source_sha256 =
        sha256_path(&Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/read_path_product.rs"))
            .or_else(|_| {
                sha256_path(&Path::new(env!("CARGO_MANIFEST_DIR")).join("read_path_product.rs"))
            })?;
    let runner_build_sha256 = sha256_path(&Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs"))?;
    let (checksum, allocations, elapsed_ns, proof) = execute(&request)?;
    let row = ProcessRow {
        lane: request.lane,
        pair: request.pair,
        order: request.order.clone(),
        iterations: request.iterations,
        checksum,
        allocations,
        source_commit,
        executable_sha256,
        cpu_set,
        manifest_sha256,
        runner_source_sha256,
        runner_build_sha256,
        elapsed_ns,
    };
    append_row(&request.output, &row)?;
    if let Some(path) = &request.proof {
        let proof =
            proof.ok_or_else(|| "proof output is valid only for the cycling lane".to_owned())?;
        write_cycling_proof(path, &request, &proof)?;
    }
    Ok(())
}

fn parse_request(args: &[String]) -> Result<RunRequest, String> {
    match args {
        [mode, lane, arm, pair, order, fold, iterations, schedule, input, output, manifest]
            if mode == "smoke" =>
        {
            if fold != SMOKE_FOLD {
                return Err(format!("unknown full-granule fold {fold:?}"));
            }
            Ok(RunRequest {
                kind: RunKind::Smoke,
                lane: parse_lane(lane, arm)?,
                pair: parse_number(pair, "pair")?,
                order: parse_order(order)?,
                iterations: parse_number(iterations, "iterations")?,
                schedule: parse_schedule(schedule)?,
                input: PathBuf::from(input),
                output: PathBuf::from(output),
                manifest: PathBuf::from(manifest),
                proof: None,
            })
        }
        [mode, lane, arm, pair, order, fold, iterations, schedule, input, output, manifest, proof]
            if mode == "smoke" =>
        {
            if fold != SMOKE_FOLD {
                return Err(format!("unknown full-granule fold {fold:?}"));
            }
            Ok(RunRequest {
                kind: RunKind::Smoke,
                lane: parse_lane(lane, arm)?,
                pair: parse_number(pair, "pair")?,
                order: parse_order(order)?,
                iterations: parse_number(iterations, "iterations")?,
                schedule: parse_schedule(schedule)?,
                input: PathBuf::from(input),
                output: PathBuf::from(output),
                manifest: PathBuf::from(manifest),
                proof: Some(PathBuf::from(proof)),
            })
        }
        [mode, lane, arm, pair, order, input, output, manifest] if mode == "run" => Ok(RunRequest {
            kind: RunKind::Binding,
            lane: parse_lane(lane, arm)?,
            pair: parse_number(pair, "pair")?,
            order: parse_order(order)?,
            iterations: BINDING_ITERATIONS,
            schedule: binding_schedule(lane, arm),
            input: PathBuf::from(input),
            output: PathBuf::from(output),
            manifest: PathBuf::from(manifest),
            proof: None,
        }),
        _ => Err("usage: read_path_product <smoke LANE ARM PAIR ORDER FOLD ITERATIONS SCHEDULE INPUT OUTPUT MANIFEST [PROOF]|run LANE ARM PAIR ORDER INPUT OUTPUT MANIFEST>".to_owned()),
    }
}

fn parse_lane(lane: &str, arm: &str) -> Result<Lane, String> {
    let (gate, workload, access, threads, cycling) = match (lane, arm) {
        ("drp_g2_warm_ordinary", "base" | "candidate") => (
            "DRP-G2",
            "real_pool_driver_warm_ordinary_full_4096_byte_fold",
            Access::Ordinary,
            1,
            false,
        ),
        ("drp_g2_cycling_reuse", "base" | "candidate") => (
            "DRP-G2",
            "real_pool_driver_cycling_reuse_full_4096_byte_fold",
            Access::Ordinary,
            1,
            true,
        ),
        ("drp_g3_hint_materiality", "ordinary") => (
            "DRP-G3",
            "real_pool_driver_resident_ordinary_full_4096_byte_fold",
            Access::Ordinary,
            1,
            false,
        ),
        ("drp_g3_hint_materiality", "hinted") => (
            "DRP-G3",
            "real_pool_driver_resident_hinted_full_4096_byte_fold",
            Access::Hinted,
            1,
            false,
        ),
        _ => return parse_g4_lane(lane, arm),
    };
    Ok(Lane {
        gate,
        name: lane_static(lane)?,
        arm: arm_static(arm)?,
        workload,
        access,
        threads,
        cycling,
    })
}

fn parse_g4_lane(lane: &str, arm: &str) -> Result<Lane, String> {
    let (workload, access, threads) = match (lane, arm) {
        ("drp_g4_ordinary_base_8t", "base" | "candidate")
        | ("drp_g4_ordinary_scaling", "eight_threads") => (
            "real_pool_driver_shared_8_thread_ordinary_full_4096_byte_fold",
            Access::Ordinary,
            8,
        ),
        ("drp_g4_ordinary_scaling", "one_thread") => (
            "real_pool_driver_shared_1_thread_ordinary_full_4096_byte_fold",
            Access::Ordinary,
            1,
        ),
        ("drp_g4_hint_scaling", "one_thread") => (
            "real_pool_driver_shared_1_thread_hinted_full_4096_byte_fold",
            Access::Hinted,
            1,
        ),
        ("drp_g4_hint_scaling", "eight_threads") => (
            "real_pool_driver_shared_8_thread_hinted_full_4096_byte_fold",
            Access::Hinted,
            8,
        ),
        _ => return Err(format!("unknown frozen lane/arm {lane:?}/{arm:?}")),
    };
    Ok(Lane {
        gate: "DRP-G4",
        name: lane_static(lane)?,
        arm: arm_static(arm)?,
        workload,
        access,
        threads,
        cycling: false,
    })
}

fn lane_static(lane: &str) -> Result<&'static str, String> {
    match lane {
        "drp_g2_warm_ordinary" => Ok("drp_g2_warm_ordinary"),
        "drp_g2_cycling_reuse" => Ok("drp_g2_cycling_reuse"),
        "drp_g3_hint_materiality" => Ok("drp_g3_hint_materiality"),
        "drp_g4_ordinary_base_8t" => Ok("drp_g4_ordinary_base_8t"),
        "drp_g4_ordinary_scaling" => Ok("drp_g4_ordinary_scaling"),
        "drp_g4_hint_scaling" => Ok("drp_g4_hint_scaling"),
        _ => Err(format!("unknown frozen lane {lane:?}")),
    }
}

fn arm_static(arm: &str) -> Result<&'static str, String> {
    match arm {
        "base" => Ok("base"),
        "candidate" => Ok("candidate"),
        "ordinary" => Ok("ordinary"),
        "hinted" => Ok("hinted"),
        "one_thread" => Ok("one_thread"),
        "eight_threads" => Ok("eight_threads"),
        _ => Err(format!("unknown frozen arm {arm:?}")),
    }
}

fn binding_schedule(lane: &str, arm: &str) -> Vec<u32> {
    let count = if lane == "drp_g2_cycling_reuse" {
        CYCLING_WORKING_SET
    } else if lane.starts_with("drp_g4_") && arm == "one_thread" {
        64
    } else {
        RESIDENT_PAGE_COUNT
    };
    (0..count).collect()
}

fn parse_schedule(value: &str) -> Result<Vec<u32>, String> {
    let schedule = value
        .split(',')
        .map(|item| parse_number(item, "page schedule"))
        .collect::<Result<Vec<_>, _>>()?;
    if schedule.is_empty() || schedule.len() > 4096 {
        return Err("page schedule must contain 1..=4096 entries".to_owned());
    }
    Ok(schedule)
}

fn parse_order(value: &str) -> Result<String, String> {
    if value == "base-candidate" || value == "candidate-base" {
        Ok(value.to_owned())
    } else {
        Err(format!("unknown process order {value:?}"))
    }
}

fn parse_number<T: std::str::FromStr>(value: &str, field: &str) -> Result<T, String> {
    value
        .parse()
        .map_err(|_| format!("invalid {field} value {value:?}"))
}

fn execute(request: &RunRequest) -> Result<(u64, u64, u64, Option<CyclingProof>), String> {
    if request.iterations == 0 {
        return Err("iterations must be positive".to_owned());
    }
    let highest_page = request
        .schedule
        .iter()
        .copied()
        .max()
        .expect("validated schedule is nonempty");
    validate_input(&request.input, highest_page)?;
    if request.kind == RunKind::Binding {
        validate_binding_input(&request.input, binding_page_count(request.lane.name))?;
    }
    let frame_count = if request.lane.cycling {
        CYCLING_FRAME_COUNT
    } else {
        WARM_FRAME_COUNT.max(highest_page.saturating_add(8))
    };
    let pool = Arc::new(build_pool(frame_count)?);
    let direct_io = match request.kind {
        RunKind::Smoke => DirectIo::Disabled,
        RunKind::Binding => DirectIo::Required,
    };
    let file = pool
        .open(&request.input, direct_io)
        .map_err(|error| format!("open product input: {error}"))?;
    if request.proof.is_some() {
        return execute_cycling_proof(&pool, file, request);
    }
    warm_pool(&pool, file, request)?;
    let (checksum, allocations, elapsed_ns) = measure(&pool, file, request)?;
    Ok((checksum, allocations, elapsed_ns, None))
}

fn execute_cycling_proof(
    pool: &Arc<Pool>,
    file: dios::FileId,
    request: &RunRequest,
) -> Result<(u64, u64, u64, Option<CyclingProof>), String> {
    if !request.lane.cycling
        || request.iterations != 192
        || request.schedule != (0..CYCLING_WORKING_SET).collect::<Vec<_>>()
    {
        return Err(
            "cycling proof requires exactly 64 frames, 96 pages, and 192 iterations".to_owned(),
        );
    }
    let reader = pool
        .register_reader()
        .map_err(|error| format!("register proof reader: {error:?}"))?;
    let mut counters = CyclingProof {
        actual_reads: 0,
        completed_reads: 0,
        evicted_frames: 0,
        reclaimed_frames: 0,
        reused_frames: 0,
        reuse_cycles: [EMPTY_REUSE_CYCLE; 32],
    };
    let mut checksum = 0_u64;
    pool.wake_handle().wake();
    assert_eq!(
        pool.poll(),
        0,
        "proof priming has no admitted read to complete"
    );
    ALLOCATION_COUNT.store(0, Ordering::Relaxed);
    ALLOCATION_WINDOW.store(true, Ordering::Release);
    let started = Instant::now();
    cycling_fill(pool, &reader, file, request, &mut counters, &mut checksum)?;
    for (cycle_index, page_index) in (CYCLING_FRAME_COUNT..CYCLING_WORKING_SET).enumerate() {
        let page = PageId::new(file, request.schedule[page_index as usize]);
        counters.reuse_cycles[cycle_index] =
            cycling_reuse(pool, &reader, page, &mut counters, &mut checksum)?;
    }
    let elapsed_ns = u64::try_from(started.elapsed().as_nanos()).map_err(display_error)?;
    ALLOCATION_WINDOW.store(false, Ordering::Release);
    let allocations = ALLOCATION_COUNT.load(Ordering::Relaxed);
    validate_cycling_proof(&counters)?;
    Ok((checksum, allocations, elapsed_ns, Some(counters)))
}

fn cycling_fill(
    pool: &Pool,
    reader: &ReaderCtx,
    file: dios::FileId,
    request: &RunRequest,
    counters: &mut CyclingProof,
    checksum: &mut u64,
) -> Result<(), String> {
    for page_index in 0..CYCLING_FRAME_COUNT {
        let page = PageId::new(file, request.schedule[page_index as usize]);
        for _ in 0..2 {
            let guard = cycling_resolve(pool, reader, page, counters)?;
            *checksum = checksum.rotate_left(7) ^ fold_frame(&guard);
        }
    }
    Ok(())
}

fn cycling_resolve<'pool>(
    pool: &'pool Pool,
    reader: &'pool ReaderCtx,
    page: PageId,
    counters: &mut CyclingProof,
) -> Result<dios::FrameGuard<'pool>, String> {
    let mut completions = PoolCompletionBatch::with_capacity(0);
    match pool.get(reader, page).map_err(|error| error.to_string())? {
        Get::Hit(guard) => Ok(guard),
        Get::Pending(mut token) => {
            counters.actual_reads += 1;
            for _ in 0..POLLS_MAX {
                match pool.ready(reader, token) {
                    ReadyResult::Ready(guard) => return Ok(guard),
                    ReadyResult::NotYet(returned) => token = returned,
                    ReadyResult::Err(error) => {
                        return Err(format!("product read failed: {error}"));
                    }
                }
                let report = pool.poll_report(&mut completions);
                counters.completed_reads += u64::from(report.backend_completions());
                counters.reclaimed_frames += u64::from(report.reclaimed_frames());
            }
            Err("cycling read exceeded the fixed poll bound".to_owned())
        }
        Get::Busy => Err("cycling fill unexpectedly exhausted its frame pool".to_owned()),
    }
}

fn cycling_reuse(
    pool: &Pool,
    reader: &ReaderCtx,
    page: PageId,
    counters: &mut CyclingProof,
    checksum: &mut u64,
) -> Result<ReuseCycle, String> {
    match pool.get(reader, page).map_err(|error| error.to_string())? {
        Get::Busy => {}
        Get::Hit(_) | Get::Pending(_) => {
            return Err("new cycling page did not produce controlled Busy".to_owned());
        }
    }
    counters.evicted_frames += 1;
    let reclaimed_frames = cycling_observe_reclaim(pool, counters)?;
    let token = match pool.get(reader, page).map_err(|error| error.to_string())? {
        Get::Pending(token) => token,
        Get::Busy | Get::Hit(_) => {
            return Err("reclaimed cycling page did not transition to Pending".to_owned());
        }
    };
    counters.actual_reads += 1;
    let (guard, backend_completions) = cycling_complete(pool, reader, token, counters)?;
    let reused_frame = u64::try_from(guard.as_ptr() as usize).map_err(display_error)?;
    *checksum = checksum.rotate_left(7) ^ fold_frame(&guard);
    drop(guard);
    let guard = match pool.get(reader, page).map_err(|error| error.to_string())? {
        Get::Hit(guard) => guard,
        Get::Busy | Get::Pending(_) => {
            return Err("reused cycling page was not resident".to_owned());
        }
    };
    *checksum = checksum.rotate_left(7) ^ fold_frame(&guard);
    counters.reused_frames += 1;
    Ok(ReuseCycle {
        page: page.granule_idx(),
        reclaimed_frames,
        backend_completions,
        reused_frame,
    })
}

fn cycling_observe_reclaim(pool: &Pool, counters: &mut CyclingProof) -> Result<u32, String> {
    let mut completions = PoolCompletionBatch::with_capacity(0);
    let mut reclaimed_frames = 0_u32;
    for _ in 0..POLLS_MAX {
        let report = pool.poll_report(&mut completions);
        if report.backend_completions() != 0 {
            return Err("Busy cycling miss had an admitted backend read".to_owned());
        }
        reclaimed_frames += report.reclaimed_frames();
        counters.reclaimed_frames += u64::from(report.reclaimed_frames());
        if reclaimed_frames > 0 {
            return Ok(reclaimed_frames);
        }
    }
    Err("Busy cycling miss did not produce observed reclamation".to_owned())
}

fn cycling_complete<'pool>(
    pool: &'pool Pool,
    reader: &'pool ReaderCtx,
    mut token: PendingToken,
    counters: &mut CyclingProof,
) -> Result<(dios::FrameGuard<'pool>, u32), String> {
    let mut completions = PoolCompletionBatch::with_capacity(0);
    let mut backend_completions = 0_u32;
    for _ in 0..POLLS_MAX {
        match pool.ready(reader, token) {
            ReadyResult::Ready(guard) => return Ok((guard, backend_completions)),
            ReadyResult::NotYet(returned) => token = returned,
            ReadyResult::Err(error) => return Err(format!("product read failed: {error}")),
        }
        let report = pool.poll_report(&mut completions);
        backend_completions += report.backend_completions();
        counters.completed_reads += u64::from(report.backend_completions());
        counters.reclaimed_frames += u64::from(report.reclaimed_frames());
    }
    Err("cycling reuse read exceeded the fixed poll bound".to_owned())
}

fn validate_cycling_proof(proof: &CyclingProof) -> Result<(), String> {
    let expected = [
        ("actual reads", proof.actual_reads, 96),
        ("completed reads", proof.completed_reads, 96),
        ("CLOCK evictions", proof.evicted_frames, 32),
        ("reclaimed frames", proof.reclaimed_frames, 32),
        ("reused frames", proof.reused_frames, 32),
    ];
    for (name, observed, exact) in expected {
        if observed != exact {
            return Err(format!(
                "cycling proof observed {observed} {name}, expected {exact}"
            ));
        }
    }
    Ok(())
}

fn binding_page_count(lane: &str) -> u32 {
    if lane == "drp_g2_cycling_reuse" {
        CYCLING_WORKING_SET
    } else {
        RESIDENT_PAGE_COUNT
    }
}

fn validate_binding_input(path: &Path, page_count: u32) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let required = usize::try_from(page_count)
        .expect("fixed page count fits usize")
        .checked_mul(GRANULE_BYTES as usize)
        .expect("fixed binding input size fits usize");
    if bytes.len() < required {
        return Err(format!(
            "binding input has {} bytes, requires {required}",
            bytes.len()
        ));
    }
    for page in 0..page_count {
        let page_base =
            usize::try_from(page).expect("fixed page index fits usize") * GRANULE_BYTES as usize;
        let logical_page = page % INPUT_PAGE_PERIOD;
        for offset in 0..GRANULE_BYTES {
            let expected =
                (logical_page * INPUT_PAGE_FACTOR + offset * INPUT_OFFSET_FACTOR + INPUT_SEED)
                    % INPUT_MODULUS;
            if bytes[page_base + offset as usize] != expected as u8 {
                return Err(format!(
                    "binding input differs from the fixed seed at page {page} byte {offset}"
                ));
            }
        }
    }
    Ok(())
}

fn validate_input(path: &Path, highest_page: u32) -> Result<(), String> {
    let bytes = fs::metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?
        .len();
    let required = u64::from(highest_page.saturating_add(1)) * u64::from(GRANULE_BYTES);
    if bytes < required {
        return Err(format!(
            "input has {bytes} bytes, requires at least {required}"
        ));
    }
    Ok(())
}

fn build_pool(frame_count: u32) -> Result<Pool, String> {
    assert!(
        frame_count >= CYCLING_FRAME_COUNT,
        "benchmark pool has a fixed positive frame bound"
    );
    let pool = Pool::builder()
        .frame_count(frame_count)
        .granule(GRANULE_BYTES)
        .max_concurrent_readers(8)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .registration_posture(dios::bench::registration_policy_from_env()?)
        .build()
        .map_err(|error| format!("build product pool: {error}"))?;
    eprintln!(
        "read_path_product: registration posture {:?}, arena locked {}",
        pool.registration_posture(),
        pool.arena_locked()
    );
    Ok(pool)
}

fn warm_pool(pool: &Pool, file: dios::FileId, request: &RunRequest) -> Result<(), String> {
    let reader = pool
        .register_reader()
        .map_err(|error| format!("register warmup reader: {error:?}"))?;
    if request.lane.cycling {
        for _ in 0..2 {
            for granule in 0..CYCLING_WORKING_SET {
                drop(resolve_page(pool, &reader, PageId::new(file, granule))?);
            }
        }
    } else {
        let mut seen = vec![false; pool_schedule_bound(&request.schedule)?];
        for &granule in &request.schedule {
            let index = usize::try_from(granule).map_err(display_error)?;
            if !seen[index] {
                drop(resolve_page(pool, &reader, PageId::new(file, granule))?);
                seen[index] = true;
            }
        }
    }
    Ok(())
}

fn pool_schedule_bound(schedule: &[u32]) -> Result<usize, String> {
    let maximum = schedule
        .iter()
        .copied()
        .max()
        .expect("schedule is nonempty");
    usize::try_from(maximum.saturating_add(1)).map_err(display_error)
}

fn resolve_page<'pool>(
    pool: &'pool Pool,
    reader: &'pool ReaderCtx,
    page: PageId,
) -> Result<dios::FrameGuard<'pool>, String> {
    for _ in 0..POLLS_MAX {
        match pool.get(reader, page).map_err(|error| error.to_string())? {
            Get::Hit(guard) => return Ok(guard),
            Get::Pending(token) => return resolve_pending(pool, reader, token),
            Get::Busy => {
                pool.poll();
            }
        }
    }
    Err("pool remained busy beyond the fixed poll bound".to_owned())
}

fn resolve_pending<'pool>(
    pool: &'pool Pool,
    reader: &'pool ReaderCtx,
    mut token: PendingToken,
) -> Result<dios::FrameGuard<'pool>, String> {
    for _ in 0..POLLS_MAX {
        match pool.ready(reader, token) {
            ReadyResult::Ready(guard) => return Ok(guard),
            ReadyResult::NotYet(returned) => {
                token = returned;
                pool.poll();
            }
            ReadyResult::Err(error) => return Err(format!("product read failed: {error}")),
        }
    }
    Err("product read did not complete within the fixed poll bound".to_owned())
}

fn measure(
    pool: &Arc<Pool>,
    file: dios::FileId,
    request: &RunRequest,
) -> Result<(u64, u64, u64), String> {
    let thread_count = request.lane.threads;
    let schedule = Arc::new(request.schedule.clone());
    let accumulator_count = usize::try_from(thread_count).map_err(display_error)?;
    let results: Arc<[ThreadAccumulator]> = (0..accumulator_count)
        .map(|_| ThreadAccumulator(AtomicU64::new(0)))
        .collect::<Vec<_>>()
        .into();
    let phase = Arc::new(Barrier::new(thread_count as usize + 1));
    let mut workers = Vec::with_capacity(thread_count as usize);
    for thread_index in 0..thread_count {
        workers.push(spawn_worker(
            Arc::clone(pool),
            file,
            request.lane.access,
            Arc::clone(&schedule),
            request.iterations,
            thread_index,
            thread_count,
            Arc::clone(&results),
            Arc::clone(&phase),
        ));
    }
    phase.wait();
    ALLOCATION_COUNT.store(0, Ordering::Relaxed);
    ALLOCATION_WINDOW.store(true, Ordering::Release);
    let started = Instant::now();
    phase.wait();
    phase.wait();
    let elapsed_ns = u64::try_from(started.elapsed().as_nanos()).map_err(display_error)?;
    ALLOCATION_WINDOW.store(false, Ordering::Release);
    let allocations = ALLOCATION_COUNT.load(Ordering::Relaxed);
    join_workers(workers)?;
    Ok((fold_results(&results), allocations, elapsed_ns))
}

#[expect(
    clippy::too_many_arguments,
    reason = "the worker receives only fixed preallocated benchmark state"
)]
fn spawn_worker(
    pool: Arc<Pool>,
    file: dios::FileId,
    access: Access,
    schedule: Arc<Vec<u32>>,
    iterations: u64,
    thread_index: u32,
    thread_count: u32,
    results: Arc<[ThreadAccumulator]>,
    phase: Arc<Barrier>,
) -> thread::JoinHandle<Result<(), String>> {
    thread::spawn(move || {
        let reader = pool
            .register_reader()
            .map_err(|error| format!("register measured reader: {error:?}"))?;
        match access {
            Access::Ordinary => run_worker(&phase, || {
                ordinary_operations(
                    &pool,
                    &reader,
                    file,
                    &schedule,
                    iterations,
                    thread_index,
                    thread_count,
                    &results,
                )
            }),
            Access::Hinted => {
                #[cfg(dios_resident_hint)]
                {
                    let (lease, hints) = prepare_hints(&pool, file, &schedule)?;
                    run_worker(&phase, || {
                        hinted_operations(
                            &pool,
                            &reader,
                            file,
                            &lease,
                            &hints,
                            &schedule,
                            iterations,
                            thread_index,
                            thread_count,
                            &results,
                        )
                    })
                }
                #[cfg(not(dios_resident_hint))]
                {
                    Err("hinted access is unavailable in this product source".to_owned())
                }
            }
        }
    })
}

fn run_worker(
    operation_phase: &Barrier,
    operation: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    operation_phase.wait();
    operation_phase.wait();
    let outcome = operation();
    operation_phase.wait();
    outcome
}

#[cfg(dios_resident_hint)]
fn prepare_hints(
    pool: &Pool,
    file: dios::FileId,
    schedule: &[u32],
) -> Result<(ResidentFileLease, Vec<ResidentHint>), String> {
    let lease = pool
        .lease_file(file)
        .map_err(|error| format!("prepare resident lease: {error:?}"))?;
    let hints = schedule
        .iter()
        .map(|&granule| {
            pool.resident_hint(&lease, PageId::new(file, granule))
                .ok_or_else(|| format!("page {granule} is not resident before timing"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((lease, hints))
}

#[expect(
    clippy::too_many_arguments,
    reason = "the hot loop takes primitive preallocated benchmark state"
)]
fn ordinary_operations(
    pool: &Pool,
    reader: &ReaderCtx,
    file: dios::FileId,
    schedule: &[u32],
    iterations: u64,
    thread_index: u32,
    thread_count: u32,
    results: &[ThreadAccumulator],
) -> Result<(), String> {
    let mut ordinal = u64::from(thread_index);
    let mut accumulator = 0_u64;
    while ordinal < iterations {
        let schedule_index =
            usize::try_from(ordinal % schedule.len() as u64).map_err(display_error)?;
        let page = PageId::new(file, schedule[schedule_index]);
        let guard = resolve_page(pool, reader, page)?;
        let rotation = u32::try_from((iterations - ordinal - 1) % 64).map_err(display_error)? * 7;
        accumulator ^= fold_frame(&guard).rotate_left(rotation % 64);
        ordinal += u64::from(thread_count);
    }
    publish_result(results, thread_index, accumulator)
}

#[cfg(dios_resident_hint)]
#[expect(
    clippy::too_many_arguments,
    reason = "the hot loop takes primitive preallocated benchmark state"
)]
fn hinted_operations(
    pool: &Pool,
    reader: &ReaderCtx,
    file: dios::FileId,
    lease: &ResidentFileLease,
    hints: &[ResidentHint],
    schedule: &[u32],
    iterations: u64,
    thread_index: u32,
    thread_count: u32,
    results: &[ThreadAccumulator],
) -> Result<(), String> {
    let mut ordinal = u64::from(thread_index);
    let mut accumulator = 0_u64;
    while ordinal < iterations {
        let schedule_index =
            usize::try_from(ordinal % schedule.len() as u64).map_err(display_error)?;
        let page = PageId::new(file, schedule[schedule_index]);
        let guard = measured_get_hinted(pool, reader, page, lease, hints[schedule_index])?;
        let rotation = u32::try_from((iterations - ordinal - 1) % 64).map_err(display_error)? * 7;
        accumulator ^= fold_frame(&guard).rotate_left(rotation % 64);
        ordinal += u64::from(thread_count);
    }
    publish_result(results, thread_index, accumulator)
}

#[cfg(dios_resident_hint)]
fn measured_get_hinted<'pool>(
    pool: &'pool Pool,
    reader: &'pool ReaderCtx,
    page: PageId,
    lease: &ResidentFileLease,
    hint: ResidentHint,
) -> Result<dios::FrameGuard<'pool>, String> {
    match pool
        .get_with_hint(reader, lease, page, Some(hint))
        .map_err(|error| error.to_string())?
    {
        Get::Hit(guard) => Ok(guard),
        Get::Pending(token) => resolve_pending(pool, reader, token),
        Get::Busy => resolve_page(pool, reader, page),
    }
}

fn publish_result(
    results: &[ThreadAccumulator],
    thread_index: u32,
    accumulator: u64,
) -> Result<(), String> {
    let result_index = usize::try_from(thread_index).map_err(display_error)?;
    results[result_index]
        .0
        .store(accumulator, Ordering::Relaxed);
    Ok(())
}

fn fold_frame(bytes: &[u8]) -> u64 {
    assert_eq!(
        bytes.len(),
        GRANULE_BYTES as usize,
        "binding reads one complete granule"
    );
    bytes.chunks_exact(8).fold(0_u64, |checksum, chunk| {
        let word = u64::from_le_bytes(chunk.try_into().expect("granule chunks are eight bytes"));
        checksum.rotate_left(1) ^ black_box(word)
    })
}

fn fold_results(results: &[ThreadAccumulator]) -> u64 {
    results.iter().fold(0_u64, |checksum, result| {
        checksum ^ result.0.load(Ordering::Relaxed)
    })
}

fn join_workers(workers: Vec<thread::JoinHandle<Result<(), String>>>) -> Result<(), String> {
    for worker in workers {
        match worker.join() {
            Ok(result) => result?,
            Err(_) => return Err("measured worker panicked".to_owned()),
        }
    }
    Ok(())
}

fn append_row(path: &Path, row: &ProcessRow) -> Result<(), String> {
    let write_header = path.metadata().map_or(true, |metadata| metadata.len() == 0);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    if write_header {
        writeln!(file, "{PROCESS_HEADER}").map_err(display_error)?;
    }
    let ns_per_op = row.elapsed_ns / row.iterations;
    let ns_per_op_fraction = (row.elapsed_ns % row.iterations) * 1_000_000 / row.iterations;
    let cpu_set = csv_field(&row.cpu_set)?;
    writeln!(
        file,
        "{},{},{},{},{},{},{},{:016x},{},{},{},{},{},{},{},{},{}.{:06}",
        row.lane.gate,
        row.lane.name,
        row.pair,
        row.order,
        row.lane.arm,
        row.lane.workload,
        row.iterations,
        row.checksum,
        row.allocations,
        row.source_commit,
        row.executable_sha256,
        cpu_set,
        row.manifest_sha256,
        row.runner_source_sha256,
        row.runner_build_sha256,
        row.elapsed_ns,
        ns_per_op,
        ns_per_op_fraction,
    )
    .map_err(display_error)
}

fn write_cycling_proof(
    path: &Path,
    request: &RunRequest,
    proof: &CyclingProof,
) -> Result<(), String> {
    let reuse_cycles = proof
        .reuse_cycles
        .iter()
        .map(|cycle| {
            json!({
                "page": cycle.page,
                "busy_without_pending": true,
                "reclaimed_frames": cycle.reclaimed_frames,
                "post_reclaim_get": "pending",
                "backend_completions": cycle.backend_completions,
                "reused_frame": cycle.reused_frame,
            })
        })
        .collect::<Vec<_>>();
    let value = json!({
        "schema": "dios-drp009-cycling-proof-v1",
        "manifest_sha256": sha256_path(&request.manifest)?,
        "process_csv_sha256": sha256_path(&request.output)?,
        "dimensions": {
            "frame_count": CYCLING_FRAME_COUNT,
            "working_set": CYCLING_WORKING_SET,
            "iterations": request.iterations,
        },
        "expected": {
            "completed_reads": 96,
            "evicted_frames": 32,
            "reclaimed_frames": 32,
            "reused_frames": 32,
            "successful_epoch_advances": 2,
        },
        "observed": {
            "actual_reads": proof.actual_reads,
            "completed_reads": proof.completed_reads,
            "evicted_frames": proof.evicted_frames,
            "reclaimed_frames": proof.reclaimed_frames,
            "reused_frames": proof.reused_frames,
            "two_epoch_maturity": "inferred_from_observed_reclaim",
            "reuse_cycles": reuse_cycles,
        }
    });
    let bytes = serde_json::to_vec_pretty(&value).map_err(display_error)?;
    fs::write(path, bytes).map_err(display_error)
}

fn csv_field(value: &str) -> Result<String, String> {
    if value.contains(['"', '\n', '\r']) {
        return Err("CPU-set identity contains an unsupported CSV character".to_owned());
    }
    if value.contains(',') {
        Ok(format!("\"{value}\""))
    } else {
        Ok(value.to_owned())
    }
}

fn observed_cpu_set() -> Result<String, String> {
    let supplied = env::var("DIOS_BENCH_CPU_SET").ok();
    #[cfg(target_os = "linux")]
    let observed = fs::read_to_string("/proc/self/status")
        .map_err(display_error)?
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:").map(str::trim))
        .ok_or_else(|| "Linux did not expose Cpus_allowed_list".to_owned())?
        .to_owned();
    #[cfg(not(target_os = "linux"))]
    let observed = "unbound".to_owned();
    if let Some(expected) = supplied {
        if expected.is_empty() {
            return Err("DIOS_BENCH_CPU_SET cannot be empty".to_owned());
        }
        #[cfg(target_os = "linux")]
        if expected != observed {
            return Err(format!(
                "supplied CPU set {expected:?} does not match observed {observed:?}"
            ));
        }
        Ok(expected)
    } else {
        Ok(observed)
    }
}

fn require_clean_product_source() -> Result<(), String> {
    let output = Command::new("git")
        .args(["status", "--porcelain=v1"])
        .current_dir(product_root())
        .output()
        .map_err(display_error)?;
    if !output.status.success() {
        return Err("git status failed while validating product identity".to_owned());
    }
    if !output.stdout.is_empty() {
        return Err("binding runs require a clean product worktree".to_owned());
    }
    Ok(())
}

fn product_source_commit(kind: RunKind) -> Result<String, String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(product_root())
        .output();
    let commit = match output {
        Ok(output) if output.status.success() => {
            String::from_utf8(output.stdout).map_err(display_error)?
        }
        Ok(_) | Err(_) if kind == RunKind::Smoke => env!("DIOS_PRODUCT_SOURCE_COMMIT").to_owned(),
        Ok(_) | Err(_) => {
            return Err("binding run cannot resolve the product source commit".to_owned());
        }
    };
    let commit = commit.trim().to_owned();
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("git returned an invalid product source commit".to_owned());
    }
    let commit = commit.to_ascii_lowercase();
    if commit != env!("DIOS_PRODUCT_SOURCE_COMMIT") {
        return Err(format!(
            "benchmark executable was built for {}, but product worktree is now {commit}",
            env!("DIOS_PRODUCT_SOURCE_COMMIT")
        ));
    }
    Ok(commit)
}

fn product_root() -> &'static Path {
    Path::new(env!("DIOS_PRODUCT_WORKTREE"))
}

fn sha256_path(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn display_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}
