use std::fs;

const CLEAN_BASE: &str = "1004a2e6fcae0bcc9552dc3211c2416e388a250d";
const PLAN_PATH: &str = "benches/plans/dios_r1_r7_read_performance.md";
const DIOS_DEPENDENCIES_PATH: &str = "scopes/active/dios-v1/dependencies.yaml";

fn require_all(document: &str, requirement: &str, needles: &[&str], missing: &mut Vec<String>) {
    if !needles.iter().all(|needle| document.contains(needle)) {
        missing.push(requirement.to_owned());
    }
}

fn section<'document>(
    document: &'document str,
    heading: &str,
    next: Option<&str>,
) -> &'document str {
    let marker = format!("## {heading}");
    let Some(start) = document.find(&marker) else {
        return "";
    };
    let tail = &document[start..];
    let end = next
        .and_then(|name| tail.find(&format!("## {name}")))
        .unwrap_or(tail.len());
    &tail[..end]
}

fn require_gate(
    plan: &str,
    gate: &str,
    next: Option<&str>,
    thresholds: &[&str],
    missing: &mut Vec<String>,
) {
    let gate_section = section(plan, gate, next);
    let mut required = thresholds.to_vec();
    required.extend(["mise run gate", "Escalation lever"]);
    require_all(
        gate_section,
        &format!("{gate} section-local thresholds, compare command, and escalation lever"),
        &required,
        missing,
    );
}

fn require_gate_artifact(
    plan: &str,
    gate: &str,
    next: Option<&str>,
    stem: &str,
    threshold: &str,
    missing: &mut Vec<String>,
) {
    let gate_section = section(plan, gate, next);
    let process_path = format!("target/bench-samples/{stem}_process.csv");
    let paired_path = format!("target/bench-samples/{stem}_paired.csv");
    let gate_command = format!("mise run gate {paired_path} {threshold}");
    require_all(
        gate_section,
        &format!("{gate} distinct rich and paired artifacts for {stem}"),
        &[&process_path, &paired_path, &gate_command],
        missing,
    );
}

fn require_artifact_protocol(plan: &str, missing: &mut Vec<String>) {
    let protocol = section(plan, "Run identity and shared protocol", Some("DRP-G1"));
    require_all(
        protocol,
        "deterministic validated rich-to-paired conversion and provenance binding",
        &[
            "one row per process",
            "base_ns,candidate_ns",
            "mise run prepare-gate-pairs",
            "deterministic",
            "validate",
            "provenance manifest",
            "SHA-256",
        ],
        missing,
    );
}

fn require_prerequisite_validators(plan: &str, missing: &mut Vec<String>) {
    let gate_one = section(plan, "DRP-G1", Some("DRP-G2"));
    require_all(
        gate_one,
        "DRP-G1 executable probe-quality validation before timing authorization",
        &[
            "mise run validate-drp-g1-probes",
            "must exit successfully before",
            "drp_g1_warm_get_paired.csv",
        ],
        missing,
    );

    let gate_two = section(plan, "DRP-G2", Some("DRP-G3"));
    require_all(
        gate_two,
        "DRP-G2 executable zero-allocation validation for both lanes before timing authorization",
        &[
            "mise run validate-drp-g2-zero-alloc target/bench-samples/drp_g2_warm_ordinary_process.csv",
            "mise run validate-drp-g2-zero-alloc target/bench-samples/drp_g2_cycling_reuse_process.csv",
            "must exit successfully before",
        ],
        missing,
    );
}

fn has_distinct_sha(text: &str) -> bool {
    text.as_bytes().windows(40).any(|window| {
        window.iter().all(u8::is_ascii_hexdigit)
            && !window.eq_ignore_ascii_case(CLEAN_BASE.as_bytes())
    })
}

fn has_exact_candidate_identity(plan: &str) -> bool {
    plan.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        let named = lower.contains("candidate identity");
        let placeholder = [
            "<",
            "tbd",
            "placeholder",
            "unpinned",
            "to be filled",
            "once landed",
        ]
        .iter()
        .any(|word| lower.contains(word));
        let exact_capture = lower.contains("git rev-parse head")
            && (lower.contains("sha256sum") || lower.contains("sha-256"));
        named && !placeholder && (has_distinct_sha(line) || exact_capture)
    })
}

fn compact(document: &str) -> String {
    document
        .chars()
        .filter(|character| !character.is_ascii_whitespace() && *character != '`')
        .collect()
}

fn require_holdout(plan: &str, missing: &mut Vec<String>) {
    let holdout = compact(plan);
    require_all(
        &holdout,
        "the exact independent holdout dimensions and formulas",
        &[
            "0x71c3_5a09_d4e2_b687",
            "0xd903_4f61_28bc_7a55",
            "2,048,65,536,and262,144slots",
            "3,31,and127",
            "iin0..slots/2",
            "f=i%files",
            "3+5*f",
            "0x8000_0001+17*f",
            "11+257*(i/files)",
            "[0,1,...,count-1]",
            "upper=count-1",
            "state^=state<<13",
            "state^=state>>7",
            "state^=state<<17",
            "state%(upper+1)",
            "byte-identicalinsertionorder",
            "ceil(0.99*count)-1",
        ],
        missing,
    );
}

fn normalized_paragraphs(document: &str) -> impl Iterator<Item = String> + '_ {
    document.split("\n\n").map(|paragraph| {
        paragraph
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase()
    })
}

fn has_positive_relation(document: &str, subject: &str, relation: &str, object: &str) -> bool {
    normalized_paragraphs(document).any(|paragraph| {
        let Some(subject_at) = paragraph.find(subject) else {
            return false;
        };
        let after_subject = &paragraph[subject_at + subject.len()..];
        let Some(relation_at) = after_subject.find(relation) else {
            return false;
        };
        let relation_prefix = &after_subject[..relation_at];
        let after_relation = &after_subject[relation_at + relation.len()..];
        let negated = [" not ", " never ", " no longer ", "doesn't "]
            .iter()
            .any(|negation| relation_prefix.contains(negation));
        !negated && after_relation.contains(object)
    })
}

fn require_ownership_handoffs(missing: &mut Vec<String>) {
    let atomic_scope =
        fs::read_to_string("scopes/draft/read-protocol-atomic/scope.md").unwrap_or_default();
    if !has_positive_relation(
        &atomic_scope,
        "dios-r1-r7-read-performance",
        "supersedes",
        "read-protocol-atomic",
    ) || !has_positive_relation(
        &atomic_scope,
        "read-protocol-atomic",
        "remains",
        "evidence source",
    ) {
        missing.push(
            "positive read-protocol-atomic ownership/evidence handoff in its scope.md".into(),
        );
    }
    if !atomic_scope.contains("status: evidence-only")
        || !atomic_scope.contains("non-actionable")
        || atomic_scope.contains("## Goal")
        || atomic_scope.contains("## Sequencing")
    {
        missing.push("read-protocol-atomic is an evidence-only, non-actionable record".into());
    }

    let dios_tasks = fs::read_to_string("scopes/active/dios-v1/tasks.yaml").unwrap_or_default();
    if !has_positive_relation(
        &dios_tasks,
        "t018",
        "consumes",
        "dios-r1-r7-read-performance",
    ) || !has_positive_relation(
        &dios_tasks,
        "dios-r1-r7-read-performance",
        "owns",
        "primitive implementation",
    ) {
        missing.push("positive T018 primitive-ownership handoff in dios-v1/tasks.yaml".into());
    }

    let dios_scope = fs::read_to_string("scopes/active/dios-v1/scope.md").unwrap_or_default();
    if !has_positive_relation(
        &dios_scope,
        "dio-g10",
        "remains",
        "migration-level consumer gate",
    ) || !has_positive_relation(
        &dios_scope,
        "dio-g10",
        "consumes",
        "dios-r1-r7-read-performance",
    ) {
        missing.push("positive DIO-G10 migration-consumer handoff in dios-v1/scope.md".into());
    }

    require_dios_dependencies(missing);
}

fn yaml_task<'document>(document: &'document str, id: &str) -> &'document str {
    let marker = format!("      - id: {id}");
    let Some(start) = document.find(&marker) else {
        return "";
    };
    let tail = &document[start..];
    let end = tail[marker.len()..]
        .find("\n      - id:")
        .map_or(tail.len(), |offset| marker.len() + offset);
    &tail[..end]
}

fn require_dios_dependencies(missing: &mut Vec<String>) {
    let dependencies = fs::read_to_string(DIOS_DEPENDENCIES_PATH).unwrap_or_default();
    let task = yaml_task(&dependencies, "T018");
    require_all(
        task,
        "dios-v1 dependencies.yaml T018 depends on T017",
        &["depends_on: [T017]"],
        missing,
    );

    let follow_up = dependencies
        .split_once("follow_up_batches:")
        .map_or("", |(_, tail)| tail);
    let scheduled_after = follow_up
        .find("tasks: [T017]")
        .zip(follow_up.find("tasks: [T018]"));
    if !matches!(scheduled_after, Some((t017, t018)) if t017 < t018) {
        missing.push("dios-v1 dependencies.yaml schedules T018 after T017".into());
    }
    require_all(
        &dependencies,
        "dios-v1 dependencies.yaml binds T018 to DRP completion",
        &[
            "scope: dios-r1-r7-read-performance",
            "required_endpoint: DRP010",
        ],
        missing,
    );
}

fn require_timing_artifacts(plan: &str, missing: &mut Vec<String>) {
    let artifacts = [
        ("DRP-G1", Some("DRP-G2"), "drp_g1_warm_get", "0.98"),
        ("DRP-G2", Some("DRP-G3"), "drp_g2_warm_ordinary", "1.01"),
        ("DRP-G2", Some("DRP-G3"), "drp_g2_cycling_reuse", "1.01"),
        ("DRP-G3", Some("DRP-G4"), "drp_g3_hint_materiality", "0.95"),
        ("DRP-G4", None, "drp_g4_ordinary_base_8t", "1.00"),
        ("DRP-G4", None, "drp_g4_ordinary_scaling", "0.50"),
        ("DRP-G4", None, "drp_g4_hint_scaling", "0.50"),
    ];
    for (gate, next, stem, threshold) in artifacts {
        require_gate_artifact(plan, gate, next, stem, threshold, missing);
    }
}

#[test]
fn drp001_freezes_the_product_gates_and_ownership_handoff() {
    let plan = fs::read_to_string(PLAN_PATH).unwrap_or_default();
    let mut missing = Vec::new();

    if !has_exact_candidate_identity(&plan) {
        missing.push("exact non-placeholder candidate source/binary identity contract".into());
    }
    require_gate(
        &plan,
        "DRP-G1",
        Some("DRP-G2"),
        &["1.05", "+1", "<64", "0.98"],
        &mut missing,
    );
    require_gate(
        &plan,
        "DRP-G2",
        Some("DRP-G3"),
        &["1.01", "zero"],
        &mut missing,
    );
    require_gate(&plan, "DRP-G3", Some("DRP-G4"), &["0.95"], &mut missing);
    require_gate(&plan, "DRP-G4", None, &["1.00", "0.50"], &mut missing);
    require_artifact_protocol(&plan, &mut missing);
    require_timing_artifacts(&plan, &mut missing);
    require_prerequisite_validators(&plan, &mut missing);
    require_holdout(&plan, &mut missing);
    require_ownership_handoffs(&mut missing);

    assert!(
        missing.is_empty(),
        "DRP001 plan/ownership contract is incomplete:\n- {}",
        missing.join("\n- ")
    );
}
