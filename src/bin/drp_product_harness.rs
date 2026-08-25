use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

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
    let [product, harness] = args else {
        return Err("usage: drp_product_harness PRODUCT_WORKTREE HARNESS_DIR".to_owned());
    };
    let product = fs::canonicalize(product)
        .map_err(|error| format!("resolve product worktree {product:?}: {error}"))?;
    require_clean_product(&product)?;
    let harness = PathBuf::from(harness);
    if harness.exists() {
        return Err(format!(
            "harness directory already exists: {}",
            harness.display()
        ));
    }
    if path_would_be_inside(&harness, &product)? {
        return Err("harness directory must be outside the clean product worktree".to_owned());
    }
    fs::create_dir_all(harness.join(".cargo"))
        .map_err(|error| format!("create {}: {error}", harness.display()))?;
    write_harness(&product, &harness)
}

fn require_clean_product(product: &Path) -> Result<(), String> {
    let status = Command::new("git")
        .args(["status", "--porcelain=v1"])
        .current_dir(product)
        .output()
        .map_err(|error| format!("run git status: {error}"))?;
    if !status.status.success() {
        return Err("product path is not a Git worktree".to_owned());
    }
    if !status.stdout.is_empty() {
        return Err("product worktree must be clean before harness preparation".to_owned());
    }
    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(product)
        .output()
        .map_err(|error| format!("run git rev-parse: {error}"))?;
    if !commit.status.success() || commit.stdout.len() != 41 {
        return Err("product worktree has no exact source commit".to_owned());
    }
    Ok(())
}

fn path_would_be_inside(path: &Path, product: &Path) -> Result<bool, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "harness path has no parent".to_owned())?;
    let parent = fs::canonicalize(parent)
        .map_err(|error| format!("resolve harness parent {}: {error}", parent.display()))?;
    Ok(parent.starts_with(product))
}

fn write_harness(product: &Path, harness: &Path) -> Result<(), String> {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    fs::copy(
        source_root.join("benches/read_path_product.rs"),
        harness.join("read_path_product.rs"),
    )
    .map_err(|error| format!("copy benchmark runner: {error}"))?;
    fs::copy(source_root.join("build.rs"), harness.join("build.rs"))
        .map_err(|error| format!("copy harness build script: {error}"))?;
    let product_text = product
        .to_str()
        .ok_or_else(|| "product worktree path is not UTF-8".to_owned())?;
    if product_text.contains(['"', '\n', '\r']) {
        return Err("product path cannot be represented in the harness manifest".to_owned());
    }
    let manifest = format!(
        "[package]\nname = \"dios-drp-product-harness\"\nversion = \"0.0.0\"\nedition = \"2024\"\nrust-version = \"1.95\"\npublish = false\nbuild = \"build.rs\"\n\n[features]\nbench = []\n\n[dependencies]\ndios = {{ path = \"{product_text}\", features = [\"bench\"] }}\nserde_json = \"1\"\nsha2 = \"0.10\"\n\n[build-dependencies]\nsha2 = \"0.10\"\n\n[profile.profiling]\ninherits = \"release\"\ndebug = true\nstrip = false\n\n[[bin]]\nname = \"read_path_product\"\npath = \"read_path_product.rs\"\n"
    );
    let config = format!(
        "[env]\nDIOS_DRP_PRODUCT_HARNESS = {{ value = \"1\", force = true }}\nDIOS_PRODUCT_WORKTREE = {{ value = \"{product_text}\", force = true }}\n"
    );
    fs::write(harness.join("Cargo.toml"), manifest)
        .map_err(|error| format!("write harness manifest: {error}"))?;
    fs::write(harness.join(".cargo/config.toml"), config)
        .map_err(|error| format!("write harness config: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::write_harness;

    static TEMP_SEQUENCE: AtomicU32 = AtomicU32::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn create() -> Self {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "dios-drp-product-harness-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create test-owned harness directory");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove test-owned harness directory");
        }
    }

    #[test]
    fn generated_harness_exposes_bench_package_feature() {
        let test_dir = TestDir::create();
        let harness = test_dir.0.join("harness");
        fs::create_dir(&harness).expect("create harness directory");
        fs::create_dir(harness.join(".cargo")).expect("create harness Cargo directory");
        write_harness(Path::new(env!("CARGO_MANIFEST_DIR")), &harness)
            .expect("write product harness");

        let output = Command::new(env!("CARGO"))
            .args(["metadata", "--no-deps", "--format-version", "1"])
            .arg("--manifest-path")
            .arg(harness.join("Cargo.toml"))
            .args(["--features", "bench"])
            .output()
            .expect("ask Cargo to select the harness bench feature");

        assert!(
            output.status.success(),
            "generated harness must accept `--features bench`: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("parse Cargo metadata");
        let package = metadata["packages"]
            .as_array()
            .expect("Cargo packages")
            .iter()
            .find(|package| package["name"] == "dios-drp-product-harness")
            .expect("generated harness package");
        let dios_dependency = package["dependencies"]
            .as_array()
            .expect("harness dependencies")
            .iter()
            .find(|dependency| dependency["name"] == "dios")
            .expect("Dios dependency");
        assert!(
            dios_dependency["features"]
                .as_array()
                .expect("Dios dependency features")
                .iter()
                .any(|feature| feature == "bench"),
            "generated harness must enable the Dios dependency's `bench` feature"
        );
        assert!(
            package["features"].get("bench").is_some(),
            "generated harness package must declare its own `bench` feature"
        );
        assert!(
            package["features"]["bench"]
                .as_array()
                .is_some_and(Vec::is_empty),
            "generated harness package `bench` feature must be empty"
        );
    }

    #[test]
    fn generated_harness_builds_with_release_based_profiling_profile() {
        let test_dir = TestDir::create();
        let harness = test_dir.0.join("harness");
        fs::create_dir(&harness).expect("create harness directory");
        fs::create_dir(harness.join(".cargo")).expect("create harness Cargo directory");
        write_harness(Path::new(env!("CARGO_MANIFEST_DIR")), &harness)
            .expect("write product harness");

        let output = Command::new(env!("CARGO"))
            .args([
                "build",
                "--offline",
                "--profile",
                "profiling",
                "--features",
                "bench",
                "--message-format",
                "json",
            ])
            .arg("--manifest-path")
            .arg(harness.join("Cargo.toml"))
            .env("CARGO_TARGET_DIR", test_dir.0.join("target"))
            .output()
            .expect("build generated harness with profiling profile");

        assert!(
            output.status.success(),
            "generated harness must define the profiling profile: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let artifact = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|message| {
                message["reason"] == "compiler-artifact"
                    && message["target"]["name"] == "read_path_product"
                    && message["package_id"]
                        .as_str()
                        .is_some_and(|package_id| package_id.contains("#dios-drp-product-harness@"))
            })
            .expect("generated harness compiler artifact");
        assert_eq!(artifact["profile"]["opt_level"], "3");
        assert_eq!(artifact["profile"]["debuginfo"], 2);
        assert_eq!(artifact["profile"]["debug_assertions"], false);
        assert_eq!(artifact["profile"]["overflow_checks"], false);
    }
}
