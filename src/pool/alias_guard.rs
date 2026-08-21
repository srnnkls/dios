//! ARCH-3 regression guard: proof-bearing pool concurrency primitives must route
//! through [`crate::sync`] so loom can see them. Clippy's `disallowed_types` cannot
//! express this (the alias re-exports the very same `std` types), so this scans the
//! `src/pool` sources for a direct `std` bypass and fails on any not in the
//! documented diagnostics-only allowlist ([`crate::sync`] lists them).

use std::fs;
use std::path::Path;

/// `(file, marker)` — a bypass line in `file` is permitted only if it names
/// `marker`, tying each carve-out to the exact diagnostics-only field.
const ALLOWLIST: &[(&str, &str)] = &[
    ("clock.rs", "reference_stores"),
    ("loom_model.rs", "held_frame"),
    ("mod.rs", "control_acquisitions"),
];

/// This file grabs the bypass patterns as literals, so it would flag itself.
const SELF: &str = "alias_guard.rs";

#[test]
fn pool_concurrency_primitives_route_through_the_sync_alias() {
    let pool_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pool");
    let mut offenders = Vec::new();

    for entry in fs::read_dir(&pool_dir).expect("read src/pool") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("utf-8 file name")
            .to_owned();
        if name == SELF {
            continue;
        }
        let source = fs::read_to_string(&path).expect("read pool source");
        for (offset, line) in source.lines().enumerate() {
            let bypasses = line.contains("std::sync::atomic") || line.contains("std::sync::Mutex");
            if !bypasses {
                continue;
            }
            let exempt = ALLOWLIST
                .iter()
                .any(|&(file, marker)| name == file && line.contains(marker));
            if !exempt {
                offenders.push(format!("{name}:{}: {}", offset + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "pool concurrency primitives must use crate::sync, not std directly, or loom \
         cannot see them (ARCH-3); undocumented bypasses:\n{}",
        offenders.join("\n")
    );
}
