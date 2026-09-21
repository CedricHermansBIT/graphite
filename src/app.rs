//! Central application state and the eframe `App` implementation.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use egui::{Color32, Context, Key, Pos2, Rect, Vec2};
use egui::containers::panel::{CentralPanel, Panel};

use crate::export::{copy_sequence_to_clipboard, export_csv, AssemblyStats};
use crate::filter::{ColorMode, FilterParams};
use crate::gfa::GfaGraph;
use crate::graph::ViewGraph;
use crate::layout::{Layout, LayoutBackend, LayoutParams, LayoutRunner};
use crate::render::{draw_graph, hit_test_node, RenderParams};
use crate::selection::Selection;
use crate::ui::{
    component_table, display_panel, filter_panel, selection_panel, stats_panel, DisplayOptions,
    ThemePreset,
};

// ── Load state machine ────────────────────────────────────────────────────────

enum LoadState {
    Empty,
    Loading(std::thread::JoinHandle<anyhow::Result<PreparedGraph>>),
    Loaded {
        gfa: Arc<GfaGraph>,
        stats: AssemblyStats,
        view: ViewGraph,
        layout_runner: LayoutRunner,
        layout_snapshot: Layout,
    },
    Error(String),
}

// Parsing, statistics and native FMMM initialization all run off the UI thread.
struct PreparedGraph {
    filter: FilterParams,
    gfa: Arc<GfaGraph>,
    stats: AssemblyStats,
    view: ViewGraph,
    runner: LayoutRunner,
    snapshot: Layout,
}

impl PreparedGraph {
    fn new(
        gfa: Arc<GfaGraph>,
        filter: &FilterParams,
        backend: LayoutBackend,
        publish_interval: Duration,
    ) -> Self {
        let stats = AssemblyStats::compute(&gfa);
        let view = ViewGraph::from_gfa(&gfa, filter);
        let runner = LayoutRunner::start_with_backend(
            Arc::new(view.rebuild_clone()),
            LayoutParams::default(),
            backend,
            publish_interval,
        );
        let snapshot = runner
            .snapshot()
            .expect("new layout mutex cannot be poisoned");
        Self {
            filter: filter.clone(),
            gfa,
            stats,
            view,
            runner,
            snapshot,
        }
    }
}

// ── Interaction mode ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum InteractionMode {
    Pan,
    Select,
    Grab,
}

// ── App ───────────────────────────────────────────────────────────────────────

pub struct GfaApp {
    load_state: LoadState,
    layout_backend: LayoutBackend,
    remote_ui: bool,
    filter: FilterParams,
    display: DisplayOptions,
    selection: Selection,
    zoom: f32,
    pan: Vec2,
    interaction_mode: InteractionMode,
    rubber_start: Option<Pos2>,
    status_msg: String,
    component_query: String,
    component_page: usize,
    pending_focus_nodes: Option<Vec<usize>>,
    show_filter_panel: bool,
    show_display_panel: bool,
    show_stats_panel: bool,
    show_selection_panel: bool,
    /// Fit screen once on first frame after load.
    pending_fit: bool,
    /// Grabbed physics node index for rope drag.
    grabbed_phys: Option<usize>,
    grab_offset: [f32; 2],
    /// Current grab cursor world-space position.
    grab_world: Option<[f32; 2]>,
}

impl GfaApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        initial_file: Option<String>,
        layout_backend: LayoutBackend,
        remote_ui: bool,
    ) -> Self {
        configure_style(&cc.egui_ctx, ThemePreset::Graphite);
        let mut app = Self {
            load_state: LoadState::Empty,
            layout_backend,
            remote_ui,
            filter: FilterParams::default(),
            display: DisplayOptions::default(),
            selection: Selection::default(),
            zoom: 1.0,
            pan: Vec2::ZERO,
            interaction_mode: InteractionMode::Pan,
            rubber_start: None,
            status_msg: if remote_ui {
                "Open a GFA file to start. Remote UI mode is enabled.".to_string()
            } else {
                "Open a GFA file to start.".to_string()
            },
            component_query: String::new(),
            component_page: 0,
            pending_focus_nodes: None,
            show_filter_panel: true,
            show_display_panel: true,
            show_stats_panel: true,
            show_selection_panel: true,
            pending_fit: false,
            grabbed_phys: None,
            grab_offset: [0.0; 2],
            grab_world: None,
        };
        if let Some(path) = initial_file {
            app.start_load(PathBuf::from(path));
        }
        app
    }

    fn layout_publish_interval(&self) -> Duration {
        if self.remote_ui {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(16)
        }
    }

    fn start_load(&mut self, path: PathBuf) {
        self.status_msg = format!("Loading {}…", path.display());
        self.selection.clear();
        self.pan = Vec2::ZERO;
        self.zoom = 1.0;

        self.grabbed_phys = None;
        self.grab_world = None;
        self.pending_focus_nodes = None;
        let filter = self.filter.clone();
        let backend = self.layout_backend;
        let publish_interval = self.layout_publish_interval();
        let handle = std::thread::spawn(move || {
            let gfa = Arc::new(crate::gfa::parse_gfa(&path)?);
            Ok(PreparedGraph::new(
                gfa,
                &filter,
                backend,
                publish_interval,
            ))
        });
        self.load_state = LoadState::Loading(handle);
    }

    fn check_loading(&mut self, ctx: &Context) {
        let ready = if let LoadState::Loading(h) = &self.load_state {
            h.is_finished()
        } else {
            false
        };

        if ready {
            let old = std::mem::replace(&mut self.load_state, LoadState::Empty);
            if let LoadState::Loading(handle) = old {
                match handle
                    .join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("thread panic")))
                {
                    Ok(PreparedGraph {
                        filter,
                        gfa,
                        stats,
                        view,
                        runner,
                        snapshot,
                    }) => {
                        if filter != self.filter {
                            let filter = self.filter.clone();
                            let backend = self.layout_backend;
                            let publish_interval = self.layout_publish_interval();
                            self.load_state = LoadState::Loading(std::thread::spawn(move || {
                                Ok(PreparedGraph::new(
                                    gfa,
                                    &filter,
                                    backend,
                                    publish_interval,
                                ))
                            }));
                            return;
                        }
                        // Auto-scale depth / read-depth color range on initial load
                        let mut min_depth_color = 0.0;
                        let mut max_depth_color = 100.0;
                        let scale_values: Vec<f32> = view
                            .nodes
                            .iter()
                            .filter_map(|n| match self.display.color_mode {
                                ColorMode::ReadCount => n.read_count.map(|v| v as f32),
                                _ => n.depth.map(|v| v as f32),
                            })
                            .collect();
                        if !scale_values.is_empty() {
                            let mn = scale_values.iter().cloned().fold(f32::INFINITY, f32::min);
                            let mx = scale_values
                                .iter()
                                .cloned()
                                .fold(f32::NEG_INFINITY, f32::max);
                            min_depth_color = (mn - 1.0).max(0.0);
                            max_depth_color = if mx == mn { mn + 1.0 } else { mx + 1.0 };
                        }

                        self.status_msg = format!(
                            "Loaded {} segments, {} links, {} jumps, {} paths, {} walks",
                            gfa.segments.len(),
                            gfa.links.len(),
                            gfa.jumps.len(),
                            gfa.paths.len(),
                            gfa.walks.len()
                        );
                        self.display.min_depth_color = min_depth_color;
                        self.display.max_depth_color = max_depth_color;
                        self.load_state = LoadState::Loaded {
                            gfa,
                            stats,
                            view,
                            layout_runner: runner,
                            layout_snapshot: snapshot,
                        };
                        // Signal canvas to auto-fit once layout is shown.
                        self.pending_fit = true;
                    }
                    Err(e) => {
                        self.status_msg = format!("Error: {e}");
                        self.load_state = LoadState::Error(e.to_string());
                    }
                }
            }
        }

        // Poll layout progress.
        if let LoadState::Loaded {
            layout_runner,
            layout_snapshot,
            ..
        } = &mut self.load_state
        {
            if layout_runner.is_running() {
                if layout_runner.update_snapshot(layout_snapshot) {
                    ctx.request_repaint();
                }
            }
        }
    }

    fn rebuild_view(&mut self) {
        self.grabbed_phys = None;
        self.grab_world = None;
        self.pending_focus_nodes = None;
        if let LoadState::Loaded { gfa, .. } = &self.load_state {
            let gfa = gfa.clone();
            let filter = self.filter.clone();
            let backend = self.layout_backend;
            let publish_interval = self.layout_publish_interval();
            self.selection.clear();
            self.status_msg = format!("Computing {} layout…", backend.as_str());
            self.load_state = LoadState::Loading(std::thread::spawn(move || {
                Ok(PreparedGraph::new(
                    gfa,
                    &filter,
                    backend,
                    publish_interval,
                ))
            }));
        }
    }

    fn export_figure(&mut self, path: &std::path::Path, svg: bool) {
        let params = self.render_params();
        let result = match &self.load_state {
            LoadState::Loaded {
                view,
                layout_snapshot,
                ..
            } if svg => crate::export::export_svg(path, view, layout_snapshot, &params),
            LoadState::Loaded {
                view,
                layout_snapshot,
                ..
            } => crate::export::export_png(path, view, layout_snapshot, &params),
            _ => Err(anyhow::anyhow!("No loaded graph to export")),
        };
        self.status_msg = match result {
            Ok(()) if svg => format!("SVG figure exported to {}.", path.display()),
            Ok(()) => format!("PNG figure exported to {}.", path.display()),
            Err(error) => format!("Figure export error: {error}"),
        };
    }

    fn top_menu(&mut self, root_ui: &mut egui::Ui) {
        let ctx = root_ui.ctx().clone();
        let can_export_figure = matches!(&self.load_state, LoadState::Loaded { .. });
        let (can_export_fasta, fasta_disabled_reason) =
            if let LoadState::Loaded { gfa, view, .. } = &self.load_state {
                let has_selected_sequence = self.selection.nodes.iter().any(|&node_index| {
                    view.nodes
                        .get(node_index)
                        .and_then(|node| gfa.segments.get(node.seg_idx))
                        .is_some_and(|segment| !segment.seq_range.is_empty())
                });
                let reason = if self.selection.is_empty() {
                    "Select one or more segments before exporting FASTA."
                } else if gfa.sequence_segment_count == 0 {
                    "This graph contains no embedded nucleotide sequences. FASTA export is unavailable for .noseq GFA files."
                } else {
                    "The current selection contains no embedded nucleotide sequence."
                };
                (has_selected_sequence, reason)
            } else {
                (false, "Load a graph and select one or more segments first.")
            };

        Panel::top("menu_bar").show(root_ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open GFA…").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("GFA", &["gfa", "gfa1", "gfa2"])
                            .pick_file()
                        {
                            self.start_load(path);
                        }
                        ui.close();
                    }
                    ui.separator();
                    let export_fasta_response = ui
                        .add_enabled(
                            can_export_fasta,
                            egui::Button::new("Export FASTA (selected)…"),
                        )
                        .on_disabled_hover_text(fasta_disabled_reason);
                    if export_fasta_response.clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("FASTA", &["fa", "fasta"])
                            .save_file()
                        {
                            if let LoadState::Loaded { gfa, view, .. } = &self.load_state {
                                match crate::export::export_fasta(&path, gfa, view, &self.selection)
                                {
                                    Ok(_) => self.status_msg = "FASTA exported.".to_string(),
                                    Err(e) => self.status_msg = format!("Export error: {e}"),
                                }
                            }
                        }
                        ui.close();
                    }
                    if ui.button("Export CSV stats…").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("CSV", &["csv"])
                            .save_file()
                        {
                            if let LoadState::Loaded { gfa, view, .. } = &self.load_state {
                                match export_csv(&path, gfa, view, &self.selection) {
                                    Ok(_) => self.status_msg = "CSV exported.".to_string(),
                                    Err(e) => self.status_msg = format!("Export error: {e}"),
                                }
                            }
                        }
                        ui.close();
                    }
                    ui.separator();
                    ui.menu_button("Export figure…", |ui| {
                        let svg = ui
                            .add_enabled(can_export_figure, egui::Button::new("SVG (vector)…"))
                            .on_disabled_hover_text("Load a graph before exporting a figure.");
                        if svg.clicked() {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter("SVG", &["svg"])
                                .save_file()
                            {
                                self.export_figure(&path, true);
                            }
                            ui.close();
                        }
                        let png = ui
                            .add_enabled(can_export_figure, egui::Button::new("PNG (2400 × 1600)…"))
                            .on_disabled_hover_text("Load a graph before exporting a figure.");
                        if png.clicked() {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter("PNG", &["png"])
                                .save_file()
                            {
                                self.export_figure(&path, false);
                            }
                            ui.close();
                        }
                    });
                });

                ui.menu_button("View", |ui| {
                    ui.checkbox(&mut self.show_filter_panel, "Filter panel");
                    ui.checkbox(&mut self.show_display_panel, "Display panel");
                    ui.checkbox(&mut self.show_stats_panel, "Stats panel");
                    ui.checkbox(&mut self.show_selection_panel, "Selection panel");
                    ui.separator();
                    if ui.button("Reset view").clicked() {
                        self.zoom = 1.0;
                        self.pan = Vec2::ZERO;
                        ui.close();
                    }
                    if ui.button("Fit to screen").clicked() {
                        self.fit_to_screen();
                        ui.close();
                    }
                });

                ui.menu_button("Select", |ui| {
                    if ui.button("Select all").clicked() {
                        if let LoadState::Loaded { view, .. } = &self.load_state {
                            self.selection.clear();
                            for i in 0..view.nodes.len() {
                                self.selection.nodes.insert(i);
                            }
                        }
                        ui.close();
                    }
                    if ui.button("Deselect all").clicked() {
                        self.selection.clear();
                        ui.close();
                    }
                    if ui.button("Invert selection").clicked() {
                        if let LoadState::Loaded { view, .. } = &self.load_state {
                            let n = view.nodes.len();
                            let old = self.selection.nodes.clone();
                            self.selection.clear();
                            for i in 0..n {
                                if !old.contains(&i) {
                                    self.selection.nodes.insert(i);
                                }
                            }
                        }
                        ui.close();
                    }
                });

                ui.menu_button("Settings", |ui| {
                    ui.menu_button("Theme", |ui| {
                        for theme in [
                            ThemePreset::Graphite,
                            ThemePreset::Midnight,
                            ThemePreset::Light,
                            ThemePreset::Paper,
                        ] {
                            if ui
                                .selectable_value(&mut self.display.theme, theme, theme.label())
                                .changed()
                            {
                                configure_style(&ctx, theme);
                                ctx.request_repaint();
                            }
                        }
                    });
                });

                ui.separator();
                for (mode, label, shortcut) in [
                    (InteractionMode::Pan, "Pan", "P"),
                    (InteractionMode::Select, "Select", "S"),
                    (InteractionMode::Grab, "Move", "G"),
                ] {
                    if ui
                        .selectable_label(
                            self.interaction_mode == mode,
                            format!("{label}  {shortcut}"),
                        )
                        .clicked()
                    {
                        self.interaction_mode = mode;
                    }
                }
                if ui.button("Fit  F").clicked() {
                    self.fit_to_screen();
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(&self.status_msg)
                            .small()
                            .color(Color32::GRAY),
                    );
                });
            });
        });
    }

    fn left_panels(&mut self, root_ui: &mut egui::Ui) {
        let ctx = root_ui.ctx().clone();
        Panel::left("left_panel")
            .resizable(true)
            .default_size(300.0)
            .size_range(270.0..=420.0)
            .show(root_ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                    if self.show_filter_panel {
                        let changed = filter_panel(ui, &mut self.filter);
                        if changed {
                            self.rebuild_view();
                        }
                        ui.separator();
                    }
                    if self.show_display_panel {
                        let previous_theme = self.display.theme;
                        if display_panel(ui, &mut self.display) {
                            if self.display.theme != previous_theme {
                                configure_style(&ctx, self.display.theme);
                            }
                            ctx.request_repaint();
                        }
                        ui.separator();
                    }
                    if self.show_stats_panel {
                        if let LoadState::Loaded { stats, view, .. } = &self.load_state {
                            stats_panel(ui, stats, view);
                            ui.separator();
                        }
                    }
                });
            });
    }

    fn right_panel(&mut self, root_ui: &mut egui::Ui) {
        let ctx = root_ui.ctx().clone();
        if !self.show_selection_panel {
            return;
        }
        let (mut copy_seq, mut export_fasta, mut sel_component) = (false, false, false);
        let mut focus_component = None;

        Panel::right("right_panel")
            .resizable(true)
            .default_size(280.0)
            .size_range(240.0..=420.0)
            .show(root_ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                    if let LoadState::Loaded { gfa, view, .. } = &self.load_state {
                        focus_component = component_table(
                            ui,
                            &view.components,
                            &mut self.component_query,
                            &mut self.component_page,
                        );
                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(8.0);
                        selection_panel(
                            ui,
                            &self.selection,
                            gfa,
                            view,
                            &mut copy_seq,
                            &mut export_fasta,
                            &mut sel_component,
                        );
                    } else {
                        ui.label("No file loaded.");
                    }
                });
            });

        if copy_seq {
            self.copy_selected_sequences(&ctx);
        }
        if export_fasta {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("FASTA", &["fa", "fasta"])
                .save_file()
            {
                if let LoadState::Loaded { gfa, view, .. } = &self.load_state {
                    match crate::export::export_fasta(&path, gfa, view, &self.selection) {
                        Ok(_) => self.status_msg = "FASTA exported.".to_string(),
                        Err(e) => self.status_msg = format!("Error: {e}"),
                    }
                }
            }
        }
        if sel_component {
            if let Some(&start) = self.selection.nodes.iter().next() {
                if let LoadState::Loaded { view, .. } = &self.load_state {
                    self.selection.select_component(start, view, false);
                }
            }
        }
        if let Some(nodes) = focus_component {
            self.selection.clear();
            self.selection.nodes.extend(nodes.iter().copied());
            self.pending_focus_nodes = Some(nodes);
            self.status_msg = format!("Focused {}-segment component.", self.selection.node_count());
        }
    }

    fn copy_selected_sequences(&mut self, ctx: &Context) {
        let sequence = match &self.load_state {
            LoadState::Loaded { gfa, view, .. } => {
                copy_sequence_to_clipboard(gfa, view, &self.selection)
            }
            _ => None,
        };
        if let Some(sequence) = sequence {
            ctx.copy_text(sequence);
            self.status_msg = "Selected sequence(s) copied to clipboard.".to_string();
        }
    }

    fn canvas(&mut self, root_ui: &mut egui::Ui) {
        let ctx = root_ui.ctx().clone();
        CentralPanel::default()
            .frame(egui::Frame::new().fill(self.display.theme.canvas_background()))
            .show(root_ui, |ui| {
            match &self.load_state {
                LoadState::Empty => {
                    ui.centered_and_justified(|ui| {
                        ui.label("Open a GFA file via File → Open GFA…");
                    });
                    return;
                }
                LoadState::Loading(_) => {
                    ui.centered_and_justified(|ui| {
                        ui.spinner();
                        ui.label("Loading graph and computing layout…");
                    });
                    return;
                }
                LoadState::Error(msg) => {
                    ui.centered_and_justified(|ui| {
                        ui.colored_label(Color32::RED, msg);
                    });
                    return;
                }
                LoadState::Loaded { .. } => {}
            }

            let response = ui.allocate_response(ui.available_size(), egui::Sense::click_and_drag());
            let viewport = response.rect;
            if let Some(nodes) = self.pending_focus_nodes.take() {
                self.focus_nodes_with_viewport(&nodes, viewport);
            }

            // ── Interaction ──────────────────────────────────────────────────

            // Zoom with scroll / pinch.
            if response.hovered() {
                // Use zoom_delta (trackpad pinch) if available; fall back to scroll.
                let delta_zoom = ctx.input(|i| i.zoom_delta());
                let was_zoomed = delta_zoom != 1.0;

                let scroll = ctx.input(|i| i.smooth_scroll_delta.y);
                let was_scrolled = scroll != 0.0;

                if was_scrolled || was_zoomed {
                    let old_zoom = self.zoom;
                    if was_zoomed && !was_scrolled {
                        // Pure touchpad pinch.
                        self.zoom = (self.zoom * delta_zoom).clamp(0.00001, 1000.0);
                    } else if was_scrolled {
                        // Mouse wheel — exponential scaling for smooth control.
                        let factor = (scroll * -0.001).exp();
                        self.zoom = (self.zoom * factor).clamp(0.00001, 1000.0);
                    }

                    // "Pinch-to-cursor": keep the world point under the mouse stationary.
                    if let Some(cursor) = ctx.input(|i| i.pointer.hover_pos()) {
                        let vp_center = viewport.center();
                        let world_x = (cursor.x - vp_center.x - self.pan.x) / old_zoom;
                        let world_y = (cursor.y - vp_center.y - self.pan.y) / old_zoom;
                        self.pan.x = cursor.x - vp_center.x - world_x * self.zoom;
                        self.pan.y = cursor.y - vp_center.y - world_y * self.zoom;
                    }
                }
            }

            // Pan with middle-mouse drag or left-drag in Pan mode.
            let mmb = ctx.input(|i| i.pointer.middle_down());
            if mmb
                || (self.interaction_mode == InteractionMode::Pan
                    && response.dragged_by(egui::PointerButton::Primary))
            {
                self.pan += response.drag_delta();
            }

            // Selection click.
            if self.interaction_mode == InteractionMode::Select {
                if response.clicked() {
                    let click_pos = response.interact_pointer_pos().unwrap_or_default();
                    let add = ctx.input(|i| i.modifiers.shift);
                    if let LoadState::Loaded {
                        view,
                        layout_snapshot,
                        ..
                    } = &self.load_state
                    {
                        let render_p = self.render_params();
                        if let Some(ni) =
                            hit_test_node(click_pos, viewport, view, layout_snapshot, &render_p)
                        {
                            self.selection.select_node(ni, add);
                            self.status_msg = format!("Selected: {}", view.nodes[ni].name);
                        } else if !add {
                            self.selection.clear();
                        }
                    }
                }

                // Rubber-band drag.
                if response.drag_started_by(egui::PointerButton::Primary) {
                    self.rubber_start = response.interact_pointer_pos();
                }
                if response.drag_stopped() {
                    if let Some(start) = self.rubber_start.take() {
                        if let Some(end) = response.interact_pointer_pos() {
                            let rect = Rect::from_two_pos(start, end);
                            if rect.width() > 4.0 || rect.height() > 4.0 {
                                let add = ctx.input(|i| i.modifiers.shift);
                                if let LoadState::Loaded {
                                    view,
                                    layout_snapshot,
                                    ..
                                } = &self.load_state
                                {
                                    self.selection.rubber_band_select(
                                        rect,
                                        view,
                                        layout_snapshot,
                                        self.zoom,
                                        self.pan,
                                        viewport.center(),
                                        add,
                                    );
                                }
                            }
                        }
                    }
                }
            }

            // Mode toggle with 'S' / 'P' / 'G' shortcuts.
            ctx.input(|i| {
                if i.key_pressed(Key::P) {
                    self.interaction_mode = InteractionMode::Pan;
                }
                if i.key_pressed(Key::S) {
                    self.interaction_mode = InteractionMode::Select;
                }
                if i.key_pressed(Key::G) {
                    self.interaction_mode = InteractionMode::Grab;
                }
                if i.key_pressed(Key::F) {
                    self.fit_to_screen_with_viewport(viewport);
                }
            });

            // ── Grab / rope drag ─────────────────────────────────────────────
            if self.interaction_mode == InteractionMode::Grab {
                let vp_center = viewport.center();
                let cursor_world = ctx.input(|i| i.pointer.hover_pos()).map(|cursor| {
                    let wx = (cursor.x - vp_center.x - self.pan.x) / self.zoom;
                    let wy = (cursor.y - vp_center.y - self.pan.y) / self.zoom;
                    [wx, wy]
                });

                let just_pressed = response.drag_started_by(egui::PointerButton::Primary);
                let dragging = response.dragged_by(egui::PointerButton::Primary);
                let just_released = response.drag_stopped_by(egui::PointerButton::Primary);

                if just_pressed {
                    let render_p = self.render_params();
                    if let (
                        Some(screen),
                        LoadState::Loaded {
                            view,
                            layout_snapshot,
                            ..
                        },
                    ) = (ctx.input(|i| i.pointer.press_origin()), &self.load_state)
                    {
                        self.grabbed_phys =
                            hit_test_node(screen, viewport, view, layout_snapshot, &render_p)
                                .map(|ni| layout_snapshot.node_pts_start[ni]);
                        if let Some(pi) = self.grabbed_phys {
                            let w = [
                                (screen.x - vp_center.x - self.pan.x) / self.zoom,
                                (screen.y - vp_center.y - self.pan.y) / self.zoom,
                            ];
                            let p = layout_snapshot.positions[pi];
                            self.grab_offset = [p[0] - w[0], p[1] - w[1]];
                        }
                    }
                }

                if dragging {
                    if let Some(wpos) = cursor_world {
                        self.grab_world =
                            Some([wpos[0] + self.grab_offset[0], wpos[1] + self.grab_offset[1]]);
                    }
                } else if just_released {
                    self.grabbed_phys = None;
                    self.grab_world = None;
                }

                // Push (cursor_pos, grabbed_node) to layout runner.
                if let LoadState::Loaded {
                    layout_runner,
                    layout_snapshot,
                    ..
                } = &mut self.load_state
                {
                    let att = self
                        .grab_world
                        .zip(self.grabbed_phys)
                        .map(|(pos, pi)| (pos, pi));
                    layout_runner.set_attractor(att);
                    if let Some((pos, pi)) = att {
                        layout_snapshot.drag_to(pos, pi);
                    }
                    if dragging {
                        ctx.request_repaint();
                    }
                }
            } else {
                // Clear grab when mode switches.
                self.grabbed_phys = None;
                self.grab_world = None;
                if let LoadState::Loaded { layout_runner, .. } = &self.load_state {
                    layout_runner.set_attractor(None);
                }
            }

            // ── Draw ─────────────────────────────────────────────────────────
            if let LoadState::Loaded {
                view,
                layout_snapshot,
                ..
            } = &self.load_state
            {
                let painter = ui.painter_at(viewport);
                // Background.
                painter.rect_filled(viewport, 0.0, self.display.theme.canvas_background());

                let rp = self.render_params();
                draw_graph(
                    &painter,
                    viewport,
                    view,
                    layout_snapshot,
                    &self.selection,
                    &rp,
                );

                // Rubber-band rect.
                if self.interaction_mode == InteractionMode::Select {
                    if let Some(start) = self.rubber_start {
                        if response.dragged_by(egui::PointerButton::Primary) {
                            if let Some(cur) = response.interact_pointer_pos() {
                                let rect = Rect::from_two_pos(start, cur);
                                painter.rect_stroke(
                                    rect,
                                    0.0,
                                    egui::Stroke::new(1.0, Color32::YELLOW),
                                    egui::StrokeKind::Inside,
                                );
                                painter.rect_filled(
                                    rect,
                                    0.0,
                                    Color32::from_rgba_unmultiplied(255, 255, 100, 20),
                                );
                            }
                        }
                    }
                }

                // Status overlay (mode indicator).
                painter.text(
                    viewport.min + Vec2::new(8.0, 8.0),
                    egui::Align2::LEFT_TOP,
                    match self.interaction_mode {
                        InteractionMode::Pan => "Mode: Pan  [P/S/G]",
                        InteractionMode::Select => "Mode: Select  [P/S/G, F=fit]",
                        InteractionMode::Grab => "Mode: Grab  [P/S/G] — drag a contig to move it",
                    },
                    egui::FontId::proportional(12.0),
                    self.display.theme.canvas_foreground().gamma_multiply(0.8),
                );

                // Auto-fit once after load (do after layout is available).
                if self.pending_fit {
                    self.fit_to_screen_with_viewport(viewport);
                    self.pending_fit = false;
                }

                // Draw grab attractor cursor.
                if let Some(world) = self.grab_world.filter(|_| self.grabbed_phys.is_some()) {
                    let vp_center = viewport.center();
                    let sx =
                        (world[0] - self.grab_offset[0]) * self.zoom + self.pan.x + vp_center.x;
                    let sy =
                        (world[1] - self.grab_offset[1]) * self.zoom + self.pan.y + vp_center.y;
                    let sp = Pos2::new(sx, sy);
                    painter.circle_stroke(
                        sp,
                        12.0,
                        egui::Stroke::new(2.0, Color32::from_rgb(255, 200, 50)),
                    );
                    painter.circle_filled(sp, 4.0, Color32::from_rgb(255, 200, 50));
                }

                // Layout progress indicator.
                if let LoadState::Loaded {
                    layout_runner,
                    layout_snapshot,
                    ..
                } = &self.load_state
                {
                    if layout_runner.is_running() {
                        let iter = layout_snapshot.iteration;
                        ui.painter_at(viewport).text(
                            viewport.min + Vec2::new(8.0, 28.0),
                            egui::Align2::LEFT_TOP,
                            if layout_snapshot.converged {
                                "Layout settled".to_string()
                            } else {
                                format!("Layout: iter {}", iter)
                            },
                            egui::FontId::proportional(11.0),
                            Color32::from_rgb(100, 200, 100),
                        );
                    }
                }
            }

            if let LoadState::Loaded {
                layout_snapshot, ..
            } = &self.load_state
            {
                if let Some(target) = draw_minimap(
                    ui,
                    viewport,
                    layout_snapshot,
                    self.zoom,
                    self.pan,
                    self.display.theme,
                ) {
                    self.pan = Vec2::new(-target[0] * self.zoom, -target[1] * self.zoom);
                    ctx.request_repaint();
                }
            }
        });

    }

    fn render_params(&self) -> RenderParams {
        let mut min_depth = self.display.min_depth_color;
        let mut max_depth = self.display.max_depth_color;
        let mut min_length = 1.0;
        let mut max_length = 100_000.0;

        if let LoadState::Loaded { view, .. } = &self.load_state {
            if self.display.auto_color_scale && self.display.color_mode == ColorMode::Depth {
                let depths: Vec<f32> = view
                    .nodes
                    .iter()
                    .filter_map(|n| n.depth)
                    .map(|d| d as f32)
                    .collect();
                if !depths.is_empty() {
                    let mn = depths.iter().cloned().fold(f32::INFINITY, f32::min);
                    let mx = depths.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
                    min_depth = mn;
                    max_depth = if mx == mn { mn + 1.0 } else { mx };
                }
            } else if self.display.auto_color_scale
                && self.display.color_mode == ColorMode::ReadCount
            {
                let rcs: Vec<f32> = view
                    .nodes
                    .iter()
                    .filter_map(|n| n.read_count)
                    .map(|rc| rc as f32)
                    .collect();
                if !rcs.is_empty() {
                    let mn = rcs.iter().cloned().fold(f32::INFINITY, f32::min);
                    let mx = rcs.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
                    min_depth = mn;
                    max_depth = if mx == mn { mn + 1.0 } else { mx };
                }
            }

            // Always calculate actual min/max lengths dynamically for the length scale!
            let lengths: Vec<f32> = view.nodes.iter().map(|n| n.length as f32).collect();
            if !lengths.is_empty() {
                let mn = lengths.iter().cloned().fold(f32::INFINITY, f32::min);
                let mx = lengths.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
                min_length = mn;
                max_length = if mx == mn { mn + 1.0 } else { mx };
            }
        }

        RenderParams {
            zoom: self.zoom,
            pan: self.pan,
            color_mode: self.display.color_mode.clone(),
            show_labels: self.display.show_labels,
            edge_opacity: self.display.edge_opacity,
            edge_visible_min_zoom: 0.0,
            min_depth_color: min_depth,
            max_depth_color: max_depth,
            min_length_color: min_length,
            max_length_color: max_length,
            node_scale: self.display.node_scale,
            canvas_foreground: self.display.theme.canvas_foreground(),
            canvas_background: self.display.theme.canvas_background(),
        }
    }

    fn fit_to_screen(&mut self) {
        // Fit using a dummy viewport size.
        self.fit_to_screen_with_viewport(Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0)));
    }

    fn fit_to_screen_with_viewport(&mut self, viewport: Rect) {
        if let LoadState::Loaded {
            layout_snapshot,
            view,
            ..
        } = &self.load_state
        {
            if layout_snapshot.positions.is_empty() {
                return;
            }
            if view.nodes.is_empty() || layout_snapshot.num_nodes() == 0 {
                return;
            }
            let mut min_x = f32::MAX;
            let mut max_x = f32::MIN;
            let mut min_y = f32::MAX;
            let mut max_y = f32::MIN;

            // Iterate over every physics-node position (the positions Vec now contains
            // one entry per physics node, not per GFA node).
            for &[x, y] in &layout_snapshot.positions {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }

            let gw = (max_x - min_x).max(1.0);
            let gh = (max_y - min_y).max(1.0);
            let cx = (min_x + max_x) * 0.5;
            let cy = (min_y + max_y) * 0.5;
            let zoom_x = viewport.width() * 0.85 / gw;
            let zoom_y = viewport.height() * 0.85 / gh;
            self.zoom = zoom_x.min(zoom_y).clamp(0.00001, 1000.0);
            self.pan = Vec2::new(-cx * self.zoom, -cy * self.zoom);
        }
    }

    fn focus_nodes_with_viewport(&mut self, nodes: &[usize], viewport: Rect) {
        let LoadState::Loaded {
            layout_snapshot,
            ..
        } = &self.load_state
        else {
            return;
        };
        let mut min_x = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        for &node in nodes {
            if node >= layout_snapshot.num_nodes() {
                continue;
            }
            for &[x, y] in layout_snapshot.pts(node) {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
        if !min_x.is_finite() {
            return;
        }
        let width = (max_x - min_x).max(80.0);
        let height = (max_y - min_y).max(80.0);
        let center_x = (min_x + max_x) * 0.5;
        let center_y = (min_y + max_y) * 0.5;
        self.zoom = (viewport.width() * 0.72 / width)
            .min(viewport.height() * 0.72 / height)
            .clamp(0.00001, 1000.0);
        self.pan = Vec2::new(-center_x * self.zoom, -center_y * self.zoom);
    }
}

/// A sampled overview for very large assemblies. Sampling bounds the per-frame
/// cost while retaining enough geometry to navigate dense graphs.
fn draw_minimap(
    ui: &mut egui::Ui,
    viewport: Rect,
    layout: &Layout,
    zoom: f32,
    pan: Vec2,
    theme: ThemePreset,
) -> Option<[f32; 2]> {
    if layout.positions.is_empty() {
        return None;
    }
    let size = Vec2::new(viewport.width().min(190.0), viewport.height().min(130.0));
    if size.x < 80.0 || size.y < 60.0 {
        return None;
    }
    let max = viewport.max - Vec2::splat(14.0);
    let rect = Rect::from_min_max(max - size, max);
    let step = (layout.positions.len() / 12_000).max(1);
    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for &[x, y] in layout.positions.iter().step_by(step) {
        min_x = min_x.min(x);
        max_x = max_x.max(x);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    }
    if !min_x.is_finite() {
        return None;
    }
    let width = (max_x - min_x).max(1.0);
    let height = (max_y - min_y).max(1.0);
    let scale = (rect.width() / width).min(rect.height() / height) * 0.92;
    let offset = rect.center()
        - Vec2::new((min_x + max_x) * 0.5 * scale, (min_y + max_y) * 0.5 * scale);
    let to_map = |point: [f32; 2]| Pos2::new(point[0] * scale + offset.x, point[1] * scale + offset.y);
    let from_map = |point: Pos2| [(point.x - offset.x) / scale, (point.y - offset.y) / scale];

    let painter = ui.painter();
    painter.rect_filled(
        rect,
        6.0,
        Color32::from_rgba_unmultiplied(
            theme.canvas_background().r(),
            theme.canvas_background().g(),
            theme.canvas_background().b(),
            235,
        ),
    );
    painter.rect_stroke(
        rect,
        6.0,
        egui::Stroke::new(1.0, theme.canvas_foreground().gamma_multiply(0.55)),
        egui::StrokeKind::Inside,
    );
    for &point in layout.positions.iter().step_by(step) {
        painter.circle_filled(to_map(point), 0.7, theme.canvas_foreground().gamma_multiply(0.72));
    }

    let center = viewport.center();
    let visible_min = [
        (viewport.min.x - center.x - pan.x) / zoom,
        (viewport.min.y - center.y - pan.y) / zoom,
    ];
    let visible_max = [
        (viewport.max.x - center.x - pan.x) / zoom,
        (viewport.max.y - center.y - pan.y) / zoom,
    ];
    painter.rect_stroke(
        Rect::from_two_pos(to_map(visible_min), to_map(visible_max)).intersect(rect),
        2.0,
        egui::Stroke::new(1.5, Color32::from_rgb(255, 205, 70)),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.min + Vec2::new(6.0, 5.0),
        egui::Align2::LEFT_TOP,
        "Overview",
        egui::FontId::proportional(10.0),
        theme.canvas_foreground().gamma_multiply(0.8),
    );

    let response = ui.interact(rect, ui.id().with("assembly_minimap"), egui::Sense::click_and_drag());
    if (response.clicked() || response.dragged()) && response.interact_pointer_pos().is_some() {
        return response.interact_pointer_pos().map(from_map);
    }
    None
}

// ── eframe::App ───────────────────────────────────────────────────────────────

impl eframe::App for GfaApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.check_loading(&ctx);
        self.top_menu(ui);
        self.left_panels(ui);
        self.right_panel(ui);
        self.canvas(ui);

        let copy_shortcut = ctx.input(|input| {
            input.modifiers.command && input.key_pressed(Key::C)
        });
        if copy_shortcut && !ctx.text_edit_focused() {
            self.copy_selected_sequences(&ctx);
        }

        // Re-render while layout is running.
        if let LoadState::Loaded {
            layout_runner,
            layout_snapshot,
            ..
        } = &self.load_state
        {
            if layout_runner.is_running() && !layout_snapshot.converged {
                let repaint_interval = if self.remote_ui {
                    Duration::from_millis(100)
                } else {
                    Duration::from_millis(16)
                };
                ctx.request_repaint_after(repaint_interval);
            }
        }
    }
}

// ── Helper trait: clone-able ViewGraph for layout thread ──────────────────────

trait RebuildClone {
    fn rebuild_clone(&self) -> Self;
}

impl RebuildClone for ViewGraph {
    fn rebuild_clone(&self) -> Self {
        Self {
            nodes: self.nodes.clone(),
            edges: self.edges.clone(),
            seg_to_node: self.seg_to_node.clone(),
            components: self.components.clone(),
        }
    }
}

fn configure_style(ctx: &Context, theme: ThemePreset) {
    ctx.set_theme(match theme {
        ThemePreset::Graphite | ThemePreset::Midnight => egui::Theme::Dark,
        ThemePreset::Light | ThemePreset::Paper => egui::Theme::Light,
    });

    let (mut visuals, panel, extreme, card, inactive, hovered, active, accent) = match theme {
        ThemePreset::Graphite => (
            egui::Visuals::dark(),
            Color32::from_rgb(24, 28, 36),
            Color32::from_rgb(14, 17, 22),
            Color32::from_rgb(31, 36, 46),
            Color32::from_rgb(38, 44, 55),
            Color32::from_rgb(48, 57, 70),
            Color32::from_rgb(39, 104, 132),
            Color32::from_rgb(112, 211, 255),
        ),
        ThemePreset::Midnight => (
            egui::Visuals::dark(),
            Color32::from_rgb(15, 23, 42),
            Color32::from_rgb(7, 12, 25),
            Color32::from_rgb(23, 34, 58),
            Color32::from_rgb(29, 43, 70),
            Color32::from_rgb(39, 57, 91),
            Color32::from_rgb(28, 86, 148),
            Color32::from_rgb(105, 183, 255),
        ),
        ThemePreset::Light => (
            egui::Visuals::light(),
            Color32::from_rgb(244, 247, 251),
            Color32::from_rgb(255, 255, 255),
            Color32::from_rgb(229, 235, 243),
            Color32::from_rgb(222, 228, 237),
            Color32::from_rgb(207, 220, 234),
            Color32::from_rgb(164, 207, 224),
            Color32::from_rgb(20, 118, 158),
        ),
        ThemePreset::Paper => (
            egui::Visuals::light(),
            Color32::from_rgb(247, 243, 234),
            Color32::from_rgb(255, 252, 245),
            Color32::from_rgb(235, 229, 215),
            Color32::from_rgb(228, 220, 204),
            Color32::from_rgb(218, 207, 185),
            Color32::from_rgb(213, 171, 139),
            Color32::from_rgb(161, 75, 52),
        ),
    };
    visuals.panel_fill = panel;
    visuals.window_fill = panel;
    visuals.extreme_bg_color = extreme;
    visuals.faint_bg_color = card;
    visuals.selection.bg_fill = active;
    visuals.selection.stroke = egui::Stroke::new(1.0, accent);
    visuals.widgets.noninteractive.bg_fill = card;
    visuals.widgets.inactive.bg_fill = inactive;
    visuals.widgets.hovered.bg_fill = hovered;
    visuals.widgets.active.bg_fill = active;
    visuals.widgets.open.bg_fill = hovered;
    visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(5);
    visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(5);
    visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(5);
    visuals.widgets.active.corner_radius = egui::CornerRadius::same(5);
    visuals.widgets.open.corner_radius = egui::CornerRadius::same(5);
    visuals.window_corner_radius = egui::CornerRadius::same(7);
    ctx.set_visuals(visuals);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 7.0);
        style.spacing.button_padding = Vec2::new(10.0, 6.0);
        style.spacing.indent = 16.0;
        style.spacing.slider_width = 150.0;
    });
}
