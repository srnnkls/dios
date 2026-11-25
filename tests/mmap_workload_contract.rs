#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};

#[path = "../benches/mmap_workloads/mod.rs"]
mod mmap_workloads;

/// The fixture every sampling test reads. Its lock serializes those tests,
/// since each sample discards and rewarms the fixture's page cache.
static FIXTURE: LazyLock<Mutex<PathBuf>> = LazyLock::new(|| {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mmap-workload-contract-fixture");
    if directory.exists() {
        std::fs::remove_dir_all(&directory).expect("the previous run's fixture is removed");
    }
    mmap_workloads::run(&["create".to_owned(), directory.display().to_string()])
        .expect("the fixture is created");
    Mutex::new(directory)
});

/// Runs one runner command over the fixture and returns the row it wrote;
/// `arguments` follow the input directory and precede the output path.
fn sample_row(fixture: &Path, command: &str, arguments: &[&str], name: &str) -> serde_json::Value {
    let rows = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mmap-workload-contract-rows");
    std::fs::create_dir_all(&rows).expect("the row directory exists");
    let output = rows.join(format!("{name}.json"));
    if output.exists() {
        std::fs::remove_file(&output).expect("the previous run's row is removed");
    }
    let mut command_line = vec![command.to_owned(), fixture.display().to_string()];
    command_line.extend(arguments.iter().map(|&argument| argument.to_owned()));
    command_line.push(output.display().to_string());
    mmap_workloads::run(&command_line)
        .unwrap_or_else(|failure| panic!("{command} {arguments:?} writes its row: {failure}"));
    serde_json::from_slice(&std::fs::read(&output).expect("the row is readable"))
        .expect("the row is JSON")
}

fn row_checksum(row: &serde_json::Value) -> u64 {
    row["workers"]
        .as_array()
        .expect("the row lists its workers")
        .iter()
        .map(|worker| {
            worker["checksum"]
                .as_u64()
                .expect("each worker reports its checksum")
        })
        .fold(0, u64::wrapping_add)
}

#[test]
fn mmap_workload_cli_rejects_missing_and_unknown_arguments() {
    assert!(mmap_workloads::run(&[]).is_err());
    assert!(mmap_workloads::run(&["unknown".to_owned()]).is_err());
}

#[test]
fn scan_geometry_catalog_is_available_without_creating_a_fixture() {
    assert!(mmap_workloads::run(&["scan-list".to_owned()]).is_ok());
}

#[test]
fn readv_probe_catalog_is_available_without_creating_a_fixture() {
    assert!(mmap_workloads::run(&["probe-list".to_owned()]).is_ok());
}

/// Fair lanes: name, product arm, bytes per read, repetitions of the 65,536-request sequence.
const FAIR_LANES: [(&str, &str, u64, u64); 4] = [
    (
        "resident_access_session_projection",
        "dios_session",
        64,
        2_048,
    ),
    ("resident_access_session_fold", "dios_session", 4_096, 256),
    (
        "resident_access_session_fold_256m",
        "dios_session",
        4_096,
        64,
    ),
    ("resident_access_fair_get", "dios_get", 64, 2_048),
];

fn absent_fixture(purpose: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "dios-{purpose}-fixture-absent-{}",
        std::process::id()
    ))
}

fn sample_without_fixture(lane: &str, arm: &str, tracing: &str, purpose: &str) -> String {
    let missing = absent_fixture(purpose);
    mmap_workloads::run(&[
        "sample".to_owned(),
        missing.display().to_string(),
        lane.to_owned(),
        arm.to_owned(),
        "0".to_owned(),
        tracing.to_owned(),
        missing
            .join(format!("{lane}-{arm}.json"))
            .display()
            .to_string(),
    ])
    .expect_err("no fixture exists at the input")
}

#[test]
fn fair_lanes_pair_the_mmap_arm_with_the_product_arm_and_carry_the_plain_array_floor() {
    let catalog = mmap_workloads::lane_catalog();
    for (name, product_arm, bytes_per_read, repetitions) in FAIR_LANES {
        let lane = catalog
            .iter()
            .find(|lane| lane["name"] == name)
            .unwrap_or_else(|| panic!("{name} is cataloged"));
        assert_eq!(
            lane["arms"],
            serde_json::json!(["mmap_random", product_arm]),
            "{name} pairs mmap_random against {product_arm}"
        );
        assert_eq!(
            lane["floor_arm"], "plain_array",
            "{name} carries the plain_array floor arm"
        );
        assert_eq!(lane["cache"], "resident", "{name} is a resident lane");
        assert_eq!(
            lane["operations"],
            65_536 * repetitions,
            "{name} times the 65,536-request sequence {repetitions} times"
        );
        assert_eq!(
            lane["useful_bytes_per_read"], bytes_per_read,
            "{name} reads {bytes_per_read} bytes per request"
        );
        for arm in ["mmap_random", product_arm, "plain_array"] {
            let error = sample_without_fixture(name, arm, "plain", "fair-lane");
            assert!(
                !error.starts_with("unknown") && error != "arm does not belong to this comparison",
                "{name} must accept the {arm} arm: {error}"
            );
        }
    }

    let fold_256m = catalog
        .iter()
        .find(|lane| lane["name"] == "resident_access_session_fold_256m")
        .expect("resident_access_session_fold_256m is cataloged");
    assert_eq!(
        fold_256m["frames"], 65_536,
        "all 65,536 fixture pages are resident in the pool"
    );

    let ordinary = catalog
        .iter()
        .find(|lane| lane["name"] == "resident_access_ordinary")
        .expect("resident_access_ordinary is cataloged");
    assert_eq!(
        ordinary["operations"], 65_536,
        "legacy resident lanes keep one pass of the sequence"
    );
}

#[test]
fn session_lanes_name_the_flat_reference_arm_and_the_get_lane_names_none() {
    let catalog = mmap_workloads::lane_catalog();
    for (name, product_arm, _, _) in FAIR_LANES {
        let lane = catalog
            .iter()
            .find(|lane| lane["name"] == name)
            .unwrap_or_else(|| panic!("{name} is cataloged"));
        if product_arm == "dios_session" {
            assert_eq!(
                lane["flat_arm"], "mmap_random_flat",
                "{name} names mmap_random_flat as its flat reference arm"
            );
            assert_eq!(
                lane["arms"],
                serde_json::json!(["mmap_random", "dios_session"]),
                "{name} keeps the flat arm out of the gated pair"
            );
            let error = sample_without_fixture(name, "mmap_random_flat", "plain", "flat-arm");
            assert!(
                !error.starts_with("unknown") && error != "arm does not belong to this comparison",
                "{name} must accept the mmap_random_flat arm: {error}"
            );
        } else {
            assert!(
                lane["flat_arm"].is_null(),
                "{name} has no sessions and names no flat reference arm"
            );
        }
    }
}

/// Copy lanes: name and base arm; each times `dios_copy` against its base.
const COPY_LANES: [(&str, &str); 2] = [
    ("resident_access_copy", "mmap_copy"),
    ("resident_access_copy_floor", "optimistic_floor"),
];

#[test]
fn copy_lanes_pair_their_base_arm_with_dios_copy_and_carry_the_plain_array_floor() {
    let catalog = mmap_workloads::lane_catalog();
    for (name, base_arm) in COPY_LANES {
        let lane = catalog
            .iter()
            .find(|lane| lane["name"] == name)
            .unwrap_or_else(|| panic!("{name} is cataloged"));
        assert_eq!(
            lane["arms"],
            serde_json::json!([base_arm, "dios_copy"]),
            "{name} pairs {base_arm} as base against dios_copy"
        );
        assert_eq!(
            lane["floor_arm"], "plain_array",
            "{name} carries the plain_array floor arm"
        );
        assert!(lane["flat_arm"].is_null(), "{name} has no flat arm");
        assert_eq!(lane["cache"], "resident", "{name} is a resident lane");
        assert_eq!(
            lane["operations"],
            65_536 * 2_048,
            "{name} times the 65,536-request sequence 2,048 times"
        );
        assert_eq!(
            lane["useful_bytes_per_read"], 64,
            "{name} copies 64 bytes per request"
        );
        assert_eq!(lane["frames"], 1_024, "{name} reads the 1,024-page set");
        for arm in [base_arm, "dios_copy", "plain_array"] {
            let error = sample_without_fixture(name, arm, "plain", "copy-lane");
            assert!(
                !error.starts_with("unknown") && error != "arm does not belong to this comparison",
                "{name} must accept the {arm} arm: {error}"
            );
            let error = sample_without_fixture(name, arm, "trace", "copy-trace");
            assert!(
                error.contains("trace"),
                "{name} refuses trace mode on {arm} before the fixture is read: {error}"
            );
        }
    }
}

#[test]
fn dios_rows_record_translation_memory_beside_the_arena_size() {
    let fixture = FIXTURE.lock().unwrap_or_else(PoisonError::into_inner);
    let row = sample_row(
        &fixture,
        "sample",
        &["resident_access_ordinary", "dios_serial", "0", "plain"],
        "translation-memory",
    );
    let memory = &row["translation_memory"];
    let field = |name: &str| {
        memory[name]
            .as_u64()
            .unwrap_or_else(|| panic!("a dios_serial row records translation_memory.{name}: {row}"))
    };
    let mapping_bytes = field("mapping_bytes");
    let resident_bytes = field("resident_bytes");
    let arena_bytes = field("arena_bytes");
    assert!(
        resident_bytes > 0,
        "65,536 reads through the pool leave translation pages resident"
    );
    assert!(
        resident_bytes <= mapping_bytes,
        "resident bytes {resident_bytes} lie within the translation mapping of {mapping_bytes} bytes"
    );
    assert_eq!(
        resident_bytes % 4096,
        0,
        "residency is counted in whole 4 KiB pages of the reservation"
    );
    assert_eq!(
        arena_bytes,
        row["frames"].as_u64().expect("the row records its frames") * 4096,
        "the arena holds one 4 KiB granule per frame"
    );
}

const AT_G1_LANE: &str = "resident_access_session_projection";
const SESSION_LENGTHS: [u64; 4] = [1, 4, 16, 64];

#[test]
fn session_length_sweep_rows_carry_their_length_and_the_work_of_the_gated_sixteen() {
    let fixture = FIXTURE.lock().unwrap_or_else(PoisonError::into_inner);
    for arm in ["dios_session", "mmap_random"] {
        let gated = sample_row(
            &fixture,
            "sample",
            &[AT_G1_LANE, arm, "0", "plain"],
            &format!("gated-{arm}"),
        );
        assert_eq!(
            gated["session_requests"], 16,
            "the gated {arm} row of the AT-G1 lane runs sessions of exactly 16 and says so"
        );
        for length in SESSION_LENGTHS {
            let swept = sample_row(
                &fixture,
                "session-sample",
                &[AT_G1_LANE, arm, "0", &length.to_string()],
                &format!("sweep-{arm}-{length}"),
            );
            assert_eq!(
                swept["session_requests"], length,
                "the {arm} sweep row records its session length {length}"
            );
            assert_eq!(
                (&swept["lane"], &swept["arm"]),
                (&gated["lane"], &gated["arm"]),
                "the sweep samples the AT-G1 lane's {arm} arm"
            );
            for field in ["operations", "useful_bytes", "expected_checksum"] {
                assert_eq!(
                    swept[field], gated[field],
                    "the {arm} sweep row at length {length} keeps the gated row's {field}"
                );
            }
            assert_eq!(
                row_checksum(&swept),
                row_checksum(&gated),
                "the {arm} sweep row at length {length} folds the gated row's bytes"
            );
        }
    }
}

#[test]
fn fair_lanes_refuse_trace_mode_on_every_arm() {
    for (name, product_arm, _, _) in FAIR_LANES {
        for arm in ["mmap_random", product_arm, "plain_array"] {
            let error = sample_without_fixture(name, arm, "trace", "fair-trace");
            assert!(
                error.contains("trace"),
                "{name} refuses trace mode on {arm} before the fixture is read: {error}"
            );
        }
    }
}
