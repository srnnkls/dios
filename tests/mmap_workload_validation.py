# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Rejection contracts for benchmark evidence, independent of host timing."""

import contextlib
import importlib.util
import hashlib
import itertools
import json
import os
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
        "fair": False,
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


SEQUENCE_REQUESTS = 65_536
TIMED_REGION_NS_MIN = 100_000_000
THP_ORDINARY = "always madvise [never]"
THP_MADVISE = "always [madvise] never"

FAIR_LANES = {
    lane: dict(
        name=lane,
        arms=["mmap_random", product_arm],
        floor_arm="plain_array",
        **({"flat_arm": "mmap_random_flat"} if product_arm == "dios_session" else {}),
        cache="resident",
        operations=SEQUENCE_REQUESTS * repetitions,
        workers=1,
        useful_bytes_per_read=bytes_per_read,
        frames=pages,
    )
    for lane, product_arm, bytes_per_read, repetitions, pages in (
        ("resident_access_session_projection", "dios_session", 64, 2_048, 1024),
        ("resident_access_session_fold", "dios_session", 4096, 256, 1024),
        ("resident_access_session_fold_256m", "dios_session", 4096, 64, 65_536),
        ("resident_access_fair_get", "dios_get", 64, 2_048, 1024),
    )
}
SESSION_LANES = {
    name: FAIR_LANES[name]
    for name in ("resident_access_session_projection", "resident_access_session_fold")
}


def page_size_witness(row: dict, lane: dict) -> dict:
    """The F7 witness of the mapping one arm reads, as the runner records it."""
    mapping_kib = lane["frames"] * 4
    anonhugepages_kib = 0
    advice = "none"
    if row["arm"].startswith("mmap_"):
        advice = "random"
    elif row["arm"].startswith("dios_") and lane["name"].endswith("_256m"):
        anonhugepages_kib = mapping_kib
    point = dict(
        anonhugepages_kib=anonhugepages_kib,
        filepmdmapped_kib=0,
        kernelpagesize_kib=4,
        mapping_kib=mapping_kib,
        transparent_hugepage=THP_MADVISE if anonhugepages_kib else THP_ORDINARY,
        advice=advice,
        filesystem="ext4",
    )
    return dict(before=dict(point), after=dict(point))


def shape_fair_row(row: dict, lane: dict, elapsed_ns: int) -> None:
    """Shapes a row as the runner writes it for one arm of a fair lane."""
    operations = lane["operations"]
    row.update(
        lane=lane["name"],
        fair=True,
        cache="resident",
        operations=operations,
        useful_bytes=operations * lane["useful_bytes_per_read"],
        elapsed_ns=elapsed_ns,
        warm_pass=dict(operations=SEQUENCE_REQUESTS),
    )
    row["cache_before"].update(
        hot_expected=lane["frames"], hot_resident=lane["frames"], cold_expected=0
    )
    row["workers"][0].update(
        operations=operations,
        hits=operations,
        major_faults=0,
        minor_faults=0,
        elapsed_ns=elapsed_ns,
    )
    if not row["arm"].startswith("dios_"):
        row.update(io_mode="mmap" if row["arm"].startswith("mmap_") else "none")
        row.update(registration="none")
        row["workers"][0]["hits"] = 0
    row["page_size"] = page_size_witness(row, lane)


def fair_row(lane: dict, arm: str, seed: int, elapsed_ns: int) -> dict:
    row = resident_row(seed=seed, elapsed_ns=elapsed_ns)
    row["arm"] = arm
    row["fixture"]["pages"] = 65_536
    shape_fair_row(row, lane, elapsed_ns)
    return row


def session_row_shape(fixture_identity: dict, lane: dict):
    """Shapes a fake row as the runner writes it for each arm of a fair lane."""

    def shape(row: dict) -> None:
        row["fixture"] = dict(fixture_identity)
        shape_fair_row(row, lane, TIMED_REGION_NS_MIN + row["elapsed_ns"])

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
                shape_row=session_row_shape(fixture_identity, lane),
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


FLOOR_ELAPSED_NS = 120_000_000
MMAP_ELAPSED_NS = 150_000_000
FLAT_ELAPSED_NS = 135_000_000
PRODUCT_ELAPSED_NS = 180_000_000
SUMMARIZE_HALF_WIDTH_NS = 1_000


def write_fair_capture(
    root: Path, lane: dict, host_profile: str = "bench", base_build: bool = False
) -> dict:
    """Writes the rows and manifest a fair-lane collection leaves behind.

    The gated comparison pairs the lane's arms; the floor comparison pairs the
    floor arm, and a session lane's flat comparison its flat arm, against the
    product arm. Every arm runs a constant elapsed time, so
    each arm's ns per read and each paired difference are exact. A base-build
    capture holds one comparison of the product arm against itself, the base
    build at `PRODUCT_ELAPSED_NS` and the candidate build at `MMAP_ELAPSED_NS`.
    """
    capture = root / "capture"
    capture.mkdir(parents=True)
    executable = capture / "mmap_workloads-retained"
    executable.write_bytes(b"retained fair runner")
    executable_sha256 = hashlib.sha256(executable.read_bytes()).hexdigest()
    product_arm = lane["arms"][1]
    elapsed = {
        "mmap_random": MMAP_ELAPSED_NS,
        product_arm: PRODUCT_ELAPSED_NS,
        lane["floor_arm"]: FLOOR_ELAPSED_NS,
        "mmap_random_flat": FLAT_ELAPSED_NS,
    }
    gated_arms = [product_arm, product_arm] if base_build else list(lane["arms"])
    plan = [(lane["name"], gated_arms)]
    if not base_build:
        plan.append((f"{lane['name']}-{lane['floor_arm']}", [lane["floor_arm"], product_arm]))
        if "flat_arm" in lane:
            plan.append((f"{lane['name']}-{lane['flat_arm']}", [lane["flat_arm"], product_arm]))
    comparisons = []
    for name, arms in plan:
        directory = capture / name
        directory.mkdir()
        references = []
        for step in COLLECT.base_build_schedule(QUALIFICATION_PAIRS, MEASURED_PAIRS):
            pair, position = step["pair"], step["position"]
            arm = arms[position]
            elapsed_ns = (PRODUCT_ELAPSED_NS, MMAP_ELAPSED_NS)[position] if base_build else elapsed[arm]
            row = fair_row(lane, arm, seed=max(pair, 0), elapsed_ns=elapsed_ns)
            raw = json.dumps(row).encode()
            (directory / step["row"]).write_bytes(raw)
            reference = dict(
                raw=step["row"],
                sha256=hashlib.sha256(raw).hexdigest(),
                pair=pair,
                position=position,
                seed=max(pair, 0),
                executable_sha256=executable_sha256,
            )
            if base_build:
                reference["build"] = step["build"]
            references.append(reference)
        comparisons.append(
            dict(name=name, lane=dict(lane), arms=list(arms), modes=["plain", "plain"],
                 rows=references)
        )
    frozen = dict(
        gate="at-gN",
        lane=lane["name"],
        arms=gated_arms,
        executables=[executable_sha256, executable_sha256],
        fixture_sha256=FIXTURE_SHA256,
        qualification_pairs=QUALIFICATION_PAIRS,
        measured_pairs=MEASURED_PAIRS,
        host_profile=host_profile,
        **({"base_build": str(root / "base-tree")} if base_build else {}),
    )
    manifest = dict(
        frozen,
        status="complete",
        executable=str(executable),
        executable_sha256=executable_sha256,
        comparisons=comparisons,
    )
    (capture / "manifest.json").write_text(json.dumps(manifest))
    (root / "frozen.json").write_text(json.dumps(frozen))
    return manifest


def rewrite_capture_rows(root: Path, manifest: dict, change, comparison_index: int = 0) -> None:
    comparison = manifest["comparisons"][comparison_index]
    for reference in comparison["rows"]:
        path = root / "capture" / comparison["name"] / reference["raw"]
        row = json.loads(path.read_text())
        change(row)
        raw = json.dumps(row).encode()
        path.write_bytes(raw)
        reference["sha256"] = hashlib.sha256(raw).hexdigest()
    (root / "capture/manifest.json").write_text(json.dumps(manifest))


def fake_summarize(command, *arguments, **keywords) -> subprocess.CompletedProcess:
    """Stands in for the shared harness's `summarize` over the paired CSV it is given."""
    command = [str(argument) for argument in command]
    if "summarize" not in command:
        raise AssertionError(f"warm-prepare runs only summarize: {command!r}")
    lines = Path(command[-1]).read_text().splitlines()
    pairs = [tuple(int(value) for value in line.split(",")) for line in lines[1:]]
    differences = [candidate - base for base, candidate in pairs]
    mean = sum(differences) / len(differences)
    ratio = sum(candidate / base for base, candidate in pairs) / len(pairs)
    summary = dict(
        ratio_orientation="candidate/base",
        pairs=len(pairs),
        ratio_geomean=ratio,
        ci95_upper=ratio,
        difference_ns_mean=mean,
        difference_ns_ci95_lower=mean - SUMMARIZE_HALF_WIDTH_NS,
        difference_ns_ci95_upper=mean + SUMMARIZE_HALF_WIDTH_NS,
        kind="characterization",
        adopted_bound=None,
    )
    return subprocess.CompletedProcess(command, 0, json.dumps(summary), "")


def collector_options(argv: list[str]):
    with unittest.mock.patch.object(sys, "argv", ["collect.py"] + argv):
        try:
            return COLLECT.arguments()
        except SystemExit as refusal:
            raise AssertionError(f"collector refuses {argv}") from refusal


def refuse_any_process(command, *arguments, **keywords):
    raise AssertionError(f"a refused option starts no process: {command!r}")


def run_collector(options, run) -> Exception | None:
    """Runs the collector over faked processes; returns the failure it stopped with."""
    with (
        contextlib.chdir(REPOSITORY),
        unittest.mock.patch.object(COLLECT.subprocess, "run", run),
        unittest.mock.patch.object(
            COLLECT, "host_snapshot", return_value=dict(governors=["performance"])
        ),
    ):
        try:
            COLLECT.collect(options)
        except (ValueError, RuntimeError, OSError, KeyError) as error:
            return error
    return None


class MmapEvidenceContract(unittest.TestCase):
    def accepts_row(self, row: dict) -> None:
        try:
            MODULE.validate_row(row)
        except (ValueError, KeyError) as error:
            self.fail(f"{row['lane']}/{row['arm']}: a conforming fair row is refused: {error!r}")

    def accepts_capture(self, root: Path) -> None:
        try:
            MODULE.warm_prepare(root / "capture", root / "frozen.json", root / "paired.csv")
        except (ValueError, KeyError) as error:
            self.fail(f"warm-prepare refused a complete fair capture: {error!r}")

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
                    with unittest.mock.patch.object(subprocess, "run", fake_summarize):
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
                        with (
                            unittest.mock.patch.object(subprocess, "run", fake_summarize),
                            self.assertRaisesRegex(ValueError, pattern),
                        ):
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
                    doubled = 2 * row["operations"]
                    row["operations"] = doubled
                    row["useful_bytes"] = doubled * lane["useful_bytes_per_read"]
                    row["workers"][0].update(operations=doubled, hits=doubled)

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


    def test_fair_rows_require_the_warm_pass_a_100_ms_timed_region_and_no_resident_faults(
        self,
    ) -> None:
        lane = FAIR_LANES["resident_access_session_projection"]

        def rejects(arm: str, corrupt, reason: str) -> None:
            row = fair_row(lane, arm, seed=0, elapsed_ns=MMAP_ELAPSED_NS)
            corrupt(row)
            with self.assertRaises(ValueError, msg=f"{arm}: {reason}"):
                MODULE.validate_row(row)

        def short_timed_region(row: dict) -> None:
            row["elapsed_ns"] = TIMED_REGION_NS_MIN - 1
            row["workers"][0]["elapsed_ns"] = TIMED_REGION_NS_MIN - 1

        for arm in ("mmap_random", "mmap_random_flat", "dios_session", "plain_array"):
            self.accepts_row(fair_row(lane, arm, seed=0, elapsed_ns=MMAP_ELAPSED_NS))
            rejects(arm, lambda row: row.pop("warm_pass"), "no warm-pass witness")
            rejects(arm, short_timed_region, "timed region under 100 ms")
            rejects(
                arm,
                lambda row: row["workers"][0].update(minor_faults=1),
                "minor-fault delta in the timed region of a resident lane",
            )
        rejects(
            "dios_session",
            lambda row: row["workers"][0].update(major_faults=1),
            "major fault in the timed region of a resident lane",
        )

    def test_fair_rows_require_the_page_size_witness_per_arm(self) -> None:
        lane = FAIR_LANES["resident_access_fair_get"]
        for arm in ("mmap_random", "dios_get"):
            self.accepts_row(fair_row(lane, arm, seed=0, elapsed_ns=MMAP_ELAPSED_NS))
            row = fair_row(lane, arm, seed=0, elapsed_ns=MMAP_ELAPSED_NS)
            del row["page_size"]
            with self.assertRaises(ValueError, msg=f"{arm} without its page-size witness"):
                MODULE.validate_row(row)
            for point in ("before", "after"):
                for field in (
                    "anonhugepages_kib",
                    "filepmdmapped_kib",
                    "kernelpagesize_kib",
                    "transparent_hugepage",
                    "advice",
                    "filesystem",
                ):
                    row = fair_row(lane, arm, seed=0, elapsed_ns=MMAP_ELAPSED_NS)
                    del row["page_size"][point][field]
                    with self.assertRaises(
                        ValueError, msg=f"{arm} without {field} {point} the timed region"
                    ):
                        MODULE.validate_row(row)

    def test_warm_prepare_reports_ns_per_read_and_requires_the_floor_and_flat_comparisons(
        self,
    ) -> None:
        lane = FAIR_LANES["resident_access_session_projection"]
        operations = lane["operations"]
        with (
            tempfile.TemporaryDirectory() as temporary,
            unittest.mock.patch.object(subprocess, "run", fake_summarize),
        ):
            root = Path(temporary)
            write_fair_capture(root, lane)
            output = root / "paired.csv"
            self.accepts_capture(root)
            self.assertEqual(
                output.read_text().splitlines(),
                ["base_ns,candidate_ns"]
                + [f"{MMAP_ELAPSED_NS},{PRODUCT_ELAPSED_NS}"] * MEASURED_PAIRS,
                "paired.csv keeps the gated pairs in base_ns,candidate_ns form",
            )
            report = json.loads((root / "report.json").read_text())
            comparisons = {
                tuple(comparison["arms"]): comparison for comparison in report["comparisons"]
            }
            self.assertEqual(
                len(report["comparisons"]), 3, "report.json holds exactly three comparisons"
            )
            self.assertEqual(
                set(comparisons),
                {
                    ("mmap_random", "dios_session"),
                    ("plain_array", "dios_session"),
                    ("mmap_random_flat", "dios_session"),
                },
                "report.json holds the gated, the floor and the flat comparison",
            )
            for (base, candidate), base_ns, candidate_ns in (
                (("mmap_random", "dios_session"), MMAP_ELAPSED_NS, PRODUCT_ELAPSED_NS),
                (("plain_array", "dios_session"), FLOOR_ELAPSED_NS, PRODUCT_ELAPSED_NS),
                (("mmap_random_flat", "dios_session"), FLAT_ELAPSED_NS, PRODUCT_ELAPSED_NS),
            ):
                comparison = comparisons[(base, candidate)]
                self.assertEqual(
                    comparison["ns_per_read"],
                    dict(
                        base=dict(arm=base, ns_per_read=base_ns / operations),
                        candidate=dict(arm=candidate, ns_per_read=candidate_ns / operations),
                    ),
                    "ns per read is keyed by position, each entry naming its arm",
                )
                difference = comparison["difference_ns_per_read"]
                expected = (candidate_ns - base_ns) / operations
                half_width = SUMMARIZE_HALF_WIDTH_NS / operations
                self.assertAlmostEqual(difference["estimate"], expected, places=9)
                self.assertAlmostEqual(
                    difference["ci95_lower"], expected - half_width, places=9
                )
                self.assertAlmostEqual(
                    difference["ci95_upper"], expected + half_width, places=9
                )

        with (
            tempfile.TemporaryDirectory() as temporary,
            unittest.mock.patch.object(subprocess, "run", fake_summarize),
        ):
            for (missing_arm, field, pattern), undeclared in itertools.product(
                (("plain_array", "floor_arm", "floor"), ("mmap_random_flat", "flat_arm", "flat")),
                (False, True),
            ):
                with self.subTest(missing=missing_arm, undeclared=undeclared):
                    root = Path(temporary) / f"{missing_arm}-{undeclared}"
                    manifest = write_fair_capture(root, lane)
                    manifest["comparisons"] = [
                        comparison for comparison in manifest["comparisons"]
                        if comparison["arms"][0] != missing_arm
                    ]
                    if undeclared:
                        for comparison in manifest["comparisons"]:
                            comparison["lane"].pop(field)
                    (root / "capture/manifest.json").write_text(json.dumps(manifest))
                    with self.assertRaisesRegex(ValueError, pattern):
                        MODULE.warm_prepare(
                            root / "capture", root / "frozen.json", root / "paired.csv"
                        )
                    self.assertFalse((root / "paired.csv").exists())
                    self.assertFalse((root / "report.json").exists())

            for missing_arms in (
                {"plain_array"}, {"mmap_random_flat"}, {"plain_array", "mmap_random_flat"}
            ):
                with self.subTest(self_declared_base_build=sorted(missing_arms)):
                    root = Path(temporary) / f"self-declared-{'-'.join(sorted(missing_arms))}"
                    manifest = write_fair_capture(root, lane)
                    manifest["base_build"] = str(root / "base-tree")
                    manifest["comparisons"] = [
                        comparison for comparison in manifest["comparisons"]
                        if comparison["arms"][0] not in missing_arms
                    ]
                    for comparison in manifest["comparisons"]:
                        for field in ("floor_arm", "flat_arm"):
                            comparison["lane"].pop(field)
                        for reference in comparison["rows"]:
                            reference["build"] = ("base", "candidate")[reference["position"]]
                    (root / "capture/manifest.json").write_text(json.dumps(manifest))
                    with self.assertRaisesRegex(ValueError, "base_build"):
                        MODULE.warm_prepare(
                            root / "capture", root / "frozen.json", root / "paired.csv"
                        )
                    self.assertFalse((root / "paired.csv").exists())
                    self.assertFalse((root / "report.json").exists())

    def test_warm_prepare_reports_both_builds_of_a_base_build_capture_with_equal_arms(
        self,
    ) -> None:
        lane = FAIR_LANES["resident_access_fair_get"]
        operations = lane["operations"]
        with (
            tempfile.TemporaryDirectory() as temporary,
            unittest.mock.patch.object(subprocess, "run", fake_summarize),
        ):
            root = Path(temporary)
            write_fair_capture(root, lane, base_build=True)
            self.accepts_capture(root)
            report = json.loads((root / "report.json").read_text())
            [comparison] = report["comparisons"]
            self.assertEqual(comparison["arms"], ["dios_get", "dios_get"])
            self.assertEqual(
                comparison["ns_per_read"],
                dict(
                    base=dict(
                        arm="dios_get", build="base",
                        ns_per_read=PRODUCT_ELAPSED_NS / operations,
                    ),
                    candidate=dict(
                        arm="dios_get", build="candidate",
                        ns_per_read=MMAP_ELAPSED_NS / operations,
                    ),
                ),
                "the candidate of an equal-arm capture keeps the base entry",
            )

    def test_rows_carry_the_runner_fair_flag_and_validation_branches_on_it(self) -> None:
        row = sample()
        del row["fair"]
        with self.assertRaises((ValueError, KeyError), msg="a row without the fair flag"):
            MODULE.validate_row(row)

        lane = dict(FAIR_LANES["resident_access_fair_get"], name="resident_access_fair_next")
        for arm in ("mmap_random", "dios_get", "plain_array"):
            self.accepts_row(fair_row(lane, arm, seed=0, elapsed_ns=MMAP_ELAPSED_NS))
            row = fair_row(lane, arm, seed=0, elapsed_ns=MMAP_ELAPSED_NS)
            row.pop("warm_pass")
            with self.assertRaises(
                ValueError, msg=f"{arm}: a fair row of a lane the validator never named"
            ):
                MODULE.validate_row(row)

    def test_collector_takes_the_hugepage_lanes_and_profile_from_the_validator(self) -> None:
        validator = importlib.import_module("validate")
        self.assertIs(COLLECT.HUGEPAGE_LANES, validator.HUGEPAGE_LANES)
        self.assertIs(COLLECT.HUGEPAGE_PROFILE, validator.HUGEPAGE_PROFILE)
        self.assertIn(validator.HUGEPAGE_PROFILE, COLLECT.HOST_PROFILES)

    def test_warm_prepare_rejects_a_base_build_capture_with_unequal_runner_hashes(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = write_base_build_capture(root)
            comparison = manifest["comparisons"][0]
            runners = ["a" * 64, "r" * 64]
            comparison["runners"] = runners
            for reference in comparison["rows"]:
                path = root / "capture/resident_access_ordinary" / reference["raw"]
                row = json.loads(path.read_text())
                row["runner_sha256"] = runners[reference["position"]]
                raw = json.dumps(row).encode()
                path.write_bytes(raw)
                reference["sha256"] = hashlib.sha256(raw).hexdigest()
            (root / "capture/manifest.json").write_text(json.dumps(manifest))
            with self.assertRaisesRegex(ValueError, "runner"):
                MODULE.warm_prepare(root / "capture", root / "frozen.json", root / "paired.csv")
            self.assertFalse((root / "paired.csv").exists())

    def test_fold_256m_rows_follow_the_hugepage_posture_of_their_manifest(self) -> None:
        lane = FAIR_LANES["resident_access_session_fold_256m"]

        def set_arena(anonhugepages_kib: int):
            def change(row: dict) -> None:
                if row["arm"] != "dios_session":
                    return
                for point in row["page_size"].values():
                    point["anonhugepages_kib"] = anonhugepages_kib
                    point["transparent_hugepage"] = (
                        THP_MADVISE if anonhugepages_kib else THP_ORDINARY
                    )

            return change

        full_kib = lane["frames"] * 4
        for host_profile, accepted, refused, reason in (
            ("bench-thp-madvise", full_kib, full_kib - 2048, "arena not fully hugepage-backed"),
            ("bench", 0, 2048, "equal-reach arena holding a huge page"),
        ):
            with (
                self.subTest(host_profile=host_profile),
                tempfile.TemporaryDirectory() as temporary,
                unittest.mock.patch.object(subprocess, "run", fake_summarize),
            ):
                root = Path(temporary)
                manifest = write_fair_capture(root, lane, host_profile=host_profile)
                for index in range(len(manifest["comparisons"])):
                    rewrite_capture_rows(root, manifest, set_arena(accepted), index)
                self.accepts_capture(root)
                (root / "paired.csv").unlink()
                (root / "report.json").unlink(missing_ok=True)

                rewrite_capture_rows(root, manifest, set_arena(refused), 0)
                with self.assertRaisesRegex(ValueError, "huge|invalid", msg=reason):
                    MODULE.warm_prepare(
                        root / "capture", root / "frozen.json", root / "paired.csv"
                    )
                self.assertFalse((root / "paired.csv").exists())

    def test_collector_layout_builds_record_their_function_alignment(self) -> None:
        def build_flags(command: list[str], keywords: dict) -> tuple:
            environment = keywords.get("env") or os.environ
            return tuple(command) + tuple(
                f"{key}={value}" for key, value in sorted(environment.items())
                if "RUSTFLAGS" in key
            )

        builds = {}
        for layout in (16, 32, 64):
            with self.subTest(layout=layout), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                lane = FAIR_LANES["resident_access_session_projection"]
                fixture_sha256 = write_fixture(root / "fixture")
                fixture_identity = json.loads((root / "fixture/fixture.json").read_text())
                fake = fake_collector_processes(
                    root, root / "no-base-build", fixture_sha256, [], lanes=(lane,),
                    shape_row=session_row_shape(fixture_identity, lane),
                )
                flags = []

                def run(command, *arguments, **keywords):
                    if str(command[0]) == "cargo":
                        flags.append(build_flags([str(part) for part in command], keywords))
                    return fake(command, *arguments, **keywords)

                options = collector_options(
                    [str(root / "capture"), "--input", str(root / "fixture"),
                     "--lanes", lane["name"], "--layout", str(layout)]
                )
                run_collector(options, run)
                manifest = json.loads((root / "capture/manifest.json").read_text())
                self.assertEqual(manifest.get("layout"), layout, "manifest records the layout")
                self.assertEqual(len(flags), 1, "one build per layout capture")
                builds[layout] = flags[0]
        self.assertEqual(
            len(set(builds.values())), 3, f"each layout builds with its own alignment: {builds}"
        )
        for refused in ("8", "128", "default"):
            with (
                self.subTest(refused=refused),
                tempfile.TemporaryDirectory() as temporary,
                unittest.mock.patch.object(COLLECT.subprocess, "run", refuse_any_process),
                unittest.mock.patch.object(
                    sys, "argv",
                    ["collect.py", str(Path(temporary) / "capture"), "--layout", refused],
                ),
                self.assertRaises((SystemExit, ValueError)),
            ):
                COLLECT.collect(COLLECT.arguments())

    def test_collector_shared_runner_builds_this_runner_against_the_base_tree(self) -> None:
        this_runner = runner_sha256()
        for shared in (True, False):
            with self.subTest(shared=shared), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                base_tree = root / "dios-at-base"
                write_base_tree(base_tree)
                runner_source = base_tree / "benches/mmap_workloads/mod.rs"
                runner_source.write_bytes(runner_source.read_bytes() + b"\n// base runner\n")
                base_runner = hashlib.sha256(
                    b"".join((base_tree / path).read_bytes() for path in COLLECT.RUNNER_SOURCES)
                ).hexdigest()
                fixture_sha256 = write_fixture(root / "fixture")
                fake = fake_collector_processes(root, base_tree, fixture_sha256, [])
                own_base_builds, names_base_builds = [], []

                def run(command, *arguments, cwd=None, **keywords):
                    command = [str(part) for part in command]
                    if command[0] == "cargo":
                        environment = " ".join(str(value) for value in (keywords.get("env") or {}).values())
                        names_base = names_tree(command + [environment], cwd, base_tree)
                        builds_base_sources = cwd is not None and Path(cwd).resolve().is_relative_to(base_tree)
                        own_base_builds.append(builds_base_sources)
                        names_base_builds.append(names_base)
                        if names_base and not builds_base_sources:
                            stdout = fake(command, *arguments, cwd=base_tree, **keywords).stdout
                            executable = Path(json.loads(stdout)["executable"])
                            executable.write_bytes(b"shared runner against base")
                            return subprocess.CompletedProcess(command, 0, stdout, "")
                    if command[-1] == "identity":
                        content = Path(command[0]).read_bytes()
                        runner = base_runner if content == b"base runner" else this_runner
                        return subprocess.CompletedProcess(
                            command, 0, json.dumps(dict(runner_sha256=runner, debug_assertions=False)), ""
                        )
                    return fake(command, *arguments, cwd=cwd, **keywords)

                argv = [str(root / "capture"), "--input", str(root / "fixture"),
                        "--lanes", "resident_access_ordinary", "--base-build", str(base_tree)]
                options = collector_options(argv + (["--shared-runner"] if shared else []))
                failure = run_collector(options, run)
                manifest = json.loads((root / "capture/manifest.json").read_text())
                self.assertEqual(len(manifest["comparisons"]), 1, f"collector stopped with {failure!r}")
                runners = manifest["comparisons"][0]["runners"]
                if shared:
                    self.assertEqual(
                        runners, [this_runner, this_runner],
                        "one runner hash for both executables",
                    )
                    self.assertNotIn(True, own_base_builds, "the base tree's own runner is not built")
                    self.assertIn(True, names_base_builds, "one runner build targets the base tree")
                else:
                    self.assertEqual(runners, [base_runner, this_runner])
                self.assertIsNone(failure)
        self.assertIn(
            "benches/mmap_workloads/fair.rs",
            COLLECT.RUNNER_SOURCES,
            "the runner hash covers the fair-lane module and its cfg-gated arms",
        )

    def test_collector_shared_runner_builds_from_one_manifest_across_captures(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            base_tree = root / "dios-at-base"
            write_base_tree(base_tree)
            fixture_sha256 = write_fixture(root / "fixture")
            fake = fake_collector_processes(root, base_tree, fixture_sha256, [])
            manifest_paths = []

            def run(command, *arguments, **keywords):
                command = [str(part) for part in command]
                if command[0] == "cargo" and "--manifest-path" in command:
                    manifest_paths.append(Path(command[command.index("--manifest-path") + 1]))
                return fake(command, *arguments, **keywords)

            captures = [root / "capture-freeze", root / "capture-gated"]
            for capture in captures:
                options = collector_options(
                    [str(capture), "--input", str(root / "fixture"),
                     "--lanes", "resident_access_ordinary",
                     "--base-build", str(base_tree), "--shared-runner"]
                )
                self.assertIsNone(run_collector(options, run))
            self.assertEqual(len(manifest_paths), 2, "one shared-runner build per capture")
            self.assertEqual(
                manifest_paths[0], manifest_paths[1],
                "repeated captures build the base from one harness manifest",
            )
            for capture in captures:
                self.assertFalse(
                    manifest_paths[0].resolve().is_relative_to(capture),
                    "the harness lies outside every capture's output directory",
                )


if __name__ == "__main__":
    unittest.main()
