#!/usr/bin/env python3
"""Build vector benchmark figures from the combined publication summary."""

import argparse
import csv
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt


TOOLS = (
    ("graphite-rust", "Graphite Rust", "#2457a6", "o"),
    ("graphite-gpu", "Graphite CUDA", "#7040a0", "P"),
    ("graphite-bandage", "Graphite OGDF", "#b15b16", "s"),
    ("bandage-ng", "BandageNG", "#15815c", "D"),
    ("bandage", "Bandage", "#a23949", "v"),
)
REAL = (
    ("vg-cactus-brca2", "BRCA2"),
    ("spades-esc", "SPAdes"),
    ("flye-1y3b", "Flye"),
    ("megahit-5g", "MEGAHIT"),
    ("myloasm", "myloasm"),
    ("hifiasm-chr11-2m", "hifiasm"),
    ("hprc-v1.1-chr22", "HPRC chr22"),
)


def number(row, key):
    return float(row[key]) if row and row.get(key) else None


def point(ax, x, row, prefix, color, marker, label=None):
    center = number(row, f"median_{prefix}")
    if center is None:
        return False
    lo = number(row, f"q1_{prefix}") or center
    hi = number(row, f"q3_{prefix}") or center
    ax.errorbar(x, center, yerr=[[max(0, center - lo)], [max(0, hi - center)]],
                fmt=marker, color=color, markersize=5, capsize=2,
                linewidth=1.1, label=label)
    return True


def scaling(rows, topology, output):
    fig, ax = plt.subplots(figsize=(6.2, 4.0))
    sizes = (10_000, 100_000, 500_000, 1_000_000)
    for name, title, color, marker in TOOLS:
        xs, ys = [], []
        for size in sizes:
            row = rows.get((name, f"synthetic_{topology}_{size}"))
            if point(ax, size, row, "wall_s", color, marker):
                xs.append(size)
                ys.append(number(row, "median_wall_s"))
            elif row and int(row["timeouts"]):
                ax.scatter(size, 900, marker="^", s=55, facecolors="none",
                           edgecolors=color, linewidths=1.3)
        if xs:
            ax.plot(xs, ys, color=color, linewidth=1.1, label=title)
    ax.set(xscale="log", yscale="log", xlabel="Segments", ylabel="Wall time (s)",
           title=f"{topology.capitalize()} synthetic graphs")
    ax.set_xticks(sizes, ["10k", "100k", "500k", "1M"])
    ax.grid(alpha=0.2, which="both")
    ax.legend(frameon=False, fontsize=8)
    fig.tight_layout()
    fig.savefig(output)
    plt.close(fig)


def real(rows, metric, ylabel, output):
    fig, ax = plt.subplots(figsize=(8.6, 4.1))
    for index, (name, title, color, marker) in enumerate(TOOLS):
        offset = (index - (len(TOOLS) - 1) / 2) * 0.14
        plotted = False
        for dataset_index, (dataset, _) in enumerate(REAL):
            row = rows.get((name, dataset))
            x = dataset_index + offset
            if point(ax, x, row, metric, color, marker,
                     title if not plotted else None):
                plotted = True
            elif metric == "wall_s" and row and int(row["timeouts"]):
                ax.scatter(x, 900, marker="^", s=45, facecolors="none",
                           edgecolors=color, linewidths=1.2)
    ax.set(yscale="log", ylabel=ylabel)
    ax.set_xticks(range(len(REAL)), [label for _, label in REAL], rotation=25)
    ax.grid(axis="y", alpha=0.2, which="both")
    ax.legend(frameon=False, fontsize=8, ncol=2)
    fig.tight_layout()
    fig.savefig(output)
    plt.close(fig)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("summary", type=Path)
    parser.add_argument("--output-dir", type=Path, default=Path("paper/figures"))
    args = parser.parse_args()
    with args.summary.open(newline="") as handle:
        rows = {(row["tool"], row["dataset"]): row for row in csv.DictReader(handle)}
    args.output_dir.mkdir(parents=True, exist_ok=True)
    for topology in ("branched", "fragmented"):
        scaling(rows, topology, args.output_dir / f"fig_wall_time_{topology}.pdf")
    real(rows, "wall_s", "Wall time (s)", args.output_dir / "fig_real_assemblies_wall_time.pdf")
    real(rows, "peak_rss_mib", "Peak RSS (MiB)", args.output_dir / "fig_real_assemblies_peak_rss.pdf")


if __name__ == "__main__":
    main()
