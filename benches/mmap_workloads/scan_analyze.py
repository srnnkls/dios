"""Join validated scan pairs, observation controls and normalized CPU profiles."""

import argparse
from collections import Counter
import csv
import json
from pathlib import Path
from statistics import fmean

from collect import digest
from diagnose import category
from scan_collect import validate_scan
from stacks import workload_boundary
from validate import counters, validate_row


def match_identity(candidate: dict, primary: dict) -> None:
    for field in ("executable_sha256", "fixture"):
        if candidate[field] != primary[field]:
            raise ValueError(f"scan attribution {field} differs from primary")


def read_pairs(root: Path, case: dict) -> list[tuple[dict, dict]]:
    pairs = {}
    for reference in case["rows"]:
        path = root / case["name"] / Path(reference["raw"]).name
        if digest(path) != reference["sha256"]:
            raise ValueError(f"sample hash differs: {path}")
        row = json.loads(path.read_text())
        if reference["configuration"].startswith("legacy:"):
            validate_row(row)
        else:
            validate_scan(row)
            if row["configuration"] != reference["configuration"]:
                raise ValueError("raw scan configuration differs from index")
        if row["trace"] != (reference["mode"] == "trace"):
            raise ValueError("raw scan observer differs from index")
        pair, position = reference["pair"], reference["position"]
        if pair < 0:
            continue
        values = pairs.setdefault(pair, [None, None])
        if values[position] is not None:
            raise ValueError("duplicate scan arm")
        values[position] = row
    result = []
    for index in range(len(pairs)):
        base, candidate = pairs[index]
        if base is None or candidate is None:
            raise ValueError("missing scan pair")
        for name in ("expected_checksum", "useful_bytes", "runner_sha256"):
            if base[name] != candidate[name]:
                raise ValueError(f"paired scan {name} differs")
        result.append((base, candidate))
    if "summary" in case:
        path = root / case["name"] / "paired.csv"
        if digest(path) != case["csv_sha256"]:
            raise ValueError("scan CSV hash differs")
        rows = list(csv.reader(path.open()))
        expected = [[str(a["elapsed_ns"]), str(b["elapsed_ns"])] for a, b in result]
        if rows != [["base_ns", "candidate_ns"]] + expected or len(result) < 30:
            raise ValueError("scan CSV differs from validated pairs")
        if case["summary"]["pairs"] != len(result):
            raise ValueError("scan statistic sample count differs")
    return result


def io_bytes(row: dict) -> int:
    before = counters(row["system_before"]["process_io"])
    after = counters(row["system_after"]["process_io"])
    value = after["read_bytes"] - before["read_bytes"]
    if value < 0:
        raise ValueError("non-monotonic process input counter")
    return value


def arm(rows: list[dict]) -> dict:
    first = rows[0]
    legacy = "configuration" not in first
    pages = first["operations"] if legacy else first["pages"]
    cpu = [sum(w["cpu_ns"] for w in row["workers"]) if legacy else row["cpu_ns"] for row in rows]
    values = {"samples": len(rows), "ns_per_page": fmean(r["elapsed_ns"] / pages for r in rows),
              "cpu_ns_per_page": fmean(cpu) / pages,
              "read_amplification": fmean(io_bytes(r) / r["useful_bytes"] for r in rows),
              "gb_per_second": fmean(r["useful_bytes"] / r["elapsed_ns"] for r in rows)}
    if not legacy:
        values["config"] = first["config"]
        for field in ("polls", "prefetch_calls", "ready_checks", "busy"):
            values[field + "_per_page"] = fmean(r["counters"][field] / pages for r in rows)
        for field in ("minor_faults", "major_faults", "voluntary_switches", "involuntary_switches"):
            values[field] = fmean(r["usage"][field] for r in rows)
        for field in ("user_cpu_us", "system_cpu_us"):
            values[field.replace("_us", "_ns_per_page")] = fmean(r["usage"][field] * 1000 / pages for r in rows)
        if first["prefetch"] is not None:
            values["prefetch"] = {key: fmean(r["prefetch"][key] for r in rows) for key in first["prefetch"]}
    return values


def flights(row: dict) -> dict:
    previous_time = previous_reads = integral = maximum = 0
    for event in row["flights"]:
        integral += previous_reads * (event["at_ns"] - previous_time)
        previous_time, previous_reads = event["at_ns"], event["reads"]
        maximum = max(maximum, previous_reads)
    integral += previous_reads * (row["elapsed_ns"] - previous_time)
    return {"logical_reads_mean": integral / row["elapsed_ns"], "logical_reads_max": maximum,
            "logical_bytes_mean": integral / row["elapsed_ns"] * row["config"]["granule"],
            "flight_events": len(row["flights"])}


def profile_counts(lines: list[str], boundaries: list[str]) -> tuple[Counter, int, int]:
    counts, excluded, orphaned = Counter(), 0, 0
    for line in lines:
        stack, weight = line.rsplit(" ", 1)
        frames = stack.split(";")
        if workload_boundary(frames, boundaries) is None:
            excluded += int(weight)
            if not any("mmap_workloads::main" in frame for frame in frames):
                orphaned += int(weight)
        else:
            counts[category(frames)] += int(weight)
    return counts, excluded, orphaned


def profile(path: Path, cpu: float, identity: dict) -> dict:
    metadata = json.loads((path / "metadata.json").read_text())
    match_identity(metadata, identity)
    if metadata["status"] != "complete":
        raise ValueError("unmatched or incomplete scan profile")
    if any(metadata[key] for key in ("lost_events", "throttle_events", "unthrottle_events")):
        raise ValueError("scan profile lost or throttled samples")
    counts, excluded, orphaned = profile_counts((path / "profile.folded").read_text().splitlines(),
                                                metadata["workload_boundaries"])
    total = sum(counts.values())
    if total != metadata["timed_samples"] or total < 100:
        raise ValueError("scan profile coverage differs")
    replay = []
    for name, expected in metadata["sample_sha256"].items():
        sample = path / "samples" / name
        if digest(sample) != expected:
            raise ValueError("scan profile replay hash differs")
        row = json.loads(sample.read_text())
        validate_scan(row)
        if row["configuration"] != metadata["configuration"]:
            raise ValueError("scan profile configuration differs")
        replay.append(row["cpu_ns"] / row["pages"])
    if len(replay) != metadata["repetitions"]:
        raise ValueError("scan profile repetitions differ")
    qualified = orphaned * 100 <= total
    return {"metadata": metadata, "excluded_samples": excluded, "orphaned_samples": orphaned,
            "cpu_budget_qualified": qualified,
            "profile_cpu_over_primary": fmean(replay) / cpu,
            "categories": {key: {"samples": value, "fraction": value / total,
                                 "estimated_cpu_ns_per_page": cpu * value / total if qualified else None}
                           for key, value in counts.most_common()}}


def analyze(options: argparse.Namespace) -> None:
    result, primary = {"comparisons": [], "observations": {}, "profiles": {}}, {}
    identity = json.loads((options.primary[0] / "manifest.json").read_text())
    for root in options.primary:
        manifest = json.loads((root / "manifest.json").read_text())
        match_identity(manifest, identity)
        if manifest["status"] != "complete" or manifest["mode"] != "run":
            raise ValueError("analysis requires completed primary campaigns")
        for case in manifest["comparisons"]:
            pairs = read_pairs(root, case)
            base, candidate = [arm([pair[index] for pair in pairs]) for index in (0, 1)]
            result["comparisons"].append({"name": case["name"], "base": case["base"],
                                          "candidate": case["candidate"], "summary": case["summary"],
                                          "base_cost": base, "candidate_cost": candidate})
            for key, value in (("base", base), ("candidate", candidate)):
                primary[case[key]] = value["cpu_ns_per_page"]
    for root in options.observe or []:
        manifest = json.loads((root / "manifest.json").read_text())
        match_identity(manifest, identity)
        if manifest["status"] != "complete" or manifest["experiment"] != "observe":
            raise ValueError("observation campaign incomplete")
        for case in manifest["comparisons"]:
            if case["base"] != case["candidate"] or case["candidate"] not in primary:
                raise ValueError("observer changes configuration or lacks primary samples")
            pairs = read_pairs(root, case)
            witnesses = [flights(candidate) for _, candidate in pairs]
            result["observations"][case["candidate"]] = {"overhead": case["summary"],
                "trace_latency_qualified": case["summary"]["ci95_upper"] <= 1.05,
                "occupancy": {key: fmean(w[key] for w in witnesses) for key in witnesses[0]}}
    for directory in options.profiles or []:
        metadata = json.loads((directory / "metadata.json").read_text())
        config = metadata["configuration"]
        result["profiles"][config] = profile(directory, primary[config], identity)
    options.output.mkdir(parents=True, exist_ok=False)
    (options.output / "model.json").write_text(json.dumps(result, indent=2) + "\n")
    (options.output / "tables.md").write_text(tables(result))


def tables(model: dict) -> str:
    lines = ["Candidate/base elapsed; shared one-sided 95% upper bounds.", "",
             "| Comparison | Base | Candidate | Ratio | Upper | Base ns/page | Candidate ns/page |",
             "|---|---|---|---:|---:|---:|---:|"]
    for row in model["comparisons"]:
        lines.append(f"| {row['name']} | {row['base']} | {row['candidate']} | {row['summary']['ratio_geomean']:.4f} | {row['summary']['ci95_upper']:.4f} | {row['base_cost']['ns_per_page']:.1f} | {row['candidate_cost']['ns_per_page']:.1f} |")
    lines += ["", "CPU category budgets are unprofiled CPU/page times replay sample shares.", "",
              "| Configuration | Category | Share | Estimated CPU ns/page |", "|---|---|---:|---:|"]
    for config, profile in model["profiles"].items():
        for name, value in profile["categories"].items():
            estimate = value['estimated_cpu_ns_per_page']
            budget = "withheld: orphaned stacks" if estimate is None else f"{estimate:.1f}"
            lines.append(f"| {config} | {name} | {value['fraction']:.3f} | {budget} |")
    return "\n".join(lines) + "\n"


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--primary", type=Path, nargs="+", required=True)
    parser.add_argument("--observe", type=Path, nargs="+")
    parser.add_argument("--profiles", type=Path, nargs="+")
    analyze(parser.parse_args())
