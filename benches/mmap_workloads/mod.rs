mod catalog;
mod engine;
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
use fixture::error;

pub(crate) fn run(arguments: &[String]) -> Result<(), String> {
    match arguments {
        [command, ..] if command.starts_with("probe-") => run_probe(arguments),
        [command] if command == "scan-list" => {
            println!("{}", json!(scan_config::catalog()));
            Ok(())
        }
        [command, input, config, mode, output] if command == "scan-sample" => match mode.as_str() {
            "plain" => scan::sample::<false>(Path::new(input), config, Path::new(output)),
            "trace" => scan::sample::<true>(Path::new(input), config, Path::new(output)),
            _ => Err("scan mode must be plain or trace".to_owned()),
        },
        [command, input, config, repetitions, output] if command == "scan-profile" => {
            scan::profile(Path::new(input), config, repetitions, Path::new(output))
        }
        [command] if command == "list" => {
            let lanes: Vec<_> = Lane::ALL.into_iter().map(|lane| json!({
                "name": lane.name(), "arms": lane.arms(), "cache": lane.cache(),
                "operations": lane.operations(), "workers": lane.workers(),
                "useful_bytes_per_read": lane.useful_bytes(), "frames": lane.frames(),
            })).collect();
            println!("{}", serde_json::to_string_pretty(&lanes).map_err(error)?);
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
            if !lane.arms().contains(&arm) { return Err("arm does not belong to this comparison".to_owned()); }
            let seed: u32 = seed.parse().map_err(error)?;
            if seed > 1_000_000 { return Err("seed exceeds fixed bound".to_owned()); }
            match tracing.as_str() {
                "plain" => sample::<false>(Path::new(directory), lane, arm, seed, Path::new(output)),
                "trace" => sample::<true>(Path::new(directory), lane, arm, seed, Path::new(output)),
                _ => Err("mode must be plain or trace".to_owned()),
            }
        }
        _ => Err("usage: list | create NEW_DIR | summarize CSV | sample INPUT LANE ARM SEED plain|trace NEW_JSON".to_owned()),
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

fn sample<const TRACE: bool>(
    directory: &Path,
    lane: Lane,
    arm: Arm,
    seed: u32,
    output: &Path,
) -> Result<(), String> {
    os::pin(0).map_err(error)?;
    let fixture_identity: serde_json::Value =
        serde_json::from_reader(fs::File::open(directory.join("fixture.json")).map_err(error)?)
            .map_err(error)?;
    let expected_checksum = fixture::expected(lane, seed);
    let mapping = os::Mapping::open(&directory.join("pages.bin")).map_err(error)?;
    let cache_before = fixture::prepare(&mapping, lane, arm, seed).map_err(error)?;
    let pool = if arm.is_mmap() {
        None
    } else {
        Some(fixture::pool(&directory.join("pages.bin"), lane, arm)?)
    };
    let source = pool
        .as_ref()
        .map_or(Source::Mmap(&mapping), |(pool, file)| {
            Source::Dios(pool, *file)
        });
    let before = snapshot();
    sample_check_limit(lane, &before)?;
    let workers = engine::measure::<TRACE>(Work {
        source,
        lane,
        arm,
        seed,
    })?;
    let after = snapshot();
    let elapsed_ns = workers
        .iter()
        .map(|worker| worker.finished_ns)
        .max()
        .expect("worker exists");
    sample_check_workers(&workers, expected_checksum)?;
    let (io_mode, registration) = sample_posture(pool.as_ref());
    let row = json!({"schema": 1, "lane": lane, "arm": arm, "seed": seed,
        "trace": TRACE, "cache": lane.cache(), "operations": lane.operations(),
        "useful_bytes": u64::from(lane.operations()) * u64::from(lane.useful_bytes()),
        "frames": lane.frames(), "expected_checksum": expected_checksum,
        "elapsed_ns": elapsed_ns, "cache_before": cache_before,
        "system_before": before, "system_after": after, "io_mode": io_mode,
        "registration": registration, "fixture": fixture_identity,
        "runner_sha256": runner_hash(),
        "debug_assertions": cfg!(debug_assertions),
        "prefetch": sample_prefetch(pool.as_ref()),
    });
    sample_write(output, &row, &workers)
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
    let gate = dios::bench::ratio_gate(&samples, 10_000);
    println!(
        "{}",
        json!({"ratio_orientation": "candidate/base", "pairs": samples.base_ns.len(),
        "ratio_geomean": gate.ratio_geomean, "ci95_upper": gate.ratio_ci95_upper,
        "kind": "characterization", "adopted_bound": null})
    );
    Ok(())
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
