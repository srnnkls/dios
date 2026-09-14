"""Frozen/candidate identities and raw-to-CSV checks for the RC gate matrix."""

from __future__ import annotations

import csv
import hashlib
import json
from pathlib import Path
import shutil
import tarfile

from collect import RUNNER_SOURCES, archive_sources, digest, execute, source_hashes


def archived_sources(path: Path) -> dict[str, bytes]:
    sources, size = {}, 0
    with tarfile.open(path) as archive:
        for index, member in enumerate(archive):
            if index >= 4096:
                raise ValueError("source archive member bound exceeded")
            name = member.name.removeprefix("./")
            selected = name.startswith(("src/", "benches/mmap_workloads")) or name in (
                "Cargo.toml", "Cargo.lock", "build.rs")
            if not selected or member.isdir():
                continue
            if not member.isfile() or Path(name).is_absolute() or ".." in Path(name).parts:
                raise ValueError("source archive contains a non-local regular file")
            size += member.size
            if size > 64 << 20 or name in sources:
                raise ValueError("source archive is excessive or repeats a source")
            with archive.extractfile(member) as stream:
                sources[name] = stream.read()
    if set(RUNNER_SOURCES) - sources.keys():
        raise ValueError("source archive lacks runner inputs")
    return sources


def retain_frozen(options, output: Path) -> dict:
    preflight_path = options.preflight.resolve()
    preflight = json.loads(preflight_path.read_text())
    source = preflight_path.parent / "retained-source.tar.gz"
    starting = preflight_path.parent / preflight["starting_tree_source_archive"]
    binary = (options.frozen_binary or Path(preflight["retained_executable"])).resolve()
    for path, key in ((source, "retained_source_archive_sha256"),
                      (starting, "starting_tree_source_sha256"),
                      (binary, "retained_executable_sha256")):
        if digest(path) != preflight[key]:
            raise ValueError(f"frozen pre-dispatch identity differs: {path}")
    sources, before = archived_sources(source), archived_sources(starting)
    product = {name for name in sources if name.startswith("src/")} | {"Cargo.toml", "Cargo.lock", "build.rs"}
    if any(sources[name] != before.get(name) for name in product):
        raise ValueError("frozen executable source differs from approved starting product")
    compiled = json.loads(execute([str(binary), "identity"]))
    runner = hashlib.sha256(b"".join(sources[name] for name in RUNNER_SOURCES)).hexdigest()
    if compiled["runner_sha256"] != runner or compiled["debug_assertions"]:
        raise ValueError("frozen executable differs from retained release runner source")
    destination = output / "frozen"
    destination.mkdir()
    for path, name in ((source, "source.tar.gz"), (starting, "starting-tree.tar.gz"),
                       (preflight_path, "preflight.json"), (binary, "mmap_workloads")):
        shutil.copy2(path, destination / name)
        if digest(destination / name) != digest(path):
            raise ValueError("frozen archive copy differs")
    return {"executable": str(destination / "mmap_workloads"), "executable_sha256": digest(binary),
            "runner_sha256": runner, "source_archive": str(destination / "source.tar.gz"),
            "source_archive_sha256": digest(source), "preflight_sha256": digest(preflight_path),
            "sources": {name: hashlib.sha256(value).hexdigest() for name, value in sources.items()},
            "starting_tree_source_sha256": digest(starting)}


def retain_candidate(options, manifest: dict, before: dict) -> dict:
    if source_hashes() != before or manifest["sources"] != before:
        raise ValueError("candidate sources changed during build/preparation")
    if options.binary:
        if options.candidate_manifest is None:
            raise ValueError("a supplied RC binary requires its retained --candidate-manifest")
        retained = json.loads(options.candidate_manifest.read_text())
        if retained["sources"] != before or retained["executable_sha256"] != manifest["executable_sha256"]:
            raise ValueError("candidate executable/source provenance differs")
        archive = options.candidate_manifest.parent / Path(retained["source_archive"]).name
        if digest(archive) != retained["source_archive_sha256"]:
            raise ValueError("candidate provenance source archive differs")
        archived = {name: hashlib.sha256(value).hexdigest()
                    for name, value in archived_sources(archive).items()}
        if archived != before:
            raise ValueError("candidate provenance archive differs from measured source identities")
    snapshot = archive_sources(options.output.resolve(), before)
    manifest.update(snapshot)
    binary = Path(manifest["executable"])
    return {"executable": str(binary), "executable_sha256": digest(binary),
            "runner_sha256": json.loads(execute([str(binary), "identity"]))["runner_sha256"],
            "sources": before, **snapshot}


def case_arms(case: tuple[str, str, str], identities: dict) -> list[dict]:
    _, base, candidate = case
    label = "mmap-sequential" if ":mmap:" in base else (
        "current-old-budget" if ":32:64" in base else "current-new-budget")
    return [{"label": label, "configuration": base, "executable_role": "frozen",
             "credit_selection": "override", **identities["frozen"]},
            {"label": "coalesced-default", "configuration": candidate,
             "executable_role": "candidate", "credit_selection": "default", **identities["candidate"]}]


def validate_raw_identity(row: dict, reference: dict, arm: dict) -> None:
    for field in ("configuration", "executable_sha256", "runner_sha256", "credit_selection"):
        if reference[field] != arm[field]:
            raise ValueError(f"scan index arm identity differs: {field}")
    if row["configuration"] != arm["configuration"] or row["runner_sha256"] != arm["runner_sha256"]:
        raise ValueError("raw scan source/configuration identity differs")
    if arm["credit_selection"] == "default":
        if row.get("prefetch_credit_selection") != "default":
            raise ValueError("coalesced candidate did not use the actual default")
        if row.get("miss_headroom") != 3 * row["config"]["read_limit"]:
            raise ValueError("coalesced candidate miss headroom differs from INV-9")


def validate_pair_csv(root: Path, case: dict, pairs: list, metric: str = "elapsed_ns") -> None:
    directory = root / case["name"]
    metadata = case
    if metric == "cpu_ns":
        directory /= "cpu"
        metadata = case["cpu"]
    path = directory / "paired.csv"
    if digest(path) != metadata["csv_sha256"]:
        raise ValueError("scan CSV hash differs")
    with path.open(newline="") as stream:
        rows = list(csv.reader(stream))
    expected = [[str(metric_value(a, metric)), str(metric_value(b, metric))] for a, b in pairs]
    if rows != [["base_ns", "candidate_ns"]] + expected or len(pairs) != 30:
        raise ValueError("scan CSV differs from 30 validated raw pairs")
    if metadata["summary"]["pairs"] != len(pairs):
        raise ValueError("scan statistic sample count differs")


def metric_value(row: dict, metric: str) -> int:
    if metric == "cpu_ns" and "workers" in row:
        return sum(worker["cpu_ns"] for worker in row["workers"])
    return row[metric]


def verify_retained(root: Path, identity: dict) -> None:
    for path_field, hash_field in (("executable", "executable_sha256"),
                                  ("source_archive", "source_archive_sha256")):
        path = root / Path(identity[path_field]).name
        if digest(path) != identity[hash_field]:
            raise ValueError(f"retained scan {path_field} differs")
    sources = archived_sources(root / Path(identity["source_archive"]).name)
    hashes = {name: hashlib.sha256(value).hexdigest() for name, value in sources.items()}
    if hashes != identity["sources"]:
        raise ValueError("retained source archive differs from source identities")
