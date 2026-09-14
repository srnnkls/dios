//! The workload suite must prove its access patterns before publishing timing.

#[path = "../benches/workload_suite/mod.rs"]
mod workload_suite;

#[test]
fn real_pool_workloads_prove_equal_work_and_their_intended_paths() {
    let output = std::env::temp_dir().join(format!(
        "dios-workload-contract-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    workload_suite::run(&["smoke".to_owned(), output.to_string_lossy().into_owned()])
        .expect("all seven shipping-pool workloads validate");
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(output.join("manifest.json")).expect("read manifest"),
    )
    .expect("valid JSON manifest");
    assert_eq!(manifest["mode"], "smoke");
    assert_eq!(manifest["lanes"].as_array().expect("lane list").len(), 7);
    let measurements =
        std::fs::read_to_string(output.join("measurements.csv")).expect("rich measurements");
    assert_eq!(measurements.lines().count(), 15, "header plus seven pairs");
    assert!(!output.join("point_batch.csv").exists());
    std::fs::remove_dir_all(output).expect("remove this test's artifacts");
}

#[test]
fn unknown_workloads_and_extra_arguments_fail_before_creating_artifacts() {
    assert!(workload_suite::run(&["list".to_owned(), "unexpected".to_owned()]).is_err());
    assert!(
        workload_suite::run(&[
            "run".to_owned(),
            "/unused-workload-output".to_owned(),
            "unknown".to_owned(),
        ])
        .is_err()
    );
}
