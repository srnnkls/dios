use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(dios_resident_hint)");
    println!("cargo::rustc-check-cfg=cfg(dios_reclamation_observation)");
    println!("cargo::rerun-if-env-changed=CARGO_FEATURE_BENCH");
    println!("cargo::rerun-if-env-changed=DIOS_DRP_PRODUCT_HARNESS");
    println!("cargo::rerun-if-env-changed=DIOS_PRODUCT_WORKTREE");
    println!("cargo::rerun-if-env-changed=DIOS_PRODUCT_SOURCE_COMMIT");
    if env::var_os("CARGO_FEATURE_BENCH").is_none()
        && env::var_os("DIOS_DRP_PRODUCT_HARNESS").is_none()
    {
        return;
    }
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let product = env::var_os("DIOS_PRODUCT_WORKTREE").map_or(manifest, PathBuf::from);
    let pool_path = product.join("src/pool/mod.rs");
    println!("cargo::rerun-if-changed={}", pool_path.display());
    println!(
        "cargo::rustc-env=DIOS_PRODUCT_WORKTREE={}",
        product.display()
    );
    let commit = env::var("DIOS_PRODUCT_SOURCE_COMMIT").unwrap_or_else(|_| {
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&product)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .unwrap_or_else(|| "0000000000000000000000000000000000000000".to_owned())
    });
    println!(
        "cargo::rustc-env=DIOS_PRODUCT_SOURCE_COMMIT={}",
        commit.trim()
    );
    let pool = fs::read_to_string(pool_path).expect("read pool source for API selection");
    if pool.contains("pub fn get_with_hint")
        && pool.contains("pub fn resident_hint")
        && pool.contains("pub fn lease_file")
    {
        println!("cargo::rustc-cfg=dios_resident_hint");
    }
    if pool.contains("reclamation_epochs_observed_internal") {
        println!("cargo::rustc-cfg=dios_reclamation_observation");
    }
}
