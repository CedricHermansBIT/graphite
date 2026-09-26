# Publication benchmark records

The manuscript uses the designated combined dataset in `paper/benchmark_runs/combined/`.

That directory contains:

- `raw.jsonl`: per-run benchmark records used for the manuscript analysis;
- `summary.csv`: the summarized values used by the tables and figures;
- `provenance.json`: source-run and dataset provenance for every selected record.

The publication protocol is serial execution with one warm-up and five measured repetitions per tool and dataset, a 900 s timeout, and 50 ms RSS sampling. Graphite Rust, CUDA and OGDF use `RAYON_NUM_THREADS=8`. Graphite and the standalone Bandage/BandageNG measurements were collected on the same machine with the same dataset set and timing protocol, in separate serial sessions.

The manuscript reports only the records selected into `combined/`. The remaining dated directories are retained as raw provenance and are not used when quoting results, generating manuscript tables, or building the publication figures.

Regenerate the summary and figures with:

```bash
python3 benchmarks/summarize.py paper/benchmark_runs/combined/raw.jsonl \
  --output paper/benchmark_runs/combined/summary.csv
python3 paper/figures/build.py paper/benchmark_runs/combined/summary.csv
```

## CPU-side canvas measurements

The manuscript reports the designated CPU-side canvas dataset for the synthetic branched 100,000- and 1,000,000-segment graphs. Each dataset uses one process warm-up and five measured processes, with 10 warm-up frames and 60 timed frames for both overview and detail views. The reported values summarize CPU-side graph-canvas construction and egui tessellation; they do not measure GPU presentation or complete input-to-display latency.

Exact source identifiers, dataset hashes, binary hashes, machine metadata, and commands remain in the raw run metadata.
