//! Platform-independent application state and interaction helpers.
//!
//! Native and browser frontends keep only their I/O, scheduling and persistence
//! adapters. Shared graph-view state lives here so behavior cannot drift between
//! the two applications.

use web_time::{Duration, Instant};

use egui::containers::panel::Panel;
use egui::{Color32, Context, Key, Pos2, Rect, Response, Vec2};

use crate::{
    export::{AssemblyStats, FigureOptions, copy_sequence_to_clipboard},
    filter::{ColorMode, FilterParams},
    gfa::GfaGraph,
    graph::ViewGraph,
    history::{History, PendingEdit},
    layout::Layout,
    render::{
        RenderCache, RenderParams, draw_gfa_overlays, draw_graph, draw_graph_cached, hit_test_node,
    },
    selection::Selection,
    ui::{
        DisplayOptions, OverlayAction, OverlayOptions, ThemePreset, component_table, display_panel,
        filter_panel, overlays_panel, selection_panel, stats_panel,
    },
    visuals::{configure_style, draw_minimap},
};

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum InteractionMode {
    Pan,
    Select,
    Grab,
}

#[derive(Clone, Copy)]
pub(crate) enum OutputKind {
    Svg,
    Png,
    Fasta,
    Csv,
    Session,
}

#[derive(Default)]
pub(crate) struct ColorRanges {
    pub(crate) depth: Option<(f32, f32)>,
    pub(crate) read_count: Option<(f32, f32)>,
    pub(crate) length: Option<(f32, f32)>,
}

impl ColorRanges {
    pub(crate) fn from_graph(graph: &ViewGraph) -> Self {
        fn range(values: impl Iterator<Item = f32>) -> Option<(f32, f32)> {
            let (lo, hi) = values
                .filter(|v| v.is_finite())
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), value| {
                    (lo.min(value), hi.max(value))
                });
            lo.is_finite()
                .then_some((lo, if hi > lo { hi } else { lo + 1.0 }))
        }

        Self {
            depth: range(
                graph
                    .nodes
                    .iter()
                    .filter_map(|node| node.depth)
                    .map(|v| v as f32),
            ),
            read_count: range(
                graph
                    .nodes
                    .iter()
                    .filter_map(|node| node.read_count)
                    .map(|v| v as f32),
            ),
            length: range(graph.nodes.iter().map(|node| node.length as f32)),
        }
    }
}

pub(crate) struct OverlayResolution {
    pub(crate) nodes: Vec<usize>,
    pub(crate) total_steps: usize,
    pub(crate) label: String,
    pub(crate) focus: bool,
}

#[derive(Default)]
pub(crate) struct GrabInteraction {
    pub(crate) target: Option<([f32; 2], usize)>,
    pub(crate) dragging: bool,
}

#[derive(Default)]
pub(crate) struct ShortcutActions {
    pub(crate) undo_redo: Option<bool>,
    pub(crate) copy: bool,
}

#[derive(Default)]
pub(crate) struct CommonMenuActions {
    pub(crate) undo_redo: Option<bool>,
    pub(crate) output: Option<OutputKind>,
    pub(crate) fit: bool,
}

#[derive(Default)]
pub(crate) struct RightPanelActions {
    pub(crate) copy_sequence: bool,
    pub(crate) export_fasta: bool,
    pub(crate) select_component: bool,
    pub(crate) focus_nodes: Option<Vec<usize>>,
}

pub(crate) struct AppCore {
    pub(crate) last_viewport: Option<Rect>,
    pub(crate) render_cache: Option<RenderCache>,
    pub(crate) export_width: u32,
    pub(crate) export_height: u32,
    pub(crate) export_current_view: bool,
    pub(crate) strict_parsing: bool,
    pub(crate) show_diagnostics: bool,
    pub(crate) color_ranges: ColorRanges,
    pub(crate) history: History,
    pub(crate) pending_edit: Option<PendingEdit>,
    pub(crate) pending_rebuild: Option<Instant>,
    pub(crate) applied_filter: FilterParams,
    pub(crate) filter: FilterParams,
    pub(crate) display: DisplayOptions,
    pub(crate) overlays: OverlayOptions,
    pub(crate) selection: Selection,
    pub(crate) zoom: f32,
    pub(crate) pan: Vec2,
    pub(crate) interaction_mode: InteractionMode,
    pub(crate) rubber_start: Option<Pos2>,
    pub(crate) status_msg: String,
    pub(crate) component_query: String,
    pub(crate) component_page: usize,
    pub(crate) pending_focus_nodes: Option<Vec<usize>>,
    pub(crate) show_filter_panel: bool,
    pub(crate) show_display_panel: bool,
    pub(crate) show_overlay_panel: bool,
    pub(crate) show_stats_panel: bool,
    pub(crate) show_selection_panel: bool,
    pub(crate) pending_fit: bool,
    pub(crate) grabbed_phys: Option<usize>,
    pub(crate) grab_offset: [f32; 2],
    pub(crate) grab_world: Option<[f32; 2]>,
}

impl AppCore {
    pub(crate) fn new(display: DisplayOptions, status_msg: String) -> Self {
        Self {
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
            filter: FilterParams::default(),
            display,
            overlays: OverlayOptions::default(),
            selection: Selection::default(),
            zoom: 1.0,
            pan: Vec2::ZERO,
            interaction_mode: InteractionMode::Pan,
            rubber_start: None,
            status_msg,
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
        }
    }

    pub(crate) fn edit_menu(&mut self, ui: &mut egui::Ui, loaded: bool) -> Option<bool> {
        let mut redo = None;
        ui.menu_button("Edit", |ui| {
            if ui
                .add_enabled(
                    loaded && (self.history.can_undo() || self.pending_edit.is_some()),
                    egui::Button::new("Undo movement  Ctrl/Cmd+Z"),
                )
                .clicked()
            {
                redo = Some(false);
                ui.close();
            }
            if ui
                .add_enabled(
                    loaded && self.history.can_redo(),
                    egui::Button::new("Redo movement  Ctrl/Cmd+Shift+Z"),
                )
                .clicked()
            {
                redo = Some(true);
                ui.close();
            }
        });
        redo
    }

    pub(crate) fn fasta_export_state(
        &self,
        gfa: Option<&GfaGraph>,
        view: Option<&ViewGraph>,
    ) -> (bool, &'static str) {
        let (Some(gfa), Some(view)) = (gfa, view) else {
            return (false, "Load a graph and select one or more segments first.");
        };
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
    }

    pub(crate) fn export_menu(
        &mut self,
        ui: &mut egui::Ui,
        can_export_figure: bool,
        can_export_fasta: bool,
        fasta_disabled_reason: &str,
    ) -> Option<OutputKind> {
        let mut request = None;
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

            for (kind, label) in [
                (OutputKind::Svg, "Figure as SVG…"),
                (OutputKind::Png, "Figure as PNG…"),
            ] {
                if ui
                    .add_enabled(can_export_figure, egui::Button::new(label))
                    .on_disabled_hover_text("Load a graph before exporting a figure.")
                    .clicked()
                {
                    request = Some(kind);
                    ui.close();
                }
            }

            ui.separator();
            if ui
                .add_enabled(
                    can_export_fasta,
                    egui::Button::new("Selected segments as FASTA…"),
                )
                .on_disabled_hover_text(fasta_disabled_reason)
                .clicked()
            {
                request = Some(OutputKind::Fasta);
                ui.close();
            }
            if ui
                .add_enabled(
                    can_export_figure,
                    egui::Button::new("Graph statistics as CSV…"),
                )
                .on_disabled_hover_text("Load a graph before exporting statistics.")
                .clicked()
            {
                request = Some(OutputKind::Csv);
                ui.close();
            }
        });
        request
    }

    pub(crate) fn view_menu(&mut self, ui: &mut egui::Ui) -> bool {
        let mut fit = false;
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
                fit = true;
                ui.close();
            }
        });
        fit
    }

    pub(crate) fn select_menu(&mut self, ui: &mut egui::Ui, view: Option<&ViewGraph>) {
        ui.menu_button("Select", |ui| {
            if ui.button("Select all").clicked() {
                if let Some(view) = view {
                    self.select_all(view);
                }
                ui.close();
            }
            if ui.button("Deselect all").clicked() {
                self.selection.clear();
                ui.close();
            }
            if ui.button("Invert selection").clicked() {
                if let Some(view) = view {
                    self.invert_selection(view);
                }
                ui.close();
            }
        });
    }

    pub(crate) fn settings_menu(&mut self, ui: &mut egui::Ui, ctx: &Context) {
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
                        configure_style(ctx, theme);
                        ctx.request_repaint();
                    }
                }
            });
        });
    }

    pub(crate) fn mode_controls(&mut self, ui: &mut egui::Ui) -> bool {
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
        ui.button("Fit  F").clicked()
    }

    pub(crate) fn take_due_rebuild(&mut self, ctx: &Context) -> bool {
        let due = self
            .pending_rebuild
            .is_some_and(|changed| changed.elapsed() >= Duration::from_millis(250))
            && !ctx.input(|input| input.pointer.any_down());
        if due {
            self.pending_rebuild = None;
        }
        due
    }

    pub(crate) fn shortcut_actions(&self, ctx: &Context) -> ShortcutActions {
        if ctx.text_edit_focused() {
            return ShortcutActions::default();
        }

        ctx.input(|input| ShortcutActions {
            undo_redo: (input.modifiers.command && input.key_pressed(Key::Z))
                .then_some(input.modifiers.shift),
            copy: input
                .events
                .iter()
                .any(|event| matches!(event, egui::Event::Copy))
                || (input.modifiers.command && input.key_pressed(Key::C)),
        })
    }

    pub(crate) fn common_top_menu(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &Context,
        loaded: bool,
        outputs_enabled: bool,
        gfa: Option<&GfaGraph>,
        view: Option<&ViewGraph>,
    ) -> CommonMenuActions {
        let undo_redo = self.edit_menu(ui, loaded);
        let (has_exportable_fasta, fasta_disabled_reason) = self.fasta_export_state(gfa, view);
        let output = self.export_menu(
            ui,
            loaded && outputs_enabled,
            has_exportable_fasta && outputs_enabled,
            fasta_disabled_reason,
        );
        let mut actions = CommonMenuActions {
            undo_redo,
            output,
            fit: false,
        };

        if self.view_menu(ui) {
            actions.fit = true;
        }
        self.select_menu(ui, view);
        self.settings_menu(ui, ctx);

        ui.separator();
        if self.mode_controls(ui) {
            actions.fit = true;
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(&self.status_msg)
                    .small()
                    .color(Color32::GRAY),
            );
        });

        actions
    }

    pub(crate) fn show_left_panel(
        &mut self,
        root: &mut egui::Ui,
        gfa: Option<&GfaGraph>,
        view: Option<&ViewGraph>,
        stats: Option<&AssemblyStats>,
        filter_enabled: bool,
    ) -> Option<OverlayAction> {
        let ctx = root.ctx().clone();
        let mut action = None;
        Panel::left("left_panel")
            .resizable(true)
            .default_size(300.0)
            .size_range(270.0..=420.0)
            .show(root, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        action =
                            self.left_panel_contents(ui, &ctx, gfa, view, stats, filter_enabled);
                    });
            });
        action
    }

    pub(crate) fn show_right_panel(
        &mut self,
        root: &mut egui::Ui,
        gfa: Option<&GfaGraph>,
        view: Option<&ViewGraph>,
    ) -> RightPanelActions {
        if !self.show_selection_panel {
            return RightPanelActions::default();
        }

        let mut actions = RightPanelActions::default();
        Panel::right("right_panel")
            .resizable(true)
            .default_size(280.0)
            .size_range(240.0..=420.0)
            .show(root, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        actions = self.right_panel_contents(ui, gfa, view);
                    });
            });
        actions
    }

    pub(crate) fn apply_right_panel_actions(
        &mut self,
        view: Option<&ViewGraph>,
        actions: &mut RightPanelActions,
    ) {
        if actions.select_component
            && let (Some(&start), Some(view)) = (self.selection.nodes.iter().next(), view)
        {
            self.selection.select_component(start, view, false);
        }

        if let Some(nodes) = actions.focus_nodes.take() {
            self.selection.clear();
            self.selection.nodes.extend(nodes.iter().copied());
            self.pending_focus_nodes = Some(nodes);
            self.status_msg = format!("Focused {}-segment component.", self.selection.node_count());
        }
    }

    pub(crate) fn left_panel_contents(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &Context,
        gfa: Option<&GfaGraph>,
        view: Option<&ViewGraph>,
        stats: Option<&AssemblyStats>,
        filter_enabled: bool,
    ) -> Option<OverlayAction> {
        let mut overlay_action = None;
        if self.show_filter_panel {
            let changed = ui
                .add_enabled_ui(filter_enabled, |ui| filter_panel(ui, &mut self.filter))
                .inner;
            if changed {
                self.pending_rebuild = Some(Instant::now());
            }
            ui.separator();
        }

        if self.show_display_panel {
            let previous_theme = self.display.theme;
            if display_panel(ui, &mut self.display) {
                if self.display.theme != previous_theme {
                    configure_style(ctx, self.display.theme);
                }
                ctx.request_repaint();
            }
            ui.separator();
        }

        if self.show_overlay_panel
            && let Some(gfa) = gfa
            && (!gfa.paths.is_empty() || !gfa.walks.is_empty() || !gfa.containments.is_empty())
        {
            let (changed, action) = overlays_panel(ui, gfa, &mut self.overlays);
            if changed {
                ctx.request_repaint();
            }
            overlay_action = action;
            ui.separator();
        }

        if self.show_stats_panel
            && let (Some(stats), Some(view)) = (stats, view)
        {
            stats_panel(ui, stats, view);
            ui.separator();
        }

        overlay_action
    }

    pub(crate) fn right_panel_contents(
        &mut self,
        ui: &mut egui::Ui,
        gfa: Option<&GfaGraph>,
        view: Option<&ViewGraph>,
    ) -> RightPanelActions {
        let mut actions = RightPanelActions::default();
        if let (Some(gfa), Some(view)) = (gfa, view) {
            actions.focus_nodes = component_table(
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
                &mut actions.copy_sequence,
                &mut actions.export_fasta,
                &mut actions.select_component,
            );
        } else {
            ui.label("No file loaded.");
        }
        actions
    }

    pub(crate) fn reset_for_graph(&mut self, view: &ViewGraph, filter: FilterParams) {
        self.color_ranges = ColorRanges::from_graph(view);
        self.applied_filter = filter.clone();
        self.filter = filter;
        self.render_cache = None;
        self.selection.clear();
        self.overlays = OverlayOptions::default();
        self.history.clear();
        self.pending_edit = None;
        self.pending_focus_nodes = None;
        self.component_query.clear();
        self.component_page = 0;
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
        self.pending_fit = true;
        self.grabbed_phys = None;
        self.grab_offset = [0.0; 2];
        self.grab_world = None;
    }

    pub(crate) fn restore_session_ui(&mut self, session: &crate::session::Session) {
        self.display = session.display.clone();
        self.overlays = session.overlays.clone();
        self.selection.clear();
        self.selection
            .nodes
            .extend(session.selection.iter().copied());
        self.zoom = session.zoom;
        self.pan = Vec2::new(session.pan[0], session.pan[1]);
        self.pending_fit = false;
    }

    pub(crate) fn render_params(&self) -> RenderParams {
        let mut min_depth = self.display.min_depth_color;
        let mut max_depth = self.display.max_depth_color;
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
        let (min_length, max_length) = self.color_ranges.length.unwrap_or((1.0, 100_000.0));

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

    pub(crate) fn figure_options(&self) -> FigureOptions {
        FigureOptions {
            width: self.export_width,
            height: self.export_height,
            world_bounds: if self.export_current_view {
                self.last_viewport.map(|viewport| {
                    let lo = (viewport.min - viewport.center() - self.pan) / self.zoom;
                    let hi = (viewport.max - viewport.center() - self.pan) / self.zoom;
                    [lo.x, lo.y, hi.x, hi.y]
                })
            } else {
                None
            },
        }
    }

    pub(crate) fn prepare_canvas_interaction(
        &mut self,
        ctx: &Context,
        response: &Response,
        view: &ViewGraph,
        layout: &Layout,
    ) {
        let viewport = response.rect;
        self.last_viewport = Some(viewport);

        if self.pending_fit {
            self.fit_to_layout(layout, viewport);
            self.pending_fit = false;
        }
        if let Some(nodes) = self.pending_focus_nodes.take() {
            self.focus_layout_nodes(layout, &nodes, viewport);
        }
        if self.handle_navigation(ctx, response, viewport) {
            self.fit_to_layout(layout, viewport);
        }
        self.handle_selection(ctx, response, viewport, view, layout);
    }

    pub(crate) fn draw_canvas_contents(
        &mut self,
        ui: &mut egui::Ui,
        response: &Response,
        gfa: &GfaGraph,
        view: &ViewGraph,
        layout: &Layout,
        layout_running: bool,
    ) {
        let ctx = ui.ctx().clone();
        let viewport = response.rect;
        let painter = ui.painter_at(viewport);
        painter.rect_filled(viewport, 0.0, self.display.theme.canvas_background());

        let render_params = self.render_params();
        if let Some(cache) = &self.render_cache {
            draw_graph_cached(
                &painter,
                viewport,
                view,
                layout,
                &self.selection,
                &render_params,
                cache,
            );
        } else {
            draw_graph(
                &painter,
                viewport,
                view,
                layout,
                &self.selection,
                &render_params,
            );
        }

        draw_gfa_overlays(
            &painter,
            viewport,
            gfa,
            view,
            layout,
            &render_params,
            self.overlays.selected_path,
            self.overlays.selected_walk,
            self.overlays.show_containments,
        );

        if self.interaction_mode == InteractionMode::Select
            && let Some(start) = self.rubber_start
            && response.dragged_by(egui::PointerButton::Primary)
            && let Some(current) = response.interact_pointer_pos()
        {
            let rect = Rect::from_two_pos(start, current);
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

        if let Some(world) = self.grab_world.filter(|_| self.grabbed_phys.is_some()) {
            let center = viewport.center();
            let screen = Pos2::new(
                (world[0] - self.grab_offset[0]) * self.zoom + self.pan.x + center.x,
                (world[1] - self.grab_offset[1]) * self.zoom + self.pan.y + center.y,
            );
            painter.circle_stroke(
                screen,
                12.0,
                egui::Stroke::new(2.0, Color32::from_rgb(255, 200, 50)),
            );
            painter.circle_filled(screen, 4.0, Color32::from_rgb(255, 200, 50));
        }

        if layout_running {
            painter.text(
                viewport.min + Vec2::new(8.0, 28.0),
                egui::Align2::LEFT_TOP,
                if layout.converged {
                    "Layout settled".to_string()
                } else {
                    format!("Layout: iter {}", layout.iteration)
                },
                egui::FontId::proportional(11.0),
                Color32::from_rgb(100, 200, 100),
            );
        }

        if let Some(target) = draw_minimap(
            ui,
            viewport,
            layout,
            self.zoom,
            self.pan,
            self.display.theme,
        ) {
            self.pan = Vec2::new(-target[0] * self.zoom, -target[1] * self.zoom);
            ctx.request_repaint();
        }
    }

    pub(crate) fn handle_navigation(
        &mut self,
        ctx: &Context,
        response: &Response,
        viewport: Rect,
    ) -> bool {
        if response.clicked() || response.drag_started() {
            response.request_focus();
        }

        if response.hovered() {
            let delta_zoom = ctx.input(|input| input.zoom_delta());
            let was_zoomed = delta_zoom != 1.0;
            let scroll = ctx.input(|input| input.smooth_scroll_delta.y);
            let was_scrolled = scroll != 0.0;

            if was_scrolled || was_zoomed {
                let old_zoom = self.zoom;
                if was_zoomed && !was_scrolled {
                    self.zoom = (self.zoom * delta_zoom).clamp(0.00001, 1000.0);
                } else if was_scrolled {
                    let factor = (scroll * -0.001).exp();
                    self.zoom = (self.zoom * factor).clamp(0.00001, 1000.0);
                }

                if let Some(cursor) = ctx.input(|input| input.pointer.hover_pos()) {
                    let center = viewport.center();
                    let world = (cursor - center - self.pan) / old_zoom;
                    self.pan = cursor - center - world * self.zoom;
                }
            }
        }

        if ctx.input(|input| input.pointer.middle_down())
            || (self.interaction_mode == InteractionMode::Pan
                && response.dragged_by(egui::PointerButton::Primary))
        {
            self.pan += response.drag_delta();
        }

        let mut fit = false;
        if !ctx.text_edit_focused() {
            ctx.input(|input| {
                if input.modifiers.command || input.modifiers.alt {
                    return;
                }
                if input.key_pressed(Key::P) {
                    self.interaction_mode = InteractionMode::Pan;
                }
                if input.key_pressed(Key::S) {
                    self.interaction_mode = InteractionMode::Select;
                }
                if input.key_pressed(Key::G) {
                    self.interaction_mode = InteractionMode::Grab;
                }
                if input.key_pressed(Key::F) {
                    fit = true;
                }
            });
        }
        fit
    }

    pub(crate) fn handle_selection(
        &mut self,
        ctx: &Context,
        response: &Response,
        viewport: Rect,
        view: &ViewGraph,
        layout: &Layout,
    ) {
        if self.interaction_mode != InteractionMode::Select {
            return;
        }

        if response.clicked() {
            let click_pos = response.interact_pointer_pos().unwrap_or_default();
            let add = ctx.input(|input| input.modifiers.shift);
            let render_params = self.render_params();
            if let Some(node_index) =
                hit_test_node(click_pos, viewport, view, layout, &render_params)
            {
                self.selection.select_node(node_index, add);
                self.status_msg = format!("Selected: {}", view.nodes[node_index].name);
            } else if !add {
                self.selection.clear();
            }
        }

        if response.drag_started_by(egui::PointerButton::Primary) {
            self.rubber_start = response.interact_pointer_pos();
        }
        if response.drag_stopped()
            && let Some(start) = self.rubber_start.take()
            && let Some(end) = response.interact_pointer_pos()
        {
            let rect = Rect::from_two_pos(start, end);
            if rect.width() > 4.0 || rect.height() > 4.0 {
                self.selection.rubber_band_select(
                    rect,
                    view,
                    layout,
                    self.zoom,
                    self.pan,
                    viewport.center(),
                    ctx.input(|input| input.modifiers.shift),
                );
            }
        }
    }

    pub(crate) fn update_grab_interaction(
        &mut self,
        response: &Response,
        view: &ViewGraph,
        layout: &Layout,
    ) -> GrabInteraction {
        let viewport = response.rect;
        if self.interaction_mode != InteractionMode::Grab {
            if self.grabbed_phys.is_some() {
                self.finish_edit(layout);
            }
            self.clear_grab();
            return GrabInteraction::default();
        }

        if response.drag_started_by(egui::PointerButton::Primary) {
            self.finish_edit(layout);
            if let Some(pointer) = response.interact_pointer_pos()
                && let Some(physics_index) = self.start_grab(pointer, viewport, view, layout)
            {
                self.begin_edit(layout, view, physics_index);
            }
        }

        let dragging = response.dragged_by(egui::PointerButton::Primary);
        let target = if dragging {
            response
                .interact_pointer_pos()
                .and_then(|pointer| self.update_grab(pointer, viewport))
        } else {
            None
        };

        if response.drag_stopped_by(egui::PointerButton::Primary) {
            self.finish_edit(layout);
            self.clear_grab();
        }

        GrabInteraction { target, dragging }
    }

    pub(crate) fn start_grab(
        &mut self,
        screen: Pos2,
        viewport: Rect,
        view: &ViewGraph,
        layout: &Layout,
    ) -> Option<usize> {
        let world = (screen - viewport.center() - self.pan) / self.zoom;
        let render_params = self.render_params();
        self.grabbed_phys = hit_test_node(screen, viewport, view, layout, &render_params)
            .and_then(|node| layout.nearest_physics_point(node, [world.x, world.y]));
        if let Some(physics_index) = self.grabbed_phys {
            let position = layout.positions[physics_index];
            self.grab_offset = [position[0] - world.x, position[1] - world.y];
        }
        self.grabbed_phys
    }

    pub(crate) fn update_grab(
        &mut self,
        screen: Pos2,
        viewport: Rect,
    ) -> Option<([f32; 2], usize)> {
        let physics_index = self.grabbed_phys?;
        let world = (screen - viewport.center() - self.pan) / self.zoom;
        let target = [world.x + self.grab_offset[0], world.y + self.grab_offset[1]];
        self.grab_world = Some(target);
        Some((target, physics_index))
    }

    pub(crate) fn clear_grab(&mut self) {
        self.grabbed_phys = None;
        self.grab_world = None;
    }

    pub(crate) fn fit_to_layout(&mut self, layout: &Layout, viewport: Rect) {
        if layout.positions.is_empty() {
            return;
        }

        let mut min = [f32::MAX; 2];
        let mut max = [f32::MIN; 2];
        for &[x, y] in &layout.positions {
            min[0] = min[0].min(x);
            max[0] = max[0].max(x);
            min[1] = min[1].min(y);
            max[1] = max[1].max(y);
        }

        self.zoom = (viewport.width() * 0.85 / (max[0] - min[0]).max(1.0))
            .min(viewport.height() * 0.85 / (max[1] - min[1]).max(1.0))
            .clamp(0.00001, 1000.0);
        self.pan = Vec2::new(
            -(min[0] + max[0]) * 0.5 * self.zoom,
            -(min[1] + max[1]) * 0.5 * self.zoom,
        );
    }

    pub(crate) fn focus_layout_nodes(&mut self, layout: &Layout, nodes: &[usize], viewport: Rect) {
        let mut min = [f32::INFINITY; 2];
        let mut max = [f32::NEG_INFINITY; 2];
        for &node in nodes {
            if node >= layout.num_nodes() {
                continue;
            }
            for &[x, y] in layout.pts(node) {
                min[0] = min[0].min(x);
                max[0] = max[0].max(x);
                min[1] = min[1].min(y);
                max[1] = max[1].max(y);
            }
        }
        if !min[0].is_finite() {
            return;
        }

        let width = (max[0] - min[0]).max(80.0);
        let height = (max[1] - min[1]).max(80.0);
        self.zoom = (viewport.width() * 0.72 / width)
            .min(viewport.height() * 0.72 / height)
            .clamp(0.00001, 1000.0);
        self.pan = Vec2::new(
            -(min[0] + max[0]) * 0.5 * self.zoom,
            -(min[1] + max[1]) * 0.5 * self.zoom,
        );
    }

    fn resolve_overlay_action(
        &self,
        gfa: &GfaGraph,
        view: &ViewGraph,
        action: OverlayAction,
    ) -> Option<OverlayResolution> {
        let (mut nodes, total_steps, label, focus) = match action {
            OverlayAction::FocusPath(index) | OverlayAction::SelectPath(index) => {
                let path = gfa.paths.get(index)?;
                (
                    gfa.path_steps(path)
                        .iter()
                        .filter_map(|step| view.seg_to_node.get(&step.segment).copied())
                        .collect::<Vec<_>>(),
                    path.steps.len(),
                    format!("path {}", path.name.as_ref()),
                    matches!(action, OverlayAction::FocusPath(_)),
                )
            }
            OverlayAction::FocusWalk(index) | OverlayAction::SelectWalk(index) => {
                let walk = gfa.walks.get(index)?;
                (
                    gfa.walk_steps(walk)
                        .iter()
                        .filter_map(|step| view.seg_to_node.get(&step.segment).copied())
                        .collect::<Vec<_>>(),
                    walk.steps.len(),
                    format!(
                        "walk {} / h{} / {}",
                        walk.sample_id.as_ref(),
                        walk.haplotype_index,
                        walk.sequence_id.as_ref()
                    ),
                    matches!(action, OverlayAction::FocusWalk(_)),
                )
            }
        };

        nodes.sort_unstable();
        nodes.dedup();
        Some(OverlayResolution {
            nodes,
            total_steps,
            label,
            focus,
        })
    }

    pub(crate) fn apply_overlay_action(
        &mut self,
        gfa: &GfaGraph,
        view: &ViewGraph,
        action: OverlayAction,
    ) {
        let Some(resolution) = self.resolve_overlay_action(gfa, view, action) else {
            return;
        };
        if resolution.nodes.is_empty() {
            self.status_msg = format!(
                "No visible segments from {}; the current filters hide all {} steps.",
                resolution.label, resolution.total_steps
            );
            return;
        }

        if resolution.focus {
            self.pending_focus_nodes = Some(resolution.nodes.clone());
            self.status_msg = format!(
                "Focused {}: {} visible segment{} from {} step{}.",
                resolution.label,
                resolution.nodes.len(),
                if resolution.nodes.len() == 1 { "" } else { "s" },
                resolution.total_steps,
                if resolution.total_steps == 1 { "" } else { "s" },
            );
        } else {
            self.selection.clear();
            self.selection
                .nodes
                .extend(resolution.nodes.iter().copied());
            self.status_msg = format!(
                "Selected {} visible segment{} from {} ({} step{}).",
                resolution.nodes.len(),
                if resolution.nodes.len() == 1 { "" } else { "s" },
                resolution.label,
                resolution.total_steps,
                if resolution.total_steps == 1 { "" } else { "s" },
            );
        }
    }

    pub(crate) fn diagnostics_window(&mut self, ctx: &Context, gfa: Option<&GfaGraph>) {
        if !self.show_diagnostics {
            return;
        }

        egui::Window::new("GFA loading diagnostics")
            .open(&mut self.show_diagnostics)
            .default_width(660.0)
            .show(ctx, |ui| {
                if let Some(gfa) = gfa {
                    if gfa.diagnostics.is_empty() {
                        ui.label("No parser warnings.");
                    } else {
                        ui.label(
                            "This graph loaded with warnings. Some records may have been skipped.",
                        );
                        ui.separator();
                        egui::ScrollArea::vertical()
                            .max_height(420.0)
                            .show(ui, |ui| {
                                for diagnostic in &gfa.diagnostics {
                                    ui.label(format!(
                                        "Line {}: {}",
                                        diagnostic.line, diagnostic.message
                                    ));
                                }
                            });
                    }
                } else {
                    ui.label("Load a graph to see its diagnostics.");
                }
            });
    }

    pub(crate) fn copy_selected_sequences_to_clipboard(
        &mut self,
        ctx: &Context,
        gfa: Option<&GfaGraph>,
        view: Option<&ViewGraph>,
    ) {
        let Some((gfa, view)) = gfa.zip(view) else {
            self.status_msg = "No selected segment with an embedded sequence to copy.".into();
            return;
        };
        if let Some(sequence) = self.copy_selected_sequences(gfa, view) {
            ctx.copy_text(sequence);
        }
    }

    pub(crate) fn copy_selected_sequences(
        &mut self,
        gfa: &GfaGraph,
        view: &ViewGraph,
    ) -> Option<String> {
        let sequence = copy_sequence_to_clipboard(gfa, view, &self.selection);
        self.status_msg = if sequence.is_some() {
            "Selected sequence(s) copied to clipboard.".into()
        } else {
            "No selected segment with an embedded sequence to copy.".into()
        };
        sequence
    }

    pub(crate) fn begin_edit(&mut self, layout: &Layout, view: &ViewGraph, physics_index: usize) {
        let node = layout
            .node_pts_start
            .partition_point(|&start| start <= physics_index)
            .saturating_sub(1);
        let Some(component) = view
            .components
            .iter()
            .find(|component| component.nodes.contains(&node))
        else {
            return;
        };
        let point_count: usize = component
            .nodes
            .iter()
            .map(|&node_index| layout.node_pts_count[node_index])
            .sum();
        if !History::can_record(point_count) {
            self.history.clear();
            self.status_msg =
                "This component exceeds the undo memory limit; movement will not be recorded."
                    .into();
            return;
        }

        let indices: Vec<_> = component
            .nodes
            .iter()
            .flat_map(|&node_index| {
                let start = layout.node_pts_start[node_index];
                start..start + layout.node_pts_count[node_index]
            })
            .collect();
        let before = indices
            .iter()
            .map(|&index| layout.positions[index])
            .collect();
        self.pending_edit = Some(PendingEdit { indices, before });
    }

    pub(crate) fn finish_edit(&mut self, layout: &Layout) {
        if let Some(pending) = self.pending_edit.take() {
            self.history.record(pending, &layout.positions);
        }
    }

    pub(crate) fn apply_history(
        &mut self,
        layout: &mut Layout,
        redo: bool,
    ) -> anyhow::Result<bool> {
        let mut positions = layout.positions.clone();
        if !self.history.apply(&mut positions, redo) {
            return Ok(false);
        }
        layout.restore_positions(&positions)?;
        self.render_cache = None;
        self.grabbed_phys = None;
        self.grab_world = None;
        self.status_msg = if redo {
            "Movement redone."
        } else {
            "Movement undone."
        }
        .into();
        Ok(true)
    }

    pub(crate) fn refresh_render_cache(
        &mut self,
        view: &ViewGraph,
        layout: &Layout,
        allow_cache: bool,
    ) {
        if allow_cache && layout.converged {
            if self
                .render_cache
                .as_ref()
                .is_none_or(|cache| cache.revision != layout.revision())
            {
                self.render_cache = Some(RenderCache::new(view, layout));
            }
        } else {
            self.render_cache = None;
        }
    }

    pub(crate) fn select_all(&mut self, view: &ViewGraph) {
        self.selection.clear();
        self.selection.nodes.extend(0..view.nodes.len());
    }

    pub(crate) fn invert_selection(&mut self, view: &ViewGraph) {
        let old = self.selection.nodes.clone();
        self.selection.clear();
        self.selection
            .nodes
            .extend((0..view.nodes.len()).filter(|index| !old.contains(index)));
    }
}
