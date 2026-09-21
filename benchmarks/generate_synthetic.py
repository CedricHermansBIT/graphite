#!/usr/bin/env python3
"""Generate deterministic synthetic GFA1 graphs for scaling and parser-feature benchmarks."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path


FEATURE_CASES = ("tags", "jumps", "paths", "walks", "containments", "mixed")


def name(index: int) -> str:
    return f"s{index:09d}"


def deterministic_sequence(length: int) -> str:
    motif = "ACGT"
    return (motif * ((length + 3) // 4))[:length]


def segment_line(
    index: int,
    sequence: str | None,
    segment_length: int,
    extra_tags: bool = False,
) -> tuple[str, int]:
    depth = 10.0 + (index % 41) * 0.75
    reads = 20 + (index % 101)
    length = len(sequence) if sequence is not None else segment_length
    seq = sequence if sequence is not None else "*"
    fields = [
        "S",
        name(index),
        seq,
        f"LN:i:{length}",
        f"DP:f:{depth:.2f}",
        f"RC:i:{reads}",
    ]
    tag_count = 3

    if extra_tags:
        # Exercise standard GFA1 tags plus one unknown tag that should be
        # retained by Graphite's generic zero-copy tag table.
        checksum_source = sequence if sequence is not None else deterministic_sequence(segment_length)
        checksum = hashlib.sha256(checksum_source.encode("ascii")).hexdigest()
        fields.extend(
            [
                f"FC:i:{5 + index % 29}",
                f"KC:i:{100 + index % 997}",
                f"SH:H:{checksum}",
                f"UR:Z:sequence_{index:09d}.fa",
                f"ZZ:Z:synthetic-{index % 17}",
            ]
        )
        tag_count += 5

    return "\t".join(fields) + "\n", tag_count


def link_line(left: int, right: int, extra_tags: bool = False, edge_id: int = 0) -> tuple[str, int]:
    fields = ["L", name(left), "+", name(right), "+", "0M"]
    tag_count = 0
    if extra_tags:
        fields.extend(
            [
                f"MQ:i:{20 + edge_id % 41}",
                f"NM:i:{edge_id % 4}",
                f"RC:i:{10 + edge_id % 101}",
                f"FC:i:{3 + edge_id % 31}",
                f"KC:i:{50 + edge_id % 503}",
                f"ID:Z:e{edge_id:09d}",
            ]
        )
        tag_count = 6
    return "\t".join(fields) + "\n", tag_count


def jump_line(left: int, right: int, jump_id: int, shortcut: bool = False) -> tuple[str, int]:
    distance = 100
    fields = ["J", name(left), "+", name(right), "+", str(distance)]
    tag_count = 0
    if shortcut:
        fields.append("SC:i:1")
        tag_count = 1
    return "\t".join(fields) + "\n", tag_count


def containment_line(container: int, contained: int, containment_id: int) -> tuple[str, int]:
    position = 10 + containment_id % 100
    fields = [
        "C",
        name(container),
        "+",
        name(contained),
        "+",
        str(position),
        "50M",
        f"NM:i:{containment_id % 3}",
        f"RC:i:{10 + containment_id % 97}",
        f"ID:Z:c{containment_id:09d}",
    ]
    return "\t".join(fields) + "\n", 3


def path_line(path_id: int, start: int, stop: int, use_jumps: bool = False) -> str:
    steps: list[str] = []
    overlaps: list[str] = []
    for index in range(start, stop):
        strand = "+" if index % 7 else "-"
        steps.append(f"{name(index)}{strand}")
        if index + 1 < stop:
            if use_jumps and (index - start + 1) % 10 == 0:
                overlaps.append("100J")
            else:
                overlaps.append("0M")

    if use_jumps and len(steps) > 1:
        # P-line separators encode whether the transition uses L (comma) or J
        # (semicolon). Build the field explicitly rather than joining uniformly.
        segment_field = steps[0]
        for i, step in enumerate(steps[1:], start=1):
            separator = ";" if i % 10 == 0 else ","
            segment_field += separator + step
    else:
        segment_field = ",".join(steps)

    overlap_field = ",".join(overlaps) if overlaps else "*"
    return f"P\tpath_{path_id:06d}\t{segment_field}\t{overlap_field}\n"


def walk_line(walk_id: int, start: int, stop: int) -> str:
    walk = "".join(
        (">" if index % 7 else "<") + name(index)
        for index in range(start, stop)
    )
    return (
        f"W\tsample_{walk_id % 8:02d}\t{1 + walk_id % 2}"
        f"\tchr{1 + walk_id % 22}\t{start * 1000}\t{stop * 1000}\t{walk}\n"
    )


def topology_links(
    segments: int,
    topology: str,
    component_size: int,
    branch_every: int,
) -> list[tuple[int, int]]:
    edges: list[tuple[int, int]] = []
    if topology == "chain":
        edges.extend((index, index + 1) for index in range(segments - 1))
    elif topology == "ring":
        edges.extend((index, index + 1) for index in range(segments - 1))
        if segments > 1:
            edges.append((segments - 1, 0))
    elif topology == "branched":
        edges.extend((index, index + 1) for index in range(segments - 1))
        edges.extend(
            (index, index + 2)
            for index in range(0, max(0, segments - 2), branch_every)
        )
    elif topology == "fragmented":
        for start in range(0, segments, component_size):
            end = min(start + component_size, segments)
            edges.extend((index, index + 1) for index in range(start, end - 1))
    else:
        raise ValueError(f"unknown topology: {topology}")
    return edges


def version_for_feature(feature_case: str | None) -> str:
    if feature_case in {"jumps", "mixed"}:
        return "1.2"
    if feature_case == "walks":
        return "1.1"
    return "1.0"


def write_graph(
    path: Path,
    segments: int,
    topology: str,
    component_size: int,
    branch_every: int,
    sequence_bases: int,
    segment_length: int,
    feature_case: str | None = None,
    feature_chunk_size: int = 250,
) -> dict[str, int | str | None]:
    links = topology_links(segments, topology, component_size, branch_every)
    extra_tags = feature_case in {"tags", "mixed"}
    add_jumps = feature_case in {"jumps", "mixed"}
    add_paths = feature_case in {"paths", "mixed"}
    add_walks = feature_case in {"walks", "mixed"}
    add_containments = feature_case in {"containments", "mixed"}

    sequence = deterministic_sequence(sequence_bases) if sequence_bases > 0 else None

    tag_count = 0
    jump_count = 0
    containment_count = 0
    path_count = 0
    walk_count = 0

    with path.open("w", encoding="ascii", buffering=1024 * 1024) as handle:
        header_fields = ["H", f"VN:Z:{version_for_feature(feature_case)}"]
        if extra_tags:
            header_fields.extend(["TS:i:100", "XX:Z:synthetic-header"])
            tag_count += 2
        handle.write("\t".join(header_fields) + "\n")
        tag_count += 1

        for index in range(segments):
            line, tags = segment_line(index, sequence, segment_length, extra_tags)
            handle.write(line)
            tag_count += tags

        for edge_id, (left, right) in enumerate(links):
            line, tags = link_line(left, right, extra_tags, edge_id)
            handle.write(line)
            tag_count += tags

        if add_jumps:
            if add_paths:
                # For the mixed case, make every semicolon transition in the
                # generated P records correspond to a real adjacent J record.
                jump_id = 0
                for start in range(0, segments, feature_chunk_size):
                    stop = min(start + feature_chunk_size, segments)
                    for right in range(start + 10, stop, 10):
                        left = right - 1
                        line, tags = jump_line(
                            left,
                            right,
                            jump_id,
                            shortcut=(jump_id % 4 == 0),
                        )
                        handle.write(line)
                        jump_count += 1
                        tag_count += tags
                        jump_id += 1
            else:
                # The jump-only case uses sparse long-range connections.
                for jump_id, left in enumerate(range(0, max(0, segments - 20), 200)):
                    right = min(segments - 1, left + 20)
                    line, tags = jump_line(
                        left,
                        right,
                        jump_id,
                        shortcut=(jump_id % 4 == 0),
                    )
                    handle.write(line)
                    jump_count += 1
                    tag_count += tags

        if add_containments:
            for containment_id, contained in enumerate(range(1, segments, 200)):
                container = contained - 1
                line, tags = containment_line(container, contained, containment_id)
                handle.write(line)
                containment_count += 1
                tag_count += tags

        if add_paths:
            for path_id, start in enumerate(range(0, segments, feature_chunk_size)):
                stop = min(start + feature_chunk_size, segments)
                handle.write(path_line(path_id, start, stop, use_jumps=add_jumps))
                path_count += 1

        if add_walks:
            for walk_id, start in enumerate(range(0, segments, feature_chunk_size)):
                stop = min(start + feature_chunk_size, segments)
                handle.write(walk_line(walk_id, start, stop))
                walk_count += 1

    return {
        "links": len(links),
        "jumps": jump_count,
        "containments": containment_count,
        "paths": path_count,
        "walks": walk_count,
        "tags": tag_count,
        "feature_case": feature_case,
    }


def manifest_entry(
    dataset_name: str,
    path: Path,
    segments: int,
    topology: str,
    segment_length: int,
    sequence_bases: int,
    counts: dict[str, int | str | None],
) -> dict[str, object]:
    return {
        "name": dataset_name,
        "path": str(path),
        "kind": "synthetic",
        "topology": topology,
        "segments": segments,
        "links": counts["links"],
        "jumps": counts["jumps"],
        "containments": counts["containments"],
        "paths": counts["paths"],
        "walks": counts["walks"],
        "tags": counts["tags"],
        "feature_case": counts["feature_case"],
        "segment_length": segment_length,
        "sequence_bases": sequence_bases,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "output_dir",
        nargs="?",
        type=Path,
        default=Path("benchmarks/data"),
        help="output directory (default: benchmarks/data)",
    )
    parser.add_argument(
        "--sizes",
        nargs="+",
        type=int,
        default=[10_000, 100_000, 500_000, 1_000_000],
    )
    parser.add_argument(
        "--topologies",
        nargs="+",
        choices=["chain", "branched", "fragmented", "ring"],
        default=["chain", "branched", "fragmented"],
    )
    parser.add_argument("--component-size", type=int, default=25)
    parser.add_argument("--branch-every", type=int, default=20)
    parser.add_argument(
        "--sequence-bases",
        type=int,
        default=0,
        help="embed this many bases per segment; 0 uses '*' and LN tags",
    )
    parser.add_argument("--segment-length", type=int, default=1000)
    parser.add_argument(
        "--feature-cases",
        nargs="+",
        choices=FEATURE_CASES,
        default=list(FEATURE_CASES),
        help="additional parser/metadata cases generated at --feature-size",
    )
    parser.add_argument(
        "--feature-size",
        type=int,
        default=10_000,
        help="segment count for feature cases (default: 10000)",
    )
    parser.add_argument(
        "--feature-topology",
        choices=["chain", "branched", "fragmented", "ring"],
        default="branched",
    )
    parser.add_argument(
        "--feature-chunk-size",
        type=int,
        default=250,
        help="steps per generated P/W record",
    )
    parser.add_argument(
        "--no-feature-cases",
        action="store_true",
        help="generate only the original scaling datasets",
    )
    args = parser.parse_args()

    args.output_dir.mkdir(parents=True, exist_ok=True)
    manifest: list[dict[str, object]] = []

    # Original scaling suite: deliberately unchanged so historical runs remain
    # directly comparable.
    for topology in args.topologies:
        for segments in args.sizes:
            path = args.output_dir / f"synthetic_{topology}_{segments}.gfa"
            print(f"writing {path}")
            counts = write_graph(
                path,
                segments=segments,
                topology=topology,
                component_size=args.component_size,
                branch_every=args.branch_every,
                sequence_bases=args.sequence_bases,
                segment_length=args.segment_length,
            )
            manifest.append(
                manifest_entry(
                    f"synthetic_{topology}_{segments}",
                    path,
                    segments,
                    topology,
                    args.segment_length,
                    args.sequence_bases,
                    counts,
                )
            )

    # Feature suite: one controlled graph size per GFA1 feature. This isolates
    # parser/data-model overhead without multiplying every 1M-node scaling case.
    if not args.no_feature_cases:
        for feature_case in args.feature_cases:
            segments = args.feature_size
            path = args.output_dir / f"synthetic_{feature_case}_{segments}.gfa"
            print(f"writing {path}")
            counts = write_graph(
                path,
                segments=segments,
                topology=args.feature_topology,
                component_size=args.component_size,
                branch_every=args.branch_every,
                sequence_bases=args.sequence_bases,
                segment_length=args.segment_length,
                feature_case=feature_case,
                feature_chunk_size=args.feature_chunk_size,
            )
            manifest.append(
                manifest_entry(
                    f"synthetic_{feature_case}_{segments}",
                    path,
                    segments,
                    args.feature_topology,
                    args.segment_length,
                    args.sequence_bases,
                    counts,
                )
            )

    (args.output_dir / "manifest.json").write_text(
        json.dumps(manifest, indent=2),
        encoding="utf-8",
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
