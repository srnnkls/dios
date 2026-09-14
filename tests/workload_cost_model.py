# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Check attribution boundaries using deliberately overlapping sample stacks."""

from pathlib import Path
from dataclasses import replace
import csv
import json
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "benches/workload_suite"))
from analyze import (  # noqa: E402
    Run,
    load_run,
    profile_budgets,
    read_profile,
    require_matching,
)


def main() -> None:
    with tempfile.TemporaryDirectory(prefix="dios-cost-model-") as temporary:
        path = Path(temporary) / "profile.folded"
        path.write_text(
            "main;fixture::create_file;fold_bytes 1000\n"
            "main;engine::timed_workload;consume_guard;consume_bytes;fold_bytes 60\n"
            "main;engine::timed_workload;consume_guard;drop_guard 10\n"
            "main;engine::timed_workload;phase_poll;dios::driver::poll 30\n"
        )
        categories, excluded = read_profile(path)
        assert excluded == 1000, "fixture CPU must not enter workload attribution"
        assert categories.total() == 100, (
            "inclusive frames must not double-count samples"
        )
        assert categories["decode"] == 60
        assert categories["guard_release_and_harness"] == 10
        assert categories["poll_and_completion"] == 30
        path.write_text("main;engine::timed_workload -1\n")
        try:
            read_profile(path)
        except ValueError:
            pass
        else:
            raise AssertionError("negative sample weights must fail")
        primary = Run(
            directory=Path(temporary),
            mode="run",
            runner="runner",
            executable="one",
            recipe="recipe",
            posture=("Unregistered", "Direct"),
            platform="linux",
            groups={},
        )
        changed = Run(
            directory=Path(temporary),
            mode="trace",
            runner="runner",
            executable="two",
            recipe="recipe",
            posture=("Unregistered", "Direct"),
            platform="linux",
            groups={},
        )
        try:
            require_matching(primary, changed)
        except ValueError:
            pass
        else:
            raise AssertionError("different binaries must not be silently combined")
        try:
            require_matching(
                primary, replace(primary, posture=("Registered", "Direct"))
            )
        except ValueError:
            pass
        else:
            raise AssertionError("different I/O postures must not be combined")
        check_mixed_posture(Path(temporary))
        check_profile_budget(Path(temporary))
    print("cost-model attribution checks passed")


def check_mixed_posture(directory: Path) -> None:
    (directory / "manifest.json").write_text(
        json.dumps(
            {
                "schema": "dios-workload-suite-v1",
                "status": "complete",
                "mode": "run",
                "os": "linux",
                "runner_sha256": "runner",
                "executable_sha256": "executable",
                "fixture_recipe": "recipe",
            }
        )
    )
    with (directory / "measurements.csv").open("w", newline="") as target:
        writer = csv.writer(target)
        writer.writerow(
            [
                "operations",
                "allocations",
                "elapsed_ns",
                "foreground_ns",
                "lane",
                "arm",
                "pair",
                "registration",
                "io_mode",
            ]
        )
        for pair, posture in enumerate(("Registered", "Unregistered")):
            writer.writerow(
                [1, 0, 100, 90, "point_batch", "base", pair, posture, "Direct"]
            )
    try:
        load_run(directory)
    except ValueError:
        pass
    else:
        raise AssertionError("a mixed-posture run must not enter the cost model")


def check_profile_budget(root: Path) -> None:
    directory = root / "point_batch/base"
    measurement = directory / "measurement"
    measurement.mkdir(parents=True)
    manifest = json.loads((root / "manifest.json").read_text())
    manifest["mode"] = "profile"
    (measurement / "manifest.json").write_text(json.dumps(manifest))
    row = dict(
        operations="10",
        allocations="0",
        elapsed_ns="10000",
        foreground_ns="9000",
        lane="point_batch",
        arm="base",
        pair="0",
        registration="Unregistered",
        io_mode="Direct",
        cpu_ns="9000",
    )
    with (measurement / "measurements.csv").open("w", newline="") as target:
        writer = csv.DictWriter(target, fieldnames=list(row))
        writer.writeheader()
        writer.writerow(row)
    profiled = load_run(measurement)
    primary = replace(
        profiled,
        mode="run",
        groups={
            ("point_batch", "base"): (dict(row, cpu_ns="1000"),),
        },
    )
    (directory / "profile.folded").write_text(
        "main;engine::timed_workload;fold_bytes 100\n"
    )
    (directory / "perf-statistics.txt").write_text(
        "SAMPLE events: 100\nTHROTTLE events: 1\nUNTHROTTLE events: 1\n"
    )
    for invalid in (
        None,
        {"sample_unit": "event_period", "lost_events": 0},
        {"sample_unit": "samples", "lost_events": 1},
    ):
        if invalid is not None:
            (directory / "profile-metadata.json").write_text(json.dumps(invalid))
        try:
            profile_budgets(primary, root)
        except (ValueError, FileNotFoundError):
            pass
        else:
            raise AssertionError("unverified units or lost samples must fail")
    (directory / "profile-metadata.json").write_text(
        json.dumps(
            {
                "sample_unit": "samples",
                "lost_events": 0,
                "event": "cycles",
                "period": 100000,
                "frequency": None,
            }
        )
    )
    budget = profile_budgets(primary, root)["point_batch/base"]
    assert budget["timed_samples"] == 100
    assert budget["categories"]["decode"]["cpu_ns_per_batch_estimate"] == 1000
    assert budget["profile_cpu_per_primary_cpu"] == 9
    assert budget["sample_kind"] == "on_cpu_cycles"
    assert budget["collector"]["throttle_events"] == 1


if __name__ == "__main__":
    main()
