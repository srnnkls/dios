use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest as _, Sha256};

const PFR_SUPPORT_FILES: [&str; 5] = [
    "common.rs",
    "platform.rs",
    "workloads.rs",
    "artifacts.rs",
    "harness.rs",
];

fn main() {
    println!("cargo::rustc-check-cfg=cfg(dios_resident_hint)");
    println!("cargo::rustc-check-cfg=cfg(dios_reclamation_observation)");
    println!("cargo::rustc-check-cfg=cfg(pfr_product_retention)");
    println!("cargo::rerun-if-env-changed=CARGO_FEATURE_BENCH");
    if env::var_os("CARGO_FEATURE_BENCH").is_some() {
        println!("cargo::rustc-cfg=pfr_product_retention");
    }
    println!("cargo::rerun-if-env-changed=DIOS_DRP_PRODUCT_HARNESS");
    println!("cargo::rerun-if-env-changed=DIOS_PRODUCT_WORKTREE");
    println!("cargo::rerun-if-env-changed=DIOS_PRODUCT_SOURCE_COMMIT");
    if env::var_os("CARGO_FEATURE_BENCH").is_none()
        && env::var_os("DIOS_DRP_PRODUCT_HARNESS").is_none()
    {
        return;
    }
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let product =
        env::var_os("DIOS_PRODUCT_WORKTREE").map_or_else(|| manifest.clone(), PathBuf::from);
    let pool_path = product.join("src/pool/mod.rs");
    println!("cargo::rerun-if-changed={}", pool_path.display());
    println!(
        "cargo::rustc-env=DIOS_PRODUCT_WORKTREE={}",
        product.display()
    );
    main_git_rerun_inputs(&product);
    let commit = env::var("DIOS_PRODUCT_SOURCE_COMMIT").unwrap_or_else(|_| {
        main_git_output(&product, &["rev-parse", "HEAD"])
            .unwrap_or_else(|| "0000000000000000000000000000000000000000".to_owned())
    });
    println!(
        "cargo::rustc-env=DIOS_PRODUCT_SOURCE_COMMIT={}",
        commit.trim()
    );
    if env::var("CARGO_PKG_NAME").as_deref() == Ok("dios") {
        main_bench_hashes(&manifest, &product);
    }
    println!(
        "cargo::rustc-env=DIOS_BUILD_RUST_VERSION={}",
        rustc_version()
    );
    println!(
        "cargo::rustc-env=DIOS_BUILD_PROFILE={}",
        env::var("PROFILE").expect("build profile")
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

fn main_git_rerun_inputs(product: &Path) {
    main_git_rerun_input(product, "HEAD");
    if let Some(reference) = main_git_output(product, &["symbolic-ref", "-q", "HEAD"]) {
        main_git_rerun_input(product, &reference);
        main_git_rerun_input(product, "packed-refs");
    }
}

fn main_git_rerun_input(product: &Path, git_path: &str) {
    if let Some(path) = main_git_output(
        product,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            git_path,
        ],
    ) {
        println!("cargo::rerun-if-changed={path}");
    }
}

fn main_git_output(product: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .current_dir(product)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|output| output.trim().to_owned())
}

fn main_bench_hashes(manifest: &Path, product: &Path) {
    let product_lock = main_bench_hashes_sha256_path(&product.join("Cargo.lock"));
    let harness_lock = main_bench_hashes_sha256_path(&manifest.join("Cargo.lock"));
    let runner = main_bench_hashes_runner(manifest);
    println!("cargo::rustc-env=DIOS_PRODUCT_CARGO_LOCK_SHA256={product_lock}");
    println!("cargo::rustc-env=DIOS_HARNESS_CARGO_LOCK_SHA256={harness_lock}");
    println!("cargo::rustc-env=DIOS_RUNNER_SHA256={runner}");
    println!(
        "cargo::rerun-if-changed={}",
        product.join("Cargo.lock").display()
    );
    println!(
        "cargo::rerun-if-changed={}",
        manifest.join("Cargo.lock").display()
    );
    let root = manifest.join("benches");
    println!(
        "cargo::rerun-if-changed={}",
        root.join("pinned_frame_retention.rs").display()
    );
    for file in PFR_SUPPORT_FILES {
        println!(
            "cargo::rerun-if-changed={}",
            root.join("pinned_frame_retention").join(file).display()
        );
    }
}

fn main_bench_hashes_runner(manifest: &Path) -> String {
    let root = manifest.join("benches");
    let mut digest = Sha256::new();
    digest.update(fs::read(root.join("pinned_frame_retention.rs")).expect("read PFR runner"));
    for file in PFR_SUPPORT_FILES {
        digest.update(file.as_bytes());
        digest.update(
            fs::read(root.join("pinned_frame_retention").join(file))
                .expect("read PFR runner support"),
        );
    }
    format!("{:x}", digest.finalize())
}

fn main_bench_hashes_sha256_path(path: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(fs::read(path).expect("read identity input"))
    )
}

fn rustc_version() -> String {
    let rustc = env::var_os("RUSTC").expect("Rust compiler path");
    Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map_or_else(
            || "rustc unavailable".to_owned(),
            |version| version.trim().to_owned(),
        )
}
