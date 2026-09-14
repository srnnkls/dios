# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Collect bounded, interleaved mmap/Dios process pairs on the pinned Linux host."""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tarfile
import time

from validate import validate_row


RUNNER_SOURCES = ["benches/mmap_workloads.rs"] + [
    f"benches/mmap_workloads/{name}" for name in (
        "mod.rs", "catalog.rs", "engine.rs", "fixture.rs", "observe.rs", "os.rs",
        "resident.rs", "scan_config.rs", "scan.rs", "probe.rs", "probe/clock.rs")
]


def source_paths() -> list[Path]:
    sources = sorted(Path(__file__).parent.rglob("*.rs"))
    sources += sorted(Path(__file__).parent.glob("*.py"))
    sources += [Path("benches/mmap_workloads.rs"), Path("Cargo.toml"), Path("Cargo.lock")]
    return sources + sorted(Path("src").rglob("*.rs")) + [Path("build.rs")]


def source_hashes() -> dict[str, str]:
    root = Path(__file__).resolve().parents[2]
    return {str(path.resolve().relative_to(root)): digest(path) for path in source_paths()}


def archive_sources(output: Path, expected: dict[str, str]) -> dict:
    path = output / "source.tar.gz"
    with tarfile.open(path, "x:gz") as archive:
        for name, checksum in expected.items():
            if digest(Path(name)) != checksum:
                raise ValueError(f"source changed during snapshot: {name}")
            archive.add(name, arcname=name, recursive=False)
    if source_hashes() != expected:
        raise ValueError("source changed during archival")
    return {"source_archive": str(path), "source_archive_sha256": digest(path)}


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def execute(command: list[str], log: Path | None = None, timeout: int = 190) -> str:
    result = subprocess.run(command, text=True, capture_output=True, timeout=timeout)
    if log is not None:
        log.write_text(result.stdout + result.stderr)
    if result.returncode:
        raise RuntimeError(
            f"exit {result.returncode}: {command!r}\n{result.stderr[-3000:]}"
        )
    return result.stdout


def build_binary(output: Path) -> Path:
    text = execute(
        [
            "cargo",
            "build",
            "--profile",
            "profiling",
            "--features",
            "bench",
            "--bench",
            "mmap_workloads",
            "--message-format=json",
        ],
        output / "build.log",
        timeout=300,
    )
    binaries = [
        entry["executable"]
        for line in text.splitlines()
        if (entry := json.loads(line)).get("executable")
        and entry.get("target", {}).get("name") == "mmap_workloads"
    ]
    if len(binaries) != 1:
        raise ValueError("expected exactly one mmap_workloads executable")
    return Path(binaries[0]).resolve()


def block_queues(root: Path = Path("/sys/class/block")) -> dict:
    queues = sorted(root.glob("*/queue/read_ahead_kb"))
    if len(queues) > 256:
        raise ValueError("block-device snapshot exceeds fixed bound")
    result = {}
    for setting in queues:
        queue, device = setting.parent, setting.parent.parent
        paths = {name: queue / name for name in (
            "read_ahead_kb", "max_sectors_kb", "max_hw_sectors_kb", "max_segments",
            "max_segment_size", "logical_block_size", "physical_block_size", "nr_requests")}
        paths["device_number"] = device / "dev"
        result[device.name] = {name: path.read_text().strip() if path.exists() else None
                               for name, path in paths.items()}
    return result


def host_snapshot(output: Path, name: str) -> dict:
    text = execute(["ps", "-eo", "pid,comm,args", "--sort=-pcpu"])
    (output / f"{name}-processes.txt").write_text(text)
    paths = {
        "thp": Path("/sys/kernel/mm/transparent_hugepage/enabled"),
        "meminfo": Path("/proc/meminfo"),
        "loadavg": Path("/proc/loadavg"),
        "boot_id": Path("/proc/sys/kernel/random/boot_id"),
    }
    snapshot = {key: path.read_text().strip() for key, path in paths.items()}
    snapshot["governors"] = sorted(
        {
            path.read_text().strip()
            for path in Path("/sys/devices/system/cpu").glob(
                "cpu[0-9]*/cpufreq/scaling_governor"
            )
        }
    )
    snapshot["uname"] = list(platform.uname())
    snapshot["block_queues"] = block_queues()
    snapshot["time_ns"] = time.time_ns()
    snapshot["artifact_filesystem"] = execute(
        ["findmnt", "-T", str(output), "-no", "SOURCE,FSTYPE"]
    ).strip()
    if snapshot["governors"] != ["performance"] or "[never]" not in snapshot["thp"]:
        raise ValueError("host differs from pinned governor/THP protocol")
    return snapshot


def prepare(options: argparse.Namespace) -> tuple[Path, Path, dict]:
    output = options.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    binary = options.binary.resolve() if options.binary else build_binary(output)
    compiled = json.loads(execute([str(binary), "identity"]))
    runner_hash = hashlib.sha256(
        b"".join(Path(path).read_bytes() for path in RUNNER_SOURCES)
    ).hexdigest()
    if compiled["runner_sha256"] != runner_hash or compiled["debug_assertions"]:
        raise ValueError("retained executable does not match current release harness")
    identity = digest(binary)
    saved = output / f"mmap_workloads-{identity}"
    shutil.copy2(binary, saved)
    fixture = options.input.resolve() if options.input else output / "input"
    if not options.input:
        execute([str(saved), "create", str(fixture)], output / "fixture.log")
    fixture_identity = json.loads((fixture / "fixture.json").read_text())
    if digest(fixture / "pages.bin") != fixture_identity["sha256"]:
        raise ValueError("fixture content hash differs")
    manifest = {
        "schema": 1,
        "status": "running",
        "mode": options.mode,
        "executable_sha256": identity,
        "executable": str(saved),
        "sources": source_hashes(),
        "fixture": fixture_identity,
        "fixture_device": str(Path(execute(["findmnt", "-T", str(fixture / "pages.bin"),
                                            "-no", "SOURCE"]).strip()).resolve()),
        "fixture_filesystem": execute(
            ["findmnt", "-T", str(fixture / "pages.bin"), "-no", "SOURCE,FSTYPE"]
        ).strip(),
        "input": str(fixture),
        "git_head": execute(["git", "rev-parse", "HEAD"]).strip(),
        "git_status": execute(["git", "status", "--short"]),
        "host_before": host_snapshot(output, "before"),
        "comparisons": [],
    }
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    return saved, fixture, manifest


def service_command(
    binary: Path,
    fixture: Path,
    lane: dict,
    arm: str,
    seed: int,
    tracing: str,
    target: Path,
) -> list[str]:
    command = [
        str(binary),
        "sample",
        str(fixture),
        lane["name"],
        arm,
        str(seed),
        tracing,
        str(target),
    ]
    if lane["cache"] == "pressure":
        prefix = [
            "systemd-run",
            "--user",
            "--wait",
            "--pipe",
            "--collect",
            f"--unit=dios-mmap-{os.getpid()}-{target.stem}",
            "-p",
            "MemoryMax=128M",
            "-p",
            "MemorySwapMax=0",
            "-p",
            "RuntimeMaxSec=180s",
        ]
        for name in ("NIX_LD", "NIX_LD_LIBRARY_PATH", "PATH"):
            if name in os.environ:
                prefix.append(f"--setenv={name}={os.environ[name]}")
        command = prefix + command
    return command


def collect_one(
    binary: Path,
    fixture: Path,
    lane: dict,
    arm: str,
    seed: int,
    tracing: str,
    target: Path,
    mode: str,
) -> dict:
    started = time.time_ns()
    command = service_command(binary, fixture, lane, arm, seed, tracing, target)
    execute(command, target.with_suffix(".log"))
    finished = time.time_ns()
    row = json.loads(target.read_text())
    validate_row(row)
    if row["lane"] != lane["name"] or row["arm"] != arm or row["seed"] != seed:
        raise ValueError("sample identity differs from command")
    if row["trace"] != (tracing == "trace") or row["debug_assertions"]:
        raise ValueError("sample instrumentation/build differs")
    if (
        row["operations"] != lane["operations"]
        or len(row["workers"]) != lane["workers"]
    ):
        raise ValueError("workload counts differ from catalog")
    if row["useful_bytes"] != lane["operations"] * lane["useful_bytes_per_read"]:
        raise ValueError("useful byte count differs")
    return {
        "raw": str(target),
        "sha256": digest(target),
        "arm": arm,
        "seed": seed,
        "tracing": tracing,
        "mode": mode,
        "started_ns": started,
        "finished_ns": finished,
        "elapsed_ns": row["elapsed_ns"],
    }


def collect_comparison(
    options: argparse.Namespace,
    binary: Path,
    fixture: Path,
    lane: dict,
    arms: list[str],
    modes: list[str],
    name: str,
) -> dict:
    output = options.output.resolve() / name
    output.mkdir()
    pairs = 30 if options.mode in ("run", "observe") else 1
    warmups = 2 if pairs == 30 else 0
    rows, timings = [], []
    for pair in range(-warmups, pairs):
        seed = pair if pair >= 0 else 1_000_000 + pair
        order = (0, 1) if pair % 2 == 0 else (1, 0)
        measured = [0, 0]
        for position in order:
            target = output / f"{pair:04d}-{position}.json"
            row = collect_one(
                binary,
                fixture,
                lane,
                arms[position],
                seed,
                modes[position],
                target,
                options.mode,
            )
            row.update(pair=pair, position=position)
            rows.append(row)
            measured[position] = row["elapsed_ns"]
        if pair >= 0:
            timings.append(measured)
        if pair >= 0 and (pair + 1) % 10 == 0:
            print(f"{name}: {pair + 1}/{pairs} pairs", flush=True)
    index = {"lane": lane, "name": name, "arms": arms, "modes": modes, "rows": rows}
    if pairs >= 30:
        index.update(write_comparison(binary, output, timings))
    (output / "index.json").write_text(json.dumps(index, indent=2) + "\n")
    return index


def write_comparison(binary: Path, output: Path, timings: list) -> dict:
    path = output / "paired.csv"
    with path.open("w", newline="") as stream:
        writer = csv.writer(stream)
        writer.writerow(["base_ns", "candidate_ns"])
        writer.writerows(timings)
    summary = json.loads(execute([str(binary), "summarize", str(path)]))
    (output / "ratio.json").write_text(json.dumps(summary, indent=2) + "\n")
    return {"csv_sha256": digest(path), "summary": summary}


def collect(options: argparse.Namespace) -> None:
    binary, fixture, manifest = prepare(options)
    output = options.output.resolve()
    try:
        lanes = json.loads(execute([str(binary), "list"]))
        selected = (
            options.lanes.split(",")
            if options.lanes
            else [lane["name"] for lane in lanes]
        )
        if set(selected) - {lane["name"] for lane in lanes}:
            raise ValueError("unknown lane selection")
        for lane in lanes:
            if lane["name"] not in selected:
                continue
            if options.mode == "observe":
                for arm in lane["arms"]:
                    result = collect_comparison(
                        options,
                        binary,
                        fixture,
                        lane,
                        [arm, arm],
                        ["plain", "trace"],
                        f"{lane['name']}-{arm}",
                    )
                    manifest["comparisons"].append(result)
            else:
                modes = (
                    ["trace", "trace"]
                    if options.mode == "trace"
                    else ["plain", "plain"]
                )
                result = collect_comparison(
                    options, binary, fixture, lane, lane["arms"], modes, lane["name"]
                )
                manifest["comparisons"].append(result)
            (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
            print(f"{lane['name']}: complete", flush=True)
        if digest(binary) != manifest["executable_sha256"]:
            raise ValueError("executable changed during collection")
        manifest.update(status="complete", host_after=host_snapshot(output, "after"))
    except (ValueError, RuntimeError, OSError, subprocess.TimeoutExpired) as failure:
        manifest.update(status="failed", error=str(failure))
        raise
    finally:
        (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--binary", type=Path)
    parser.add_argument(
        "--input", type=Path, help="existing task-owned immutable fixture"
    )
    parser.add_argument(
        "--mode", choices=("run", "smoke", "trace", "observe"), default="run"
    )
    parser.add_argument("--lanes", help="comma-separated catalog names; default all")
    return parser.parse_args()


if __name__ == "__main__":
    collect(arguments())
