# Graphite benchmark framework

This directory contains the reproducible benchmark harness for comparing Graphite with Bandage-style assembly-graph viewers.

The framework records two complementary measurements:

1. **End-to-end process performance** for every tool: wall time, peak resident memory (RSS), CPU time, exit status, timeout status and output size.
2. **Graphite stage timings** from its headless benchmark mode: GFA parsing, view-graph construction, initial layout, optional refinement and SVG export.

Use the end-to-end values for the main cross-tool comparison. Use Graphite's internal timings only to explain where its runtime is spent.

## Build Graphite

```bash
cargo build --release
```

Graphite now has a headless benchmark mode:

```bash
./target/release/graphite \
  --benchmark \
  --benchmark-steps 0 \
  --benchmark-output /tmp/graphite.svg \
  assembly.gfa
```

It prints one JSON record to stdout and exits. `--benchmark-output` is optional. When present, SVG export is timed separately and included in total runtime.

For the main comparison, keep `--benchmark-steps 0`. The native FMMM initial placement is the layout used when a graph first becomes available. Extra Rust-side refinement is mainly relevant after interactive movement, so arbitrary refinement iterations would make a cross-tool comparison harder to interpret.

## Generate synthetic scaling datasets

```bash
python3 benchmarks/generate_synthetic.py benchmarks/data \
  --sizes 10000 100000 500000 1000000 \
  --topologies chain branched fragmented
```

The generator writes deterministic GFA1 graphs and a `manifest.json`. The original scaling files remain plain `S`/`L` graphs with `LN`, `DP` and `RC` segment tags so historical benchmark runs stay directly comparable.

Available topologies:

- `chain`: one linear connected component
- `branched`: a chain with regular skip links
- `fragmented`: many small linear components
- `ring`: one circular component

By default the generator also creates controlled 10k-segment feature datasets:

- `synthetic_tags_10000`: header, segment and link optional tags, including generic/unknown tags
- `synthetic_jumps_10000`: GFA1.2 `J` connections and `SC:i:1` shortcuts
- `synthetic_paths_10000`: chunked `P` path records
- `synthetic_walks_10000`: GFA1.1 `W` walk records with sample/haplotype metadata
- `synthetic_containments_10000`: `C` containment records and tags
- `synthetic_mixed_10000`: tags, valid path/jump transitions, walks and containments together

The feature cases intentionally use one moderate graph size so parser/data-model overhead can be measured without multiplying every 500k/1M scaling run. Change it with `--feature-size`, choose a subset with `--feature-cases`, or reproduce the historical topology-only suite with `--no-feature-cases`.

For example:

```bash
python3 benchmarks/generate_synthetic.py benchmarks/data-features \
  --sizes 10000 \
  --topologies branched \
  --feature-size 100000 \
  --feature-cases tags jumps mixed
```

Synthetic graphs characterize scaling and parser behavior. They should not replace real biological assemblies in the paper. Use the original `S`/`L` topology suite for the clean cross-tool scaling comparison; the feature cases are primarily Graphite parser/regression benchmarks because support for GFA1.1/1.2 records differs between external viewer versions.

To stress parser and file-memory behavior with embedded sequence:

```bash
python3 benchmarks/generate_synthetic.py benchmarks/data-seq \
  --sizes 10000 100000 \
  --topologies chain branched \
  --sequence-bases 1000
```

## Configure the tools

```bash
cp benchmarks/config.example.json benchmarks/config.local.json
```

The example config automatically loads `benchmarks/data/manifest.json` through `dataset_manifests`. Add real assemblies under `datasets`.

Tool commands are arrays, not shell strings. Two placeholders are available:

- `{input}`: absolute path to the GFA file
- `{output}`: temporary SVG path for that run

The example assumes Bandage-compatible image commands. Adjust the executable names or CLI flags for the exact Bandage and Bandage-NG versions installed on the benchmark machine.

For a paper-quality experiment, pin exact versions or commits for every tool.

## Run

```bash
python3 benchmarks/run_benchmarks.py benchmarks/config.local.json
```

Useful subsets:

```bash
python3 benchmarks/run_benchmarks.py benchmarks/config.local.json --tool graphite-rust
python3 benchmarks/run_benchmarks.py benchmarks/config.local.json --dataset synthetic_chain_100000
```

Results go to `benchmarks/results/`:

- `run_info.json`: machine, tool, dataset and configuration metadata
- `raw.jsonl`: full record for every run
- `raw.csv`: flattened table for inspection

The default configuration performs one warm-up and five measured repetitions. Warm-ups are retained but marked `warmup=true`.

Tools are interleaved within each dataset and repetition. This avoids running all Graphite measurements first and all Bandage measurements later, which reduces bias from gradual machine drift during a long run.

The default therefore measures a warm filesystem-cache workload after the warm-up. If cold-cache loading matters, run it as a separate experiment with explicit OS cache control rather than mixing cold and warm runs.


## Exploratory versus publication mode

The runner has two execution modes.

### Exploratory mode

Use this while developing the benchmark set or checking how tools scale:

```bash
python3 benchmarks/run_benchmarks.py benchmarks/config.local.json \
  --mode exploratory \
  --jobs 4
```

Exploratory mode may run several benchmark processes at the same time. This is much faster when Bandage already takes tens of seconds per graph, but those timings are affected by CPU, cache and memory-bandwidth contention. Records therefore contain `mode="exploratory"`, the worker count, and `contention_warning=true` when more than one job is active.

By default, exploratory mode also uses adaptive measured-repeat counts. It first performs two measured pilot runs for every tool/dataset pair and then chooses how many measured repetitions to keep:

- median below 10 s: 5 measured runs
- median from 10 s to below 60 s: 3 measured runs
- median 60 s or higher: 2 measured runs

The thresholds and repetition counts live in `adaptive_repetitions` in the JSON config. Use `--no-adaptive` when you want the full configured repetition count even in exploratory mode.

This means a Bandage dataset taking about 70 s per run stops after two measured repetitions instead of five.

### Publication mode

Use this for numbers that will appear in the manuscript:

```bash
python3 benchmarks/run_benchmarks.py benchmarks/config.local.json \
  --mode publication
```

Publication mode is intentionally serial. Passing `--jobs 2` or higher is rejected so final measurements cannot accidentally be collected under cross-process contention. It always uses the fixed `repetitions` count from the config and ignores adaptive repetition logic.

A practical workflow is to use parallel exploratory mode to choose the final datasets and identify timeout points, then run only that reduced set again in publication mode.

### Thread-count control

Each tool may define an `env` object in the config. The example pins Graphite's Rayon pool:

```json
"env": {
  "RAYON_NUM_THREADS": "8"
}
```

The runner records these environment overrides in both raw results and `run_info.json`. Pick a thread count that matches the benchmark machine and keep it unchanged for all final Graphite measurements.


## Memory measurement

On Linux, the runner uses GNU `/usr/bin/time -v` when available and records maximum resident set size. It also samples `/proc` during execution and falls back to that estimate if GNU time is unavailable.

Raw records include:

- `peak_rss_bytes`
- `peak_rss_source`
- `user_seconds`
- `system_seconds`

Use the same machine and operating-system session for all tools. Avoid unrelated heavy jobs. For final manuscript numbers, a dedicated workstation or reserved compute node is preferable.

## Timeouts and failures

The example timeout is 900 seconds. A timeout or crash remains in the raw dataset. Do not remove failed large graphs just because another tool completed them. Failure at a predeclared resource limit is itself a benchmark result.

## Summarize

```bash
python3 benchmarks/summarize.py benchmarks/results/raw.jsonl
```

The summary reports median and interquartile range for wall time and peak RSS. It also reports median Graphite stage timings when available.

Five measured runs after one warm-up are a reasonable starting point. Increase repetitions if variance is large.

## Plot scaling curves

Plotting requires Matplotlib:

```bash
python3 -m pip install matplotlib
python3 benchmarks/plot_results.py benchmarks/results/summary.csv
```

The plotting script produces separate SVG timing and memory plots for each synthetic topology.

## Recommended manuscript design

Use two groups of benchmarks.

### Controlled scaling

Use identical generated topology at increasing segment counts. At minimum include a large connected/branched graph and a fragmented graph. These stress different layout paths. Sequence-bearing synthetic files can be reported separately as a parser/memory experiment.

### Real assemblies

Use several assembly graphs from different biological and assembly contexts. Record:

- assembler and version
- sequencing technology
- GFA file size
- segment count
- link count
- total segment length

Do not select only graphs where Graphite is known to perform well.

For the primary cross-tool result, compare the same operation: load the GFA, compute the default layout and write an SVG. Report median wall time and peak RSS. If a tool fails or exceeds the timeout, report that outcome.

Graphite's internal stage timings are diagnostic measurements. Do not compare them directly with another program's end-to-end runtime unless that program is instrumented at equivalent stages.
