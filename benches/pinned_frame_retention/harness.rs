use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::common::{display_error, is_lower_hex, sha256_path};

const SUPPORT_FILES: [&str; 5] = [
    "common.rs",
    "platform.rs",
    "workloads.rs",
    "artifacts.rs",
    "harness.rs",
];

#[derive(Clone, Debug)]
pub(crate) struct ProductIdentity {
    pub(crate) source_commit: String,
    pub(crate) executable_sha256: String,
    pub(crate) cargo_lock_sha256: String,
    pub(crate) harness_cargo_lock_sha256: String,
    pub(crate) rust_version: String,
    pub(crate) runner_sha256: String,
    pub(crate) build_profile: String,
}

pub(crate) fn prepare(product: &Path, harness: &Path) -> Result<(), String> {
    let product = fs::canonicalize(product)
        .map_err(|error| format!("resolve product {}: {error}", product.display()))?;
    require_clean_product(&product)?;
    if harness.exists() {
        return Err(format!(
            "harness directory already exists: {}",
            harness.display()
        ));
    }
    if harness.starts_with(&product) {
        return Err("product harness must remain outside its clean source worktree".to_owned());
    }
    fs::create_dir_all(harness.join("pinned_frame_retention")).map_err(display_error)?;
    fs::create_dir_all(harness.join(".cargo")).map_err(display_error)?;
    copy_runner(harness)?;
    write_manifest(&product, harness)?;
    write_build_script(&product, harness)?;
    write_config(&product, harness)
}

fn copy_runner(harness: &Path) -> Result<(), String> {
    let root = repository_root();
    fs::copy(
        root.join("benches/pinned_frame_retention.rs"),
        harness.join("pinned_frame_retention.rs"),
    )
    .map_err(display_error)?;
    for file in SUPPORT_FILES {
        fs::copy(
            root.join("benches/pinned_frame_retention").join(file),
            harness.join("pinned_frame_retention").join(file),
        )
        .map_err(display_error)?;
    }
    Ok(())
}

fn write_manifest(product: &Path, harness: &Path) -> Result<(), String> {
    let product = toml_path(product)?;
    let manifest = format!(
        "[package]\nname = \"dios-pfr-product-harness\"\nversion = \"0.0.0\"\nedition = \"2024\"\nrust-version = \"1.95\"\npublish = false\nbuild = \"build.rs\"\n\n[features]\nbench = []\npfr-product-retention = []\n\n[dependencies]\ndios = {{ path = \"{product}\", features = [\"bench\"] }}\nserde_json = \"1\"\nsha2 = \"0.10\"\n\n[build-dependencies]\nsha2 = \"0.10\"\n\n[lints.clippy]\npedantic = {{ level = \"deny\", priority = -1 }}\nallow_attributes = \"deny\"\nallow_attributes_without_reason = \"deny\"\nundocumented_unsafe_blocks = \"deny\"\n\n[profile.release]\ndebug = true\nstrip = false\n\n[[bin]]\nname = \"pinned_frame_retention\"\npath = \"pinned_frame_retention.rs\"\n"
    );
    fs::write(harness.join("Cargo.toml"), manifest).map_err(display_error)
}

fn write_build_script(product: &Path, harness: &Path) -> Result<(), String> {
    let product = toml_path(product)?;
    let script = BUILD_SCRIPT.replace("__PRODUCT__", &format!("{product:?}"));
    fs::write(harness.join("build.rs"), script).map_err(display_error)
}

const BUILD_SCRIPT: &str = r#"use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest as _, Sha256};

const PRODUCT: &str = __PRODUCT__;
const SUPPORT_FILES: [&str; 5] = [
    "common.rs",
    "platform.rs",
    "workloads.rs",
    "artifacts.rs",
    "harness.rs",
];

fn main() {
    println!("cargo::rustc-check-cfg=cfg(pfr_product_retention)");
    if env::var_os("CARGO_FEATURE_PFR_PRODUCT_RETENTION").is_some() {
        println!("cargo::rustc-cfg=pfr_product_retention");
    }
    let harness = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let profile = env::var("PROFILE").expect("build profile");
    assert_eq!(profile, "release", "product harness requires release profile");
    require_clean_source();
    let source_commit = git_output(&["rev-parse", "HEAD"]);
    let product_lock = sha256_path(&Path::new(PRODUCT).join("Cargo.lock"));
    let harness_lock = sha256_path(&harness.join("Cargo.lock"));
    let runner = runner_sha256(&harness);
    let rust = rust_version();
    emit_identity(&source_commit, &product_lock, &harness_lock, &runner, &rust, &profile);
    println!("cargo::rerun-if-changed={PRODUCT}");
    println!("cargo::rerun-if-changed={}", harness.join("Cargo.lock").display());
    println!("cargo::rerun-if-changed={}", harness.join("pinned_frame_retention.rs").display());
    for file in SUPPORT_FILES {
        println!(
            "cargo::rerun-if-changed={}",
            harness.join("pinned_frame_retention").join(file).display()
        );
    }
}

fn require_clean_source() {
    let output = Command::new("git")
        .args(["status", "--porcelain=v1", "--", ".", ":(exclude)Cargo.lock"])
        .current_dir(PRODUCT)
        .output()
        .expect("inspect product source");
    assert!(output.status.success(), "inspect product source");
    assert!(output.stdout.is_empty(), "product source must be clean at build time");
}

fn emit_identity(source: &str, product_lock: &str, harness_lock: &str, runner: &str, rust: &str, profile: &str) {
    println!("cargo::rustc-env=DIOS_PRODUCT_WORKTREE={PRODUCT}");
    println!("cargo::rustc-env=DIOS_PRODUCT_SOURCE_COMMIT={source}");
    println!("cargo::rustc-env=DIOS_PRODUCT_CARGO_LOCK_SHA256={product_lock}");
    println!("cargo::rustc-env=DIOS_HARNESS_CARGO_LOCK_SHA256={harness_lock}");
    println!("cargo::rustc-env=DIOS_RUNNER_SHA256={runner}");
    println!("cargo::rustc-env=DIOS_BUILD_RUST_VERSION={rust}");
    println!("cargo::rustc-env=DIOS_BUILD_PROFILE={profile}");
}

fn git_output(args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(PRODUCT)
        .output()
        .expect("run git");
    assert!(output.status.success(), "git command failed");
    String::from_utf8(output.stdout).expect("UTF-8 git output").trim().to_owned()
}

fn rust_version() -> String {
    let rustc = env::var("RUSTC").expect("Rust compiler path");
    let output = Command::new(rustc).arg("--version").output().expect("run rustc");
    assert!(output.status.success(), "rustc --version failed");
    String::from_utf8(output.stdout).expect("UTF-8 rustc output").trim().to_owned()
}

fn runner_sha256(harness: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(fs::read(harness.join("pinned_frame_retention.rs")).expect("read runner"));
    for file in SUPPORT_FILES {
        digest.update(file.as_bytes());
        digest.update(fs::read(harness.join("pinned_frame_retention").join(file)).expect("read support"));
    }
    format!("{:x}", digest.finalize())
}

fn sha256_path(path: &Path) -> String {
    format!("{:x}", Sha256::digest(fs::read(path).expect("read identity input")))
}
"#;

fn write_config(product: &Path, harness: &Path) -> Result<(), String> {
    let product = toml_path(product)?;
    let config =
        format!("[env]\nDIOS_PRODUCT_WORKTREE = {{ value = \"{product}\", force = true }}\n");
    fs::write(harness.join(".cargo/config.toml"), config).map_err(display_error)
}

fn toml_path(path: &Path) -> Result<String, String> {
    let value = path
        .to_str()
        .ok_or_else(|| "product path is not UTF-8".to_owned())?;
    if value.contains(['"', '\n', '\r', '\\']) {
        return Err("product path cannot be represented in the harness manifest".to_owned());
    }
    Ok(value.to_owned())
}

pub(crate) fn runtime_identity() -> Result<ProductIdentity, String> {
    let (identity, state_matches) = reported_identity()?;
    if !state_matches {
        return Err(
            "runtime source, runner, or lock state differs from the sealed build".to_owned(),
        );
    }
    Ok(identity)
}

pub(crate) fn reported_identity() -> Result<(ProductIdentity, bool), String> {
    let mut identity = embedded_identity();
    let executable = std::env::current_exe().map_err(display_error)?;
    identity.executable_sha256 = sha256_path(&executable)?;
    validate_identity_shape(&identity)?;
    let product = Path::new(env!("DIOS_PRODUCT_WORKTREE"));
    let harness = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source_clean = clean_product_source(product)?;
    let state_matches = source_clean
        && git_output(product, &["rev-parse", "HEAD"])? == identity.source_commit
        && sha256_path(&product.join("Cargo.lock"))? == identity.cargo_lock_sha256
        && sha256_path(&harness.join("Cargo.lock"))? == identity.harness_cargo_lock_sha256
        && runner_sha256()? == identity.runner_sha256;
    Ok((identity, state_matches))
}

fn embedded_identity() -> ProductIdentity {
    ProductIdentity {
        source_commit: env!("DIOS_PRODUCT_SOURCE_COMMIT").to_owned(),
        executable_sha256: String::new(),
        cargo_lock_sha256: env!("DIOS_PRODUCT_CARGO_LOCK_SHA256").to_owned(),
        harness_cargo_lock_sha256: env!("DIOS_HARNESS_CARGO_LOCK_SHA256").to_owned(),
        rust_version: env!("DIOS_BUILD_RUST_VERSION").to_owned(),
        runner_sha256: env!("DIOS_RUNNER_SHA256").to_owned(),
        build_profile: env!("DIOS_BUILD_PROFILE").to_owned(),
    }
}

pub(crate) fn identity_json(identity: &ProductIdentity, runtime_state_matches: bool) -> Value {
    json!({
        "source_commit": identity.source_commit,
        "executable_sha256": identity.executable_sha256,
        "cargo_lock_sha256": identity.cargo_lock_sha256,
        "harness_cargo_lock_sha256": identity.harness_cargo_lock_sha256,
        "rust_version": identity.rust_version,
        "runner_sha256": identity.runner_sha256,
        "build_profile": identity.build_profile,
        "runtime_state_matches_build": runtime_state_matches,
    })
}

pub(crate) fn parse_identity(bytes: &[u8]) -> Result<ProductIdentity, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(display_error)?;
    if value["runtime_state_matches_build"] != true {
        return Err("product runtime state differs from its sealed build identity".to_owned());
    }
    let identity = ProductIdentity {
        source_commit: json_string(&value, "source_commit")?,
        executable_sha256: json_string(&value, "executable_sha256")?,
        cargo_lock_sha256: json_string(&value, "cargo_lock_sha256")?,
        harness_cargo_lock_sha256: json_string(&value, "harness_cargo_lock_sha256")?,
        rust_version: json_string(&value, "rust_version")?,
        runner_sha256: json_string(&value, "runner_sha256")?,
        build_profile: json_string(&value, "build_profile")?,
    };
    validate_identity_shape(&identity)?;
    Ok(identity)
}

fn validate_identity_shape(identity: &ProductIdentity) -> Result<(), String> {
    if !is_lower_hex(&identity.source_commit, 40)
        || !is_lower_hex(&identity.executable_sha256, 64)
        || !is_lower_hex(&identity.cargo_lock_sha256, 64)
        || !is_lower_hex(&identity.harness_cargo_lock_sha256, 64)
        || !is_lower_hex(&identity.runner_sha256, 64)
        || !identity.rust_version.starts_with("rustc 1.96.0")
        || identity.build_profile != "release"
    {
        return Err(
            "product identity is incomplete or uses the wrong build configuration".to_owned(),
        );
    }
    Ok(())
}

pub(crate) fn runner_sha256() -> Result<String, String> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = if manifest.join("benches/pinned_frame_retention.rs").exists() {
        manifest.join("benches")
    } else {
        manifest.to_path_buf()
    };
    let mut digest = Sha256::new();
    digest.update(fs::read(root.join("pinned_frame_retention.rs")).map_err(display_error)?);
    for file in SUPPORT_FILES {
        digest.update(file.as_bytes());
        digest.update(
            fs::read(root.join("pinned_frame_retention").join(file)).map_err(display_error)?,
        );
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub(crate) fn require_clean_product(product: &Path) -> Result<(), String> {
    if clean_product_source_and_lock(product)? {
        Ok(())
    } else {
        Err(format!(
            "product worktree is not clean: {}",
            product.display()
        ))
    }
}

fn clean_product_source_and_lock(product: &Path) -> Result<bool, String> {
    let output = Command::new("git")
        .args(["status", "--porcelain=v1"])
        .current_dir(product)
        .output()
        .map_err(display_error)?;
    if !output.status.success() {
        return Err(format!("{} is not a Git worktree", product.display()));
    }
    Ok(output.stdout.is_empty())
}

fn clean_product_source(product: &Path) -> Result<bool, String> {
    let output = Command::new("git")
        .args([
            "status",
            "--porcelain=v1",
            "--",
            ".",
            ":(exclude)Cargo.lock",
        ])
        .current_dir(product)
        .output()
        .map_err(display_error)?;
    if !output.status.success() {
        return Err(format!("{} is not a Git worktree", product.display()));
    }
    Ok(output.stdout.is_empty())
}

pub(crate) fn command_output(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(display_error)?;
    if !output.status.success() {
        return Err(format!("{program} failed with {}", output.status));
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(display_error)
}

fn git_output(product: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(product)
        .output()
        .map_err(display_error)?;
    if !output.status.success() {
        return Err(format!("git {args:?} failed in {}", product.display()));
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(display_error)
}

fn json_string(value: &Value, field: &str) -> Result<String, String> {
    value[field]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("identity JSON has no {field}"))
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}
