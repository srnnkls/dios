"""Separate serial submit/observed-ready diagnostics and paired observer controls."""

import argparse
import json
from pathlib import Path

from collect import digest, execute, host_snapshot, prepare, write_comparison
from probe_collect import validate_probe


def metrics(row: dict) -> dict:
    result = dict(elapsed_ns=row["elapsed_ns"], cpu_ns=row["cpu_ns"])
    if row.get("diagnostic") is not None:
        for field in ("submit_ns", "ready_wait_ns"):
            result[field] = sum(group[field] for group in row["diagnostic"]["groups"])
        result["other_ns"] = row["elapsed_ns"] - result["submit_ns"] - result["ready_wait_ns"]
    return result


def comparison(options: argparse.Namespace, binary: Path, fixture: Path, name: str, arms: tuple) -> dict:
    output = options.output.resolve() / name
    output.mkdir()
    repetitions, qualification = (1, 0) if options.mode == "smoke" else (30, 2)
    entries, timings = [], {}
    for pair in range(-qualification, repetitions):
        rows = [None, None]
        for position in ((0, 1) if pair % 2 == 0 else (1, 0)):
            method, command = arms[position]
            target = output / f"{pair:04d}-{position}.json"
            arguments = [str(binary), command, str(fixture), method, str(target)]
            execute(arguments, target.with_suffix(".log"))
            row = json.loads(target.read_text())
            validate_probe(row)
            if row["method"] != method or row["depth"] != 1:
                raise ValueError("clock replay differs from requested serial arm")
            if (row.get("diagnostic") is not None) != (command == "probe-clock-sample"):
                raise ValueError("clock replay instrumentation differs")
            rows[position] = row
            entries.append(dict(pair=pair, position=position, method=method, raw=str(target),
                                sha256=digest(target), command=arguments))
        for field in ("expected_checksum", "useful_bytes", "runner_sha256", "depth", "ring_entries"):
            if rows[0][field] != rows[1][field]:
                raise ValueError(f"clock paired {field} differs")
        values = [metrics(row) for row in rows]
        if pair >= 0:
            for field in values[0].keys() & values[1].keys():
                timings.setdefault(field, []).append([value[field] for value in values])
        if (pair + 1) % 10 == 0:
            print(f"{name}: {pair+1}/{repetitions} pairs", flush=True)
    index = dict(name=name, arms=arms, rows=entries, metrics={})
    if repetitions >= 30:
        for field in sorted(timings):
            directory = output if field == "elapsed_ns" else output / field.removesuffix("_ns")
            directory.mkdir(exist_ok=True)
            index["metrics"][field] = write_comparison(binary, directory, timings[field])
    (output / "index.json").write_text(json.dumps(index, indent=2) + "\n")
    print(name, index["metrics"], flush=True)
    return index


def collect(options: argparse.Namespace) -> None:
    binary, fixture, manifest = prepare(options)
    output = options.output.resolve()
    manifest.update(kind="readv_clock_diagnostic", plan="benches/plans/readv_pipelined.md")
    cases = [("clock-vectored-over-contiguous", (("read_contiguous", "probe-clock-sample"),
                                               ("read_vectored", "probe-clock-sample")))]
    cases += [(f"observer-{method}", ((method, "probe-sample"), (method, "probe-clock-sample")))
              for method in ("read_contiguous", "read_vectored")]
    try:
        for name, arms in cases:
            manifest["comparisons"].append(comparison(options, binary, fixture, name, arms))
        if digest(binary) != manifest["executable_sha256"]:
            raise ValueError("clock executable changed during collection")
        after = host_snapshot(output, "after")
        for field in ("boot_id", "block_queues"):
            if after[field] != manifest["host_before"][field]:
                raise ValueError(f"clock host {field} changed during collection")
        manifest.update(status="complete", host_after=after)
    except Exception as failure:
        manifest.update(status="failed", error=str(failure))
        raise
    finally:
        (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--mode", choices=("smoke", "run"), default="run")
    collect(parser.parse_args())
