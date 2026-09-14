"""Paired sequential scan geometry experiments; see benches/plans/scan_geometry.md."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess

from collect import digest, execute, host_snapshot, prepare, service_command, write_comparison
from validate import counters, validate_prefetch, validate_pressure, validate_row


def validate_scan(row: dict) -> None:
    config, work = row["config"], row["counters"]
    shape, method, kib, credits, limit = row["configuration"].split(":")
    expected = {"granule": int(kib) * 1024, "credits": int(credits),
                "read_limit": int(limit), "method": method,
                "arena_bytes": {"cold": 4, "geometry": 8, "pressure": 64}[shape] << 20,
                "lane": "pressure_scan" if shape == "pressure" else "scan_decode"}
    if config != expected or row["debug_assertions"]:
        raise ValueError("scan configuration/build differs")
    pages = 3 * 65536 if shape == "pressure" else 16384
    if row["pages"] != pages or row["useful_bytes"] != pages * 4096:
        raise ValueError("scan useful work differs")
    if row["requests"] * config["granule"] != row["useful_bytes"]:
        raise ValueError("scan request granularity differs")
    if work["operations"] != row["requests"] or work["allocations"]:
        raise ValueError("scan operation/allocation witness differs")
    if work["checksum"] != row["expected_checksum"] or row["elapsed_ns"] <= 0:
        raise ValueError("scan checksum/timing differs")
    if row["cpu_ns"] <= 0 or work["pending"] != work["completed_pending"]:
        raise ValueError("scan CPU/drain witness differs")
    if row["cache_before"]["cold_resident"] or row["cache_before"]["cold_present_ptes"]:
        raise ValueError("scan targets were not cold")
    if row["cache_before"]["cold_expected"] != min(pages, 65536):
        raise ValueError("scan cache witness coverage differs")
    if method == "mmap":
        if row["usage"]["major_faults"] == 0 or work["hits"] or work["pending"]:
            raise ValueError("mmap fault/path witness differs")
        if shape == "pressure":
            before = counters(row["system_before"]["memory.stat"])
            after = counters(row["system_after"]["memory.stat"])
            if after.get("pgscan", 0) <= before.get("pgscan", 0):
                raise ValueError("mmap scan lacks reclaim evidence")
    else:
        if row["io_mode"] != "Direct" or row["registration"] != "Unregistered":
            raise ValueError("scan storage posture differs")
        if work["hits"] + work["pending"] != row["requests"] or not work["pending"]:
            raise ValueError("Dios scan demand witness differs")
        validate_prefetch({**row, "lane": "scan_geometry"})
        if row["prefetch"]["capacity"] != config["credits"]:
            raise ValueError("scan credit capacity differs")
        if method == "explicit" and row["prefetch"]["automatic_admitted"]:
            raise ValueError("explicit control learned automatically")
    validate_pressure({**row, "cache": "pressure" if shape == "pressure" else "cold"})
    validate_observations(row)


def validate_observations(row: dict) -> None:
    events, flights, config = row["events"], row["flights"], row["config"]
    if not row["trace"]:
        if events or flights:
            raise ValueError("plain scan contains detailed observations")
        return
    if len(events) != row["requests"]:
        raise ValueError("scan trace coverage differs")
    per_pass = row["requests"] // (3 if config["lane"] == "pressure_scan" else 1)
    for index, event in enumerate(events):
        if event["operation"] != index or event["page"] != (index % per_pass) * (config["granule"] // 4096):
            raise ValueError("scan trace order/address differs")
        if not 0 <= event["start_ns"] <= event["end_ns"] <= row["elapsed_ns"]:
            raise ValueError("scan trace outside timing")
    for previous, event in zip([{"at_ns": 0}] + flights, flights):
        if not previous["at_ns"] <= event["at_ns"] <= row["elapsed_ns"]:
            raise ValueError("scan flight time differs")
        if not 0 <= event["reads"] <= config["read_limit"]:
            raise ValueError("scan read occupancy exceeds capacity")
        if not 0 <= event["speculative"] <= config["credits"]:
            raise ValueError("scan speculative occupancy exceeds capacity")
    if config["method"] != "mmap" and (not flights or flights[-1]["reads"]):
        raise ValueError("scan trace lacks terminal drain")


def matrix(experiment: str, selected: list[str] | None) -> list[tuple[str, str, str]]:
    cases = []
    for shape in ("cold", "pressure"):
        mmap = f"{shape}:mmap:4:0:0"
        default = f"{shape}:automatic:4:32:64"
        if experiment == "lookahead":
            cases.append((f"{shape}-default-mmap", mmap, default))
            for credits in (16, 32, 64):
                cases.append((f"{shape}-matched-{credits}", f"{shape}:explicit:4:{credits}:128", f"{shape}:automatic:4:{credits}:128"))
            for method in ("explicit", "automatic"):
                for before, after in ((16, 32), (32, 64)):
                    cases.append((f"{shape}-{method}-{before}-{after}", f"{shape}:{method}:4:{before}:128", f"{shape}:{method}:4:{after}:128"))
        elif experiment == "geometry":
            geometry = "geometry" if shape == "cold" else shape
            for kib in (64, 256, 1024):
                limit = 2048 // kib
                cases.append((f"{shape}-granule-{kib}", f"{geometry}:explicit:4:511:512", f"{geometry}:explicit:{kib}:{limit-1}:{limit}"))
        elif experiment == "bridge":
            legacy = "automatic_scan" if shape == "cold" else "automatic_pressure_scan"
            cases.append((f"{shape}-bridge", f"legacy:{legacy}:dios_automatic", default))
    if experiment in ("confirmation", "observe", "trace"):
        if not selected:
            raise ValueError("selected configurations required")
        for index, config in enumerate(selected):
            shape = "pressure" if config.startswith("pressure:") else "cold"
            base = config if experiment in ("observe", "trace") else f"{shape}:mmap:4:0:0"
            cases.append((f"selected-{index}", base, config))
    if not cases or len(cases) > 32:
        raise ValueError("invalid or excessive comparison count")
    return cases


def command(binary: Path, fixture: Path, config: str, mode: str, target: Path) -> list[str]:
    legacy = config.startswith("legacy:")
    shape = "pressure" if "pressure" in config.split(":")[0 if not legacy else 1] else "cold"
    lane = {"name": "pressure_scan" if shape == "pressure" else "scan_decode", "cache": shape}
    original = service_command(binary, fixture, lane, "mmap_sequential", 0, mode, target)
    prefix = original[:original.index(str(binary))]
    if legacy:
        _, lane, arm = config.split(":")
        return prefix + [str(binary), "sample", str(fixture), lane, arm, "0", mode, str(target)]
    return prefix + [str(binary), "scan-sample", str(fixture), config, mode, str(target)]


def sample(binary: Path, fixture: Path, config: str, mode: str, target: Path) -> dict:
    execute(command(binary, fixture, config, mode, target), target.with_suffix(".log"))
    row = json.loads(target.read_text())
    if config.startswith("legacy:"):
        validate_row(row)
    else:
        validate_scan(row)
        if row["configuration"] != config:
            raise ValueError("sample configuration differs from request")
    if row["trace"] != (mode == "trace"):
        raise ValueError("sample observer differs from request")
    return row


def comparison(options: argparse.Namespace, binary: Path, fixture: Path, case: tuple) -> dict:
    name, base, candidate = case
    output = options.output.resolve() / name
    output.mkdir()
    pairs = 1 if options.mode == "smoke" or options.experiment == "trace" else 30
    warmups = 2 if pairs == 30 else 0
    modes = ["plain", "trace"] if options.experiment == "observe" else ["plain", "plain"]
    if options.experiment == "trace":
        modes = ["trace", "trace"]
    entries, timings = [], []
    for pair in range(-warmups, pairs):
        measured, rows = [0, 0], [None, None]
        for position in ((0, 1) if pair % 2 == 0 else (1, 0)):
            config, mode = (base, candidate)[position], modes[position]
            target = output / f"{pair:04d}-{position}.json"
            row = sample(binary, fixture, config, mode, target)
            rows[position] = row
            measured[position] = row["elapsed_ns"]
            entries.append({"raw": str(target), "sha256": digest(target), "pair": pair,
                            "position": position, "configuration": config, "mode": mode})
        for field in ("expected_checksum", "useful_bytes"):
            if rows[0][field] != rows[1][field]:
                raise ValueError(f"paired scan {field} differs")
        if pair >= 0:
            timings.append(measured)
        if (pair + 1) % 10 == 0:
            print(f"{name}: {pair+1}/{pairs} pairs", flush=True)
    index = {"name": name, "base": base, "candidate": candidate, "rows": entries}
    if pairs >= 30:
        index.update(write_comparison(binary, output, timings))
    (output / "index.json").write_text(json.dumps(index, indent=2) + "\n")
    print(f"{name}: {index.get('summary', {}).get('ratio_geomean', 'smoke')}", flush=True)
    return index


def collect(options: argparse.Namespace) -> None:
    cases = matrix(options.experiment, options.configs.split(",") if options.configs else None)
    if options.cases:
        selected = options.cases.split(",")
        if set(selected) - {case[0] for case in cases}:
            raise ValueError("unknown comparison selection")
        cases = [case for case in cases if case[0] in selected]
    binary, fixture, manifest = prepare(options)
    output = options.output.resolve()
    manifest.update(kind="scan_geometry", experiment=options.experiment)
    try:
        for case in cases:
            manifest["comparisons"].append(comparison(options, binary, fixture, case))
            (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        if digest(binary) != manifest["executable_sha256"]:
            raise ValueError("scan binary changed during collection")
        manifest.update(status="complete", host_after=host_snapshot(output, "after"))
    except (ValueError, RuntimeError, OSError, subprocess.TimeoutExpired) as failure:
        manifest.update(status="failed", error=str(failure))
        raise
    finally:
        (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--mode", choices=("smoke", "run"), default="run")
    parser.add_argument("--experiment", choices=("bridge", "lookahead", "geometry", "confirmation", "observe", "trace"), required=True)
    parser.add_argument("--configs", help="comma-separated configurations for confirmation/observation")
    parser.add_argument("--cases", help="bounded subset of named matrix comparisons")
    return parser.parse_args()


if __name__ == "__main__":
    collect(arguments())
