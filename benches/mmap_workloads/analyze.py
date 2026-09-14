# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Validate paired artifacts and explain mmap/Dios CPU, fault and I/O costs."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from statistics import fmean

from validate import counters, validate_row


def read_comparison(
    root: Path, comparison: dict, *, retain_events: bool = False
) -> list[tuple[dict, dict]]:
    directory = root / comparison["name"]
    paired = {}
    for reference in comparison["rows"]:
        path = directory / Path(reference["raw"]).name
        raw = path.read_bytes()
        if hashlib.sha256(raw).hexdigest() != reference["sha256"]:
            raise ValueError(f"raw sample hash differs: {path}")
        row = json.loads(raw)
        validate_row(row)
        position, pair = reference["position"], reference["pair"]
        expected_lane = comparison.get("lanes", [comparison["lane"]["name"]] * 2)[
            position
        ]
        if "runners" in comparison:
            if row["runner_sha256"] != comparison["runners"][position]:
                raise ValueError("paired runner identity differs")
        if row["arm"] != comparison["arms"][position]:
            raise ValueError("paired arm identity differs")
        if row["trace"] != (comparison["modes"][position] == "trace"):
            raise ValueError("paired observation mode differs")
        if row["seed"] != reference["seed"] or row["lane"] != expected_lane:
            raise ValueError("paired workload identity differs")
        if pair < 0:
            continue
        if not retain_events:
            for worker in row["workers"]:
                worker["events"] = []
                worker["flights"] = []
        if pair not in paired:
            paired[pair] = [None, None]
        if paired[pair][position] is not None:
            raise ValueError("duplicate paired arm")
        paired[pair][position] = row
    result = []
    for pair in range(len(paired)):
        base, candidate = paired[pair]
        if base is None or candidate is None:
            raise ValueError("missing paired arm")
        for key in (
            "seed",
            "expected_checksum",
            "operations",
            "useful_bytes",
            "fixture",
        ):
            if base[key] != candidate[key]:
                raise ValueError(f"unequal paired work: {key}")
        for key, declaration in (("lane", "lanes"), ("runner_sha256", "runners")):
            if declaration not in comparison and base[key] != candidate[key]:
                raise ValueError(f"unequal paired work: {key}")
        result.append((base, candidate))
    if "summary" in comparison:
        verify_csv(directory, comparison, result)
    return result


def verify_csv(directory: Path, comparison: dict, pairs: list) -> None:
    csv = (directory / "paired.csv").read_bytes()
    if hashlib.sha256(csv).hexdigest() != comparison["csv_sha256"]:
        raise ValueError("paired CSV hash differs")
    lines = csv.decode().splitlines()
    expected = [
        f"{base['elapsed_ns']},{candidate['elapsed_ns']}" for base, candidate in pairs
    ]
    if lines[0] != "base_ns,candidate_ns" or lines[1:] != expected:
        raise ValueError("paired CSV does not reflect validated raw samples")
    if len(pairs) < 30 or comparison["summary"]["pairs"] != len(pairs):
        raise ValueError("insufficient comparison pairs")


def resource_delta(row: dict, section: str, counter: str) -> int | None:
    before = counters(row["system_before"][section])
    after = counters(row["system_after"][section])
    if counter not in before or counter not in after:
        return None
    difference = after[counter] - before[counter]
    if difference < 0:
        raise ValueError(f"non-monotonic counter {counter}")
    return difference


def summarize_arm(rows: list[dict]) -> dict:
    result = {"arm": rows[0]["arm"], "samples": len(rows)}
    for name in (
        "cpu_ns",
        "minor_faults",
        "major_faults",
        "voluntary_switches",
        "involuntary_switches",
        "hits",
        "pending",
        "polls",
        "ready_checks",
        "reclaimed_lower_bound",
        "allocations",
        "user_cpu_us",
        "system_cpu_us",
        "setup_ns",
        "teardown_ns",
        "setup_storage_bytes",
        "retained_payload_bytes",
        "retained_reads",
        "guard_acquisitions",
    ):
        result[f"{name}_mean"] = fmean(
            sum(worker.get(name, 0) for worker in row["workers"]) for row in rows
        )
    result["elapsed_ns_mean"] = fmean(row["elapsed_ns"] for row in rows)
    result["ns_per_read"] = result["elapsed_ns_mean"] / rows[0]["operations"]
    result["cpu_ns_per_read"] = result["cpu_ns_mean"] / rows[0]["operations"]
    elapsed_workers = fmean(
        sum(worker["elapsed_ns"] for worker in row["workers"]) for row in rows
    )
    result["worker_cpu_capacity_fraction"] = result["cpu_ns_mean"] / elapsed_workers
    for section, counter in [
        ("process_io", "read_bytes"),
        ("memory.stat", "pgscan"),
        ("memory.stat", "pgsteal"),
        ("memory.stat", "workingset_refault_file"),
    ]:
        values = [resource_delta(row, section, counter) for row in rows]
        result[f"{counter}_mean"] = (
            None if any(value is None for value in values) else fmean(values)
        )
    result["read_amplification_per_page"] = result["read_bytes_mean"] / (
        rows[0]["operations"] * 4096
    )
    if rows[0].get("prefetch") is not None:
        result["prefetch_mean"] = {
            name: fmean(row["prefetch"][name] for row in rows)
            for name in rows[0]["prefetch"]
        }
    return result


def analyze(root: Path) -> dict:
    manifest = json.loads((root / "manifest.json").read_text())
    if manifest["status"] != "complete" or manifest["mode"] != "run":
        raise ValueError("primary must be a complete 30-pair campaign")
    results = []
    for comparison in manifest["comparisons"]:
        pairs = read_comparison(root, comparison)
        results.append(
            {
                "name": comparison["name"],
                "workload": comparison["lane"],
                "ratio": comparison["summary"],
                "base": summarize_arm([base for base, _ in pairs]),
                "candidate": summarize_arm([candidate for _, candidate in pairs]),
            }
        )
    return {
        "schema": 1,
        "source_manifest_sha256": hashlib.sha256(
            (root / "manifest.json").read_bytes()
        ).hexdigest(),
        "executable_sha256": manifest["executable_sha256"],
        "interpretation": "Candidate/base, named matched-work arms; worker CPU includes polling and first-fill faults",
        "results": results,
    }


def markdown(model: dict) -> str:
    lines = [
        "Candidate elapsed / base elapsed. Values below 1 favor the named candidate.",
        "",
        "| Workload | Base arm | Candidate arm | Ratio | 95% upper | Base ns/read | Candidate ns/read |",
        "|---|---|---|---:|---:|---:|---:|",
    ]
    for row in model["results"]:
        base, candidate, ratio = row["base"], row["candidate"], row["ratio"]
        lines.append(
            f"| {row['name']} | {base['arm']} | {candidate['arm']} | {ratio['ratio_geomean']:.3f} | "
            f"{ratio['ci95_upper']:.3f} | {base['ns_per_read']:.1f} | {candidate['ns_per_read']:.1f} |"
        )
    lines += [
        "",
        "Arithmetic ns/read means and paired geometric ratios need not have the same quotient.",
        "",
        "| Workload / arm | CPU ns/read | Minor faults | Major faults | OS read bytes / requested page bytes |",
        "|---|---:|---:|---:|---:|",
    ]
    for row in model["results"]:
        for key in ("base", "candidate"):
            arm = row[key]
            lines.append(
                f"| {row['name']} / {arm['arm']} | {arm['cpu_ns_per_read']:.1f} | "
                f"{arm['minor_faults_mean']:.1f} | {arm['major_faults_mean']:.1f} | "
                f"{arm['read_amplification_per_page']:.2f} |"
            )
    return "\n".join(lines) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("primary", type=Path)
    parser.add_argument("output", type=Path)
    options = parser.parse_args()
    model = analyze(options.primary.resolve())
    options.output.mkdir(parents=True, exist_ok=False)
    (options.output / "model.json").write_text(json.dumps(model, indent=2) + "\n")
    (options.output / "report.md").write_text(markdown(model))


if __name__ == "__main__":
    main()
