# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Combine measured workload budgets, bounded traces, and sampled CPU stacks."""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
from collections import Counter, defaultdict
from dataclasses import dataclass
from pathlib import Path
from statistics import fmean

ROWS_MAX = 150_000
EVENTS_MAX = 4_000_000
SAMPLES_MIN = 100


@dataclass(frozen=True, kw_only=True)
class Run:
    directory: Path
    mode: str
    runner: str
    executable: str
    recipe: str
    posture: tuple[str, str]
    platform: str
    groups: dict[tuple[str, str], tuple[dict[str, str], ...]]


def load_run(directory: Path) -> Run:
    manifest = json.loads((directory / "manifest.json").read_text())
    if (
        manifest["schema"] != "dios-workload-suite-v1"
        or manifest["status"] != "complete"
    ):
        raise ValueError(f"incomplete or unsupported run: {directory}")
    groups: dict[tuple[str, str], list[dict[str, str]]] = defaultdict(list)
    with (directory / "measurements.csv").open(newline="") as source:
        for number, row in enumerate(csv.DictReader(source)):
            if number >= ROWS_MAX:
                raise ValueError("measurement row bound exceeded")
            if int(row["operations"]) <= 0 or int(row["allocations"]) != 0:
                raise ValueError("invalid useful work or timed allocation")
            if int(row["elapsed_ns"]) < int(row["foreground_ns"]):
                raise ValueError("foreground ends after total completion")
            groups[row["lane"], row["arm"]].append(row)
    if not groups:
        raise ValueError("empty run")
    postures = {
        (row["registration"], row["io_mode"])
        for rows in groups.values()
        for row in rows
    }
    if len(postures) != 1:
        raise ValueError("resolved registration/I/O posture changes within run")
    for rows in groups.values():
        if [int(row["pair"]) for row in rows] != list(range(len(rows))):
            raise ValueError("missing, duplicated, or reordered measurement pairs")
    return Run(
        directory=directory,
        mode=manifest["mode"],
        runner=manifest["runner_sha256"],
        executable=manifest["executable_sha256"],
        recipe=manifest["fixture_recipe"],
        posture=postures.pop(),
        platform=manifest["os"],
        groups={key: tuple(rows) for key, rows in groups.items()},
    )


def require_matching(reference: Run, diagnostic: Run) -> None:
    if reference.posture != diagnostic.posture:
        raise ValueError("resolved registration/I/O posture differs between runs")
    if (reference.runner, reference.recipe) != (diagnostic.runner, diagnostic.recipe):
        raise ValueError("runner or fixture identities differ between runs")
    if reference.executable != diagnostic.executable:
        raise ValueError(
            "executable identities differ; collect matched diagnostic replays"
        )


def mean_field(rows: tuple[dict[str, str], ...], field: str) -> float | None:
    values = [int(row[field]) for row in rows if row[field]]
    if len(values) != len(rows):
        return None
    return fmean(values)


def budgets(run: Run) -> dict[str, dict[str, object]]:
    result: dict[str, dict[str, object]] = {}
    fields = (
        "elapsed_ns",
        "foreground_ns",
        "cpu_ns",
        "operations",
        "useful_bytes",
        "processed_bytes",
        "hits",
        "pending",
        "ready_checks",
        "busy",
        "polls",
        "poll_reclaimed",
        "poll_backend_completions",
        "pending_max",
        "writes",
        "barriers",
    )
    for (lane, arm), rows in run.groups.items():
        values = {field: mean_field(rows, field) for field in fields}
        elapsed = values["elapsed_ns"]
        operations = values["operations"]
        assert elapsed is not None and operations is not None
        workers = 4 if (lane, arm) == ("multirun_readers", "candidate") else 1
        cpu = values["cpu_ns"]
        result[f"{lane}/{arm}"] = {
            "means": values,
            "workers": workers,
            "ns_per_operation": elapsed / operations,
            "foreground_ns_per_operation": values["foreground_ns"] / operations,
            "operations_per_second": operations * 1e9 / elapsed,
            "cpu_floor_ns": None if cpu is None else cpu / workers,
            "worker_cpu_capacity_fraction": None
            if cpu is None
            else cpu / (workers * elapsed),
            "drain_tail_ns": elapsed - values["foreground_ns"],
            "observation": "CPU includes busy polling; explicit poll counters omit internal get progress",
        }
    return result


def trace_budgets(run: Run) -> dict[str, dict[str, object]]:
    phases: dict[str, Counter[str]] = defaultdict(Counter)
    counts: dict[str, Counter[str]] = defaultdict(Counter)
    with (run.directory / "events.csv").open(newline="") as source:
        for number, event in enumerate(csv.DictReader(source)):
            if number >= EVENTS_MAX:
                raise ValueError("trace row bound exceeded")
            duration = int(event["end_ns"]) - int(event["start_ns"])
            if duration < 0:
                raise ValueError("negative event duration")
            key = f"{event['lane']}/{event['arm']}"
            phases[key][event["phase"]] += duration
            counts[key][event["phase"]] += 1
    result = {}
    for (lane, arm), rows in run.groups.items():
        key = f"{lane}/{arm}"
        lifetime = sum(int(row["pending_lifetime_ns"]) for row in rows)
        pending = sum(int(row["pending"]) for row in rows)
        foreground = sum(int(row["foreground_ns"]) for row in rows)
        if phases[key]["pending"] != lifetime or counts[key]["pending"] != pending:
            raise ValueError(f"trace/counter lifetime accounting differs for {key}")
        result[key] = {
            "pending_mean_ns": lifetime / pending if pending else None,
            "pending_occupancy_mean": lifetime / foreground,
            "phase_mean_ns": {
                phase: duration / counts[key][phase]
                for phase, duration in phases[key].items()
                if counts[key][phase]
            },
            "phase_counts": dict(counts[key]),
            "boundary": "API observation spans; successful ready checks only; not device service time",
        }
    return result


def categorize(frames: list[str]) -> str:
    for frame in reversed(frames):
        if "fold_bytes" in frame or "consume_bytes" in frame:
            return "decode"
        if "phase_poll" in frame:
            return "poll_and_completion"
        if "dios::pool" in frame and "::get" in frame:
            return "lookup_and_admission"
        if "ready_read" in frame or ("dios::pool" in frame and "::ready" in frame):
            return "ready_and_pin"
        if "consume_guard" in frame:
            return "guard_release_and_harness"
        if "Output::submit" in frame or "fill_page" in frame:
            return "output_control_and_submission"
        if "retained_session" in frame:
            return "retained_session_other"
    return "harness_or_unresolved"


def read_profile(path: Path) -> tuple[Counter[str], int]:
    categories: Counter[str] = Counter()
    excluded = 0
    with path.open() as source:
        for number, line in enumerate(source):
            if number >= ROWS_MAX:
                raise ValueError("folded-stack row bound exceeded")
            stack, count_text = line.rstrip().rsplit(" ", 1)
            count = int(count_text)
            if count <= 0:
                raise ValueError("non-positive sample weight")
            frames = stack.split(";")
            if not any("engine::timed_workload" in frame for frame in frames):
                excluded += count
                continue
            categories[categorize(frames)] += count
    return categories, excluded


def profile_sampling(directory: Path, platform: str) -> dict[str, object]:
    if platform != "linux":
        return {"sample_unit": "samples", "event": "thread_stack_snapshot"}
    metadata = json.loads((directory / "profile-metadata.json").read_text())
    if metadata.get("sample_unit") != "samples" or metadata.get("lost_events") != 0:
        raise ValueError("profile requires actual sample counts and zero lost events")
    event = metadata.get("event")
    if event == "cycles":
        if metadata.get("period") != 100000 or metadata.get("frequency") is not None:
            raise ValueError("cycle profile requires the fixed 100,000-cycle period")
    elif event == "cpu-clock":
        if metadata.get("frequency") != 997 or metadata.get("period") is not None:
            raise ValueError("CPU-clock profile requires the recorded 997 Hz protocol")
    else:
        raise ValueError("unsupported profile event")
    statistics = (directory / "perf-statistics.txt").read_text()
    counts = {}
    for line in statistics.splitlines():
        fields = line.split()
        if len(fields) >= 3 and fields[1] == "events:":
            counts[fields[0]] = int(fields[2])
    if counts.get("LOST", 0) or counts.get("LOST_SAMPLES", 0):
        raise ValueError("raw perf statistics report lost events")
    metadata["throttle_events"] = counts.get("THROTTLE", 0)
    metadata["unthrottle_events"] = counts.get("UNTHROTTLE", 0)
    return metadata


def profile_budgets(reference: Run, root: Path) -> dict[str, dict[str, object]]:
    result = {}
    for lane, arm in reference.groups:
        directory = root / lane / arm
        folded = directory / "profile.folded"
        if not folded.exists():
            continue
        run = load_run(directory / "measurement")
        require_matching(reference, run)
        if run.mode != "profile" or set(run.groups) != {(lane, arm)}:
            raise ValueError(
                "profile measurements do not match their lane/arm directory"
            )
        sampling = profile_sampling(directory, run.platform)
        categories, excluded = read_profile(folded)
        total = categories.total()
        rows = reference.groups[lane, arm]
        cpu = mean_field(rows, "cpu_ns")
        operations = mean_field(rows, "operations")
        profile_cpu = mean_field(run.groups[lane, arm], "cpu_ns")
        known = total >= SAMPLES_MIN and cpu is not None and run.platform == "linux"
        quality = "insufficient_samples"
        if total >= SAMPLES_MIN:
            quality = "advisory_stack_observations"
            if run.platform == "linux":
                quality = "attribution_estimate" if known else "cpu_clock_unavailable"
                if known and sampling["event"] == "cycles":
                    quality = "cycle_share_cpu_estimate"
        estimates = {}
        for category, samples in categories.items():
            cpu_budget = cpu * samples / total if known else None
            estimates[category] = {
                "samples": samples,
                "sample_fraction": samples / total,
                "cpu_ns_per_batch_estimate": cpu_budget,
                "cpu_ns_per_useful_operation_estimate": None
                if cpu_budget is None
                else cpu_budget / operations,
            }
        result[f"{lane}/{arm}"] = {
            "timed_samples": total,
            "excluded_samples": excluded,
            "quality": quality,
            "sample_kind": f"on_cpu_{sampling['event']}"
            if run.platform == "linux"
            else "filtered_thread_stack_snapshots",
            "collector": sampling,
            "cpu_budget_source": "primary_unprofiled_worker_clock",
            "profile_cpu_per_primary_cpu": None
            if cpu is None or profile_cpu is None
            else profile_cpu / cpu,
            "excluded_samples_scope": "folded input after collector filtering",
            "categories": estimates,
            "folded_sha256": hashlib.sha256(folded.read_bytes()).hexdigest(),
            "boundary": "workload-local CPU attribution; categories are not independently identified coefficients",
        }
    return result


def prepare_overhead_pairs(primary: Run, traced: Run, output: Path) -> list[str]:
    if primary.mode != "observe" or traced.mode != "observe":
        raise ValueError("observer overhead requires interleaved observe-mode runs")
    names = []
    if primary.groups.keys() != traced.groups.keys():
        raise ValueError("trace lane set differs from primary run")
    for key, base_rows in primary.groups.items():
        candidate_rows = traced.groups[key]
        if len(base_rows) != len(candidate_rows) or len(base_rows) < 30:
            raise ValueError(
                "observer-overhead comparison requires at least 30 matched pairs"
            )
        name = f"instrumentation_{key[0]}_{key[1]}.csv"
        with (output / name).open("x", newline="") as target:
            writer = csv.writer(target)
            writer.writerow(["base_ns", "candidate_ns"])
            for base, candidate in zip(base_rows, candidate_rows, strict=True):
                if (base["traced"], candidate["traced"]) != ("false", "true"):
                    raise ValueError("observer pairs must compare trace off/on")
                for field in (
                    "pair",
                    "operations",
                    "useful_bytes",
                    "processed_bytes",
                    "checksum",
                    "hits",
                    "pending",
                ):
                    if base[field] != candidate[field]:
                        raise ValueError(f"observer comparison differs on {field}")
                writer.writerow([base["elapsed_ns"], candidate["elapsed_ns"]])
        names.append(name)
    return names


def write_report(output: Path, model: dict[str, object]) -> None:
    (output / "cost-model.json").write_text(json.dumps(model, indent=2) + "\n")
    lines = [
        "# Workload cost evidence",
        "",
        "Primary timings are closed-loop characterization. CPU budgets below include busy polling.",
        "",
        "| Workload/arm | Total ns/read | Foreground ns/read | Worker CPU capacity | Pending max | Busy |",
        "|---|---:|---:|---:|---:|---:|",
    ]
    for key, values in model["primary"].items():
        cpu = values["worker_cpu_capacity_fraction"]
        cpu_text = "unavailable" if cpu is None else f"{cpu:.1%}"
        lines.append(
            f"| {key} | {values['ns_per_operation']:.1f} | {values['foreground_ns_per_operation']:.1f} | {cpu_text} | {values['means']['pending_max']:.1f} | {values['means']['busy']:.1f} |"
        )
    lines += [
        "",
        "## API trace evidence",
        "",
        "These diagnostic lifetimes include instrumentation and readiness-observation delay.",
        "",
        "| Workload/arm | Mean pending µs | Mean unresolved interests |",
        "|---|---:|---:|",
    ]
    for key, values in model["traces"].items():
        pending = values["pending_mean_ns"]
        latency = "n/a" if pending is None else f"{pending / 1000:.2f}"
        lines.append(f"| {key} | {latency} | {values['pending_occupancy_mean']:.2f} |")
    lines += [
        "",
        "## Instrumentation overhead",
        "",
        "Each fixed workload/arm alternates trace off/on for 30 pairs. Ratios use total elapsed time and the shared bootstrap harness.",
        "",
        "| Workload/arm | Trace on/off | One-sided 95% upper |",
        "|---|---:|---:|",
    ]
    for key, ratio in model["observer_overhead"].items():
        lines.append(
            f"| {key} | {ratio['ratio_geomean']:.3f} | {ratio['ci95_upper']:.3f} |"
        )
    lines += [
        "",
        "## CPU attribution",
        "",
        "Budgets apply sampled execution shares to unprofiled worker CPU time. Fixed-cycle shares assume approximately stable effective frequency. Categories are mutually exclusive; unresolved work remains visible. Output control includes no-op checks in read-only lanes.",
        "",
    ]
    for key, profile in model["profiles"].items():
        lines.extend(write_report_profile(key, profile))
    lines += [
        "",
        "## Interpretation limits",
        "",
        "Pending occupancy comes from diagnostic API lifetimes; it is not device queue depth. Trace timings contain observer overhead. Explicit poll progress omits work performed internally by get(). Raw-device service, mutex wait, DRAM traffic, and independent model coefficients remain unmeasured.",
        "",
        "The model is a CPU/event budget and a concurrency accounting model. Predictive coefficients need independent workload perturbations and held-out validation. Observer ratios come from the runner's shared comparison harness; the analyzer implements no gate statistics.",
    ]
    (output / "cost-model.md").write_text("\n".join(lines) + "\n")


def write_report_profile(key: str, profile: dict[str, object]) -> list[str]:
    lines = [
        f"- **{key}**: {profile['timed_samples']} timed samples; {profile['excluded_samples']} setup/other samples excluded; {profile['sample_kind']}; {profile['quality']}.",
    ]
    perturbation = profile["profile_cpu_per_primary_cpu"]
    if perturbation is not None:
        lines.append(
            f"  - Profile/primary worker CPU: {perturbation:.2f}x in separate replays; budgets use the unprofiled primary CPU clock. Cycle shares assume approximately stable effective frequency."
        )
    throttles = profile["collector"].get("throttle_events", 0)
    if throttles:
        lines.append(
            f"  - {throttles} throttle event(s) recorded; sampling gaps can bias these estimates even with no lost records."
        )
    for category, value in profile["categories"].items():
        budget = value["cpu_ns_per_useful_operation_estimate"]
        estimate = "absolute CPU budget unavailable"
        if budget is not None:
            estimate = f"approximately {budget:.1f} CPU ns/useful operation"
        lines.append(
            f"  - {category}: {value['sample_fraction']:.1%} of retained samples; {estimate}."
        )
    return lines


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("primary", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--trace", type=Path)
    parser.add_argument("--profiles", type=Path)
    parser.add_argument("--observer", type=Path)
    args = parser.parse_args()
    primary = load_run(args.primary)
    if primary.mode != "run":
        raise ValueError("primary evidence must be an untraced full run")
    model = {
        "schema": "dios-workload-cost-v1",
        "analyzer_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "primary": budgets(primary),
        "traces": {},
        "profiles": {},
        "runner_sha256": primary.runner,
        "executable_sha256": primary.executable,
        "instrumentation_pairs": [],
        "observer_overhead": {},
        "posture": primary.posture,
    }
    traced = load_run(args.trace) if args.trace else None
    if traced:
        require_matching(primary, traced)
        if traced.mode != "trace":
            raise ValueError("trace evidence must come from trace mode")
        model["traces"] = trace_budgets(traced)
    if args.profiles:
        model["profiles"] = profile_budgets(primary, args.profiles)
    args.output.mkdir(parents=True, exist_ok=False)
    if args.observer:
        observer_base = load_run(args.observer)
        observer_trace = load_run(args.observer / "traced")
        require_matching(primary, observer_base)
        require_matching(primary, observer_trace)
        model["instrumentation_pairs"] = prepare_overhead_pairs(
            observer_base, observer_trace, args.output
        )
        for lane, arm in observer_base.groups:
            ratio_path = args.observer / f"instrumentation_{lane}_{arm}.ratio.json"
            model["observer_overhead"][f"{lane}/{arm}"] = json.loads(
                ratio_path.read_text()
            )
    write_report(args.output, model)
    print(args.output / "cost-model.md")


if __name__ == "__main__":
    main()
