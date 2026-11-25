use std::fs;
use std::path::Path;

const PLAN_PATH: &str = "benches/plans/array_translation.md";
const EVIDENCE_README_PATH: &str = "benches/evidence/array_translation/README.md";
const SCOPE_DIRECTORY: &str = "scopes/done/array-translation";
const WARM_READ_COPY_OUT_SCOPE_PATH: &str = "scopes/draft/warm-read-copy-out/scope.md";

fn read_lowercase(relative_path: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative_path);
    fs::read_to_string(path).unwrap_or_default().to_lowercase()
}

fn scope_markdown_lowercase() -> String {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join(SCOPE_DIRECTORY);
    let mut paths: Vec<_> = fs::read_dir(directory)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    paths
        .iter()
        .map(|path| fs::read_to_string(path).unwrap_or_default().to_lowercase())
        .collect::<Vec<_>>()
        .join("\n")
}

fn sections(document: &str) -> Vec<(String, String)> {
    let mut sections = Vec::new();
    let mut heading = String::new();
    let mut body = String::new();
    for line in document.lines() {
        if line.trim_start().starts_with('#') {
            sections.push((std::mem::take(&mut heading), std::mem::take(&mut body)));
            line.clone_into(&mut heading);
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    sections.push((heading, body));
    sections
}

fn section_with_heading(document: &str, heading_terms: &[&str]) -> Option<String> {
    sections(document)
        .into_iter()
        .find(|(heading, _)| heading_terms.iter().all(|term| heading.contains(term)))
        .map(|(_, body)| body)
}

fn has_angle_placeholder(line: &str) -> bool {
    line.match_indices('<').any(|(start, _)| {
        let rest = &line[start + 1..];
        rest.find('>').is_some_and(|end| {
            end > 0
                && rest[..end]
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "_-".contains(character))
        })
    })
}

fn gate_capture_row_is_pinned(document: &str, gate: &str) -> bool {
    document.lines().any(|line| {
        line.contains(gate)
            && line.contains("manifest")
            && line.contains("mise run")
            && !has_angle_placeholder(line)
    })
}

fn names_an_owner(clause: &str) -> bool {
    let names_person = ["sören", "soren"].iter().any(|name| clause.contains(name));
    let names_task = (1..=8).any(|index| {
        let task = format!("at{index}");
        clause.match_indices(&task).any(|(start, _)| {
            let before = clause[..start].chars().next_back();
            let after = clause[start + task.len()..].chars().next();
            before.is_none_or(|character| !character.is_ascii_alphanumeric())
                && after.is_none_or(|character| !character.is_ascii_alphanumeric())
        })
    });
    names_person || names_task
}

#[test]
fn every_gate_pins_a_manifest_and_capture_command_and_the_evidence_records_the_hugepage_witness() {
    let plan = read_lowercase(PLAN_PATH);
    let evidence = read_lowercase(EVIDENCE_README_PATH);
    let capture_contract = format!("{plan}\n{evidence}");

    let unpinned_gates: Vec<String> = (1..=7)
        .map(|index| format!("at-g{index}"))
        .filter(|gate| !gate_capture_row_is_pinned(&capture_contract, gate))
        .collect();
    assert!(
        unpinned_gates.is_empty(),
        "gates without a line naming their manifest and a concrete `mise run` capture command: {unpinned_gates:?}"
    );

    assert!(
        evidence.contains("anonhugepages") && evidence.contains("transparent_hugepage"),
        "{EVIDENCE_README_PATH} must pin the AT-G3 hugepage witness (THP mode and the arena's AnonHugePages)"
    );
}

#[test]
fn scope_records_the_writer_inventory_ordering_audit_and_miri_model_limits() {
    let scope = scope_markdown_lowercase();

    let inventory = section_with_heading(&scope, &["writer", "inventory"]).unwrap_or_default();
    for required in ["src/pool/frames.rs", "src/mock.rs", "release"] {
        assert!(
            inventory.contains(required),
            "writer inventory section must name `{required}` (in-process read-frame payload writers and their FR-11 fence)"
        );
    }

    let ordering = section_with_heading(&scope, &["order", "audit"]).unwrap_or_default();
    for required in [
        "x86-64", "aarch64", "compiler", "stamp", "entry", "volatile",
    ] {
        assert!(
            ordering.contains(required),
            "ordering audit section must cover `{required}` for the stamp, entry and volatile loads"
        );
    }

    let miri = section_with_heading(&scope, &["miri"]).unwrap_or_default();
    for required in ["limit", "volatile", "kernel"] {
        assert!(
            miri.contains(required),
            "Miri model limits section must state `{required}`"
        );
    }
}

#[test]
fn drp_prerequisite_names_its_owner_and_warm_read_copy_out_points_to_its_successor() {
    let records = format!(
        "{}\n{}\n{}",
        scope_markdown_lowercase(),
        read_lowercase(PLAN_PATH),
        read_lowercase(EVIDENCE_README_PATH)
    );
    let prerequisite_named_with_owner = records.lines().any(|line| {
        let frozen_runner = line.contains("frozen-runner") || line.contains("frozen runner");
        let fresh_base = line.contains("fresh base") || line.contains("fresh-base");
        frozen_runner && fresh_base && names_an_owner(line)
    });
    assert!(
        prerequisite_named_with_owner,
        "the DRP frozen-runner revision and fresh-base prerequisite must be named with its owner"
    );

    let warm_read_copy_out = read_lowercase(WARM_READ_COPY_OUT_SCOPE_PATH);
    assert!(
        warm_read_copy_out
            .lines()
            .any(|line| line.contains("successor") && line.contains("array-translation")),
        "{WARM_READ_COPY_OUT_SCOPE_PATH} must point to array-translation as its successor"
    );
}
