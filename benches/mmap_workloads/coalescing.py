"""Measured RC witnesses; the producer contract is in the RC evidence README."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


def count(value: object, name: str, maximum: int = (1 << 63) - 1) -> int:
    if type(value) is not int or not 0 <= value <= maximum:
        raise ValueError(f"invalid coalescing count: {name}")
    return value


def records(value: object, name: str, maximum: int) -> list[dict]:
    if not isinstance(value, list) or len(value) > maximum:
        raise ValueError(f"invalid or excessive coalescing records: {name}")
    if any(not isinstance(row, dict) for row in value):
        raise ValueError(f"invalid coalescing record: {name}")
    return value


def validate_explicit(calls: list[dict], capacity: int) -> None:
    for call in records(calls, "explicit calls", 1 << 20):
        examined = count(call["examined"], "examined")
        if count(call["protection_lookups"], "protection lookups") > examined:
            raise ValueError("explicit protection work exceeds examined prefix")
        if count(call["replacement_visits"], "replacement visits") > capacity:
            raise ValueError("explicit replacement work exceeds one capacity sweep")
        for field in ("admission_visits", "clock_visits"):
            count(call[field], field)


def validate_interval(interval: dict, per_pass: int) -> None:
    start = count(interval["start_page"], "interval start")
    if interval["pages"] != 1024 or not 1024 <= start < per_pass - 1024:
        raise ValueError("steady interval is not an interior 1,024-page witness")
    if count(interval["refills"], "refills") == 0:
        raise ValueError("steady interval lacks a refill cycle")
    reads = records(interval["reads"], "intersecting read SQEs", 1024)
    if not reads or len(reads) > 33:
        raise ValueError("steady interval exceeds 33 READ/READV SQEs")
    covered = []
    for read in reads:
        page = count(read["page"], "read start")
        width = count(read["pages"], "read pages", 32)
        if read["kind"] not in ("demand", "speculative"):
            raise ValueError("steady interval has an unknown read kind")
        if width != (1 if read["kind"] == "demand" else 32):
            raise ValueError("steady interval has a non-full speculative refill")
        if page >= start + 1024 or page + width <= start:
            raise ValueError("SQE does not intersect its witness interval")
        covered.append((max(start, page), min(start + 1024, page + width)))
    cursor = start
    for begin, end in sorted(covered):
        if begin > cursor:
            raise ValueError("steady witness omits a read range")
        cursor = max(cursor, end)
    if cursor != start + 1024:
        raise ValueError("steady witness omits the interval tail")


def validate_scan_witness(row: dict) -> None:
    witness = row.get("coalescing")
    if witness is None:
        return
    config = row["config"]
    passes = 3 if config["lane"] == "pressure_scan" else 1
    intervals = records(witness["steady_intervals"], "steady intervals", passes)
    if config["method"] == "automatic":
        confirmed = count(witness["confirmed_window_page"], "confirmed window page")
        if confirmed >= 1024:
            raise ValueError("automatic window did not confirm within startup")
        if {value["pass_index"] for value in intervals} != set(range(passes)):
            raise ValueError("steady witness must cover every pass exactly once")
        for interval in intervals:
            validate_interval(interval, row["pages"] // passes)
    elif intervals:
        raise ValueError("non-automatic arm contains automatic steady intervals")
    validate_explicit(witness["explicit_calls"], config["credits"])
    if "io" in witness:
        validate_io(witness["io"], row)
    if "control" in witness:
        validate_control(witness["control"])


def validate_lengths(rows: list[dict], name: str) -> int:
    total = 0
    lengths = set()
    for row in records(rows, name, 32):
        width = count(row["pages"], name, 32)
        requests = count(row["requests"], name)
        if width == 0 or requests == 0 or width in lengths:
            raise ValueError("invalid vector length histogram")
        lengths.add(width)
        total += requests
    return total


def validate_io(io: dict, row: dict) -> None:
    fields = ("read_sqes", "demand_read_sqes", "speculative_read_sqes", "continuation_sqes",
              "cqes", "read_bytes", "requested_bytes", "publications", "failures",
              "terminal_read_credits", "terminal_destinations", "eof_sqes", "eof_bytes",
              "beyond_stop_sqes", "beyond_stop_bytes", "short_bytes", "pending_requests_max",
              "pending_bytes_max", "refills_while_pending")
    for field in fields:
        count(io[field], field)
    if io["read_sqes"] != sum(io[key] for key in fields[1:4]):
        raise ValueError("read SQE partition differs")
    if io["cqes"] != io["read_sqes"] or io["terminal_read_credits"] or io["terminal_destinations"]:
        raise ValueError("coalesced reads or destinations did not drain")
    if validate_lengths(io["initial_vector_lengths"], "initial lengths") != io["speculative_read_sqes"]:
        raise ValueError("initial vector counts differ from actual speculative SQEs")
    if validate_lengths(io["continuation_vector_lengths"], "continuation lengths") != io["continuation_sqes"]:
        raise ValueError("continuation vector counts differ from actual SQEs")
    if not row["useful_bytes"] <= io["read_bytes"] <= io["requested_bytes"]:
        raise ValueError("measured read bytes do not cover useful work")
    if io["publications"] < row["requests"]:
        raise ValueError("page publications do not cover useful work")
    for field in ("eof_sqes", "beyond_stop_sqes"):
        if io[field] > io["read_sqes"]:
            raise ValueError("wasted SQEs exceed actual SQEs")
    for field in ("eof_bytes", "beyond_stop_bytes", "short_bytes"):
        if io[field] > io["requested_bytes"]:
            raise ValueError("wasted/short bytes exceed requested bytes")
    if io["pending_bytes_max"] > row["config"]["read_limit"] * row["config"]["granule"]:
        raise ValueError("achieved pending bytes exceed the per-frame read limit")
    if io["pending_requests_max"] > row["config"]["read_limit"]:
        raise ValueError("achieved pending requests exceed the per-frame read limit")


def validate_control(events: list[dict]) -> None:
    for event in records(events, "control work", 1 << 20):
        visits = count(event["entry_visits"], "entry visits")
        affected = count(event["affected_entries"], "affected entries", 32)
        capacity = count(event["capacity"], "control capacity")
        cleanup = count(event["cleanup_entries"], "cleanup entries", capacity)
        if event["cause"] == "idle":
            if visits or affected or cleanup:
                raise ValueError("idle poll visited speculative entries")
        elif event["cause"] in ("completion", "consumption", "invalidation"):
            if visits > affected + cleanup:
                raise ValueError("control work exceeds affected entries and explicit cleanup")
        else:
            raise ValueError("unknown prefetch control event")


def require_scan_measurements(row: dict) -> None:
    witness = row.get("coalescing")
    if not isinstance(witness, dict):
        raise ValueError("missing measured coalescing witness; RC3/RC4 producer is required")
    validate_scan_witness(row)
    validate_io(witness["io"], row)
    validate_control(witness["control"])
    control_entry_visits_total = count(witness.get("control_entry_visits_total"),
                                      "whole-run control entry visits")
    if control_entry_visits_total < sum(event["entry_visits"] for event in witness["control"]):
        raise ValueError("whole-run control entry visits omit captured control work")
    if not witness["control"] or any(event["capacity"] != row["config"]["credits"]
                                     for event in witness["control"]):
        raise ValueError("missing or differently configured prefetch-control capture")
    if count(witness["metadata_bytes"], "metadata bytes") == 0:
        raise ValueError("missing allocation-time metadata byte measurement")
    if witness["overflow"] or witness["dropped_events"]:
        raise ValueError("coalescing observation overflow or sampling loss")


def validate_small_capacity(rows: list[dict]) -> None:
    required = {(1, 64), (8, 64), (32, 64), (32, 33), (128, 33)}
    observed = set()
    for row in records(rows, "small capacity", 32):
        observed.add((row["credits"], row["read_limit"]))
        if row["granule"] != 4096 or (row["credits"], row["read_limit"]) not in required:
            raise ValueError("small-capacity witness geometry differs")
        admitted = count(row["admitted_pages"], "fallback admitted pages")
        if admitted == 0 or row["point_reads"] != admitted or row["vector_reads"]:
            raise ValueError("small-capacity automatic admission is not the point path")
        if row["deferred_with_available_credit"]:
            raise ValueError("small-capacity admission waits for a full vector")
    if observed != required or len(rows) != len(required):
        raise ValueError("small-capacity witness coverage differs")


def validate_turns(events: list[dict]) -> None:
    previous = None
    admitted, deferred = set(), set()
    for event in records(events, "shared-reader turns", 4096):
        ready = event["ready"]
        if ready != [0, 1] or event["stream"] != event["turn_before"]:
            raise ValueError("ready stream did not receive its retained turn")
        before, after = event["turn_before"], event["turn_after"]
        if before not in ready or (previous is not None and before != previous):
            raise ValueError("shared-reader turn skipped between attempts")
        if event["outcome"] == "credit_deferred":
            if after != before or event["admitted_pages"]:
                raise ValueError("credit deferral consumed a reader turn")
            deferred.add(before)
        elif event["outcome"] == "admitted":
            if after != 1 - before or not 1 <= event["admitted_pages"] <= 32:
                raise ValueError("committed run did not advance the round-robin turn")
            admitted.add(before)
        else:
            raise ValueError("unknown shared-reader turn outcome")
        previous = after
    if admitted != {0, 1} or not deferred:
        raise ValueError("shared-reader witness lacks shared admission and credit deferral")


def validate_mechanisms(document: dict) -> None:
    if document["schema"] != 1 or document["overflow"] or document["dropped_events"]:
        raise ValueError("invalid or lossy mechanism capture")
    validate_small_capacity(document["small_capacity"])
    validate_turns(document["shared_reader_turns"])
    control = document["control"]
    validate_control(control)
    if {row["capacity"] for row in control if row["cause"] == "idle"} != {32, 128, 256}:
        raise ValueError("idle-poll capacity coverage differs")
    if not any(row["cause"] == "consumption" and row["after_last_cqe"]
               and row["credits_recovered"] > 0 for row in control):
        raise ValueError("missing post-completion consumption/recovery witness")
    calls = records(document["explicit_calls"], "explicit mechanism calls", 4096)
    scenarios = {call["scenario"] for call in calls}
    if scenarios != {"duplicates", "multiple_runs", "newly_admitted_protected"}:
        raise ValueError("explicit full-credit witness coverage differs")
    for call in calls:
        validate_explicit([call], count(call["capacity"], "explicit capacity"))
        if call["protected_evictions"]:
            raise ValueError("explicit replacement evicted a protected useful hint")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("capture", type=Path)
    validate_mechanisms(json.loads(parser.parse_args().capture.read_text()))
