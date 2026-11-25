mod catalog;
mod engine;
mod fair;
mod fixture;
mod observe;
mod os;
mod probe;
mod resident;
mod scan;
mod scan_config;

use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use serde_json::json;

use catalog::{Arm, Cache, Lane};
use engine::{Source, Work};
use fair::{SWEPT_SESSION_LENGTHS, SessionLength};
use fixture::error;
use scan_config::CreditSelection;

pub(crate) fn run(arguments: &[String]) -> Result<(), String> {
    match arguments {
        [command, ..] if command.starts_with("probe-") => run_probe(arguments),
        [command] if command == "scan-list" => {
            println!("{}", json!(scan_config::catalog()));
            Ok(())
        }
        [command, ..] if command.starts_with("scan-") => run_scan(arguments),
        [command] if command == "list" => {
            println!("{}", serde_json::to_string_pretty(&lane_catalog()).map_err(error)?);
            Ok(())
        }
        [command, directory] if command == "create" => fixture::create(Path::new(directory)).map_err(error),
        [command, csv] if command == "summarize" => summarize(Path::new(csv)),
        [command] if command == "identity" => {
            println!("{}", json!({"runner_sha256": runner_hash(), "debug_assertions": cfg!(debug_assertions)}));
            Ok(())
        }
        [command, directory, lane, arm, repetitions, output] if command == "profile" => {
            profile(Path::new(directory), lane, arm, repetitions, Path::new(output))
        }
        [command, directory, lane, arm, seed, tracing, output] if command == "sample" => {
            let lane = Lane::parse(lane)?;
            let arm = Arm::parse(arm)?;
            if !lane.arms().contains(&arm) && lane.floor_arm() != Some(arm) && lane.flat_arm() != Some(arm) {
                return Err("arm does not belong to this comparison".to_owned());
            }
            let seed: u32 = seed.parse().map_err(error)?;
            if seed > 1_000_000 { return Err("seed exceeds fixed bound".to_owned()); }
            if tracing == "trace" && lane.repetitions() > 1 {
                return Err("trace mode is refused for a repeated sequence".to_owned());
            }
            let sessions = SessionLength::Gated;
            match tracing.as_str() {
                "plain" => sample::<false>(Path::new(directory), lane, arm, seed, sessions, Path::new(output)),
                "trace" => sample::<true>(Path::new(directory), lane, arm, seed, sessions, Path::new(output)),
                _ => Err("mode must be plain or trace".to_owned()),
            }
        }
        [command, directory, lane, arm, seed, length, output] if command == "session-sample" => {
            session_sample(Path::new(directory), lane, arm, seed, length, Path::new(output))
        }
        _ => Err("usage: list | create NEW_DIR | summarize CSV | sample INPUT LANE ARM SEED plain|trace NEW_JSON | session-sample INPUT LANE ARM SEED LENGTH NEW_JSON".to_owned()),
    }
}

/// The lane catalog `list` prints and the collector freezes.
pub(crate) fn lane_catalog() -> Vec<serde_json::Value> {
    Lane::ALL
        .into_iter()
        .map(|lane| {
            let mut entry = json!({
                "name": lane.name(), "arms": lane.arms(), "cache": lane.cache(),
                "operations": lane.timed_operations(), "workers": lane.workers(),
                "useful_bytes_per_read": lane.useful_bytes(), "frames": lane.frames(),
            });
            if let Some(floor_arm) = lane.floor_arm() {
                entry["floor_arm"] = json!(floor_arm);
            }
            if let Some(flat_arm) = lane.flat_arm() {
                entry["flat_arm"] = json!(flat_arm);
            }
            entry
        })
        .collect()
}

fn run_scan(arguments: &[String]) -> Result<(), String> {
    let [command, input, config, mode_or_repetitions, output] = arguments else {
        let usage = match arguments.first().map(String::as_str) {
            Some("scan-profile" | "scan-default-profile") => {
                "scan requires INPUT CONFIG REPETITIONS OUTPUT"
            }
            _ => "scan requires INPUT CONFIG MODE OUTPUT",
        };
        return Err(usage.to_owned());
    };
    let credits = match command.as_str() {
        "scan-sample" | "scan-profile" => CreditSelection::Override,
        "scan-default-sample" | "scan-default-profile" => CreditSelection::Default,
        _ => return Err("unknown scan command".to_owned()),
    };
    if command.ends_with("profile") {
        return scan::profile(
            Path::new(input),
            config,
            credits,
            mode_or_repetitions,
            Path::new(output),
        );
    }
    match mode_or_repetitions.as_str() {
        "plain" => scan::sample::<false>(Path::new(input), config, credits, Path::new(output)),
        "trace" => scan::sample::<true>(Path::new(input), config, credits, Path::new(output)),
        _ => Err("scan mode must be plain or trace".to_owned()),
    }
}

fn run_probe(arguments: &[String]) -> Result<(), String> {
    match arguments {
        [command] if command == "probe-list" => { println!("{}", json!(probe::METHODS)); Ok(()) }
        [command, input, method, output] if command == "probe-sample" => {
            probe::sample(Path::new(input), method, "1", Path::new(output))
        }
        [command, input, method, depth, output] if command == "probe-sample" => {
            probe::sample(Path::new(input), method, depth, Path::new(output))
        }
        [command, input, method, output] if command == "probe-clock-sample" => {
            probe::clock_sample(Path::new(input), method, Path::new(output))
        }
        [command, input, method, repetitions, output] if command == "probe-profile" => {
            probe::profile(Path::new(input), method, "1", repetitions, Path::new(output))
        }
        [command, input, method, depth, repetitions, output] if command == "probe-profile" => {
            probe::profile(Path::new(input), method, depth, repetitions, Path::new(output))
        }
        _ => Err("usage: probe-list | probe-sample INPUT METHOD DEPTH OUTPUT | probe-profile INPUT METHOD DEPTH REPS OUTPUT".to_owned()),
    }
}

/// One reported, never gated sample of a session lane's mmap or session arm
/// at one sweep length of requests per session.
fn session_sample(
    directory: &Path,
    lane: &str,
    arm: &str,
    seed: &str,
    length: &str,
    output: &Path,
) -> Result<(), String> {
    let lane = Lane::parse(lane)?;
    let arm = Arm::parse(arm)?;
    if !lane.session() || !lane.arms().contains(&arm) {
        return Err(
            "the session-length sweep samples a session lane's mmap or session arm".to_owned(),
        );
    }
    let seed: u32 = seed.parse().map_err(error)?;
    if seed > 1_000_000 {
        return Err("seed exceeds fixed bound".to_owned());
    }
    let length: usize = length.parse().map_err(error)?;
    if !SWEPT_SESSION_LENGTHS.contains(&length) {
        return Err(format!(
            "session length must be one of {SWEPT_SESSION_LENGTHS:?}"
        ));
    }
    sample::<false>(
        directory,
        lane,
        arm,
        seed,
        SessionLength::Swept(length),
        output,
    )
}

fn sample<const TRACE: bool>(
    directory: &Path,
    lane: Lane,
    arm: Arm,
    seed: u32,
    sessions: SessionLength,
    output: &Path,
) -> Result<(), String> {
    os::pin(0).map_err(error)?;
    let fixture_identity: serde_json::Value =
        serde_json::from_reader(fs::File::open(directory.join("fixture.json")).map_err(error)?)
            .map_err(error)?;
    let expected_checksum =
        fixture::expected(lane, seed).wrapping_mul(u64::from(lane.repetitions()));
    let mapping = os::Mapping::open(&directory.join("pages.bin")).map_err(error)?;
    let cache_before = fixture::prepare(&mapping, lane, arm, seed).map_err(error)?;
    let pool = if arm.reads_pool() {
        Some(fixture::pool(&directory.join("pages.bin"), lane, arm)?)
    } else {
        None
    };
    let source = pool
        .as_ref()
        .map_or(Source::Mmap(&mapping), |(pool, file)| {
            Source::Dios(pool, *file)
        });
    let before = snapshot();
    sample_check_limit(lane, &before)?;
    let work = Work {
        source,
        lane,
        arm,
        seed,
    };
    let (workers, witnesses) = sample_measure::<TRACE>(work, sessions)?;
    let after = snapshot();
    let elapsed_ns = workers
        .iter()
        .map(|worker| worker.finished_ns)
        .max()
        .expect("worker exists");
    sample_check_workers(&workers, expected_checksum)?;
    let (io_mode, registration) = if arm.reads_private_array() {
        ("none".to_owned(), "none".to_owned())
    } else {
        sample_posture(pool.as_ref())
    };
    let mut row = json!({"schema": 1, "lane": lane, "arm": arm, "seed": seed,
        "fair": lane.fair(), "trace": TRACE, "cache": lane.cache(), "operations": lane.timed_operations(),
        "useful_bytes": u64::from(lane.timed_operations()) * u64::from(lane.useful_bytes()),
        "frames": lane.frames(), "expected_checksum": expected_checksum,
        "elapsed_ns": elapsed_ns, "cache_before": cache_before,
        "system_before": before, "system_after": after, "io_mode": io_mode,
        "registration": registration, "fixture": fixture_identity,
        "runner_sha256": runner_hash(),
        "debug_assertions": cfg!(debug_assertions),
        "prefetch": sample_prefetch(pool.as_ref()),
    });
    #[cfg(dios_translation_memory)]
    if let Some((pool, _)) = pool.as_ref() {
        row["translation_memory"] = sample_translation_memory(pool, lane);
    }
    let serde_json::Value::Object(witnesses) = witnesses else {
        unreachable!("sample witnesses are an object")
    };
    row.as_object_mut()
        .expect("row is an object")
        .extend(witnesses);
    sample_write(output, &row, &workers)
}

/// The timed workers of one sample and an object of the row fields of its
/// fair-lane witnesses: the warm pass, the page size and, for a session lane's
/// chunked arms, the requests per session; empty for other lanes.
fn sample_measure<const TRACE: bool>(
    work: Work<'_>,
    sessions: SessionLength,
) -> Result<(Vec<engine::WorkerResult>, serde_json::Value), String> {
    if work.lane.fair() {
        assert!(!TRACE, "fair lanes refuse trace mode");
        let measured = fair::measure(work, sessions)?;
        let mut witnesses = json!({"warm_pass": {"operations": measured.warm_pass_operations},
            "page_size": measured.page_size});
        if work.lane.session() && work.lane.flat_arm() != Some(work.arm) {
            witnesses["session_requests"] = json!(sessions.requests());
        }
        if sessions != SessionLength::Gated {
            witnesses["characterization"] = json!("session_length_sweep");
        }
        Ok((vec![measured.worker], witnesses))
    } else {
        Ok((engine::measure::<TRACE>(work)?, json!({})))
    }
}

#[derive(serde::Serialize)]
struct Record<'a> {
    #[serde(flatten)]
    metadata: &'a serde_json::Value,
    workers: &'a [engine::WorkerResult],
}

fn sample_write(
    output: &Path,
    metadata: &serde_json::Value,
    workers: &[engine::WorkerResult],
) -> Result<(), String> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(error)?;
    let mut writer = BufWriter::with_capacity(65_536, file);
    serde_json::to_writer(&mut writer, &Record { metadata, workers }).map_err(error)?;
    writer.flush().map_err(error)
}

fn profile(
    directory: &Path,
    lane: &str,
    arm: &str,
    repetitions: &str,
    output: &Path,
) -> Result<(), String> {
    let lane = Lane::parse(lane)?;
    let arm = Arm::parse(arm)?;
    let repetitions: u32 = repetitions.parse().map_err(error)?;
    if !lane.arms().contains(&arm) {
        return Err("arm does not belong to lane".to_owned());
    }
    if !(1..=128).contains(&repetitions) {
        return Err("profile repetitions must be 1..128".to_owned());
    }
    fs::create_dir(output).map_err(error)?;
    for seed in 0..repetitions {
        sample::<false>(
            directory,
            lane,
            arm,
            seed,
            SessionLength::Gated,
            &output.join(format!("{seed:04}.json")),
        )?;
    }
    Ok(())
}

fn runner_hash() -> String {
    use sha2::{Digest, Sha256};

    let mut hash = Sha256::new();
    for source in [
        include_str!("../mmap_workloads.rs"),
        include_str!("mod.rs"),
        include_str!("catalog.rs"),
        include_str!("engine.rs"),
        include_str!("fixture.rs"),
        include_str!("observe.rs"),
        include_str!("os.rs"),
        include_str!("resident.rs"),
        include_str!("scan_config.rs"),
        include_str!("scan.rs"),
        include_str!("probe.rs"),
        include_str!("probe/clock.rs"),
        include_str!("fair.rs"),
    ] {
        hash.update(source.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

fn sample_check_workers(workers: &[engine::WorkerResult], expected: u64) -> Result<(), String> {
    let checksum = workers.iter().fold(0_u64, |sum, worker| {
        sum.wrapping_add(worker.counters.checksum)
    });
    if checksum != expected {
        return Err("result checksum differs".to_owned());
    }
    if workers
        .iter()
        .any(|worker| worker.counters.allocations != 0)
    {
        return Err("timed workload allocated".to_owned());
    }
    Ok(())
}

fn sample_check_limit(lane: Lane, before: &serde_json::Value) -> Result<(), String> {
    if lane.cache() == Cache::Pressure {
        if before["memory.max"] != "134217728" {
            return Err("pressure lane requires MemoryMax=128M".to_owned());
        }
        if before["memory.swap.max"] != "0" {
            return Err("pressure lane requires MemorySwapMax=0".to_owned());
        }
    }
    Ok(())
}

fn sample_posture(pool: Option<&(dios::Pool, dios::FileId)>) -> (String, String) {
    pool.map_or(("mmap".to_owned(), "none".to_owned()), |(pool, file)| {
        let mode = match pool.io_mode(*file).expect("registered live file") {
            dios::IoMode::Direct(_) => "Direct",
            dios::IoMode::Buffered => "Buffered",
        };
        (
            mode.to_owned(),
            format!("{:?}", pool.registration_posture()),
        )
    })
}

/// The translation mapping's size and resident bytes after the timed loop,
/// beside the arena of one granule per frame.
#[cfg(dios_translation_memory)]
fn sample_translation_memory(pool: &dios::Pool, lane: Lane) -> serde_json::Value {
    let translation = dios::bench::pool_translation(pool);
    let mapping_bytes = translation.entry_count() * size_of::<u32>() as u64;
    let resident_bytes = translation.resident_bytes();
    assert!(
        resident_bytes <= mapping_bytes,
        "residency lies within the mapping"
    );
    json!({"mapping_bytes": mapping_bytes, "resident_bytes": resident_bytes,
        "arena_bytes": u64::from(lane.frames()) * u64::from(catalog::GRANULE)})
}

fn sample_prefetch(pool: Option<&(dios::Pool, dios::FileId)>) -> serde_json::Value {
    pool.map_or(serde_json::Value::Null, |(pool, _)| {
        let stats = pool.prefetch_stats();
        json!({"admitted": stats.admitted, "automatic_admitted": stats.automatic_admitted,
            "demand_promoted": stats.demand_promoted, "evicted_unused": stats.evicted_unused,
            "failed": stats.failed, "deferred": stats.deferred, "occupied": stats.occupied,
            "capacity": stats.capacity, "reserve_free": stats.reserve_free,
            "submission_refused": stats.submission_refused, "reads_in_flight": stats.reads_in_flight})
    })
}

fn summarize(path: &Path) -> Result<(), String> {
    let samples = dios::bench::read_samples(path).map_err(error)?;
    if samples.base_ns.len() < 30 {
        return Err("characterization requires at least 30 pairs".to_owned());
    }
    println!("{}", summarize_samples(&samples));
    Ok(())
}

/// The shared harness's ratio and paired difference candidate minus base in
/// nanoseconds per process; a product tree without the paired difference
/// omits its fields (F10).
fn summarize_samples(samples: &dios::bench::PairedSamples) -> serde_json::Value {
    let gate = dios::bench::ratio_gate(samples, 10_000);
    #[cfg_attr(
        not(dios_paired_difference),
        expect(unused_mut, reason = "only the paired difference extends the summary")
    )]
    let mut summary = json!({"ratio_orientation": "candidate/base",
        "pairs": samples.base_ns.len(),
        "ratio_geomean": gate.ratio_geomean, "ci95_upper": gate.ratio_ci95_upper,
        "kind": "characterization", "adopted_bound": null});
    #[cfg(dios_paired_difference)]
    {
        let difference = dios::bench::paired_difference(samples, 10_000);
        summary["difference_ns_mean"] = json!(difference.difference_ns_mean);
        summary["difference_ns_ci95_lower"] = json!(difference.difference_ns_ci95_lower);
        summary["difference_ns_ci95_upper"] = json!(difference.difference_ns_ci95_upper);
    }
    summary
}

fn snapshot() -> serde_json::Value {
    let cgroup = fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("0::"))
                .map(str::to_owned)
        });
    let root = cgroup
        .as_ref()
        .map(|path| PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/')));
    let mut value = serde_json::Map::new();
    for name in [
        "memory.current",
        "memory.peak",
        "memory.max",
        "memory.swap.max",
        "memory.stat",
        "memory.events",
        "memory.pressure",
    ] {
        let text = root
            .as_ref()
            .and_then(|path| fs::read_to_string(path.join(name)).ok());
        value.insert(
            name.to_owned(),
            text.map_or(serde_json::Value::Null, |text| json!(text.trim())),
        );
    }
    value.insert("cgroup".to_owned(), json!(cgroup));
    value.insert(
        "process_io".to_owned(),
        json!(fs::read_to_string("/proc/self/io").ok()),
    );
    let interrupts = fs::read_to_string("/proc/interrupts")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.contains("TLB:"))
                .map(str::to_owned)
        });
    value.insert("global_tlb_interrupts".to_owned(), json!(interrupts));
    serde_json::Value::Object(value)
}

#[cfg(test)]
mod tests {
    #[test]
    fn summarize_formats_the_shared_harness_ratio_and_paired_difference() {
        let base_ns: Vec<u64> = (0..32_u64).map(|pair| 1_000_000 + pair * 1_000).collect();
        let samples = dios::bench::PairedSamples {
            name: "noisy".to_owned(),
            candidate_ns: base_ns
                .iter()
                .enumerate()
                .map(|(pair, &ns)| if pair % 2 == 0 { ns + 400 } else { ns + 600 })
                .collect(),
            base_ns,
        };
        let summary = super::summarize_samples(&samples);
        let gate = dios::bench::ratio_gate(&samples, 10_000);
        let difference = dios::bench::paired_difference(&samples, 10_000);
        assert_eq!(summary["ratio_geomean"], gate.ratio_geomean);
        assert_eq!(summary["ci95_upper"], gate.ratio_ci95_upper);
        assert_eq!(summary["pairs"], 32);
        assert_eq!(summary["difference_ns_mean"], difference.difference_ns_mean);
        assert_eq!(
            summary["difference_ns_ci95_lower"],
            difference.difference_ns_ci95_lower
        );
        assert_eq!(
            summary["difference_ns_ci95_upper"],
            difference.difference_ns_ci95_upper
        );
    }
}
