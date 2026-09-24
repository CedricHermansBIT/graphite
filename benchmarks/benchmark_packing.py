#!/usr/bin/env python3
"""Compare production packing against its exhaustive reference, with exact equality.

This isolates candidate placement; it is not an end-to-end loading benchmark.
"""
import argparse
import os
import platform
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repeats", type=int, default=3, choices=range(1, 11))
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    print(platform.platform(), flush=True)
    subprocess.run(["rustc", "--version"], check=True)
    env = dict(os.environ, GRAPHITE_PACKING_REPEATS=str(args.repeats))
    subprocess.run(
        ["cargo", "test", "--release", "--locked", "packing_benchmark", "--", "--ignored", "--nocapture"],
        cwd=root, env=env, check=True,
    )


if __name__ == "__main__":
    main()
