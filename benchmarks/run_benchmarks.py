#!/usr/bin/env python3
"""Reproducible cross-tool benchmark runner for assembly-graph viewers."""

from __future__ import annotations

import argparse
import csv
import json
import os
import platform
import signal
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


def load_config(path: Path) -> dict[str, Any]:
    config = json.loads(path.read_text(encoding="utf-8"))
    datasets = list(config.get("datasets", []))
    for manifest_path in config.get("dataset_manifests", []):
        manifest = json.loads(Path(manifest_path).read_text(encoding="utf-8"))
        if not isinstance(manifest, list):
            raise ValueError(f"dataset manifest must be a JSON array: {manifest_path}")
        datasets.extend(manifest)
    config["datasets"] = datasets
    if not config.get("tools") or not datasets:
        raise ValueError("config must contain tools and datasets")
    return config


def inspect_gfa(path: Path) -> dict[str, Any]:
    segments = links = total_length = embedded = 0
    with path.open("rb") as handle:
        for raw in handle:
            if raw.startswith(b"S\t"):
                segments += 1
                fields = raw.rstrip(b"\r\n").split(b"\t")
                if len(fields) >= 3 and fields[2] != b"*":
                    embedded += 1
                    total_length += len(fields[2])
                else:
                    for tag in fields[3:]:
                        if tag.startswith(b"LN:i:"):
                            try:
                                total_length += int(tag[5:])
                            except ValueError:
                                pass
                            break
            elif raw.startswith(b"L\t"):
                links += 1
    return {
        "path": str(path.resolve()),
        "file_bytes": path.stat().st_size,
        "segments": segments,
        "links": links,
        "total_segment_length": total_length,
        "embedded_sequence_segments": embedded,
    }


def cpu_model() -> str | None:
    if sys.platform.startswith("linux"):
        try:
            for line in Path("/proc/cpuinfo").read_text(errors="replace").splitlines():
                if line.lower().startswith("model name"):
                    return line.split(":", 1)[1].strip()
        except OSError:
            pass
    return platform.processor() or None


def total_memory_bytes() -> int | None:
    if sys.platform.startswith("linux"):
        try:
            for line in Path("/proc/meminfo").read_text().splitlines():
                if line.startswith("MemTotal:"):
                    return int(line.split()[1]) * 1024
        except (OSError, ValueError, IndexError):
            pass
    return None


def git_commit() -> str | None:
    try:
        return subprocess.check_output(
            ["git", "rev-parse", "HEAD"], text=True, stderr=subprocess.DEVNULL
        ).strip()
    except (OSError, subprocess.CalledProcessError):
        return None


def version(command: list[str] | None) -> str | None:
    if not command:
        return None
    try:
        result = subprocess.run(
            command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, timeout=15, check=False
        )
        lines = result.stdout.strip().splitlines()
        return lines[0] if lines else f"exit={result.returncode}"
    except (OSError, subprocess.TimeoutExpired) as exc:
        return f"unavailable: {exc}"


def expand(command: list[str], input_path: Path, output_path: Path) -> list[str]:
    values = {"input": str(input_path.resolve()), "output": str(output_path.resolve())}
    return [
        part.replace("{input}", values["input"]).replace("{output}", values["output"])
        for part in command
    ]


def proc_tree_rss(root_pid: int) -> int | None:
    if not sys.platform.startswith("linux"):
        return None
    pending, seen, total = [root_pid], set(), 0
    while pending:
        pid = pending.pop()
        if pid in seen:
            continue
        seen.add(pid)
        try:
            lines = Path(f"/proc/{pid}/status").read_text(errors="replace").splitlines()
        except OSError:
            continue
        for line in lines:
            if line.startswith("VmRSS:"):
                try:
                    total += int(line.split()[1]) * 1024
                except (ValueError, IndexError):
                    pass
                break
        try:
            children = Path(f"/proc/{pid}/task/{pid}/children").read_text().split()
            pending.extend(int(child) for child in children)
        except (OSError, ValueError):
            pass
    return total


def parse_gnu_time(path: Path) -> dict[str, float | int]:
    values: dict[str, float | int] = {}
    if not path.exists():
        return values
    for line in path.read_text(errors="replace").splitlines():
        if ":" not in line:
            continue
        key, raw = (part.strip() for part in line.split(":", 1))
        try:
            if key == "Maximum resident set size (kbytes)":
                values["peak_rss_bytes"] = int(raw) * 1024
            elif key == "User time (seconds)":
                values["user_seconds"] = float(raw)
            elif key == "System time (seconds)":
                values["system_seconds"] = float(raw)
        except ValueError:
            pass
    return values


def stop_process(process: subprocess.Popen[Any]) -> None:
    if process.poll() is not None:
        return
    try:
        if os.name == "posix":
            os.killpg(process.pid, signal.SIGTERM)
        else:
            process.terminate()
        process.wait(timeout=3)
    except (OSError, subprocess.TimeoutExpired):
        try:
            if os.name == "posix":
                os.killpg(process.pid, signal.SIGKILL)
            else:
                process.kill()
        except OSError:
            pass


def graphite_json(stdout: str) -> dict[str, Any] | None:
    for line in reversed(stdout.splitlines()):
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(item, dict) and item.get("schema_version") == 1:
            return item
    return None


def run_once(
    tool: dict[str, Any], dataset: dict[str, Any], run_index: int, warmup: bool,
    timeout_s: float, sample_s: float, output_dir: Path
) -> dict[str, Any]:
    input_path = Path(dataset["path"])
    output_path = output_dir / f"{tool['name']}__{dataset['name']}__{run_index}.svg"
    command = expand(tool["command"], input_path, output_path)

    stdout_tmp = tempfile.NamedTemporaryFile(prefix="graphite-bench-out-", delete=False)
    stderr_tmp = tempfile.NamedTemporaryFile(prefix="graphite-bench-err-", delete=False)
    time_tmp = tempfile.NamedTemporaryFile(prefix="graphite-bench-time-", delete=False)
    for handle in (stdout_tmp, stderr_tmp, time_tmp):
        handle.close()
    stdout_path, stderr_path, time_path = map(
        Path, (stdout_tmp.name, stderr_tmp.name, time_tmp.name)
    )

    gnu_time = Path("/usr/bin/time")
    use_gnu_time = sys.platform.startswith("linux") and gnu_time.is_file()
    launch = [str(gnu_time), "-v", "-o", str(time_path), "--", *command] if use_gnu_time else command

    start = time.perf_counter()
    peak_sample = 0
    timed_out = False
    launch_error = None
    process = None
    try:
        with stdout_path.open("wb") as out, stderr_path.open("wb") as err:
            process = subprocess.Popen(
                launch, stdout=out, stderr=err, cwd=tool.get("cwd") or None,
                start_new_session=(os.name == "posix")
            )
            while process.poll() is None:
                rss = proc_tree_rss(process.pid)
                if rss is not None:
                    peak_sample = max(peak_sample, rss)
                if time.perf_counter() - start > timeout_s:
                    timed_out = True
                    stop_process(process)
                    break
                time.sleep(sample_s)
            if process.poll() is None:
                process.wait()
    except OSError as exc:
        launch_error = str(exc)
        if process is not None:
            stop_process(process)

    wall = time.perf_counter() - start
    stdout = stdout_path.read_text(errors="replace")
    stderr = stderr_path.read_text(errors="replace")
    measured = parse_gnu_time(time_path) if use_gnu_time else {}
    peak_rss = int(measured.get("peak_rss_bytes", peak_sample)) or None
    rss_source = "gnu_time" if measured.get("peak_rss_bytes") else ("proc_sample" if peak_sample else None)
    for path in (stdout_path, stderr_path, time_path):
        path.unlink(missing_ok=True)

    exit_code = process.returncode if process is not None else None
    if timed_out:
        status = "timeout"
    elif launch_error is not None:
        status = "launch_error"
    elif exit_code != 0:
        status = "error"
    else:
        status = "ok"

    output_bytes = output_path.stat().st_size if output_path.exists() else None
    if output_path.exists() and not tool.get("keep_output", False):
        output_path.unlink()

    return {
        "timestamp_utc": datetime.now(timezone.utc).isoformat(),
        "tool": tool["name"], "dataset": dataset["name"], "run": run_index,
        "warmup": warmup, "status": status, "exit_code": exit_code,
        "timed_out": timed_out, "wall_seconds": wall,
        "peak_rss_bytes": peak_rss, "peak_rss_source": rss_source,
        "user_seconds": measured.get("user_seconds"),
        "system_seconds": measured.get("system_seconds"),
        "output_bytes": output_bytes, "command": command,
        "launch_error": launch_error,
        "stdout_tail": "\n".join(stdout.splitlines()[-20:]),
        "stderr_tail": "\n".join(stderr.splitlines()[-20:]),
        "internal": graphite_json(stdout) if tool.get("parse_json", False) else None,
    }


def write_csv(records: list[dict[str, Any]], path: Path) -> None:
    fields = [
        "tool", "dataset", "run", "warmup", "status", "exit_code", "timed_out",
        "wall_seconds", "peak_rss_bytes", "peak_rss_source", "user_seconds",
        "system_seconds", "output_bytes", "parse_ms", "view_graph_ms",
        "initial_layout_ms", "refinement_ms", "export_ms", "total_ms",
    ]
    with path.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=fields)
        writer.writeheader()
        for record in records:
            row = {field: record.get(field) for field in fields}
            internal = record.get("internal") or {}
            for field in ["parse_ms", "view_graph_ms", "initial_layout_ms",
                          "refinement_ms", "export_ms", "total_ms"]:
                row[field] = internal.get(field)
            writer.writerow(row)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("config", type=Path)
    parser.add_argument("--output", type=Path, default=Path("benchmarks/results"))
    parser.add_argument("--tool", action="append")
    parser.add_argument("--dataset", action="append")
    args = parser.parse_args()

    config = load_config(args.config)
    tools = [x for x in config["tools"] if not args.tool or x["name"] in args.tool]
    datasets = [x for x in config["datasets"] if not args.dataset or x["name"] in args.dataset]
    if not tools or not datasets:
        raise SystemExit("selection produced no tools or datasets")

    args.output.mkdir(parents=True, exist_ok=True)
    for dataset in datasets:
        path = Path(dataset["path"])
        if not path.is_file():
            raise SystemExit(f"dataset does not exist: {path}")
        dataset["inspection"] = inspect_gfa(path)

    info = {
        "timestamp_utc": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(), "python": platform.python_version(),
        "cpu_model": cpu_model(), "logical_cpus": os.cpu_count(),
        "total_memory_bytes": total_memory_bytes(), "git_commit": git_commit(),
        "tools": {tool["name"]: version(tool.get("version_command")) for tool in tools},
        "datasets": datasets, "config": config,
    }
    (args.output / "run_info.json").write_text(json.dumps(info, indent=2), encoding="utf-8")

    repeats = int(config.get("repetitions", 5))
    warmups = int(config.get("warmups", 1))
    timeout_s = float(config.get("timeout_seconds", 900))
    sample_s = float(config.get("sample_interval_ms", 50)) / 1000.0

    records: list[dict[str, Any]] = []
    with (args.output / "raw.jsonl").open("w", encoding="utf-8") as raw:
        for dataset in datasets:
            for index in range(warmups + repeats):
                warmup = index < warmups
                label = "warmup" if warmup else f"repeat {index - warmups + 1}/{repeats}"
                for tool in tools:
                    print(f"[{tool['name']}] {dataset['name']} - {label}", flush=True)
                    record = run_once(
                        tool, dataset, index, warmup, timeout_s, sample_s, args.output
                    )
                    records.append(record)
                    raw.write(json.dumps(record) + "\n")
                    raw.flush()
                    rss = record["peak_rss_bytes"]
                    rss_text = f", {rss / 2**20:.1f} MiB" if rss else ""
                    print(f"  {record['status']}: {record['wall_seconds']:.3f} s{rss_text}", flush=True)

    write_csv(records, args.output / "raw.csv")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
