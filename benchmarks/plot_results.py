#!/usr/bin/env python3
"""Create publication-friendly SVG scaling plots from summary.csv."""

from __future__ import annotations

import argparse
import csv
from collections import defaultdict
from pathlib import Path


SCALING_TOPOLOGIES = {"chain", "branched", "fragmented", "ring"}


def load_rows(path: Path):
    with path.open("r", encoding="utf-8", newline="") as handle:
        return list(csv.DictReader(handle))


def synthetic_parts(dataset: str) -> tuple[str, int] | None:
    if not dataset.startswith("synthetic_"):
        return None
    rest = dataset[len("synthetic_"):]
    try:
        topology, size = rest.rsplit("_", 1)
        if topology not in SCALING_TOPOLOGIES:
            return None
        return topology, int(size)
    except (ValueError, IndexError):
        return None


def plot_metric(rows, topology: str, metric: str, ylabel: str, output: Path) -> None:
    import matplotlib.pyplot as plt

    grouped = defaultdict(list)
    for row in rows:
        parsed = synthetic_parts(row["dataset"])
        value = row.get(metric)
        if parsed is None or not value:
            continue
        row_topology, size = parsed
        if row_topology != topology:
            continue
        grouped[row["tool"]].append((size, float(value)))

    if not grouped:
        return
    fig, ax = plt.subplots(figsize=(7.0, 4.6))
    for tool, points in sorted(grouped.items()):
        points.sort()
        ax.plot([p[0] for p in points], [p[1] for p in points], marker="o", label=tool)
    ax.set_xscale("log")
    ax.set_yscale("log")
    ax.set_xlabel("Segments")
    ax.set_ylabel(ylabel)
    ax.set_title(topology.capitalize())
    ax.grid(True, which="both", alpha=0.2)
    ax.legend(frameon=False)
    fig.tight_layout()
    fig.savefig(output)
    plt.close(fig)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("summary", type=Path, nargs="?", default=Path("benchmarks/results/summary.csv"))
    parser.add_argument("--output-dir", type=Path, default=Path("benchmarks/results"))
    args = parser.parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    rows = load_rows(args.summary)
    topologies = sorted({parts[0] for row in rows if (parts := synthetic_parts(row["dataset"]))})
    for topology in topologies:
        plot_metric(rows, topology, "median_wall_s", "Wall time (s)", args.output_dir / f"wall_time_{topology}.svg")
        plot_metric(rows, topology, "median_peak_rss_mib", "Peak RSS (MiB)", args.output_dir / f"peak_rss_{topology}.svg")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
