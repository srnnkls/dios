use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

const PROCESS_HEADER: &str = "gate,lane,pair,order,arm,workload,iterations,checksum,allocations,source_commit,executable_sha256,cpu_set,manifest_sha256,runner_source_sha256,runner_build_sha256,elapsed_ns,ns_per_op";
const PAIR_FIXTURE_CHECKSUM: &str = "1111111111111111";
const SMOKE_FOLD: &str = "xor_le_u64_rotate_v1";
const SMOKE_ITERATIONS: &str = "4";
const SMOKE_PAGE_SCHEDULE: &str = "0,1,512,7";
const G2_BASE_COMMIT: &str = "a94860f31e9f1649fcb73eccf8c3798c739c64fe";
const CLEAN_BASE_COMMIT: &str = "1004a2e6fcae0bcc9552dc3211c2416e388a250d";
const CANDIDATE_COMMIT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const BASE_EXECUTABLE: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const CANDIDATE_EXECUTABLE: &str =
    "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
const WARM_WORKLOAD: &str = "real_pool_driver_warm_ordinary_full_4096_byte_fold";
const CYCLING_WORKLOAD: &str = "real_pool_driver_cycling_reuse_full_4096_byte_fold";
const RUNNER_SOURCE: &str = "benches/read_path_product.rs";
const RUNNER_BUILD: &str = "build.rs";
const CONVERTER_SOURCE: &str = "src/bin/drp_gate_artifacts.rs";

static TEMP_SEQUENCE: AtomicU32 = AtomicU32::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create() -> Self {
        let base =
            std::env::var_os("CARGO_TARGET_TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        for attempt in 0_u32..128 {
            let path = base.join(format!(
                "dios-drp009-{}-{sequence}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create test-owned DRP009 directory: {error}"),
            }
        }
        panic!("could not create a unique DRP009 test directory");
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove test-owned DRP009 directory");
    }
}

#[derive(Clone, Copy)]
struct SmokeCase {
    gate: &'static str,
    lane: &'static str,
    arm: &'static str,
    workload: &'static str,
}

const SMOKE_CASES: [SmokeCase; 12] = [
    SmokeCase {
        gate: "DRP-G2",
        lane: "drp_g2_warm_ordinary",
        arm: "base",
        workload: WARM_WORKLOAD,
    },
    SmokeCase {
        gate: "DRP-G2",
        lane: "drp_g2_warm_ordinary",
        arm: "candidate",
        workload: WARM_WORKLOAD,
    },
    SmokeCase {
        gate: "DRP-G2",
        lane: "drp_g2_cycling_reuse",
        arm: "base",
        workload: CYCLING_WORKLOAD,
    },
    SmokeCase {
        gate: "DRP-G2",
        lane: "drp_g2_cycling_reuse",
        arm: "candidate",
        workload: CYCLING_WORKLOAD,
    },
    SmokeCase {
        gate: "DRP-G3",
        lane: "drp_g3_hint_materiality",
        arm: "ordinary",
        workload: "real_pool_driver_resident_ordinary_full_4096_byte_fold",
    },
    SmokeCase {
        gate: "DRP-G3",
        lane: "drp_g3_hint_materiality",
        arm: "hinted",
        workload: "real_pool_driver_resident_hinted_full_4096_byte_fold",
    },
    SmokeCase {
        gate: "DRP-G4",
        lane: "drp_g4_ordinary_base_8t",
        arm: "base",
        workload: "real_pool_driver_shared_8_thread_ordinary_full_4096_byte_fold",
    },
    SmokeCase {
        gate: "DRP-G4",
        lane: "drp_g4_ordinary_base_8t",
        arm: "candidate",
        workload: "real_pool_driver_shared_8_thread_ordinary_full_4096_byte_fold",
    },
    SmokeCase {
        gate: "DRP-G4",
        lane: "drp_g4_ordinary_scaling",
        arm: "one_thread",
        workload: "real_pool_driver_shared_1_thread_ordinary_full_4096_byte_fold",
    },
    SmokeCase {
        gate: "DRP-G4",
        lane: "drp_g4_ordinary_scaling",
        arm: "eight_threads",
        workload: "real_pool_driver_shared_8_thread_ordinary_full_4096_byte_fold",
    },
    SmokeCase {
        gate: "DRP-G4",
        lane: "drp_g4_hint_scaling",
        arm: "one_thread",
        workload: "real_pool_driver_shared_1_thread_hinted_full_4096_byte_fold",
    },
    SmokeCase {
        gate: "DRP-G4",
        lane: "drp_g4_hint_scaling",
        arm: "eight_threads",
        workload: "real_pool_driver_shared_8_thread_hinted_full_4096_byte_fold",
    },
];

fn diagnostic(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn run_bench(case: SmokeCase, input: &Path, output: &Path, manifest: &Path) -> Output {
    Command::new(env!("CARGO"))
        .args([
            "bench",
            "--quiet",
            "--features",
            "bench",
            "--bench",
            "read_path_product",
            "--",
            "smoke",
        ])
        .args([
            OsStr::new(case.lane),
            OsStr::new(case.arm),
            OsStr::new("0"),
            OsStr::new("base-candidate"),
            OsStr::new(SMOKE_FOLD),
            OsStr::new(SMOKE_ITERATIONS),
            OsStr::new(SMOKE_PAGE_SCHEDULE),
            input.as_os_str(),
            output.as_os_str(),
            manifest.as_os_str(),
        ])
        .output()
        .expect("execute the DRP009 real-backend runner")
}

fn run_task<I, S>(task: &str, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("mise")
        .args(["run", task])
        .args(args)
        .output()
        .expect("execute a frozen DRP009 task")
}

fn run_converter(process: &Path, paired: &Path, provenance: &Path) -> Output {
    run_task("prepare-gate-pairs", [process, paired, provenance])
}

fn write_new(path: &Path, contents: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .expect("create a new DRP009 artifact");
    file.write_all(contents).expect("write a DRP009 artifact");
}

#[cfg(unix)]
fn write_executable(path: &Path, contents: &[u8]) {
    write_new(path, contents);
    let mut permissions = fs::metadata(path)
        .expect("read test executable metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("make test fixture executable");
}

#[cfg(unix)]
#[test]
fn linux_flamegraph_bounds_perf_mmap_ring_for_eight_mib_memlock_host() {
    let dir = TestDir::create();
    let fake_bin = dir.0.join("bin");
    fs::create_dir_all(&fake_bin).expect("create fake tool directory");
    fs::create_dir_all(dir.0.join("src/bin")).expect("create profiling fixture source directory");
    write_new(
        &dir.0.join("Cargo.toml"),
        b"[package]\nname='probe'\nversion='0.0.0'\nedition='2024'\n[features]\nbench=[]\n[profile.profiling]\ninherits='release'\ndebug=true\nstrip=false\n",
    );
    write_new(&dir.0.join("src/bin/probe.rs"), b"fn main() {}\n");
    write_executable(
        &fake_bin.join("uname"),
        b"#!/usr/bin/env bash\nprintf 'Linux\\n'\n",
    );
    for command in ["inferno-collapse-perf", "inferno-flamegraph", "rustfilt"] {
        write_executable(&fake_bin.join(command), b"#!/usr/bin/env bash\ncat\n");
    }
    write_executable(
        &fake_bin.join("perf"),
        b"#!/usr/bin/env bash\nset -eu\nif [ \"${1-}\" = record ]; then\n  shift\n  printf '%s\\0' \"$@\" > \"${PERF_ARGUMENT_LOG:?}\"\n  exit 73\nfi\nexit 0\n",
    );

    let argument_log = dir.0.join("perf-arguments");
    let path = std::env::join_paths(
        std::iter::once(fake_bin.as_path()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .collect::<Vec<_>>()
                .iter()
                .map(PathBuf::as_path),
        ),
    )
    .expect("construct fake tool PATH");
    let task = Path::new(env!("CARGO_MANIFEST_DIR")).join(".mise/tasks/flamegraph");
    let run = Command::new("bash")
        .arg(task)
        .args(["--bin", "probe"])
        .current_dir(&dir.0)
        .env("MISE_PROJECT_ROOT", &dir.0)
        .env("PERF_ARGUMENT_LOG", &argument_log)
        .env("PATH", path)
        .output()
        .expect("execute flamegraph task with fake Linux perf");
    assert_eq!(
        run.status.code(),
        Some(73),
        "fake perf must stop the task after recording argv: {}",
        diagnostic(&run)
    );
    let arguments = fs::read(&argument_log).expect("perf record writes its argument log");
    let arguments: Vec<&[u8]> = arguments
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .collect();
    let separator = arguments
        .iter()
        .position(|argument| *argument == b"--")
        .expect("perf record separates its options from the workload");
    let perf_options = &arguments[..separator];
    let has_mmap_bound = perf_options.windows(2).any(|pair| pair == [b"-m", b"16"]);
    assert!(
        has_mmap_bound,
        "perf record must cap its mmap ring at 16 pages under the host's 8 MiB memlock limit; argv was {:?}",
        arguments
            .iter()
            .map(|argument| String::from_utf8_lossy(argument))
            .collect::<Vec<_>>()
    );
}

fn write_pre_run_manifest(path: &Path, benchmark_arguments: &[&str], cpu_set: &str) {
    write_manifest_for_products(
        path,
        benchmark_arguments,
        cpu_set,
        ("base", G2_BASE_COMMIT, BASE_EXECUTABLE),
        ("candidate", CANDIDATE_COMMIT, CANDIDATE_EXECUTABLE),
    );
}

fn write_manifest_for_products(
    path: &Path,
    benchmark_arguments: &[&str],
    cpu_set: &str,
    first: (&str, &str, &str),
    second: (&str, &str, &str),
) {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    let first_cargo_lock = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let second_cargo_lock = if first.1 == second.1 && first.2 == second.2 {
        first_cargo_lock
    } else {
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
    };
    let value = json!({
        "schema": "dios-drp009-pre-run-v1",
        "products": [
            {
                "arm": first.0,
                "source_commit": first.1,
                "executable_sha256": first.2,
                "cargo_lock_sha256": first_cargo_lock
            },
            {
                "arm": second.0,
                "source_commit": second.1,
                "executable_sha256": second.2,
                "cargo_lock_sha256": second_cargo_lock
            }
        ],
        "toolchain": {"rust": "rustc 1.96.0", "mise": "2026.8.0"},
        "benchmark_arguments": benchmark_arguments,
        "runner": {
            "source_sha256": sha256_path(&repository.join(RUNNER_SOURCE)),
            "build_sha256": sha256_path(&repository.join(RUNNER_BUILD))
        },
        "converter": {
            "source_sha256": sha256_path(&repository.join(CONVERTER_SOURCE))
        },
        "host": {
            "cpu": "AMD Ryzen Threadripper 3970X 32-Core Processor",
            "cpu_set": cpu_set,
            "governor": "performance",
            "kernel": "6.6.64",
            "topology": "CCX-0 CPUs 0-3,32-35",
            "nvme": "Samsung SSD 970 PRO",
            "direct_io": "verified",
            "transparent_hugepage": "never",
            "cache_protocol": "resident-prefill-or-cycling-warmup"
        }
    });
    let bytes = serde_json::to_vec_pretty(&value).expect("encode pre-run manifest fixture");
    write_new(path, &bytes);
}

fn product_input_bytes() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(513 * 4096);
    for page in 0_u32..513 {
        for offset in 0_u32..4096 {
            let byte = (page * 17 + offset * 73 + 41) % 251;
            bytes.push(u8::try_from(byte).expect("pattern byte fits u8"));
        }
    }
    bytes
}

fn expected_smoke_checksum(bytes: &[u8]) -> String {
    let mut checksum = 0_u64;
    for page in [0_usize, 1, 512, 7] {
        let start = page * 4096;
        let frame = &bytes[start..start + 4096];
        let frame_checksum = frame.chunks_exact(8).fold(0_u64, |fold, chunk| {
            let word = u64::from_le_bytes(chunk.try_into().expect("eight-byte fold word"));
            fold.rotate_left(1) ^ word
        });
        checksum = checksum.rotate_left(7) ^ frame_checksum;
    }
    format!("{checksum:016x}")
}

#[test]
fn every_frozen_lane_executes_a_product_backed_full_granule_smoke() {
    let dir = TestDir::create();
    let input = dir.0.join("product-input.bin");
    let input_bytes = product_input_bytes();
    let expected_checksum = expected_smoke_checksum(&input_bytes);
    write_new(&input, &input_bytes);
    for (ordinal, case) in SMOKE_CASES.into_iter().enumerate() {
        let output_path = dir.0.join(format!("smoke-{ordinal}.csv"));
        let manifest = dir.0.join(format!("smoke-{ordinal}-manifest.json"));
        write_pre_run_manifest(
            &manifest,
            &["smoke", case.lane, case.arm, SMOKE_ITERATIONS],
            "0",
        );
        let smoke = run_bench(case, &input, &output_path, &manifest);
        assert!(
            smoke.status.success(),
            "{} {} product-backed smoke must succeed: {}",
            case.lane,
            case.arm,
            diagnostic(&smoke)
        );
        assert_complete_smoke_row(&output_path, &manifest, case, &expected_checksum);
    }
}

#[test]
fn cycling_smoke_reports_the_production_reclamation_and_reuse_proof() {
    let dir = TestDir::create();
    let input = dir.0.join("cycling-input.bin");
    write_new(&input, &product_input_bytes());
    let output = dir.0.join("cycling.csv");
    let proof = dir.0.join("cycling-proof.json");
    let manifest = dir.0.join("cycling-manifest.json");
    let schedule = (0_u32..96)
        .map(|page| page.to_string())
        .collect::<Vec<_>>()
        .join(",");
    write_pre_run_manifest(
        &manifest,
        &["smoke", "drp_g2_cycling_reuse", "candidate", "192"],
        "0",
    );
    let manifest_sha256 = sha256_path(&manifest);
    let run = Command::new(env!("CARGO"))
        .args([
            "bench",
            "--quiet",
            "--features",
            "bench",
            "--bench",
            "read_path_product",
            "--",
            "smoke",
            "drp_g2_cycling_reuse",
            "candidate",
            "0",
            "base-candidate",
            SMOKE_FOLD,
            "192",
            &schedule,
        ])
        .args([
            input.as_os_str(),
            output.as_os_str(),
            manifest.as_os_str(),
            proof.as_os_str(),
        ])
        .output()
        .expect("execute the cycling proof smoke");
    assert!(
        run.status.success(),
        "cycling proof smoke must succeed: {}",
        diagnostic(&run)
    );
    let value: Value = serde_json::from_slice(&fs::read(&proof).expect("read cycling proof"))
        .expect("cycling proof is JSON");
    assert_eq!(value["manifest_sha256"], manifest_sha256);
    assert_eq!(value["process_csv_sha256"], sha256_path(&output));
    assert_eq!(value["dimensions"]["frame_count"], 64);
    assert_eq!(value["dimensions"]["working_set"], 96);
    assert_eq!(value["dimensions"]["iterations"], 192);
    assert_counter_proof(&value, "completed_reads", 96);
    assert_counter_proof(&value, "evicted_frames", 32);
    assert_counter_proof(&value, "reclaimed_frames", 32);
    assert_counter_proof(&value, "reused_frames", 32);
    assert_eq!(
        proof_count(&value, "expected", "successful_epoch_advances"),
        2
    );
    let observed = value["observed"]
        .as_object()
        .expect("cycling proof has observed evidence");
    assert!(
        !observed.contains_key("successful_epoch_advances"),
        "reclamation proves maturity without fabricating an observed epoch counter"
    );
    assert_eq!(
        observed["two_epoch_maturity"],
        "inferred_from_observed_reclaim"
    );
    assert_causal_reuse_proof(&value);
}

fn assert_causal_reuse_proof(value: &Value) {
    let cycles = value["observed"]["reuse_cycles"]
        .as_array()
        .expect("cycling proof records causal frame-reuse cycles");
    assert_eq!(
        cycles.len(),
        32,
        "every reclaimed frame has a causal record"
    );
    for (ordinal, cycle) in cycles.iter().enumerate() {
        assert_eq!(cycle["page"], 64 + ordinal);
        assert_eq!(cycle["busy_without_pending"], true);
        assert!(
            cycle["reclaimed_frames"]
                .as_u64()
                .is_some_and(|count| count > 0),
            "cycle {ordinal} observes reclamation before reuse"
        );
        assert_eq!(cycle["post_reclaim_get"], "pending");
        assert_eq!(cycle["backend_completions"], 1);
        assert!(
            cycle["reused_frame"].as_u64().is_some(),
            "cycle {ordinal} identifies the reused frame"
        );
    }
}

fn assert_counter_proof(value: &Value, field: &str, exact: u64) {
    let expected = proof_count(value, "expected", field);
    let observed = proof_count(value, "observed", field);
    assert_eq!(expected, exact, "{field} has the frozen expectation");
    assert_eq!(observed, exact, "{field} proves exact cycling progress");
}

fn proof_count(value: &Value, section: &str, field: &str) -> u64 {
    value[section][field]
        .as_u64()
        .unwrap_or_else(|| panic!("cycling proof has no integer {section}.{field}"))
}

fn assert_complete_smoke_row(
    path: &Path,
    manifest: &Path,
    case: SmokeCase,
    expected_checksum: &str,
) {
    let csv = fs::read_to_string(path).expect("runner writes its rich process row");
    let mut lines = csv.lines();
    assert_eq!(lines.next(), Some(PROCESS_HEADER));
    let fields: Vec<&str> = lines
        .next()
        .expect("runner emits one process row")
        .split(',')
        .collect();
    assert_eq!(fields.len(), 17, "rich process row field count");
    assert!(lines.next().is_none(), "smoke emits exactly one row");
    assert_eq!(fields[0], case.gate);
    assert_eq!(fields[1], case.lane);
    assert_eq!(&fields[2..5], &["0", "base-candidate", case.arm]);
    assert_eq!(fields[5], case.workload);
    assert_eq!(fields[6], SMOKE_ITERATIONS);
    assert_eq!(fields[7], expected_checksum);
    assert_eq!(fields[8], "0", "post-warmup operation allocations");
    assert!(is_lower_hex(fields[9], 40), "source commit identity");
    assert!(is_lower_hex(fields[10], 64), "executable SHA-256 identity");
    assert!(!fields[11].is_empty(), "CPU set identity");
    assert_eq!(fields[12], sha256_path(manifest));
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert_eq!(fields[13], sha256_path(&repository.join(RUNNER_SOURCE)));
    assert_eq!(fields[14], sha256_path(&repository.join(RUNNER_BUILD)));
    assert!(fields[15].parse::<u64>().is_ok());
    assert!(fields[16].parse::<f64>().is_ok());
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Copy)]
enum RowMutation {
    None,
    NonzeroAllocation(u32),
    ChecksumMismatch(u32),
}

fn process_rows(lane: &str, workload: &str, mutation: RowMutation, manifest: &Path) -> String {
    let mut csv = format!("{PROCESS_HEADER}\n");
    for pair in 0_u32..30 {
        let base_ns = 1_000_u64 + u64::from(pair) * 100;
        let candidate_ns = 900_u64 + u64::from(pair) * 100;
        let allocations =
            u64::from(matches!(mutation, RowMutation::NonzeroAllocation(at) if at == pair));
        let checksum = if matches!(mutation, RowMutation::ChecksumMismatch(at) if at == pair) {
            "0000000000000000"
        } else {
            PAIR_FIXTURE_CHECKSUM
        };
        let base = process_row(
            lane,
            workload,
            pair,
            "base",
            base_ns,
            0,
            PAIR_FIXTURE_CHECKSUM,
            G2_BASE_COMMIT,
            BASE_EXECUTABLE,
            manifest,
        );
        let candidate = process_row(
            lane,
            workload,
            pair,
            "candidate",
            candidate_ns,
            allocations,
            checksum,
            CANDIDATE_COMMIT,
            CANDIDATE_EXECUTABLE,
            manifest,
        );
        if pair.is_multiple_of(2) {
            csv.push_str(&base);
            csv.push_str(&candidate);
        } else {
            csv.push_str(&candidate);
            csv.push_str(&base);
        }
    }
    csv
}

#[expect(
    clippy::too_many_arguments,
    reason = "the helper exposes the binding rich-row fields"
)]
fn process_row(
    lane: &str,
    workload: &str,
    pair: u32,
    arm: &str,
    elapsed_ns: u64,
    allocations: u64,
    checksum: &str,
    source_commit: &str,
    executable_sha256: &str,
    manifest: &Path,
) -> String {
    let order = if pair.is_multiple_of(2) {
        "base-candidate"
    } else {
        "candidate-base"
    };
    let ns_per_op = elapsed_ns / 100;
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    format!(
        "DRP-G2,{lane},{pair},{order},{arm},{workload},100,{checksum},{allocations},{source_commit},{executable_sha256},0,{},{},{},{elapsed_ns},{ns_per_op}\n",
        sha256_path(manifest),
        sha256_path(&repository.join(RUNNER_SOURCE)),
        sha256_path(&repository.join(RUNNER_BUILD)),
    )
}

#[derive(Clone, Copy)]
struct PairSpec {
    gate: &'static str,
    lane: &'static str,
    first_arm: &'static str,
    second_arm: &'static str,
    first_workload: &'static str,
    second_workload: &'static str,
    first_commit: &'static str,
    second_commit: &'static str,
    first_executable: &'static str,
    second_executable: &'static str,
    first_cpu: &'static str,
    second_cpu: &'static str,
}

fn pair_fixture_rows(spec: PairSpec, manifest: &Path) -> String {
    let mut csv = format!("{PROCESS_HEADER}\n");
    for pair in 0_u32..30 {
        let first = pair_fixture_row(spec, manifest, pair, true);
        let second = pair_fixture_row(spec, manifest, pair, false);
        if pair.is_multiple_of(2) {
            csv.push_str(&first);
            csv.push_str(&second);
        } else {
            csv.push_str(&second);
            csv.push_str(&first);
        }
    }
    csv
}

fn pair_fixture_row(spec: PairSpec, manifest: &Path, pair: u32, first: bool) -> String {
    let (arm, workload, commit, executable, cpu, elapsed_ns) = if first {
        (
            spec.first_arm,
            spec.first_workload,
            spec.first_commit,
            spec.first_executable,
            spec.first_cpu,
            1_000_u64 + u64::from(pair) * 100,
        )
    } else {
        (
            spec.second_arm,
            spec.second_workload,
            spec.second_commit,
            spec.second_executable,
            spec.second_cpu,
            900_u64 + u64::from(pair) * 100,
        )
    };
    let order = if pair.is_multiple_of(2) {
        "base-candidate"
    } else {
        "candidate-base"
    };
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    format!(
        "{},{},{pair},{order},{arm},{workload},100,{PAIR_FIXTURE_CHECKSUM},0,{commit},{executable},{cpu},{},{},{},{elapsed_ns},{}\n",
        spec.gate,
        spec.lane,
        sha256_path(manifest),
        sha256_path(&repository.join(RUNNER_SOURCE)),
        sha256_path(&repository.join(RUNNER_BUILD)),
        elapsed_ns / 100,
    )
}

#[test]
fn converter_binds_pairs_and_actual_provenance() {
    let dir = TestDir::create();
    let process = dir.0.join("process.csv");
    let paired = dir.0.join("paired.csv");
    let provenance = dir.0.join("provenance.json");
    write_pre_run_manifest(&provenance, &["run", "drp_g2_warm_ordinary"], "0");
    let manifest_sha256 = sha256_path(&provenance);
    write_new(
        &process,
        process_rows(
            "drp_g2_warm_ordinary",
            WARM_WORKLOAD,
            RowMutation::None,
            &provenance,
        )
        .as_bytes(),
    );
    let converted = run_converter(&process, &paired, &provenance);
    assert!(
        converted.status.success(),
        "complete alternating process rows must convert: {}",
        diagnostic(&converted)
    );
    assert_paired_rows(&paired);
    assert_provenance(&process, &paired, &provenance, &manifest_sha256);

    let corrupt = dir.0.join("checksum-mismatch.csv");
    let corrupt_provenance = dir.0.join("corrupt-provenance.json");
    write_pre_run_manifest(&corrupt_provenance, &["run", "drp_g2_warm_ordinary"], "0");
    write_new(
        &corrupt,
        process_rows(
            "drp_g2_warm_ordinary",
            WARM_WORKLOAD,
            RowMutation::ChecksumMismatch(7),
            &corrupt_provenance,
        )
        .as_bytes(),
    );
    let rejected = run_converter(
        &corrupt,
        &dir.0.join("corrupt-paired.csv"),
        &corrupt_provenance,
    );
    let rejection = diagnostic(&rejected).to_ascii_lowercase();
    assert!(
        !rejected.status.success(),
        "pair checksum mismatch must fail"
    );
    assert!(
        rejection.contains("checksum"),
        "rejection identifies pair integrity: {rejection}"
    );
}

#[test]
fn converter_verifies_the_pre_run_manifest_and_runner_identity() {
    let dir = TestDir::create();
    let manifest = dir.0.join("manifest.json");
    write_pre_run_manifest(&manifest, &["run", "drp_g2_warm_ordinary"], "0");
    let process = dir.0.join("process.csv");
    write_new(
        &process,
        process_rows(
            "drp_g2_warm_ordinary",
            WARM_WORKLOAD,
            RowMutation::None,
            &manifest,
        )
        .as_bytes(),
    );
    OpenOptions::new()
        .append(true)
        .open(&manifest)
        .expect("open manifest for a post-run mutation")
        .write_all(b"\n")
        .expect("mutate manifest after rows were recorded");
    assert_conversion_rejected(&dir.0, "digest", &process, &manifest, "manifest");

    assert_incomplete_manifest_rejected(&dir.0);
    assert_runner_identity_rejected(&dir.0);
    assert_product_identity_mismatch_rejected(&dir.0);
}

#[test]
fn converter_rejects_stale_host_toolchain_and_lane_invocation_facts() {
    let dir = TestDir::create();
    assert_manifest_binding_rejected(&dir.0, "stale-host", |value| {
        value["host"]["kernel"] = json!("6.6.63");
    });
    assert_manifest_binding_rejected(&dir.0, "stale-toolchain", |value| {
        value["toolchain"]["rust"] = json!("rustc 1.95.0");
    });
    assert_manifest_binding_rejected(&dir.0, "wrong-cpu", |value| {
        value["host"]["cpu_set"] = json!("0-3,32-35");
    });
    assert_manifest_binding_rejected(&dir.0, "wrong-invocation", |value| {
        value["benchmark_arguments"] = json!(["run", "drp_g4_ordinary_scaling"]);
    });
}

fn assert_manifest_binding_rejected(dir: &Path, name: &str, mutate: impl FnOnce(&mut Value)) {
    let manifest = dir.join(format!("{name}-manifest.json"));
    write_pre_run_manifest(&manifest, &["run", "drp_g2_warm_ordinary"], "0");
    let mut value: Value =
        serde_json::from_slice(&fs::read(&manifest).expect("read complete pre-run manifest"))
            .expect("parse complete pre-run manifest");
    mutate(&mut value);
    fs::write(
        &manifest,
        serde_json::to_vec_pretty(&value).expect("encode stale manifest"),
    )
    .expect("write stale pre-run manifest");
    let process = dir.join(format!("{name}-process.csv"));
    write_new(
        &process,
        process_rows(
            "drp_g2_warm_ordinary",
            WARM_WORKLOAD,
            RowMutation::None,
            &manifest,
        )
        .as_bytes(),
    );
    assert_conversion_rejected(dir, name, &process, &manifest, "manifest");
}

fn assert_incomplete_manifest_rejected(dir: &Path) {
    let incomplete = dir.join("incomplete-manifest.json");
    write_pre_run_manifest(&incomplete, &["run", "drp_g2_warm_ordinary"], "0");
    let mut value: Value =
        serde_json::from_slice(&fs::read(&incomplete).expect("read complete pre-run manifest"))
            .expect("parse complete pre-run manifest");
    value
        .as_object_mut()
        .expect("manifest fixture is an object")
        .remove("benchmark_arguments");
    fs::write(
        &incomplete,
        serde_json::to_vec_pretty(&value).expect("encode incomplete manifest"),
    )
    .expect("write incomplete pre-run manifest");
    let incomplete_rows = dir.join("incomplete-rows.csv");
    write_new(
        &incomplete_rows,
        process_rows(
            "drp_g2_warm_ordinary",
            WARM_WORKLOAD,
            RowMutation::None,
            &incomplete,
        )
        .as_bytes(),
    );
    assert_conversion_rejected(dir, "incomplete", &incomplete_rows, &incomplete, "manifest");
}

fn assert_runner_identity_rejected(dir: &Path) {
    let runner_rows = dir.join("runner-rows.csv");
    let valid_manifest = dir.join("runner-manifest.json");
    write_pre_run_manifest(&valid_manifest, &["run", "drp_g2_warm_ordinary"], "0");
    let runner_hash = sha256_path(&Path::new(env!("CARGO_MANIFEST_DIR")).join(RUNNER_SOURCE));
    let rows = process_rows(
        "drp_g2_warm_ordinary",
        WARM_WORKLOAD,
        RowMutation::None,
        &valid_manifest,
    )
    .replacen(&runner_hash, &"0".repeat(64), 1);
    write_new(&runner_rows, rows.as_bytes());
    assert_conversion_rejected(dir, "runner", &runner_rows, &valid_manifest, "runner");
}

fn assert_product_identity_mismatch_rejected(dir: &Path) {
    let process = dir.join("product-mismatch-rows.csv");
    let provenance = dir.join("product-mismatch-provenance.json");
    write_pre_run_manifest(&provenance, &["run", "drp_g2_warm_ordinary"], "0");
    let rows = process_rows(
        "drp_g2_warm_ordinary",
        WARM_WORKLOAD,
        RowMutation::None,
        &provenance,
    )
    .replace(CANDIDATE_COMMIT, CLEAN_BASE_COMMIT);
    write_new(&process, rows.as_bytes());
    assert_conversion_rejected(dir, "product", &process, &provenance, "manifest");
}

fn assert_conversion_rejected(
    dir: &Path,
    name: &str,
    process: &Path,
    manifest: &Path,
    expected: &str,
) {
    let rejected = run_converter(process, &dir.join(format!("{name}-paired.csv")), manifest);
    let diagnostic = diagnostic(&rejected).to_ascii_lowercase();
    assert!(!rejected.status.success(), "corrupt {name} must fail");
    assert!(
        diagnostic.contains(expected),
        "rejection must identify {expected}: {diagnostic}"
    );
}

fn assert_paired_rows(path: &Path) {
    let paired = fs::read_to_string(path).expect("converter writes paired samples");
    let expected_rows: Vec<String> = (0_u64..30)
        .map(|pair| format!("{},{}", 1_000 + pair * 100, 900 + pair * 100))
        .collect();
    let expected = format!("base_ns,candidate_ns\n{}\n", expected_rows.join("\n"));
    assert_eq!(
        paired, expected,
        "pairs are emitted in ascending pair order"
    );
}

fn assert_provenance(process: &Path, paired: &Path, provenance: &Path, manifest_sha256: &str) {
    let text = fs::read_to_string(provenance).expect("converter writes provenance");
    let value: Value = serde_json::from_str(&text).expect("provenance is JSON");
    assert_eq!(value["base_source_commit"], G2_BASE_COMMIT);
    assert_eq!(value["candidate_source_commit"], CANDIDATE_COMMIT);
    assert_eq!(value["base_executable_sha256"], BASE_EXECUTABLE);
    assert_eq!(value["candidate_executable_sha256"], CANDIDATE_EXECUTABLE);
    assert_eq!(value["process_csv_sha256"], sha256_path(process));
    assert_eq!(value["paired_csv_sha256"], sha256_path(paired));
    assert_eq!(value["pre_run_manifest_sha256"], manifest_sha256);
    let converter_path = value["converter_path"]
        .as_str()
        .map(Path::new)
        .expect("provenance retains the actual converter path");
    assert_eq!(value["converter_sha256"], sha256_path(converter_path));
}

fn sha256_path(path: &Path) -> String {
    let bytes = fs::read(path).expect("read a provenance-bound artifact");
    format!("{:x}", Sha256::digest(bytes))
}

#[test]
fn converter_rejects_noncanonical_lane_relationships() {
    let dir = TestDir::create();
    assert_canonical_converts(&dir.0, "canonical-g2", g2_pair_spec());
    assert_canonical_converts(&dir.0, "canonical-g2-cycling", g2_cycling_pair_spec());
    assert_canonical_converts(&dir.0, "canonical-g3", g3_pair_spec());
    assert_canonical_converts(&dir.0, "canonical-g4-base", g4_base_pair_spec());
    assert_canonical_converts(&dir.0, "canonical-g4-scale", g4_scaling_pair_spec());
    assert_canonical_converts(
        &dir.0,
        "canonical-g4-hint-scale",
        g4_hint_scaling_pair_spec(),
    );

    let mut g2 = g2_pair_spec();
    g2.first_commit = CLEAN_BASE_COMMIT;
    assert_noncanonical_rejected(&dir.0, "g2-base", g2);

    let mut g2_workload = g2_pair_spec();
    g2_workload.second_workload = "ordinary_get_without_the_frozen_full_frame_fold";
    assert_noncanonical_rejected(&dir.0, "g2-workload", g2_workload);

    let mut g4_base = g4_base_pair_spec();
    g4_base.first_commit = G2_BASE_COMMIT;
    assert_noncanonical_rejected(&dir.0, "g4-base", g4_base);

    let mut g3 = g3_pair_spec();
    g3.first_executable = BASE_EXECUTABLE;
    assert_noncanonical_rejected(&dir.0, "g3-product", g3);

    let mut g4_scaling = g4_scaling_pair_spec();
    g4_scaling.second_executable = BASE_EXECUTABLE;
    assert_noncanonical_rejected(&dir.0, "g4-product", g4_scaling);

    let mut g4_cpu = g4_scaling_pair_spec();
    g4_cpu.second_cpu = "0-7";
    assert_noncanonical_rejected(&dir.0, "g4-cpu", g4_cpu);
}

fn assert_canonical_converts(dir: &Path, name: &str, spec: PairSpec) {
    let manifest = dir.join(format!("{name}-manifest.json"));
    write_manifest_for_products(
        &manifest,
        &["run", spec.lane],
        spec.second_cpu,
        (spec.first_arm, spec.first_commit, spec.first_executable),
        (spec.second_arm, spec.second_commit, spec.second_executable),
    );
    let process = dir.join(format!("{name}-process.csv"));
    write_new(&process, pair_fixture_rows(spec, &manifest).as_bytes());
    let converted = run_converter(&process, &dir.join(format!("{name}-paired.csv")), &manifest);
    assert!(
        converted.status.success(),
        "canonical {name} must convert: {}",
        diagnostic(&converted)
    );
}

fn assert_noncanonical_rejected(dir: &Path, name: &str, spec: PairSpec) {
    let manifest = dir.join(format!("{name}-manifest.json"));
    write_manifest_for_products(
        &manifest,
        &["run", spec.lane],
        spec.second_cpu,
        (spec.first_arm, spec.first_commit, spec.first_executable),
        (spec.second_arm, spec.second_commit, spec.second_executable),
    );
    let process = dir.join(format!("{name}-process.csv"));
    write_new(&process, pair_fixture_rows(spec, &manifest).as_bytes());
    let rejected = run_converter(&process, &dir.join(format!("{name}-paired.csv")), &manifest);
    assert!(
        !rejected.status.success(),
        "noncanonical {name} relation must fail conversion"
    );
}

fn g2_pair_spec() -> PairSpec {
    PairSpec {
        gate: "DRP-G2",
        lane: "drp_g2_warm_ordinary",
        first_arm: "base",
        second_arm: "candidate",
        first_workload: WARM_WORKLOAD,
        second_workload: WARM_WORKLOAD,
        first_commit: G2_BASE_COMMIT,
        second_commit: CANDIDATE_COMMIT,
        first_executable: BASE_EXECUTABLE,
        second_executable: CANDIDATE_EXECUTABLE,
        first_cpu: "0",
        second_cpu: "0",
    }
}

fn g2_cycling_pair_spec() -> PairSpec {
    PairSpec {
        lane: "drp_g2_cycling_reuse",
        first_workload: CYCLING_WORKLOAD,
        second_workload: CYCLING_WORKLOAD,
        ..g2_pair_spec()
    }
}

fn g3_pair_spec() -> PairSpec {
    PairSpec {
        gate: "DRP-G3",
        lane: "drp_g3_hint_materiality",
        first_arm: "ordinary",
        second_arm: "hinted",
        first_workload: "real_pool_driver_resident_ordinary_full_4096_byte_fold",
        second_workload: "real_pool_driver_resident_hinted_full_4096_byte_fold",
        first_commit: CANDIDATE_COMMIT,
        second_commit: CANDIDATE_COMMIT,
        first_executable: CANDIDATE_EXECUTABLE,
        second_executable: CANDIDATE_EXECUTABLE,
        first_cpu: "0",
        second_cpu: "0",
    }
}

fn g4_base_pair_spec() -> PairSpec {
    PairSpec {
        gate: "DRP-G4",
        lane: "drp_g4_ordinary_base_8t",
        first_arm: "base",
        second_arm: "candidate",
        first_workload: "real_pool_driver_shared_8_thread_ordinary_full_4096_byte_fold",
        second_workload: "real_pool_driver_shared_8_thread_ordinary_full_4096_byte_fold",
        first_commit: CLEAN_BASE_COMMIT,
        second_commit: CANDIDATE_COMMIT,
        first_executable: BASE_EXECUTABLE,
        second_executable: CANDIDATE_EXECUTABLE,
        first_cpu: "0-3,32-35",
        second_cpu: "0-3,32-35",
    }
}

fn g4_scaling_pair_spec() -> PairSpec {
    PairSpec {
        gate: "DRP-G4",
        lane: "drp_g4_ordinary_scaling",
        first_arm: "one_thread",
        second_arm: "eight_threads",
        first_workload: "real_pool_driver_shared_1_thread_ordinary_full_4096_byte_fold",
        second_workload: "real_pool_driver_shared_8_thread_ordinary_full_4096_byte_fold",
        first_commit: CANDIDATE_COMMIT,
        second_commit: CANDIDATE_COMMIT,
        first_executable: CANDIDATE_EXECUTABLE,
        second_executable: CANDIDATE_EXECUTABLE,
        first_cpu: "0",
        second_cpu: "0-3,32-35",
    }
}

fn g4_hint_scaling_pair_spec() -> PairSpec {
    PairSpec {
        lane: "drp_g4_hint_scaling",
        first_workload: "real_pool_driver_shared_1_thread_hinted_full_4096_byte_fold",
        second_workload: "real_pool_driver_shared_8_thread_hinted_full_4096_byte_fold",
        ..g4_scaling_pair_spec()
    }
}

#[test]
fn zero_allocation_validator_covers_both_g2_lane_shapes() {
    let dir = TestDir::create();
    let warm = dir.0.join("warm.csv");
    let cycling = dir.0.join("cycling.csv");
    let manifest = dir.0.join("pre-run-manifest.json");
    write_pre_run_manifest(&manifest, &["run", "drp_g2_zero_alloc"], "0");
    write_new(
        &warm,
        process_rows(
            "drp_g2_warm_ordinary",
            WARM_WORKLOAD,
            RowMutation::None,
            &manifest,
        )
        .as_bytes(),
    );
    write_new(
        &cycling,
        process_rows(
            "drp_g2_cycling_reuse",
            CYCLING_WORKLOAD,
            RowMutation::None,
            &manifest,
        )
        .as_bytes(),
    );
    for path in [&warm, &cycling] {
        let valid = run_task("validate-drp-g2-zero-alloc", [path]);
        assert!(
            valid.status.success(),
            "clean G2 lane must validate: {}",
            diagnostic(&valid)
        );
    }

    let corrupt = dir.0.join("nonzero-allocation.csv");
    write_new(
        &corrupt,
        process_rows(
            "drp_g2_cycling_reuse",
            CYCLING_WORKLOAD,
            RowMutation::NonzeroAllocation(7),
            &manifest,
        )
        .as_bytes(),
    );
    let rejected = run_task("validate-drp-g2-zero-alloc", [&corrupt]);
    let rejection = diagnostic(&rejected).to_ascii_lowercase();
    assert!(
        !rejected.status.success(),
        "nonzero G2 allocation must fail"
    );
    assert!(
        rejection.contains("allocation"),
        "rejection identifies allocation: {rejection}"
    );
}
