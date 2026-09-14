"""Paired unregistered READ/READV mechanism probe; no pool or allocator change."""

import argparse
import json
from pathlib import Path

from collect import digest, execute, host_snapshot, prepare, write_comparison


def validate_probe(row: dict) -> None:
    requests = {"read_scattered": 32, "read_vectored": 1, "read_contiguous": 1,
                "read_vectored_adjacent": 1}[row["method"]]
    depth = row.get("depth", 1)
    if type(depth) is not int or depth not in (1, 2, 4, 8, 16):
        raise ValueError("probe depth differs from bounded catalog")
    if row["groups"] != 6144 or row["group_bytes"] != 131072 or row["pages"] != 196608:
        raise ValueError("probe work geometry differs")
    if row["useful_bytes"] != 768 << 20 or row["buffer_bytes"] != depth * (384 << 10):
        raise ValueError("probe useful bytes or buffer budget differs")
    if row["debug_assertions"] or row["elapsed_ns"] <= 0 or row["cpu_ns"] <= 0:
        raise ValueError("probe build or timing differs")
    if row["io_mode"] != "Direct" or row["registration"] != "Unregistered":
        raise ValueError("probe buffer posture differs")
    if row["file_registration"] != "Fixed" or row["ring_entries"] != depth * 64:
        raise ValueError("probe ring/file posture differs")
    for field in ("memory_alignment", "offset_alignment"):
        if row[field] <= 0 or 4096 % row[field]:
            raise ValueError("probe alignment witness differs")
    cache, counts = row["cache_before"], row["counters"]
    if cache["cold_resident"] or cache["cold_present_ptes"] or cache["cold_expected"] != 65536:
        raise ValueError("probe input was not cold")
    if counts["submitted"] != requests * row["groups"] or counts["completed"] != counts["submitted"]:
        raise ValueError("probe SQE/CQE count differs")
    if counts["read_bytes"] != row["useful_bytes"] or counts["checksum"] != row["expected_checksum"]:
        raise ValueError("probe byte/consumption witness differs")
    if counts["allocations"] or row["outstanding_end"] or counts["outstanding_max"] != depth * requests:
        raise ValueError("probe allocation or drain witness differs")
    if counts["enter_calls"] < row["groups"] // depth or counts["poll_calls"] < row["groups"] // depth:
        raise ValueError("probe progress counters differ")
    if "depth" in row:
        validate_probe_pipeline(row, depth)
    if row.get("diagnostic") is not None:
        validate_probe_clock(row)


def validate_probe_clock(row: dict) -> None:
    if row.get("depth", 1) != 1 or row["method"] not in ("read_vectored", "read_contiguous"):
        raise ValueError("clock split requires the serial b/c arms")
    groups = row["diagnostic"]["groups"]
    if len(groups) != row["groups"]:
        raise ValueError("clock split group count differs")
    elapsed = 0
    for index, group in enumerate(groups):
        if group["group"] != index:
            raise ValueError("clock split group order differs")
        for field in ("submit_ns", "ready_wait_ns"):
            if type(group[field]) is not int or group[field] <= 0:
                raise ValueError("clock split interval is not positive")
            elapsed += group[field]
    if elapsed >= row["elapsed_ns"]:
        raise ValueError("clock split exceeds enclosing elapsed time")


def validate_probe_pipeline(row: dict, depth: int) -> None:
    counts = row["counters"]
    if row["schema"] != 2 or row["group_bytes_max"] != depth * row["group_bytes"]:
        raise ValueError("probe pipeline byte bound differs")
    if counts["groups_completed"] != row["groups"] or counts["groups_consumed"] != row["groups"]:
        raise ValueError("probe did not complete and consume every group")
    if counts["groups_max"] != depth:
        raise ValueError("probe did not admit configured group depth")
    if not 0 <= counts["refills_while_pending"] <= row["groups"] - depth:
        raise ValueError("probe refill count exceeds work")
    if depth > 1 and counts["refills_while_pending"] == 0:
        raise ValueError("probe has no refill-with-overlap witness")
    if "groups_at_poll" in counts:
        observed = counts["groups_at_poll"]
        if len(observed) != 17 or sum(observed) != counts["poll_calls"] or any(observed[depth + 1:]):
            raise ValueError("probe poll-weighted group observations differ")


def comparison(options: argparse.Namespace, binary: Path, fixture: Path, case: tuple, depth: int) -> dict:
    name, base, candidate = case
    if depth != 1:
        name = f"depth-{depth}-{name}"
    output = options.output.resolve() / name
    output.mkdir()
    pairs, qualification = (1, 0) if options.mode == "smoke" else (30, 2)
    entries, timings = [], []
    for pair in range(-qualification, pairs):
        rows = [None, None]
        for position in ((0, 1) if pair % 2 == 0 else (1, 0)):
            method = (base, candidate)[position]
            target = output / f"{pair:04d}-{position}.json"
            command = [str(binary), "probe-sample", str(fixture), method, str(depth), str(target)]
            execute(command, target.with_suffix(".log"))
            row = json.loads(target.read_text())
            validate_probe(row)
            if row["method"] != method or row["depth"] != depth:
                raise ValueError("probe method or depth differs from request")
            rows[position] = row
            entries.append(dict(pair=pair, position=position, method=method, depth=depth, raw=str(target),
                                sha256=digest(target), command=command))
        for field in ("expected_checksum", "useful_bytes", "runner_sha256", "depth", "buffer_bytes", "ring_entries"):
            if rows[0][field] != rows[1][field]:
                raise ValueError(f"paired probe {field} differs")
        if pair >= 0:
            timings.append([row["elapsed_ns"] for row in rows])
        if (pair + 1) % 10 == 0:
            print(f"{name}: {pair+1}/{pairs} pairs", flush=True)
    index = dict(name=name, base=base, candidate=candidate, depth=depth, rows=entries)
    if pairs >= 30:
        index.update(write_comparison(binary, output, timings))
    (output / "index.json").write_text(json.dumps(index, indent=2) + "\n")
    print(name, index.get("summary", "smoke"), flush=True)
    return index


def collect(options: argparse.Namespace) -> None:
    binary, fixture, manifest = prepare(options)
    output = options.output.resolve()
    depths = [int(value) for value in options.depths.split(",")]
    if len(depths) > 5 or len(set(depths)) != len(depths) or any(value not in (1, 2, 4, 8, 16) for value in depths):
        raise ValueError("probe depths must be distinct members of 1,2,4,8,16")
    plan = "readv_mechanism" if depths == [1] else "readv_pipelined"
    manifest.update(kind="readv_probe", depths=depths, plan=f"benches/plans/{plan}.md")
    try:
        for depth in depths:
            cases = [("vectored-over-contiguous", "read_contiguous", "read_vectored"),
                     ("vectored-over-scattered", "read_scattered", "read_vectored")]
            if depth > 1:
                cases += [("contiguous-over-scattered", "read_scattered", "read_contiguous"),
                          ("adjacent-over-contiguous", "read_contiguous", "read_vectored_adjacent")]
            if options.comparison == "vectored-over-contiguous":
                cases = cases[:1]
            for case in cases:
                manifest["comparisons"].append(comparison(options, binary, fixture, case, depth))
        if digest(binary) != manifest["executable_sha256"]:
            raise ValueError("probe executable changed during collection")
        after = host_snapshot(output, "after")
        for field in ("boot_id", "block_queues"):
            if after[field] != manifest["host_before"][field]:
                raise ValueError(f"probe host {field} changed during collection")
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
    parser.add_argument("--depths", default="1")
    parser.add_argument("--comparison", choices=("all", "vectored-over-contiguous"), default="all")
    collect(parser.parse_args())
