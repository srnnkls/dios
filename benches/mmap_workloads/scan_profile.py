"""Collect exact-executable scan profiles outside the workload memory cgroup."""

import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile

from collect import digest
from profile import finish, record, wait_ready
from scan_collect import command, validate_scan
from scan_analyze import read_pairs


def selected_arm(options: argparse.Namespace, manifest: dict) -> dict:
    eligible = {}
    for case in manifest["comparisons"]:
        for position, name in enumerate(("base", "candidate")):
            if case[name] != options.config:
                continue
            arm = case["arms"][position] if "arms" in case else {
                **manifest, "credit_selection": case["rows"][0].get("credit_selection", "override")}
            if options.executable_role and arm.get("executable_role") != options.executable_role:
                continue
            if len(read_pairs(options.primary, case)) != 30:
                raise ValueError("profile arm lacks 30 validated primary pairs")
            eligible[arm["executable_sha256"]] = arm
    if len(eligible) != 1:
        raise ValueError("select exactly one measured identity with --executable-role frozen|candidate")
    return next(iter(eligible.values()))


def profile(options: argparse.Namespace) -> None:
    for tool in ("inferno-collapse-perf", "inferno-flamegraph", "rustfilt"):
        if shutil.which(tool) is None:
            raise ValueError(f"missing profile tool: {tool}")
    manifest = json.loads((options.primary / "manifest.json").read_text())
    if manifest["status"] not in ("complete", "failed") or manifest["mode"] != "run":
        raise ValueError("profile requires measured primary samples")
    if not 1 <= options.repetitions <= 128:
        raise ValueError("excessive profile repetitions")
    arm = selected_arm(options, manifest)
    binary = Path(arm["executable"])
    if digest(binary) != arm["executable_sha256"]:
        raise ValueError("profile executable differs from primary")
    output = options.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    scratch = Path(tempfile.mkdtemp(prefix="dios-scan-perf-", dir="/dev/shm"))
    original = command(binary, Path(manifest["input"]), options.config, "plain", scratch / "unused.json",
                       arm["credit_selection"])
    prefix = original[:original.index(str(binary))]
    if prefix:
        prefix.append("--expand-environment=no")
    wrapper = 'printf "%s\\n" "$$" > "$1"; kill -STOP "$$"; shift; exec "$@"'
    launch = prefix + [shutil.which("bash"), "-c", wrapper, "scan-profile", str(scratch / "pid"),
                       str(binary), "scan-default-profile" if arm["credit_selection"] == "default" else "scan-profile",
                       manifest["input"], options.config,
                       str(options.repetitions), str(output / "samples")]
    event = "cpu-clock" if options.config.startswith("pressure:") else "cycles"
    with (output / "workload.log").open("w") as log:
        workload = subprocess.Popen(launch, stdout=log, stderr=log)
        pid = None
        try:
            pid = wait_ready(scratch / "pid", workload)
            record(options.perf, pid, scratch, event, workload, options.stack_bytes)
        finally:
            if workload.poll() is None:
                if pid is not None:
                    os.kill(pid, signal.SIGKILL)
                workload.wait(timeout=10)
    for path in sorted((output / "samples").glob("*.json")):
        row = json.loads(path.read_text())
        validate_scan(row)
        if arm["credit_selection"] == "default" and row.get("prefetch_credit_selection") != "default":
            raise ValueError("profile replay did not preserve actual-default selection")
    metadata = dict(configuration=options.config, event=event, sample_unit="samples",
                    period=1000000 if event == "cycles" else None,
                    frequency=997 if event == "cpu-clock" else None,
                    repetitions=options.repetitions, executable_sha256=arm["executable_sha256"],
                    credit_selection=arm["credit_selection"],
                    fixture=manifest["fixture"],
                    stack_bytes=options.stack_bytes,
                    recorder_cpu=4, recorder_outside_memory_limit=True, mmap_pages=128,
                    collector_sha256=digest(Path(__file__)), status="recorded")
    finish(options.perf, scratch, output, metadata)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("primary", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("config")
    parser.add_argument("--executable-role", choices=("frozen", "candidate"))
    parser.add_argument("--repetitions", type=int, default=16)
    parser.add_argument("--perf", default="perf")
    parser.add_argument("--stack-bytes", type=int, choices=(16384, 32768, 65528), default=16384)
    profile(parser.parse_args())
