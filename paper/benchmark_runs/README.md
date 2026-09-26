# Publication benchmark records

The manuscript uses the designated combined dataset in `paper/benchmark_runs/combined/`.

That directory contains:

- `raw.jsonl`: per-run benchmark records used for the manuscript analysis;
- `summary.csv`: the summarized values used by the tables and figures;
- `provenance.json`: source-run and dataset provenance for every selected record.

The publication protocol is serial execution with one warm-up and five measured repetitions per tool and dataset, a 900 s timeout, and 50 ms RSS sampling. Graphite Rust, CUDA and OGDF use `RAYON_NUM_THREADS=8`. Graphite and the standalone Bandage/BandageNG measurements were collected on the same machine with the same dataset set and timing protocol, in separate serial sessions.

The manuscript reports only the records selected into `combined/`. Its 360 Bandage and BandageNG records are unchanged from the 21 September comparator run. Its 540 Graphite records come from `graphite-2026-09-26-c873f8b/`, which used release source commit `c873f8b491afe3f8896d40288f87f2a4653fde19`. All Graphite records succeeded; all GPU records selected CUDA. The previous records in `canvas/` remain archived but are no longer used for the canvas table.

Recombine the Graphite records with the archived comparator records, then regenerate the summary and figures with:

```bash
python3 paper/benchmark_runs/combine.py \
  paper/benchmark_runs/combined \
  paper/benchmark_runs/graphite-2026-09-26-c873f8b \
  paper/benchmark_runs/combined
python3 benchmarks/summarize.py paper/benchmark_runs/combined/raw.jsonl \
  --output paper/benchmark_runs/combined/summary.csv
python3 paper/figures/build.py paper/benchmark_runs/combined/summary.csv
```

## CPU-side canvas measurements

The manuscript reports `canvas-2026-09-26-c873f8b/` for the synthetic branched 100,000- and 1,000,000-segment graphs. Each dataset uses one process warm-up and five measured processes, with 10 warm-up frames and 60 timed frames for both overview and detail views. The release source was built with the measurement-only `instrumentation.patch` from that directory; `run_ui_frames.py` records the process-level results. The reported values summarize CPU-side graph-canvas construction and egui tessellation; they do not measure GPU presentation or complete input-to-display latency.

To reproduce, copy the patch and `run_ui_frames.py` from this branch into a clean `v0.1.0` checkout. Apply the patch with `git apply --unidiff-zero instrumentation.patch`, build with `cargo build --release --features cuda,ogdf`, then run `run_ui_frames.py` with the two synthetic branched GFAs, 60 frames, five repetitions, and `RAYON_NUM_THREADS=8`. The patch only adds the headless timing command and measurement module; it does not alter Graphite's graph or drawing algorithms.

Exact source identifiers, dataset hashes, binary hashes, machine metadata, and commands remain in the raw run metadata.
