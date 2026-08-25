use std::fs;

const PLAN_PATH: &str = "benches/plans/pinned-frame-retention.md";

fn normalized(text: &str) -> String {
    text.to_ascii_lowercase()
        .replace('≤', "<=")
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '<' | '>' | '=' | '%' | '.')
            {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn sections(document: &str) -> Vec<String> {
    let mut sections = Vec::new();
    let mut section = String::new();
    for line in document.lines() {
        let trimmed = line.trim_start();
        let heading = trimmed.starts_with('#') && trimmed.contains("# ");
        if heading && !section.trim().is_empty() {
            sections.push(std::mem::take(&mut section));
        }
        section.push_str(line);
        section.push('\n');
    }
    if !section.trim().is_empty() {
        sections.push(section);
    }
    sections
}

fn clauses(section: &str) -> Vec<String> {
    let mut joined = String::new();
    for line in section.lines().map(str::trim) {
        let boundary = line.is_empty()
            || line.starts_with('#')
            || line.starts_with("- ")
            || line.starts_with("* ")
            || line.starts_with("| ");
        if boundary && !joined.is_empty() {
            joined.push('\n');
        } else if !joined.is_empty() {
            joined.push(' ');
        }
        joined.push_str(line);
    }
    joined
        .lines()
        .flat_map(|line| line.split(". "))
        .flat_map(|line| line.split("; "))
        .map(normalized)
        .filter(|line| !line.is_empty())
        .collect()
}

fn contains_any(text: &str, alternatives: &[&str]) -> bool {
    alternatives.iter().any(|term| text.contains(term))
}

fn contains_groups(text: &str, groups: &[&[&str]]) -> bool {
    groups.iter().all(|group| contains_any(text, group))
}

fn has_clause(section: &str, groups: &[&[&str]], forbidden: &[&str]) -> bool {
    clauses(section).iter().any(|clause| {
        contains_groups(clause, groups) && !forbidden.iter().any(|term| clause.contains(term))
    })
}

fn find_section(document: &str, anchors: &[&[&str]]) -> Option<String> {
    sections(document).into_iter().find(|section| {
        let section = normalized(section);
        contains_groups(&section, anchors)
    })
}

fn has_placeholder(text: &str) -> bool {
    contains_any(
        text,
        &[
            "tbd",
            "todo",
            "placeholder",
            "to be determined",
            "to be decided",
            "<bound>",
            "<command>",
            "<workload>",
        ],
    )
}

fn first_integer(text: &str) -> Option<(u32, usize, usize, bool)> {
    let bytes = text.as_bytes();
    let start = bytes.iter().position(u8::is_ascii_digit)?;
    let end = bytes[start..]
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .map_or(bytes.len(), |length| start + length);
    let mut after = end;
    while bytes.get(after) == Some(&b' ') {
        after += 1;
    }
    Some((
        text[start..end].parse().unwrap_or_default(),
        start,
        end,
        bytes.get(after) == Some(&b'%'),
    ))
}

fn last_integer(text: &str) -> Option<(u32, usize, usize, bool)> {
    let mut remaining = text;
    let mut offset = 0;
    let mut last = None;
    while let Some((value, start, end, percent)) = first_integer(remaining) {
        last = Some((value, offset + start, offset + end, percent));
        offset += end;
        remaining = &text[offset..];
    }
    last
}

fn repetition_label(line: &str) -> Option<(usize, usize)> {
    ["repetitions", "reps"].into_iter().find_map(|label| {
        line.match_indices(label).find_map(|(start, _)| {
            let end = start + label.len();
            let before = line[..start].bytes().next_back();
            let after = line[end..].bytes().next();
            (before.is_none_or(|byte| !byte.is_ascii_alphanumeric())
                && after.is_none_or(|byte| !byte.is_ascii_alphanumeric()))
            .then_some((start, end))
        })
    })
}

fn has_repetition_floor(document: &str) -> bool {
    document.lines().any(|line| {
        let line = line.to_ascii_lowercase();
        let Some((label_start, label_end)) = repetition_label(&line) else {
            return false;
        };
        if has_placeholder(&line)
            || contains_any(
                &line,
                &[
                    "fewer than",
                    "less than",
                    "at most",
                    "no more than",
                    "not enough",
                    "< 30",
                ],
            )
        {
            return false;
        }

        let after = &line[label_end..];
        if let Some((count, start, end, percent)) = first_integer(after) {
            let lead = normalized(&after[..start]);
            let unit = normalized(&after[end..])
                .split_whitespace()
                .next()
                .is_some_and(|word| {
                    matches!(
                        word,
                        "ms" | "ns" | "us" | "seconds" | "milliseconds" | "bytes"
                    )
                });
            let relation = lead.split_whitespace().all(|word| {
                matches!(
                    word,
                    "n" | "count" | "minimum" | "min" | "at" | "least" | "exactly" | "is" | "are"
                )
            });
            return start <= 32 && relation && !unit && !percent && count >= 30;
        }

        let before = &line[..label_start];
        let Some((count, _, end, percent)) = last_integer(before) else {
            return false;
        };
        let gap = normalized(&before[end..]);
        let relation = gap
            .split_whitespace()
            .all(|word| matches!(word, "paired" | "independent" | "timed"));
        relation && !percent && count >= 30
    })
}

fn numeric_bound(clause: &str, anchor: &str) -> Option<(f64, bool)> {
    let clause = normalized(clause);
    let anchor_at = clause.find(anchor)?;
    let tail = &clause[anchor_at + anchor.len()..];
    let operator_at = tail.find("<=")?;
    if operator_at > 120 {
        return None;
    }
    let value = &tail[operator_at + 2..];
    let bytes = value.as_bytes();
    let start = bytes.iter().position(u8::is_ascii_digit)?;
    if start > 8 {
        return None;
    }
    let mut end = start;
    while bytes
        .get(end)
        .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'.')
    {
        end += 1;
    }
    let mut after = end;
    while bytes.get(after) == Some(&b' ') {
        after += 1;
    }
    Some((
        value[start..end].parse().ok()?,
        bytes.get(after) == Some(&b'%'),
    ))
}

fn has_ci_gate(section: &str) -> bool {
    clauses(section).iter().any(|clause| {
        contains_any(clause, &["one sided 95%", "one sided 95 percent"])
            && numeric_bound(clause, "upper bound").is_some()
    })
}

fn candidate_over_baseline(section: &str) -> bool {
    contains_any(
        &section.to_ascii_lowercase(),
        &[
            "candidate / baseline",
            "candidate/baseline",
            "candidate-to-baseline",
            "candidate to baseline",
            "candidate over baseline",
            "candidate divided by baseline",
        ],
    )
}

fn arm_is_gated(section: &str, anchors: &[&[&str]]) -> bool {
    let text = normalized(section);
    contains_groups(&text, anchors)
        && !has_placeholder(&text)
        && !contains_any(
            &text,
            &[
                "not gated",
                "ungated",
                "non gating",
                "exploratory",
                "record only",
                "does not gate",
            ],
        )
        && text.contains("workload")
        && text.contains("baseline")
        && text.contains("candidate")
        && candidate_over_baseline(section)
        && contains_any(
            &text,
            &[
                "lower is better",
                "smaller is better",
                "closer to 1 is better",
                "closer to one is better",
            ],
        )
        && has_ci_gate(section)
        && contains_any(&text, &["compare command", "mise run gate"])
        && text.contains("escalation lever")
}

fn has_gated_arm(document: &str, anchors: &[&[&str]]) -> bool {
    find_section(document, anchors).is_some_and(|section| arm_is_gated(&section, anchors))
}

fn has_shared_protocol(document: &str) -> bool {
    sections(document).iter().any(|section| {
        let text = normalized(section);
        has_repetition_floor(section)
            && candidate_over_baseline(section)
            && contains_any(&text, &["lower is better", "smaller is better"])
            && has_ci_gate(section)
            && text.contains("compare command")
            && text.contains("escalation lever")
            && !has_placeholder(&text)
    })
}

fn has_numeric_refusal_gate(document: &str) -> bool {
    let Some(section) = find_section(
        document,
        &[
            &["same frame"],
            &["promotion", "promote"],
            &["refusal rate"],
            &["refused contention"],
        ],
    ) else {
        return false;
    };
    let text = normalized(&section);
    !has_placeholder(&text)
        && !contains_any(
            &text,
            &["not gated", "ungated", "exploratory", "record only"],
        )
        && text.contains("workload")
        && text.contains("escalation lever")
        && clauses(&section).iter().any(|clause| {
            contains_groups(
                clause,
                &[
                    &["one sided 95%", "one sided 95 percent"],
                    &["upper bound"],
                    &["refusal rate"],
                    &["refused contention"],
                ],
            ) && numeric_bound(clause, "upper bound").is_some_and(|(value, percent)| {
                if percent { value < 100.0 } else { value < 1.0 }
            })
        })
}

fn has_exact_r8_executable_identity(document: &str) -> bool {
    let shared_protocol_override = find_section(document, &[&["shared"], &["protocol"]])
        .is_some_and(|section| {
            has_clause(
                &section,
                &[
                    &["exact r8"],
                    &["override"],
                    &["may explicitly override", "explicitly permits"],
                ],
                &["forbids", "denies", "does not permit"],
            )
        });
    if !shared_protocol_override {
        return false;
    }
    let Some(section) = find_section(
        document,
        &[&["exact r8"], &["t8b"], &["executable", "runnable pair"]],
    ) else {
        return false;
    };
    has_clause(
        &section,
        &[
            &["exact r8"],
            &["t8b"],
            &["runnable pair"],
            &["one clean"],
            &["post am4"],
            &["integration commit"],
        ],
        &["t7", "retention commit"],
    ) && has_clause(
        &section,
        &[
            &["baseline and candidate", "both arms"],
            &["one executable", "same executable", "shared executable"],
        ],
        &["different executable", "separate executable"],
    ) && has_clause(
        &section,
        &[
            &["t7"],
            &["retention commit", "retention code"],
            &["recorded separately", "separate provenance"],
            &["only"],
            &["base", "provenance"],
        ],
        &["runnable pair uses t7", "executable commit"],
    )
}

fn has_same_frame_anchor_isolation_zero_refusals(section: &str) -> bool {
    let permitted = [
        "permits refused budget",
        "permits refused ceiling",
        "permits refused retiring",
        "allows refused budget",
        "allows refused ceiling",
        "allows refused retiring",
    ];
    has_clause(
        section,
        &[
            &[
                "both control and contended arms",
                "control and contended arms",
                "both arms",
            ],
            &["require", "must"],
            &["refused budget"],
            &["refused ceiling"],
            &["refused retiring"],
            &["remain zero", "must be zero"],
        ],
        &permitted,
    ) && !contains_any(&normalized(section), &permitted)
}

fn has_same_frame_anchor_isolation(document: &str) -> bool {
    let Some(section) = find_section(
        document,
        &[
            &["same frame"],
            &["promotion", "promote"],
            &["control", "contended"],
        ],
    ) else {
        return false;
    };
    has_clause(
        &section,
        &[
            &[
                "both control and contended arms",
                "control and contended arms",
            ],
            &["one setup anchor", "single setup anchor"],
            &["retainedframe", "retained frame"],
            &["same frame", "the frame"],
            &["outside"],
            &["sampled attempts", "attempted promotions"],
            &["timing", "timed"],
        ],
        &["control arm has no anchor", "inside timing"],
    ) && has_clause(
        &section,
        &[
            &["sampled promotions", "sampled attempts"],
            &[
                "count > 0",
                "count greater than 0",
                "nonzero count",
                "non zero count",
            ],
            &["cannot", "do not", "never"],
            &["first reservation", "first reservation path"],
            &["budget refusal", "budget refusal path"],
        ],
        &["count 0", "count zero", "may use"],
    ) && has_clause(
        &section,
        &[
            &["anchor"],
            &["excluded", "not counted"],
            &["attempted promotion", "sampled attempt"],
            &["timing count", "timed count"],
            &["dropped after sampling", "drop after sampling"],
        ],
        &["included in", "kept after sampling"],
    ) && has_same_frame_anchor_isolation_zero_refusals(&section)
}

fn has_timed_wake_fault_validity(document: &str) -> bool {
    let Some(section) = find_section(
        document,
        &[&["wake if parked", "wake arm"], &["cpu 0"], &["cpu 1"]],
    ) else {
        return false;
    };
    has_clause(
        &section,
        &[
            &["both arms", "both sides"],
            &["cpu 0"],
            &["bench thread"],
            &["cpu 1"],
            &["poller thread"],
            &["separate per thread", "separate thread", "per thread"],
            &["rusage thread"],
            &["ru minflt"],
            &["ru majflt"],
            &["delta", "deltas"],
        ],
        &["bench thread only", "process wide"],
    ) && has_clause(
        &section,
        &[
            &["registered pair"],
            &["every timed thread", "all timed threads"],
            &["must", "require"],
            &["zero minor"],
            &["zero major"],
        ],
        &["faults permitted", "faults allowed", "bench thread only"],
    )
}

fn has_r8_access_section_arm_equivalence(section: &str) -> bool {
    has_clause(
        section,
        &[
            &["candidate and baseline", "both arms"],
            &["identical", "same"],
            &[
                "descriptor order",
                "access order",
                "descriptor access order",
            ],
        ],
        &["different order", "reverse order", "unrelated order"],
    ) && has_clause(
        section,
        &[
            &["candidate and baseline", "both arms"],
            &["identical", "same"],
            &["useful byte"],
            &["work", "consume", "consumption"],
        ],
        &["different work", "different bytes", "extra work"],
    )
}

fn has_r8_access_section(document: &str) -> bool {
    let Some(section) = find_section(
        document,
        &[&["r8"], &["8 035", "8035"], &["retained session"]],
    ) else {
        return false;
    };
    has_clause(
        &section,
        &[
            &["8 035", "8035"],
            &["distinct"],
            &["promoted pages", "pages promoted", "promotes"],
        ],
        &["one page", "same page", "repeated page"],
    ) && has_clause(
        &section,
        &[
            &["candidate"],
            &["retainedframe", "retained frame"],
            &["index", "indexed", "indexes"],
            &["byte", "bytes"],
        ],
        &["does not index", "never indexes", "baseline"],
    ) && has_clause(
        &section,
        &[&["baseline"], &["transient"], &["guarded", "guard"]],
        &["does not use", "without transient", "candidate"],
    ) && has_clause(
        &section,
        &[
            &["promotion", "promote"],
            &["outside"],
            &["timed", "timing"],
        ],
        &["not outside", "inside the timed", "inside timing"],
    ) && has_clause(
        &section,
        &[
            &["fixed capacity"],
            &["integer"],
            &["descriptor"],
            &["composition", "compose", "precomposed"],
            &["outside"],
            &["timed", "timing"],
        ],
        &["not outside", "inside the timed", "inside timing"],
    ) && has_r8_access_section_arm_equivalence(&section)
        && has_clause(
            &section,
            &[&["shipping backend"], &["nix"]],
            &["does not run", "mock only", "instead of nix"],
        )
}

fn has_r8_posture_section(document: &str) -> bool {
    let Some(section) = find_section(
        document,
        &[
            &["exact r8", "8 035", "8035"],
            &["unregistered"],
            &["arena modernization am4"],
        ],
    ) else {
        return false;
    };
    has_clause(
        &section,
        &[
            &["exact r8", "8 035", "8035"],
            &["unregistered posture", "unregistered arena"],
        ],
        &["not unregistered", "instead of unregistered"],
    ) && has_clause(
        &section,
        &[
            &["exact r8", "8 035", "8035"],
            &["blocked on", "blocked until"],
            &["arena modernization am4"],
        ],
        &["not blocked", "already unblocked"],
    ) && has_clause(
        &section,
        &[
            &["baseline"],
            &["stock"],
            &["8 mib"],
            &["unprivileged"],
            &["memlock"],
        ],
        &["raised limit baseline", "requires a raised"],
    ) && has_clause(
        &section,
        &[
            &["registered"],
            &["raised limit"],
            &[
                "separately labelled",
                "separately labeled",
                "separate label",
            ],
        ],
        &["substitute", "same label"],
    ) && has_clause(
        &section,
        &[
            &["scaled"],
            &["non equivalent", "not equivalent"],
            &[
                "separately labelled",
                "separately labeled",
                "separate label",
            ],
        ],
        &["equivalent to exact r8", "substitute"],
    )
}

fn has_fault_control_section(document: &str) -> bool {
    let Some(section) = find_section(document, &[&["fault"], &["warm state", "timed region"]])
    else {
        return false;
    };
    has_clause(
        &section,
        &[
            &["both sides", "each side"],
            &["ru minflt"],
            &["ru majflt"],
            &["rusage thread"],
        ],
        &["one side only", "process wide"],
    ) && has_clause(
        &section,
        &[
            &["nonzero major", "non zero major"],
            &["invalidates", "invalid"],
            &["every pair", "the pair"],
        ],
        &["does not invalidate", "remains valid"],
    ) && has_clause(
        &section,
        &[
            &["registered"],
            &["exact r8"],
            &["zero minor"],
            &["require", "must"],
        ],
        &["minor faults permitted", "minor faults allowed"],
    ) && has_clause(
        &section,
        &[
            &["both arms", "both sides"],
            &["identical", "same"],
            &["segment layout"],
        ],
        &["different segment", "may differ"],
    )
}

fn has_sira_obligation_section(document: &str) -> bool {
    let Some(section) = find_section(
        document,
        &[
            &["sira side"],
            &["retained vs locator", "retained versus locator"],
            &["companion", "obligation"],
        ],
    ) else {
        return false;
    };
    let text = normalized(&section);
    !contains_any(
        &text,
        &["dios authors the arm", "dios owns the arm", "not sira side"],
    ) && clauses(&section).iter().any(|clause| {
        contains_groups(
            clause,
            &[
                &["retained vs locator", "retained versus locator"],
                &["tie gate"],
            ],
        ) && numeric_bound(clause, "tie gate")
            .is_some_and(|(value, percent)| !percent && value > 0.0 && value <= 1.02)
    }) && has_clause(
        &section,
        &[
            &["locator"],
            &["madv populate read"],
            &["prefault", "populate"],
        ],
        &["does not prefault", "optional prefault"],
    ) && has_clause(
        &section,
        &[
            &["both arms", "both sides"],
            &["identical", "same"],
            &["blockverification"],
            &["page size state"],
        ],
        &["different blockverification", "page size may differ"],
    )
}

fn has_evidence_and_state_sections(document: &str) -> bool {
    let evidence = sections(document).iter().any(|section| {
        let text = normalized(section);
        contains_groups(
            &text,
            &[
                &["resources r8 resident set.md"],
                &["formal fail"],
                &["preserve", "preserved", "retains", "cites"],
            ],
        ) && !contains_any(&text, &["discard formal fail", "supersede formal fail"])
    });
    let state = sections(document).iter().any(|section| {
        has_clause(
            section,
            &[
                &["separate"],
                &["held count word"],
                &["orthogonal"],
                &["drp010"],
                &["packed frame state generation"],
            ],
            &[
                "not orthogonal",
                "packed into frame state",
                "becomes a frame state bit",
            ],
        )
    });
    evidence && state
}

fn optional_arm_is_exploratory(document: &str, arm: &[&str]) -> bool {
    let text = normalized(document);
    if !contains_any(&text, arm) {
        return true;
    }
    find_section(document, &[arm]).is_some_and(|section| {
        let section = normalized(&section);
        contains_any(&section, &["exploratory", "non gating", "not gated"])
            && !contains_any(&section, &["closes the gate", "substitutes for exact r8"])
    })
}

#[test]
fn repetition_floor_requires_an_affirmative_bound_count() {
    assert!(has_repetition_floor("| Reps | 30 paired repetitions |"));
    assert!(has_repetition_floor("Repetitions: 40"));
    assert!(has_repetition_floor("At least 31 independent repetitions"));
    assert!(!has_repetition_floor("30 ms; reps: 1"));
    assert!(!has_repetition_floor("Reps: 1; latency 30 ms"));
    assert!(!has_repetition_floor("Reps: 30 ms"));
    assert!(!has_repetition_floor("Fewer than 30 repetitions"));
}

#[test]
fn same_frame_refusal_gate_binds_a_non_vacuous_rate_threshold() {
    let valid = "## Same-frame promotion\nPinned workload: 8 threads promote one frame. The one-sided 95% CI upper bound of the refused_contention refusal rate must be <= 0.5%. Compare command: mise run refusal-gate. Escalation lever: increase the retry bound.";
    let unrelated = "## Same-frame promotion\nPinned workload: 8 threads promote one frame. The one-sided 95% CI upper bound of the refused_contention refusal rate is recorded. Latency upper bound <= 20 nanoseconds. Escalation lever: increase the retry bound.";
    let vacuous = "## Same-frame promotion\nPinned workload: 8 threads promote one frame. The one-sided 95% CI upper bound of the refused_contention refusal rate must be <= 100%. Escalation lever: increase the retry bound.";

    assert!(has_numeric_refusal_gate(valid));
    assert!(!has_numeric_refusal_gate(unrelated));
    assert!(!has_numeric_refusal_gate(vacuous));
}

#[test]
fn exact_r8_pair_uses_post_am4_executable_identity_and_separate_t7_provenance() {
    let valid = "## Shared protocol\nThe shared protocol explicitly permits the exact-R8 identity override.\n\n## Exact R8 T8b executable identity\nThe exact-R8 T8b runnable pair uses one clean post-AM4 integration commit `abc1234`. Baseline and candidate use one executable, `pinned-frame-retention-t8b`. The T7 retention commit is recorded separately only as the retention-code base/provenance.";
    let t7_runnable = valid.replace(
        "one clean post-AM4 integration commit `abc1234`",
        "the T7 retention commit `def5678`",
    );
    let split_executable = valid.replace(
        "Baseline and candidate use one executable, `pinned-frame-retention-t8b`",
        "Baseline uses executable `r8-baseline`; candidate uses executable `r8-candidate`",
    );
    let t7_not_separate = valid.replace(
        "recorded separately only as the retention-code base/provenance",
        "also defines the runnable pair",
    );
    let shared_protocol_forbids_override = valid.replace(
        "explicitly permits the exact-R8 identity override",
        "forbids the exact-R8 identity override",
    );

    assert!(has_exact_r8_executable_identity(valid));
    assert!(!has_exact_r8_executable_identity(&t7_runnable));
    assert!(!has_exact_r8_executable_identity(&split_executable));
    assert!(!has_exact_r8_executable_identity(&t7_not_separate));
    assert!(!has_exact_r8_executable_identity(
        &shared_protocol_forbids_override
    ));
}

#[test]
fn same_frame_sampling_keeps_an_uncounted_anchor_in_both_arms() {
    let valid = "## Same-frame promotion sampling\nBoth control and contended arms keep one setup anchor RetainedFrame on the same frame outside sampled attempts and timing. Sampled promotions start from count > 0 and cannot use the first-reservation or budget-refusal path. Both control and contended arms require refused_budget, refused_ceiling, and refused_retiring to remain zero. The anchor is excluded from attempted-promotion and timing counts and dropped after sampling.";
    let control_unanchored = valid.replace(
        "Both control and contended arms keep one setup anchor",
        "Only the contended arm keeps one setup anchor",
    );
    let zero_start = valid.replace(
        "start from count > 0 and cannot use the first-reservation or budget-refusal path",
        "start from count zero with budget one and may use the first-reservation path",
    );
    let counted_anchor = valid.replace(
        "excluded from attempted-promotion and timing counts and dropped after sampling",
        "included in attempted-promotion and timing counts and kept after sampling",
    );
    let control_permits_refused_budget = valid.replace(
        "Both control and contended arms require refused_budget, refused_ceiling, and refused_retiring to remain zero",
        "The contended arm requires refused_budget, refused_ceiling, and refused_retiring to remain zero, but the control arm permits refused_budget to be one",
    );

    assert!(has_same_frame_anchor_isolation(valid));
    assert!(!has_same_frame_anchor_isolation(&control_unanchored));
    assert!(!has_same_frame_anchor_isolation(&zero_start));
    assert!(!has_same_frame_anchor_isolation(&counted_anchor));
    assert!(!has_same_frame_anchor_isolation(
        &control_permits_refused_budget
    ));
}

#[test]
fn timed_wake_pair_requires_per_thread_fault_deltas_and_zero_faults() {
    let valid = "## Promote-release wake-if-parked arm\nIn both arms, the CPU-0 bench thread and CPU-1 poller thread each record separate per-thread RUSAGE_THREAD ru_minflt and ru_majflt deltas. For the registered pair, every timed thread must have zero minor and zero major faults.";
    let bench_thread_only = "## Promote-release wake-if-parked arm\nIn both arms, the CPU-0 bench thread records separate per-thread RUSAGE_THREAD ru_minflt and ru_majflt deltas; the CPU-1 poller thread is not sampled. For the registered pair, the CPU-0 bench thread must have zero minor and zero major faults.";
    let one_arm_only = valid.replace("In both arms", "In the candidate arm");
    let minor_only = valid.replace(
        "must have zero minor and zero major faults",
        "must have zero minor faults; major faults are allowed",
    );

    assert!(has_timed_wake_fault_validity(valid));
    assert!(!has_timed_wake_fault_validity(bench_thread_only));
    assert!(!has_timed_wake_fault_validity(&one_arm_only));
    assert!(!has_timed_wake_fault_validity(&minor_only));
}

#[test]
fn local_relations_reject_ungated_and_reversed_access_contracts() {
    let gated = "## Transient guard A/B\nPinned workload: fixed warm hits. Baseline: current guard. Candidate: retained build. Metric: candidate / baseline wall time; lower is better. One-sided 95% CI upper bound <= 1.01. Compare command: mise run gate samples.csv 1.01. Escalation lever: inspect guard traffic.";
    let ungated = gated.replace(
        "Pinned workload:",
        "This arm is not gated and remains exploratory. Pinned workload:",
    );
    assert!(arm_is_gated(gated, &[&["transient"], &["guard"]]));
    assert!(!arm_is_gated(&ungated, &[&["transient"], &["guard"]]));

    let valid_r8 = "## Exact R8 retained session\nSetup promotes 8,035 distinct pages. The 8,035-page candidate directly indexes RetainedFrame bytes. The baseline uses transient guarded access. Promotion remains outside the timed region. Fixed-capacity integer descriptor composition remains outside the timed region. Candidate and baseline use identical descriptor/access order. Candidate and baseline perform identical useful-byte work. The shipping backend runs on nix.";
    let reversed_r8 = valid_r8.replace(
        "candidate directly indexes RetainedFrame bytes. The baseline uses transient guarded access",
        "candidate uses transient guarded access. The baseline directly indexes RetainedFrame bytes",
    );
    let negated_r8 = valid_r8.replace("directly indexes", "does not index");
    let wrapped_r8 = valid_r8.replace("directly indexes", "directly\nindexes");
    let malformed_r8 = "## Exact R8 retained session\nSetup promotes one page 8,035 times. The 8,035-page candidate directly indexes RetainedFrame bytes. The baseline uses transient guarded access. Promotion remains outside the timed region. Fixed-capacity floating-point descriptor composition remains outside the timed region. Candidate and baseline use different descriptor/access order. Candidate and baseline perform different useful-byte work. The shipping backend runs on nix.";
    assert!(has_r8_access_section(valid_r8));
    assert!(has_r8_access_section(&wrapped_r8));
    assert!(!has_r8_access_section(&reversed_r8));
    assert!(!has_r8_access_section(&negated_r8));
    assert!(!has_r8_access_section(malformed_r8));
}

#[test]
fn t1_plan_freezes_the_pinned_frame_retention_gate_contract() {
    let plan = fs::read_to_string(PLAN_PATH).unwrap_or_default();
    let mut missing = Vec::new();

    if !has_shared_protocol(&plan) {
        missing.push("affirmative >=30-rep metric/ratio/CI/compare/escalation protocol");
    }
    if !has_exact_r8_executable_identity(&plan) {
        missing.push("exact-R8 T8b post-AM4 shared executable with separate T7 provenance");
    }
    if !has_same_frame_anchor_isolation(&plan) {
        missing.push("same-frame control/contended uncounted setup-anchor isolation");
    }
    if !has_timed_wake_fault_validity(&plan) {
        missing.push("both timed wake-arm threads have separate zero-fault deltas");
    }
    let gated_arms: [&[&[&str]]; 4] = [
        &[&["transient"], &["guard"]],
        &[&["nonzero", "non zero"], &["poll boundary"]],
        &[&["zero budget"], &["bypass"], &["parity"]],
        &[
            &["promote release", "promotion release"],
            &["wake if parked"],
        ],
    ];
    if gated_arms
        .iter()
        .any(|anchors| !has_gated_arm(&plan, anchors))
    {
        missing.push("four section-local transient/poll/bypass/promote-release A/B gates");
    }
    if !has_numeric_refusal_gate(&plan) {
        missing.push("non-vacuous same-frame refused_contention CI rate gate");
    }
    if !has_r8_access_section(&plan) {
        missing.push("R8 distinct-page integer-descriptor/order/work relations on shipping nix");
    }
    if !has_r8_posture_section(&plan) {
        missing.push("exact-R8 Unregistered/AM4/8-MiB posture and separate alternatives");
    }
    if !has_fault_control_section(&plan) {
        missing.push("per-pair thread-fault and identical-layout validity controls");
    }
    if !has_sira_obligation_section(&plan) {
        missing.push("Sira-side retained-vs-locator <=1.02 companion obligation");
    }
    if !has_evidence_and_state_sections(&plan) {
        missing.push("R8 formal FAIL evidence and DRP010-orthogonal HELD/count word");
    }
    if !optional_arm_is_exploratory(&plan, &["thp", "madv hugepage"])
        || !optional_arm_is_exploratory(&plan, &["sparse registration"])
    {
        missing.push("optional THP and sparse-registration arms remain exploratory");
    }

    assert!(
        missing.is_empty(),
        "pinned-frame-retention T1 bench plan is incomplete:\n- {}",
        missing.join("\n- ")
    );
}
