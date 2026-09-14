# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Attach perf outside a benchmark's memory cgroup, then retain matched CPU evidence."""

from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import subprocess
import tempfile
import time

from collect import digest, execute, service_command
from stacks import TIMED_BOUNDARIES, workload_boundary
from validate import validate_row


def stopped_command(
    binary: Path,
    fixture: Path,
    lane: dict,
    arm: str,
    repetitions: int,
    output: Path,
    ready: Path,
) -> list[str]:
    sample = service_command(
        binary, fixture, lane, arm, 0, "plain", output / "unused.json"
    )
    prefix = sample[: sample.index(str(binary))]
    if prefix:
        prefix.append("--expand-environment=no")
    wrapper = 'printf "%s\\n" "$$" > "$1"; kill -STOP "$$"; shift; exec "$@"'
    shell = shutil.which("bash")
    if shell is None:
        raise ValueError("bash unavailable")
    return prefix + [
        shell,
        "-c",
        wrapper,
        "mmap-profile",
        str(ready),
        str(binary),
        "profile",
        str(fixture),
        lane["name"],
        arm,
        str(repetitions),
        str(output / "samples"),
    ]


def wait_ready(path: Path, workload: subprocess.Popen) -> int:
    for _ in range(500):
        if path.exists() and path.read_text().strip():
            pid = int(path.read_text())
            status = Path(f"/proc/{pid}/status").read_text()
            if re.search(r"^State:\s+T", status, re.MULTILINE):
                return pid
        if workload.poll() is not None:
            raise RuntimeError("workload exited before profiler attachment")
        time.sleep(0.02)
    raise TimeoutError("workload did not publish its stopped pid")


def record(
    perf: str, pid: int, scratch: Path, event: str, workload: subprocess.Popen,
    stack_bytes: int = 16384,
) -> None:
    control, acknowledgement = scratch / "control", scratch / "ack"
    os.mkfifo(control)
    os.mkfifo(acknowledgement)
    control_fd = os.open(control, os.O_RDWR | os.O_NONBLOCK)
    ack_fd = os.open(acknowledgement, os.O_RDWR | os.O_NONBLOCK)
    sampling = (
        ["-e", "cycles", "-c", "1000000"]
        if event == "cycles"
        else ["-e", "cpu-clock", "-F", "997"]
    )
    command = [
        "taskset",
        "-c",
        "4",
        perf,
        "record",
        "-p",
        str(pid),
        "--inherit",
        "--delay=-1",
        f"--control=fifo:{control},{acknowledgement}",
        "-m",
        "128",
        "--call-graph",
        f"dwarf,{stack_bytes}",
        "-o",
        str(scratch / "perf.data"),
        *sampling,
    ]
    with (scratch / "record.log").open("w") as log:
        recorder = subprocess.Popen(command, stdout=log, stderr=log)
        try:
            os.write(control_fd, b"enable\n")
            readable, _, _ = select.select([ack_fd], [], [], 15)
            if not readable or os.read(ack_fd, 64).rstrip(b"\x00\r\n") != b"ack":
                raise RuntimeError("perf did not acknowledge enabled events")
            os.kill(pid, signal.SIGCONT)
            result = workload.wait(timeout=180)
            if result:
                raise RuntimeError(f"profile workload exit {result}")
            if recorder.poll() is None:
                recorder.send_signal(signal.SIGINT)
            result = recorder.wait(timeout=30)
            if result not in (0, -signal.SIGINT):
                raise RuntimeError(f"perf exit {result}")
        finally:
            os.close(control_fd)
            os.close(ack_fd)
            if recorder.poll() is None:
                recorder.terminate()
                recorder.wait(timeout=10)


def fold(perf: str, raw: Path, output: Path) -> None:
    commands = [
        [perf, "script", "-i", str(raw), "-F", "-period"],
        ["inferno-collapse-perf"],
        ["rustfilt"],
    ]
    with (output / "collapse.log").open("w") as errors:
        first = subprocess.Popen(commands[0], stdout=subprocess.PIPE, stderr=errors)
        second = subprocess.Popen(
            commands[1], stdin=first.stdout, stdout=subprocess.PIPE, stderr=errors
        )
        first.stdout.close()
        with (output / "profile.folded").open("w") as folded:
            third = subprocess.Popen(
                commands[2], stdin=second.stdout, stdout=folded, stderr=errors
            )
            second.stdout.close()
            for process in (third, second, first):
                if process.wait(timeout=180):
                    raise RuntimeError("perf stack conversion failed")


def workload_view(output: Path) -> int:
    selected, self_counts = Counter(), Counter()
    for line in (output / "profile.folded").read_text().splitlines():
        stack, count = line.rsplit(" ", 1)
        frames = stack.split(";")
        boundary = workload_boundary(frames)
        if boundary is not None:
            selected[";".join(frames[boundary:])] += int(count)
            self_counts[frames[-1]] += int(count)
    total = sum(selected.values())
    (output / "workload.folded").write_text(
        "".join(f"{stack} {count}\n" for stack, count in sorted(selected.items()))
    )
    (output / "top_self.txt").write_text(
        "".join(
            f"{count} {100 * count / total:.2f}% {frame}\n"
            for frame, count in self_counts.most_common(40)
        )
    )
    with (output / "workload.folded").open("r") as source:
        with (output / "workload.svg").open("w") as svg:
            subprocess.run(
                ["inferno-flamegraph", "--title", output.name],
                stdin=source,
                stdout=svg,
                check=True,
                timeout=60,
            )
    return total


def finish(perf: str, scratch: Path, output: Path, metadata: dict) -> None:
    for name in ("perf.data", "record.log"):
        shutil.copyfile(scratch / name, output / name)
    metadata["raw_perf_sha256"] = digest(output / "perf.data")
    metadata["profile_collector_sha256"] = digest(Path(__file__))
    metadata["workload_boundaries"] = TIMED_BOUNDARIES
    metadata["workload_filter_sha256"] = digest(Path(__file__).with_name("stacks.py"))
    statistics = execute(
        [perf, "report", "-i", str(output / "perf.data"), "--stdio", "--stats"],
        timeout=180,
    )
    (output / "perf-statistics.txt").write_text(statistics)
    header = execute([perf, "report", "-i", str(output / "perf.data"), "--header-only"])
    (output / "perf-header.txt").write_text(header)
    counts = {
        name: int(count)
        for name, count in re.findall(r"([A-Z_]+) events:\s*(\d+)", statistics)
    }
    metadata.update(
        lost_events=counts.get("LOST", 0) + counts.get("LOST_SAMPLES", 0),
        throttle_events=counts.get("THROTTLE", 0),
        unthrottle_events=counts.get("UNTHROTTLE", 0),
    )
    (output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    if metadata["lost_events"]:
        raise ValueError(
            "profile lost samples; retained raw evidence requires a new capture"
        )
    fold(perf, output / "perf.data", output)
    total = workload_view(output)
    metadata.update(
        timed_samples=total,
        status="complete" if total >= 100 else "insufficient_samples",
    )
    metadata["sample_sha256"] = {
        path.name: digest(path) for path in sorted((output / "samples").glob("*.json"))
    }
    (output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    if total < 100:
        raise ValueError(
            f"only {total} workload samples; increase replay count within 128"
        )


def profile(options: argparse.Namespace) -> None:
    for tool in ("inferno-collapse-perf", "inferno-flamegraph", "rustfilt"):
        if shutil.which(tool) is None:
            raise ValueError(
                f"profile normalization tool unavailable before capture: {tool}"
            )
    root = options.primary.resolve()
    manifest = json.loads((root / "manifest.json").read_text())
    if manifest["status"] != "complete" or manifest["mode"] != "run":
        raise ValueError("profile requires a completed primary campaign")
    binary = root / Path(manifest["executable"]).name
    if hashlib.sha256(binary.read_bytes()).hexdigest() != manifest["executable_sha256"]:
        raise ValueError("profile executable differs from primary")
    lane = next(
        entry["lane"]
        for entry in manifest["comparisons"]
        if entry["name"] == options.lane
    )
    if options.arm not in lane["arms"] or not 1 <= options.repetitions <= 128:
        raise ValueError("invalid profile arm or repetition count")
    fixture = Path(manifest["input"])
    output = options.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    scratch = Path(tempfile.mkdtemp(prefix="dios-mmap-perf-", dir="/dev/shm"))
    event = "cpu-clock" if lane["cache"] == "pressure" else "cycles"
    metadata = {
        "status": "recording",
        "lane": options.lane,
        "arm": options.arm,
        "sample_unit": "samples",
        "event": event,
        "period": 1_000_000 if event == "cycles" else None,
        "frequency": 997 if event == "cpu-clock" else None,
        "repetitions": options.repetitions,
        "executable_sha256": manifest["executable_sha256"],
        "scratch": str(scratch),
        "recorder_cpu": 4,
        "mmap_pages": 128,
        "recorder_outside_memory_limit": True,
        "record_collector_sha256": digest(Path(__file__)),
    }
    (output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    command = stopped_command(
        binary, fixture, lane, options.arm, options.repetitions, output, scratch / "pid"
    )
    with (output / "workload.log").open("w") as log:
        workload = subprocess.Popen(command, stdout=log, stderr=log)
        pid = None
        try:
            pid = wait_ready(scratch / "pid", workload)
            record(options.perf, pid, scratch, event, workload)
        finally:
            if workload.poll() is None:
                if pid is not None:
                    os.kill(pid, signal.SIGKILL)
                workload.wait(timeout=10)
    for path in sorted((output / "samples").glob("*.json")):
        validate_row(json.loads(path.read_text()))
    finish(options.perf, scratch, output, metadata)


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("primary", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("lane")
    parser.add_argument("arm")
    parser.add_argument("--repetitions", type=int, default=16)
    parser.add_argument("--perf", default="perf")
    return parser.parse_args()


if __name__ == "__main__":
    profile(arguments())
