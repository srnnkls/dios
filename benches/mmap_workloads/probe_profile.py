"""Profile a failed READV mechanism gate using its exact retained executable."""

import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile

from collect import digest, host_snapshot
from profile import finish, record, wait_ready
from probe_collect import validate_probe


def profile(options: argparse.Namespace) -> None:
    for tool in ("inferno-collapse-perf", "inferno-flamegraph", "rustfilt"):
        if shutil.which(tool) is None:
            raise ValueError(f"missing normalization tool: {tool}")
    manifest = json.loads((options.primary / "manifest.json").read_text())
    eligible = {(case[arm], case.get("depth", 1)) for case in manifest["comparisons"]
                for arm in ("base", "candidate")}
    if manifest["status"] != "complete" or manifest["mode"] != "run":
        raise ValueError("probe profile requires completed primary pairs")
    if (options.method, options.depth) not in eligible or not 1 <= options.repetitions <= 32:
        raise ValueError("unmeasured method or excessive replay count")
    binary = Path(manifest["executable"])
    if digest(binary) != manifest["executable_sha256"]:
        raise ValueError("probe profile executable differs")
    output = options.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    host_before = host_snapshot(output, "before")
    scratch = Path(tempfile.mkdtemp(prefix="dios-probe-perf-", dir="/dev/shm"))
    wrapper = 'printf "%s\\n" "$$" > "$1"; kill -STOP "$$"; shift; exec "$@"'
    arguments = [str(binary), "probe-profile", manifest["input"], options.method]
    if "depths" in manifest:
        arguments.append(str(options.depth))
    launch = [shutil.which("bash"), "-c", wrapper, "probe-profile", str(scratch / "pid"),
              *arguments, str(options.repetitions), str(output / "samples")]
    with (output / "workload.log").open("w") as log:
        workload = subprocess.Popen(launch, stdout=log, stderr=log)
        pid = None
        try:
            pid = wait_ready(scratch / "pid", workload)
            record(options.perf, pid, scratch, "cycles", workload, 32768)
        finally:
            if workload.poll() is None:
                if pid is not None:
                    os.kill(pid, signal.SIGKILL)
                workload.wait(timeout=10)
    for path in sorted((output / "samples").glob("*.json")):
        row = json.loads(path.read_text())
        validate_probe(row)
        if row["method"] != options.method or row.get("depth", 1) != options.depth:
            raise ValueError("profile method or depth differs from primary")
    metadata = dict(method=options.method, depth=options.depth, event="cycles", period=1000000, stack_bytes=32768,
                    sample_unit="samples", repetitions=options.repetitions, recorder_cpu=4,
                    executable_sha256=manifest["executable_sha256"], fixture=manifest["fixture"],
                    host_before=host_before, host_after=host_snapshot(output, "after"),
                    collector_sha256=digest(Path(__file__)), status="recorded")
    finish(options.perf, scratch, output, metadata)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("primary", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("method")
    parser.add_argument("--repetitions", type=int, default=8)
    parser.add_argument("--depth", type=int, choices=(1, 8, 16), default=1)
    parser.add_argument("--perf", default="perf")
    profile(parser.parse_args())
