"""Storage-only sequential ceilings; fio does not execute the Dios page fold."""

import argparse
import json
from pathlib import Path
import shutil

from collect import digest, execute, host_snapshot, write_comparison


def sample(fixture: Path, granule: int, target: Path) -> tuple[int, dict]:
    depth = (2 << 20) // granule
    command = ["fio", "--name=scan-ceiling", f"--filename={fixture / 'pages.bin'}",
               "--readonly", "--allow_file_create=0", "--rw=read", "--ioengine=io_uring",
               "--direct=1", "--invalidate=1", "--size=256m", "--io_size=768m",
               f"--bs={granule}", f"--iodepth={depth}", "--numjobs=1", "--thread=1",
               "--cpus_allowed=0", "--output-format=json", "--eta=never"]
    target.write_text(execute(command, timeout=180))
    row = json.loads(target.read_text())
    if len(row["jobs"]) != 1:
        raise ValueError("fio worker count differs")
    job = row["jobs"][0]
    read = job["read"]
    if job["error"] or read["io_bytes"] != 768 << 20 or job["write"]["io_bytes"]:
        raise ValueError("fio error or unequal scan bytes")
    if read["total_ios"] != (768 << 20) // granule or read["short_ios"]:
        raise ValueError("fio I/O count or short-read witness differs")
    if read["runtime"] <= 0:
        raise ValueError("fio reported empty timing")
    return read["runtime"] * 1_000_000, {"command": command, "sha256": digest(target),
                                       "granule": granule, "depth": depth, "raw": str(target)}


def collect(options: argparse.Namespace) -> None:
    if shutil.which("fio") is None:
        raise ValueError("fio unavailable")
    output, fixture = options.output.resolve(), options.input.resolve()
    output.mkdir(parents=True, exist_ok=False)
    identity = json.loads((fixture / "fixture.json").read_text())
    if digest(fixture / "pages.bin") != identity["sha256"]:
        raise ValueError("fio fixture identity differs")
    manifest = dict(status="running", kind="fio_scan_ceiling", fixture=identity,
                    fio_version=execute(["fio", "--version"]).strip(),
                    host_before=host_snapshot(output, "before"), comparisons=[],
                    collector_sha256=digest(Path(__file__)),
                    timing="fio read runtime, millisecond resolution; no page consumption kernel")
    try:
        for kib in (64, 256, 1024):
            directory = output / f"granule-{kib}"
            directory.mkdir()
            timings, entries = [], []
            for pair in range(-2, 30):
                measured = [0, 0]
                for position in ((0, 1) if pair % 2 == 0 else (1, 0)):
                    granule = (4096, kib * 1024)[position]
                    elapsed, entry = sample(fixture, granule, directory / f"{pair:04d}-{position}.json")
                    entry.update(pair=pair, position=position)
                    measured[position] = elapsed
                    entries.append(entry)
                if pair >= 0:
                    timings.append(measured)
            index = dict(name=directory.name, rows=entries,
                         **write_comparison(options.binary.resolve(), directory, timings))
            manifest["comparisons"].append(index)
            print(directory.name, index["summary"], flush=True)
        if digest(fixture / "pages.bin") != identity["sha256"]:
            raise ValueError("fio fixture changed during collection")
        manifest.update(status="complete", host_after=host_snapshot(output, "after"))
    except Exception as failure:
        manifest.update(status="failed", error=str(failure))
        raise
    finally:
        (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True, help="retained shared statistics executable")
    collect(parser.parse_args())
