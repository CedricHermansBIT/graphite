# Skyline packing comparison

The optimized implementation retains the original exhaustive search's exact
placements. It removes repeated top-envelope scans and abandons candidate
columns that cannot improve the best vertical offset. Finding `y = 0` ends
the search because later columns cannot win the leftmost tie-break.

Run:

```sh
python3 benchmarks/benchmark_packing.py --repeats 3
```

Local result on 2026-09-23, Intel Xeon Silver 4310 at 2.10 GHz,
Linux x86-64, Rust 1.98.1, release profile:

| Synthetic component profiles | Exhaustive median | Optimized median | Speedup |
| ---: | ---: | ---: | ---: |
| 1,000 | 20.645 ms | 3.174 ms | 6.50× |
| 10,000 | 660.929 ms | 86.032 ms | 7.68× |
| 50,000 | 7,545.240 ms | 880.946 ms | 8.56× |

All three repetitions at every size compared every placement for equality.
The standard test suite additionally compares mixed profiles with empty columns
and checks that their occupied envelopes do not overlap.

This measures only the skyline placement search on generated profiles. It
excludes parsing, component discovery, profile rasterization, layout solving,
rendering and export. These are local measurements, not whole-application
speedups or a guarantee on other hardware or graph shapes.
