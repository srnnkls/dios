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

from validate import HUGEPAGE_LANES, HUGEPAGE_PROFILE, validate_row


RUNNER_SOURCES = ["benches/mmap_workloads.rs"] + [
    f"benches/mmap_workloads/{name}" for name in (
        "mod.rs", "catalog.rs", "engine.rs", "fixture.rs", "observe.rs", "os.rs",
        "resident.rs", "scan_config.rs", "scan.rs", "probe.rs", "probe/clock.rs", "fair.rs")
]
# The runner sources of the frozen RC executable, as 604410a defines them.
RUNNER_SOURCES_FROZEN = ["benches/mmap_workloads.rs"] + [
    f"benches/mmap_workloads/{name}" for name in (
        "mod.rs", "catalog.rs", "engine.rs", "fixture.rs", "observe.rs", "os.rs",
        "resident.rs", "scan_config.rs", "scan.rs", "probe.rs", "probe/clock.rs")
]
REPOSITORY = Path(__file__).resolve().parents[2]
LAYOUTS = (16, 32, 64)
HOST_PROFILES = {"bench": "[never]", HUGEPAGE_PROFILE: "[madvise]"}


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


def execute(
    command: list[str],
    log: Path | None = None,
    timeout: int = 190,
    cwd: Path | None = None,
    env: dict[str, str] | None = None,
) -> str:
    result = subprocess.run(
        command, text=True, capture_output=True, timeout=timeout, cwd=cwd, env=env
    )
    if log is not None:
        log.write_text(result.stdout + result.stderr)
    if result.returncode:
        raise RuntimeError(
            f"exit {result.returncode}: {command!r}\n{result.stderr[-3000:]}"
        )
    return result.stdout


def build_binary(
    output: Path,
    tree: Path | None = None,
    env: dict[str, str] | None = None,
    manifest_path: Path | None = None,
) -> Path:
    selection = [] if manifest_path is None else ["--manifest-path", str(manifest_path)]
    text = execute(
        [
            "cargo",
            "build",
            *selection,
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
        cwd=tree,
        env=env,
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


def layout_environment(layout: int | None) -> dict[str, str] | None:
    """The build environment aligning every function at `layout` bytes (F12)."""
    if layout is None:
        return None
    if layout not in LAYOUTS:
        raise ValueError(f"layout must be one of {LAYOUTS}")
    flags = os.environ.get("RUSTFLAGS", "").split()
    flags += ["-C", f"llvm-args=-align-all-functions={layout.bit_length() - 1}"]
    return dict(os.environ, RUSTFLAGS=" ".join(flags))


def build_shared_runner(output: Path, tree: Path, env: dict[str, str] | None) -> Path:
    """Builds this tree's runner sources against the product sources of `tree` (F10).

    The harness path keys on the base tree and the runner inputs alone: cargo bakes the
    manifest path into the executable, so repeated captures must share it to build the
    executable a frozen manifest pins.
    """
    harness_manifest = shared_runner_manifest(tree)
    lock = (REPOSITORY / "Cargo.lock").read_bytes()
    runner = b"".join((REPOSITORY / path).read_bytes() for path in RUNNER_SOURCES)
    key = hashlib.sha256(harness_manifest.encode() + lock + runner).hexdigest()
    harness = tree / "target/shared-runner" / key
    harness.mkdir(parents=True, exist_ok=True)
    if not (harness / "Cargo.lock").exists():
        (harness / "Cargo.lock").write_bytes(lock)
    if not (harness / "Cargo.toml").exists():
        (harness / "Cargo.toml").write_text(harness_manifest)
    environment = dict(env if env is not None else os.environ, DIOS_PRODUCT_WORKTREE=str(tree))
    return build_binary(output, env=environment, manifest_path=harness / "Cargo.toml")


def shared_runner_manifest(tree: Path) -> str:
    return (
        "[package]\n"
        'name = "dios-mmap-workloads-shared-runner"\n'
        'version = "0.0.0"\n'
        'edition = "2024"\n'
        "publish = false\n"
        f"build = {json.dumps(str(REPOSITORY / 'build.rs'))}\n"
        "autobenches = false\n\n"
        "[features]\nbench = []\n\n"
        "[dependencies]\n"
        f'dios = {{ path = {json.dumps(str(tree))}, features = ["bench"] }}\n'
        'serde = { version = "1", features = ["derive"] }\n'
        'serde_json = "1"\nsha2 = "0.10"\nlibc = "0.2"\nio-uring = "0.7"\n\n'
        '[build-dependencies]\nsha2 = "0.10"\n\n'
        '[profile.profiling]\ninherits = "release"\ndebug = true\nstrip = false\n\n'
        "[[bench]]\n"
        'name = "mmap_workloads"\n'
        f"path = {json.dumps(str(REPOSITORY / 'benches/mmap_workloads.rs'))}\n"
        "harness = false\n"
    )


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


def host_snapshot(output: Path, name: str, host_profile: str = "bench") -> dict:
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
    if snapshot["governors"] != ["performance"] or HOST_PROFILES[host_profile] not in snapshot["thp"]:
        raise ValueError("host differs from pinned governor/THP protocol")
    return snapshot


def runner_identity(binary: Path, tree: Path, runner_sources: list[str]) -> str:
    compiled = json.loads(execute([str(binary), "identity"]))
    runner_hash = hashlib.sha256(
        b"".join((tree / path).read_bytes() for path in runner_sources)
    ).hexdigest()
    if compiled["runner_sha256"] != runner_hash or compiled["debug_assertions"]:
        raise ValueError("retained executable does not match current release harness")
    return runner_hash


def retain_binary(binary: Path, output: Path) -> tuple[Path, str]:
    identity = digest(binary)
    saved = output / f"mmap_workloads-{identity}"
    shutil.copy2(binary, saved)
    return saved, identity


def prepare(options: argparse.Namespace, profile: dict | None = None) -> tuple[Path, Path, dict]:
    """Retains the executable and fixture identity and opens the capture manifest.

    `profile` holds the `host_profile` and `layout` a fair-lane capture records; a collector
    without it runs the bench host profile on the default layout and records neither.
    """
    profile = profile or {}
    output = options.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    binary = (
        options.binary.resolve()
        if options.binary
        else build_binary(output, env=layout_environment(profile.get("layout")))
    )
    runner_identity(binary, Path(), RUNNER_SOURCES)
    saved, identity = retain_binary(binary, output)
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
        **profile,
        "host_before": host_snapshot(output, "before", profile.get("host_profile", "bench")),
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


def comparison_pairs(mode: str) -> tuple[int, int]:
    """Qualification and measured pair counts of one comparison."""
    pairs = 30 if mode in ("run", "observe") else 1
    return (2 if pairs == 30 else 0), pairs


def collect_comparison(
    options: argparse.Namespace,
    binaries: list[Path],
    fixture: Path,
    lane: dict,
    arms: list[str],
    modes: list[str],
    name: str,
    runners: list[str] | None = None,
) -> dict:
    output = options.output.resolve() / name
    output.mkdir()
    warmups, pairs = comparison_pairs(options.mode)
    identities = [digest(binary) for binary in binaries]
    schedule = base_build_schedule(warmups, pairs)
    rows, timings = [], []
    for steps in zip(schedule[::2], schedule[1::2]):
        pair = steps[0]["pair"]
        seed = pair if pair >= 0 else 1_000_000 + pair
        measured = [0, 0]
        for step in steps:
            position = step["position"]
            row = collect_one(
                binaries[position],
                fixture,
                lane,
                arms[position],
                seed,
                modes[position],
                output / step["row"],
                options.mode,
            )
            row.update(
                pair=pair, position=position, executable_sha256=identities[position]
            )
            rows.append(row)
            measured[position] = row["elapsed_ns"]
        if pair >= 0:
            timings.append(measured)
        if pair >= 0 and (pair + 1) % 10 == 0:
            print(f"{name}: {pair + 1}/{pairs} pairs", flush=True)
    index = {"lane": lane, "name": name, "arms": arms, "modes": modes, "rows": rows}
    if runners is not None:
        index["runners"] = runners
    if pairs >= 30:
        index.update(write_comparison(binaries[1], output, timings))
    (output / "index.json").write_text(json.dumps(index, indent=2) + "\n")
    return index


def base_build_schedule(qualification_pairs: int, measured_pairs: int) -> list[dict]:
    """One process per build and pair, in AB/BA order, each naming its own row."""
    if qualification_pairs < 0 or measured_pairs < 0:
        raise ValueError("pair counts are non-negative")
    builds = ("base", "candidate")
    schedule = []
    for pair in range(-qualification_pairs, measured_pairs):
        order = (0, 1) if pair % 2 == 0 else (1, 0)
        for position in order:
            schedule.append(
                {
                    "pair": pair,
                    "position": position,
                    "build": builds[position],
                    "row": f"{pair:04d}-{position}.json",
                }
            )
    return schedule


def collect_base_build(
    options: argparse.Namespace, binary: Path, fixture: Path, lane: dict, manifest: dict
) -> dict:
    """Pair the lane's candidate arm in the base-build executable against this one."""
    output = options.output.resolve()
    tree = options.base_build.resolve()
    base_output = output / "base-build"
    base_output.mkdir()
    environment = layout_environment(options.layout)
    if options.shared_runner:
        built = build_shared_runner(base_output, tree, environment)
        base_runner = runner_identity(built, Path(), RUNNER_SOURCES)
    else:
        built = build_binary(base_output, tree, environment)
        base_runner = collect_base_build_runner_identity(built, tree)
    runners = [base_runner, runner_identity(binary, Path(), RUNNER_SOURCES)]
    base, identity = retain_binary(built, base_output)
    arms = [lane["arms"][1]] * 2
    manifest.update(base_build=str(tree))
    record_comparison_identity(
        options, manifest, lane, arms, [identity, manifest["executable_sha256"]]
    )
    return collect_comparison(
        options, [base, binary], fixture, lane, arms, ["plain", "plain"], lane["name"], runners
    )


def collect_base_build_runner_identity(binary: Path, tree: Path) -> str:
    """The base executable's runner hash, identified by one of this collector's runner-source lists."""
    for sources in (RUNNER_SOURCES, RUNNER_SOURCES_FROZEN):
        if all((tree / path).is_file() for path in sources):
            try:
                return runner_identity(binary, tree, sources)
            except ValueError:
                continue
    raise ValueError(
        f"base executable's runner matches neither RUNNER_SOURCES nor RUNNER_SOURCES_FROZEN over {tree}"
    )


def record_comparison_identity(
    options: argparse.Namespace, manifest: dict, lane: dict, arms: list[str], executables: list[str]
) -> None:
    """The identity fields `warm-prepare` checks against the frozen manifest."""
    qualification_pairs, measured_pairs = comparison_pairs(options.mode)
    manifest.update(
        lane=lane["name"],
        arms=arms,
        executables=executables,
        fixture_sha256=manifest["fixture"]["sha256"],
        qualification_pairs=qualification_pairs,
        measured_pairs=measured_pairs,
    )


def write_comparison(binary: Path, output: Path, timings: list) -> dict:
    path = output / "paired.csv"
    with path.open("w", newline="") as stream:
        writer = csv.writer(stream)
        writer.writerow(["base_ns", "candidate_ns"])
        writer.writerows(timings)
    summary = json.loads(execute([str(binary), "summarize", str(path)]))
    (output / "ratio.json").write_text(json.dumps(summary, indent=2) + "\n")
    return {"csv_sha256": digest(path), "summary": summary}


def collect_prepare(options: argparse.Namespace) -> tuple[Path, Path, dict]:
    if options.layout is not None and options.binary:
        raise ValueError("a layout capture builds its own executable")
    if options.shared_runner and not options.base_build:
        raise ValueError("--shared-runner pairs against a --base-build tree")
    if options.base_build and options.mode != "run":
        raise ValueError("a base-build capture runs one lane in run mode")
    return prepare(options, dict(host_profile=options.host_profile, layout=options.layout))


def collect(options: argparse.Namespace) -> None:
    binary, fixture, manifest = collect_prepare(options)
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
        if options.base_build and len(selected) != 1:
            raise ValueError("a base-build capture runs one lane in run mode")
        if options.host_profile != "bench" and set(selected) - set(HUGEPAGE_LANES):
            raise ValueError(f"{options.host_profile} is accepted for {HUGEPAGE_LANES} only")
        for lane in lanes:
            if lane["name"] not in selected:
                continue
            if options.base_build:
                result = collect_base_build(options, binary, fixture, lane, manifest)
                manifest["comparisons"].append(result)
            elif options.mode == "observe":
                for arm in lane["arms"]:
                    result = collect_comparison(
                        options,
                        [binary, binary],
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
                if len(selected) == 1 and options.mode == "run":
                    record_comparison_identity(
                        options, manifest, lane, lane["arms"],
                        [manifest["executable_sha256"]] * 2,
                    )
                result = collect_comparison(
                    options, [binary, binary], fixture, lane, lane["arms"], modes, lane["name"]
                )
                manifest["comparisons"].append(result)
                for reference in ("floor_arm", "flat_arm"):
                    if lane.get(reference):
                        reference_arms = [lane[reference], lane["arms"][1]]
                        result = collect_comparison(
                            options, [binary, binary], fixture, lane, reference_arms, modes,
                            f"{lane['name']}-{lane[reference]}",
                        )
                        manifest["comparisons"].append(result)
            (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
            print(f"{lane['name']}: complete", flush=True)
        if digest(binary) != manifest["executable_sha256"]:
            raise ValueError("executable changed during collection")
        manifest.update(
            status="complete", host_after=host_snapshot(output, "after", options.host_profile)
        )
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
    parser.add_argument(
        "--base-build",
        type=Path,
        help="base-build tree whose runner pairs with this tree's runner",
    )
    parser.add_argument(
        "--shared-runner",
        action="store_true",
        help="build this tree's runner against the --base-build tree instead of its own",
    )
    parser.add_argument(
        "--layout",
        type=int,
        choices=LAYOUTS,
        help="function alignment in bytes of a code-layout build",
    )
    parser.add_argument("--host-profile", choices=tuple(HOST_PROFILES), default="bench")
    return parser.parse_args()


if __name__ == "__main__":
    collect(arguments())
