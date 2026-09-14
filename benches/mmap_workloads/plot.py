# /// script
# requires-python = ">=3.11"
# dependencies = ["matplotlib==3.10.7"]
# ///
"""Render primary mmap comparisons without treating one-sided bounds as two-sided intervals."""

import argparse
import json
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt


def ratios(axis, rows: list[dict], title: str) -> None:
    names = [
        row["name"].replace("resident_access_", "").replace("_", " ") for row in rows
    ]
    for position, row in enumerate(rows):
        ratio = row["ratio"]["ratio_geomean"]
        upper = row["ratio"]["ci95_upper"]
        color = "#167b8a" if ratio < 1 else "#b64938"
        axis.plot(ratio, position, "o", color=color, markersize=5)
        axis.hlines(position, ratio, upper, color=color, linewidth=1.5)
        axis.plot(upper, position, "|", color=color)
        axis.annotate(
            f" {ratio:.2f}",
            (upper, position),
            xytext=(5, 0),
            textcoords="offset points",
            va="center",
            fontsize=8,
        )
    axis.axvline(1, color="#555555", linewidth=1)
    axis.set_xscale("log")
    axis.set_xlim(0.055, 16)
    axis.set_xticks([0.1, 0.25, 0.5, 1, 2, 4, 8])
    axis.set_xticklabels(["0.1", "0.25", "0.5", "1", "2", "4", "8"])
    axis.set_yticks(range(len(names)), names)
    axis.invert_yaxis()
    axis.grid(axis="x", alpha=0.18)
    axis.set_title(title, loc="left", fontsize=11, fontweight="bold")
    axis.set_xlabel("Dios elapsed / mmap elapsed  (lower favors Dios)")
    axis.spines[["top", "right", "left"]].set_visible(False)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("primary_model", "resident_model", "output"):
        parser.add_argument(name, type=Path)
    options = parser.parse_args()
    primary = json.loads(options.primary_model.read_text())["results"]
    resident = json.loads(options.resident_model.read_text())["results"]
    plt.rcParams.update({"font.size": 9, "svg.fonttype": "none"})
    figure, axes = plt.subplots(
        1,
        2,
        figsize=(14, 6.6),
        layout="constrained",
        gridspec_kw={"width_ratios": [1.25, 1]},
    )
    ratios(axes[0], primary, "Storage page kernels · original API usage")
    ratios(axes[1], resident, "Resident API controls · setup reported separately")
    figure.suptitle(
        "Dios and mmap: workload shape and API use change the result",
        fontsize=15,
        fontweight="bold",
    )
    figure.supxlabel(
        "30 alternating fresh-process pairs per row · points: paired geometric ratios · caps: one-sided 95% upper bounds\n"
        "Threadripper 3970X / Samsung 970 PRO / Linux 6.6.64 · synthetic read-only workloads · 2026-09-13",
        fontsize=9,
    )
    options.output.mkdir(parents=True, exist_ok=True)
    for suffix in ("svg", "png"):
        figure.savefig(options.output / f"comparisons.{suffix}", dpi=180)
    plt.close(figure)


if __name__ == "__main__":
    main()
