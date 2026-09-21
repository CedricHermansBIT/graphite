# Layout and interaction

## Experimental Rust initial layout

The `rust-layout` branch contains a second initial-layout backend implemented in Rust. It is intentionally kept beside the existing Bandage/OGDF backend while its geometry and performance are evaluated.

The Rust backend operates on the same reduced representation Graphite currently passes to OGDF. For graphs above 100,000 active physics points, each contig is represented by its two endpoints and full visual length; intermediate render points are interpolated after layout. Circular components and isolated contigs continue to bypass the general solver.

The first implementation uses:

- deterministic greedy edge matching to build a multilevel hierarchy;
- coarse-to-fine prolongation with deterministic jitter;
- a flat 2D Barnes-Hut quadtree for approximate long-range repulsion;
- weighted spring attraction using Graphite's desired edge lengths;
- Rayon-parallel per-node repulsive-force evaluation;
- independent connected-component solves, which can execute in parallel;
- Graphite's existing component packing after the solver completes.

This is a clean Graphite-specific implementation, not a source translation of OGDF's FMMM/NMM code.

For headless comparison:

```bash
./target/release/graphite --benchmark --layout-backend bandage graph.gfa
./target/release/graphite --benchmark --layout-backend rust graph.gfa
```

The benchmark JSON includes `layout_backend`. The normal GUI still uses the Bandage backend while the Rust implementation is experimental.

Initial layout uses the actual OGDF FMMM implementation bundled in `Bandage/ogdf`.
The native wrapper follows `Bandage/program/graphlayoutworker.cpp` at quality 1:
12 fixed iterations, 8 fine-tuning iterations, multipole precision 2, and 50
component rotation steps. A fixed random seed makes repeated loads reproducible.
Contigs are represented by chains of points connected at their strand-correct
endpoints, as in `Bandage/graph/debruijnnode.cpp` and `debruijnedge.cpp`.
For graphs above 100,000 active physics points, FMMM receives the two contig
endpoints and the full contig length; intermediate render points are interpolated
afterward. This preserves the Bandage topology while keeping very large layouts
practical.

Isolated contigs and unambiguous circular components bypass the native solver.
This avoids running FMMM on hundreds of thousands of already-settled components.
Their actual polyline bounds are packed with the solved components, including
space for the interiors of circles and the full lengths of isolated contigs.
The finished FMMM placement stays settled until the user drags something.
Parsing and initialization run in a background loading thread.

Components with exactly one unique external link at each segment endpoint are
laid out as circles. This includes single-contig self-loops, two-contig rings,
reverse-oriented segments, and reciprocal duplicate links. Ambiguous and
branching cyclic components are handled by FMMM rather than forced into one ring.

In Grab (`G`) mode, click a contig's visible polyline and drag. The cursor keeps
its original offset and moves the whole contig without stretching it. A circular
component moves as a unit. Dragging gives immediate visual feedback and restarts
Rust-side relaxation. Automatic packing stops after manual placement to preserve
edits; manually placed components may therefore overlap.

Interactive repulsion uses reusable hashed spatial buckets, an actual distance
cutoff, and queries confined to the same connected component. Settled contigs
and rings have no simulation springs. Position updates reuse buffers rather than
cloning all springs and topology. Dense near-field cells can still incur
quadratic work. Overview rendering culls offscreen edges, merges indistinguishable
dots, and draws visible contigs as single polylines.

## Building

A C++14 compiler is now required in addition to the Rust toolchain. `build.rs`
compiles the existing Bandage OGDF source into a static library. Qt is not needed:
`native/qt_geometry` supplies only the point and segment-intersection operations
used by Bandage's split untangler. The bundled source and its license notices
remain intact; see `Bandage/ogdf/LICENSE.txt` and the Bandage source headers.

The existing Linux-to-Windows command remains supported:

```bash
cargo xwin build --release --target x86_64-pc-windows-msvc
```

For this target, the build script wraps Rust's bundled `lld-link` in LLVM
librarian mode. A separately installed `llvm-lib` executable is not required.

## Checks

Run regression tests with `cargo test`. For a real-file benchmark:

```bash
GFA_BENCH_PATH=/path/to/assembly.gfa cargo test benchmark_gfa -- --ignored --nocapture
```

`GFA_BENCH_STEPS` changes the number of Rust relaxation steps (default 20, use 0
to inspect the initial FMMM result). `GFA_BENCH_OUTPUT=/tmp/layout.json` optionally
writes geometry. `GFA_PROFILE=1` prints per-stage iteration timings.
Input GFA files are read-only.

## Component filters

Component filters are evaluated after name, length, and depth predicates, so
their size and topology describe the graph that will actually be displayed.
The topology selector offers all components, circular components, or linear
components. A linear component is an unbranched path with two open ends; an
isolated contig is also linear. Minimum and maximum segment counts filter by
connected-component size, and top-N is applied to the remaining components.
Components and their segments are ordered largest first before layout and
rendering.
