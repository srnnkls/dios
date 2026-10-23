#!/usr/bin/env python3
"""Advisory local A/B runner that also retains per-sample path counters.

Same protocol as abpairs.py (alternating fresh-process pairs, seeds pair % 29,
runner-validated checksums), plus a counters CSV with busy/polls/ready_checks.
"""
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

FIXTURE = str(Path(__file__).with_name("fixture"))
KEYS = ("busy", "polls", "ready_checks", "pending", "hits")


def sample(binary, lane, arm, seed):
    with tempfile.TemporaryDirectory() as tmp:
        out = Path(tmp) / "row.json"
        env = dict(os.environ, SCRATCH_NO_CGROUP="1")
        result = subprocess.run([binary, "sample", FIXTURE, lane, arm, str(seed), "plain", str(out)],
                                capture_output=True, text=True, timeout=1800, env=env)
        if result.returncode:
            raise RuntimeError(f"{binary} {lane}/{arm}: {result.stderr[-2000:]}")
        row = json.loads(out.read_text())
    workers = row["workers"]
    counters = {key: sum(w.get(key, 0) for w in workers) for key in KEYS}
    return row["elapsed_ns"], sum(w["cpu_ns"] for w in workers), counters, row["registration"]


def main():
    base, candidate, lane, arm, pairs, output = sys.argv[1:7]
    pairs = int(pairs)
    rows, cpu, counts = [], [], []
    for pair in range(pairs):
        seed = pair % 29
        first, second = (base, candidate) if pair % 2 == 0 else (candidate, base)
        got = {}
        for binary in (first, second):
            got[binary] = sample(binary, lane, arm, seed)
        rows.append((got[base][0], got[candidate][0]))
        cpu.append((got[base][1], got[candidate][1]))
        counts.append((pair, seed, got[base][2], got[candidate][2], got[base][3], got[candidate][3]))
    out = Path(output)
    out.write_text("base_ns,candidate_ns\n" + "".join(f"{b},{c}\n" for b, c in rows))
    out.with_suffix(".cpu.csv").write_text(
        "base_ns,candidate_ns\n" + "".join(f"{b},{c}\n" for b, c in cpu))
    header = "pair,seed," + ",".join(f"base_{k}" for k in KEYS) + "," + ",".join(
        f"candidate_{k}" for k in KEYS) + ",base_registration,candidate_registration\n"
    out.with_suffix(".counters.csv").write_text(header + "".join(
        f"{p},{s}," + ",".join(str(b[k]) for k in KEYS) + "," + ",".join(str(c[k]) for k in KEYS)
        + f",{br},{cr}\n" for p, s, b, c, br, cr in counts))
    med = lambda xs: sorted(xs)[len(xs) // 2]
    print(f"{lane}/{arm}: base median {med([r[0] for r in rows])/1e6:.3f} ms, "
          f"candidate median {med([r[1] for r in rows])/1e6:.3f} ms, "
          f"busy base median {med([c[2]['busy'] for c in counts])} "
          f"candidate median {med([c[3]['busy'] for c in counts])} max {max(c[3]['busy'] for c in counts)}")


if __name__ == "__main__":
    main()
