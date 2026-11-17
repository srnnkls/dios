"""Fresh-process DRP lanes of AT-G7 under the amended DRP-G4 protocol (run on nix)."""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys


SHORT = sys.argv[1]
ROOT = Path.cwd()
GATES = Path.home() / "build/dios-gates"
OUTPUT = ROOT / "target/array-translation/at-g7-drp"
INPUT = GATES / "artifacts/drp_binding_input.bin"
CANDIDATE_TREE = GATES / "worktrees/at-candidate"
CANDIDATE = GATES / f"targets/at-candidate-{SHORT}/release/read_path_product"
CONVERTER = GATES / f"targets/at-control-{SHORT}/debug/drp_gate_artifacts"
LANES = (
    "drp_g2_warm_ordinary", "drp_g2_cycling_reuse",
    "drp_g4_ordinary_base_8t", "drp_g4_ordinary_scaling",
)
EIGHT_THREAD_CPUS = "0-7"


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def command(arguments, stem, environment=None, cwd=None):
    with stem.with_suffix(".stdout").open("w") as stdout:
        with stem.with_suffix(".stderr").open("w") as stderr:
            subprocess.run(arguments, check=True, env=environment, cwd=cwd,
                           stdout=stdout, stderr=stderr)


def identity(tree, executable, arm):
    status = subprocess.check_output(["git", "-C", str(tree), "status", "--porcelain=v1"])
    assert not status, (tree, status)
    commit = subprocess.check_output(["git", "-C", str(tree), "rev-parse", "HEAD"]).decode().strip()
    return dict(arm=arm, source_commit=commit, executable_sha256=digest(executable),
                cargo_lock_sha256=digest(tree / "Cargo.lock"))


def host(lane):
    drp_g4 = lane.startswith("drp_g4")
    return dict(
        cpu="AMD Ryzen Threadripper 3970X 32-Core Processor",
        cpu_set=EIGHT_THREAD_CPUS if drp_g4 else "0",
        governor="performance",
        kernel="6.6.64",
        topology=("physical CPUs 0-7, SMT siblings 32-39 idle" if drp_g4
                  else "CCX-0 CPUs 0-3,32-35"),
        nvme="Samsung SSD 970 PRO",
        direct_io="verified",
        transparent_hugepage="never",
        cache_protocol="resident-prefill-or-cycling-warmup",
    )


def collect(lane):
    output = OUTPUT / lane
    output.mkdir(parents=True)
    scaling = lane.endswith("scaling")
    baseline = "g2-base" if lane.startswith("drp_g2") else "g4-base"
    arms = ("one_thread", "eight_threads") if scaling else ("base", "candidate")
    base = CANDIDATE if scaling else GATES / f"targets/at-{baseline}-{SHORT}/release/read_path_product"
    base_tree = CANDIDATE_TREE if scaling else GATES / f"worktrees/{baseline}"
    manifest = dict(
        schema="dios-drp009-pre-run-v1",
        toolchain=dict(rust="rustc 1.96.0", mise="2026.8.0"),
        runner=dict(source_sha256=digest(CANDIDATE_TREE / "benches/read_path_product.rs"),
                    build_sha256=digest(CANDIDATE_TREE / "build.rs")),
        converter=dict(source_sha256=digest(CANDIDATE_TREE / "src/bin/drp_gate_artifacts.rs")),
        host=host(lane),
        benchmark_arguments=["run", lane],
        products=[identity(base_tree, base, arms[0]), identity(CANDIDATE_TREE, CANDIDATE, arms[1])],
    )
    pre_run = output / "pre-run.json"
    pre_run.write_text(json.dumps(manifest, indent=2) + "\n")
    (output / "provenance.json").write_text(pre_run.read_text())
    for pair in range(30):
        order = "base-candidate" if pair % 2 == 0 else "candidate-base"
        order_arms = arms if pair % 2 == 0 else tuple(reversed(arms))
        for arm in order_arms:
            executable = base if arm == arms[0] else CANDIDATE
            single = lane.startswith("drp_g2") or arm == "one_thread"
            cpus = "0" if single else EIGHT_THREAD_CPUS
            environment = dict(os.environ, DIOS_BENCH_CPU_SET=cpus)
            arguments = ["taskset", "-c", cpus, str(executable), "run", lane, arm,
                         str(pair), order, str(INPUT), str(output / "process.csv"), str(pre_run)]
            command(arguments, output / f"pair-{pair:02d}-{arm}", environment)
        if pair % 5 == 4:
            print(lane, "completed pairs", pair + 1, flush=True)
    if lane.startswith("drp_g2"):
        command([str(CONVERTER), "validate-g2-zero-alloc", str(output / "process.csv")],
                output / "zero-alloc", cwd=CANDIDATE_TREE)
    command([str(CONVERTER), "prepare", str(output / "process.csv"),
             str(output / "paired.csv"), str(output / "provenance.json")],
            output / "converter", cwd=CANDIDATE_TREE)


assert digest(INPUT) == "915341006e824c0212e82abc923d5999e34ede24e102ebd821dcc3d4494af37a"
for lane in sys.argv[2:] or LANES:
    collect(lane)
print("All fresh process pairs and shared conversions completed.", flush=True)
