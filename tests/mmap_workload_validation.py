# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Rejection contracts for benchmark evidence, independent of host timing."""

import importlib.util
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[1] / "benches/mmap_workloads/validate.py"
SPEC = importlib.util.spec_from_file_location("mmap_validation", SOURCE)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
sys.path.insert(0, str(SOURCE.parent))
ANALYZE = importlib.import_module("analyze")
DIAGNOSE = importlib.import_module("diagnose")
STACKS = importlib.import_module("stacks")


def sample() -> dict:
    return {
        "lane": "cold_point",
        "arm": "mmap_random",
        "cache": "cold",
        "operations": 1024,
        "useful_bytes": 65_536,
        "expected_checksum": 71,
        "cache_before": {
            "hot_expected": 0,
            "hot_resident": 0,
            "cold_expected": 1024,
            "cold_resident": 0,
        },
        "elapsed_ns": 100_000,
        "io_mode": "mmap",
        "registration": "none",
        "workers": [
            {
                "operations": 1024,
                "checksum": 71,
                "allocations": 0,
                "minor_faults": 0,
                "major_faults": 1024,
                "pending": 0,
                "hits": 0,
                "completed_pending": 0,
                "cpu_ns": 50_000,
                "elapsed_ns": 100_000,
            }
        ],
    }


def prefetch_sample() -> dict:
    row = sample()
    row.update(arm="dios_automatic", io_mode="Direct", registration="Unregistered")
    row["workers"][0].update(pending=1024, completed_pending=1024)
    row["prefetch"] = dict(
        reads_in_flight=0,
        occupied=2,
        capacity=4,
        admitted=8,
        automatic_admitted=8,
        demand_promoted=5,
        evicted_unused=1,
        failed=0,
    )
    return row


class MmapEvidenceContract(unittest.TestCase):
    def test_cpu_scope_accepts_tail_called_read_loops_and_excludes_fixture_setup(
        self,
    ) -> None:
        self.assertEqual(
            STACKS.workload_boundary(
                ["engine::run_worker", "engine::read_dios", "Pool::get"]
            ),
            1,
        )
        self.assertEqual(
            STACKS.workload_boundary(["engine::timed_workload", "engine::read_mmap"]), 0
        )
        self.assertIsNone(STACKS.workload_boundary(["fixture::warm_page", "Pool::get"]))
        self.assertIsNone(STACKS.workload_boundary(["engine::run_worker", "os::usage"]))

    def test_frozen_runner_changes_require_explicit_matching_identities(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directory = root / "frozen"
            directory.mkdir()
            comparison = dict(
                name="frozen",
                lane=dict(name="cold_point"),
                arms=["mmap_random", "mmap_random"],
                modes=["plain", "plain"],
                rows=[],
            )
            for position, runner in enumerate(("old-runner", "new-runner")):
                row = sample()
                row.update(seed=0, fixture="sealed", runner_sha256=runner, trace=False)
                path = directory / f"{position}.json"
                raw = json.dumps(row).encode()
                path.write_bytes(raw)
                comparison["rows"].append(
                    dict(
                        raw=path.name,
                        sha256=hashlib.sha256(raw).hexdigest(),
                        position=position,
                        pair=0,
                        seed=0,
                    )
                )
            with self.assertRaisesRegex(ValueError, "runner_sha256"):
                ANALYZE.read_comparison(root, comparison)
            comparison["runners"] = ["old-runner", "new-runner"]
            self.assertEqual(len(ANALYZE.read_comparison(root, comparison)), 1)
            comparison["runners"][1] = "unexpected-runner"
            with self.assertRaisesRegex(ValueError, "runner identity"):
                ANALYZE.read_comparison(root, comparison)

    def test_observed_occupancy_uses_time_weighted_levels_and_includes_the_drain(
        self,
    ) -> None:
        events = [
            dict(at_ns=0, reads=0, speculative=0),
            dict(at_ns=10, reads=2, speculative=3),
            dict(at_ns=30, reads=1, speculative=2),
            dict(at_ns=50, reads=0, speculative=1),
        ]
        observed = DIAGNOSE.flight_summary(events, 100)
        self.assertEqual(observed["reads_mean_observed"], 0.6)
        self.assertEqual(observed["reads_maximum_observed"], 2)
        self.assertEqual(observed["speculative_mean_observed"], 1.5)

    def test_credit_accounting_and_complete_drain_are_required(self) -> None:
        MODULE.validate_row(prefetch_sample())
        for field, value in (("reads_in_flight", 1), ("occupied", 5), ("admitted", 9)):
            row = prefetch_sample()
            row["prefetch"][field] = value
            with self.assertRaises(ValueError):
                MODULE.validate_row(row)

    def test_nonsequential_automatic_controls_reject_prediction(self) -> None:
        for lane in ("automatic_fragmented", "automatic_dependent"):
            row = prefetch_sample()
            row["lane"] = lane
            with self.assertRaisesRegex(ValueError, "non-sequential"):
                MODULE.validate_row(row)

    def test_wrong_hint_control_rejects_a_hot_miss(self) -> None:
        row = prefetch_sample()
        row["lane"] = "prefetch_pollution"
        with self.assertRaisesRegex(ValueError, "protected hot set"):
            MODULE.validate_row(row)

    def test_flight_observations_reject_excess_and_reversed_occupancy(self) -> None:
        row = prefetch_sample()
        row["workers"][0].update(
            operations=1, events=[dict(operation=0, page=0, start_ns=0, end_ns=10)]
        )
        for flights in (
            [dict(at_ns=5, reads=65)],
            [dict(at_ns=10, reads=1), dict(at_ns=5, reads=0)],
        ):
            row["workers"][0]["flights"] = flights
            with self.assertRaises(ValueError):
                MODULE.validate_trace(row)

    def test_tracing_requires_complete_operation_spans(self) -> None:
        row = sample()
        row["trace"] = True
        row["workers"][0]["events"] = []
        with self.assertRaisesRegex(ValueError, "trace"):
            MODULE.validate_row(row)

    def test_warm_pages_cannot_be_accepted_as_cold(self) -> None:
        row = sample()
        row["cache_before"]["cold_resident"] = 1
        with self.assertRaisesRegex(ValueError, "cold"):
            MODULE.validate_row(row)

    def test_cold_mmap_requires_observed_major_faults(self) -> None:
        row = sample()
        row["workers"][0]["major_faults"] = 0
        with self.assertRaisesRegex(ValueError, "major"):
            MODULE.validate_row(row)

    def test_allocations_corruption_and_undrained_io_are_rejected(self) -> None:
        for field, value in [("allocations", 1), ("checksum", 72)]:
            row = sample()
            row["workers"][0][field] = value
            with self.assertRaises(ValueError):
                MODULE.validate_row(row)
        row = sample()
        row.update(arm="dios_serial", io_mode="Direct", registration="Unregistered")
        row["workers"][0].update(pending=1024, completed_pending=1023)
        with self.assertRaisesRegex(ValueError, "pending"):
            MODULE.validate_row(row)

    def test_minor_fault_regime_rejects_storage_faults(self) -> None:
        row = sample()
        row.update(lane="minor_projection", cache="minor")
        row["workers"][0]["minor_faults"] = 100
        with self.assertRaisesRegex(ValueError, "major"):
            MODULE.validate_row(row)


if __name__ == "__main__":
    unittest.main()
