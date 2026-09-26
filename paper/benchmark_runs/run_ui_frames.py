#!/usr/bin/env python3
"""Archive repeated, serial Graphite canvas-frame measurements."""

import argparse
import hashlib
import json
import os
import platform
import subprocess
from datetime import datetime, timezone
from pathlib import Path


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--dataset", action="append", required=True,
                        help="name=path; repeat for each public GFA")
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--frames", type=int, default=60)
    parser.add_argument("--repetitions", type=int, default=5)
    args = parser.parse_args()
    if args.frames < 1 or args.repetitions < 1:
        parser.error("frames and repetitions must be positive")

    binary = args.binary.resolve(strict=True)
    datasets = []
    for item in args.dataset:
        name, separator, path = item.partition("=")
        if not separator or not name:
            parser.error(f"invalid dataset {item!r}; expected name=path")
        graph = Path(path).resolve(strict=True)
        datasets.append({"name": name, "path": str(graph), "sha256": sha256(graph)})

    args.output_dir.mkdir(parents=True, exist_ok=False)
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    metadata = {
        "timestamp_utc": datetime.now(timezone.utc).isoformat(),
        "git_commit": commit,
        "binary": str(binary),
        "binary_sha256": sha256(binary),
        "platform": platform.platform(),
        "processor": platform.processor(),
        "frames_per_view": args.frames,
        "process_warmups": 1,
        "measured_repetitions": args.repetitions,
        "rayon_threads": os.environ.get("RAYON_NUM_THREADS"),
        "datasets": datasets,
        "measurement_scope": "CPU-side graph canvas construction plus egui tessellation",
    }
    (args.output_dir / "run_info.json").write_text(json.dumps(metadata, indent=2) + "\n")
    with (args.output_dir / "raw.jsonl").open("w") as handle:
        for dataset in datasets:
            for index in range(args.repetitions + 1):
                command = [str(binary), "--benchmark", "--benchmark-ui-frames",
                           str(args.frames), dataset["path"]]
                result = subprocess.run(command, capture_output=True, text=True, check=True)
                record = json.loads(result.stdout)
                record.update({"dataset": dataset["name"],
                               "run_type": "warmup" if index == 0 else "measured",
                               "run_index": index})
                handle.write(json.dumps(record, separators=(",", ":")) + "\n")
                handle.flush()
                overview = record["ui_benchmark"]["overview"]["median_ms"]
                detail = record["ui_benchmark"]["detail"]["median_ms"]
                print(f"{dataset['name']} {record['run_type']} {index}: "
                      f"overview {overview:.2f} ms, detail {detail:.2f} ms", flush=True)


if __name__ == "__main__":
    main()
