//! Fair lanes: one `#[inline(never)]` loop function per arm over one request
//! vector, an untimed warm pass through that loop, and the page-size witness of
//! the mapping each arm reads (fairness contract F1 to F7).

use std::fs;
use std::hint::black_box;
use std::time::Instant;

use dios::{FileId, Get, GetError, PageId, Pool, ReaderCtx};
use serde_json::{Value, json};

use super::catalog::{Arm, GRANULE, PageNumber, consume};
use super::engine::{Source, Work, WorkerResult};
use super::fixture::{self, error};
use super::observe::{allocations_begin, allocations_end, nanoseconds};
use super::os::{self, Mapping};

/// Requests one session serves, the chunking of `resident_access_epoch_batch`.
#[cfg(dios_read_session)]
const SESSION_REQUESTS: usize = 16;

/// One measured fair-lane process: the worker row and the F6 and F7 witnesses.
pub(super) struct Measured {
    pub(super) worker: WorkerResult,
    pub(super) warm_pass_operations: u32,
    pub(super) page_size: Value,
}

pub(super) fn measure(work: Work<'_>) -> Result<Measured, String> {
    os::pin(0).map_err(error)?;
    assert!(work.lane.fair(), "the fair-lane module serves fair lanes");
    assert_eq!(work.lane.workers(), 1);
    let requests: Vec<u32> = (0..work.lane.operations())
        .map(|index| work.lane.page(work.seed, index).0)
        .collect();
    let setup_started = Instant::now();
    let reader = match work.source {
        Source::Mmap(_) => None,
        Source::Dios(pool, _) => Some(pool.register_reader().map_err(error)?),
    };
    let array = match work.source {
        Source::Mmap(mapping) if work.arm == Arm::PlainArray => {
            plain_array(mapping, work.lane.warm_pages())
        }
        _ => Vec::new(),
    };
    let mapped = match work.source {
        Source::Mmap(mapping) => mapping.bytes(),
        Source::Dios(..) => &[],
    };
    let address = mapping_address(work, reader.as_ref(), &array)?;
    let setup_ns = nanoseconds(setup_started.elapsed());
    let read = |repetitions: u32| match (work.source, work.arm) {
        (Source::Mmap(_), Arm::MmapRandom) => Ok(mmap_random_loop(
            mapped,
            &requests,
            repetitions,
            work.lane.useful_bytes() as usize,
        )),
        (Source::Mmap(_), Arm::PlainArray) => Ok(plain_array_loop(
            &array,
            &requests,
            repetitions,
            work.lane.useful_bytes() as usize,
        )),
        (Source::Dios(pool, file), arm) => read_pool(
            pool,
            reader.as_ref().expect("Dios reader"),
            file,
            arm,
            &requests,
            repetitions,
            work.lane.useful_bytes() as usize,
        ),
        (Source::Mmap(_), arm) => Err(format!("{arm:?} does not read the file mapping")),
    };
    let warm_checksum = read(1)?;
    if warm_checksum != fixture::expected(work.lane, work.seed) {
        return Err("warm pass checksum differs".to_owned());
    }
    measure_timed(work, address, setup_ns, read)
}

/// The timed region over the warmed `read`, bracketed by the page-size witness.
fn measure_timed(
    work: Work<'_>,
    address: usize,
    setup_ns: u64,
    read: impl Fn(u32) -> Result<u64, String>,
) -> Result<Measured, String> {
    let before = page_size(address)?;
    let usage_before = os::usage().map_err(error)?;
    let cpu_before = os::cpu_ns().map_err(error)?;
    let started = Instant::now();
    allocations_begin();
    let checksum = read(work.lane.repetitions());
    let allocations = allocations_end();
    let elapsed_ns = nanoseconds(started.elapsed());
    let cpu_ns = os::cpu_ns().map_err(error)? - cpu_before;
    let usage = os::usage().map_err(error)?.since(usage_before);
    let after = page_size(address)?;
    let worker = measure_worker(
        work,
        checksum?,
        allocations,
        elapsed_ns,
        cpu_ns,
        setup_ns,
        usage,
    );
    Ok(Measured {
        worker,
        warm_pass_operations: work.lane.operations(),
        page_size: json!({"before": before, "after": after}),
    })
}

/// The worker row, with every count derived after the timed loop.
fn measure_worker(
    work: Work<'_>,
    checksum: u64,
    allocations: u64,
    elapsed_ns: u64,
    cpu_ns: u64,
    setup_ns: u64,
    usage: os::Usage,
) -> WorkerResult {
    let reads = work.lane.timed_operations();
    let mut worker = WorkerResult {
        worker: 0,
        elapsed_ns,
        finished_ns: elapsed_ns,
        cpu_ns,
        setup_ns,
        teardown_ns: 0,
        retained_payload_bytes: 0,
        setup_storage_bytes: 0,
        counters: super::observe::Counters::default(),
        usage,
        events: Vec::new(),
        flights: Vec::new(),
    };
    let counters = &mut worker.counters;
    counters.operations = reads;
    counters.checksum = checksum;
    counters.allocations = allocations;
    match work.arm {
        Arm::DiosGet => {
            counters.hits = reads;
            counters.guard_acquisitions = reads;
        }
        Arm::DiosSession => counters.hits = reads,
        _ => {}
    }
    worker
}

/// The floor arm's private copy of the lane's resident pages, built before timing.
fn plain_array(mapping: &Mapping, pages: u32) -> Vec<u8> {
    let mut array = Vec::with_capacity(pages as usize * GRANULE as usize);
    for page in 0..pages {
        array.extend_from_slice(mapping.page(PageNumber(page)));
    }
    assert_eq!(array.len(), pages as usize * GRANULE as usize);
    array
}

/// An address inside the mapping the arm reads: the file mapping, the floor
/// arm's array, or the pool arena.
fn mapping_address(
    work: Work<'_>,
    reader: Option<&ReaderCtx>,
    array: &[u8],
) -> Result<usize, String> {
    match work.source {
        Source::Mmap(_) if work.arm == Arm::PlainArray => Ok(array.as_ptr().addr()),
        Source::Mmap(mapping) => Ok(mapping.page(PageNumber(0)).as_ptr().addr()),
        Source::Dios(pool, file) => {
            let reader = reader.expect("Dios reader");
            match pool.get(reader, PageId::new(file, 0)).map_err(error)? {
                Get::Hit(guard) => Ok(guard.as_ptr().addr()),
                Get::Pending(_) | Get::Busy => Err("a warmed page must hit".to_owned()),
            }
        }
    }
}

fn read_pool(
    pool: &Pool,
    reader: &ReaderCtx,
    file: FileId,
    arm: Arm,
    requests: &[u32],
    repetitions: u32,
    useful_bytes: usize,
) -> Result<u64, String> {
    match arm {
        Arm::DiosGet => Ok(dios_get_loop(
            pool,
            reader,
            file,
            requests,
            repetitions,
            useful_bytes,
        )),
        #[cfg(dios_read_session)]
        Arm::DiosSession => Ok(dios_session_loop(
            pool,
            reader,
            file,
            requests,
            repetitions,
            useful_bytes,
        )),
        #[cfg(not(dios_read_session))]
        Arm::DiosSession => Err("the product tree lacks the session read".to_owned()),
        _ => Err(format!("{arm:?} is not a fair-lane pool arm")),
    }
}

#[inline(never)]
fn mmap_random_loop(mapped: &[u8], requests: &[u32], repetitions: u32, useful_bytes: usize) -> u64 {
    let mut checksum = 0_u64;
    for _ in 0..repetitions {
        for &page in requests {
            let page = black_box(page);
            let start = page as usize * GRANULE as usize;
            checksum =
                checksum.wrapping_add(black_box(consume(&mapped[start..start + useful_bytes])));
        }
    }
    checksum
}

#[inline(never)]
fn plain_array_loop(array: &[u8], requests: &[u32], repetitions: u32, useful_bytes: usize) -> u64 {
    let mut checksum = 0_u64;
    for _ in 0..repetitions {
        for &page in requests {
            let page = black_box(page);
            let start = page as usize * GRANULE as usize;
            checksum =
                checksum.wrapping_add(black_box(consume(&array[start..start + useful_bytes])));
        }
    }
    checksum
}

#[inline(never)]
fn dios_get_loop(
    pool: &Pool,
    reader: &ReaderCtx,
    file: FileId,
    requests: &[u32],
    repetitions: u32,
    useful_bytes: usize,
) -> u64 {
    let mut checksum = 0_u64;
    for _ in 0..repetitions {
        for &page in requests {
            let page = black_box(page);
            match pool.get(reader, PageId::new(file, page)) {
                Ok(Get::Hit(guard)) => {
                    checksum = checksum.wrapping_add(black_box(consume(&guard[..useful_bytes])));
                }
                outcome => dios_get_loop_missed(&outcome),
            }
        }
    }
    checksum
}

#[cold]
#[inline(never)]
fn dios_get_loop_missed(outcome: &Result<Get<'_>, GetError>) -> ! {
    panic!("a resident fair-lane get did not hit: {outcome:?}")
}

#[cfg(dios_read_session)]
#[inline(never)]
fn dios_session_loop(
    pool: &Pool,
    reader: &ReaderCtx,
    file: FileId,
    requests: &[u32],
    repetitions: u32,
    useful_bytes: usize,
) -> u64 {
    let mut checksum = 0_u64;
    for _ in 0..repetitions {
        for chunk in requests.chunks_exact(SESSION_REQUESTS) {
            let session = pool.session(reader);
            let view = match session.file(file) {
                Ok(view) => view,
                Err(failure) => dios_session_loop_stale(&failure),
            };
            for &page in chunk {
                let page = black_box(page);
                match view.page(page) {
                    Ok(dios::SessionRead::Hit(bytes)) => {
                        checksum =
                            checksum.wrapping_add(black_box(consume(&bytes[..useful_bytes])));
                    }
                    outcome => dios_session_loop_missed(&outcome),
                }
            }
        }
    }
    checksum
}

#[cfg(dios_read_session)]
#[cold]
#[inline(never)]
fn dios_session_loop_stale(failure: &GetError) -> ! {
    panic!("a fair-lane session file was stale: {failure:?}")
}

#[cfg(dios_read_session)]
#[cold]
#[inline(never)]
fn dios_session_loop_missed(outcome: &Result<dios::SessionRead<'_>, GetError>) -> ! {
    panic!("a resident fair-lane session read did not hit: {outcome:?}")
}

/// The F7 witness of the mapping containing `address`: its backing page sizes
/// from `/proc/self/smaps`, the THP mode, the advice in effect and the filesystem.
fn page_size(address: usize) -> Result<Value, String> {
    let transparent_hugepage =
        fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled").map_err(error)?;
    let smaps = fs::read_to_string("/proc/self/smaps").map_err(error)?;
    let mapping = page_size_mapping(&smaps, address)?;
    let field = |name: &str| -> Result<u64, String> {
        mapping
            .iter()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .ok_or_else(|| format!("smaps lacks {name}"))?
            .trim()
            .trim_end_matches(" kB")
            .parse()
            .map_err(error)
    };
    let flags = mapping
        .iter()
        .find_map(|line| line.strip_prefix("VmFlags:"))
        .ok_or("smaps lacks VmFlags")?;
    let advice: Vec<&str> = flags
        .split_whitespace()
        .filter_map(|flag| match flag {
            "rr" => Some("random"),
            "sr" => Some("sequential"),
            "hg" => Some("hugepage"),
            "nh" => Some("nohugepage"),
            _ => None,
        })
        .collect();
    let path = mapping[0].split_whitespace().nth(5).unwrap_or("");
    Ok(json!({
        "anonhugepages_kib": field("AnonHugePages")?,
        "filepmdmapped_kib": field("FilePmdMapped")?,
        "kernelpagesize_kib": field("KernelPageSize")?,
        "mapping_kib": field("Size")?,
        "transparent_hugepage": transparent_hugepage.trim(),
        "advice": if advice.is_empty() { "none".to_owned() } else { advice.join(",") },
        "filesystem": page_size_filesystem(path)?,
    }))
}

/// The smaps lines of the mapping containing `address`, header first.
fn page_size_mapping(smaps: &str, address: usize) -> Result<Vec<&str>, String> {
    let mut lines = Vec::new();
    for line in smaps.lines() {
        let header = line
            .split_once(' ')
            .and_then(|(range, _)| range.split_once('-'))
            .and_then(|(start, end)| {
                Some(usize::from_str_radix(start, 16).ok()?..usize::from_str_radix(end, 16).ok()?)
            });
        match header {
            Some(_) if !lines.is_empty() => return Ok(lines),
            Some(range) => {
                if range.contains(&address) {
                    lines.push(line);
                }
            }
            None => {
                if !lines.is_empty() {
                    lines.push(line);
                }
            }
        }
    }
    if lines.is_empty() {
        return Err("no smaps mapping contains the read address".to_owned());
    }
    Ok(lines)
}

/// The filesystem type of the mount holding `path`, from `/proc/self/mountinfo`;
/// `anonymous` for a mapping without a file.
fn page_size_filesystem(path: &str) -> Result<String, String> {
    if !path.starts_with('/') {
        return Ok("anonymous".to_owned());
    }
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").map_err(error)?;
    let mut best: Option<(usize, &str)> = None;
    for line in mountinfo.lines() {
        let Some((mount, filesystem)) = line
            .split_once(" - ")
            .and_then(|(fields, rest)| Some((fields.split(' ').nth(4)?, rest.split(' ').next()?)))
        else {
            continue;
        };
        let contains = path == mount
            || mount == "/"
            || path
                .strip_prefix(mount)
                .is_some_and(|rest| rest.starts_with('/'));
        if contains && best.is_none_or(|(length, _)| mount.len() > length) {
            best = Some((mount.len(), filesystem));
        }
    }
    best.map(|(_, filesystem)| filesystem.to_owned())
        .ok_or_else(|| format!("no mount holds {path}"))
}
