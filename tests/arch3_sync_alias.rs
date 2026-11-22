//! ARCH-3 regression guard: proof-bearing pool concurrency primitives must route
//! through `crate::sync` so loom can see them. Clippy's `disallowed_types` cannot
//! express this — under `cfg(not(loom))` the alias re-exports the very same `std`
//! types — so this scans the `src/pool` sources for a direct `std` bypass.
//!
//! `src/pool/diagnostics.rs` owns every permitted bypass. A counter that no proof
//! reads belongs there as a `DiagnosticCounter`, not behind a per-line carve-out.

use std::fs;
use std::path::{Path, PathBuf};

const OWNER: &str = "diagnostics.rs";

fn pool_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read a pool source directory") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            pool_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn comment_stripped_items(source: &str) -> Vec<(usize, String)> {
    let code = source
        .lines()
        .map(|line| line.split("//").next().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    let mut line = 1;
    let mut items = Vec::new();
    for item in code.split(';') {
        let leading = &item[..item.len() - item.trim_start().len()];
        items.push((line + leading.matches('\n').count(), item.to_owned()));
        line += item.matches('\n').count();
    }
    items
}

fn bypasses_the_alias(item: &str) -> bool {
    let code = item.split("//").next().unwrap_or_default();
    let dense: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if dense.contains("std::sync::atomic")
        || dense.contains("std::sync::Mutex")
        || dense.contains("std::cell::UnsafeCell")
    {
        return true;
    }
    if let Some((_, after)) = dense.split_once("std::cell::{") {
        let group = after.split('}').next().unwrap_or_default();
        if group.contains("UnsafeCell") {
            return true;
        }
    }
    let Some((_, after)) = dense.split_once("std::sync::{") else {
        return false;
    };
    let group = after.split('}').next().unwrap_or_default();
    group.contains("atomic") || group.contains("Mutex")
}

#[test]
fn pool_concurrency_primitives_route_through_the_sync_alias() {
    let pool_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pool");
    let mut sources = Vec::new();
    pool_sources(&pool_dir, &mut sources);
    assert!(
        sources.len() > 1,
        "the pool source walk found nothing to check"
    );

    let mut offenders = Vec::new();
    for path in sources {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("utf-8 file name")
            .to_owned();
        if name == OWNER {
            continue;
        }
        let source = fs::read_to_string(&path).expect("read pool source");
        for (line, item) in comment_stripped_items(&source) {
            if bypasses_the_alias(&item) {
                offenders.push(format!("{name}:{line}: {}", item.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "pool concurrency primitives must use crate::sync, not std directly, or loom \
         cannot see them (ARCH-3). Move an observation no proof reads into \
         src/pool/{OWNER} as a DiagnosticCounter:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_guard_catches_a_brace_group_bypass_and_spares_prose() {
    assert!(bypasses_the_alias("use std::sync::atomic::AtomicU64;"));
    assert!(bypasses_the_alias("use std::sync::{atomic, Mutex};"));
    assert!(bypasses_the_alias(
        "    let x: std::sync::atomic::AtomicU64;"
    ));
    assert!(bypasses_the_alias("use std::sync::atomic as a;"));
    assert!(bypasses_the_alias("use std::cell::UnsafeCell;"));
    assert!(bypasses_the_alias("use std::cell::{Cell, UnsafeCell};"));
    assert!(bypasses_the_alias(
        "    cells: MappedSlice<std::cell::UnsafeCell<MaybeUninit<PageId>>>,"
    ));

    assert!(!bypasses_the_alias(
        "// never reach for std::sync::atomic here"
    ));
    assert!(!bypasses_the_alias(
        "/// See `std::sync::Mutex` for the shipping type."
    ));
    assert!(!bypasses_the_alias("use std::sync::Arc;"));
    assert!(!bypasses_the_alias("use std::cell::Cell;"));
    assert!(!bypasses_the_alias("    visits: std::cell::Cell<u64>,"));
    assert!(!bypasses_the_alias("use crate::sync::UnsafeCell;"));
    assert!(!bypasses_the_alias(
        "use crate::sync::{AtomicU64, Ordering};"
    ));
}

#[test]
fn the_guard_catches_a_rustfmt_wrapped_use_group() {
    let source = "use std::cell::{\n    Cell,\n    UnsafeCell,\n};\n\
                  use std::sync::{\n    Arc,\n    Mutex, // shared\n};\n\
                  use std::sync::{\n    Arc,\n    OnceLock,\n};\n";
    let offending: Vec<usize> = comment_stripped_items(source)
        .into_iter()
        .filter(|(_, item)| bypasses_the_alias(item))
        .map(|(line, _)| line)
        .collect();
    assert_eq!(offending, vec![1, 5]);
}

const VOLATILE_OWNER: &str = "src/pool/read_copy.rs";
const SEAM_HOME: &str = "src/pool/frames.rs";
const SEAM: &str = "optimistic_payload_ptr";

fn dense(item: &str) -> String {
    item.chars().filter(|c| !c.is_whitespace()).collect()
}

fn defines_the_seam(item: &str) -> bool {
    dense(item).contains(&format!("fn{SEAM}("))
}

fn volatile_confinement_offenders(sources: &[(String, String)]) -> Vec<String> {
    let mut offenders = Vec::new();
    for (path, source) in sources {
        if path == VOLATILE_OWNER {
            continue;
        }
        for (line, item) in comment_stripped_items(source) {
            let code = dense(&item);
            if code.contains("read_volatile") {
                offenders.push(format!("{path}:{line}: volatile read: {}", item.trim()));
            }
            if code.contains(SEAM) && !(path == SEAM_HOME && defines_the_seam(&item)) {
                offenders.push(format!("{path}:{line}: seam use: {}", item.trim()));
            }
        }
    }
    offenders
}

fn crate_sources() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut paths = Vec::new();
    pool_sources(&root.join("src"), &mut paths);
    paths
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(root)
                .expect("a walked source lies under the manifest directory")
                .components()
                .map(|part| part.as_os_str().to_str().expect("utf-8 path"))
                .collect::<Vec<_>>()
                .join("/");
            let source = fs::read_to_string(&path).expect("read a crate source");
            (relative, source)
        })
        .collect()
}

fn fixture(path: &str, source: &str) -> Vec<(String, String)> {
    vec![(path.to_owned(), source.to_owned())]
}

#[test]
fn volatile_payload_reads_and_the_optimistic_seam_stay_in_read_copy() {
    let volatile_read = "let word = unsafe { std::ptr::read_volatile(payload.add(index)) };";
    let method_volatile_read = "let word = unsafe { payload.add(index).read_volatile() };";
    let seam_use = "let payload = frames.optimistic_payload_ptr(frame);";
    let seam_definition = "pub(crate) fn optimistic_payload_ptr(&self, frame: ReadFrameIdx) -> *const u64 {\n    self.base.wrapping_add(frame.get() as usize)\n}";
    for path in ["src/pool/miss.rs", "src/mock.rs"] {
        for source in [volatile_read, method_volatile_read] {
            assert_eq!(
                volatile_confinement_offenders(&fixture(path, source)).len(),
                1,
                "a volatile read in {path} is rejected: {source}"
            );
        }
        assert_eq!(
            volatile_confinement_offenders(&fixture(path, seam_use)).len(),
            1,
            "a seam call in {path} is rejected"
        );
    }
    assert_eq!(
        volatile_confinement_offenders(&fixture(SEAM_HOME, seam_use)).len(),
        1,
        "frames.rs holds the seam's definition, not a call of it"
    );
    assert_eq!(
        volatile_confinement_offenders(&fixture("src/mock.rs", seam_definition)).len(),
        1,
        "the seam is defined only in frames.rs"
    );
    assert!(
        volatile_confinement_offenders(&fixture(SEAM_HOME, seam_definition)).is_empty(),
        "the seam's one definition in frames.rs is accepted"
    );
    for source in [volatile_read, method_volatile_read, seam_use] {
        assert!(
            volatile_confinement_offenders(&fixture(VOLATILE_OWNER, source)).is_empty(),
            "read_copy.rs owns the volatile reads and the seam calls: {source}"
        );
    }
    assert!(
        volatile_confinement_offenders(&fixture(
            "src/mock.rs",
            "// never std::ptr::read_volatile or optimistic_payload_ptr here\n/// See `optimistic_payload_ptr`.\nfn fill() {}"
        ))
        .is_empty(),
        "prose naming the volatile read or the seam is not code"
    );

    let sources = crate_sources();
    assert!(
        sources.iter().any(|(path, _)| path == "src/mock.rs")
            && sources.iter().any(|(path, _)| path == "src/driver.rs"),
        "the walk covers sources outside src/pool"
    );
    let offenders = volatile_confinement_offenders(&sources);
    assert!(
        offenders.is_empty(),
        "volatile payload reads and the optimistic seam live only in {VOLATILE_OWNER} (TR-6, INV-9):\n{}",
        offenders.join("\n")
    );
    let owner_code: String = sources
        .iter()
        .filter(|(path, _)| path == VOLATILE_OWNER)
        .flat_map(|(_, source)| comment_stripped_items(source))
        .map(|(_, item)| dense(&item))
        .collect();
    assert!(
        owner_code.contains("read_volatile") && owner_code.contains(SEAM),
        "{VOLATILE_OWNER} copies through volatile word reads behind the {SEAM} seam"
    );
    let definitions = sources
        .iter()
        .filter(|(path, _)| path == SEAM_HOME)
        .flat_map(|(_, source)| comment_stripped_items(source))
        .filter(|(_, item)| defines_the_seam(item))
        .count();
    assert_eq!(definitions, 1, "{SEAM_HOME} defines the {SEAM} seam once");
}
