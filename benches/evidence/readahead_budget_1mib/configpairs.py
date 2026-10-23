#!/usr/bin/env python3
"""Advisory scan-runner A/B between two configurations of one executable:
alternating fresh-process pairs; writes base_ns,candidate_ns and a .cpu.csv."""
import json
import subprocess
import sys
import tempfile
from pathlib import Path

FIXTURE = str(Path(__file__).with_name("fixture"))


def sample(binary, verb, config):
    with tempfile.TemporaryDirectory() as tmp:
        out = Path(tmp) / "row.json"
        result = subprocess.run(["taskset", "-c", "0", binary, verb, FIXTURE, config, "plain", str(out)],
                                capture_output=True, text=True, timeout=900)
        if result.returncode:
            raise RuntimeError(f"{config}: {result.stderr[-2000:]}")
        row = json.loads(out.read_text())
    return row["elapsed_ns"], row["cpu_ns"]


def main():
    binary, base_verb, base_config, candidate_verb, candidate_config, pairs, output = sys.argv[1:8]
    arms = {"base": (base_verb, base_config), "candidate": (candidate_verb, candidate_config)}
    rows, cpu = [], []
    for pair in range(int(pairs)):
        order = ("base", "candidate") if pair % 2 == 0 else ("candidate", "base")
        got = {arm: sample(binary, *arms[arm]) for arm in order}
        rows.append((got["base"][0], got["candidate"][0]))
        cpu.append((got["base"][1], got["candidate"][1]))
    out = Path(output)
    out.write_text("base_ns,candidate_ns\n" + "".join(f"{b},{c}\n" for b, c in rows))
    out.with_suffix(".cpu.csv").write_text("base_ns,candidate_ns\n" + "".join(f"{b},{c}\n" for b, c in cpu))
    med = lambda xs: sorted(xs)[len(xs) // 2]
    print(f"{base_config} median {med([r[0] for r in rows])/1e6:.3f} ms, {candidate_config} median {med([r[1] for r in rows])/1e6:.3f} ms")


if __name__ == "__main__":
    main()
