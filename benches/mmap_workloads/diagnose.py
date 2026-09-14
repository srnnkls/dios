# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Join validated mmap traces, observer pairs and CPU profiles to primary costs."""

from __future__ import annotations

import argparse
from collections import Counter, defaultdict
import hashlib
import json
import math
from pathlib import Path
from statistics import fmean

from analyze import analyze, read_comparison
from stacks import workload_boundary
from validate import validate_row


def matched_manifest(root: Path, primary: dict, mode: str) -> dict:
    manifest = json.loads((root / "manifest.json").read_text())
    if manifest["status"] != "complete" or manifest["mode"] != mode:
        raise ValueError(f"incomplete {mode} campaign")
    for key in ("executable_sha256", "fixture"):
        if manifest[key] != primary[key]:
            raise ValueError(f"diagnostic {key} differs from primary")
    return manifest


def percentile(values: list[int], fraction: float) -> int:
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * fraction) - 1)]


def spans(events: list[dict]) -> dict:
    duration = max(event["end_ns"] for event in events) - min(
        event["start_ns"] for event in events
    )
    latencies = [event["end_ns"] - event["start_ns"] for event in events]
    return {
        "operations": len(events),
        "duration_ns": duration,
        "ns_per_read": duration / len(events),
        "span_ns_p50": percentile(latencies, 0.50),
        "span_ns_p99": percentile(latencies, 0.99),
        "span_ns_p999": percentile(latencies, 0.999),
        "span_ns_max": max(latencies),
        "span_occupancy_mean": sum(latencies) / duration,
    }


def trace_summary(row: dict) -> dict:
    blocks = defaultdict(list)
    events = []
    count = len(row["workers"])
    for worker in row["workers"]:
        for event in worker["events"]:
            operation = event["operation"] * count + worker["worker"]
            blocks[operation // 16_384].append(event)
            events.append(event)
    result = spans(events)
    result["blocks"] = [
        {"operation_start": block * 16_384, **spans(entries)}
        for block, entries in sorted(blocks.items())
    ]
    result["logical_reads_observed"] = [
        {
            "worker": worker["worker"],
            **flight_summary(worker["flights"], row["elapsed_ns"]),
        }
        for worker in row["workers"]
        if worker.get("flights")
    ]
    return result


def flight_summary(events: list[dict], elapsed_ns: int) -> dict:
    result = {"observations": len(events)}
    for field in ("reads", "speculative"):
        integral = 0
        for index, event in enumerate(events):
            end = events[index + 1]["at_ns"] if index + 1 < len(events) else elapsed_ns
            integral += event[field] * (end - event["at_ns"])
        result[f"{field}_mean_observed"] = integral / elapsed_ns
        result[f"{field}_maximum_observed"] = max(event[field] for event in events)
    return result


def diagnostic_replays(trace: Path, observer: Path, primary: dict) -> dict:
    traces = matched_manifest(trace, primary, "trace")
    observers = matched_manifest(observer, primary, "observe")
    results = {}
    for comparison in traces["comparisons"]:
        pairs = read_comparison(trace, comparison, retain_events=True)
        if len(pairs) != 1:
            raise ValueError("trace campaign must contain one matched replay")
        for row in pairs[0]:
            results[f"{row['lane']}-{row['arm']}"] = trace_summary(row)
    for comparison in observers["comparisons"]:
        read_comparison(observer, comparison)
        if comparison["arms"][0] != comparison["arms"][1]:
            raise ValueError("observer comparison changes the workload arm")
        results[comparison["name"]]["trace_on_over_off"] = comparison["summary"]
    if any("trace_on_over_off" not in row for row in results.values()):
        raise ValueError("trace lacks an observer control")
    return results


def category(frames: list[str]) -> str:
    leaf = frames[-1]
    if "unknown" in leaf.lower() or leaf.startswith("0x"):
        return "unresolved_cpu"
    for frame in reversed(frames):
        if "probe::Probe::drain" in frame or "probe::Probe::poll" in frame:
            return "submission_poll_and_completion"
        if "probe::Probe::stage" in frame:
            return "sqe_construction"
        if any(name in frame for name in ("::prefetch", "Prefetch::", "Pattern::")):
            return "prefetch_and_prediction"
        if "catalog::consume" in frame:
            return "page_consumption"
        if any(
            name in frame for name in ("::poll", "::drain_completions", "::reap_ring")
        ):
            return "poll_and_completion"
        if any(name in frame for name in ("::get", "::pin_owned", "::claim_frame")):
            return "lookup_and_admission"
        if "::ready" in frame:
            return "ready_and_guard"
        if "FrameGuard" in frame or "::release_pin" in frame:
            return "guard_release"
        if "engine::read_mmap" in frame:
            return "mapped_access_and_harness"
    return "caller_loop_or_other"


def cpu_profile(directory: Path, primary: dict, cpu_ns_per_read: float) -> dict:
    metadata = json.loads((directory / "metadata.json").read_text())
    if metadata["executable_sha256"] != primary["executable_sha256"]:
        raise ValueError("profile executable differs from primary")
    if metadata["status"] != "complete" or metadata["lost_events"]:
        raise ValueError("profile failed sampling qualification")
    counts, excluded = Counter(), 0
    for line in (directory / "profile.folded").read_text().splitlines():
        stack, weight = line.rsplit(" ", 1)
        frames, count = stack.split(";"), int(weight)
        if count <= 0:
            raise ValueError("non-positive profile sample count")
        if (
            workload_boundary(
                frames, metadata.get("workload_boundaries", ("engine::timed_workload",))
            )
            is not None
        ):
            counts[category(frames)] += count
        else:
            excluded += count
    total = sum(counts.values())
    if total != metadata["timed_samples"] or total < 100:
        raise ValueError("profile workload sample count differs")
    return {
        "metadata": metadata,
        "excluded_setup_samples": excluded,
        "folded_sha256": hashlib.sha256(
            (directory / "profile.folded").read_bytes()
        ).hexdigest(),
        "perturbation": profile_perturbation(
            directory, metadata, primary, cpu_ns_per_read
        ),
        "categories": {
            name: {
                "samples": count,
                "fraction": count / total,
                "estimated_cpu_ns_per_read": cpu_ns_per_read * count / total,
            }
            for name, count in counts.most_common()
        },
    }


def profile_perturbation(
    directory: Path, metadata: dict, primary: dict, primary_cpu: float
) -> dict | None:
    expected = metadata.get("sample_sha256")
    if expected is None:
        return None
    if len(expected) != metadata["repetitions"]:
        raise ValueError("profile replay count differs")
    values = []
    for name, digest in expected.items():
        raw = (directory / "samples" / name).read_bytes()
        if hashlib.sha256(raw).hexdigest() != digest:
            raise ValueError("profile sample hash differs")
        row = json.loads(raw)
        validate_row(row)
        if row["lane"] != metadata["lane"] or row["arm"] != metadata["arm"]:
            raise ValueError("profile sample workload differs")
        if row["fixture"] != primary["fixture"]:
            raise ValueError("profile sample fixture differs")
        values.append(
            sum(worker["cpu_ns"] for worker in row["workers"]) / row["operations"]
        )
    return {
        "replays": len(values),
        "cpu_ns_per_read": fmean(values),
        "profile_over_primary_cpu": fmean(values) / primary_cpu,
        "interpretation": "separate-replay ratio; not an interleaved profiler-overhead estimate",
    }


def profile_budgets(root: Path, primary: dict, model: dict) -> dict:
    rows = {
        f"{row['name']}-{row[arm]['arm']}": row[arm]
        for row in model["results"]
        for arm in ("base", "candidate")
    }
    result = {}
    for directory in sorted(root.iterdir()):
        if not directory.is_dir():
            continue
        if directory.name not in rows:
            raise ValueError(f"unexpected profile {directory}")
        cpu = rows[directory.name]["cpu_ns_per_read"]
        result[directory.name] = cpu_profile(directory, primary, cpu)
    return result


def markdown(model: dict) -> str:
    lines = [
        "Diagnostic replays; primary timings exclude per-request clocks.",
        "",
        "Spans run from an accepted acquisition attempt through consumption,",
        "excluding earlier Busy retries and final guard release. They are",
        "closed-loop observations, not device latency or arrival-time SLOs.",
        "",
        "| Workload / arm | Trace on/off | 95% upper | Span p50 ns | p99 ns | Mean span occupancy |",
        "|---|---:|---:|---:|---:|---:|",
    ]
    for name, row in model["replays"].items():
        overhead = row["trace_on_over_off"]
        lines.append(
            f"| {name} | {overhead['ratio_geomean']:.3f} | {overhead['ci95_upper']:.3f} | "
            f"{row['span_ns_p50']} | {row['span_ns_p99']} | {row['span_occupancy_mean']:.2f} |"
        )
    lines += [
        "",
        "Logical occupancy integrates snapshots at admission/poll boundaries.",
        "It includes accepted reads awaiting observation, not device queue depth.",
        "Shared-pool snapshots from different workers are listed separately.",
        "",
        "| Workload / arm / worker | Observations | Mean reads | Max reads | Mean credits | Max credits |",
        "|---|---:|---:|---:|---:|---:|",
    ]
    for name, row in model["replays"].items():
        for observed in row.get("logical_reads_observed", []):
            lines.append(
                f"| {name}/{observed['worker']} | {observed['observations']} | "
                f"{observed['reads_mean_observed']:.2f} | {observed['reads_maximum_observed']} | "
                f"{observed['speculative_mean_observed']:.2f} | {observed['speculative_maximum_observed']} |"
            )
    lines += [
        "",
        "CPU budgets scale unprofiled thread CPU by replay sample fractions.",
        "They are attribution estimates, not independently identified cost coefficients.",
        "The prefetch category includes control checks when prediction is disabled.",
        "",
        "| Workload / arm | Category | Samples | CPU fraction | Estimated CPU ns/read |",
        "|---|---|---:|---:|---:|",
    ]
    for name, profile in model["profiles"].items():
        for label, row in profile["categories"].items():
            lines.append(
                f"| {name} | {label} | {row['samples']} | {row['fraction']:.1%} | "
                f"{row['estimated_cpu_ns_per_read']:.1f} |"
            )
    lines += [
        "",
        "Separate-replay CPU perturbation (profile/primary; drift and sampling bias remain):",
        "",
    ]
    for name, profile in model["profiles"].items():
        if profile.get("perturbation") is not None:
            lines.append(
                f"- {name}: {profile['perturbation']['profile_over_primary_cpu']:.3f}x"
            )
    return "\n".join(lines) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("primary", "trace", "observer", "output"):
        parser.add_argument(name, type=Path)
    parser.add_argument("--profiles", type=Path)
    options = parser.parse_args()
    primary = json.loads((options.primary / "manifest.json").read_text())
    base = analyze(options.primary)
    replays = diagnostic_replays(options.trace, options.observer, primary)
    profiles = (
        profile_budgets(options.profiles, primary, base) if options.profiles else {}
    )
    result = {
        "schema": 1,
        "primary_manifest_sha256": base["source_manifest_sha256"],
        "replays": replays,
        "profiles": profiles,
    }
    options.output.mkdir(parents=True, exist_ok=False)
    (options.output / "diagnostics.json").write_text(
        json.dumps(result, indent=2) + "\n"
    )
    (options.output / "diagnostics.md").write_text(markdown(result))


if __name__ == "__main__":
    main()
