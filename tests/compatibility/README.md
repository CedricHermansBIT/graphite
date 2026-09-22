# GFA compatibility corpus

This directory defines Graphite's public compatibility/regression corpus. Third-party GFA files are **not committed** to this repository. The manifest pins public GitHub fixtures to exact commits where possible; the fetcher records the actual SHA-256 of every downloaded or generated file in a local lock file.

## Dataset tiers

- **smoke**: tiny correctness fixtures from Bandage and vg. Suitable for frequent local checks and CI when network access is available.
- **standard**: adds biologically meaningful and assembler-produced graphs from vg/Cactus, SPAdes, Flye, MEGAHIT and myloasm.
- **large**: additionally downloads the HPRC v1.1 chromosome 22 minigraph-cactus graph. This is intentionally not part of ordinary CI.
- **generated hifiasm**: optional. Downloads hifiasm's official chr11-2M test reads and uses the locally installed hifiasm executable to produce a current `*.bp.p_ctg.gfa`.

The manifest is [`datasets.json`](datasets.json).

## List datasets

~~~bash
python3 tests/compatibility/run_compatibility.py list --tier smoke
python3 tests/compatibility/run_compatibility.py list --tier standard
python3 tests/compatibility/run_compatibility.py list --tier large
~~~

Add `--include-generated` to include the hifiasm-generated case.

## Fetch

Smoke corpus:

~~~bash
python3 tests/compatibility/run_compatibility.py fetch --tier smoke
~~~

Standard corpus:

~~~bash
python3 tests/compatibility/run_compatibility.py fetch --tier standard
~~~

Include the large HPRC chr22 graph:

~~~bash
python3 tests/compatibility/run_compatibility.py fetch --tier large
~~~

Generate the hifiasm case as well:

~~~bash
python3 tests/compatibility/run_compatibility.py fetch \
  --tier standard \
  --include-generated \
  --hifiasm /path/to/hifiasm \
  --hifiasm-threads 8
~~~

Downloaded/generated files go into `tests/compatibility/data/`. Each successful fetch updates the local `tests/compatibility/datasets.lock.json` with byte size and SHA-256. Compressed sources also retain source hashes.

The lock file is ignored by Git because a generated hifiasm result depends on the exact installed hifiasm version; it is intended to accompany a benchmark/archive run.

## Run Graphite over the corpus

The runner uses Graphite's headless benchmark path, so each case performs GFA parsing, view-graph construction, initial layout and optional refinement steps, then checks any known record counts from the manifest.

~~~bash
python3 tests/compatibility/run_compatibility.py run --tier smoke
~~~

For the standard suite:

~~~bash
python3 tests/compatibility/run_compatibility.py run \
  --tier standard \
  --backend rust
~~~

If `target/release/graphite` does not exist, the script runs `cargo build --release` automatically for the Rust backend. With `--backend bandage`, it builds with `--features ogdf`. Use `--no-build` to disable that behavior.

Results are written to `tests/compatibility/results/compatibility.jsonl` and `tests/compatibility/results/summary.json`.

A non-zero exit status means at least one dataset was missing, timed out, crashed, returned invalid benchmark JSON, or failed an expected-record-count check.

## Fetch and run in one command

~~~bash
python3 tests/compatibility/run_compatibility.py all --tier standard
~~~

For the large HPRC case:

~~~bash
python3 tests/compatibility/run_compatibility.py all \
  --tier large \
  --timeout 3600
~~~

## Reproducibility notes

Public GitHub fixtures use commit-pinned raw URLs so upstream `main`/`master` changes do not silently change the corpus. The HPRC Zenodo file is immutable and the manifest records its published MD5; the local lock records the downloaded source SHA-256 and the decompressed GFA SHA-256.

The hifiasm case is deliberately different: it tests the **current installed hifiasm output dialect**. Its lock entry records the executable/version, input hash and generated GFA hash.

Compatibility runs are regression tests, not publication performance benchmarks. Use `benchmarks/run_benchmarks.py --mode publication` for timing/memory claims.
