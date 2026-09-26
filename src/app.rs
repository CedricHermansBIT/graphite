//! Central application state and the eframe `App` implementation.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use egui::containers::panel::{CentralPanel, Panel};
use egui::{Color32, Context};

use crate::app_core::{AppCore, OutputKind};
use crate::export::AssemblyStats;
use crate::gfa::GfaGraph;
use crate::graph::ViewGraph;
use crate::layout::{Layout, LayoutBackend, LayoutRunner};
use crate::session::{Preferences, Session};
use crate::tasks::{LoadJob, LoadRequest, PreparedGraph};
use crate::ui::DisplayOptions;
use crate::visuals::configure_style;

#[cfg(test)]
mod tests;
mod workflows;

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

// ── App ───────────────────────────────────────────────────────────────────────

pub struct GfaApp {
    core: AppCore,
    load_state: LoadState,
    previous_load: Option<Box<LoadState>>,
    preferences: Preferences,
    source_path: Option<PathBuf>,
    pending_recent: Option<PathBuf>,
    output_job: Option<std::thread::JoinHandle<anyhow::Result<String>>>,
    layout_backend: LayoutBackend,
    remote_ui: bool,
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
        let status_msg = if remote_ui {
            "Open a GFA file to start. Remote UI mode is enabled.".to_string()
        } else {
            "Open a GFA file to start.".to_string()
        };
        let core = AppCore::new(preferences.display.clone(), status_msg);
        let mut app = Self {
            core,
            load_state: LoadState::Empty,
            preferences,
            previous_load: None,
            source_path: None,
            pending_recent: None,
            output_job: None,
            layout_backend,
            remote_ui,
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
        self.core.status_msg = format!("Loading {}…", path.display());
        self.begin_load(request);
        self.pending_recent = Some(path);
    }

    fn begin_load(&mut self, request: LoadRequest) {
        self.cancel_load();
        self.finish_edit();
        if let LoadState::Loaded { layout_runner, .. } = &self.load_state {
            layout_runner.set_attractor(None);
        }
        self.core.pending_rebuild = None;
        let job = LoadJob::spawn(
            request,
            self.core.filter.clone(),
            self.layout_backend,
            self.layout_publish_interval(),
            self.core.strict_parsing,
        );
        let old = std::mem::replace(&mut self.load_state, LoadState::Loading(job));
        self.previous_load = Some(Box::new(old));
        self.core.grabbed_phys = None;
        self.core.grab_world = None;
        self.core.render_cache = None;
    }

    fn cancel_load(&mut self) {
        if matches!(self.load_state, LoadState::Loading(_)) {
            self.load_state = self
                .previous_load
                .take()
                .map(|s| *s)
                .unwrap_or(LoadState::Empty);
            self.core.status_msg = "Loading cancelled.".into();
            self.core.filter = self.core.applied_filter.clone();
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
                    self.core.reset_for_graph(&view, filter);
                    self.core.show_diagnostics = !gfa.diagnostics.is_empty();
                    self.core.status_msg = format!(
                        "Loaded {} segments, {} links, {} warnings.",
                        gfa.segments.len(),
                        gfa.links.len(),
                        gfa.diagnostics.len()
                    );
                    if let Some(session) = &session {
                        self.core.restore_session_ui(session);
                        configure_style(ctx, self.core.display.theme);
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
                    self.core.status_msg = format!("Loading failed: {error:#}");
                    self.load_state = self
                        .previous_load
                        .take()
                        .map(|s| *s)
                        .unwrap_or_else(|| LoadState::Error(format!("{error:#}")));
                    self.pending_recent = None;
                    self.core.filter = self.core.applied_filter.clone();
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
        if self.core.grabbed_phys.is_none()
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
            self.core.status_msg = match result {
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
            self.core.status_msg = "Updating filtered graph…".into();
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

                let loaded = matches!(self.load_state, LoadState::Loaded { .. });
                let outputs_enabled = self.output_job.is_none();
                let (gfa, view) = match &self.load_state {
                    LoadState::Loaded { gfa, view, .. } => {
                        (Some(gfa.as_ref()), Some(view.as_ref()))
                    }
                    _ => (None, None),
                };
                let actions =
                    self.core
                        .common_top_menu(ui, &ctx, loaded, outputs_enabled, gfa, view);

                if let Some(redo) = actions.undo_redo {
                    self.undo_redo(redo);
                }
                if let Some(kind) = actions.output {
                    match kind {
                        OutputKind::Svg => {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter("SVG", &["svg"])
                                .set_file_name("graphite.svg")
                                .save_file()
                            {
                                self.export_figure(&path, true);
                            }
                        }
                        OutputKind::Png => {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter("PNG", &["png"])
                                .set_file_name("graphite.png")
                                .save_file()
                            {
                                self.export_figure(&path, false);
                            }
                        }
                        OutputKind::Fasta => {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter("FASTA", &["fa", "fasta"])
                                .set_file_name("selection.fasta")
                                .save_file()
                            {
                                self.start_output(path, OutputKind::Fasta);
                            }
                        }
                        OutputKind::Csv => {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter("CSV", &["csv"])
                                .set_file_name("graphite-stats.csv")
                                .save_file()
                            {
                                self.start_output(path, OutputKind::Csv);
                            }
                        }
                        OutputKind::Session => {}
                    }
                }
                if actions.fit {
                    self.core.pending_fit = true;
                }
            });
        });
    }

    fn left_panels(&mut self, root_ui: &mut egui::Ui) {
        let ctx = root_ui.ctx().clone();
        let (gfa, view, stats, filter_enabled) = match &self.load_state {
            LoadState::Loaded {
                gfa, view, stats, ..
            } => (Some(gfa.as_ref()), Some(view.as_ref()), Some(stats), true),
            LoadState::Loading(_) => (None, None, None, false),
            _ => (None, None, None, true),
        };
        if let Some(action) = self
            .core
            .show_left_panel(root_ui, gfa, view, stats, filter_enabled)
            && let (Some(gfa), Some(view)) = (gfa, view)
        {
            self.core.apply_overlay_action(gfa, view, action);
            ctx.request_repaint();
        }
    }

    fn right_panel(&mut self, root_ui: &mut egui::Ui) {
        let ctx = root_ui.ctx().clone();
        let (gfa, view) = match &self.load_state {
            LoadState::Loaded { gfa, view, .. } => (Some(gfa.as_ref()), Some(view.as_ref())),
            _ => (None, None),
        };
        let mut actions = self.core.show_right_panel(root_ui, gfa, view);
        self.core.apply_right_panel_actions(view, &mut actions);

        if actions.copy_sequence {
            let (gfa, view) = match &self.load_state {
                LoadState::Loaded { gfa, view, .. } => (Some(gfa.as_ref()), Some(view.as_ref())),
                _ => (None, None),
            };
            self.core
                .copy_selected_sequences_to_clipboard(&ctx, gfa, view);
        }
        if actions.export_fasta
            && let Some(path) = rfd::FileDialog::new()
                .add_filter("FASTA", &["fa", "fasta"])
                .save_file()
        {
            self.start_output(path, OutputKind::Fasta);
        }
    }

    fn refresh_render_cache(&mut self) {
        let (core, load_state) = (&mut self.core, &self.load_state);
        match load_state {
            LoadState::Loaded {
                view,
                layout_snapshot,
                ..
            } => {
                let allow_cache = core.grabbed_phys.is_none();
                core.refresh_render_cache(view, layout_snapshot, allow_cache);
            }
            _ => core.render_cache = None,
        }
    }

    fn canvas(&mut self, root_ui: &mut egui::Ui) {
        self.refresh_render_cache();
        let ctx = root_ui.ctx().clone();
        CentralPanel::default()
            .frame(egui::Frame::new().fill(self.core.display.theme.canvas_background()))
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
                if let LoadState::Loaded {
                    view,
                    layout_snapshot,
                    ..
                } = &self.load_state
                {
                    self.core
                        .prepare_canvas_interaction(&ctx, &response, view, layout_snapshot);
                }

                let grab = if let LoadState::Loaded {
                    view,
                    layout_snapshot,
                    ..
                } = &self.load_state
                {
                    self.core
                        .update_grab_interaction(&response, view, layout_snapshot)
                } else {
                    Default::default()
                };

                let attractor = self.core.grab_world.zip(self.core.grabbed_phys);
                if let LoadState::Loaded {
                    layout_runner,
                    layout_snapshot,
                    ..
                } = &mut self.load_state
                {
                    layout_runner.set_attractor(attractor);
                    if let Some((position, physics_index)) = grab.target {
                        Arc::make_mut(layout_snapshot).drag_preview_to(position, physics_index);
                        self.core.render_cache = None;
                    }
                }
                if grab.dragging {
                    ctx.request_repaint();
                }

                if let LoadState::Loaded {
                    gfa,
                    view,
                    layout_runner,
                    layout_snapshot,
                    ..
                } = &self.load_state
                {
                    self.core.draw_canvas_contents(
                        ui,
                        &response,
                        gfa,
                        view,
                        layout_snapshot,
                        layout_runner.is_running(),
                    );
                }
            });
    }
}

// ── eframe::App ───────────────────────────────────────────────────────────────

impl eframe::App for GfaApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.preferences.display = self.core.display.clone();
        eframe::set_value(storage, "graphite.preferences.v1", &self.preferences);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.check_loading(&ctx);
        self.poll_workflows(&ctx);
        let shortcuts = self.core.shortcut_actions(&ctx);
        if let Some(redo) = shortcuts.undo_redo {
            self.undo_redo(redo);
        }
        if shortcuts.copy {
            let (gfa, view) = match &self.load_state {
                LoadState::Loaded { gfa, view, .. } => (Some(gfa.as_ref()), Some(view.as_ref())),
                _ => (None, None),
            };
            self.core
                .copy_selected_sequences_to_clipboard(&ctx, gfa, view);
        }
        self.top_menu(ui);
        self.left_panels(ui);
        self.right_panel(ui);
        self.canvas(ui);
        let gfa = match &self.load_state {
            LoadState::Loaded { gfa, .. } => Some(gfa.as_ref()),
            _ => None,
        };
        self.core.diagnostics_window(&ctx, gfa);

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
