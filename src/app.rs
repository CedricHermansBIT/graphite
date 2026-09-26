//! Central application state and the eframe `App` implementation.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use egui::containers::panel::{CentralPanel, Panel};
use egui::{Color32, Context, Key, Pos2, Rect, Vec2};

use crate::export::{AssemblyStats, FigureOptions, copy_sequence_to_clipboard};
use crate::filter::{ColorMode, FilterParams};
use crate::gfa::GfaGraph;
use crate::graph::ViewGraph;
use crate::history::{History, PendingEdit};
use crate::layout::{Layout, LayoutBackend, LayoutParams, LayoutRunner};
use crate::render::{
    RenderCache, RenderParams, draw_gfa_overlays, draw_graph, draw_graph_cached, hit_test_node,
};
use crate::selection::Selection;
use crate::session::{Preferences, Session};
use crate::tasks::{LoadJob, LoadRequest, PreparedGraph};
use crate::ui::{
    DisplayOptions, OverlayAction, OverlayOptions, ThemePreset, component_table, display_panel,
    filter_panel, overlays_panel, selection_panel, stats_panel,
};
use crate::visuals::{configure_style, draw_minimap};

#[cfg(test)]
mod tests;
mod workflows;
use workflows::{ColorRanges, OutputKind};

// ── Load state machine ────────────────────────────────────────────────────────

enum LoadState {
    Empty,
    Loading(LoadJob),
    Loaded {
        gfa: Arc<GfaGraph>,
        stats: AssemblyStats,
        view: Arc<ViewGraph>,
        layout_runner: LayoutRunner,
        layout_snapshot: Arc<Layout>,
    },
    Error(String),
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
    previous_load: Option<Box<LoadState>>,
    preferences: Preferences,
    source_path: Option<PathBuf>,
    pending_recent: Option<PathBuf>,
    output_job: Option<std::thread::JoinHandle<anyhow::Result<String>>>,
    last_viewport: Option<Rect>,
    render_cache: Option<RenderCache>,
    export_width: u32,
    export_height: u32,
    export_current_view: bool,
    strict_parsing: bool,
    show_diagnostics: bool,
    color_ranges: ColorRanges,
    history: History,
    pending_edit: Option<PendingEdit>,
    pending_rebuild: Option<std::time::Instant>,
    applied_filter: FilterParams,
    layout_backend: LayoutBackend,
    remote_ui: bool,
    filter: FilterParams,
    display: DisplayOptions,
    overlays: OverlayOptions,
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
    show_overlay_panel: bool,
    show_stats_panel: bool,
    show_selection_panel: bool,
    /// Fit screen once on first frame after load.
    pending_fit: bool,
    /// Grabbed physics node index for PBD graph manipulation.
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
        let mut preferences: Preferences = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, "graphite.preferences.v1"))
            .unwrap_or_default();
        if !crate::session::valid_display(&preferences.display) {
            preferences.display = DisplayOptions::default();
        }
        preferences.recent_files.truncate(10);
        configure_style(&cc.egui_ctx, preferences.display.theme);
        let mut app = Self {
            load_state: LoadState::Empty,
            display: preferences.display.clone(),
            preferences,
            previous_load: None,
            source_path: None,
            pending_recent: None,
            output_job: None,
            last_viewport: None,
            render_cache: None,
            export_width: 2400,
            export_height: 1600,
            export_current_view: false,
            strict_parsing: false,
            show_diagnostics: false,
            color_ranges: ColorRanges::default(),
            history: History::default(),
            pending_edit: None,
            pending_rebuild: None,
            applied_filter: FilterParams::default(),
            layout_backend,
            remote_ui,
            filter: FilterParams::default(),
            overlays: OverlayOptions::default(),
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
            show_overlay_panel: true,
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
        let is_session = path
            .extension()
            .is_some_and(|ext| ext == "json" || ext == "graphite");
        let request = if is_session {
            LoadRequest::Session(path.clone())
        } else {
            LoadRequest::File(path.clone())
        };
        self.status_msg = format!("Loading {}…", path.display());
        self.begin_load(request);
        self.pending_recent = Some(path);
    }

    fn begin_load(&mut self, request: LoadRequest) {
        self.cancel_load();
        self.finish_edit();
        if let LoadState::Loaded { layout_runner, .. } = &self.load_state {
            layout_runner.set_attractor(None);
        }
        self.pending_rebuild = None;
        let job = LoadJob::spawn(
            request,
            self.filter.clone(),
            self.layout_backend,
            self.layout_publish_interval(),
            self.strict_parsing,
        );
        let old = std::mem::replace(&mut self.load_state, LoadState::Loading(job));
        self.previous_load = Some(Box::new(old));
        self.grabbed_phys = None;
        self.grab_world = None;
        self.render_cache = None;
    }

    fn cancel_load(&mut self) {
        if matches!(self.load_state, LoadState::Loading(_)) {
            self.load_state = self
                .previous_load
                .take()
                .map(|s| *s)
                .unwrap_or(LoadState::Empty);
            self.status_msg = "Loading cancelled.".into();
            self.filter = self.applied_filter.clone();
            self.pending_recent = None;
        }
    }

    fn check_loading(&mut self, ctx: &Context) {
        let ready = matches!(&self.load_state, LoadState::Loading(job) if job.is_finished());
        if ready
            && let LoadState::Loading(job) =
                std::mem::replace(&mut self.load_state, LoadState::Empty)
        {
            match job.finish() {
                Ok(PreparedGraph {
                    filter,
                    source,
                    gfa,
                    stats,
                    view,
                    runner,
                    snapshot,
                    session,
                }) => {
                    self.previous_load = None;
                    self.source_path = Some(source);
                    self.color_ranges = ColorRanges::from_graph(&view);
                    self.applied_filter = filter.clone();
                    self.filter = filter;
                    self.selection.clear();
                    self.overlays = OverlayOptions::default();
                    self.history.clear();
                    self.pending_edit = None;
                    self.pending_focus_nodes = None;
                    self.component_page = 0;
                    self.zoom = 1.0;
                    self.pan = Vec2::ZERO;
                    self.pending_fit = true;
                    self.show_diagnostics = !gfa.diagnostics.is_empty();
                    self.status_msg = format!(
                        "Loaded {} segments, {} links, {} warnings.",
                        gfa.segments.len(),
                        gfa.links.len(),
                        gfa.diagnostics.len()
                    );
                    if let Some(session) = session {
                        self.display = session.display;
                        self.overlays = session.overlays;
                        self.selection.nodes.extend(session.selection);
                        self.zoom = session.zoom;
                        self.pan = Vec2::new(session.pan[0], session.pan[1]);
                        self.pending_fit = false;
                        configure_style(ctx, self.display.theme);
                    }
                    self.load_state = LoadState::Loaded {
                        gfa,
                        stats,
                        view,
                        layout_runner: runner,
                        layout_snapshot: Arc::new(snapshot),
                    };
                    if let Some(path) = self.pending_recent.take() {
                        self.preferences
                            .remember(path.canonicalize().unwrap_or(path));
                    }
                }
                Err(error) => {
                    self.status_msg = format!("Loading failed: {error:#}");
                    self.load_state = self
                        .previous_load
                        .take()
                        .map(|s| *s)
                        .unwrap_or_else(|| LoadState::Error(format!("{error:#}")));
                    self.pending_recent = None;
                    self.filter = self.applied_filter.clone();
                }
            }
        }
        if let LoadState::Loaded {
            layout_runner,
            layout_snapshot,
            ..
        } = &mut self.load_state
            && layout_runner.snapshot_changed(layout_snapshot)
            && layout_runner.update_snapshot(Arc::make_mut(layout_snapshot))
        {
            ctx.request_repaint();
        }
        if self.grabbed_phys.is_none()
            && matches!(&self.load_state, LoadState::Loaded { layout_snapshot, .. } if layout_snapshot.converged)
        {
            self.finish_edit();
        }
        if self
            .output_job
            .as_ref()
            .is_some_and(|job| job.is_finished())
        {
            let result = self
                .output_job
                .take()
                .unwrap()
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Output worker panicked")));
            self.status_msg = match result {
                Ok(message) => message,
                Err(error) => format!("Output failed: {error:#}"),
            };
        }
    }

    fn rebuild_view(&mut self) {
        if let LoadState::Loaded { gfa, .. } = &self.load_state {
            let request = LoadRequest::Rebuild {
                gfa: gfa.clone(),
                source: self.source_path.clone().unwrap_or_default(),
            };
            self.status_msg = "Updating filtered graph…".into();
            self.begin_load(request);
        }
    }

    fn export_figure(&mut self, path: &std::path::Path, svg: bool) {
        self.start_output(
            path.to_path_buf(),
            if svg {
                OutputKind::Svg
            } else {
                OutputKind::Png
            },
        );
    }

    fn top_menu(&mut self, root_ui: &mut egui::Ui) {
        let ctx = root_ui.ctx().clone();
        let can_export_figure =
            self.output_job.is_none() && matches!(&self.load_state, LoadState::Loaded { .. });
        let (can_export_fasta, fasta_disabled_reason) = if let LoadState::Loaded {
            gfa, view, ..
        } = &self.load_state
        {
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
            (has_selected_sequence && self.output_job.is_none(), reason)
        } else {
            (false, "Load a graph and select one or more segments first.")
        };

        Panel::top("menu_bar").show(root_ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open GFA…").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("GFA (plain or gzip)", &["gfa", "gfa1", "gfa2", "gz"])
                            .pick_file()
                        {
                            self.start_load(path);
                        }
                        ui.close();
                    }
                    self.file_workflow_menu(ui);
                });

                self.edit_menu(ui);
                ui.menu_button("Export", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Size");
                        ui.add(
                            egui::DragValue::new(&mut self.export_width)
                                .range(64..=16384)
                                .suffix(" px"),
                        );
                        ui.label("×");
                        ui.add(
                            egui::DragValue::new(&mut self.export_height)
                                .range(64..=16384)
                                .suffix(" px"),
                        );
                    });
                    ui.checkbox(
                        &mut self.export_current_view,
                        "Export current viewport only",
                    );
                    ui.separator();
                    let svg = ui
                        .add_enabled(can_export_figure, egui::Button::new("Figure as SVG…"))
                        .on_disabled_hover_text("Load a graph before exporting a figure.");
                    if svg.clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("SVG", &["svg"])
                            .set_file_name("graphite.svg")
                            .save_file()
                        {
                            self.export_figure(&path, true);
                        }
                        ui.close();
                    }

                    let png = ui
                        .add_enabled(can_export_figure, egui::Button::new("Figure as PNG…"))
                        .on_disabled_hover_text("Load a graph before exporting a figure.");
                    if png.clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("PNG", &["png"])
                            .set_file_name("graphite.png")
                            .save_file()
                        {
                            self.export_figure(&path, false);
                        }
                        ui.close();
                    }

                    ui.separator();

                    let export_fasta_response = ui
                        .add_enabled(
                            can_export_fasta,
                            egui::Button::new("Selected segments as FASTA…"),
                        )
                        .on_disabled_hover_text(fasta_disabled_reason);
                    if export_fasta_response.clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("FASTA", &["fa", "fasta"])
                            .set_file_name("selection.fasta")
                            .save_file()
                        {
                            self.start_output(path, OutputKind::Fasta);
                        }
                        ui.close();
                    }

                    let csv = ui
                        .add_enabled(
                            can_export_figure,
                            egui::Button::new("Graph statistics as CSV…"),
                        )
                        .on_disabled_hover_text("Load a graph before exporting statistics.");
                    if csv.clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("CSV", &["csv"])
                            .set_file_name("graphite-stats.csv")
                            .save_file()
                        {
                            self.start_output(path, OutputKind::Csv);
                        }
                        ui.close();
                    }
                });

                ui.menu_button("View", |ui| {
                    ui.checkbox(&mut self.show_filter_panel, "Filter panel");
                    ui.checkbox(&mut self.show_display_panel, "Display panel");
                    ui.checkbox(&mut self.show_overlay_panel, "GFA overlay panel");
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
                    ui.checkbox(
                        &mut self.strict_parsing,
                        "Reject graphs with parser warnings",
                    )
                    .on_hover_text(
                        "Applies on the next load. Duplicate segment names are always rejected.",
                    );
                    if ui.button("Show parser diagnostics").clicked() {
                        self.show_diagnostics = true;
                        ui.close();
                    }
                    ui.separator();
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
                            let changed = ui
                                .add_enabled_ui(
                                    !matches!(self.load_state, LoadState::Loading(_)),
                                    |ui| filter_panel(ui, &mut self.filter),
                                )
                                .inner;
                            if changed {
                                self.pending_rebuild = Some(std::time::Instant::now());
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
                        if self.show_overlay_panel {
                            let mut overlay_action = None;
                            if let LoadState::Loaded { gfa, .. } = &self.load_state
                                && (!gfa.paths.is_empty()
                                    || !gfa.walks.is_empty()
                                    || !gfa.containments.is_empty())
                            {
                                let (changed, action) = overlays_panel(ui, gfa, &mut self.overlays);
                                if changed {
                                    ctx.request_repaint();
                                }
                                overlay_action = action;
                                ui.separator();
                            }
                            if let Some(action) = overlay_action {
                                self.apply_overlay_action(action);
                                ctx.request_repaint();
                            }
                        }
                        if self.show_stats_panel
                            && let LoadState::Loaded { stats, view, .. } = &self.load_state
                        {
                            stats_panel(ui, stats, view);
                            ui.separator();
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
        if export_fasta
            && let Some(path) = rfd::FileDialog::new()
                .add_filter("FASTA", &["fa", "fasta"])
                .save_file()
        {
            self.start_output(path, OutputKind::Fasta);
        }
        if sel_component
            && let Some(&start) = self.selection.nodes.iter().next()
            && let LoadState::Loaded { view, .. } = &self.load_state
        {
            self.selection.select_component(start, view, false);
        }
        if let Some(nodes) = focus_component {
            self.selection.clear();
            self.selection.nodes.extend(nodes.iter().copied());
            self.pending_focus_nodes = Some(nodes);
            self.status_msg = format!("Focused {}-segment component.", self.selection.node_count());
        }
    }

    fn apply_overlay_action(&mut self, action: OverlayAction) {
        let (nodes, total_steps, label) = match (&self.load_state, action) {
            (
                LoadState::Loaded { gfa, view, .. },
                OverlayAction::FocusPath(index) | OverlayAction::SelectPath(index),
            ) => {
                let Some(path) = gfa.paths.get(index) else {
                    return;
                };
                let mut nodes: Vec<usize> = gfa
                    .path_steps(path)
                    .iter()
                    .filter_map(|step| view.seg_to_node.get(&step.segment).copied())
                    .collect();
                nodes.sort_unstable();
                nodes.dedup();
                (
                    nodes,
                    path.steps.len(),
                    format!("path {}", path.name.as_ref()),
                )
            }
            (
                LoadState::Loaded { gfa, view, .. },
                OverlayAction::FocusWalk(index) | OverlayAction::SelectWalk(index),
            ) => {
                let Some(walk) = gfa.walks.get(index) else {
                    return;
                };
                let mut nodes: Vec<usize> = gfa
                    .walk_steps(walk)
                    .iter()
                    .filter_map(|step| view.seg_to_node.get(&step.segment).copied())
                    .collect();
                nodes.sort_unstable();
                nodes.dedup();
                (
                    nodes,
                    walk.steps.len(),
                    format!(
                        "walk {} / h{} / {}",
                        walk.sample_id.as_ref(),
                        walk.haplotype_index,
                        walk.sequence_id.as_ref()
                    ),
                )
            }
            _ => return,
        };

        if nodes.is_empty() {
            self.status_msg = format!(
                "No visible segments from {label}; the current filters hide all {} steps.",
                total_steps
            );
            return;
        }

        match action {
            OverlayAction::FocusPath(_) | OverlayAction::FocusWalk(_) => {
                self.pending_focus_nodes = Some(nodes.clone());
                self.status_msg = format!(
                    "Focused {label}: {} visible segment{} from {} step{}.",
                    nodes.len(),
                    if nodes.len() == 1 { "" } else { "s" },
                    total_steps,
                    if total_steps == 1 { "" } else { "s" },
                );
            }
            OverlayAction::SelectPath(_) | OverlayAction::SelectWalk(_) => {
                self.selection.clear();
                self.selection.nodes.extend(nodes.iter().copied());
                self.status_msg = format!(
                    "Selected {} visible segment{} from {label} ({} step{}).",
                    nodes.len(),
                    if nodes.len() == 1 { "" } else { "s" },
                    total_steps,
                    if total_steps == 1 { "" } else { "s" },
                );
            }
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
        } else {
            self.status_msg = "No selected segment with an embedded sequence to copy.".to_string();
        }
    }

    fn refresh_render_cache(&mut self) {
        match &self.load_state {
            LoadState::Loaded {
                view,
                layout_snapshot,
                ..
            } if layout_snapshot.converged && self.grabbed_phys.is_none() => {
                if self
                    .render_cache
                    .as_ref()
                    .is_none_or(|cache| cache.revision != layout_snapshot.revision())
                {
                    self.render_cache = Some(RenderCache::new(view, layout_snapshot));
                }
            }
            _ => self.render_cache = None,
        }
    }

    fn canvas(&mut self, root_ui: &mut egui::Ui) {
        self.refresh_render_cache();
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
                    LoadState::Loading(job) => {
                        let stage = job.stage();
                        ui.centered_and_justified(|ui| {
                            ui.vertical_centered(|ui| {
                                ui.spinner();
                                ui.label(stage);
                                if ui.button("Cancel loading").clicked() {
                                    self.cancel_load();
                                }
                            });
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

                let response =
                    ui.allocate_response(ui.available_size(), egui::Sense::click_and_drag());
                let viewport = response.rect;
                self.last_viewport = Some(viewport);

                // Explicitly give the graph canvas keyboard focus after pointer
                // interaction. egui TextEdits keep focus until another widget asks
                // for it, so after using a filter/search field Ctrl+C could remain
                // routed to that stale text focus instead of the graph selection.
                if response.clicked() || response.drag_started() {
                    response.request_focus();
                }
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
                    if response.drag_stopped()
                        && let Some(start) = self.rubber_start.take()
                        && let Some(end) = response.interact_pointer_pos()
                    {
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

                // Mode toggle with 'S' / 'P' / 'G' shortcuts.
                if !ctx.text_edit_focused() {
                    ctx.input(|i| {
                        if i.modifiers.command || i.modifiers.alt {
                            return;
                        }
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
                }

                // ── Grab / PBD graph manipulation ─────────────────────────────────
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
                        self.finish_edit();
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
                            let w = [
                                (screen.x - vp_center.x - self.pan.x) / self.zoom,
                                (screen.y - vp_center.y - self.pan.y) / self.zoom,
                            ];
                            self.grabbed_phys =
                                hit_test_node(screen, viewport, view, layout_snapshot, &render_p)
                                    .and_then(|ni| layout_snapshot.nearest_physics_point(ni, w));
                            if let Some(pi) = self.grabbed_phys {
                                let p = layout_snapshot.positions[pi];
                                self.grab_offset = [p[0] - w[0], p[1] - w[1]];
                            }
                        }
                    }

                    if just_pressed && self.grabbed_phys.is_some() {
                        self.begin_edit();
                    }
                    if dragging {
                        if let Some(wpos) = cursor_world {
                            self.grab_world = Some([
                                wpos[0] + self.grab_offset[0],
                                wpos[1] + self.grab_offset[1],
                            ]);
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
                        let att = self.grab_world.zip(self.grabbed_phys);
                        layout_runner.set_attractor(att);
                        if let Some((pos, pi)) = att {
                            Arc::make_mut(layout_snapshot).drag_preview_to(pos, pi);
                            self.render_cache = None;
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
                    gfa,
                    view,
                    layout_snapshot,
                    ..
                } = &self.load_state
                {
                    let painter = ui.painter_at(viewport);
                    // Background.
                    painter.rect_filled(viewport, 0.0, self.display.theme.canvas_background());

                    let rp = self.render_params();
                    if let Some(cache) = &self.render_cache {
                        draw_graph_cached(
                            &painter,
                            viewport,
                            view,
                            layout_snapshot,
                            &self.selection,
                            &rp,
                            cache,
                        );
                    } else {
                        draw_graph(
                            &painter,
                            viewport,
                            view,
                            layout_snapshot,
                            &self.selection,
                            &rp,
                        );
                    }

                    draw_gfa_overlays(
                        &painter,
                        viewport,
                        gfa,
                        view,
                        layout_snapshot,
                        &rp,
                        self.overlays.selected_path,
                        self.overlays.selected_walk,
                        self.overlays.show_containments,
                    );

                    // Rubber-band rect.
                    if self.interaction_mode == InteractionMode::Select
                        && let Some(start) = self.rubber_start
                        && response.dragged_by(egui::PointerButton::Primary)
                        && let Some(cur) = response.interact_pointer_pos()
                    {
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

                    // Status overlay (mode indicator).
                    painter.text(
                        viewport.min + Vec2::new(8.0, 8.0),
                        egui::Align2::LEFT_TOP,
                        match self.interaction_mode {
                            InteractionMode::Pan => "Mode: Pan  [P/S/G]",
                            InteractionMode::Select => "Mode: Select  [P/S/G, F=fit]",
                            InteractionMode::Grab => {
                                "Mode: Grab  [P/S/G] — drag a contig to move it"
                            }
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
                        && layout_runner.is_running()
                    {
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

                if let LoadState::Loaded {
                    layout_snapshot, ..
                } = &self.load_state
                    && let Some(target) = draw_minimap(
                        ui,
                        viewport,
                        layout_snapshot,
                        self.zoom,
                        self.pan,
                        self.display.theme,
                    )
                {
                    self.pan = Vec2::new(-target[0] * self.zoom, -target[1] * self.zoom);
                    ctx.request_repaint();
                }
            });
    }

    fn render_params(&self) -> RenderParams {
        let mut min_depth = self.display.min_depth_color;
        let mut max_depth = self.display.max_depth_color;
        let mut min_length = 1.0;
        let mut max_length = 100_000.0;

        if self.display.auto_color_scale {
            let range = match self.display.color_mode {
                ColorMode::Depth => self.color_ranges.depth,
                ColorMode::ReadCount => self.color_ranges.read_count,
                _ => None,
            };
            if let Some((lo, hi)) = range {
                min_depth = lo;
                max_depth = hi;
            }
        }
        if let Some((lo, hi)) = self.color_ranges.length {
            min_length = lo;
            max_length = hi;
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
        self.pending_fit = true;
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
            layout_snapshot, ..
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

// ── eframe::App ───────────────────────────────────────────────────────────────

impl eframe::App for GfaApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.preferences.display = self.display.clone();
        eframe::set_value(storage, "graphite.preferences.v1", &self.preferences);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.check_loading(&ctx);
        self.poll_workflows(&ctx);
        self.top_menu(ui);
        self.left_panels(ui);
        self.right_panel(ui);
        self.canvas(ui);
        self.diagnostics_window(&ctx);

        // eframe/egui exposes the platform copy gesture as Event::Copy.
        // Keep the key check as a native fallback, but don't rely on it alone:
        // some integrations handle Ctrl/Cmd+C semantically and may not leave a
        // normal Key::C press for application-level shortcut code.
        let copy_shortcut = ctx.input(|input| {
            input
                .events
                .iter()
                .any(|event| matches!(event, egui::Event::Copy))
                || (input.modifiers.command && input.key_pressed(Key::C))
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
            && layout_runner.is_running()
            && !layout_snapshot.converged
        {
            let repaint_interval = if self.remote_ui {
                Duration::from_millis(100)
            } else {
                Duration::from_millis(16)
            };
            ctx.request_repaint_after(repaint_interval);
        }
    }
}
