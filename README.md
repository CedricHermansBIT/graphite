# Graphite

<p align="center">
  <img src="assets/graphite-icon.png" width="128" alt="Graphite graph icon">
</p>

Graphite is a fast desktop viewer for large assembly graphs. It is designed around memory-mapped parsing, compact graph structures, asynchronous layout and level-of-detail rendering so navigation remains practical as assemblies grow.

Graphite uses its Graphite-specific multilevel Rust implementation as the default initial-layout backend for connected, non-circular components. The bundled Bandage/OGDF FMMM implementation remains available as a reference backend for validation and benchmarking. Circular components are arranged as rings and components are packed with spacing so they do not overlap.

## Highlights

- Memory-mapped, byte-level GFA parsing that does not copy embedded sequences until needed.
- Scalable Rust multilevel layout with Barnes-Hut repulsion, plus a Bandage/OGDF reference backend for comparison.
- Asynchronous refinement, pan, zoom, rubber-band selection, and direct contig dragging.
- Circular, linear, and branched component classification.
- Filters for segment name, length, depth/coverage, topology, and minimum/maximum segments per component.
- Component sorting by length, segment count, coverage, or read count, with ascending/descending order and a top-N limit.
- A component browser with topology, segment count, total length, mean coverage, and read count. Click a row to select and focus that component.
- A minimap for navigation in large assemblies.
- Depth/coverage and read-count support from common GFA tags, including hifiasm `rd:i` tags.
- Graph colour modes for coverage, length, read count, or a uniform colour.
- Graphite, Midnight, Light, and Paper interface themes under **Settings → Theme**.
- SVG and PNG figure export using the selected UI theme.

## Requirements

- Rust stable with Rust 2024 edition support (Rust 1.85 or newer). Update with `rustup update stable`.
- A C++14 compiler; it builds the bundled Bandage OGDF layout bridge.

On Debian/Ubuntu, install the native windowing libraries before building:

```bash
sudo apt install libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev
```

## Build and run

```bash
cargo build --release
./target/release/graphite path/to/assembly.gfa
```

The file argument is optional: without it, use **File → Open GFA…**.

The Rust backend is the default. Select a backend explicitly when comparing implementations:

```bash
./target/release/graphite --layout-backend rust path/to/assembly.gfa
./target/release/graphite --layout-backend bandage path/to/assembly.gfa
```

For SSH/X11 forwarding, `--remote-ui` reduces continuous layout snapshot and repaint traffic:

```bash
./target/release/graphite --remote-ui --layout-backend rust path/to/assembly.gfa
```

To create the Windows release executable from Linux/WSL:

```bash
cargo xwin build --release --target x86_64-pc-windows-msvc
./target/x86_64-pc-windows-msvc/release/graphite.exe path/to/assembly.gfa
```

`build.rs` supplies an `llvm-lib` compatibility wrapper for `cargo-xwin`, so a separately installed `llvm-lib` is not required.

## Benchmarking

The benchmark framework is part of the main repository under `benchmarks/`. It supports synthetic dataset generation, Graphite/Bandage/BandageNG command configuration, publication-mode serial runs, exploratory parallel runs, JSONL/CSV output, summary statistics, and SVG scaling plots.

Generate the default synthetic datasets:

```bash
python3 benchmarks/generate_synthetic.py
```

Run a serial publication benchmark:

```bash
python3 benchmarks/run_benchmarks.py benchmarks/config.local.json --mode publication
```

For development, individual tools and datasets can be selected, for example:

```bash
python3 benchmarks/run_benchmarks.py benchmarks/config.local.json \
  --mode exploratory --jobs 4 \
  --tool graphite-rust \
  --output benchmarks/results-rust
```

See `benchmarks/README.md` for the complete benchmark workflow.

## Navigation and selection

| Action | Input |
| --- | --- |
| Pan | Middle-mouse drag, or `P` then left drag |
| Zoom | Scroll wheel or touchpad pinch |
| Select a segment | `S` then left click |
| Add to selection | `Shift` + click |
| Rubber-band select | `S` then left drag |
| Move a segment | `G` then drag it |
| Fit the graph | `F` |
| Switch modes | `P` pan, `S` select, `G` move |
| Focus a component | Click its row in the Components browser |
| Navigate the overview | Click or drag in the minimap |
| Copy selected sequence(s) | `Ctrl+C` or **Copy sequence** |

`Ctrl+C` leaves ordinary text copying alone while a text field is focused. For one selected segment it copies a single FASTA record; for multiple selected segments it copies one FASTA record per segment. Segments without embedded sequence are skipped. When a `.noseq.gfa` file is loaded, sequence copy and FASTA export controls are disabled and explain why.

## Filters, display, and components

The left sidebar controls what is visible:

- **Segments** filters names, length, and depth/coverage range.
- **Components** filters topology (**circular**, **linear**, or **branched**) and component size.
- **Order & limit** determines which components appear first and can cap the view to the first N matching components.
- **Appearance** controls colours, labels, edge opacity, scale, and coverage colour range.

The right sidebar contains the paged Components browser and selected-segment details. Selecting a component row selects every segment in it and centers the canvas on that component.

## Export

- **Copy sequence** and **Export FASTA** operate on every selected segment that has embedded sequence.
- **Export CSV stats** exports selected segments, or the full current graph when nothing is selected.
- **Export figure → SVG** creates a scalable vector graph figure.
- **Export figure → PNG** creates a 2400 × 1600 raster graph figure.

SVG and PNG exports use the active graph colours and UI theme background, making them suitable starting points for publication figures.

## Project structure

```text
src/
  main.rs       application entry point and command-line argument
  app.rs        application state, interaction, panels, minimap
  gfa.rs        memory-mapped GFA parser and metadata extraction
  graph.rs      filtered graph and component summaries
  layout.rs     layout representation, backend dispatch and background refinement
  rust_layout.rs Rust multilevel/Barnes-Hut initial layout backend
  render.rs     canvas rendering, colours, hit testing
  filter.rs     filtering and sorting parameters
  selection.rs  selection and rubber-band logic
  export.rs     FASTA, CSV, SVG, and PNG export
  ui.rs         filter, display, component, stats, and selection UI
native/
  bandage_layout.cpp  bridge to bundled Bandage OGDF layout code
Bandage/
  bundled Bandage and OGDF source used by the reference layout backend
benchmarks/
  benchmark runner, synthetic datasets, summaries and plotting utilities
assets/
  graphite-icon.png  application and README icon
  graphite-icon.ico  multi-resolution Windows icon
```
