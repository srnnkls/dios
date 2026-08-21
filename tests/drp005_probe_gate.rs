use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

static TEMP_SEQUENCE: AtomicU32 = AtomicU32::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create() -> Self {
        let base =
            std::env::var_os("CARGO_TARGET_TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        for attempt in 0_u32..128 {
            let name = format!("dios-drp005-{}-{sequence}-{attempt}", std::process::id());
            let path = base.join(name);
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create test-owned DRP-G1 directory: {error}"),
            }
        }
        panic!("could not create a unique DRP-G1 test directory");
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove test-owned DRP-G1 directory");
    }
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
        .expect("execute DRP-G1 task")
}

fn diagnostic(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[derive(Clone, Copy)]
enum ValidationMode {
    StructureOnly,
    Gate,
}

fn validate(path: &Path, mode: ValidationMode) -> Output {
    match mode {
        ValidationMode::StructureOnly => run_task(
            "validate-drp-g1-probes",
            [OsStr::new("--structure-only"), path.as_os_str()],
        ),
        ValidationMode::Gate => run_task("validate-drp-g1-probes", [path.as_os_str()]),
    }
}

fn remove_phase_row(source: &str, phase: &str) -> String {
    let prefix = format!("{phase},");
    let mut removed = false;
    let mut rows = Vec::new();
    for row in source.lines() {
        if !removed && row.starts_with(&prefix) {
            removed = true;
        } else {
            rows.push(row);
        }
    }
    assert!(removed, "generated artifact must contain a {phase} row");
    format!("{}\n", rows.join("\n"))
}

fn replace_first_field(source: &str, field: &str, replacement: &str) -> String {
    let mut rows = source.lines();
    let header = rows.next().expect("generated artifact has a header");
    let index = header
        .split(',')
        .position(|column| column == field)
        .expect("generated artifact has the frozen field");
    let mut output = vec![header.to_owned()];
    let mut replaced = false;
    for row in rows {
        let mut columns: Vec<&str> = row.split(',').collect();
        if !replaced {
            columns[index] = replacement;
            replaced = true;
        }
        output.push(columns.join(","));
    }
    assert!(replaced, "generated artifact has at least one data row");
    format!("{}\n", output.join("\n"))
}

fn write_new(path: &Path, contents: &str) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .expect("create a new DRP-G1 mutation artifact");
    file.write_all(contents.as_bytes())
        .expect("write DRP-G1 mutation artifact");
}

fn assert_rejected(dir: &Path, name: &str, contents: &str, category: &str) {
    let path = dir.join(name);
    write_new(&path, contents);
    let output = validate(&path, ValidationMode::StructureOnly);
    let diagnostic = diagnostic(&output).to_ascii_lowercase();
    assert!(!output.status.success(), "invalid quality rows must fail");
    assert!(
        diagnostic.contains(category),
        "rejection must identify {category}: {diagnostic}"
    );
}

#[test]
fn generated_probe_matrix_validates_and_single_mutations_fail() {
    let dir = TestDir::create();
    let complete = dir.path().join("complete.csv");
    let generated = run_task("generate-drp-g1-probes", [&complete]);
    assert!(
        generated.status.success(),
        "production generator must emit the quality artifact: {}",
        diagnostic(&generated)
    );
    let validated = validate(&complete, ValidationMode::StructureOnly);
    assert!(
        validated.status.success(),
        "complete calibration and holdout structure must validate: {}",
        diagnostic(&validated)
    );
    let gate = validate(&complete, ValidationMode::Gate);
    let gate_diagnostic = diagnostic(&gate).to_ascii_lowercase();
    assert!(!gate.status.success(), "failed holdout must block timing");
    assert!(
        gate_diagnostic.contains("holdout"),
        "gate must identify the failed holdout: {gate_diagnostic}"
    );

    let source = fs::read_to_string(&complete).expect("read generated DRP-G1 artifact");
    assert_rejected(
        dir.path(),
        "missing-calibration.csv",
        &remove_phase_row(&source, "calibration"),
        "calibration",
    );
    assert_rejected(
        dir.path(),
        "missing-holdout.csv",
        &remove_phase_row(&source, "holdout"),
        "holdout",
    );
    assert_rejected(
        dir.path(),
        "malformed-statistic.csv",
        &replace_first_field(&source, "candidate_mean", "not-a-number"),
        "candidate_mean",
    );
    assert_rejected(
        dir.path(),
        "generator-mismatch.csv",
        &replace_first_field(&source, "generator_sha256", &"f".repeat(64)),
        "generator",
    );
}
