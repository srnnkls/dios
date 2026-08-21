use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde::Deserialize;

const CLEAN_BASE: &str = "1004a2e6fcae0bcc9552dc3211c2416e388a250d";
const SCOPE: &str = "scopes/done/dios-r1-r7-read-performance";
const MANIFEST: &str = "resources/r7-source-manifest.json";
const FILE_BYTES_MAX: u64 = 128 * 1024;
const TEMPORARY_DIRECTORY_ATTEMPTS: u32 = 16;
const FORBIDDEN_SYMBOLS: [&str; 5] = [
    "ResidentSet",
    "ResidentSetLease",
    "ResidentSetError",
    "retention_counts",
    "resident_set",
];

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SourceManifest {
    base_commit: String,
    bindings: Vec<Binding>,
    extraction: Extraction,
    extracted_hunks: Vec<ExtractedHunk>,
    forbidden_symbol_scans: Vec<ForbiddenSymbolScan>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Binding {
    path: String,
    bytes: u64,
    sha256: String,
    kind: BindingKind,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum BindingKind {
    BaseToR7SourceCarrier,
    PlanSnapshot,
    IsolatedExtractionOutput,
    InstalledPlanSnapshot,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Extraction {
    isolated_checkout: bool,
    carrier_applied_to_clean_base: bool,
    source: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ExtractedHunk {
    path: String,
    hunk: String,
    classification: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ForbiddenSymbolScan {
    symbol: String,
    matches: Vec<String>,
}

#[test]
fn r7_source_manifest_reconstructs_the_clean_extraction() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    let scope = repository.join(SCOPE);
    let manifest = read_manifest(&scope.join(MANIFEST));
    assert_manifest_contract(&manifest);
    assert_bound_file(
        scope.join(&manifest.bindings[0].path),
        &manifest.bindings[0],
    );
    assert_bound_file(
        scope.join(&manifest.bindings[1].path),
        &manifest.bindings[1],
    );
    assert_bound_file(
        repository.join(&manifest.bindings[6].path),
        &manifest.bindings[6],
    );

    let extraction = TemporaryDirectory::create();
    extract_clean_base(repository, extraction.path());
    apply_carrier(extraction.path(), &scope.join(&manifest.bindings[0].path));
    install_plan_snapshot(&scope, extraction.path(), &manifest.bindings[1]);

    for binding in &manifest.bindings[2..] {
        let output = extraction.path().join(&binding.path);
        assert_bound_file(&output, binding);
        assert_forbidden_symbols_absent(&output);
    }
    extraction.cleanup();
}

#[test]
fn forbidden_symbol_scanner_falsifies_each_symbol() {
    for forbidden in FORBIDDEN_SYMBOLS {
        assert!(
            contains_bytes(forbidden.as_bytes(), forbidden.as_bytes()),
            "the scanner must detect an injected {forbidden}"
        );
    }
}

fn read_manifest(path: &Path) -> SourceManifest {
    let bytes = read_bounded(path, 32 * 1024);
    serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "DRP002 manifest {} must match the fixed schema: {error}",
            path.display()
        )
    })
}

fn assert_manifest_contract(manifest: &SourceManifest) {
    assert_eq!(
        manifest.base_commit, CLEAN_BASE,
        "wrong clean extraction base"
    );
    assert_eq!(
        manifest.bindings.len(),
        7,
        "the fixed schema has seven bindings"
    );
    let expected = expected_bindings();
    for (actual, expected) in manifest.bindings.iter().zip(expected) {
        assert_eq!(actual.path, expected.0, "binding path or order changed");
        assert_eq!(
            actual.bytes, expected.1,
            "{} byte length changed",
            actual.path
        );
        assert_eq!(actual.sha256, expected.2, "{} digest changed", actual.path);
        assert_eq!(
            actual.kind, expected.3,
            "{} binding kind changed",
            actual.path
        );
    }
    assert!(
        manifest.extraction.isolated_checkout,
        "extraction must be isolated"
    );
    assert!(
        manifest.extraction.carrier_applied_to_clean_base,
        "carrier must be applied to the clean base"
    );
    assert_eq!(
        manifest.extraction.source,
        "tracked carrier and plan snapshot only"
    );
    assert_hunks(&manifest.extracted_hunks);
    assert_forbidden_scan_records(&manifest.forbidden_symbol_scans);
}

fn expected_bindings() -> [(&'static str, u64, &'static str, BindingKind); 7] {
    [
        (
            "resources/r7-source.diff",
            16_708,
            "16748dd827b1958b6889535f7244fb8ffe767dac674d587f9fee18738be4a967",
            BindingKind::BaseToR7SourceCarrier,
        ),
        (
            "resources/sira_native_hint.md",
            3_907,
            "337ecf1aafdbded58c38cca5efdf62056e139e2521744d305b43b962ebd62ecf",
            BindingKind::PlanSnapshot,
        ),
        (
            "src/lib.rs",
            13_755,
            "d56aa1eff2fe1741f6b9e7665cf07c264bb2398778f69d2191b20178c6183ca0",
            BindingKind::IsolatedExtractionOutput,
        ),
        (
            "src/mock.rs",
            42_796,
            "443d06229a59c36d5edc7a887a473a7fc371eecfcfd9a8458062b662594a7d0f",
            BindingKind::IsolatedExtractionOutput,
        ),
        (
            "src/pool/frames.rs",
            20_307,
            "8b122ffaa0ae198e4ec9dd68999ac0fbe8232cf5998b6a76d041cbfc4d76372c",
            BindingKind::IsolatedExtractionOutput,
        ),
        (
            "src/pool/mod.rs",
            64_153,
            "213697747590988533f829e5c6846b3106f9ae293ce0570cbf4d432dabaf1d10",
            BindingKind::IsolatedExtractionOutput,
        ),
        (
            "benches/plans/sira_native_hint.md",
            3_907,
            "337ecf1aafdbded58c38cca5efdf62056e139e2521744d305b43b962ebd62ecf",
            BindingKind::InstalledPlanSnapshot,
        ),
    ]
}

fn assert_hunks(hunks: &[ExtractedHunk]) {
    assert_eq!(hunks.len(), 17, "the carrier contains 17 classified hunks");
    for (path, expected_count) in [
        ("src/lib.rs", 2usize),
        ("src/mock.rs", 5),
        ("src/pool/frames.rs", 4),
        ("src/pool/mod.rs", 6),
    ] {
        let matching = hunks.iter().filter(|hunk| hunk.path == path).count();
        assert_eq!(matching, expected_count, "wrong hunk count for {path}");
    }
    for hunk in hunks {
        assert!(
            hunk.hunk.starts_with("@@ "),
            "invalid hunk marker for {}",
            hunk.path
        );
        assert!(
            !hunk.classification.is_empty(),
            "unclassified hunk for {}",
            hunk.path
        );
    }
}

fn assert_forbidden_scan_records(scans: &[ForbiddenSymbolScan]) {
    assert_eq!(
        scans.len(),
        FORBIDDEN_SYMBOLS.len(),
        "wrong forbidden scan count"
    );
    for (scan, expected) in scans.iter().zip(FORBIDDEN_SYMBOLS) {
        assert_eq!(
            scan.symbol, expected,
            "forbidden scan order or symbol changed"
        );
        assert!(
            scan.matches.is_empty(),
            "manifest records matches for {expected}"
        );
    }
}

fn extract_clean_base(repository: &Path, destination: &Path) {
    let revision = format!("{CLEAN_BASE}^{{commit}}");
    let resolved = command_output(
        Command::new("git")
            .current_dir(repository)
            .args(["rev-parse", "--verify", &revision]),
        "resolve the clean extraction base",
    );
    assert_eq!(String::from_utf8_lossy(&resolved.stdout).trim(), CLEAN_BASE);
    for path in [
        "src/lib.rs",
        "src/mock.rs",
        "src/pool/frames.rs",
        "src/pool/mod.rs",
    ] {
        extract_clean_base_file(repository, destination, path);
    }
}

fn extract_clean_base_file(repository: &Path, destination: &Path, path: &str) {
    let object = format!("{CLEAN_BASE}:{path}");
    let output = command_output(
        Command::new("git")
            .current_dir(repository)
            .args(["show", &object]),
        "read a clean-base source file",
    );
    assert!(
        !output.stdout.is_empty(),
        "clean-base {path} must not be empty"
    );
    assert!(
        output.stdout.len() as u64 <= FILE_BYTES_MAX,
        "clean-base {path} exceeds its bound"
    );
    let destination = destination.join(path);
    let parent = destination
        .parent()
        .unwrap_or_else(|| panic!("clean-base file has no parent"));
    fs::create_dir_all(parent)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", parent.display()));
    fs::write(&destination, output.stdout)
        .unwrap_or_else(|error| panic!("failed to write {}: {error}", destination.display()));
}

fn apply_carrier(extraction: &Path, carrier: &Path) {
    for check in [true, false] {
        let mut command = Command::new("git");
        command
            .current_dir(extraction)
            .args(["apply", "--whitespace=nowarn"]);
        if check {
            command.arg("--check");
        }
        command.arg(carrier);
        let purpose = if check {
            "check the R7 carrier"
        } else {
            "apply the R7 carrier"
        };
        let _output = command_output(&mut command, purpose);
    }
}

fn install_plan_snapshot(scope: &Path, extraction: &Path, binding: &Binding) {
    let source = scope.join(&binding.path);
    let destination = extraction.join("benches/plans/sira_native_hint.md");
    let parent = destination
        .parent()
        .unwrap_or_else(|| panic!("installed plan has no parent"));
    fs::create_dir_all(parent)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", parent.display()));
    fs::copy(&source, &destination).unwrap_or_else(|error| {
        panic!(
            "failed to install {} at {}: {error}",
            source.display(),
            destination.display()
        )
    });
}

fn assert_bound_file(path: impl AsRef<Path>, binding: &Binding) {
    let path = path.as_ref();
    let bytes = read_bounded(path, FILE_BYTES_MAX);
    assert_eq!(
        bytes.len() as u64,
        binding.bytes,
        "{} byte length differs",
        path.display()
    );
    assert_eq!(
        sha256_file(path),
        binding.sha256,
        "{} SHA-256 differs",
        path.display()
    );
}

fn sha256_file(path: &Path) -> String {
    let output = command_output(
        Command::new("sha256sum").arg(path),
        "hash a manifest-bound artifact",
    );
    let stdout = String::from_utf8(output.stdout)
        .unwrap_or_else(|error| panic!("sha256sum output was not UTF-8: {error}"));
    let fields: Vec<&str> = stdout.split_whitespace().collect();
    assert_eq!(fields.len(), 2, "sha256sum must return one digest and path");
    assert_eq!(fields[0].len(), 64, "sha256sum digest has the wrong length");
    fields[0].to_owned()
}

fn assert_forbidden_symbols_absent(path: &Path) {
    let bytes = read_bounded(path, FILE_BYTES_MAX);
    for forbidden in FORBIDDEN_SYMBOLS {
        assert!(
            !contains_bytes(&bytes, forbidden.as_bytes()),
            "reconstructed {} contains forbidden R8 symbol {forbidden}",
            path.display()
        );
    }
}

fn read_bounded(path: &Path, bytes_max: u64) -> Vec<u8> {
    let metadata = fs::metadata(path)
        .unwrap_or_else(|error| panic!("failed to stat {}: {error}", path.display()));
    assert!(
        metadata.is_file(),
        "{} must be a regular file",
        path.display()
    );
    assert!(
        metadata.len() <= bytes_max,
        "{} exceeds its byte bound",
        path.display()
    );
    let bytes =
        fs::read(path).unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    assert!(
        bytes.len() as u64 <= bytes_max,
        "{} grew beyond its byte bound",
        path.display()
    );
    bytes
}

fn command_output(command: &mut Command, purpose: &str) -> Output {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to {purpose}: {error}"));
    assert_command_success(purpose, &output);
    output
}

fn assert_command_success(purpose: &str, output: &Output) {
    assert!(
        output.status.success(),
        "failed to {purpose}: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct TemporaryDirectory {
    path: Option<PathBuf>,
}

impl TemporaryDirectory {
    fn create() -> Self {
        let root = std::env::temp_dir();
        for attempt in 0..TEMPORARY_DIRECTORY_ATTEMPTS {
            let path = root.join(format!("dios-drp002-{}-{attempt}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self { path: Some(path) },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("failed to create {}: {error}", path.display()),
            }
        }
        panic!("failed to create a unique DRP002 directory within the fixed attempt bound");
    }

    fn path(&self) -> &Path {
        self.path
            .as_deref()
            .unwrap_or_else(|| panic!("temporary directory was cleaned"))
    }

    fn cleanup(mut self) {
        let path = self
            .path
            .take()
            .unwrap_or_else(|| panic!("temporary directory was cleaned"));
        fs::remove_dir_all(&path)
            .unwrap_or_else(|error| panic!("failed to clean {}: {error}", path.display()));
        assert!(!path.exists(), "temporary extraction was not removed");
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _cleanup_result = fs::remove_dir_all(path);
        }
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    assert!(!needle.is_empty(), "byte scanner needle must not be empty");
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
