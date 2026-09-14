# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Validate mmap/Dios path witnesses before constructing paired comparisons."""


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
    else:
        validate_dios(row, totals)
    if row.get("trace"):
        validate_trace(row)
    if row.get("prefetch") is not None:
        validate_prefetch(row)


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


def counters(text: str | None) -> dict[str, int]:
    if text is None:
        return {}
    return {
        key.removesuffix(":"): int(value)
        for key, value in (line.split() for line in text.splitlines())
    }
