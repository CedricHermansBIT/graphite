# Preprint figures

The manuscript uses these generated vector figures:

- `fig_wall_time_branched.pdf`
- `fig_wall_time_fragmented.pdf`
- `fig_real_assemblies_wall_time.pdf`
- `fig_real_assemblies_peak_rss.pdf`

They are generated from `paper/benchmark_runs/combined/summary.csv` with:

```bash
python3 paper/figures/build.py paper/benchmark_runs/combined/summary.csv
```

The figures compare Graphite Rust, CUDA and OGDF with standalone Bandage and BandageNG where corresponding measurements are available. Keep the vector PDF files for preprint submission.
