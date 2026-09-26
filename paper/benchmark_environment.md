# Publication benchmark environment

This file summarizes the environment and protocol for the results reported in the manuscript. Exact per-run commands, source identifiers, dataset inspections, exit states, and timing records are retained in `paper/benchmark_runs/combined/` and its provenance metadata.

## Host and protocol

- Platform: Linux 6.8.0-142-generic, x86_64, glibc 2.39
- CPU: Intel Xeon Silver 4310 @ 2.10 GHz; 48 logical CPUs
- System memory: 269,877,805,056 bytes (251.3 GiB)
- Python: 3.12.3
- Protocol: serial publication mode, one warm-up and five measured repetitions per tool and dataset
- Timeout: 900 s
- RSS sampling interval: 50 ms
- Graphite Rust, CUDA and OGDF: `RAYON_NUM_THREADS=8`
- CUDA device: NVIDIA H100 PCIe (81,559 MiB), driver 580.178.04, CUDA 12.4 NVRTC selected through `LD_LIBRARY_PATH`
- Graphite source: `v0.1.0`, commit `c873f8b491afe3f8896d40288f87f2a4653fde19`
- Graphite publication binary SHA-256: `29e4ea393dead0db448e88975b5be67c755c7ad09b799792c2d1c5687d2e1621`; build features `cuda,ogdf`

All 540 Graphite benchmark records completed successfully, and all 180 GPU records selected CUDA. Peak RSS is host-process memory and does not include CUDA device memory.

## Comparator software

- Bandage 0.9.0, Ubuntu x86-64 AppImage
- BandageNG 2026.9.1

Graphite and comparator measurements were collected on the same host using the same datasets, timeout, and RSS-sampling protocol. They were collected in separate serial benchmark sessions, so session-level variation remains possible.
The Graphite run was collected on 26 September 2026; the unchanged comparator measurements were collected on 21 September 2026. Inspection of all 30 datasets matched the comparator-run metadata, including file size and GFA record counts.

## Memory-limit note

The benchmark harness did not impose an 8 GiB memory restriction on Bandage. The host exposed 251.3 GiB of RAM, and the Bandage command and environment contained no memory-limit setting. Large synthetic Bandage outcomes are therefore reported as errors without attributing them to a system-memory cap.

## CPU-side canvas measurements

The manuscript also reports CPU-side canvas construction and egui tessellation for synthetic branched 100k and 1M graphs. Each dataset used one process warm-up followed by five measured processes, with 10 warm-up frames and 60 timed frames per viewport. These measurements exclude GPU rendering, display presentation, input delivery, other interface panels, and the minimap; they are therefore a canvas-work measurement rather than a presented-frame-rate benchmark.
The 26 September canvas run used the `v0.1.0` source with only an archived measurement command-line patch. Its binary hash, dataset hashes, raw frames, and patch are in `paper/benchmark_runs/canvas-2026-09-26-c873f8b/`.

## Browser build

The browser frontend is not included in the performance benchmark. Its manuscript description is limited to architecture, availability, and explicit resource safeguards. The current browser limits are:

- 1 GiB maximum shared WebAssembly memory
- 256 MiB input-file limit
- 256 MiB decompressed gzip limit
- 500,000 segments
- 1,000,000 connections
- 2,000,000 total path/walk steps
- 3,000,000 layout physics points
- 128 MiB session files
- 16 million pixels for PNG export
