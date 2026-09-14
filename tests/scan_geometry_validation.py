"""Reject scan results with unequal useful work or invalid resource witnesses."""

import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "benches/mmap_workloads"))
from scan_collect import matrix, validate_scan
import scan_analyze
import collect
import coalescing


def sample() -> dict:
    return {
        "configuration": "geometry:explicit:256:7:8",
        "config": dict(lane="scan_decode", method="explicit", granule=262144,
                       credits=7, read_limit=8, arena_bytes=8 << 20),
        "debug_assertions": False, "pages": 16384, "useful_bytes": 64 << 20,
        "requests": 256, "elapsed_ns": 100000000, "cpu_ns": 90000000,
        "expected_checksum": 123, "trace": False, "events": [], "flights": [],
        "counters": dict(operations=256, allocations=0, checksum=123,
                         hits=128, pending=128, completed_pending=128),
        "cache_before": dict(cold_resident=0, cold_present_ptes=0, cold_expected=16384),
        "io_mode": "Direct", "registration": "Unregistered",
        "prefetch": dict(reads_in_flight=0, occupied=0, capacity=7, admitted=200,
                         demand_promoted=200, evicted_unused=0, failed=0, automatic_admitted=0),
    }


def coalescing_sample() -> dict:
    row = sample()
    row["configuration"] = "geometry:automatic:4:128:256"
    row["config"].update(method="automatic", granule=4096, credits=128, read_limit=256)
    row["requests"] = 16384
    row["counters"].update(operations=16384, hits=16352, pending=32, completed_pending=32)
    row["prefetch"].update(capacity=128, admitted=16352, demand_promoted=16352,
                           automatic_admitted=16352)
    row["coalescing"] = dict(
        confirmed_window_page=64,
        steady_intervals=[dict(
            pass_index=0, start_page=1024, pages=1024, refills=1,
            reads=[dict(kind="demand", page=1024, pages=1)] + [
                dict(kind="speculative", page=1025 + index * 32, pages=32)
                for index in range(32)
            ],
        )],
        explicit_calls=[],
    )
    return row


class ScanEvidenceContract(unittest.TestCase):
    def test_real_backend_mechanism_capture_retains_consumption_and_full_credit_state(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "pages.bin"
            seed = int.from_bytes(os.urandom(4), "little") + (1 << 32)
            with source.open("wb") as output:
                for page in range(4096):
                    output.write((seed + page).to_bytes(8, "little") * 512)
            binary = collect.build_binary(root)
            capture = root / "mechanisms.json"
            result = subprocess.run(
                [str(binary), "mechanism-capture", str(source), str(capture)],
                text=True, capture_output=True, timeout=60,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            document = json.loads(capture.read_text())
            coalescing.validate_mechanisms(document)
            scenarios = document["raw_scenarios"]
            self.assertGreater(len(scenarios), 0)
            units = "ring_read_sqes" if sys.platform == "linux" else "eager_driver_attempts"
            for scenario in scenarios:
                pages = scenario["consumed_pages"]
                self.assertGreater(len(pages), 0, scenario["scenario"])
                self.assertTrue(all(0 <= page < 4096 for page in pages))
                self.assertEqual(scenario["consumed_bytes"], len(pages) * 4096)
                expected = (sum(seed + page for page in pages) * 512) % (1 << 64)
                self.assertEqual(scenario["checksum"], expected, scenario["scenario"])
                self.assertGreaterEqual(len(scenario["stages"]), 2)
                observation = scenario["stages"][-1]["observation"]
                self.assertEqual(observation["io_units"], units)
                self.assertEqual(observation["capture_interval_pages"], 0)
                self.assertEqual((observation["overflow"], observation["dropped_events"]), (0, 0))
                attempts = [row for row in observation["read_events"] if row["event"] == "attempt"]
                completions = [row for row in observation["read_events"]
                               if row["event"] == "completion"]
                self.assertGreater(len(attempts), 0, scenario["scenario"])
                io = observation["io"]
                self.assertEqual(len(attempts), io["read_sqes"])
                self.assertEqual(len(completions), io["cqes"])
                self.assertEqual(len(attempts), len(completions))
                self.assertEqual(sum(row["bytes"] for row in attempts), io["requested_bytes"])
                self.assertEqual(sum(max(0, row["result"]) for row in completions), io["read_bytes"])
                self.assertEqual((io["terminal_read_credits"], io["terminal_destinations"]), (0, 0))
            for call in document["explicit_calls"]:
                matches = [row for row in scenarios if row["scenario"] == call["scenario"]]
                self.assertEqual(len(matches), 1)
                stages = matches[0]["stages"]
                before = next(row for row in stages if row["stage"] == "before_call")
                self.assertEqual(before["prefetch"]["occupied"], call["capacity"])
                self.assertEqual(before["prefetch"]["capacity"], call["capacity"])
                actual_call = stages[-1]["observation"]["explicit_calls"][-1]
                self.assertEqual(actual_call, {key: call[key] for key in actual_call})

    def test_coalescing_matrix_keeps_both_frozen_budget_comparisons(self):
        try:
            cases = matrix("coalescing", None)
        except ValueError as failure:
            self.fail(f"coalescing gate matrix is unavailable: {failure}")
        expected = set()
        for shape in ("geometry", "pressure"):
            candidate = f"{shape}:automatic:4:128:256"
            for base in (f"{shape}:mmap:4:0:0", f"{shape}:automatic:4:32:64", candidate):
                expected.add((base, candidate))
        self.assertTrue(expected <= {(base, candidate) for _, base, candidate in cases})
        self.assertEqual(len({name for name, _, _ in cases}), len(cases))

    def test_steady_sqe_limit_includes_demand_reads(self):
        row = coalescing_sample()
        validate_scan(row)
        interval = row["coalescing"]["steady_intervals"][0]
        interval["reads"] = [
            dict(kind="demand", page=1024, pages=1),
            dict(kind="demand", page=1025, pages=1),
        ] + [dict(kind="speculative", page=1026 + index * 32, pages=32)
             for index in range(32)]
        with self.assertRaises(ValueError, msg="32 READVs plus two demand READs exceed 33 SQEs"):
            validate_scan(row)

    def test_explicit_control_work_is_bounded_per_call(self):
        row = coalescing_sample()
        row["configuration"] = "geometry:explicit:4:128:256"
        row["config"]["method"] = "explicit"
        row["prefetch"]["automatic_admitted"] = 0
        row["coalescing"]["steady_intervals"] = []
        row["coalescing"]["explicit_calls"] = [dict(
            examined=64, protection_lookups=64, replacement_visits=128,
            admission_visits=32, clock_visits=256,
        )]
        validate_scan(row)
        for field, excessive in (("protection_lookups", 65), ("replacement_visits", 129)):
            with self.subTest(field=field):
                broken = copy.deepcopy(row)
                broken["coalescing"]["explicit_calls"][0][field] = excessive
                with self.assertRaises(ValueError, msg=f"explicit per-call {field} exceeds its bound"):
                    validate_scan(broken)

    def test_host_snapshot_retains_readahead_and_queue_limits(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            device = root / "nvme0n1"
            queue = device / "queue"
            queue.mkdir(parents=True)
            (device / "dev").write_text("259:0\n")
            (queue / "read_ahead_kb").write_text("128\n")
            (queue / "max_sectors_kb").write_text("512\n")
            value = collect.block_queues(root)["nvme0n1"]
            self.assertEqual(value["read_ahead_kb"], "128")
            self.assertEqual(value["max_sectors_kb"], "512")
            self.assertEqual(value["device_number"], "259:0")
            self.assertIsNone(value["max_segments"])

    def test_unwound_setup_and_orphaned_cpu_are_distinct(self):
        lines = ["mmap_workloads::main;scan::read_dios;catalog::consume 100",
                 "mmap_workloads::main;fixture::prepare 50", "syscall;[unknown] 900"]
        counts, excluded, orphaned = scan_analyze.profile_counts(lines, ["scan::read_dios"])
        self.assertEqual(sum(counts.values()), 100)
        self.assertEqual(excluded, 950)
        self.assertEqual(orphaned, 900)

    def test_attribution_rejects_different_executables_or_fixtures(self):
        primary = dict(executable_sha256="primary", fixture={"sha256": "immutable"})
        scan_analyze.match_identity(primary, primary)
        for field, value in [("executable_sha256", "rebuilt"),
                             ("fixture", {"sha256": "different"})]:
            changed = {**primary, field: value}
            with self.assertRaises(ValueError, msg=field):
                scan_analyze.match_identity(changed, primary)

    def test_valid_coarse_scan_has_equal_bytes_despite_fewer_requests(self):
        validate_scan(sample())

    def test_rejects_wrong_bytes_granule_and_unfinished_work(self):
        for field, value in [("requests", 16384), ("useful_bytes", 256), ("expected_checksum", 124)]:
            row = sample()
            row[field] = value
            with self.assertRaises(ValueError, msg=field):
                validate_scan(row)
        for group, field, value in [("config", "granule", 4096), ("prefetch", "reads_in_flight", 1),
                                    ("counters", "allocations", 1), ("cache_before", "cold_resident", 1)]:
            row = sample()
            row[group][field] = value
            with self.assertRaises(ValueError, msg=field):
                validate_scan(row)

    def test_trace_cannot_hide_wrong_addresses_or_excess_depth(self):
        row = sample()
        row["trace"] = True
        row["events"] = [dict(operation=i, page=i * 64, start_ns=i, end_ns=i + 1) for i in range(256)]
        row["flights"] = [dict(at_ns=1, reads=8, speculative=7), dict(at_ns=9999, reads=0, speculative=0)]
        validate_scan(row)
        for group, field, value in [("events", "page", 1), ("flights", "reads", 9), ("flights", "at_ns", 100000001)]:
            broken = copy.deepcopy(row)
            broken[group][0][field] = value
            with self.assertRaises(ValueError, msg=field):
                validate_scan(broken)

    def test_matrices_match_windows_and_bound_geometry_bytes(self):
        for _, base, candidate in matrix("lookahead", None):
            if ":explicit:" in base and ":automatic:" in candidate:
                self.assertEqual(base.split(":")[2:], candidate.split(":")[2:])
        for _, base, candidate in matrix("geometry", None):
            for config in (base, candidate):
                _, _, kib, credits, limit = config.split(":")
                self.assertEqual(int(kib) * 1024 * int(limit), 2 << 20)
                self.assertEqual(int(credits) + 1, int(limit))


if __name__ == "__main__":
    unittest.main()
