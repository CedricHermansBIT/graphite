# Layout and interaction

## Rust initial layout backend

Graphite uses a Graphite-specific Rust implementation as its default initial-layout backend. The native Bandage/OGDF FMMM bridge is an optional Cargo feature used as a reference backend for validation, benchmarking and reproducibility.

Both backends receive the same reduced Graphite representation. For graphs above 100,000 active physics points, each contig is represented by its two endpoints and full visual length; intermediate render points are interpolated after layout. Circular components and isolated contigs bypass the general solver.

The Rust backend now follows the important multilevel and force-scheduling ideas used by Bandage's bundled OGDF FMMM implementation, while replacing OGDF's New Multipole Method with a Rust Barnes-Hut implementation:

- loop-free simple-graph preprocessing with parallel-edge length averaging;
- deterministic solar-system coarsening with low-mass sun selection;
- path-aware coarse edge lengths that include distances from fine nodes to their dedicated suns;
- hierarchy mass used only for multilevel selection, not as force charge or inertia;
- topology-aware coarse-to-fine placement using dedicated-sun distances and inter-solar lambda constraints with a 5% deterministic waggle;
- exact repulsion for levels below 175 nodes and Barnes-Hut repulsion above that threshold;
- unit-charge repulsive forces and Bandage/OGDF's `fmNew` attractive force shape;
- coarse-heavy iteration scheduling, including at least 100 iterations for levels with 500 nodes or fewer;
- force scaling based on current drawing size and oscillation damping;
- Bandage-style postprocessing with rescaling and low-repulsion/high-spring fine tuning;
- Bandage's simple split/merge untangling pass;
- Rayon-parallel repulsive-force evaluation and independent connected-component solves;
- Graphite's existing component packing after the solver completes.

This is a clean Graphite-specific implementation of the algorithmic ideas, not a source translation of OGDF's FMMM/NMM code.

For headless comparison:

```bash
./target/release/graphite --benchmark --layout-backend bandage graph.gfa
./target/release/graphite --benchmark --layout-backend rust graph.gfa
```

The same selector works in the GUI:

```bash
./target/release/graphite --layout-backend bandage graph.gfa
./target/release/graphite --layout-backend rust graph.gfa
```

The benchmark JSON includes `layout_backend`.

Initial layout uses the actual OGDF FMMM implementation bundled in `Bandage/ogdf`.
The native wrapper follows `Bandage/program/graphlayoutworker.cpp` with Graphite-specific speed settings: normally 12 fixed iterations and 8 fine-tuning iterations, reduced to 3 and 1 respectively when the native layout receives more than 50,000 nodes; multipole precision is 2 and component rotation uses 50 steps. A fixed random seed makes repeated loads reproducible.
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
component moves as a unit. While the pointer is held, graph-link springs and
near-field repulsion keep the same strengths used by ordinary layout relaxation.
Dragging only changes the movement limit and damping so the grabbed region can
keep following the cursor without changing the component's force balance.
Rust-side relaxation settles the local geometry after release. Automatic packing
stops after manual placement to preserve edits; manually placed components may
therefore overlap.

Interactive repulsion uses reusable hashed spatial buckets, an actual distance
cutoff, and queries confined to the same connected component. Settled contigs
and rings have no simulation springs. Position updates reuse buffers rather than
cloning all springs and topology. Dense near-field cells can still incur
quadratic work. Overview rendering culls offscreen edges, merges indistinguishable
dots, and draws visible contigs as single polylines.

## Remote UI mode

For X11 forwarding over SSH, use `--remote-ui` to reduce continuous layout traffic without slowing the layout solver itself:

```bash
ssh -YC server
./target/release/graphite --remote-ui --layout-backend rust graph.gfa
```

Normal mode publishes layout snapshots and requests animation repaints about every 16 ms. Remote UI mode changes both cadences to 100 ms (about 10 Hz). Pointer, keyboard and other egui input events can still trigger immediate frames, so the throttle mainly affects continuous background layout animation and the large position-buffer copies associated with it. Once the graph is settled the UI remains event-driven in either mode.

## Building

The default build is Rust-only:

```bash
cargo build --release
```

It does not access the `Bandage` submodule and does not require a C++ compiler.

The Bandage/OGDF reference backend is opt-in:

```bash
git submodule update --init Bandage
cargo build --release --features ogdf
```

Only when the `ogdf` feature is enabled does `build.rs` compile the Bandage
OGDF source into a static library. Qt is not needed:
`native/qt_geometry` supplies only the point and segment-intersection operations
used by Bandage's split untangler. The submodule source and its license notices
remain intact; see `Bandage/ogdf/LICENSE.txt` and the Bandage source headers.

The default Linux-to-Windows build is likewise Rust-only:

```bash
cargo xwin build --release --target x86_64-pc-windows-msvc
```

To include OGDF, add `--features ogdf`. For that target, the build script wraps
Rust's bundled `lld-link` in LLVM librarian mode. A separately installed
`llvm-lib` executable is not required.

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
