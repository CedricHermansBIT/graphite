#!/usr/bin/env python3
"""Summarize benchmark raw.jsonl into one row per tool/dataset."""

from __future__ import annotations

import argparse
import csv
import json
import statistics
from collections import defaultdict
from pathlib import Path
from typing import Any


def percentile(values: list[float], fraction: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    position = (len(ordered) - 1) * fraction
    lower = int(position)
    upper = min(lower + 1, len(ordered) - 1)
    weight = position - lower
    return ordered[lower] * (1.0 - weight) + ordered[upper] * weight


def median(values: list[float]) -> float | None:
    return statistics.median(values) if values else None


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("raw", type=Path, nargs="?", default=Path("benchmarks/results/raw.jsonl"))
    parser.add_argument("--output", type=Path, default=Path("benchmarks/results/summary.csv"))
    args = parser.parse_args()

    groups: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)
    for line in args.raw.read_text(encoding="utf-8").splitlines():
        record = json.loads(line)
        if record.get("warmup"):
            continue
        groups[(record["tool"], record["dataset"])].append(record)

    rows = []
    for (tool, dataset), records in sorted(groups.items()):
        successes = [record for record in records if record.get("status") == "ok"]
        wall = [float(record["wall_seconds"]) for record in successes]
        rss_mib = [float(record["peak_rss_bytes"]) / 2**20 for record in successes if record.get("peak_rss_bytes")]

        internal_fields = ["parse_ms", "view_graph_ms", "initial_layout_ms", "refinement_ms", "export_ms", "total_ms"]
        internal_medians = {}
        for field in internal_fields:
            values = [
                float(record["internal"][field])
                for record in successes
                if record.get("internal") and record["internal"].get(field) is not None
            ]
            internal_medians[f"median_{field}"] = median(values)

        rows.append({
            "tool": tool,
            "dataset": dataset,
            "runs": len(records),
            "successes": len(successes),
            "timeouts": sum(record.get("status") == "timeout" for record in records),
            "errors": sum(record.get("status") not in {"ok", "timeout"} for record in records),
            "median_wall_s": median(wall),
            "q1_wall_s": percentile(wall, 0.25),
            "q3_wall_s": percentile(wall, 0.75),
            "median_peak_rss_mib": median(rss_mib),
            "q1_peak_rss_mib": percentile(rss_mib, 0.25),
            "q3_peak_rss_mib": percentile(rss_mib, 0.75),
            **internal_medians,
        })

    args.output.parent.mkdir(parents=True, exist_ok=True)
    if rows:
        with args.output.open("w", encoding="utf-8", newline="") as handle:
            writer = csv.DictWriter(handle, fieldnames=list(rows[0].keys()))
            writer.writeheader()
            writer.writerows(rows)
    print(f"wrote {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
