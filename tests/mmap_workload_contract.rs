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
