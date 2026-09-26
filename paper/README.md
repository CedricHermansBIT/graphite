# Graphite preprint

This directory contains the manuscript, publication figures, and benchmark records for the Graphite preprint. The `paper` branch is based on the current Graphite implementation; the manuscript describes that implementation only.

## Build

```bash
cd paper
latexmk -pdf main.tex
```

## Manuscript status

The manuscript is content-complete for the current feature set:

- native desktop and WebAssembly browser frontends are described;
- browser performance is not benchmarked or compared; only its explicit resource limits are reported;
- native Rust, CUDA, OGDF, Bandage, and BandageNG benchmark results are included;
- public real-dataset provenance is documented;
- hifiasm is recorded as version 0.25.0-r72;
- the public browser URL and GPL-3.0 license are included;
- the manuscript describes the software as implemented on the `paper` branch.

Before external submission, only publication-administration items remain: choose the target venue/preprint metadata, create an immutable software release/archive if desired, and add funding or acknowledgements if applicable.

## Publication figures

The manuscript uses:

- `fig_wall_time_branched.pdf`
- `fig_wall_time_fragmented.pdf`
- `fig_real_assemblies_wall_time.pdf`
- `fig_real_assemblies_peak_rss.pdf`

They are stored in `paper/figures/`. Regenerate them from the designated combined publication summary with:

```bash
python3 paper/figures/build.py paper/benchmark_runs/combined/summary.csv
```

## Benchmark records

The native benchmark protocol uses one warm-up followed by five measured repetitions per tool and dataset, serial execution, a 900 s timeout, and 50 ms RSS sampling. Graphite Rust, CUDA, and OGDF use `RAYON_NUM_THREADS=8`. The benchmark machine and software environment are summarized in `paper/benchmark_environment.md`.

The manuscript reports the records represented by `paper/benchmark_runs/combined/`. Raw records retain exact per-run provenance and machine metadata.

Current author: Cedric Hermans, ORCID `0000-0002-9310-1876`. The manuscript declares no competing interests.
