use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

const PROCESS_HEADER: &str = "gate,lane,pair,order,arm,workload,iterations,checksum,allocations,source_commit,executable_sha256,cpu_set,manifest_sha256,runner_source_sha256,runner_build_sha256,elapsed_ns,ns_per_op";
const PAIR_COUNT: u32 = 30;
const G2_BASE_COMMIT: &str = "a94860f31e9f1649fcb73eccf8c3798c739c64fe";
const G4_BASE_COMMIT: &str = "1004a2e6fcae0bcc9552dc3211c2416e388a250d";
const G4_CPU_SET: &str = "0-3,32-35";
const RUST_TOOLCHAIN: &str = "rustc 1.96.0";
const MISE_TOOLCHAIN: &str = "2026.8.0";

#[derive(Debug)]
struct ProcessRow {
    gate: String,
    lane: String,
    pair: u32,
    order: String,
    arm: String,
    workload: String,
    iterations: u64,
    checksum: String,
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
struct ValidatedRows {
    rows: Vec<ProcessRow>,
    first_arm: &'static str,
    second_arm: &'static str,
}

#[derive(Clone, Copy)]
struct ArmIdentity<'row> {
    source_commit: &'row str,
    executable_sha256: &'row str,
    workload: &'row str,
    cpu_set: &'row str,
    iterations: u64,
}

struct PreRunManifest {
    value: Value,
    sha256: String,
}

fn main() -> ExitCode {
    let args = env::args().skip(1).collect::<Vec<_>>();
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
        [command, process, paired, provenance] if command == "prepare" => {
            prepare(Path::new(process), Path::new(paired), Path::new(provenance))
        }
        [command, process] if command == "validate-g2-zero-alloc" => {
            validate_g2_zero_alloc(Path::new(process))
        }
        _ => Err("usage: drp_gate_artifacts <prepare PROCESS PAIRED PROVENANCE|validate-g2-zero-alloc PROCESS>".to_owned()),
    }
}

fn prepare(process: &Path, paired: &Path, provenance: &Path) -> Result<(), String> {
    if process == paired || process == provenance || paired == provenance {
        return Err("process, paired, and provenance paths must be distinct".to_owned());
    }
    let manifest = read_manifest(provenance)?;
    let validated = read_and_validate(process)?;
    validate_manifest_bindings(&validated, &manifest)?;
    fs::write(paired, paired_text(&validated)?)
        .map_err(|error| format!("write {}: {error}", paired.display()))?;
    enrich_manifest(process, paired, provenance, manifest, &validated)
}

fn validate_g2_zero_alloc(process: &Path) -> Result<(), String> {
    let validated = read_and_validate(process)?;
    if validated.rows[0].gate != "DRP-G2" {
        return Err("zero-allocation validation accepts only DRP-G2 rows".to_owned());
    }
    for row in &validated.rows {
        if row.allocations != 0 {
            return Err(format!(
                "allocation count for pair {} arm {} is {}, expected zero",
                row.pair, row.arm, row.allocations
            ));
        }
    }
    Ok(())
}

fn read_and_validate(path: &Path) -> Result<ValidatedRows, String> {
    let text =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let mut lines = text.lines();
    if lines.next() != Some(PROCESS_HEADER) {
        return Err("process CSV header does not match the frozen rich-row schema".to_owned());
    }
    let rows = lines
        .enumerate()
        .map(|(index, line)| parse_row(index + 2, line))
        .collect::<Result<Vec<_>, _>>()?;
    let expected = usize::try_from(PAIR_COUNT * 2).expect("fixed row count fits usize");
    if rows.len() != expected {
        return Err(format!(
            "process CSV has {} rows, expected {expected}",
            rows.len()
        ));
    }
    let (first_arm, second_arm) = lane_arms(&rows[0].lane)?;
    validate_rows(&rows, first_arm, second_arm)?;
    validate_lane_contract(&rows, first_arm, second_arm)?;
    Ok(ValidatedRows {
        rows,
        first_arm,
        second_arm,
    })
}

fn parse_row(line_number: usize, source_line: &str) -> Result<ProcessRow, String> {
    let fields = csv_fields(source_line, line_number)?;
    let [
        gate,
        lane,
        pair,
        order,
        arm,
        workload,
        iterations,
        checksum,
        allocations,
        source_commit,
        executable_sha256,
        cpu_set,
        manifest_sha256,
        runner_source_sha256,
        runner_build_sha256,
        elapsed_ns,
        ns_per_op,
    ] = fields.as_slice()
    else {
        return Err(format!("line {line_number} does not contain 17 fields"));
    };
    let row = ProcessRow {
        gate: gate.clone(),
        lane: lane.clone(),
        pair: parse_number(pair, line_number, "pair")?,
        order: order.clone(),
        arm: arm.clone(),
        workload: workload.clone(),
        iterations: parse_number(iterations, line_number, "iterations")?,
        checksum: checksum.clone(),
        allocations: parse_number(allocations, line_number, "allocations")?,
        source_commit: source_commit.clone(),
        executable_sha256: executable_sha256.clone(),
        cpu_set: cpu_set.clone(),
        manifest_sha256: manifest_sha256.clone(),
        runner_source_sha256: runner_source_sha256.clone(),
        runner_build_sha256: runner_build_sha256.clone(),
        elapsed_ns: parse_number(elapsed_ns, line_number, "elapsed_ns")?,
    };
    let ns: f64 = ns_per_op
        .parse()
        .map_err(|_| format!("line {line_number} has invalid ns_per_op"))?;
    if !ns.is_finite() || ns <= 0.0 {
        return Err(format!("line {line_number} ns_per_op must be positive"));
    }
    validate_row_shape(&row, line_number)?;
    Ok(row)
}

fn csv_fields(source_line: &str, line_number: usize) -> Result<Vec<String>, String> {
    let mut fields = Vec::with_capacity(17);
    let mut field = String::new();
    let mut quoted = false;
    for character in source_line.chars() {
        match character {
            '"' if field.is_empty() && !quoted => quoted = true,
            '"' if quoted => quoted = false,
            ',' if !quoted => fields.push(std::mem::take(&mut field)),
            '"' => return Err(format!("line {line_number} has a misplaced CSV quote")),
            _ => field.push(character),
        }
    }
    if quoted {
        return Err(format!("line {line_number} has an unterminated CSV quote"));
    }
    fields.push(field);
    if fields.len() == 18 {
        fields[11].push(',');
        let suffix = fields.remove(12);
        fields[11].push_str(&suffix);
    }
    Ok(fields)
}

fn parse_number<T: std::str::FromStr>(value: &str, line: usize, field: &str) -> Result<T, String> {
    value
        .parse()
        .map_err(|_| format!("line {line} has invalid {field}"))
}

fn validate_row_shape(row: &ProcessRow, line: usize) -> Result<(), String> {
    if row.iterations == 0 || row.elapsed_ns == 0 {
        return Err(format!("line {line} has a zero timing bound"));
    }
    for (name, value, length) in [
        ("checksum", row.checksum.as_str(), 16),
        ("source commit", row.source_commit.as_str(), 40),
        ("executable SHA-256", row.executable_sha256.as_str(), 64),
        ("manifest SHA-256", row.manifest_sha256.as_str(), 64),
        (
            "runner source SHA-256",
            row.runner_source_sha256.as_str(),
            64,
        ),
        ("runner build SHA-256", row.runner_build_sha256.as_str(), 64),
    ] {
        if !is_lower_hex(value, length) {
            return Err(format!("line {line} has invalid {name}"));
        }
    }
    if row.cpu_set.is_empty() {
        return Err(format!("line {line} has no CPU-set identity"));
    }
    Ok(())
}

fn validate_rows(rows: &[ProcessRow], first_arm: &str, second_arm: &str) -> Result<(), String> {
    let first = &rows[0];
    let first_identity = arm_identity(rows, first_arm)?;
    let second_identity = arm_identity(rows, second_arm)?;
    for pair in 0..PAIR_COUNT {
        let index = usize::try_from(pair * 2).expect("fixed pair index fits usize");
        let pair_rows = [&rows[index], &rows[index + 1]];
        validate_pair(pair_rows, pair, first_arm, second_arm, first)?;
        validate_identity(pair_rows, first_arm, first_identity)?;
        validate_identity(pair_rows, second_arm, second_identity)?;
    }
    Ok(())
}

fn validate_pair(
    rows: [&ProcessRow; 2],
    pair: u32,
    first_arm: &str,
    second_arm: &str,
    first: &ProcessRow,
) -> Result<(), String> {
    let order = if pair.is_multiple_of(2) {
        "base-candidate"
    } else {
        "candidate-base"
    };
    let arms = if pair.is_multiple_of(2) {
        [first_arm, second_arm]
    } else {
        [second_arm, first_arm]
    };
    for index in 0..2 {
        let row = rows[index];
        if row.pair != pair || row.order != order || row.arm != arms[index] {
            return Err(format!("pair {pair} is missing, duplicated, or reordered"));
        }
        if row.gate != first.gate || row.lane != first.lane || row.iterations != first.iterations {
            return Err(format!(
                "pair {pair} has a gate, lane, or iteration mismatch"
            ));
        }
    }
    if rows[0].checksum != rows[1].checksum || rows[0].checksum != first.checksum {
        return Err(format!("pair {pair} has a checksum mismatch"));
    }
    Ok(())
}

fn arm_identity<'rows>(rows: &'rows [ProcessRow], arm: &str) -> Result<ArmIdentity<'rows>, String> {
    let row = rows
        .iter()
        .find(|row| row.arm == arm)
        .ok_or_else(|| format!("artifact has no {arm} arm"))?;
    Ok(ArmIdentity {
        source_commit: &row.source_commit,
        executable_sha256: &row.executable_sha256,
        workload: &row.workload,
        cpu_set: &row.cpu_set,
        iterations: row.iterations,
    })
}

fn validate_identity(
    rows: [&ProcessRow; 2],
    arm: &str,
    identity: ArmIdentity<'_>,
) -> Result<(), String> {
    let row = rows
        .into_iter()
        .find(|row| row.arm == arm)
        .ok_or_else(|| format!("pair {} has no {arm} arm", rows[0].pair))?;
    if row.source_commit != identity.source_commit
        || row.executable_sha256 != identity.executable_sha256
        || row.workload != identity.workload
        || row.cpu_set != identity.cpu_set
        || row.iterations != identity.iterations
    {
        return Err(format!(
            "pair {} has an arm-specific product identity mismatch",
            row.pair
        ));
    }
    Ok(())
}

fn validate_lane_contract(
    rows: &[ProcessRow],
    first_arm: &str,
    second_arm: &str,
) -> Result<(), String> {
    let first = arm_identity(rows, first_arm)?;
    let second = arm_identity(rows, second_arm)?;
    match rows[0].lane.as_str() {
        "drp_g2_warm_ordinary" => validate_g2(
            first,
            second,
            "real_pool_driver_warm_ordinary_full_4096_byte_fold",
        ),
        "drp_g2_cycling_reuse" => validate_g2(
            first,
            second,
            "real_pool_driver_cycling_reuse_full_4096_byte_fold",
        ),
        "drp_g3_hint_materiality" => validate_same_product(
            first,
            second,
            "real_pool_driver_resident_ordinary_full_4096_byte_fold",
            "real_pool_driver_resident_hinted_full_4096_byte_fold",
            "0",
            "0",
        ),
        "drp_g4_ordinary_base_8t" => validate_g4_base(first, second),
        "drp_g4_ordinary_scaling" => validate_same_product(
            first,
            second,
            "real_pool_driver_shared_1_thread_ordinary_full_4096_byte_fold",
            "real_pool_driver_shared_8_thread_ordinary_full_4096_byte_fold",
            "0",
            G4_CPU_SET,
        ),
        "drp_g4_hint_scaling" => validate_same_product(
            first,
            second,
            "real_pool_driver_shared_1_thread_hinted_full_4096_byte_fold",
            "real_pool_driver_shared_8_thread_hinted_full_4096_byte_fold",
            "0",
            G4_CPU_SET,
        ),
        lane => Err(format!("unknown frozen gate lane {lane:?}")),
    }
}

fn validate_g2(
    first: ArmIdentity<'_>,
    second: ArmIdentity<'_>,
    workload: &str,
) -> Result<(), String> {
    if first.source_commit != G2_BASE_COMMIT
        || second.source_commit == G2_BASE_COMMIT
        || first.executable_sha256 == second.executable_sha256
        || first.workload != workload
        || second.workload != workload
        || first.cpu_set != "0"
        || second.cpu_set != "0"
    {
        return Err(
            "noncanonical DRP-G2 base, candidate, workload, executable, or CPU relation".to_owned(),
        );
    }
    Ok(())
}

fn validate_g4_base(first: ArmIdentity<'_>, second: ArmIdentity<'_>) -> Result<(), String> {
    let workload = "real_pool_driver_shared_8_thread_ordinary_full_4096_byte_fold";
    if first.source_commit != G4_BASE_COMMIT
        || second.source_commit == G4_BASE_COMMIT
        || first.executable_sha256 == second.executable_sha256
        || first.workload != workload
        || second.workload != workload
        || first.cpu_set != G4_CPU_SET
        || second.cpu_set != G4_CPU_SET
    {
        return Err("noncanonical DRP-G4 ordinary base product or CPU relation".to_owned());
    }
    Ok(())
}

fn validate_same_product(
    first: ArmIdentity<'_>,
    second: ArmIdentity<'_>,
    first_workload: &str,
    second_workload: &str,
    first_cpu: &str,
    second_cpu: &str,
) -> Result<(), String> {
    if first.source_commit != second.source_commit
        || first.executable_sha256 != second.executable_sha256
        || first.workload != first_workload
        || second.workload != second_workload
        || first.cpu_set != first_cpu
        || second.cpu_set != second_cpu
    {
        return Err(
            "noncanonical same-product workload, executable, source, or CPU relation".to_owned(),
        );
    }
    Ok(())
}

fn read_manifest(path: &Path) -> Result<PreRunManifest, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("read manifest {}: {error}", path.display()))?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|error| format!("parse manifest: {error}"))?;
    validate_manifest_shape(&value)?;
    Ok(PreRunManifest {
        value,
        sha256: sha256_bytes(&bytes),
    })
}

fn validate_manifest_shape(value: &Value) -> Result<(), String> {
    if value["schema"] != "dios-drp009-pre-run-v1"
        || value["products"].as_array().is_none_or(|v| v.len() != 2)
        || value["toolchain"].as_object().is_none()
        || value["benchmark_arguments"].as_array().is_none()
        || value["runner"]["source_sha256"].as_str().is_none()
        || value["runner"]["build_sha256"].as_str().is_none()
        || value["converter"]["source_sha256"].as_str().is_none()
        || value["host"].as_object().is_none()
    {
        return Err("pre-run manifest is incomplete or has the wrong schema".to_owned());
    }
    for product in value["products"].as_array().expect("checked products") {
        if product["arm"].as_str().is_none()
            || !value_is_hex(product, "source_commit", 40)
            || !value_is_hex(product, "executable_sha256", 64)
            || !value_is_hex(product, "cargo_lock_sha256", 64)
        {
            return Err("pre-run manifest has an incomplete product identity".to_owned());
        }
    }
    Ok(())
}

fn validate_manifest_bindings(
    validated: &ValidatedRows,
    manifest: &PreRunManifest,
) -> Result<(), String> {
    let runner_source = string_field(&manifest.value["runner"], "source_sha256", "runner")?;
    let runner_build = string_field(&manifest.value["runner"], "build_sha256", "runner")?;
    if runner_source != sha256_path(&repository_path("benches/read_path_product.rs"))?
        || runner_build != sha256_path(&repository_path("build.rs"))?
    {
        return Err("pre-run manifest runner identity differs from the frozen runner".to_owned());
    }
    validate_converter_identity(manifest)?;
    validate_execution_facts(validated, manifest)?;
    for row in &validated.rows {
        if row.manifest_sha256 != manifest.sha256
            || row.runner_source_sha256 != runner_source
            || row.runner_build_sha256 != runner_build
        {
            return Err("process row manifest digest or runner identity mismatch".to_owned());
        }
        validate_product_row(row, manifest)?;
    }
    validate_same_product_lock(validated, manifest)
}

fn validate_execution_facts(
    validated: &ValidatedRows,
    manifest: &PreRunManifest,
) -> Result<(), String> {
    let lane = validated.rows[0].lane.as_str();
    let cpu_set = match lane {
        "drp_g2_warm_ordinary" | "drp_g2_cycling_reuse" | "drp_g3_hint_materiality" => "0",
        "drp_g4_ordinary_base_8t" | "drp_g4_ordinary_scaling" | "drp_g4_hint_scaling" => G4_CPU_SET,
        _ => return Err(format!("manifest names unknown frozen lane {lane:?}")),
    };
    let expected_host = json!({
        "cpu": "AMD Ryzen Threadripper 3970X 32-Core Processor",
        "cpu_set": cpu_set,
        "governor": "performance",
        "kernel": "6.6.64",
        "topology": "CCX-0 CPUs 0-3,32-35",
        "nvme": "Samsung SSD 970 PRO",
        "direct_io": "verified",
        "transparent_hugepage": "never",
        "cache_protocol": "resident-prefill-or-cycling-warmup",
    });
    let expected_toolchain = json!({"rust": RUST_TOOLCHAIN, "mise": MISE_TOOLCHAIN});
    let expected_arguments = json!(["run", lane]);
    if manifest.value["host"] != expected_host
        || manifest.value["toolchain"] != expected_toolchain
        || manifest.value["benchmark_arguments"] != expected_arguments
    {
        return Err(
            "pre-run manifest host, toolchain, CPU set, or lane invocation is not frozen"
                .to_owned(),
        );
    }
    Ok(())
}

fn validate_converter_identity(manifest: &PreRunManifest) -> Result<(), String> {
    let recorded = string_field(&manifest.value["converter"], "source_sha256", "converter")?;
    if recorded != sha256_path(&repository_path("src/bin/drp_gate_artifacts.rs"))? {
        return Err(
            "pre-run manifest converter identity differs from the executing source".to_owned(),
        );
    }
    Ok(())
}

fn validate_product_row(row: &ProcessRow, manifest: &PreRunManifest) -> Result<(), String> {
    let product = manifest.value["products"]
        .as_array()
        .expect("validated products")
        .iter()
        .find(|product| product["arm"] == row.arm)
        .ok_or_else(|| format!("manifest has no product for arm {}", row.arm))?;
    if product["source_commit"] != row.source_commit
        || product["executable_sha256"] != row.executable_sha256
    {
        return Err(format!(
            "manifest product identity mismatch for arm {}",
            row.arm
        ));
    }
    Ok(())
}

fn validate_same_product_lock(
    validated: &ValidatedRows,
    manifest: &PreRunManifest,
) -> Result<(), String> {
    if !matches!(
        validated.rows[0].lane.as_str(),
        "drp_g3_hint_materiality" | "drp_g4_ordinary_scaling" | "drp_g4_hint_scaling"
    ) {
        return Ok(());
    }
    let first = manifest_product(manifest, validated.first_arm)?;
    let second = manifest_product(manifest, validated.second_arm)?;
    if first["cargo_lock_sha256"] != second["cargo_lock_sha256"] {
        return Err("same-product lane manifest has different Cargo.lock identities".to_owned());
    }
    Ok(())
}

fn manifest_product<'value>(
    manifest: &'value PreRunManifest,
    arm: &str,
) -> Result<&'value Value, String> {
    manifest.value["products"]
        .as_array()
        .expect("validated products")
        .iter()
        .find(|product| product["arm"] == arm)
        .ok_or_else(|| format!("manifest has no product for arm {arm}"))
}

fn paired_text(validated: &ValidatedRows) -> Result<String, String> {
    let mut output = String::from("base_ns,candidate_ns\n");
    for pair in 0..PAIR_COUNT {
        let index = usize::try_from(pair * 2).expect("fixed pair index fits usize");
        let pair_rows = [&validated.rows[index], &validated.rows[index + 1]];
        let first = pair_rows
            .into_iter()
            .find(|row| row.arm == validated.first_arm)
            .ok_or_else(|| format!("pair {pair} lost its first arm"))?;
        let second = pair_rows
            .into_iter()
            .find(|row| row.arm == validated.second_arm)
            .ok_or_else(|| format!("pair {pair} lost its second arm"))?;
        writeln!(output, "{},{}", first.elapsed_ns, second.elapsed_ns)
            .expect("writing paired rows to a String cannot fail");
    }
    Ok(output)
}

fn enrich_manifest(
    process: &Path,
    paired: &Path,
    provenance: &Path,
    mut manifest: PreRunManifest,
    validated: &ValidatedRows,
) -> Result<(), String> {
    let first = arm_identity(&validated.rows, validated.first_arm)?;
    let second = arm_identity(&validated.rows, validated.second_arm)?;
    let converter = env::current_exe().map_err(|error| format!("locate converter: {error}"))?;
    let object = manifest
        .value
        .as_object_mut()
        .expect("validated manifest object");
    insert(object, "base_source_commit", json!(first.source_commit));
    insert(
        object,
        "candidate_source_commit",
        json!(second.source_commit),
    );
    insert(
        object,
        "base_executable_sha256",
        json!(first.executable_sha256),
    );
    insert(
        object,
        "candidate_executable_sha256",
        json!(second.executable_sha256),
    );
    insert(object, "process_csv_sha256", json!(sha256_path(process)?));
    insert(object, "paired_csv_sha256", json!(sha256_path(paired)?));
    insert(object, "pre_run_manifest_sha256", json!(manifest.sha256));
    insert(object, "converter_path", json!(converter));
    insert(object, "converter_sha256", json!(sha256_path(&converter)?));
    let bytes = serde_json::to_vec_pretty(&manifest.value)
        .map_err(|error| format!("encode provenance: {error}"))?;
    fs::write(provenance, bytes).map_err(|error| format!("write {}: {error}", provenance.display()))
}

fn insert(object: &mut Map<String, Value>, key: &str, value: Value) {
    assert!(
        object.insert(key.to_owned(), value).is_none(),
        "pre-run manifest is enriched once"
    );
}

fn repository_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn string_field<'value>(
    value: &'value Value,
    field: &str,
    section: &str,
) -> Result<&'value str, String> {
    value[field]
        .as_str()
        .ok_or_else(|| format!("manifest {section}.{field} is missing"))
}

fn value_is_hex(value: &Value, field: &str, length: usize) -> bool {
    value[field]
        .as_str()
        .is_some_and(|text| is_lower_hex(text, length))
}

fn sha256_path(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(sha256_bytes(&bytes))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn lane_arms(lane: &str) -> Result<(&'static str, &'static str), String> {
    match lane {
        "drp_g2_warm_ordinary" | "drp_g2_cycling_reuse" | "drp_g4_ordinary_base_8t" => {
            Ok(("base", "candidate"))
        }
        "drp_g3_hint_materiality" => Ok(("ordinary", "hinted")),
        "drp_g4_ordinary_scaling" | "drp_g4_hint_scaling" => Ok(("one_thread", "eight_threads")),
        _ => Err(format!("unknown frozen gate lane {lane:?}")),
    }
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
