#![cfg(target_os = "linux")]

#[path = "../benches/mmap_workloads/mod.rs"]
mod mmap_workloads;

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

fn absent_fixture(purpose: &str) -> std::path::PathBuf {
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
