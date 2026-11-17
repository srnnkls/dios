# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Rejection contracts for benchmark evidence, independent of host timing."""

import contextlib
import importlib.util
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
import unittest.mock

REPOSITORY = Path(__file__).resolve().parents[1]
SOURCE = REPOSITORY / "benches/mmap_workloads/validate.py"
SPEC = importlib.util.spec_from_file_location("mmap_validation", SOURCE)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
sys.path.insert(0, str(SOURCE.parent))
ANALYZE = importlib.import_module("analyze")
DIAGNOSE = importlib.import_module("diagnose")
STACKS = importlib.import_module("stacks")
COLLECT = importlib.import_module("collect")

BASE_EXECUTABLE = "b" * 64
CANDIDATE_EXECUTABLE = "c" * 64
FIXTURE_SHA256 = "f" * 64
QUALIFICATION_PAIRS = 2
MEASURED_PAIRS = 30


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


def resident_row(seed: int, elapsed_ns: int) -> dict:
    row = sample()
    row.update(
        lane="resident_access_ordinary",
        arm="dios_serial",
        cache="resident",
        io_mode="Direct",
        registration="Unregistered",
        seed=seed,
        fixture=dict(granule=4096, pages=65536, schema=1, sha256=FIXTURE_SHA256, spare_pages=1),
        runner_sha256="r" * 64,
        trace=False,
        debug_assertions=False,
        elapsed_ns=elapsed_ns,
    )
    row["cache_before"].update(hot_expected=1024, hot_resident=1024, cold_expected=0)
    row["workers"][0].update(major_faults=0, hits=1024, elapsed_ns=elapsed_ns)
    return row


def frozen_manifest() -> dict:
    return dict(
        gate="at-g4",
        lane="resident_access_ordinary",
        arms=["dios_serial", "dios_serial"],
        executables=[BASE_EXECUTABLE, CANDIDATE_EXECUTABLE],
        fixture_sha256=FIXTURE_SHA256,
        qualification_pairs=QUALIFICATION_PAIRS,
        measured_pairs=MEASURED_PAIRS,
    )


def write_base_build_capture(root: Path) -> dict:
    """Writes the rows and manifest a base-build collection leaves behind."""
    capture = root / "capture"
    comparison_directory = capture / "resident_access_ordinary"
    comparison_directory.mkdir(parents=True)
    references = []
    for step in COLLECT.base_build_schedule(QUALIFICATION_PAIRS, MEASURED_PAIRS):
        pair, position = step["pair"], step["position"]
        row = resident_row(seed=pair, elapsed_ns=1_000 + 10 * max(pair, 0) + position)
        raw = json.dumps(row).encode()
        (comparison_directory / step["row"]).write_bytes(raw)
        references.append(
            dict(
                raw=step["row"],
                sha256=hashlib.sha256(raw).hexdigest(),
                pair=pair,
                position=position,
                seed=pair,
                build=step["build"],
                executable_sha256=(BASE_EXECUTABLE, CANDIDATE_EXECUTABLE)[position],
            )
        )
    manifest = dict(
        frozen_manifest(),
        status="complete",
        comparisons=[
            dict(
                name="resident_access_ordinary",
                lane=dict(name="resident_access_ordinary"),
                arms=["dios_serial", "dios_serial"],
                modes=["plain", "plain"],
                rows=references,
            )
        ],
    )
    (capture / "manifest.json").write_text(json.dumps(manifest))
    (root / "frozen.json").write_text(json.dumps(frozen_manifest()))
    return manifest


RESIDENT_LANE = dict(
    name="resident_access_ordinary",
    arms=["mmap_random", "dios_serial"],
    cache="resident",
    operations=1024,
    workers=1,
    useful_bytes_per_read=64,
    frames=1024,
)


def runner_sha256() -> str:
    return hashlib.sha256(
        b"".join((REPOSITORY / path).read_bytes() for path in COLLECT.RUNNER_SOURCES)
    ).hexdigest()


def write_fixture(directory: Path) -> str:
    directory.mkdir()
    pages = b"\0" * 4096
    (directory / "pages.bin").write_bytes(pages)
    sha256 = hashlib.sha256(pages).hexdigest()
    (directory / "fixture.json").write_text(json.dumps(dict(pages=1, sha256=sha256)))
    return sha256


def write_base_tree(directory: Path) -> None:
    for path in COLLECT.RUNNER_SOURCES + ["Cargo.toml", "Cargo.lock"]:
        (directory / path).parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(REPOSITORY / path, directory / path)


def names_tree(command: list[str], working_directory, tree: Path) -> bool:
    if working_directory is not None and Path(working_directory).resolve().is_relative_to(tree):
        return True
    return any(str(tree) in argument for argument in command)


def fake_collector_processes(
    root: Path,
    base_tree: Path,
    fixture_sha256: str,
    samples: list,
    lanes: tuple = (RESIDENT_LANE,),
    shape_row=None,
):
    """Stands in for cargo, the runner and the host tools; records each sample process."""
    built = {}
    runner = runner_sha256()

    def build(name: str) -> str:
        executable = root / f"target-{name}" / "mmap_workloads"
        executable.parent.mkdir(exist_ok=True)
        executable.write_bytes(f"{name} runner".encode())
        built[hashlib.sha256(executable.read_bytes()).hexdigest()] = name
        entry = dict(executable=str(executable), target=dict(name="mmap_workloads"))
        return json.dumps(entry) + "\n"

    def sample_row(command: list[str]) -> str:
        start = command.index("sample")
        executable = Path(command[start - 1])
        lane, arm, seed, tracing, target = command[start + 2:start + 7]
        build_name = built.get(hashlib.sha256(executable.read_bytes()).hexdigest())
        samples.append(dict(build=build_name, arm=arm, seed=int(seed), target=target))
        row = resident_row(seed=int(seed), elapsed_ns=1_000 + len(samples))
        row.update(lane=lane, arm=arm, fixture=fixture_sha256, trace=tracing == "trace")
        if shape_row is not None:
            shape_row(row)
        Path(target).write_text(json.dumps(row))
        return ""

    def run(command, *arguments, cwd=None, **keywords) -> subprocess.CompletedProcess:
        command = [str(argument) for argument in command]
        if command[0] == "cargo":
            name = "base" if names_tree(command, cwd, base_tree) else "candidate"
            stdout = build(name)
        elif "sample" in command:
            stdout = sample_row(command)
        elif command[-1] == "identity":
            stdout = json.dumps(dict(runner_sha256=runner, debug_assertions=False))
        elif command[-1] == "list":
            stdout = json.dumps(list(lanes))
        elif "summarize" in command:
            stdout = json.dumps(dict(ratio=1.0, pairs=MEASURED_PAIRS))
        elif command[0] == "findmnt":
            stdout = "/dev/nvme0n1p2 ext4\n"
        elif command[0] == "git" and "rev-parse" in command:
            stdout = "ade5310" + "0" * 33 + "\n"
        else:
            stdout = ""
        return subprocess.CompletedProcess(command, 0, stdout, "")

    return run


SESSION_LANES = {
    lane: dict(
        name=lane,
        arms=["mmap_random", "dios_session"],
        cache="resident",
        operations=1024,
        workers=1,
        useful_bytes_per_read=bytes_per_read,
        frames=1024,
    )
    for lane, bytes_per_read in (
        ("resident_access_session_projection", 64),
        ("resident_access_session_fold", 4096),
    )
}


def session_row_shape(fixture_identity: dict, bytes_per_read: int):
    """Shapes a fake row as the runner writes it for each arm of a session lane."""

    def shape(row: dict) -> None:
        row["fixture"] = dict(fixture_identity)
        row["useful_bytes"] = row["operations"] * bytes_per_read
        if row["arm"].startswith("mmap_"):
            row.update(io_mode="mmap", registration="none")
            row["workers"][0]["hits"] = 0

    return shape


def collect_session_capture(root: Path, lane: dict) -> Path:
    """Runs the collector over fake processes for one session lane."""
    fixture_sha256 = write_fixture(root / "fixture")
    fixture_identity = json.loads((root / "fixture/fixture.json").read_text())
    with unittest.mock.patch.object(
        sys,
        "argv",
        ["collect.py", str(root / "capture"), "--input", str(root / "fixture"),
         "--lanes", lane["name"]],
    ):
        options = COLLECT.arguments()
    with (
        contextlib.chdir(REPOSITORY),
        unittest.mock.patch.object(
            COLLECT.subprocess,
            "run",
            fake_collector_processes(
                root,
                root / "no-base-build",
                fixture_sha256,
                [],
                lanes=(lane,),
                shape_row=session_row_shape(
                    fixture_identity, lane["useful_bytes_per_read"]
                ),
            ),
        ),
        unittest.mock.patch.object(
            COLLECT, "host_snapshot", return_value=dict(governors=["performance"])
        ),
    ):
        COLLECT.collect(options)
    return root / "capture"


def session_frozen_manifest(capture: Path, lane: dict, gate: str) -> dict:
    """The frozen manifest of a session gate: one executable runs both arms."""
    manifest = json.loads((capture / "manifest.json").read_text())
    executable = manifest["executable_sha256"]
    fixture = json.loads((capture.parent / "fixture/fixture.json").read_text())
    return dict(
        gate=gate,
        lane=lane["name"],
        arms=["mmap_random", "dios_session"],
        executables=[executable, executable],
        fixture_sha256=fixture["sha256"],
        qualification_pairs=QUALIFICATION_PAIRS,
        measured_pairs=MEASURED_PAIRS,
    )


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

    def test_base_build_collection_runs_one_named_process_per_build_in_ab_ba_order(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            base_tree = root / "dios-at-base-ade5310"
            write_base_tree(base_tree)
            fixture_sha256 = write_fixture(root / "fixture")
            samples = []
            with unittest.mock.patch.object(
                sys,
                "argv",
                ["collect.py", str(root / "out"), "--input", str(root / "fixture"),
                 "--lanes", "resident_access_ordinary", "--base-build", str(base_tree)],
            ):
                options = COLLECT.arguments()
            self.assertEqual(options.base_build, base_tree)
            failure = None
            with (
                contextlib.chdir(REPOSITORY),
                unittest.mock.patch.object(
                    COLLECT.subprocess,
                    "run",
                    fake_collector_processes(root, base_tree, fixture_sha256, samples),
                ),
                unittest.mock.patch.object(
                    COLLECT, "host_snapshot", return_value=dict(governors=["performance"])
                ),
            ):
                try:
                    COLLECT.collect(options)
                except (ValueError, RuntimeError, OSError, KeyError) as error:
                    failure = error

            self.assertEqual(
                len(samples),
                2 * (QUALIFICATION_PAIRS + MEASURED_PAIRS),
                f"one sample process per build and pair; collector stopped with {failure!r}",
            )
            self.assertEqual(
                {sample["arm"] for sample in samples},
                {"dios_serial"},
                "both builds run the dios_serial arm",
            )
            for index, (first, second) in enumerate(zip(samples[::2], samples[1::2])):
                pair = index - QUALIFICATION_PAIRS
                expected = ["base", "candidate"] if pair % 2 == 0 else ["candidate", "base"]
                self.assertEqual([first["build"], second["build"]], expected, f"pair {pair}")
                self.assertEqual(first["seed"], second["seed"], f"pair {pair}")
            targets = [sample["target"] for sample in samples]
            self.assertEqual(len(set(targets)), len(targets), "every process names its own row")
            for target in targets:
                self.assertTrue(Path(target).is_file(), target)
            self.assertIsNone(failure)

        schedule = COLLECT.base_build_schedule(QUALIFICATION_PAIRS, MEASURED_PAIRS)
        self.assertEqual(len(schedule), 2 * (QUALIFICATION_PAIRS + MEASURED_PAIRS))
        self.assertEqual(
            len({step["row"] for step in schedule}),
            len(schedule),
            "every process writes its own uniquely named row",
        )
        pairs = list(range(-QUALIFICATION_PAIRS, MEASURED_PAIRS))
        self.assertEqual([step["pair"] for step in schedule[::2]], pairs)
        self.assertEqual([step["pair"] for step in schedule[1::2]], pairs)
        for first, second in zip(schedule[::2], schedule[1::2]):
            expected = (
                ["base", "candidate"] if first["pair"] % 2 == 0 else ["candidate", "base"]
            )
            self.assertEqual([first["build"], second["build"]], expected)
            for step in (first, second):
                self.assertEqual(step["position"], ("base", "candidate").index(step["build"]))

    def test_warm_prepare_pairs_validated_base_build_rows_and_rejects_drift(
        self,
    ) -> None:
        def prepare(root: Path) -> Path:
            output = root / "paired.csv"
            MODULE.warm_prepare(root / "capture", root / "frozen.json", output)
            return output

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            write_base_build_capture(root)
            lines = prepare(root).read_text().splitlines()
            self.assertEqual(lines[0], "base_ns,candidate_ns")
            self.assertEqual(
                lines[1:],
                [f"{1_000 + 10 * pair},{1_001 + 10 * pair}" for pair in range(MEASURED_PAIRS)],
                "measured pairs only, base first, in pair order",
            )

        def rejects(pattern: str, corrupt) -> None:
            with tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                manifest = write_base_build_capture(root)
                corrupt(root, manifest)
                (root / "capture/manifest.json").write_text(json.dumps(manifest))
                with self.assertRaisesRegex(ValueError, pattern):
                    prepare(root)
                self.assertFalse((root / "paired.csv").exists())

        def mixed_identity(root: Path, manifest: dict) -> None:
            reference = manifest["comparisons"][0]["rows"][10]
            reference["executable_sha256"] = (
                CANDIDATE_EXECUTABLE if reference["position"] == 0 else BASE_EXECUTABLE
            )

        def unequal_work(root: Path, manifest: dict) -> None:
            reference = next(
                row for row in manifest["comparisons"][0]["rows"]
                if row["pair"] == 3 and row["position"] == 1
            )
            path = root / "capture/resident_access_ordinary" / reference["raw"]
            row = json.loads(path.read_text())
            row["operations"] = 2048
            row["useful_bytes"] = 131_072
            row["workers"][0].update(operations=2048, hits=2048)
            raw = json.dumps(row).encode()
            path.write_bytes(raw)
            reference["sha256"] = hashlib.sha256(raw).hexdigest()

        def incomplete_pair(root: Path, manifest: dict) -> None:
            rows = manifest["comparisons"][0]["rows"]
            rows.remove(next(row for row in rows if row["pair"] == MEASURED_PAIRS - 1))

        def manifest_drift(root: Path, manifest: dict) -> None:
            manifest["fixture_sha256"] = "e" * 64

        def rewrite_rows(root: Path, manifest: dict, positions: tuple, change) -> None:
            for reference in manifest["comparisons"][0]["rows"]:
                if reference["position"] not in positions:
                    continue
                path = root / "capture/resident_access_ordinary" / reference["raw"]
                row = json.loads(path.read_text())
                change(row)
                raw = json.dumps(row).encode()
                path.write_bytes(raw)
                reference["sha256"] = hashlib.sha256(raw).hexdigest()

        def mmap_candidate(root: Path, manifest: dict) -> None:
            def measure_mmap(row: dict) -> None:
                row["arm"] = "mmap_random"
                row["workers"][0]["hits"] = 0

            manifest["comparisons"][0]["arms"] = ["dios_serial", "mmap_random"]
            rewrite_rows(root, manifest, (1,), measure_mmap)

        def unfrozen_fixture(root: Path, manifest: dict) -> None:
            rewrite_rows(root, manifest, (0, 1), lambda row: row["fixture"].update(sha256="e" * 64))

        rejects("identity", mixed_identity)
        rejects("work", unequal_work)
        rejects("missing|incomplete", incomplete_pair)
        rejects("manifest", manifest_drift)
        rejects("frozen lane or arms", mmap_candidate)
        rejects("frozen fixture", unfrozen_fixture)

    def test_warm_prepare_pairs_one_executable_session_arms_and_rejects_drift(
        self,
    ) -> None:
        for gate, lane in zip(("at-g1", "at-g2"), SESSION_LANES.values()):
            with self.subTest(lane=lane["name"]), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                capture = collect_session_capture(root, lane)
                frozen = root / "frozen.json"
                frozen.write_text(json.dumps(session_frozen_manifest(capture, lane, gate)))
                manifest = json.loads((capture / "manifest.json").read_text())
                comparison = manifest["comparisons"][0]
                self.assertEqual(comparison["arms"], ["mmap_random", "dios_session"])

                elapsed = {}
                for reference in comparison["rows"]:
                    row = json.loads(
                        (capture / lane["name"] / Path(reference["raw"]).name).read_text()
                    )
                    self.assertEqual(row["arm"], comparison["arms"][reference["position"]])
                    if reference["pair"] >= 0:
                        elapsed[(reference["pair"], reference["position"])] = row["elapsed_ns"]
                output = root / "paired.csv"
                try:
                    MODULE.warm_prepare(capture, frozen, output)
                except (ValueError, KeyError) as error:
                    self.fail(f"warm-prepare refused a complete session capture: {error!r}")
                self.assertEqual(
                    output.read_text().splitlines(),
                    ["base_ns,candidate_ns"]
                    + [
                        f"{elapsed[(pair, 0)]},{elapsed[(pair, 1)]}"
                        for pair in range(MEASURED_PAIRS)
                    ],
                    "measured pairs only, the mmap arm as base and the session arm as candidate",
                )

                def rejects(pattern: str, corrupt) -> None:
                    saved = manifest_path.read_text()
                    rows = {path: path.read_bytes() for path in rows_directory.iterdir()}
                    try:
                        corrupted = json.loads(saved)
                        corrupt(corrupted)
                        manifest_path.write_text(json.dumps(corrupted))
                        target = root / f"rejected-{pattern}.csv"
                        with self.assertRaisesRegex(ValueError, pattern):
                            MODULE.warm_prepare(capture, frozen, target)
                        self.assertFalse(target.exists())
                    finally:
                        manifest_path.write_text(saved)
                        for path, raw in rows.items():
                            path.write_bytes(raw)

                manifest_path = capture / "manifest.json"
                rows_directory = capture / lane["name"]

                def rewrite_session_row(manifest: dict, pair: int, change) -> None:
                    reference = next(
                        row for row in manifest["comparisons"][0]["rows"]
                        if row["pair"] == pair and row["position"] == 1
                    )
                    path = rows_directory / Path(reference["raw"]).name
                    row = json.loads(path.read_text())
                    change(row)
                    raw = json.dumps(row).encode()
                    path.write_bytes(raw)
                    reference["sha256"] = hashlib.sha256(raw).hexdigest()

                def mixed_identity(manifest: dict) -> None:
                    manifest["comparisons"][0]["rows"][10]["executable_sha256"] = "d" * 64

                def double_work(row: dict) -> None:
                    row["operations"] = 2048
                    row["useful_bytes"] = 2048 * lane["useful_bytes_per_read"]
                    row["workers"][0].update(operations=2048, hits=2048)

                def other_checksum(row: dict) -> None:
                    row["expected_checksum"] = 72
                    row["workers"][0]["checksum"] = 72

                def other_bytes(row: dict) -> None:
                    row["useful_bytes"] = row["operations"] * 8

                def incomplete_pair(manifest: dict) -> None:
                    rows = manifest["comparisons"][0]["rows"]
                    rows.remove(next(row for row in rows if row["pair"] == MEASURED_PAIRS - 1))

                rejects("identity", mixed_identity)
                rejects("work", lambda manifest: rewrite_session_row(manifest, 3, double_work))
                rejects("checksum", lambda manifest: rewrite_session_row(manifest, 4, other_checksum))
                rejects("useful_bytes", lambda manifest: rewrite_session_row(manifest, 5, other_bytes))
                rejects("missing|incomplete", incomplete_pair)

    def test_minor_fault_regime_rejects_storage_faults(self) -> None:
        row = sample()
        row.update(lane="minor_projection", cache="minor")
        row["workers"][0]["minor_faults"] = 100
        with self.assertRaisesRegex(ValueError, "major"):
            MODULE.validate_row(row)


if __name__ == "__main__":
    unittest.main()
