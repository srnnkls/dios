use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use crate::common::{
    BASE_SOURCE_COMMIT, CANDIDATE_SOURCE_COMMIT, GRANULE_BYTES, Lane, Measurement, PAIR_COUNT,
    PROCESS_HEADER, SEGMENT_LAYOUT, display_error, is_lower_hex, parse_number, sha256_bytes,
    sha256_path,
};
use crate::harness::{self, ProductIdentity};
use crate::platform::{self, HostEvidence};
use crate::workloads;

const INPUT_PAGES: u32 = 256;
const REFUSAL_THRESHOLD: f64 = 0.005;
const REFUSAL_RESAMPLES: u32 = 10_000;
const PROVENANCE_BYTES_MAX: u64 = 64 * 1024 * 1024;
const PROCESS_ROWS_MAX: usize = 80;
const CSV_LINE_BYTES_MAX: usize = 65_536;
const CSV_FIELD_BYTES_MAX: usize = 4_096;
const PROCESS_BYTES_MAX: u64 = 5_308_497;
const PAIRED_BYTES_MAX: u64 = 2_048;

pub(crate) fn prepare_input(path: &Path) -> Result<(), String> {
    if path.exists() {
        return Err(format!("binding input already exists: {}", path.display()));
    }
    let mut bytes = vec![0_u8; INPUT_PAGES as usize * GRANULE_BYTES as usize];
    for page in 0..INPUT_PAGES {
        let base = page as usize * GRANULE_BYTES as usize;
        for offset in 0..GRANULE_BYTES {
            let value = (page * 17 + offset * 73 + 41) % 251;
            bytes[base + offset as usize] = value as u8;
        }
    }
    fs::write(path, bytes).map_err(|error| format!("write {}: {error}", path.display()))?;
    validate_input(path)
}

fn validate_input(path: &Path) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let expected = INPUT_PAGES as usize * GRANULE_BYTES as usize;
    if bytes.len() != expected {
        return Err(format!(
            "binding corpus has {} bytes, expected {expected}",
            bytes.len()
        ));
    }
    for page in 0..INPUT_PAGES {
        let base = page as usize * GRANULE_BYTES as usize;
        for offset in 0..GRANULE_BYTES {
            let expected = ((page * 17 + offset * 73 + 41) % 251) as u8;
            if bytes[base + offset as usize] != expected {
                return Err(format!(
                    "binding corpus differs at page {page} byte {offset}"
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn print_identity() -> Result<(), String> {
    let (identity, runtime_state_matches) = harness::reported_identity()?;
    let bytes = serde_json::to_vec(&harness::identity_json(&identity, runtime_state_matches))
        .map_err(display_error)?;
    println!("{}", String::from_utf8(bytes).map_err(display_error)?);
    Ok(())
}

pub(crate) fn print_host(lane: Lane, input: &Path) -> Result<(), String> {
    let host = platform::binding_host(lane, input)?;
    let bytes = serde_json::to_vec(&host_json(&host)).map_err(display_error)?;
    println!("{}", String::from_utf8(bytes).map_err(display_error)?);
    Ok(())
}

pub(crate) fn drive(
    lane: Lane,
    first_executable: &Path,
    second_executable: &Path,
    input: &Path,
    process: &Path,
    provenance: &Path,
) -> Result<(), String> {
    validate_drive_paths(process, provenance)?;
    validate_input(input)?;
    let first = executable_identity(first_executable)?;
    let second = executable_identity(second_executable)?;
    validate_product_relation(lane, &first, &second)?;
    let host = executable_host(first_executable, lane, input)?;
    write_pre_run_manifest(
        lane,
        [(&first, first_executable), (&second, second_executable)],
        input,
        provenance,
        &host,
    )?;
    run_pairs(
        lane,
        [first_executable, second_executable],
        input,
        process,
        provenance,
    )
}

fn validate_drive_paths(process: &Path, provenance: &Path) -> Result<(), String> {
    if process == provenance {
        return Err("process and provenance paths must differ".to_owned());
    }
    if process.exists() || provenance.exists() {
        return Err("binding artifacts must not exist before repetition zero".to_owned());
    }
    Ok(())
}

fn executable_identity(executable: &Path) -> Result<ProductIdentity, String> {
    let output = Command::new(executable)
        .arg("identity")
        .output()
        .map_err(|error| format!("run {} identity: {error}", executable.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{} identity failed: {}",
            executable.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let identity = harness::parse_identity(&output.stdout)?;
    if identity.executable_sha256 != sha256_path(executable)? {
        return Err(format!(
            "{} changed after identity probe",
            executable.display()
        ));
    }
    Ok(identity)
}

fn executable_host(executable: &Path, lane: Lane, input: &Path) -> Result<Value, String> {
    let output = Command::new("taskset")
        .args(["-c", lane.cpu_set()])
        .arg(executable)
        .args(["host", lane.name()])
        .arg(input)
        .env("DIOS_PFR_CPU_SET", lane.cpu_set())
        .output()
        .map_err(display_error)?;
    if !output.status.success() {
        return Err(format!(
            "host probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(display_error)
}

fn validate_product_relation(
    lane: Lane,
    first: &ProductIdentity,
    second: &ProductIdentity,
) -> Result<(), String> {
    if first.cargo_lock_sha256 != second.cargo_lock_sha256
        || first.harness_cargo_lock_sha256 != second.harness_cargo_lock_sha256
        || first.runner_sha256 != second.runner_sha256
        || first.rust_version != second.rust_version
        || first.build_profile != second.build_profile
    {
        return Err("paired products do not share one immutable build identity".to_owned());
    }
    if lane.source_comparison() {
        if first.source_commit != BASE_SOURCE_COMMIT
            || second.source_commit != CANDIDATE_SOURCE_COMMIT
            || first.executable_sha256 == second.executable_sha256
        {
            return Err(
                "source A/B lane does not use the frozen clean source identities".to_owned(),
            );
        }
    } else if first.source_commit != CANDIDATE_SOURCE_COMMIT
        || second.source_commit != CANDIDATE_SOURCE_COMMIT
        || first.executable_sha256 != second.executable_sha256
    {
        return Err("within-T7 lane does not use one frozen candidate executable".to_owned());
    }
    Ok(())
}

fn write_pre_run_manifest(
    lane: Lane,
    products: [(&ProductIdentity, &Path); 2],
    input: &Path,
    provenance: &Path,
    host: &Value,
) -> Result<(), String> {
    let input = fs::canonicalize(input).map_err(display_error)?;
    let (first_arm, second_arm) = lane.arms();
    let value = json!({
        "schema": "dios-pfr-pre-run-v1",
        "lane": lane.name(),
        "workload": lane.workload(),
        "pair_count": PAIR_COUNT,
        "order": "alternating-base-candidate-v1",
        "arena_posture": "registered",
        "segment_layout": SEGMENT_LAYOUT,
        "fault_rule": "RUSAGE_THREAD:minflt=0:majflt=0",
        "allocation_rule": "post-warmup=0",
        "products": [
            manifest_product(first_arm, products[0].0, products[0].1),
            manifest_product(second_arm, products[1].0, products[1].1),
        ],
        "runner_sha256": products[0].0.runner_sha256,
        "build_profile": products[0].0.build_profile,
        "toolchain": {
            "rust": products[0].0.rust_version,
            "mise": harness::command_output("mise", &["--version"] )?,
        },
        "arguments": ["run", lane.name()],
        "input": {
            "path": input,
            "bytes": fs::metadata(&input).map_err(display_error)?.len(),
            "sha256": sha256_path(&input)?,
        },
        "host": host,
    });
    validate_manifest_contract(lane, &value)?;
    let bytes = serde_json::to_vec_pretty(&value).map_err(display_error)?;
    fs::write(provenance, bytes).map_err(display_error)
}

fn manifest_product(arm: &str, identity: &ProductIdentity, executable: &Path) -> Value {
    json!({
        "arm": arm,
        "source_commit": identity.source_commit,
        "executable_sha256": identity.executable_sha256,
        "cargo_lock_sha256": identity.cargo_lock_sha256,
        "harness_cargo_lock_sha256": identity.harness_cargo_lock_sha256,
        "executable": executable,
    })
}

fn run_pairs(
    lane: Lane,
    executables: [&Path; 2],
    input: &Path,
    process: &Path,
    provenance: &Path,
) -> Result<(), String> {
    let (first_arm, second_arm) = lane.arms();
    for pair in 0..PAIR_COUNT {
        let order = if pair.is_multiple_of(2) {
            "base-candidate"
        } else {
            "candidate-base"
        };
        let arms = if pair.is_multiple_of(2) {
            [(first_arm, executables[0]), (second_arm, executables[1])]
        } else {
            [(second_arm, executables[1]), (first_arm, executables[0])]
        };
        for (arm, executable) in arms {
            run_one(
                lane,
                arm,
                pair,
                order,
                executable,
                input,
                [process, provenance],
            )?;
        }
    }
    Ok(())
}

fn run_one(
    lane: Lane,
    arm: &str,
    pair: u32,
    order: &str,
    executable: &Path,
    input: &Path,
    artifacts: [&Path; 2],
) -> Result<(), String> {
    let status = Command::new("taskset")
        .args(["-c", lane.cpu_set()])
        .arg(executable)
        .args(["run", lane.name(), arm, &pair.to_string(), order])
        .arg(input)
        .arg(artifacts[0])
        .arg(artifacts[1])
        .env("DIOS_PFR_CPU_SET", lane.cpu_set())
        .status()
        .map_err(display_error)?;
    if !status.success() {
        return Err(format!("pair {pair} arm {arm} exited with {status}"));
    }
    Ok(())
}

pub(crate) fn run_process(
    lane: Lane,
    arm: &str,
    pair: u32,
    order: &str,
    input: &Path,
    process: &Path,
    provenance: &Path,
) -> Result<(), String> {
    lane.validate_arm(arm)?;
    validate_pair_arguments(lane, arm, pair, order)?;
    validate_input(input)?;
    let identity = harness::runtime_identity()?;
    let host = platform::binding_host(lane, input)?;
    let provenance_bytes = read_provenance(provenance)?;
    let provenance_sha256 = sha256_bytes(&provenance_bytes);
    let manifest: Value = serde_json::from_slice(&provenance_bytes).map_err(display_error)?;
    validate_process_manifest(lane, arm, &identity, input, &host, &manifest)?;
    platform::pin_current(0)?;
    let measurement = workloads::measure(lane, arm, input)?;
    let row = rich_row(
        lane,
        arm,
        pair,
        order,
        input,
        (&identity, &host, &measurement),
        &provenance_sha256,
    )?;
    append_row(process, &row)
}

fn validate_pair_arguments(lane: Lane, arm: &str, pair: u32, order: &str) -> Result<(), String> {
    if pair >= PAIR_COUNT {
        return Err(format!("pair {pair} exceeds the frozen pair count"));
    }
    let expected = if pair.is_multiple_of(2) {
        "base-candidate"
    } else {
        "candidate-base"
    };
    if order != expected {
        return Err(format!(
            "pair {pair} order is {order:?}, expected {expected:?}"
        ));
    }
    lane.validate_arm(arm)
}

fn validate_process_manifest(
    lane: Lane,
    arm: &str,
    identity: &ProductIdentity,
    input: &Path,
    host: &HostEvidence,
    manifest: &Value,
) -> Result<(), String> {
    validate_manifest_contract(lane, manifest)?;
    let product = manifest_product_for_arm(manifest, arm)?;
    if product["source_commit"] != identity.source_commit
        || product["executable_sha256"] != identity.executable_sha256
        || product["cargo_lock_sha256"] != identity.cargo_lock_sha256
        || product["harness_cargo_lock_sha256"] != identity.harness_cargo_lock_sha256
        || manifest["runner_sha256"] != identity.runner_sha256
        || manifest["build_profile"] != identity.build_profile
        || manifest["input"]["sha256"] != sha256_path(input)?
        || manifest["host"] != host_json(host)
    {
        return Err(
            "process source, executable, input, runner, or host differs from provenance".to_owned(),
        );
    }
    Ok(())
}

fn rich_row(
    lane: Lane,
    arm: &str,
    pair: u32,
    order: &str,
    input: &Path,
    evidence: (&ProductIdentity, &HostEvidence, &Measurement),
    provenance_sha256: &str,
) -> Result<String, String> {
    let (identity, host, measurement) = evidence;
    let input = fs::canonicalize(input).map_err(display_error)?;
    let arguments = argument_sha256(lane, arm, pair, order, &input, provenance_sha256);
    let (process_id, start_ticks) = process_identity()?;
    let fields = row_fields(
        lane,
        arm,
        pair,
        order,
        process_id,
        start_ticks,
        (
            identity,
            host,
            measurement,
            &arguments,
            provenance_sha256,
            &sha256_path(&input)?,
        ),
    );
    fields
        .iter()
        .map(|field| csv_encode(field))
        .collect::<Result<Vec<_>, _>>()
        .map(|fields| fields.join(","))
}

fn row_fields(
    lane: Lane,
    arm: &str,
    pair: u32,
    order: &str,
    process_id: u32,
    start_ticks: u64,
    details: (
        &ProductIdentity,
        &HostEvidence,
        &Measurement,
        &str,
        &str,
        &str,
    ),
) -> Vec<String> {
    let (identity, host, measurement, arguments, provenance, corpus) = details;
    let mut fields = vec![
        lane.name().to_owned(),
        pair.to_string(),
        order.to_owned(),
        arm.to_owned(),
        lane.workload().to_owned(),
        process_id.to_string(),
        start_ticks.to_string(),
        identity.source_commit.clone(),
        identity.executable_sha256.clone(),
        identity.cargo_lock_sha256.clone(),
        identity.harness_cargo_lock_sha256.clone(),
        identity.rust_version.clone(),
        identity.runner_sha256.clone(),
        identity.build_profile.clone(),
        arguments.to_owned(),
        provenance.to_owned(),
    ];
    fields.extend(row_measurement_fields(lane, arm, host, measurement, corpus));
    fields
}

fn row_measurement_fields(
    lane: Lane,
    arm: &str,
    host: &HostEvidence,
    measurement: &Measurement,
    corpus: &str,
) -> Vec<String> {
    let faults_minor = fault_list(&measurement.threads.faults, |fault| fault.minor);
    let faults_major = fault_list(&measurement.threads.faults, |fault| fault.major);
    vec![
        measurement.iterations.to_string(),
        measurement.useful_operations.to_string(),
        measurement.useful_bytes.to_string(),
        measurement.elapsed_ns.to_string(),
        format!("{:016x}", measurement.checksum),
        measurement.allocations.to_string(),
        host.cpu_set.clone(),
        measurement.threads.affinities.clone(),
        faults_minor,
        faults_major,
        measurement.pool_capacity.to_string(),
        measurement.retained_pages.to_string(),
        "registered".to_owned(),
        format!("{:x}", measurement.arena.base),
        measurement.arena.span.to_string(),
        GRANULE_BYTES.to_string(),
        measurement.arena.kernel_page_bytes.to_string(),
        measurement.arena.mmu_page_bytes.to_string(),
        measurement.arena.anon_huge_bytes.to_string(),
        SEGMENT_LAYOUT.to_owned(),
        corpus.to_owned(),
        host.memlock_soft.to_string(),
        host.memlock_hard.to_string(),
        lane.retention_budget(arm).to_string(),
        measurement.retention.occupied_budget.to_string(),
        measurement.retention.refused_budget.to_string(),
        measurement.retention.refused_ceiling.to_string(),
        measurement.retention.refused_contention.to_string(),
        measurement.retention.refused_retiring.to_string(),
        measurement.retention.retained_evictions_held.to_string(),
        measurement.reclaimed_frames.to_string(),
        measurement.backend_completions.to_string(),
        measurement.evictions.to_string(),
        measurement.wake_cycles.to_string(),
        measurement.parked_wakes.to_string(),
        measurement.wake_acks.to_string(),
        measurement.ring_drains.to_string(),
        measurement.held_transitions.to_string(),
        host.governor.clone(),
        host.kernel.clone(),
        host.numa_nodes.clone(),
        measurement.arena.numa_policy.clone(),
        host.numa_balancing.clone(),
        host.storage.clone(),
        host.runner_host.clone(),
        host.os_hostname.clone(),
        host.cpu_model.clone(),
        host.cpu_topology.clone(),
    ]
}

fn fault_list(
    faults: &[crate::common::FaultDelta],
    field: impl Fn(&crate::common::FaultDelta) -> u64,
) -> String {
    faults
        .iter()
        .map(|fault| field(fault).to_string())
        .collect::<Vec<_>>()
        .join(";")
}

fn append_row(path: &Path, row: &str) -> Result<(), String> {
    let header = path.metadata().map_or(true, |metadata| metadata.len() == 0);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(display_error)?;
    if header {
        writeln!(file, "{PROCESS_HEADER}").map_err(display_error)?;
    }
    writeln!(file, "{row}").map_err(display_error)
}

fn argument_sha256(
    lane: Lane,
    arm: &str,
    pair: u32,
    order: &str,
    input: &Path,
    provenance: &str,
) -> String {
    let value = format!(
        "run\0{}\0{arm}\0{pair}\0{order}\0{}\0{provenance}",
        lane.name(),
        input.display()
    );
    sha256_bytes(value.as_bytes())
}

#[cfg(target_os = "linux")]
fn process_identity() -> Result<(u32, u64), String> {
    let process_id = std::process::id();
    let stat = fs::read_to_string("/proc/self/stat").map_err(display_error)?;
    let tail = stat
        .rsplit_once(')')
        .map(|(_, tail)| tail)
        .ok_or_else(|| "Linux process stat has no command terminator".to_owned())?;
    let fields = tail.split_whitespace().collect::<Vec<_>>();
    let start_ticks = fields
        .get(19)
        .ok_or_else(|| "Linux process stat has no start time".to_owned())?;
    Ok((
        process_id,
        parse_number(start_ticks, "process start ticks")?,
    ))
}

#[cfg(not(target_os = "linux"))]
fn process_identity() -> Result<(u32, u64), String> {
    let _ = std::thread::available_parallelism().map_err(display_error)?;
    Ok((std::process::id(), 1))
}

fn host_json(host: &HostEvidence) -> Value {
    json!({
        "cpu_set": host.cpu_set,
        "governor": host.governor,
        "kernel": host.kernel,
        "numa_nodes": host.numa_nodes,
        "numa_balancing": host.numa_balancing,
        "storage": host.storage,
        "runner_host": host.runner_host,
        "os_hostname": host.os_hostname,
        "cpu_model": host.cpu_model,
        "cpu_topology": host.cpu_topology,
        "memlock_soft": host.memlock_soft,
        "memlock_hard": host.memlock_hard,
    })
}

pub(crate) fn validate_pairs(
    lane: Lane,
    process: &Path,
    paired: &Path,
    provenance: &Path,
) -> Result<(), String> {
    if lane == Lane::SameFramePromotion {
        return Err("same-frame promotion uses refusal-gate, not timing pairs".to_owned());
    }
    if process == paired || process == provenance || paired == provenance {
        return Err("process, paired, and provenance paths must be distinct".to_owned());
    }
    let manifest_bytes = read_provenance(provenance)?;
    let manifest: Value = serde_json::from_slice(&manifest_bytes).map_err(display_error)?;
    validate_manifest_contract(lane, &manifest)?;
    let pre_run_sha256 = pre_run_manifest_sha256(&manifest, &manifest_bytes)?;
    let validator_sha256 = validate_validator_identity(&manifest)?;
    let (rows, process_sha256) = read_rows(process)?;
    validate_rows(lane, &rows, &manifest, &pre_run_sha256)?;
    let paired_text = paired_text(lane, &rows)?;
    fs::write(paired, paired_text).map_err(display_error)?;
    enrich_provenance(
        process,
        Some(paired),
        provenance,
        manifest,
        (&pre_run_sha256, &validator_sha256, &process_sha256),
        None,
    )
}

fn read_rows(path: &Path) -> Result<(Vec<Row>, String), String> {
    let (file, _) = open_bounded(path, "process CSV", PROCESS_BYTES_MAX)?;
    let mut reader = BufReader::new(file);
    let mut digest = Sha256::new();
    let mut line = Vec::with_capacity(1024);
    if !read_csv_line(&mut reader, &mut line, 1, &mut digest)?
        || std::str::from_utf8(&line).map_err(display_error)? != PROCESS_HEADER
    {
        return Err("process CSV header differs from the frozen PFR schema".to_owned());
    }
    let mut rows = Vec::with_capacity(PROCESS_ROWS_MAX);
    for line_number in 2..=PROCESS_ROWS_MAX + 2 {
        if !read_csv_line(&mut reader, &mut line, line_number, &mut digest)? {
            break;
        }
        if rows.len() == PROCESS_ROWS_MAX {
            return Err(format!(
                "process CSV exceeds the {PROCESS_ROWS_MAX}-row limit"
            ));
        }
        let source = std::str::from_utf8(&line).map_err(display_error)?;
        rows.push(Row::parse(line_number, source)?);
    }
    let expected = usize::try_from(PAIR_COUNT * 2).expect("fixed row count fits usize");
    if rows.len() != expected {
        return Err(format!(
            "process CSV has {} rows, expected {expected}",
            rows.len()
        ));
    }
    Ok((rows, format!("{:x}", digest.finalize())))
}

fn read_provenance(path: &Path) -> Result<Vec<u8>, String> {
    let (mut file, capacity) = open_bounded(path, "provenance", PROVENANCE_BYTES_MAX)?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut chunk = [0_u8; 8192];
    loop {
        let count = file.read(&mut chunk).map_err(display_error)?;
        if count == 0 {
            break;
        }
        let next = bytes
            .len()
            .checked_add(count)
            .ok_or_else(|| "provenance byte count overflowed".to_owned())?;
        if u64::try_from(next).map_err(display_error)? > PROVENANCE_BYTES_MAX {
            return Err(format!(
                "provenance exceeds byte limit {PROVENANCE_BYTES_MAX}"
            ));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    Ok(bytes)
}

fn open_bounded(path: &Path, label: &str, bytes_max: u64) -> Result<(File, usize), String> {
    let path_metadata = fs::metadata(path).map_err(display_error)?;
    if !path_metadata.file_type().is_file() {
        return Err(format!("{label} is not a regular file"));
    }
    if path_metadata.len() > bytes_max {
        return Err(format!("{label} exceeds byte limit {bytes_max}"));
    }
    let file = File::open(path).map_err(display_error)?;
    let metadata = file.metadata().map_err(display_error)?;
    if !metadata.file_type().is_file() {
        return Err(format!("{label} is not a regular file"));
    }
    if metadata.len() > bytes_max {
        return Err(format!("{label} exceeds byte limit {bytes_max}"));
    }
    let bytes = usize::try_from(metadata.len()).map_err(display_error)?;
    Ok((file, bytes))
}

fn sha256_bounded_path(path: &Path, label: &str, bytes_max: u64) -> Result<String, String> {
    let (mut file, _) = open_bounded(path, label, bytes_max)?;
    let mut digest = Sha256::new();
    let mut bytes = 0_u64;
    let mut chunk = [0_u8; 8192];
    loop {
        let count = file.read(&mut chunk).map_err(display_error)?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(u64::try_from(count).map_err(display_error)?)
            .ok_or_else(|| format!("{label} byte count overflowed"))?;
        if bytes > bytes_max {
            return Err(format!("{label} exceeds byte limit {bytes_max}"));
        }
        digest.update(&chunk[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn read_csv_line(
    reader: &mut BufReader<File>,
    line: &mut Vec<u8>,
    line_number: usize,
    digest: &mut Sha256,
) -> Result<bool, String> {
    line.clear();
    let read_max = u64::try_from(CSV_LINE_BYTES_MAX + 2).expect("line bound fits u64");
    let mut bounded = reader.take(read_max);
    if bounded.read_until(b'\n', line).map_err(display_error)? == 0 {
        return Ok(false);
    }
    digest.update(&*line);
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
    if line.len() > CSV_LINE_BYTES_MAX {
        return Err(format!(
            "CSV line {line_number} exceeds byte limit {CSV_LINE_BYTES_MAX}"
        ));
    }
    Ok(true)
}

#[derive(Debug)]
struct Row {
    fields: Vec<String>,
}

impl Row {
    fn parse(line: usize, source: &str) -> Result<Self, String> {
        let fields = csv_fields(source, line)?;
        let expected = PROCESS_HEADER.split(',').count();
        if fields.len() != expected {
            return Err(format!(
                "line {line} has {} fields, expected {expected}",
                fields.len()
            ));
        }
        Ok(Self { fields })
    }

    fn field(&self, name: &str) -> &str {
        let index = PROCESS_HEADER
            .split(',')
            .position(|field| field == name)
            .expect("validator field belongs to the fixed header");
        &self.fields[index]
    }

    fn number<T: std::str::FromStr>(&self, name: &str) -> Result<T, String> {
        parse_number(self.field(name), name)
    }
}

fn validate_rows(
    lane: Lane,
    rows: &[Row],
    manifest: &Value,
    provenance_sha256: &str,
) -> Result<(), String> {
    let mut processes = HashSet::with_capacity(rows.len());
    for pair in 0..PAIR_COUNT {
        let index = usize::try_from(pair * 2).expect("fixed pair index fits usize");
        validate_pair(lane, pair, [&rows[index], &rows[index + 1]])?;
    }
    for row in rows {
        validate_row(lane, row, manifest, provenance_sha256)?;
        let process = (row.field("process_id"), row.field("process_start_ticks"));
        if !processes.insert(process) {
            return Err("binding rows do not represent fresh arm processes".to_owned());
        }
    }
    validate_arm_identities(lane, rows, manifest)
}

fn validate_pair(lane: Lane, pair: u32, rows: [&Row; 2]) -> Result<(), String> {
    let (first, second) = lane.arms();
    let order = if pair.is_multiple_of(2) {
        "base-candidate"
    } else {
        "candidate-base"
    };
    let arms = if pair.is_multiple_of(2) {
        [first, second]
    } else {
        [second, first]
    };
    for index in 0..2 {
        if rows[index].number::<u32>("pair")? != pair
            || rows[index].field("order") != order
            || rows[index].field("arm") != arms[index]
            || rows[index].field("lane") != lane.name()
        {
            return Err(format!("pair {pair} is missing, duplicated, or reordered"));
        }
    }
    if lane == Lane::SameFramePromotion {
        validate_promotion_pair(pair, rows)?;
    } else if rows[0].field("checksum") != rows[1].field("checksum")
        || rows[0].field("useful_bytes") != rows[1].field("useful_bytes")
    {
        return Err(format!("pair {pair} has unequal useful work or checksum"));
    }
    Ok(())
}

fn validate_promotion_pair(pair: u32, rows: [&Row; 2]) -> Result<(), String> {
    let one = rows
        .iter()
        .find(|row| row.field("arm") == "one_worker")
        .expect("validated one-worker arm");
    let eight = rows
        .iter()
        .find(|row| row.field("arm") == "eight_workers")
        .expect("validated eight-worker arm");
    let one_bytes = one.number::<u64>("useful_bytes")?;
    let one_checksum = u64::from_str_radix(one.field("checksum"), 16).map_err(display_error)?;
    let expected_bytes = one_bytes
        .checked_mul(8)
        .ok_or_else(|| "one-worker useful bytes overflow eight workers".to_owned())?;
    let expected_checksum = one_checksum
        .checked_mul(8)
        .ok_or_else(|| "one-worker checksum overflows eight workers".to_owned())?;
    if eight.number::<u64>("useful_bytes")? != expected_bytes
        || u64::from_str_radix(eight.field("checksum"), 16).map_err(display_error)?
            != expected_checksum
    {
        return Err(format!(
            "pair {pair} does not preserve eight-to-one promotion work"
        ));
    }
    Ok(())
}

fn validate_row(
    lane: Lane,
    row: &Row,
    manifest: &Value,
    provenance_sha256: &str,
) -> Result<(), String> {
    let arm = row.field("arm");
    validate_row_identity(lane, row, manifest, provenance_sha256)?;
    validate_row_runtime(lane, arm, row, manifest)?;
    validate_row_arena(lane, row)?;
    validate_lane_row(lane, arm, row)?;
    Ok(())
}

fn validate_row_identity(
    lane: Lane,
    row: &Row,
    manifest: &Value,
    provenance_sha256: &str,
) -> Result<(), String> {
    let product = manifest_product_for_arm(manifest, row.field("arm"))?;
    if row.field("lane") != lane.name()
        || row.field("workload") != lane.workload()
        || row.field("source_commit") != product["source_commit"]
        || row.field("executable_sha256") != product["executable_sha256"]
        || row.field("cargo_lock_sha256") != product["cargo_lock_sha256"]
        || row.field("harness_cargo_lock_sha256") != product["harness_cargo_lock_sha256"]
        || row.field("rust_version") != manifest["toolchain"]["rust"]
        || row.field("runner_sha256") != manifest["runner_sha256"]
        || row.field("build_profile") != manifest["build_profile"]
        || row.field("provenance_sha256") != provenance_sha256
        || row.field("corpus_sha256") != manifest["input"]["sha256"]
    {
        return Err(
            "row source, executable, runner, workload, corpus, or provenance differs".to_owned(),
        );
    }
    validate_hex_fields(row)
}

fn validate_hex_fields(row: &Row) -> Result<(), String> {
    for (field, len) in [
        ("source_commit", 40),
        ("executable_sha256", 64),
        ("cargo_lock_sha256", 64),
        ("harness_cargo_lock_sha256", 64),
        ("runner_sha256", 64),
        ("arguments_sha256", 64),
        ("provenance_sha256", 64),
        ("checksum", 16),
        ("corpus_sha256", 64),
    ] {
        if !is_lower_hex(row.field(field), len) {
            return Err(format!("row has invalid {field}"));
        }
    }
    Ok(())
}

fn validate_row_runtime(lane: Lane, arm: &str, row: &Row, manifest: &Value) -> Result<(), String> {
    let input = manifest["input"]["path"]
        .as_str()
        .ok_or_else(|| "manifest input path is missing".to_owned())?;
    let expected_arguments = argument_sha256(
        lane,
        arm,
        row.number("pair")?,
        row.field("order"),
        Path::new(input),
        row.field("provenance_sha256"),
    );
    if row.field("arguments_sha256") != expected_arguments
        || row.field("cpu_set") != lane.cpu_set()
        || row.field("governor") != "performance"
        || row.field("kernel") != "6.6.64"
        || row.field("runner_host") != manifest["host"]["runner_host"]
        || row.field("os_hostname") != manifest["host"]["os_hostname"]
        || !row
            .field("cpu_model")
            .contains("AMD Ryzen Threadripper 3970X")
        || row.field("cpu_topology") != manifest["host"]["cpu_topology"]
        || !row.field("storage").contains("Samsung SSD 970 PRO")
        || row.number::<u64>("memlock_soft")? != 8_388_608
        || row.number::<u64>("memlock_hard")? != 8_388_608
    {
        return Err(
            "row invocation, affinity, governor, or frozen host identity differs".to_owned(),
        );
    }
    validate_thread_evidence(lane, arm, row)
}

fn validate_thread_evidence(lane: Lane, arm: &str, row: &Row) -> Result<(), String> {
    let expected_affinity = match lane {
        Lane::PromoteReleaseWake => "0;1",
        Lane::SameFramePromotion if arm == "eight_workers" => "0;1;2;3;32;33;34;35",
        Lane::TransientGuard
        | Lane::NestedTransientGuard
        | Lane::NonzeroPoll
        | Lane::ZeroBudgetBypass
        | Lane::SameFramePromotion => "0",
    };
    if row.field("thread_affinities") != expected_affinity {
        return Err("row thread affinities differ from the frozen worker topology".to_owned());
    }
    let thread_count = expected_affinity.split(';').count();
    for field in ["thread_minflt", "thread_majflt"] {
        let values = row.field(field).split(';').collect::<Vec<_>>();
        if values.len() != thread_count
            || values
                .iter()
                .any(|value| parse_number::<u64>(value, field).is_err() || *value != "0")
        {
            return Err(format!(
                "row {field} violates the registered zero-fault rule"
            ));
        }
    }
    if row.number::<u64>("allocations")? != 0 {
        return Err("row violates the zero post-warmup allocation rule".to_owned());
    }
    Ok(())
}

fn validate_row_arena(lane: Lane, row: &Row) -> Result<(), String> {
    let base = usize::from_str_radix(row.field("arena_base"), 16).map_err(display_error)?;
    let span = u64::from(lane.pool_capacity()) * u64::from(GRANULE_BYTES);
    if row.field("arena_posture") != "registered"
        || row.field("segment_layout") != SEGMENT_LAYOUT
        || row.number::<u32>("frame_bytes")? != GRANULE_BYTES
        || row.number::<u64>("arena_span")? != span
        || base % GRANULE_BYTES as usize != 0
        || row.number::<u64>("kernel_page_bytes")? != 4096
        || row.number::<u64>("mmu_page_bytes")? != 4096
        || row.number::<u64>("anon_huge_bytes")? != 0
        || row.field("numa_policy").is_empty()
        || row.field("numa_nodes").is_empty()
        || row.field("numa_balancing").is_empty()
    {
        return Err("row arena, page-size, layout, or NUMA identity differs".to_owned());
    }
    Ok(())
}

fn validate_lane_row(lane: Lane, arm: &str, row: &Row) -> Result<(), String> {
    if row.number::<u64>("iterations")? != lane.iterations()
        || row.number::<u64>("elapsed_ns")? == 0
        || row.number::<u32>("pool_capacity")? != lane.pool_capacity()
        || row.number::<u32>("retention_budget")? != lane.retention_budget(arm)
    {
        return Err("row iteration, timing, pool, or retention-budget identity differs".to_owned());
    }
    match lane {
        Lane::TransientGuard | Lane::NestedTransientGuard => validate_transient(row),
        Lane::NonzeroPoll => validate_nonzero_poll(row),
        Lane::ZeroBudgetBypass => validate_zero_budget(row),
        Lane::PromoteReleaseWake => validate_wake(arm, row),
        Lane::SameFramePromotion => validate_promotion(arm, row),
    }
}

fn validate_transient(row: &Row) -> Result<(), String> {
    require_numbers(
        row,
        &[
            ("useful_operations", 8192),
            ("useful_bytes", 524_288),
            ("retained_pages", 0),
            ("reclaimed_frames", 0),
            ("backend_completions", 0),
            ("evictions", 0),
        ],
    )?;
    require_zero_retention(row)
}

fn validate_nonzero_poll(row: &Row) -> Result<(), String> {
    require_numbers(
        row,
        &[
            ("useful_operations", 16_384),
            ("useful_bytes", 1_048_576),
            ("retained_pages", 0),
            ("reclaimed_frames", 16_384),
            ("backend_completions", 16_384),
            ("evictions", 16_384),
        ],
    )?;
    require_zero_retention(row)
}

fn validate_zero_budget(row: &Row) -> Result<(), String> {
    require_numbers(
        row,
        &[
            ("useful_operations", 256),
            ("useful_bytes", 1_835_008),
            ("retained_pages", 0),
            ("reclaimed_frames", 4096),
            ("backend_completions", 4096),
            ("evictions", 4096),
        ],
    )?;
    require_zero_retention(row)
}

fn validate_wake(arm: &str, row: &Row) -> Result<(), String> {
    let retained = u64::from(arm == "retained");
    require_numbers(
        row,
        &[
            ("useful_operations", 4096),
            ("useful_bytes", 262_144),
            ("retained_pages", retained),
            ("reclaimed_frames", 64),
            ("backend_completions", 64),
            ("evictions", 64),
            ("wake_cycles", 64),
            ("parked_wakes", 64),
            ("wake_acks", 64),
            ("ring_drains", 64 * retained),
            ("held_transitions", 64 * retained),
            ("retained_evictions_held", 64 * retained),
        ],
    )?;
    require_zero_refusals(row)
}

fn validate_promotion(arm: &str, row: &Row) -> Result<(), String> {
    let workers = if arm == "eight_workers" { 8 } else { 1 };
    require_numbers(
        row,
        &[
            ("useful_operations", 1_000_000 * workers),
            ("useful_bytes", 64_000_000 * workers),
            ("retained_pages", 1),
            ("occupied_budget", 0),
            ("refused_budget", 0),
            ("refused_ceiling", 0),
            ("refused_retiring", 0),
            ("retained_evictions_held", 0),
        ],
    )?;
    if arm == "one_worker" && row.number::<u64>("refused_contention")? != 0 {
        return Err("one-worker control recorded contention refusal".to_owned());
    }
    Ok(())
}

fn require_numbers(row: &Row, expected: &[(&str, u64)]) -> Result<(), String> {
    for &(field, value) in expected {
        if row.number::<u64>(field)? != value {
            return Err(format!("row {field} differs from frozen value {value}"));
        }
    }
    Ok(())
}

fn require_zero_retention(row: &Row) -> Result<(), String> {
    require_numbers(
        row,
        &[
            ("occupied_budget", 0),
            ("refused_budget", 0),
            ("refused_ceiling", 0),
            ("refused_contention", 0),
            ("refused_retiring", 0),
            ("retained_evictions_held", 0),
            ("wake_cycles", 0),
            ("parked_wakes", 0),
            ("wake_acks", 0),
            ("ring_drains", 0),
            ("held_transitions", 0),
        ],
    )
}

fn require_zero_refusals(row: &Row) -> Result<(), String> {
    require_numbers(
        row,
        &[
            ("occupied_budget", 0),
            ("refused_budget", 0),
            ("refused_ceiling", 0),
            ("refused_contention", 0),
            ("refused_retiring", 0),
        ],
    )
}

fn validate_arm_identities(lane: Lane, rows: &[Row], manifest: &Value) -> Result<(), String> {
    let (first_arm, second_arm) = lane.arms();
    let first = rows
        .iter()
        .find(|row| row.field("arm") == first_arm)
        .expect("validated first arm");
    let second = rows
        .iter()
        .find(|row| row.field("arm") == second_arm)
        .expect("validated second arm");
    validate_product_relation(lane, &row_identity(first), &row_identity(second))?;
    for row in rows {
        let identity = if row.field("arm") == first_arm {
            first
        } else {
            second
        };
        for field in [
            "source_commit",
            "executable_sha256",
            "cargo_lock_sha256",
            "harness_cargo_lock_sha256",
            "rust_version",
            "runner_sha256",
            "build_profile",
        ] {
            if row.field(field) != identity.field(field) {
                return Err(format!(
                    "arm {} changed {field} across processes",
                    row.field("arm")
                ));
            }
        }
    }
    validate_manifest_contract(lane, manifest)
}

fn row_identity(row: &Row) -> ProductIdentity {
    ProductIdentity {
        source_commit: row.field("source_commit").to_owned(),
        executable_sha256: row.field("executable_sha256").to_owned(),
        cargo_lock_sha256: row.field("cargo_lock_sha256").to_owned(),
        harness_cargo_lock_sha256: row.field("harness_cargo_lock_sha256").to_owned(),
        rust_version: row.field("rust_version").to_owned(),
        runner_sha256: row.field("runner_sha256").to_owned(),
        build_profile: row.field("build_profile").to_owned(),
    }
}

fn paired_text(lane: Lane, rows: &[Row]) -> Result<String, String> {
    let (first_arm, second_arm) = lane.arms();
    let mut output = String::from("base_ns,candidate_ns\n");
    for pair in 0..PAIR_COUNT {
        let index = usize::try_from(pair * 2).expect("fixed pair index fits usize");
        let pair_rows = [&rows[index], &rows[index + 1]];
        let first = pair_rows
            .iter()
            .find(|row| row.field("arm") == first_arm)
            .ok_or_else(|| format!("pair {pair} lost its base arm"))?;
        let second = pair_rows
            .iter()
            .find(|row| row.field("arm") == second_arm)
            .ok_or_else(|| format!("pair {pair} lost its candidate arm"))?;
        writeln!(
            output,
            "{},{}",
            first.field("elapsed_ns"),
            second.field("elapsed_ns")
        )
        .expect("writing paired rows to a String cannot fail");
    }
    Ok(output)
}

pub(crate) fn refusal_gate(process: &Path, threshold: f64, resamples: u32) -> Result<(), String> {
    if threshold != REFUSAL_THRESHOLD || resamples != REFUSAL_RESAMPLES {
        return Err("refusal gate requires threshold 0.005 and 10000 resamples".to_owned());
    }
    let provenance = refusal_provenance_path(process)?;
    let (rows, process_sha256) = read_rows(process)?;
    let manifest_bytes = read_provenance(&provenance)?;
    let manifest: Value = serde_json::from_slice(&manifest_bytes).map_err(display_error)?;
    validate_manifest_contract(Lane::SameFramePromotion, &manifest)?;
    let pre_run_sha256 = pre_run_manifest_sha256(&manifest, &manifest_bytes)?;
    let validator_sha256 = validate_validator_identity(&manifest)?;
    validate_rows(Lane::SameFramePromotion, &rows, &manifest, &pre_run_sha256)?;
    let rates = candidate_refusal_rates(&rows)?;
    let (rate, upper) = bootstrap_refusal(&rates, resamples)?;
    let passed = upper <= threshold;
    println!(
        "refusal_rate={rate:.8} ci95_upper={upper:.8} threshold={threshold:.8} {}",
        if passed { "PASS" } else { "FAIL" }
    );
    enrich_provenance(
        process,
        None,
        &provenance,
        manifest,
        (&pre_run_sha256, &validator_sha256, &process_sha256),
        Some((rate, upper, threshold)),
    )?;
    if passed {
        Ok(())
    } else {
        Err(format!(
            "refusal CI95 upper {upper:.8} exceeds {threshold:.8}"
        ))
    }
}

fn refusal_provenance_path(process: &Path) -> Result<PathBuf, String> {
    let file = process
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "process artifact has no UTF-8 file name".to_owned())?;
    let prefix = file
        .strip_suffix("_process.csv")
        .ok_or_else(|| "refusal process artifact must end in _process.csv".to_owned())?;
    Ok(process.with_file_name(format!("{prefix}_provenance.json")))
}

fn candidate_refusal_rates(rows: &[Row]) -> Result<Vec<f64>, String> {
    let mut rates = Vec::with_capacity(PAIR_COUNT as usize);
    for pair in 0..PAIR_COUNT {
        let index = usize::try_from(pair * 2).expect("fixed pair index fits usize");
        let pair_rows = [&rows[index], &rows[index + 1]];
        let candidate = pair_rows
            .iter()
            .find(|row| row.field("arm") == "eight_workers")
            .ok_or_else(|| format!("pair {pair} has no contended arm"))?;
        let refused = candidate.number::<u32>("refused_contention")?;
        let attempts = candidate.number::<u32>("useful_operations")?;
        rates.push(f64::from(refused) / f64::from(attempts));
    }
    Ok(rates)
}

fn bootstrap_refusal(rates: &[f64], resamples: u32) -> Result<(f64, f64), String> {
    if rates.len() != PAIR_COUNT as usize || resamples == 0 {
        return Err("refusal bootstrap requires 40 rates and positive resamples".to_owned());
    }
    let denominator = f64::from(PAIR_COUNT);
    let rate = rates.iter().sum::<f64>() / denominator;
    let mut state = 0x5046_5252_4154_4531_u64;
    let mut means = Vec::with_capacity(resamples as usize);
    let rate_count = u64::try_from(rates.len()).map_err(display_error)?;
    for _ in 0..resamples {
        let mut sum = 0.0;
        for _ in 0..rates.len() {
            let sampled = splitmix64(&mut state) % rate_count;
            let index = usize::try_from(sampled).map_err(display_error)?;
            sum += rates[index];
        }
        means.push(sum / denominator);
    }
    means.sort_by(f64::total_cmp);
    let index =
        usize::try_from((u64::from(resamples) * 95).div_ceil(100) - 1).map_err(display_error)?;
    Ok((rate, means[index]))
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut value = *state;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn pre_run_manifest_sha256(manifest: &Value, bytes: &[u8]) -> Result<String, String> {
    let Some(recorded) = manifest.get("pre_run_manifest_sha256") else {
        return Ok(sha256_bytes(bytes));
    };
    let recorded = recorded
        .as_str()
        .ok_or_else(|| "pre-run manifest hash is not a string".to_owned())?;
    if !is_lower_hex(recorded, 64) {
        return Err("pre-run manifest hash is malformed".to_owned());
    }
    Ok(recorded.to_owned())
}

fn validate_validator_identity(manifest: &Value) -> Result<String, String> {
    let runner = harness::runner_sha256()?;
    if manifest["runner_sha256"] != runner {
        return Err("validator runner differs from the measured product runner".to_owned());
    }
    let executable = std::env::current_exe().map_err(display_error)?;
    let executable_sha256 = sha256_path(&executable)?;
    if let Some(recorded) = manifest.get("validator_executable_sha256")
        && recorded != &executable_sha256
    {
        return Err("validator executable differs from published provenance".to_owned());
    }
    Ok(executable_sha256)
}

fn enrich_provenance(
    process: &Path,
    paired: Option<&Path>,
    provenance: &Path,
    mut manifest: Value,
    hashes: (&str, &str, &str),
    refusal: Option<(f64, f64, f64)>,
) -> Result<(), String> {
    let (pre_run_sha256, validator_sha256, process_sha256) = hashes;
    let current_process_sha256 = sha256_bounded_path(process, "process CSV", PROCESS_BYTES_MAX)?;
    if current_process_sha256 != process_sha256 {
        return Err("process CSV changed during validation".to_owned());
    }
    let object = manifest
        .as_object_mut()
        .ok_or_else(|| "provenance root is not an object".to_owned())?;
    insert(object, "pre_run_manifest_sha256", json!(pre_run_sha256))?;
    insert(
        object,
        "validator_executable_sha256",
        json!(validator_sha256),
    )?;
    insert(object, "process_csv_sha256", json!(process_sha256))?;
    if let Some(paired) = paired {
        let paired_sha256 = sha256_bounded_path(paired, "paired CSV", PAIRED_BYTES_MAX)?;
        insert(object, "paired_csv_sha256", json!(paired_sha256))?;
    }
    if let Some((rate, upper, threshold)) = refusal {
        insert(object, "refusal_rate", json!(rate))?;
        insert(object, "refusal_ci95_upper", json!(upper))?;
        insert(object, "refusal_threshold", json!(threshold))?;
    }
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(display_error)?;
    fs::write(provenance, bytes).map_err(display_error)
}

fn insert(object: &mut Map<String, Value>, field: &str, value: Value) -> Result<(), String> {
    if let Some(existing) = object.get(field) {
        if existing == &value {
            return Ok(());
        }
        return Err(format!("published provenance changed {field}"));
    }
    object.insert(field.to_owned(), value);
    Ok(())
}

fn validate_manifest_contract(lane: Lane, value: &Value) -> Result<(), String> {
    if value["schema"] != "dios-pfr-pre-run-v1"
        || value["lane"] != lane.name()
        || value["workload"] != lane.workload()
        || value["pair_count"] != PAIR_COUNT
        || value["order"] != "alternating-base-candidate-v1"
        || value["arena_posture"] != "registered"
        || value["segment_layout"] != SEGMENT_LAYOUT
        || value["fault_rule"] != "RUSAGE_THREAD:minflt=0:majflt=0"
        || value["allocation_rule"] != "post-warmup=0"
        || value["build_profile"] != "release"
        || value["products"]
            .as_array()
            .is_none_or(|products| products.len() != 2)
        || value["toolchain"]["rust"]
            .as_str()
            .is_none_or(|rust| !rust.starts_with("rustc 1.96.0"))
        || value["toolchain"]["mise"]
            .as_str()
            .is_none_or(str::is_empty)
        || value["input"]["bytes"] != u64::from(INPUT_PAGES) * u64::from(GRANULE_BYTES)
        || value["host"]["governor"] != "performance"
        || value["host"]["kernel"] != "6.6.64"
        || value["host"]["runner_host"] != "nix"
        || value["host"]["os_hostname"] != "nixos"
        || value["host"]["cpu_topology"]
            .as_str()
            .is_none_or(str::is_empty)
        || value["host"]["memlock_soft"] != 8_388_608
        || value["host"]["memlock_hard"] != 8_388_608
    {
        return Err("provenance does not encode the frozen PFR binding contract".to_owned());
    }
    let topology = value["host"]["cpu_topology"]
        .as_str()
        .expect("the topology shape was validated");
    platform::validate_recorded_topology(lane, topology)?;
    let (first, second) = lane.arms();
    let first = manifest_product_for_arm(value, first)?;
    let second = manifest_product_for_arm(value, second)?;
    if !manifest_product_complete(first)
        || !manifest_product_complete(second)
        || value["runner_sha256"]
            .as_str()
            .is_none_or(|hash| !is_lower_hex(hash, 64))
        || value["input"]["sha256"]
            .as_str()
            .is_none_or(|hash| !is_lower_hex(hash, 64))
    {
        return Err("provenance product, runner, or corpus identity is incomplete".to_owned());
    }
    Ok(())
}

fn manifest_product_complete(product: &Value) -> bool {
    product["source_commit"]
        .as_str()
        .is_some_and(|value| is_lower_hex(value, 40))
        && product["executable_sha256"]
            .as_str()
            .is_some_and(|value| is_lower_hex(value, 64))
        && product["cargo_lock_sha256"]
            .as_str()
            .is_some_and(|value| is_lower_hex(value, 64))
        && product["harness_cargo_lock_sha256"]
            .as_str()
            .is_some_and(|value| is_lower_hex(value, 64))
}

fn manifest_product_for_arm<'value>(
    value: &'value Value,
    arm: &str,
) -> Result<&'value Value, String> {
    value["products"]
        .as_array()
        .ok_or_else(|| "provenance products are missing".to_owned())?
        .iter()
        .find(|product| product["arm"] == arm)
        .ok_or_else(|| format!("provenance has no product for arm {arm}"))
}

fn csv_encode(value: &str) -> Result<String, String> {
    if value.contains(['"', '\n', '\r']) {
        return Err("rich-row field contains an unsupported CSV character".to_owned());
    }
    if value.contains(',') {
        Ok(format!("\"{value}\""))
    } else {
        Ok(value.to_owned())
    }
}

fn csv_fields(line: &str, line_number: usize) -> Result<Vec<String>, String> {
    let expected = PROCESS_HEADER.split(',').count();
    let mut fields = Vec::with_capacity(expected);
    let mut field = String::new();
    let mut quoted = false;
    for character in line.chars() {
        match character {
            '"' if field.is_empty() && !quoted => quoted = true,
            '"' if quoted => quoted = false,
            ',' if !quoted => {
                if fields.len() + 1 >= expected {
                    return Err(format!(
                        "line {line_number} has more than {expected} fields"
                    ));
                }
                fields.push(std::mem::take(&mut field));
            }
            '"' => return Err(format!("line {line_number} has a misplaced CSV quote")),
            _ => {
                if field.len() + character.len_utf8() > CSV_FIELD_BYTES_MAX {
                    let field_number = fields.len() + 1;
                    return Err(format!(
                        "CSV field {field_number} on line {line_number} exceeds byte limit {CSV_FIELD_BYTES_MAX}"
                    ));
                }
                field.push(character);
            }
        }
    }
    if quoted {
        return Err(format!("line {line_number} has an unterminated CSV quote"));
    }
    fields.push(field);
    Ok(fields)
}
