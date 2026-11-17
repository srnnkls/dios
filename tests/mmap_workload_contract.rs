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

#[test]
fn session_lanes_pair_the_mmap_arm_with_the_session_arm() {
    let missing = std::env::temp_dir().join(format!(
        "dios-session-lane-fixture-absent-{}",
        std::process::id()
    ));
    for lane in [
        "resident_access_session_projection",
        "resident_access_session_fold",
    ] {
        for arm in ["mmap_random", "dios_session"] {
            let output = missing.join(format!("{lane}-{arm}.json"));
            let error = mmap_workloads::run(&[
                "sample".to_owned(),
                missing.display().to_string(),
                lane.to_owned(),
                arm.to_owned(),
                "0".to_owned(),
                "plain".to_owned(),
                output.display().to_string(),
            ])
            .expect_err("no fixture exists at the input");
            assert!(
                !error.starts_with("unknown") && error != "arm does not belong to this comparison",
                "{lane} must catalog the {arm} arm: {error}"
            );
        }
    }
}

#[test]
fn session_lanes_time_the_fixed_sequence_repeated_256_times() {
    let catalog = mmap_workloads::lane_catalog();
    for name in [
        "resident_access_session_projection",
        "resident_access_session_fold",
    ] {
        let lane = catalog
            .iter()
            .find(|lane| lane["name"] == name)
            .unwrap_or_else(|| panic!("{name} is cataloged"));
        assert_eq!(
            lane["operations"], 16_777_216,
            "{name} times the 65,536-request sequence 256 times"
        );
    }
    let ordinary = catalog
        .iter()
        .find(|lane| lane["name"] == "resident_access_ordinary")
        .expect("resident_access_ordinary is cataloged");
    assert_eq!(
        ordinary["operations"], 65_536,
        "existing resident lanes keep one pass of the sequence"
    );
}

#[test]
fn session_lanes_refuse_trace_mode() {
    let missing = std::env::temp_dir().join(format!(
        "dios-session-trace-fixture-absent-{}",
        std::process::id()
    ));
    for lane in [
        "resident_access_session_projection",
        "resident_access_session_fold",
    ] {
        let error = mmap_workloads::run(&[
            "sample".to_owned(),
            missing.display().to_string(),
            lane.to_owned(),
            "mmap_random".to_owned(),
            "0".to_owned(),
            "trace".to_owned(),
            missing.join(format!("{lane}.json")).display().to_string(),
        ])
        .expect_err("trace mode is refused before the fixture is read");
        assert!(
            error.contains("trace"),
            "{lane} refuses trace mode, whose event buffer would exceed the sequence: {error}"
        );
    }
}
