#!/usr/bin/env python3
"""Fetch and exercise public GFA compatibility datasets with Graphite.

The datasets themselves are intentionally not committed. This script downloads
commit-pinned public fixtures where possible, records SHA-256 hashes in a local
lock file, optionally generates a current hifiasm graph, and runs Graphite's
headless benchmark path as a parser/layout regression test.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]
DEFAULT_MANIFEST = HERE / "datasets.json"
DEFAULT_DATA_DIR = HERE / "data"
DEFAULT_RESULTS_DIR = HERE / "results"
DEFAULT_LOCK = HERE / "datasets.lock.json"
TIER_ORDER = {"smoke": 0, "standard": 1, "large": 2}


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def load_manifest(path: Path) -> dict[str, Any]:
    payload = json.loads(path.read_text(encoding="utf-8"))
    if payload.get("schema_version") != 1:
        raise ValueError(f"unsupported compatibility manifest schema: {path}")
    datasets = payload.get("datasets")
    if not isinstance(datasets, list):
        raise ValueError(f"manifest must contain a datasets array: {path}")
    names = [item.get("name") for item in datasets]
    if len(names) != len(set(names)):
        raise ValueError("dataset names must be unique")
    return payload


def selected_datasets(
    manifest: dict[str, Any],
    tier: str,
    include_generated: bool,
) -> list[dict[str, Any]]:
    max_tier = TIER_ORDER[tier]
    selected = []
    for item in manifest["datasets"]:
        item_tier = item.get("tier", "standard")
        if item_tier not in TIER_ORDER:
            raise ValueError(f"unknown tier {item_tier!r} for {item.get('name')}")
        if TIER_ORDER[item_tier] > max_tier:
            continue
        if item.get("kind") == "generated-hifiasm" and not include_generated:
            continue
        selected.append(item)
    return selected


def hash_file(path: Path, algorithm: str = "sha256") -> str:
    digest = hashlib.new(algorithm)
    with path.open("rb") as handle:
        while chunk := handle.read(8 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def download(url: str, destination: Path, force: bool = False) -> None:
    if destination.exists() and not force:
        return
    destination.parent.mkdir(parents=True, exist_ok=True)
    part = destination.with_suffix(destination.suffix + ".part")
    part.unlink(missing_ok=True)

    request = urllib.request.Request(
        url,
        headers={"User-Agent": "Graphite-GFA-compatibility-suite/1"},
    )
    print(f"download  {url}")
    print(f"       -> {destination}")
    try:
        with urllib.request.urlopen(request) as response, part.open("wb") as output:
            total_header = response.headers.get("Content-Length")
            total = int(total_header) if total_header and total_header.isdigit() else None
            copied = 0
            next_report = 64 * 1024 * 1024
            while chunk := response.read(4 * 1024 * 1024):
                output.write(chunk)
                copied += len(chunk)
                if copied >= next_report:
                    if total:
                        print(f"          {copied / 1e6:.1f}/{total / 1e6:.1f} MB")
                    else:
                        print(f"          {copied / 1e6:.1f} MB")
                    next_report += 64 * 1024 * 1024
        part.replace(destination)
    except Exception:
        part.unlink(missing_ok=True)
        raise


def decompress_gzip(source: Path, destination: Path, force: bool = False) -> None:
    if destination.exists() and not force:
        return
    destination.parent.mkdir(parents=True, exist_ok=True)
    part = destination.with_suffix(destination.suffix + ".part")
    part.unlink(missing_ok=True)
    print(f"gunzip    {source.name} -> {destination.name}")
    try:
        with gzip.open(source, "rb") as compressed, part.open("wb") as output:
            shutil.copyfileobj(compressed, output, length=8 * 1024 * 1024)
        part.replace(destination)
    except Exception:
        part.unlink(missing_ok=True)
        raise


def verify_download(item: dict[str, Any], path: Path) -> None:
    expected_bytes = item.get("expected_bytes")
    if expected_bytes is not None and path.stat().st_size != int(expected_bytes):
        raise RuntimeError(
            f"{item['name']}: expected {expected_bytes} bytes, got {path.stat().st_size}"
        )

    expected_sha256 = item.get("expected_sha256")
    if expected_sha256:
        actual = hash_file(path, "sha256")
        if actual.lower() != str(expected_sha256).lower():
            raise RuntimeError(
                f"{item['name']}: SHA-256 mismatch: expected {expected_sha256}, got {actual}"
            )

    expected_md5 = item.get("expected_md5")
    if expected_md5:
        actual = hash_file(path, "md5")
        if actual.lower() != str(expected_md5).lower():
            raise RuntimeError(
                f"{item['name']}: MD5 mismatch: expected {expected_md5}, got {actual}"
            )


def command_version(command: list[str]) -> str | None:
    try:
        result = subprocess.run(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=15,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    lines = result.stdout.strip().splitlines()
    return lines[0] if lines else None


def fetch_download_dataset(
    item: dict[str, Any],
    data_dir: Path,
    force: bool,
) -> dict[str, Any]:
    kind = item["kind"]
    final_path = data_dir / item["filename"]

    if kind == "download":
        download(item["url"], final_path, force=force)
        verify_download(item, final_path)
        return {
            "name": item["name"],
            "kind": kind,
            "source_url": item["url"],
            "path": str(final_path.relative_to(HERE)),
            "bytes": final_path.stat().st_size,
            "sha256": hash_file(final_path),
            "fetched_at": utc_now(),
        }

    if kind == "download-gzip":
        source_name = item.get("source_filename") or (item["filename"] + ".gz")
        source_path = data_dir / source_name
        download(item["url"], source_path, force=force)
        verify_download(item, source_path)
        decompress_gzip(source_path, final_path, force=force)
        return {
            "name": item["name"],
            "kind": kind,
            "source_url": item["url"],
            "source_path": str(source_path.relative_to(HERE)),
            "source_bytes": source_path.stat().st_size,
            "source_sha256": hash_file(source_path),
            "source_md5": hash_file(source_path, "md5"),
            "path": str(final_path.relative_to(HERE)),
            "bytes": final_path.stat().st_size,
            "sha256": hash_file(final_path),
            "fetched_at": utc_now(),
        }

    raise ValueError(f"unsupported download kind: {kind}")


def generate_hifiasm_dataset(
    item: dict[str, Any],
    data_dir: Path,
    force: bool,
    hifiasm: str | None,
    threads: int,
) -> dict[str, Any]:
    executable = hifiasm or shutil.which("hifiasm")
    if not executable:
        raise RuntimeError(
            "hifiasm dataset requested, but no hifiasm executable was found; "
            "pass --hifiasm /path/to/hifiasm"
        )

    input_path = data_dir / item["input_filename"]
    download(item["input_url"], input_path, force=force)

    final_path = data_dir / item["filename"]
    if final_path.exists() and not force:
        return {
            "name": item["name"],
            "kind": item["kind"],
            "input_url": item["input_url"],
            "input_path": str(input_path.relative_to(HERE)),
            "input_bytes": input_path.stat().st_size,
            "input_sha256": hash_file(input_path),
            "generator": str(executable),
            "generator_version": command_version([str(executable), "--version"]),
            "path": str(final_path.relative_to(HERE)),
            "bytes": final_path.stat().st_size,
            "sha256": hash_file(final_path),
            "fetched_at": utc_now(),
        }

    with tempfile.TemporaryDirectory(prefix="graphite-hifiasm-", dir=data_dir) as temp:
        prefix = Path(temp) / "test"
        command = [
            str(executable),
            "-o",
            str(prefix),
            "-t",
            str(max(1, threads)),
            "-f0",
            str(input_path.resolve()),
        ]
        print("generate  " + " ".join(command))
        log_path = data_dir / "hifiasm_chr11_2m.log"
        with log_path.open("w", encoding="utf-8") as log:
            result = subprocess.run(
                command,
                stdout=log,
                stderr=subprocess.STDOUT,
                text=True,
                check=False,
            )
        if result.returncode != 0:
            raise RuntimeError(
                f"hifiasm failed with exit code {result.returncode}; see {log_path}"
            )

        generated = Path(f"{prefix}.bp.p_ctg.gfa")
        if not generated.exists():
            candidates = sorted(Path(temp).glob("*.p_ctg.gfa"))
            if len(candidates) == 1:
                generated = candidates[0]
            else:
                raise RuntimeError(
                    "hifiasm finished but no unambiguous *.p_ctg.gfa output was found"
                )
        shutil.copy2(generated, final_path)

    return {
        "name": item["name"],
        "kind": item["kind"],
        "input_url": item["input_url"],
        "input_path": str(input_path.relative_to(HERE)),
        "input_bytes": input_path.stat().st_size,
        "input_sha256": hash_file(input_path),
        "generator": str(executable),
        "generator_version": command_version([str(executable), "--version"]),
        "generator_threads": max(1, threads),
        "path": str(final_path.relative_to(HERE)),
        "bytes": final_path.stat().st_size,
        "sha256": hash_file(final_path),
        "fetched_at": utc_now(),
    }


def read_lock(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {"schema_version": 1, "datasets": {}}
    payload = json.loads(path.read_text(encoding="utf-8"))
    if payload.get("schema_version") != 1:
        raise ValueError(f"unsupported lock schema: {path}")
    if not isinstance(payload.get("datasets"), dict):
        payload["datasets"] = {}
    return payload


def write_lock(path: Path, lock: dict[str, Any]) -> None:
    lock["schema_version"] = 1
    lock["updated_at"] = utc_now()
    path.write_text(json.dumps(lock, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def fetch_selected(args: argparse.Namespace, manifest: dict[str, Any]) -> None:
    data_dir = args.data_dir
    data_dir.mkdir(parents=True, exist_ok=True)
    lock = read_lock(args.lock)

    for item in selected_datasets(manifest, args.tier, args.include_generated):
        if item["kind"] == "generated-hifiasm":
            record = generate_hifiasm_dataset(
                item,
                data_dir,
                force=args.force,
                hifiasm=args.hifiasm,
                threads=args.hifiasm_threads,
            )
        else:
            record = fetch_download_dataset(item, data_dir, force=args.force)
        lock["datasets"][item["name"]] = record
        write_lock(args.lock, lock)
        print(
            f"locked    {item['name']}: {record['bytes']} bytes, "
            f"sha256={record['sha256']}"
        )


def git_commit() -> str | None:
    try:
        return subprocess.check_output(
            ["git", "rev-parse", "HEAD"],
            cwd=REPO_ROOT,
            text=True,
            stderr=subprocess.DEVNULL,
        ).strip()
    except (OSError, subprocess.CalledProcessError):
        return None


def ensure_graphite(binary: Path, no_build: bool) -> Path:
    if binary.exists():
        return binary.resolve()
    if no_build:
        raise FileNotFoundError(
            f"Graphite binary not found: {binary}; build it or omit --no-build"
        )
    print("build     cargo build --release")
    subprocess.run(["cargo", "build", "--release"], cwd=REPO_ROOT, check=True)
    if not binary.exists():
        raise FileNotFoundError(f"Graphite binary still not found after build: {binary}")
    return binary.resolve()


def check_expectations(
    item: dict[str, Any],
    report: dict[str, Any],
) -> list[str]:
    failures = []
    for key, expected in item.get("expect", {}).items():
        actual = report.get(key)
        if actual != expected:
            failures.append(f"{key}: expected {expected!r}, got {actual!r}")
    return failures


def run_selected(args: argparse.Namespace, manifest: dict[str, Any]) -> int:
    graphite = ensure_graphite(args.graphite, args.no_build)
    args.results_dir.mkdir(parents=True, exist_ok=True)
    selected = selected_datasets(manifest, args.tier, args.include_generated)
    results: list[dict[str, Any]] = []
    failures = 0

    jsonl_path = args.results_dir / "compatibility.jsonl"
    with jsonl_path.open("w", encoding="utf-8") as jsonl:
        for item in selected:
            path = args.data_dir / item["filename"]
            if not path.exists():
                result = {
                    "dataset": item["name"],
                    "status": "missing",
                    "path": str(path),
                    "message": "run fetch/all first",
                }
                print(f"MISSING   {item['name']}: {path}")
                failures += 1
                results.append(result)
                jsonl.write(json.dumps(result, sort_keys=True) + "\n")
                continue

            command = [
                str(graphite),
                "--benchmark",
                "--layout-backend",
                args.backend,
                "--benchmark-steps",
                str(args.steps),
                str(path.resolve()),
            ]
            print(f"run       {item['name']} ({path.stat().st_size / 1e6:.2f} MB)")
            started = time.monotonic()
            try:
                completed = subprocess.run(
                    command,
                    cwd=REPO_ROOT,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    timeout=args.timeout,
                    check=False,
                )
                wall_seconds = time.monotonic() - started
            except subprocess.TimeoutExpired as exc:
                wall_seconds = time.monotonic() - started
                result = {
                    "dataset": item["name"],
                    "producer": item.get("producer"),
                    "tier": item.get("tier"),
                    "status": "timeout",
                    "wall_seconds": wall_seconds,
                    "timeout_seconds": args.timeout,
                    "stderr": (exc.stderr or "")[-4000:] if isinstance(exc.stderr, str) else "",
                }
                print(f"TIMEOUT   {item['name']} after {wall_seconds:.1f}s")
                failures += 1
                results.append(result)
                jsonl.write(json.dumps(result, sort_keys=True) + "\n")
                continue

            if completed.returncode != 0:
                result = {
                    "dataset": item["name"],
                    "producer": item.get("producer"),
                    "tier": item.get("tier"),
                    "status": "failed",
                    "returncode": completed.returncode,
                    "wall_seconds": wall_seconds,
                    "stdout": completed.stdout[-4000:],
                    "stderr": completed.stderr[-4000:],
                }
                print(f"FAILED    {item['name']} (exit {completed.returncode})")
                failures += 1
                results.append(result)
                jsonl.write(json.dumps(result, sort_keys=True) + "\n")
                continue

            try:
                lines = [line for line in completed.stdout.splitlines() if line.strip()]
                report = json.loads(lines[-1])
            except (IndexError, json.JSONDecodeError) as exc:
                result = {
                    "dataset": item["name"],
                    "producer": item.get("producer"),
                    "tier": item.get("tier"),
                    "status": "invalid-json",
                    "wall_seconds": wall_seconds,
                    "message": str(exc),
                    "stdout": completed.stdout[-4000:],
                    "stderr": completed.stderr[-4000:],
                }
                print(f"BAD JSON  {item['name']}")
                failures += 1
                results.append(result)
                jsonl.write(json.dumps(result, sort_keys=True) + "\n")
                continue

            expectation_failures = check_expectations(item, report)
            status = "ok" if not expectation_failures else "regression"
            if expectation_failures:
                failures += 1

            result = {
                "dataset": item["name"],
                "producer": item.get("producer"),
                "tier": item.get("tier"),
                "features": item.get("features", []),
                "status": status,
                "wall_seconds": wall_seconds,
                "expectation_failures": expectation_failures,
                "graphite": report,
                "stderr_tail": completed.stderr[-2000:],
            }
            results.append(result)
            jsonl.write(json.dumps(result, sort_keys=True) + "\n")

            counts = (
                f"S={report.get('segments')} "
                f"L={report.get('gfa_links')} "
                f"J={report.get('gfa_jumps')} "
                f"P={report.get('gfa_paths')} "
                f"W={report.get('gfa_walks')} "
                f"C={report.get('gfa_containments')}"
            )
            if expectation_failures:
                print(f"REGRESS   {item['name']}: {'; '.join(expectation_failures)}")
            else:
                print(f"OK        {item['name']}: {counts}, {wall_seconds:.2f}s")

    summary = {
        "schema_version": 1,
        "created_at": utc_now(),
        "graphite_binary": str(graphite),
        "graphite_commit": git_commit(),
        "layout_backend": args.backend,
        "benchmark_steps": args.steps,
        "tier": args.tier,
        "include_generated": args.include_generated,
        "datasets": len(results),
        "passed": sum(item["status"] == "ok" for item in results),
        "failed": failures,
        "results": results,
    }
    (args.results_dir / "summary.json").write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    print(
        f"\nsummary   {summary['passed']}/{summary['datasets']} passed; "
        f"{failures} failed"
    )
    print(f"results   {args.results_dir / 'summary.json'}")
    return 1 if failures else 0


def print_dataset_list(
    manifest: dict[str, Any],
    tier: str,
    include_generated: bool,
) -> None:
    for item in selected_datasets(manifest, tier, include_generated):
        marker = "generated" if item["kind"].startswith("generated") else item["tier"]
        print(
            f"{item['name']:<24} {marker:<10} {item.get('producer', ''):<24} "
            f"{item.get('description', '')}"
        )


def common_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--data-dir", type=Path, default=DEFAULT_DATA_DIR)
    parser.add_argument("--lock", type=Path, default=DEFAULT_LOCK)
    parser.add_argument(
        "--tier",
        choices=tuple(TIER_ORDER),
        default="standard",
        help="smoke = tiny fixtures; standard adds real assembler graphs; large adds HPRC chr22",
    )
    parser.add_argument(
        "--include-generated",
        action="store_true",
        help="also generate the hifiasm compatibility graph from official test reads",
    )
    return parser


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    common = common_parser()

    fetch = subparsers.add_parser("fetch", parents=[common])
    fetch.add_argument("--force", action="store_true")
    fetch.add_argument("--hifiasm")
    fetch.add_argument("--hifiasm-threads", type=int, default=4)

    run = subparsers.add_parser("run", parents=[common])
    run.add_argument(
        "--graphite",
        type=Path,
        default=REPO_ROOT / "target" / "release" / "graphite",
    )
    run.add_argument("--backend", choices=("rust", "bandage"), default="rust")
    run.add_argument("--steps", type=int, default=0)
    run.add_argument("--timeout", type=int, default=900)
    run.add_argument("--results-dir", type=Path, default=DEFAULT_RESULTS_DIR)
    run.add_argument("--no-build", action="store_true")

    all_cmd = subparsers.add_parser("all", parents=[common])
    all_cmd.add_argument("--force", action="store_true")
    all_cmd.add_argument("--hifiasm")
    all_cmd.add_argument("--hifiasm-threads", type=int, default=4)
    all_cmd.add_argument(
        "--graphite",
        type=Path,
        default=REPO_ROOT / "target" / "release" / "graphite",
    )
    all_cmd.add_argument("--backend", choices=("rust", "bandage"), default="rust")
    all_cmd.add_argument("--steps", type=int, default=0)
    all_cmd.add_argument("--timeout", type=int, default=900)
    all_cmd.add_argument("--results-dir", type=Path, default=DEFAULT_RESULTS_DIR)
    all_cmd.add_argument("--no-build", action="store_true")

    subparsers.add_parser("list", parents=[common])
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    manifest = load_manifest(args.manifest)

    if args.command == "list":
        print_dataset_list(manifest, args.tier, args.include_generated)
        return 0
    if args.command == "fetch":
        fetch_selected(args, manifest)
        return 0
    if args.command == "run":
        return run_selected(args, manifest)
    if args.command == "all":
        fetch_selected(args, manifest)
        return run_selected(args, manifest)
    raise AssertionError(args.command)


if __name__ == "__main__":
    raise SystemExit(main())
