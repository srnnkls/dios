# /// script
# requires-python = ">=3.11"
# dependencies = ["matplotlib==3.10.7"]
# ///
"""Plot named prefetch comparisons and their unprofiled worker CPU costs."""

import argparse
import json
from pathlib import Path

from plot import plt, ratios


def cpu_costs(axis, rows: dict) -> None:
    choices = [
        ("prefetch_fragmented", "Fragmented: serial → explicit"),
        ("automatic_scan", "Scan: serial → automatic"),
        ("prefetch_mmap_pressure_scan", "Pressure: mmap → explicit"),
        ("prefetch_pollution", "Hot reads: no hints → wrong hints"),
    ]
    for position, (name, _) in enumerate(choices):
        values = [
            rows[name][arm]["cpu_ns_per_read"] / 1000 for arm in ("base", "candidate")
        ]
        axis.hlines(position, min(values), max(values), color="#bbbbbb", linewidth=2)
        for value, color, label in zip(
            values, ["#666666", "#167b8a"], ["Base", "Candidate"]
        ):
            axis.plot(
                value,
                position,
                "o",
                color=color,
                label=label if position == 0 else None,
            )
            axis.annotate(
                f"{value:.2f}",
                (value, position),
                xytext=(0, 10),
                textcoords="offset points",
                ha="center",
                fontsize=8,
            )
    axis.set_yticks(range(len(choices)), [label for _, label in choices])
    axis.set_ylim(len(choices) - 0.5, -0.65)
    axis.set_xscale("log")
    axis.set_xlim(0.03, 120)
    axis.set_xlabel("Worker CPU µs/read, including busy polling")
    axis.set_title(
        "CPU work: lower elapsed time still has a cost",
        loc="left",
        fontsize=11,
        fontweight="bold",
    )
    axis.spines[["top", "right", "left"]].set_visible(False)
    axis.grid(axis="x", alpha=0.18)
    axis.legend(loc="lower right", frameon=False)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("primary_model", "frozen_model", "output"):
        parser.add_argument(name, type=Path)
    options = parser.parse_args()
    rows = {
        row["name"]: row
        for path in (options.primary_model, options.frozen_model)
        for row in json.loads(path.read_text())["results"]
    }
    choices = [
        ("prefetch_fragmented", "Fragmented: explicit / serial"),
        ("prefetch_fragmented_whole", "Fragmented: whole window / serial"),
        ("prefetch_scan-frozen", "64 MiB scan: explicit / old pipeline"),
        ("prefetch_pressure_scan-frozen", "Pressure scan: explicit / old pipeline"),
        ("automatic_scan", "64 MiB scan: automatic / serial"),
        ("automatic_pressure_scan", "Pressure scan: automatic / serial"),
        ("prefetch_mmap_scan", "64 MiB scan: explicit / mmap"),
        ("prefetch_mmap_pressure_scan", "Pressure scan: explicit / mmap"),
    ]
    plt.rcParams.update({"font.size": 9, "svg.fonttype": "none"})
    figure, axes = plt.subplots(1, 2, figsize=(15, 6.5), layout="constrained")
    ratios(
        axes[0],
        [dict(rows[name], name=label) for name, label in choices],
        "Elapsed time: each row names its baseline",
    )
    axes[0].set_xlim(0.08, 5)
    axes[0].set_xticks([0.1, 0.25, 0.5, 1, 2, 4], ["0.1", "0.25", "0.5", "1", "2", "4"])
    axes[0].set_xlabel("Candidate / named base elapsed time (lower is better)")
    cpu_costs(axes[1], rows)
    figure.suptitle(
        "Prefetch improves overlap; mmap still leads sequential scans",
        fontsize=15,
        fontweight="bold",
    )
    figure.supxlabel(
        "30 alternating fresh-process pairs · left: geometric ratios with one-sided 95% upper caps · right: mean CPU/read\n"
        "Explicit: 16 credits · automatic: 32 credits · Threadripper 3970X / Samsung 970 PRO / Linux 6.6.64",
        fontsize=9,
    )
    options.output.mkdir(parents=True, exist_ok=True)
    for suffix in ("svg", "png"):
        figure.savefig(options.output / f"prefetch-comparisons.{suffix}", dpi=180)
    plt.close(figure)


if __name__ == "__main__":
    main()
