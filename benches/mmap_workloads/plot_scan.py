"""Plot validated scan and mechanism arm means; ratio gates remain separate."""

import argparse
import json
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt


def plot(scan: Path, probe: Path, output: Path) -> None:
    scans = json.loads(scan.read_text())["comparisons"]
    probes = json.loads(probe.read_text())["comparisons"]
    selected = {row["candidate"]: row for row in scans}
    cold = selected["geometry:explicit:256:7:8"]
    pressure = selected["pressure:explicit:256:7:8"]
    default = selected["cold:automatic:4:32:64"]
    pressure_default = selected["pressure:automatic:4:32:64"]
    panels = [
        ("Cold scan · 64 MiB", ["mmap", "Dios default · 4 KiB", "Dios coarse · 256 KiB"],
         [cold["base_cost"], default["candidate_cost"], cold["candidate_cost"]]),
        ("Pressure scan · 3 × 256 MiB", ["mmap", "Dios default · 4 KiB", "Dios coarse · 256 KiB"],
         [pressure["base_cost"], pressure_default["candidate_cost"], pressure["candidate_cost"]]),
        ("Driver probe · 128 KiB groups", ["32 × 4 KiB READ", "32-iovec READV", "128 KiB READ"],
         [probes[1]["base_cost"], probes[0]["candidate_cost"], probes[0]["base_cost"]]),
    ]
    plt.rcParams.update({"font.size": 9, "svg.fonttype": "none"})
    figure, axes = plt.subplots(1, 3, figsize=(14, 3.8), layout="constrained")
    for axis, (title, names, rows) in zip(axes, panels, strict=True):
        values = [row["ns_per_page"] / 1000 for row in rows]
        bars = axis.barh(names, values, color=["#687c91", "#d48a37", "#288b78"])
        axis.bar_label(bars, labels=[f"{value:.3f}" for value in values], padding=4)
        axis.invert_yaxis()
        axis.set(title=title, xlabel="Elapsed µs per useful 4 KiB", xlim=(0, 4.4))
        axis.spines[["top", "right"]].set_visible(False)
    figure.suptitle("Coarse scans win; scattered READV retains an 11.8% cost over contiguous READ", fontsize=12)
    figure.savefig(output.with_suffix(".svg"))
    figure.savefig(output.with_suffix(".png"), dpi=140)
    plt.close(figure)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("scan", type=Path)
    parser.add_argument("probe", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    plot(args.scan, args.probe, args.output)
