# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Validate mmap/Dios path witnesses before constructing paired comparisons."""

import argparse
import json
import os
from pathlib import Path
from statistics import fmean
import subprocess
import tempfile

HUGEPAGE_LANES = ("resident_access_session_fold_256m",)
HUGEPAGE_PROFILE = "bench-thp-madvise"
SESSION_ARM = "dios_session"
FAIR_CANDIDATE_ARMS = (SESSION_ARM, "dios_get")
FLOOR_ARM = "plain_array"
FLAT_ARM = "mmap_random_flat"
TIMED_REGION_NS_MIN = 100_000_000
PAGE_SIZE_FIELDS = (
    "anonhugepages_kib",
    "filepmdmapped_kib",
    "kernelpagesize_kib",
    "mapping_kib",
    "transparent_hugepage",
    "advice",
    "filesystem",
)


def validate_row(row: dict) -> None:
    """Reject samples whose observed work or residency differs from the contract."""
    workers = row["workers"]
    totals = {
        name: sum(worker.get(name, 0) for worker in workers)
        for name in (
            "operations",
            "allocations",
            "minor_faults",
            "major_faults",
            "pending",
            "completed_pending",
            "hits",
            "retained_reads",
        )
    }
    if totals["allocations"] != 0:
        raise ValueError("timed allocations")
    if totals["operations"] != row["operations"]:
        raise ValueError("operation count differs")
    checksum = sum(worker["checksum"] for worker in workers) % (1 << 64)
    if checksum != row["expected_checksum"]:
        raise ValueError("checksum differs")
    if row["elapsed_ns"] <= 0:
        raise ValueError("empty timing")
    cache = row["cache_before"]
    if cache["cold_resident"] != 0:
        raise ValueError("cold targets were already resident")
    if cache["hot_expected"] != cache["hot_resident"]:
        raise ValueError("hot targets were absent")
    if cache.get("cold_present_ptes", 0) != 0:
        raise ValueError("cold targets had present PTEs")
    if row["cache"] == "minor" and cache.get("hot_present_ptes", 0) != 0:
        raise ValueError("minor targets had present PTEs")
    if row["arm"].startswith("mmap_"):
        validate_mmap(row, totals)
    elif row["arm"] == "plain_array":
        validate_plain(row, totals)
    else:
        validate_dios(row, totals)
    if row["fair"]:
        validate_fair(row, totals)
    if row.get("trace"):
        validate_trace(row)
    if row.get("prefetch") is not None:
        validate_prefetch(row)


def validate_plain(row: dict, totals: dict) -> None:
    if row["io_mode"] != "none" or row["registration"] != "none":
        raise ValueError("plain array posture differs")
    if totals["pending"] != 0 or totals["hits"] != 0:
        raise ValueError("plain array sample contains Dios counters")


def validate_fair(row: dict, totals: dict) -> None:
    """F6 and F7: warm pass, timed-region length, resident faults, page-size witness."""
    warm_pass = row.get("warm_pass")
    if not isinstance(warm_pass, dict) or not warm_pass.get("operations"):
        raise ValueError("fair row lacks its warm-pass witness")
    if row["operations"] % warm_pass["operations"] != 0:
        raise ValueError("timed region is not whole repetitions of the warm pass")
    if row["elapsed_ns"] < TIMED_REGION_NS_MIN:
        raise ValueError("fair timed region is under 100 ms")
    if row["cache"] == "resident" and (totals["major_faults"] or totals["minor_faults"]):
        raise ValueError("resident fair lane faulted in the timed region")
    page_size = row.get("page_size")
    if not isinstance(page_size, dict):
        raise ValueError("fair row lacks its page-size witness")
    for point in ("before", "after"):
        witness = page_size.get(point)
        if not isinstance(witness, dict):
            raise ValueError(f"fair row lacks its page-size witness {point} timing")
        missing = [field for field in PAGE_SIZE_FIELDS if field not in witness]
        if missing:
            raise ValueError(f"page-size witness {point} timing lacks {missing}")


def validate_trace(row: dict) -> None:
    for worker in row["workers"]:
        events = worker["events"]
        if len(events) != worker["operations"]:
            raise ValueError("trace operation count differs")
        if {event["operation"] for event in events} != set(range(worker["operations"])):
            raise ValueError("trace operation coverage differs")
        for event in events:
            if not 0 <= event["start_ns"] <= event["end_ns"] <= row["elapsed_ns"]:
                raise ValueError("trace span lies outside the measured interval")
            if not 0 <= event["page"] < 65_536:
                raise ValueError("trace page lies outside the fixture")
        flights = worker.get("flights", [])
        if any(not 0 <= event["at_ns"] <= row["elapsed_ns"] for event in flights):
            raise ValueError("flight observation lies outside timing")
        if any(a["at_ns"] > b["at_ns"] for a, b in zip(flights, flights[1:])):
            raise ValueError("flight observations are not ordered")
        if any(not 0 <= event["reads"] <= 64 for event in flights):
            raise ValueError("read occupancy exceeds configured capacity")
        if row.get("prefetch") is not None:
            capacity = row["prefetch"]["capacity"]
            if any(not 0 <= event["speculative"] <= capacity for event in flights):
                raise ValueError("observed credits exceed configured capacity")
        if flights and flights[-1]["reads"] != 0:
            raise ValueError("flight observations omit the final drain")


def validate_prefetch(row: dict) -> None:
    stats = row["prefetch"]
    if stats["reads_in_flight"]:
        raise ValueError("accepted reads were not completely drained")
    if not 0 <= stats["occupied"] <= stats["capacity"]:
        raise ValueError("speculative credits exceed capacity")
    outcomes = sum(
        stats[name]
        for name in ("demand_promoted", "evicted_unused", "failed", "occupied")
    )
    if stats["admitted"] != outcomes:
        raise ValueError("speculative credit accounting differs")
    if row["lane"] in ("automatic_fragmented", "automatic_dependent"):
        if stats["automatic_admitted"]:
            raise ValueError("non-sequential demand trained a sequential stream")
    if row["lane"] == "prefetch_pollution":
        if sum(worker["hits"] for worker in row["workers"]) != row["operations"]:
            raise ValueError("wrong hints evicted the protected hot set")


def validate_mmap(row: dict, totals: dict) -> None:
    cache = row["cache"]
    if cache in ("resident", "minor") and totals["major_faults"] != 0:
        raise ValueError("resident/minor lane incurred major faults")
    if cache == "minor" and totals["minor_faults"] == 0:
        raise ValueError("minor lane did not fault")
    if cache in ("cold", "hotspot", "pressure") and totals["major_faults"] == 0:
        raise ValueError("cold mmap lane did not incur major faults")
    if totals["pending"] != 0 or totals["hits"] != 0:
        raise ValueError("mmap sample contains Dios counters")
    if row["lane"] == "pressure_random":
        before = counters(row["system_before"]["memory.stat"])
        after = counters(row["system_after"]["memory.stat"])
        for counter in ("pgscan", "workingset_refault_file"):
            if after.get(counter, 0) <= before.get(counter, 0):
                raise ValueError(f"pressure lane lacks {counter} evidence")
    validate_pressure(row)


def validate_dios(row: dict, totals: dict) -> None:
    if totals["pending"] != totals["completed_pending"]:
        raise ValueError("pending reads not completely drained")
    if (
        totals["hits"] + totals["pending"] + totals["retained_reads"]
        != row["operations"]
    ):
        raise ValueError("Dios hit/pending count differs")
    if row["io_mode"] != "Direct" or row["registration"] != "Unregistered":
        raise ValueError("Dios I/O or registration posture differs")
    if row["cache"] in ("resident", "minor") and totals["pending"] != 0:
        raise ValueError("resident pool incurred misses")
    if row["cache"] in ("cold", "pressure") and totals["pending"] == 0:
        raise ValueError("cold pool had no misses")
    validate_pressure(row)


def validate_pressure(row: dict) -> None:
    if row["cache"] != "pressure":
        return
    for snapshot in (row["system_before"], row["system_after"]):
        if snapshot["memory.max"] != "134217728" or snapshot["memory.swap.max"] != "0":
            raise ValueError("memory limit differs")
        events = counters(snapshot["memory.events"])
        if events.get("oom", 0) or events.get("oom_kill", 0):
            raise ValueError("memory pressure caused OOM")


def warm_prepare(capture: Path, frozen: Path, output: Path) -> None:
    """Pair the validated rows of a frozen capture into the measured CSV.

    A fair-lane capture also holds its floor comparison and a session-lane capture
    its flat comparison; `report.json` beside the CSV reports ns per read per arm
    and each paired difference (F11).
    """
    # Both modules import this one, so they load on first use.
    from analyze import paired_csv
    from collect import base_build_schedule

    if not capture.is_dir() or not frozen.is_file():
        raise ValueError("capture directory and frozen manifest are required")
    expected = json.loads(frozen.read_text())
    manifest = json.loads((capture / "manifest.json").read_text())
    drift = sorted(
        key for key in expected.keys() | {"base_build"}
        if key != "gate" and manifest.get(key) != expected.get(key)
    )
    if drift or manifest["status"] != "complete":
        raise ValueError(f"capture manifest differs from the frozen manifest: {drift}")
    gated = [
        comparison for comparison in manifest["comparisons"]
        if comparison["arms"] == expected["arms"]
    ]
    if len(gated) != 1:
        raise ValueError("capture manifest holds other than one comparison of the frozen lane or arms")
    base_build = "base_build" in expected
    candidate_arm = expected["arms"][1]
    floor_arm = FLOOR_ARM if candidate_arm in FAIR_CANDIDATE_ARMS else None
    flat_arm = FLAT_ARM if candidate_arm == SESSION_ARM else None
    comparisons = (
        gated
        + warm_prepare_reference(manifest, gated[0], expected, "floor_arm", floor_arm)
        + warm_prepare_reference(manifest, gated[0], expected, "flat_arm", flat_arm)
    )
    if len(comparisons) != len(manifest["comparisons"]):
        raise ValueError("capture manifest holds comparisons outside the frozen lane")
    paired = [warm_prepare_pairs(capture, comparison, expected) for comparison in comparisons]
    for comparison, pairs in zip(comparisons, paired):
        warm_prepare_posture(pairs, comparison, expected)
    paths = [output] + [
        output.with_name(f"{comparison['name']}-paired.csv") for comparison in comparisons[1:]
    ]
    builds = {}
    if base_build:
        schedule = base_build_schedule(expected["qualification_pairs"], expected["measured_pairs"])
        builds = {step["position"]: step["build"] for step in schedule}
    staged = []
    published = []
    try:
        outputs = [
            (path, warm_prepare_stage(path, paired_csv(pairs), staged))
            for path, pairs in zip(paths, paired)
        ]
        if floor_arm:
            report = [
                warm_prepare_report(manifest["executable"], comparison, pairs, temporary, builds)
                for comparison, pairs, (_, temporary) in zip(comparisons, paired, outputs)
            ]
            report_path = output.parent / "report.json"
            report_text = json.dumps({"comparisons": report}, indent=2) + "\n"
            outputs.append((report_path, warm_prepare_stage(report_path, report_text, staged)))
        # The gate reads paired.csv, so it appears last, once every other output is in place.
        for path, temporary in outputs[1:] + outputs[:1]:
            temporary.replace(path)
            published.append(path)
    except BaseException:
        for path in staged + published:
            path.unlink(missing_ok=True)
        raise


def warm_prepare_stage(path: Path, text: str, staged: list[Path]) -> Path:
    """Write `text` under a temporary name beside `path`, recorded in `staged` before writing."""
    descriptor, name = tempfile.mkstemp(
        dir=path.parent, prefix=f".{path.name}.", suffix=".partial"
    )
    os.close(descriptor)
    temporary = Path(name)
    staged.append(temporary)
    temporary.write_text(text)
    return temporary


def warm_prepare_reference(
    manifest: dict, gated: dict, expected: dict, field: str, reference_arm: str | None
) -> list[dict]:
    """The floor or flat comparison the frozen arms require; frozen base builds carry none."""
    if reference_arm is None or "base_build" in expected:
        return []
    kind = field.removesuffix("_arm")
    if gated["lane"].get(field) != reference_arm:
        raise ValueError(f"capture lane does not declare its {reference_arm} {kind} arm")
    arms = [reference_arm, expected["arms"][1]]
    found = [comparison for comparison in manifest["comparisons"] if comparison["arms"] == arms]
    if len(found) != 1:
        raise ValueError(f"fair-lane capture lacks its {reference_arm} {kind} comparison")
    return found


def warm_prepare_pairs(capture: Path, comparison: dict, expected: dict) -> list:
    from analyze import read_comparison
    from collect import base_build_schedule

    lanes = comparison.get("lanes", [comparison["lane"]["name"]] * 2)
    if lanes != [expected["lane"]] * 2:
        raise ValueError("comparison lane or arms differ from the frozen lane or arms")
    runners = comparison.get("runners")
    if runners is not None and len(set(runners)) != 1:
        raise ValueError("comparison builds unequal runner hashes")
    schedule = base_build_schedule(expected["qualification_pairs"], expected["measured_pairs"])
    references = comparison["rows"]
    if [(row["pair"], row["position"]) for row in references] != [
        (step["pair"], step["position"]) for step in schedule
    ]:
        raise ValueError("base-build rows are missing, incomplete or reordered")
    for reference in references:
        if reference["executable_sha256"] != expected["executables"][reference["position"]]:
            raise ValueError("row executable identity differs from its build")
    pairs = read_comparison(capture, comparison)
    for pair in pairs:
        for row in pair:
            if row["fixture"]["sha256"] != expected["fixture_sha256"]:
                raise ValueError("row fixture differs from the frozen fixture")
    return pairs


def warm_prepare_posture(pairs: list, comparison: dict, expected: dict) -> None:
    """AT-G3 posture: a hugepage-backed product arena under the THP profile, none otherwise."""
    if expected["lane"] not in HUGEPAGE_LANES:
        return
    hugepage = expected.get("host_profile") == HUGEPAGE_PROFILE
    for pair in pairs:
        for row in pair:
            if not row["arm"].startswith("dios_"):
                continue
            for point in row["page_size"].values():
                full = point["mapping_kib"]
                if hugepage and point["anonhugepages_kib"] != full:
                    raise ValueError("invalid run: arena is not fully hugepage-backed")
                if not hugepage and point["anonhugepages_kib"] != 0:
                    raise ValueError("invalid equal-reach run: arena holds a huge page")


def warm_prepare_report(
    executable: str, comparison: dict, pairs: list, path: Path, builds: dict[int, str]
) -> dict:
    """ns per read per position and the paired difference of one comparison (F11)."""
    result = subprocess.run(
        [executable, "summarize", str(path)],
        text=True, capture_output=True, timeout=190,
    )
    if result.returncode:
        raise ValueError(f"summarize failed: {result.stderr[-3000:]}")
    summary = json.loads(result.stdout)
    operations = pairs[0][0]["operations"]
    arms = comparison["arms"]
    ns_per_read = {}
    for position, role in enumerate(("base", "candidate")):
        entry = {"arm": arms[position]}
        if builds:
            entry["build"] = builds[position]
        entry["ns_per_read"] = fmean(pair[position]["elapsed_ns"] for pair in pairs) / operations
        ns_per_read[role] = entry
    return {
        "name": comparison["name"],
        "arms": arms,
        "ns_per_read": ns_per_read,
        "difference_ns_per_read": {
            "estimate": summary["difference_ns_mean"] / operations,
            "ci95_lower": summary["difference_ns_ci95_lower"] / operations,
            "ci95_upper": summary["difference_ns_ci95_upper"] / operations,
        },
    }


def counters(text: str | None) -> dict[str, int]:
    if text is None:
        return {}
    return {
        key.removesuffix(":"): int(value)
        for key, value in (line.split() for line in text.splitlines())
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prepare = commands.add_parser("warm-prepare", help=warm_prepare.__doc__)
    prepare.add_argument("capture", type=Path)
    prepare.add_argument("frozen", type=Path)
    prepare.add_argument("output", type=Path)
    options = parser.parse_args()
    warm_prepare(options.capture, options.frozen, options.output)


if __name__ == "__main__":
    main()
