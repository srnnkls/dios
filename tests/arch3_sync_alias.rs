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

fn bypasses_the_alias(line: &str) -> bool {
    let code = line.split("//").next().unwrap_or_default();
    let dense: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if dense.contains("std::sync::atomic") || dense.contains("std::sync::Mutex") {
        return true;
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
        for (offset, line) in source.lines().enumerate() {
            if bypasses_the_alias(line) {
                offenders.push(format!("{name}:{}: {}", offset + 1, line.trim()));
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

    assert!(!bypasses_the_alias(
        "// never reach for std::sync::atomic here"
    ));
    assert!(!bypasses_the_alias(
        "/// See `std::sync::Mutex` for the shipping type."
    ));
    assert!(!bypasses_the_alias("use std::sync::Arc;"));
    assert!(!bypasses_the_alias(
        "use crate::sync::{AtomicU64, Ordering};"
    ));
}
