"""Validate driver probe pairs and join loss/unwind-qualified CPU evidence."""

import argparse
import csv
import json
from pathlib import Path
from statistics import fmean

from collect import digest
from probe_collect import validate_probe
from scan_analyze import io_bytes, match_identity, profile_counts


def pairs(root: Path, case: dict) -> list[list[dict]]:
    measured = {}
    for entry in case["rows"]:
        path = root / case["name"] / Path(entry["raw"]).name
        if digest(path) != entry["sha256"]:
            raise ValueError("probe raw hash differs")
        row = json.loads(path.read_text())
        validate_probe(row)
        if row["method"] != entry["method"] or row.get("depth", 1) != case.get("depth", 1):
            raise ValueError("probe method or depth differs from index")
        if entry["pair"] < 0:
            continue
        values = measured.setdefault(entry["pair"], [None, None])
        if values[entry["position"]] is not None:
            raise ValueError("duplicate probe arm")
        values[entry["position"]] = row
    result = [measured[index] for index in range(len(measured))]
    if len(result) < 30 or len(result) != case["summary"]["pairs"]:
        raise ValueError("probe pair count differs")
    for base, candidate in result:
        if base["method"] != case["base"] or candidate["method"] != case["candidate"]:
            raise ValueError("probe comparison method differs")
        for field in ("useful_bytes", "expected_checksum", "runner_sha256"):
            if base[field] != candidate[field]:
                raise ValueError(f"probe paired {field} differs")
        if base.get("depth", 1) != candidate.get("depth", 1):
            raise ValueError("probe paired depth differs")
    path = root / case["name"] / "paired.csv"
    with path.open() as stream:
        rows = list(csv.reader(stream))
    expected = [["base_ns", "candidate_ns"]] + [[str(r["elapsed_ns"]) for r in pair] for pair in result]
    if digest(path) != case["csv_sha256"] or rows != expected:
        raise ValueError("probe CSV differs from raw pairs")
    return result


def costs(rows: list[dict]) -> dict:
    pages = rows[0]["pages"]
    result = dict(ns_per_page=fmean(r["elapsed_ns"] / pages for r in rows),
                  cpu_ns_per_page=fmean(r["cpu_ns"] / pages for r in rows),
                  gb_per_second=fmean(r["useful_bytes"] / r["elapsed_ns"] for r in rows),
                  read_amplification=fmean(io_bytes(r) / r["useful_bytes"] for r in rows))
    for field in ("user_cpu_us", "system_cpu_us"):
        result[field.replace("_us", "_ns_per_page")] = fmean(r["usage"][field] * 1000 / pages for r in rows)
    result["counters"] = {}
    for field, value in rows[0]["counters"].items():
        if isinstance(value, list):
            result["counters"][field] = [fmean(r["counters"][field][index] for r in rows)
                                          for index in range(len(value))]
        else:
            result["counters"][field] = fmean(r["counters"][field] for r in rows)
    return result


def profile(root: Path, manifest: dict, cpu: float) -> dict:
    metadata = json.loads((root / "metadata.json").read_text())
    match_identity(metadata, manifest)
    if metadata["status"] != "complete" or any(metadata[k] for k in ("lost_events", "throttle_events", "unthrottle_events")):
        raise ValueError("probe profile lacks complete, uninterrupted samples")
    counts, excluded, orphaned = profile_counts((root / "profile.folded").read_text().splitlines(), metadata["workload_boundaries"])
    total = sum(counts.values())
    if total != metadata["timed_samples"] or total < 100:
        raise ValueError("probe profile coverage differs")
    replay = []
    for name, expected in metadata["sample_sha256"].items():
        path = root / "samples" / name
        if digest(path) != expected:
            raise ValueError("probe replay hash differs")
        row = json.loads(path.read_text())
        validate_probe(row)
        if row["method"] != metadata["method"] or row.get("depth", 1) != metadata.get("depth", 1):
            raise ValueError("probe replay method or depth differs")
        replay.append(row["cpu_ns"] / row["pages"])
    if len(replay) != metadata["repetitions"]:
        raise ValueError("probe replay count differs")
    qualified = orphaned * 100 <= total
    return dict(metadata=metadata, excluded_samples=excluded, orphaned_samples=orphaned,
                cpu_budget_qualified=qualified, profile_cpu_over_primary=fmean(replay) / cpu,
                categories={key: dict(samples=value, fraction=value / total,
                    estimated_cpu_ns_per_page=cpu * value / total if qualified else None)
                            for key, value in counts.most_common()})


def profile_key(method: str, depth: int) -> str:
    return method if depth == 1 else f"depth-{depth}/{method}"


def analyze(options: argparse.Namespace) -> None:
    manifest = json.loads((options.primary / "manifest.json").read_text())
    if manifest["status"] != "complete" or manifest["mode"] != "run":
        raise ValueError("probe analysis requires completed primary pairs")
    model, primary = dict(comparisons=[], profiles={}), {}
    for case in manifest["comparisons"]:
        measured = pairs(options.primary, case)
        base, candidate = [costs([pair[index] for pair in measured]) for index in (0, 1)]
        depth = case.get("depth", 1)
        model["comparisons"].append(dict(name=case["name"], base=case["base"], candidate=case["candidate"], depth=depth,
                                        base_cost=base, candidate_cost=candidate, summary=case["summary"]))
        for key, value in (("base", base), ("candidate", candidate)):
            primary.setdefault(profile_key(case[key], depth), value)
    for directory in options.profiles or []:
        metadata = json.loads((directory / "metadata.json").read_text())
        key = profile_key(metadata["method"], metadata.get("depth", 1))
        model["profiles"][key] = profile(directory, manifest, primary[key]["cpu_ns_per_page"])
    options.output.mkdir(parents=True, exist_ok=False)
    (options.output / "model.json").write_text(json.dumps(model, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("primary", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--profiles", type=Path, nargs="+")
    analyze(parser.parse_args())
