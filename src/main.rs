#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod app;
#[cfg(feature = "cuda")]
mod cuda_layout;
mod export;
mod filter;
mod gfa;
mod gpu_layout;
mod graph;
mod history;
mod layout;
mod render;
mod rust_layout;
mod selection;
mod session;
mod tasks;
mod ui;

use anyhow::{Context, Result};
use argh::FromArgs;
use std::time::Instant;

#[derive(FromArgs)]
/// Graphite - optimized for large assembly graphs
struct Args {
    /// print Graphite version and exit
    #[argh(switch)]
    version: bool,

    /// run a headless benchmark and print one JSON record to stdout
    #[argh(switch)]
    benchmark: bool,

    /// number of Rust-side refinement iterations in benchmark mode
    #[argh(option, default = "0")]
    benchmark_steps: usize,

    /// optional .svg or .png path to include figure export in benchmark mode
    #[argh(option)]
    benchmark_output: Option<String>,

    /// initial layout backend: rust, gpu (with --features gpu), or bandage (with --features ogdf)
    #[argh(option, default = "String::from(\"rust\")")]
    layout_backend: String,

    /// reduce continuous UI updates for SSH/X11 forwarding
    #[argh(switch)]
    remote_ui: bool,

    /// GFA file to open on startup
    #[argh(positional)]
    file: Option<String>,
}

fn milliseconds(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn parse_layout_backend(value: &str) -> Result<layout::LayoutBackend> {
    if let Some(backend) = layout::LayoutBackend::parse(value) {
        return Ok(backend);
    }

    #[cfg(not(feature = "ogdf"))]
    if matches!(value.to_ascii_lowercase().as_str(), "bandage" | "ogdf") {
        anyhow::bail!(
            "layout backend '{value}' is not included in this build; rebuild with \
             `cargo build --release --features ogdf` to enable the Bandage/OGDF reference backend"
        );
    }

    #[cfg(not(feature = "gpu"))]
    if value.eq_ignore_ascii_case("gpu") {
        anyhow::bail!(
            "layout backend 'gpu' is not included in this build; rebuild with `cargo build --release --features gpu`"
        );
    }

    let available = [
        "rust",
        #[cfg(feature = "gpu")]
        "gpu",
        #[cfg(feature = "ogdf")]
        "bandage",
    ];
    anyhow::bail!(
        "unknown layout backend '{value}'; expected {}",
        available.join(", ")
    );
}

fn run_benchmark(
    path: &str,
    steps: usize,
    output: Option<&str>,
    backend: layout::LayoutBackend,
) -> Result<()> {
    let total_start = Instant::now();
    let file_bytes = std::fs::metadata(path)
        .with_context(|| format!("Cannot stat {path}"))?
        .len();

    let start = Instant::now();
    let gfa = gfa::parse_gfa(path)?;
    let parse_ms = milliseconds(start);

    let start = Instant::now();
    let view = graph::ViewGraph::from_gfa(&gfa, &filter::FilterParams::default());
    let view_graph_ms = milliseconds(start);

    let start = Instant::now();
    let mut layout = layout::Layout::try_new_with_graph_backend(
        &view,
        backend,
        &std::sync::atomic::AtomicBool::new(false),
    )?;
    let initial_layout_ms = milliseconds(start);
    let initial_layout_converged = layout.converged;

    let start = Instant::now();
    for _ in 0..steps {
        layout.step(&view, &layout::LayoutParams::default(), None);
    }
    let refinement_ms = milliseconds(start);

    let export_ms = if let Some(output) = output {
        let start = Instant::now();
        let output_path = std::path::Path::new(output);
        session::ensure_distinct_output(output_path, std::path::Path::new(path))?;
        let extension = output_path
            .extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase)
            .context("benchmark output path must end in .svg or .png")?;

        match extension.as_str() {
            "svg" => export::export_svg(
                output_path,
                &view,
                &layout,
                &render::RenderParams::default(),
            )?,
            "png" => export::export_png(
                output_path,
                &view,
                &layout,
                &render::RenderParams::default(),
            )?,
            _ => anyhow::bail!(
                "unsupported benchmark output format '.{extension}'; expected .svg or .png"
            ),
        }
        Some(milliseconds(start))
    } else {
        None
    };

    let circular_components = view
        .components
        .iter()
        .filter(|component| matches!(component.kind, graph::ComponentKind::Circular))
        .count();
    let linear_components = view
        .components
        .iter()
        .filter(|component| matches!(component.kind, graph::ComponentKind::Linear))
        .count();
    let branched_components = view
        .components
        .iter()
        .filter(|component| matches!(component.kind, graph::ComponentKind::Branched))
        .count();

    let report = serde_json::json!({
        "schema_version": 1,
        "tool": "graphite",
        "graphite_version": env!("CARGO_PKG_VERSION"),
        "layout_backend": backend.as_str(),
        "input": path,
        "file_bytes": file_bytes,
        "segments": view.node_count(),
        "links": view.edge_count(),
        "display_edges": view.edge_count(),
        "gfa_version": gfa.version.label(),
        "gfa_links": gfa.links.len(),
        "gfa_jumps": gfa.jumps.len(),
        "gfa_containments": gfa.containments.len(),
        "gfa_paths": gfa.paths.len(),
        "gfa_walks": gfa.walks.len(),
        "gfa_tags": gfa.tags.len(),
        "parse_warnings": gfa.diagnostics.len(),
        "components": view.components.len(),
        "circular_components": circular_components,
        "linear_components": linear_components,
        "branched_components": branched_components,
        "physics_points": layout.positions.len(),
        "benchmark_steps": steps,
        "initial_layout_converged": initial_layout_converged,
        "final_converged": layout.converged,
        "parse_ms": parse_ms,
        "view_graph_ms": view_graph_ms,
        "initial_layout_ms": initial_layout_ms,
        "refinement_ms": refinement_ms,
        "export_ms": export_ms,
        "export_format": output.and_then(|path| {
            std::path::Path::new(path)
                .extension()
                .and_then(|value| value.to_str())
                .map(str::to_ascii_lowercase)
        }),
        "total_ms": milliseconds(total_start)
    });
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Args = argh::from_env();
    if args.version {
        println!("Graphite {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    if args.benchmark {
        let file = args
            .file
            .as_deref()
            .context("benchmark mode requires a GFA file")?;
        let backend = parse_layout_backend(&args.layout_backend)?;
        return run_benchmark(
            file,
            args.benchmark_steps,
            args.benchmark_output.as_deref(),
            backend,
        );
    }

    let backend = parse_layout_backend(&args.layout_backend)?;

    let native_options = eframe::NativeOptions {
        // The layout compute device is independent of the display renderer.
        // A compute-only device can accelerate layout without presenting a window.
        #[cfg(feature = "gpu")]
        renderer: if backend == layout::LayoutBackend::Gpu {
            eframe::Renderer::Glow
        } else {
            eframe::Renderer::Wgpu
        },
        viewport: egui::ViewportBuilder::default()
            .with_title("Graphite")
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([800.0, 600.0])
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/graphite-icon.png"))
                    .expect("bundled Graphite icon must be a valid PNG"),
            ),
        ..Default::default()
    };

    eframe::run_native(
        "Graphite",
        native_options,
        Box::new(move |cc| {
            Ok(
                Box::new(app::GfaApp::new(cc, args.file, backend, args.remote_ui))
                    as Box<dyn eframe::App>,
            )
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))
}
