#!/usr/bin/env python3
"""Generate deterministic synthetic GFA1 graphs for scaling benchmarks."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


def name(index: int) -> str:
    return f"s{index:09d}"


def segment_line(index: int, sequence: str | None, segment_length: int) -> str:
    depth = 10.0 + (index % 41) * 0.75
    reads = 20 + (index % 101)
    if sequence is not None:
        return f"S\t{name(index)}\t{sequence}\tLN:i:{len(sequence)}\tDP:f:{depth:.2f}\tRC:i:{reads}\n"
    return f"S\t{name(index)}\t*\tLN:i:{segment_length}\tDP:f:{depth:.2f}\tRC:i:{reads}\n"


def link_line(left: int, right: int) -> str:
    return f"L\t{name(left)}\t+\t{name(right)}\t+\t0M\n"


def write_graph(
    path: Path,
    segments: int,
    topology: str,
    component_size: int,
    branch_every: int,
    sequence_bases: int,
    segment_length: int,
) -> int:
    links = 0
    sequence = None
    if sequence_bases > 0:
        motif = "ACGT"
        sequence = (motif * ((sequence_bases + 3) // 4))[:sequence_bases]
    with path.open("w", encoding="ascii", buffering=1024 * 1024) as handle:
        handle.write("H\tVN:Z:1.0\n")
        for index in range(segments):
            handle.write(segment_line(index, sequence, segment_length))

        if topology == "chain":
            for index in range(segments - 1):
                handle.write(link_line(index, index + 1))
                links += 1
        elif topology == "ring":
            for index in range(segments - 1):
                handle.write(link_line(index, index + 1))
                links += 1
            if segments > 1:
                handle.write(link_line(segments - 1, 0))
                links += 1
        elif topology == "branched":
            for index in range(segments - 1):
                handle.write(link_line(index, index + 1))
                links += 1
            for index in range(0, max(0, segments - 2), branch_every):
                handle.write(link_line(index, index + 2))
                links += 1
        elif topology == "fragmented":
            for start in range(0, segments, component_size):
                end = min(start + component_size, segments)
                for index in range(start, end - 1):
                    handle.write(link_line(index, index + 1))
                    links += 1
        else:
            raise ValueError(f"unknown topology: {topology}")
    return links


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("output_dir", type=Path)
    parser.add_argument("--sizes", nargs="+", type=int, default=[10_000, 100_000, 500_000, 1_000_000])
    parser.add_argument("--topologies", nargs="+", choices=["chain", "branched", "fragmented", "ring"], default=["chain", "branched", "fragmented"])
    parser.add_argument("--component-size", type=int, default=25)
    parser.add_argument("--branch-every", type=int, default=20)
    parser.add_argument("--sequence-bases", type=int, default=0, help="embed this many bases per segment; 0 uses '*' and LN tags")
    parser.add_argument("--segment-length", type=int, default=1000)
    args = parser.parse_args()

    args.output_dir.mkdir(parents=True, exist_ok=True)
    manifest = []
    for topology in args.topologies:
        for segments in args.sizes:
            path = args.output_dir / f"synthetic_{topology}_{segments}.gfa"
            print(f"writing {path}")
            links = write_graph(
                path,
                segments=segments,
                topology=topology,
                component_size=args.component_size,
                branch_every=args.branch_every,
                sequence_bases=args.sequence_bases,
                segment_length=args.segment_length,
            )
            manifest.append({
                "name": f"synthetic_{topology}_{segments}",
                "path": str(path),
                "kind": "synthetic",
                "topology": topology,
                "segments": segments,
                "links": links,
                "segment_length": args.segment_length,
                "sequence_bases": args.sequence_bases,
            })

    (args.output_dir / "manifest.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
