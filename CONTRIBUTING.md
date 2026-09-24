# Contributing

Use Rust 1.95 or later and Python 3.11 or later. The default build needs no
submodule. Follow the platform requirements in the README.

Before opening a pull request:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
python3 tests/compatibility/run_compatibility.py all --tier smoke --no-build
```

For changes to layout, run `python3 benchmarks/benchmark_packing.py` and retain
the timing output with the hardware and compiler used. Do not trade away
component separation or deterministic placement merely to improve timings.
For parser changes, add small inline fixtures and run the standard compatibility
corpus when available. User-facing regressions need behavioural tests.

The optional backend can be checked with `git submodule update --init Bandage`
followed by `cargo test --locked --features ogdf`. Keep its upstream notices.

Describe the problem, resulting behaviour and validation in pull requests.
Contributions to Graphite are under the project's GPL-3.0-only license.
