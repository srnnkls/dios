# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Pair explicit windows with the retained pre-prefetch scan executable."""

import argparse
import json
from pathlib import Path

from analyze import read_comparison
from collect import (
    collect_one,
    digest,
    execute,
    host_snapshot,
    prepare,
    write_comparison,
)


def retained(root: Path) -> tuple[dict, Path]:
    manifest = json.loads((root / "manifest.json").read_text())
    if manifest["status"] != "complete" or manifest["mode"] != "run":
        raise ValueError("baseline/candidate must be a completed primary")
    binary = Path(manifest["executable"])
    if digest(binary) != manifest["executable_sha256"]:
        raise ValueError("retained executable hash differs")
    return manifest, binary


def compare(
    output: Path, binaries: list[Path], fixture: Path, lanes: list[dict]
) -> dict:
    fields = ("cache", "frames", "operations", "useful_bytes_per_read", "workers")
    for field in fields:
        if lanes[0][field] != lanes[1][field]:
            raise ValueError(f"frozen workload shape differs: {field}")
    name = f"{lanes[1]['name']}-frozen"
    directory = output / name
    directory.mkdir()
    arms = ["dios_batch", "dios_prefetch"]
    rows, timings = [], []
    for pair in range(-2, 30):
        seed = pair if pair >= 0 else 1_000_000 + pair
        measured = [0, 0]
        for position in (0, 1) if pair % 2 == 0 else (1, 0):
            target = directory / f"{pair:04d}-{position}.json"
            row = collect_one(
                binaries[position],
                fixture,
                lanes[position],
                arms[position],
                seed,
                "plain",
                target,
                "run",
            )
            row.update(pair=pair, position=position)
            rows.append(row)
            measured[position] = row["elapsed_ns"]
        if pair >= 0:
            timings.append(measured)
        if pair >= 0 and (pair + 1) % 10 == 0:
            print(f"{name}: {pair + 1}/30 pairs", flush=True)
    index = {
        "name": name,
        "lane": lanes[1],
        "lanes": [lane["name"] for lane in lanes],
        "arms": arms,
        "modes": ["plain", "plain"],
        "rows": rows,
        "runners": [
            json.loads(execute([str(binary), "identity"]))["runner_sha256"]
            for binary in binaries
        ],
        "matched_work_fields": list(fields),
        **write_comparison(binaries[1], directory, timings),
    }
    read_comparison(output, index)
    (directory / "index.json").write_text(json.dumps(index, indent=2) + "\n")
    return index


def collect(options: argparse.Namespace) -> None:
    baseline, base_binary = retained(options.baseline)
    candidate, options.binary = retained(options.candidate)
    if baseline["fixture"] != candidate["fixture"]:
        raise ValueError("frozen fixture differs")
    for source, expected in candidate["sources"].items():
        if digest(Path(source)) != expected:
            raise ValueError(f"candidate source changed since primary: {source}")
    options.input, options.mode = Path(candidate["input"]), "run"
    binary, fixture, manifest = prepare(options)
    output = options.output.resolve()
    manifest["frozen_baseline"] = {
        "manifest_sha256": digest(options.baseline / "manifest.json"),
        "executable": str(base_binary),
        "executable_sha256": digest(base_binary),
        "sources": baseline["sources"],
    }
    catalogs = [
        json.loads(execute([str(exe), "list"])) for exe in (base_binary, binary)
    ]
    try:
        for names in (
            ("scan_decode", "prefetch_scan"),
            ("pressure_scan", "prefetch_pressure_scan"),
        ):
            lanes = [
                next(lane for lane in catalog if lane["name"] == name)
                for catalog, name in zip(catalogs, names)
            ]
            manifest["comparisons"].append(
                compare(output, [base_binary, binary], fixture, lanes)
            )
            (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        if (
            digest(base_binary) != baseline["executable_sha256"]
            or digest(binary) != candidate["executable_sha256"]
        ):
            raise ValueError("an executable changed during collection")
        manifest.update(status="complete", host_after=host_snapshot(output, "after"))
    except Exception as error:
        manifest.update(status="failed", error=str(error))
        raise
    finally:
        (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    collect(parser.parse_args())
