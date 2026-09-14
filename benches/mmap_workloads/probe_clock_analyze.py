"""Validate clock replay provenance, reconcile phases and assert observer gates."""

import argparse
import csv
import json
from pathlib import Path
from statistics import fmean
import subprocess

from collect import digest
from probe_clock_collect import metrics
from probe_collect import validate_probe


def read_pairs(root: Path, case: dict) -> list:
    measured = {}
    for entry in case["rows"]:
        path = root / case["name"] / Path(entry["raw"]).name
        if digest(path) != entry["sha256"]:
            raise ValueError("clock raw hash differs")
        row = json.loads(path.read_text())
        validate_probe(row)
        method, command = case["arms"][entry["position"]]
        if row["method"] != method or (row.get("diagnostic") is not None) != (command == "probe-clock-sample"):
            raise ValueError("clock arm differs from comparison")
        if entry["pair"] < 0:
            continue
        pair = measured.setdefault(entry["pair"], [None, None])
        if pair[entry["position"]] is not None:
            raise ValueError("duplicate clock arm")
        pair[entry["position"]] = row
    if len(measured) != 30:
        raise ValueError("clock replay requires 30 measured pairs")
    pairs = [measured[index] for index in range(30)]
    for base, candidate in pairs:
        for field in ("runner_sha256", "expected_checksum", "useful_bytes"):
            if base[field] != candidate[field]:
                raise ValueError(f"clock paired {field} differs")
    return pairs


def analyze(options: argparse.Namespace) -> None:
    manifest = json.loads((options.primary / "manifest.json").read_text())
    if manifest["status"] != "complete" or manifest["mode"] != "run":
        raise ValueError("clock analysis requires a completed primary")
    model = dict(executable_sha256=manifest["executable_sha256"],
                 compare_sha256=digest(options.compare), comparisons=[], observer_gates=[])
    for case in manifest["comparisons"]:
        pairs = read_pairs(options.primary, case)
        values = [[metrics(row) for row in pair] for pair in pairs]
        costs = [{field: fmean(pair[position][field] / 6144 for pair in values)
                  for field in values[0][position]} for position in (0, 1)]
        for field, comparison in case["metrics"].items():
            directory = options.primary / case["name"]
            if field != "elapsed_ns":
                directory /= field.removesuffix("_ns")
            path = directory / "paired.csv"
            expected = [["base_ns", "candidate_ns"]] + [[str(row[field]) for row in pair] for pair in values]
            with path.open() as stream:
                if list(csv.reader(stream)) != expected or digest(path) != comparison["csv_sha256"]:
                    raise ValueError("clock paired CSV differs from raw values")
            if case["name"].startswith("observer-"):
                command = [str(options.compare), str(path), "1.05"]
                result = subprocess.run(command, capture_output=True, text=True)
                if result.returncode not in (0, 1):
                    raise ValueError("observer compare could not run")
                model["observer_gates"].append(dict(name=case["name"], metric=field,
                    command=command, csv_sha256=digest(path), threshold=1.05,
                    exit_code=result.returncode, stdout=result.stdout, stderr=result.stderr))
        model["comparisons"].append(dict(name=case["name"], arms=case["arms"],
            base_ns_per_group=costs[0], candidate_ns_per_group=costs[1], metrics=case["metrics"]))
    if len(model["observer_gates"]) != 4:
        raise ValueError("clock observer gate set differs")
    model["observer_qualified"] = all(gate["exit_code"] == 0 for gate in model["observer_gates"])
    options.output.mkdir(parents=True, exist_ok=False)
    (options.output / "model.json").write_text(json.dumps(model, indent=2) + "\n")
    print(json.dumps(model, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("primary", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--compare", type=Path, required=True)
    analyze(parser.parse_args())
