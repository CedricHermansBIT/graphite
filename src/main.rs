#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod app;
mod gfa;
mod graph;
mod layout;
mod render;
mod ui;
mod filter;
mod selection;
mod export;

use anyhow::Result;
use argh::FromArgs;

#[derive(FromArgs)]
/// GFA Visualizer - optimized for large assembly graphs
struct Args {
    /// GFA file to open on startup
    #[argh(positional)]
    file: Option<String>,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Args = argh::from_env();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("GFA Visualizer")
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([800.0, 600.0])
            .with_icon(eframe::icon_data::from_png_bytes(&[]).unwrap_or_default()),
        ..Default::default()
    };

    eframe::run_native(
        "GFA Visualizer",
        native_options,
        Box::new(|cc| Ok(Box::new(app::GfaApp::new(cc, args.file)) as Box<dyn eframe::App>)),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))
}
