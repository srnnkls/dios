//! Shipping-pool storage workload characterization and diagnostic replays.

mod catalog;
mod engine;
mod fixture;
mod observe;

use std::fs::{self, File};
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use dios::DirectIo;
use dios::bench::{PairedSamples, ratio_gate, read_samples, write_samples};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest as _, Sha256};

use catalog::{Arm, Lane, REPS, WARMUPS};
use engine::Measurement;
use fixture::{Fixture, display_error};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Run,
    Smoke,
    Trace,
    Observe,
    Profile,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Smoke => "smoke",
            Self::Trace => "trace",
            Self::Observe => "observe",
            Self::Profile => "profile",
        }
    }
}

#[derive(Clone)]
struct Options {
    mode: Mode,
    output: PathBuf,
    lanes: Vec<Lane>,
    arm: Option<Arm>,
    repetitions: u32,
}

/// Entry point shared with the executable contract tests.
pub(crate) fn run(arguments: &[String]) -> Result<(), String> {
    if let [command, path] = arguments
        && command == "summarize"
    {
        return summarize(Path::new(path));
    }
    if arguments == ["list"] {
        for lane in Lane::ALL {
            println!("{}", lane.name());
        }
        return Ok(());
    }
    let options = parse(arguments)?;
    if options.mode != Mode::Smoke && cfg!(debug_assertions) {
        return Err("timing requires a release/profiling build".to_owned());
    }
    if let Some(parent) = options.output.parent() {
        fs::create_dir_all(parent).map_err(display_error)?;
    }
    fs::create_dir(&options.output)
        .map_err(|error| format!("output must be a new directory: {error}"))?;
    let fixture = Fixture::create(&options.output.join("input"))?;
    let mut artifacts = Artifacts::new(&options.output)?;
    write_manifest(&options, "running")?;
    if options.mode == Mode::Observe {
        return observe_run(&options, &fixture, &mut artifacts);
    }
    for lane in &options.lanes {
        measure_lane(&options, &fixture, *lane, &mut artifacts)?;
    }
    artifacts.measurements.flush().map_err(display_error)?;
    artifacts.events.flush().map_err(display_error)?;
    write_manifest(&options, "complete")?;
    println!("workload artifacts: {}", options.output.display());
    Ok(())
}

fn summarize(path: &Path) -> Result<(), String> {
    let samples = read_samples(path).map_err(display_error)?;
    if samples.base_ns.len() < 30 {
        return Err("characterization requires at least 30 pairs".to_owned());
    }
    let ratio = ratio_gate(&samples, 10_000);
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "ratio_orientation": "candidate/base",
            "ratio_geomean": ratio.ratio_geomean,
            "ci95_upper": ratio.ratio_ci95_upper,
            "pairs": samples.base_ns.len(),
            "kind": "characterization",
        }))
        .map_err(display_error)?
    );
    Ok(())
}

fn parse(arguments: &[String]) -> Result<Options, String> {
    let usage = "usage: workload_suite list | summarize CSV | (run|smoke|trace|observe) [output] [lane] | profile output lane arm repetitions";
    let (mode, output, selected, arm, repetitions) = match arguments {
        [] => (Mode::Run, None, None, None, REPS),
        [command, rest @ ..]
            if matches!(command.as_str(), "run" | "smoke" | "trace" | "observe") =>
        {
            if rest.len() > 2 {
                return Err(usage.to_owned());
            }
            let mode = match command.as_str() {
                "run" => Mode::Run,
                "trace" => Mode::Trace,
                "observe" => Mode::Observe,
                _ => Mode::Smoke,
            };
            (
                mode,
                rest.first(),
                rest.get(1),
                None,
                if mode == Mode::Smoke { 1 } else { REPS },
            )
        }
        [command, output, lane, arm, count] if command == "profile" => {
            let repetitions = count.parse::<u32>().map_err(display_error)?;
            if !(1..=10_000).contains(&repetitions) {
                return Err("profile repetitions must be 1..=10000".to_owned());
            }
            (
                Mode::Profile,
                Some(output),
                Some(lane),
                Some(Arm::parse(arm)?),
                repetitions,
            )
        }
        _ => return Err(usage.to_owned()),
    };
    let lanes = match selected {
        Some(lane) => vec![Lane::parse(lane)?],
        None => Lane::ALL.to_vec(),
    };
    let output = output.map_or_else(
        || {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            PathBuf::from(format!(
                "target/bench-samples/workloads/{}-{}-{stamp}",
                mode.name(),
                std::process::id()
            ))
        },
        PathBuf::from,
    );
    Ok(Options {
        mode,
        output,
        lanes,
        arm,
        repetitions,
    })
}

fn observe_run(
    options: &Options,
    fixture: &Fixture,
    primary: &mut Artifacts,
) -> Result<(), String> {
    let traced_options = Options {
        output: options.output.join("traced"),
        ..options.clone()
    };
    fs::create_dir(&traced_options.output).map_err(display_error)?;
    let mut traced = Artifacts::new(&traced_options.output)?;
    write_manifest(&traced_options, "running")?;
    for lane in &options.lanes {
        for arm in [Arm::Base, Arm::Candidate] {
            let samples = observe_arm(fixture, *lane, arm, primary, &mut traced)?;
            measure_lane_report(&options.output, &samples)?;
        }
    }
    for artifacts in [primary, &mut traced] {
        artifacts.measurements.flush().map_err(display_error)?;
        artifacts.events.flush().map_err(display_error)?;
    }
    write_manifest(options, "complete")?;
    write_manifest(&traced_options, "complete")?;
    Ok(())
}

fn observe_arm(
    fixture: &Fixture,
    lane: Lane,
    arm: Arm,
    primary: &mut Artifacts,
    traced: &mut Artifacts,
) -> Result<PairedSamples, String> {
    let mut samples = PairedSamples {
        name: format!("instrumentation_{}_{}", lane.name(), arm.name()),
        base_ns: Vec::with_capacity(REPS as usize),
        candidate_ns: Vec::with_capacity(REPS as usize),
    };
    for round in 0..WARMUPS + REPS {
        let modes = if round.is_multiple_of(2) {
            [Mode::Run, Mode::Trace]
        } else {
            [Mode::Trace, Mode::Run]
        };
        for mode in modes {
            let measured = if mode == Mode::Trace {
                engine::measure::<true>(fixture, lane, arm, round, DirectIo::Required)?
            } else {
                engine::measure::<false>(fixture, lane, arm, round, DirectIo::Required)?
            };
            if round < WARMUPS {
                continue;
            }
            if mode == Mode::Trace {
                samples.candidate_ns.push(measured.elapsed_ns);
                traced.record(lane, arm, round - WARMUPS, mode, measured)?;
            } else {
                samples.base_ns.push(measured.elapsed_ns);
                primary.record(lane, arm, round - WARMUPS, mode, measured)?;
            }
        }
    }
    Ok(samples)
}

fn measure_lane(
    options: &Options,
    fixture: &Fixture,
    lane: Lane,
    artifacts: &mut Artifacts,
) -> Result<(), String> {
    let warmups = if options.mode == Mode::Smoke {
        0
    } else {
        WARMUPS
    };
    let mut samples = PairedSamples {
        name: lane.name().to_owned(),
        base_ns: Vec::with_capacity(options.repetitions as usize),
        candidate_ns: Vec::with_capacity(options.repetitions as usize),
    };
    for round in 0..warmups + options.repetitions {
        let arms = if round.is_multiple_of(2) {
            [Arm::Base, Arm::Candidate]
        } else {
            [Arm::Candidate, Arm::Base]
        };
        for arm in arms {
            if let Some(selected) = options.arm
                && arm != selected
            {
                continue;
            }
            let direct = if options.mode == Mode::Smoke {
                DirectIo::Disabled
            } else {
                DirectIo::Required
            };
            let measured = if options.mode == Mode::Trace {
                engine::measure::<true>(fixture, lane, arm, round, direct)?
            } else {
                engine::measure::<false>(fixture, lane, arm, round, direct)?
            };
            if round < warmups {
                continue;
            }
            let elapsed = if lane == Lane::CompactionInterference {
                measured.foreground_ns
            } else {
                measured.elapsed_ns
            };
            match arm {
                Arm::Base => samples.base_ns.push(elapsed),
                Arm::Candidate => samples.candidate_ns.push(elapsed),
            }
            artifacts.record(lane, arm, round - warmups, options.mode, measured)?;
        }
    }
    if matches!(options.mode, Mode::Run | Mode::Trace) {
        measure_lane_report(&options.output, &samples)?;
    }
    Ok(())
}

fn measure_lane_report(output: &Path, samples: &PairedSamples) -> Result<(), String> {
    let path = write_samples(output, samples).map_err(display_error)?;
    let ratio = ratio_gate(samples, 10_000);
    fs::write(
        output.join(format!("{}.ratio.json", samples.name)),
        serde_json::to_vec_pretty(&json!({
            "ratio_orientation": "candidate/base",
            "ratio_geomean": ratio.ratio_geomean,
            "ci95_upper": ratio.ratio_ci95_upper,
            "pairs": samples.base_ns.len(),
            "kind": "characterization",
        }))
        .map_err(display_error)?,
    )
    .map_err(display_error)?;
    println!(
        "{}: candidate/base {:.4}, CI95 upper {:.4}, {}",
        samples.name,
        ratio.ratio_geomean,
        ratio.ratio_ci95_upper,
        path.display()
    );
    Ok(())
}

struct Artifacts {
    measurements: BufWriter<File>,
    events: BufWriter<File>,
    header_written: bool,
    posture: Option<(String, String)>,
}

#[derive(Serialize)]
struct Row {
    lane: Lane,
    arm: Arm,
    pair: u32,
    traced: bool,
    elapsed_ns: u64,
    foreground_ns: u64,
    cpu_ns: Option<u64>,
    registration: String,
    io_mode: String,
    #[serde(flatten)]
    counters: observe::Counters,
}

impl Artifacts {
    fn new(output: &Path) -> Result<Self, String> {
        let measurements =
            BufWriter::new(File::create(output.join("measurements.csv")).map_err(display_error)?);
        let mut events =
            BufWriter::new(File::create(output.join("events.csv")).map_err(display_error)?);
        writeln!(
            events,
            "lane,arm,pair,worker,operation,page,phase,start_ns,end_ns"
        )
        .map_err(display_error)?;
        Ok(Self {
            measurements,
            events,
            header_written: false,
            posture: None,
        })
    }

    fn record(
        &mut self,
        lane: Lane,
        arm: Arm,
        pair: u32,
        mode: Mode,
        measurement: Measurement,
    ) -> Result<(), String> {
        let posture = (
            measurement.registration.clone(),
            measurement.io_mode.clone(),
        );
        if self
            .posture
            .as_ref()
            .is_some_and(|expected| expected != &posture)
        {
            return Err("resolved registration/I/O posture changed; rerun with an explicit DIOS_REGISTRATION_POLICY".to_owned());
        }
        self.posture = Some(posture);
        self.record_events(lane, arm, pair, measurement.events)?;
        let row = serde_json::to_value(Row {
            lane,
            arm,
            pair,
            traced: mode == Mode::Trace,
            elapsed_ns: measurement.elapsed_ns,
            foreground_ns: measurement.foreground_ns,
            cpu_ns: measurement.cpu_ns,
            registration: measurement.registration,
            io_mode: measurement.io_mode,
            counters: measurement.counters,
        })
        .map_err(display_error)?;
        let fields = row
            .as_object()
            .expect("a measurement serializes as an object");
        if !self.header_written {
            writeln!(
                self.measurements,
                "{}",
                fields
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            )
            .map_err(display_error)?;
            self.header_written = true;
        }
        let values: Vec<_> = fields
            .values()
            .map(|value| match value {
                serde_json::Value::String(value) => format!("\"{}\"", value.replace('"', "\"\"")),
                serde_json::Value::Null => String::new(),
                other => other.to_string(),
            })
            .collect();
        writeln!(self.measurements, "{}", values.join(",")).map_err(display_error)
    }

    fn record_events(
        &mut self,
        lane: Lane,
        arm: Arm,
        pair: u32,
        events: Vec<observe::Event>,
    ) -> Result<(), String> {
        for event in events {
            writeln!(
                self.events,
                "{},{},{pair},{},{},{},{},{},{}",
                lane.name(),
                arm.name(),
                event.worker,
                event.operation,
                event.page,
                event.phase,
                event.start_ns,
                event.end_ns
            )
            .map_err(display_error)?;
        }
        Ok(())
    }
}

fn write_manifest(options: &Options, status: &str) -> Result<(), String> {
    let manifest = json!({
        "schema": "dios-workload-suite-v1", "status": status, "mode": options.mode.name(),
        "lanes": options.lanes, "arm": options.arm, "pairs_or_repetitions": options.repetitions,
        "arrival_model": "closed-loop", "ratio": "candidate/base; compaction uses foreground time",
        "direct_io": if options.mode == Mode::Smoke { "disabled" } else { "required" },
        "registration_requested": format!("{:?}", dios::bench::registration_policy_from_env()?),
        "os": std::env::consts::OS, "architecture": std::env::consts::ARCH,
        "backend": if cfg!(target_os = "linux") { "io_uring" } else { "eager-inline" },
        "kernel": command_output("uname", &["-r"]),
        "affinity": fs::read_to_string("/proc/self/status").ok().and_then(|value|
            value.lines().find_map(|line| line.strip_prefix("Cpus_allowed_list:").map(|value| value.trim().to_owned()))),
        "governor": fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor").ok(),
        "rust_version_build": option_env!("DIOS_BUILD_RUST_VERSION"),
        "build_profile": option_env!("DIOS_BUILD_PROFILE"),
        "source_commit_build": option_env!("DIOS_PRODUCT_SOURCE_COMMIT"),
        "source_commit_observed": command_output("git", &["rev-parse", "HEAD"]),
        "worktree_status": command_output("git", &["status", "--porcelain=v1"]),
        "executable_sha256": format!("{:x}", Sha256::digest(fs::read(std::env::current_exe().map_err(display_error)?).map_err(display_error)?)),
        "runner_sha256": runner_hash(),
        "granule_bytes": catalog::GRANULE, "file_count": catalog::FILE_COUNT,
        "file_pages": catalog::FILE_PAGES, "frames": catalog::FRAMES, "window": catalog::WINDOW,
        "priming_pages": [catalog::FILE_COUNT * catalog::FILE_PAGES - 1],
        "fixture_recipe": "interleaved file/page identity; word0=next address; other words=page<<32 + word*0x9e3779b97f4a7c15 wrapping u64",
        "interpretation": "characterization; no service-tail, physical-device, or universal engine-speedup claim",
    });
    fs::write(
        options.output.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).map_err(display_error)?,
    )
    .map_err(display_error)
}

fn runner_hash() -> String {
    let mut digest = Sha256::new();
    for source in [
        include_str!("../workload_suite.rs"),
        include_str!("mod.rs"),
        include_str!("catalog.rs"),
        include_str!("engine.rs"),
        include_str!("fixture.rs"),
        include_str!("observe.rs"),
    ] {
        digest.update(source.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn command_output(command: &str, arguments: &[&str]) -> Option<String> {
    Command::new(command)
        .args(arguments)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|output| output.trim().to_owned())
}
