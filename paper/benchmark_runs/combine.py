#!/usr/bin/env python3
"""Replace Graphite measurements while preserving archived comparator records."""

import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path


GRAPHITE = {"graphite-rust", "graphite-gpu", "graphite-bandage"}
EXTERNAL = {"bandage", "bandage-ng"}


def read_jsonl(path):
    data = path.read_bytes()
    return data, [json.loads(line) for line in data.splitlines()]


def check_records(rows, tools, datasets, warmups, repetitions):
    counts = Counter((row["tool"], row["dataset"], row["warmup"]) for row in rows)
    expected = {
        (tool, dataset, phase): warmups if phase else repetitions
        for tool in tools for dataset in datasets for phase in (True, False)
    }
    if counts != expected:
        raise ValueError("Tool, dataset, or repetition counts differ from the publication protocol")
    if any(row["mode"] != "publication" or row["jobs"] != 1 for row in rows):
        raise ValueError("The combined data must come from serial publication runs")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("comparators", type=Path, help="existing combined directory")
    parser.add_argument("graphite", type=Path, help="new Graphite-only run directory")
    parser.add_argument("output", type=Path)
    args = parser.parse_args()

    old_provenance = json.loads((args.comparators / "provenance.json").read_text())
    _, old_rows = read_jsonl(args.comparators / "raw.jsonl")
    new_raw, new_rows = read_jsonl(args.graphite / "raw.jsonl")
    info = json.loads((args.graphite / "run_info.json").read_text())
    datasets = [dataset["name"] for dataset in info["datasets"]]
    config = info["config"]
    warmups = int(config["warmups"])
    repetitions = int(config["repetitions"])
    if datasets != old_provenance["datasets"]:
        raise ValueError("Dataset names or order differ from the comparator run")
    if info["mode"] != "publication" or info["jobs"] != 1:
        raise ValueError("Graphite measurements are not a serial publication run")
    if (warmups, repetitions, config["timeout_seconds"], config["sample_interval_ms"]) != (1, 5, 900, 50):
        raise ValueError("Publication timing protocol differs from archived comparators")

    comparator_rows = [row for row in old_rows if row["tool"] in EXTERNAL]
    check_records(comparator_rows, EXTERNAL, datasets, warmups, repetitions)
    check_records(new_rows, GRAPHITE, datasets, warmups, repetitions)
    if any(row["status"] != "ok" for row in new_rows):
        raise ValueError("New Graphite run contains an unsuccessful record")

    selected = comparator_rows + [dict(row, source_run=args.graphite.name) for row in new_rows]
    provenance = {
        "method": "Graphite Rust, CUDA and OGDF from the tagged release; unchanged Bandage and BandageNG records from 2026-09-21",
        "baseline": old_provenance["baseline"],
        "refresh": {
            "directory": str(args.graphite),
            "raw_sha256": hashlib.sha256(new_raw).hexdigest(),
            "commit": info["git_commit"],
            "binary_sha256": info.get("graphite_binary_sha256"),
            "selected_tools": sorted(GRAPHITE),
            "timestamp_utc": info["timestamp_utc"],
        },
        "datasets": datasets,
        "tools": sorted(GRAPHITE | EXTERNAL),
        "measured_repetitions_per_pair": repetitions,
        "warmups_per_pair": warmups,
        "timeout_seconds": config["timeout_seconds"],
        "machine_memory_bytes": {
            "baseline": old_provenance["machine_memory_bytes"]["baseline"],
            "refresh": info["total_memory_bytes"],
        },
    }
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / "raw.jsonl").write_text(
        "".join(json.dumps(row, sort_keys=True) + "\n" for row in selected)
    )
    (args.output / "provenance.json").write_text(json.dumps(provenance, indent=2) + "\n")
    print(f"Combined {len(selected)} records across {len(datasets)} datasets")


if __name__ == "__main__":
    main()
