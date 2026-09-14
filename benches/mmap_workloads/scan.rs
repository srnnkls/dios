use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use dios::{
    DirectIo, FileId, Get, PageId, PendingToken, Pool, Readahead, ReaderCtx, ReadyResult,
    RegistrationPolicy,
};
use serde_json::json;

use super::catalog::{Arm, GRANULE, POLLS_MAX, PageNumber, consume};
use super::fixture::{self, error};
use super::observe::{Observer, allocations_begin, allocations_end, nanoseconds};
use super::os::{self, Mapping};
use super::scan_config::{Config, Method};

pub(super) fn sample<const TRACE: bool>(
    input: &Path,
    text: &str,
    output: &Path,
) -> Result<(), String> {
    let config = Config::parse(text)?;
    os::pin(0).map_err(error)?;
    let mapping = Mapping::open(&input.join("pages.bin")).map_err(error)?;
    let cache = fixture::prepare(&mapping, config.lane, Arm::MmapSequential, 0).map_err(error)?;
    let expected = fixture::expected(config.lane, 0);
    let pool = if config.method == Method::Mmap {
        None
    } else {
        Some(pool(input, config)?)
    };
    let reader = pool
        .as_ref()
        .map(|(pool, _)| pool.register_reader())
        .transpose()
        .map_err(error)?;
    let pages = pool.as_ref().map_or_else(Vec::new, |(_, file)| {
        (0..config.requests_per_pass())
            .map(|index| PageId::new(*file, index))
            .collect()
    });
    let mut observer = Observer::<TRACE>::new(config.requests());
    os::cpu_ns().map_err(error)?;
    os::usage().map_err(error)?;
    allocations_begin();
    assert_eq!(allocations_end(), 0);
    let before = super::snapshot();
    super::sample_check_limit(config.lane, &before)?;
    let (elapsed_ns, cpu_ns, usage) = measure(
        &mapping,
        pool.as_ref(),
        reader.as_ref(),
        config,
        &pages,
        &mut observer,
    )?;
    let after = super::snapshot();
    if observer.counters.allocations != 0 || observer.counters.checksum != expected {
        return Err("scan checksum or zero-allocation witness failed".to_owned());
    }
    assert_eq!(observer.counters.operations, config.requests());
    let (io_mode, registration) = super::sample_posture(pool.as_ref());
    let row = json!({"schema": 1, "kind": "scan_geometry", "configuration": text,
        "config": config, "trace": TRACE, "runner_sha256": super::runner_hash(),
        "debug_assertions": cfg!(debug_assertions), "elapsed_ns": elapsed_ns, "cpu_ns": cpu_ns,
        "usage": usage, "expected_checksum": expected, "counters": observer.counters,
        "pages": config.lane.operations(), "requests": config.requests(),
        "useful_bytes": u64::from(config.lane.operations()) * u64::from(GRANULE),
        "cache_before": cache, "system_before": before, "system_after": after,
        "io_mode": io_mode, "registration": registration,
        "prefetch": super::sample_prefetch(pool.as_ref())});
    write(output, &row, &observer)
}

fn measure<const TRACE: bool>(
    mapping: &Mapping,
    pool: Option<&(Pool, FileId)>,
    reader: Option<&ReaderCtx>,
    config: Config,
    pages: &[PageId],
    observer: &mut Observer<TRACE>,
) -> Result<(u64, u64, os::Usage), String> {
    let usage = os::usage().map_err(error)?;
    let cpu = os::cpu_ns().map_err(error)?;
    observer.origin = Instant::now();
    allocations_begin();
    let result = if let Some((pool, _)) = pool {
        read_dios(
            pool,
            reader.expect("registered reader"),
            config,
            pages,
            observer,
        )
    } else {
        read_mmap(mapping, config, observer);
        Ok(())
    };
    observer.counters.allocations = allocations_end();
    let elapsed_ns = nanoseconds(observer.origin.elapsed());
    let cpu_ns = os::cpu_ns().map_err(error)? - cpu;
    let usage = os::usage().map_err(error)?.since(usage);
    result?;
    Ok((elapsed_ns, cpu_ns, usage))
}

fn pool(input: &Path, config: Config) -> Result<(Pool, FileId), String> {
    let pool = Pool::builder()
        .granule(config.granule)
        .frame_count(config.frames())
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(config.read_limit)
        .miss_headroom(3 * config.read_limit)
        .max_retained_frames(0)
        .registered_file_capacity(1)
        .registration_posture(RegistrationPolicy::Unregistered)
        .prefetch_headroom(config.credits)
        .readahead(if config.method == Method::Automatic {
            Readahead::Automatic
        } else {
            Readahead::Disabled
        })
        .build()
        .map_err(error)?;
    let file = pool
        .open(&input.join("pages.bin"), DirectIo::Required)
        .map_err(error)?;
    Ok((pool, file))
}

#[inline(never)]
fn read_mmap<const TRACE: bool>(mapping: &Mapping, config: Config, observer: &mut Observer<TRACE>) {
    for index in 0..config.requests() {
        let page = PageNumber(index % config.requests_per_pass());
        let started = observer.start();
        let sum = black_box(consume(black_box(mapping.page(black_box(page)))));
        observer.finish(index, page, started, sum);
    }
}

struct Pump<'a, const TRACE: bool> {
    pool: &'a Pool,
    reader: &'a ReaderCtx,
    config: Config,
    pages: &'a [PageId],
    observer: &'a mut Observer<TRACE>,
    hint_next: u32,
}

#[inline(never)]
fn read_dios<const TRACE: bool>(
    pool: &Pool,
    reader: &ReaderCtx,
    config: Config,
    pages: &[PageId],
    observer: &mut Observer<TRACE>,
) -> Result<(), String> {
    let mut pump = Pump {
        pool,
        reader,
        config,
        pages,
        observer,
        hint_next: 0,
    };
    for index in 0..config.requests() {
        if index.is_multiple_of(config.requests_per_pass()) {
            pump.hint_next = index;
        }
        read_one(&mut pump, index)?;
    }
    for _ in 0..POLLS_MAX {
        poll(&mut pump);
        if pool.prefetch_stats().reads_in_flight == 0 {
            return Ok(());
        }
    }
    Err("scan accepted reads exceeded bounded drain".to_owned())
}

fn read_one<const TRACE: bool>(pump: &mut Pump<'_, TRACE>, index: u32) -> Result<(), String> {
    let mut pending: Option<PendingToken> = None;
    let offset = index % pump.config.requests_per_pass();
    let page = pump.pages[offset as usize];
    let started = pump.observer.start();
    for _ in 0..POLLS_MAX {
        prefetch(pump, index, offset);
        if let Some(token) = pending.take() {
            pump.observer.counters.ready_checks += 1;
            match pump.pool.ready(pump.reader, token) {
                ReadyResult::Ready(guard) => {
                    finish(pump, index, offset, started, &guard);
                    pump.observer.counters.completed_pending += 1;
                    return Ok(());
                }
                ReadyResult::NotYet(token) => pending = Some(token),
                ReadyResult::Err(failure) => return Err(error(failure)),
            }
        } else {
            match pump.pool.get(pump.reader, page).map_err(error)? {
                Get::Hit(guard) => {
                    pump.observer.counters.hits += 1;
                    finish(pump, index, offset, started, &guard);
                    return Ok(());
                }
                Get::Pending(token) => {
                    pending = Some(token);
                    pump.observer.counters.pending += 1;
                }
                Get::Busy => pump.observer.counters.busy += 1,
            }
        }
        poll(pump);
    }
    Err("scan request exceeded bounded progress limit".to_owned())
}

fn finish<const TRACE: bool>(
    pump: &mut Pump<'_, TRACE>,
    index: u32,
    offset: u32,
    started: u64,
    bytes: &[u8],
) {
    assert_eq!(bytes.len(), pump.config.granule as usize);
    let sum = bytes
        .chunks_exact(GRANULE as usize)
        .fold(0_u64, |sum, page| {
            sum.wrapping_add(black_box(consume(black_box(page))))
        });
    let page = PageNumber(offset * (pump.config.granule / GRANULE));
    pump.observer.finish(index, page, started, sum);
}

fn prefetch<const TRACE: bool>(pump: &mut Pump<'_, TRACE>, index: u32, offset: u32) {
    if pump.config.method != Method::Explicit || index < pump.hint_next {
        return;
    }
    let end = (offset + pump.config.credits).min(pump.config.requests_per_pass());
    let report = pump
        .pool
        .prefetch(&pump.pages[offset as usize..end as usize]);
    assert_eq!(report.rejected, 0);
    pump.hint_next = if report.deferred == 0 {
        index + 1
    } else {
        index
    };
    pump.observer.counters.prefetch_calls += 1;
    pump.observer.counters.prefetch_deferred += report.deferred;
    pump.observer.flight(pump.pool);
}

fn poll<const TRACE: bool>(pump: &mut Pump<'_, TRACE>) {
    pump.pool.poll();
    pump.observer.counters.polls += 1;
    pump.observer.flight(pump.pool);
}

fn write<const TRACE: bool>(
    output: &Path,
    metadata: &serde_json::Value,
    observer: &Observer<TRACE>,
) -> Result<(), String> {
    #[derive(serde::Serialize)]
    struct Record<'a> {
        #[serde(flatten)]
        metadata: &'a serde_json::Value,
        events: &'a [super::observe::Event],
        flights: &'a [super::observe::FlightEvent],
    }
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(error)?;
    let mut writer = BufWriter::with_capacity(65_536, file);
    serde_json::to_writer(
        &mut writer,
        &Record {
            metadata,
            events: &observer.events,
            flights: &observer.flights,
        },
    )
    .map_err(error)?;
    writer.flush().map_err(error)
}

pub(super) fn profile(
    input: &Path,
    config: &str,
    repetitions: &str,
    output: &Path,
) -> Result<(), String> {
    let repetitions: u32 = repetitions.parse().map_err(error)?;
    if !(1..=128).contains(&repetitions) {
        return Err("profile repetitions must be 1..128".to_owned());
    }
    fs::create_dir(output).map_err(error)?;
    for index in 0..repetitions {
        sample::<false>(input, config, &output.join(format!("{index:04}.json")))?;
    }
    Ok(())
}
