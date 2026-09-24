use super::*;

#[derive(Clone, Copy)]
pub(super) enum OutputKind {
    Svg,
    Png,
    Fasta,
    Csv,
    Session,
}

#[derive(Default)]
pub(super) struct ColorRanges {
    pub depth: Option<(f32, f32)>,
    pub read_count: Option<(f32, f32)>,
    pub length: Option<(f32, f32)>,
}

impl ColorRanges {
    pub fn from_graph(graph: &ViewGraph) -> Self {
        fn range(values: impl Iterator<Item = f32>) -> Option<(f32, f32)> {
            let (lo, hi) = values
                .filter(|v| v.is_finite())
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), v| {
                    (lo.min(v), hi.max(v))
                });
            lo.is_finite()
                .then_some((lo, if hi > lo { hi } else { lo + 1.0 }))
        }
        Self {
            depth: range(graph.nodes.iter().filter_map(|n| n.depth).map(|v| v as f32)),
            read_count: range(
                graph
                    .nodes
                    .iter()
                    .filter_map(|n| n.read_count)
                    .map(|v| v as f32),
            ),
            length: range(graph.nodes.iter().map(|n| n.length as f32)),
        }
    }
}

impl GfaApp {
    pub(super) fn start_output(&mut self, path: PathBuf, kind: OutputKind) {
        if self.output_job.is_some() {
            self.status_msg = "An export is already running.".into();
            return;
        }
        let Some(source) = self.source_path.clone() else {
            return;
        };
        if let Err(error) = crate::session::ensure_distinct_output(&path, &source) {
            self.status_msg = error.to_string();
            return;
        }
        let options = FigureOptions {
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
        };
        if matches!(kind, OutputKind::Svg | OutputKind::Png)
            && let Err(error) = options.validate()
        {
            self.status_msg = error.to_string();
            return;
        }
        let LoadState::Loaded {
            gfa,
            view,
            layout_snapshot,
            ..
        } = &self.load_state
        else {
            return;
        };
        let gfa = gfa.clone();
        let view = view.clone();
        let layout = matches!(
            kind,
            OutputKind::Svg | OutputKind::Png | OutputKind::Session
        )
        .then(|| layout_snapshot.clone());
        let selection = self.selection.clone();
        let params = self.render_params();
        let overlays = self.overlays.clone();
        let filter = self.applied_filter.clone();
        let display = self.display.clone();
        let backend = self.layout_backend.as_str().to_owned();
        let zoom = self.zoom;
        let pan = [self.pan.x, self.pan.y];
        self.status_msg = format!("Writing {}…", path.display());
        self.output_job = Some(std::thread::spawn(move || {
            match kind {
                OutputKind::Svg => crate::export::export_svg_with_options(
                    &path,
                    &gfa,
                    &view,
                    layout.as_ref().unwrap(),
                    &params,
                    overlays.selected_path,
                    overlays.selected_walk,
                    overlays.show_containments,
                    &options,
                )?,
                OutputKind::Png => crate::export::export_png_with_options(
                    &path,
                    &gfa,
                    &view,
                    layout.as_ref().unwrap(),
                    &params,
                    overlays.selected_path,
                    overlays.selected_walk,
                    overlays.show_containments,
                    &options,
                )?,
                OutputKind::Fasta => crate::export::export_fasta(&path, &gfa, &view, &selection)?,
                OutputKind::Csv => crate::export::export_csv(&path, &gfa, &view, &selection)?,
                OutputKind::Session => {
                    let layout = layout.unwrap();
                    let mut selection: Vec<_> = selection.nodes.into_iter().collect();
                    selection.sort_unstable();
                    Session {
                        format_version: 1,
                        source,
                        source_sha256: crate::session::fingerprint(&gfa),
                        backend,
                        filter,
                        display,
                        overlays,
                        node_names: view
                            .nodes
                            .iter()
                            .map(|node| node.name.to_string())
                            .collect(),
                        point_counts: layout.node_pts_count.clone(),
                        positions: layout.positions.clone(),
                        selection,
                        zoom,
                        pan,
                    }
                    .write(&path)?;
                }
            }
            Ok(format!("Saved {}.", path.display()))
        }));
    }

    pub(super) fn file_workflow_menu(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.menu_button("Recent files", |ui| {
            let paths = self.preferences.recent_files.clone();
            if paths.is_empty() {
                ui.label("No recent files");
            }
            for path in paths {
                if ui.button(path.display().to_string()).clicked() {
                    self.start_load(path);
                    ui.close();
                }
            }
            ui.separator();
            if ui.button("Clear recent files").clicked() {
                self.preferences.recent_files.clear();
                ui.close();
            }
        });
        if ui.button("Open example graph…").clicked() {
            if let Some(path) = rfd::FileDialog::new()
                .set_file_name("graphite-example.gfa")
                .add_filter("GFA", &["gfa"])
                .save_file()
            {
                let result = self
                    .source_path
                    .as_ref()
                    .map_or(Ok(()), |input| {
                        crate::session::ensure_distinct_output(&path, input)
                    })
                    .and_then(|()| {
                        std::fs::write(&path, include_bytes!("../../examples/example.gfa"))
                            .map_err(Into::into)
                    });
                match result {
                    Ok(()) => self.start_load(path),
                    Err(error) => self.status_msg = format!("Cannot save example: {error}"),
                }
            }
            ui.close();
        }
        ui.separator();
        if ui.button("Open session…").clicked() {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("Graphite session", &["json", "graphite"])
                .pick_file()
            {
                self.start_load(path);
            }
            ui.close();
        }
        if ui
            .add_enabled(
                self.output_job.is_none() && matches!(self.load_state, LoadState::Loaded { .. }),
                egui::Button::new("Save session…"),
            )
            .clicked()
        {
            if let Some(path) = rfd::FileDialog::new()
                .set_file_name("graph.graphite.json")
                .add_filter("Graphite session", &["json"])
                .save_file()
            {
                self.start_output(path, OutputKind::Session);
            }
            ui.close();
        }
        if matches!(self.load_state, LoadState::Loading(_)) && ui.button("Cancel loading").clicked()
        {
            self.cancel_load();
            ui.close();
        }
    }

    pub(super) fn begin_edit(&mut self) {
        let Some(pi) = self.grabbed_phys else {
            return;
        };
        let LoadState::Loaded {
            view,
            layout_snapshot,
            ..
        } = &self.load_state
        else {
            return;
        };
        let node = layout_snapshot
            .node_pts_start
            .partition_point(|&start| start <= pi)
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
            .map(|&ni| layout_snapshot.node_pts_count[ni])
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
            .flat_map(|&ni| {
                let start = layout_snapshot.node_pts_start[ni];
                start..start + layout_snapshot.node_pts_count[ni]
            })
            .collect();
        let before = indices
            .iter()
            .map(|&i| layout_snapshot.positions[i])
            .collect();
        self.pending_edit = Some(PendingEdit { indices, before });
    }

    pub(super) fn finish_edit(&mut self) {
        if let Some(pending) = self.pending_edit.take()
            && let LoadState::Loaded {
                layout_snapshot, ..
            } = &self.load_state
        {
            self.history.record(pending, &layout_snapshot.positions);
        }
    }

    pub(super) fn undo_redo(&mut self, redo: bool) {
        self.finish_edit();
        let interval = self.layout_publish_interval();
        let LoadState::Loaded {
            view,
            layout_runner,
            layout_snapshot,
            ..
        } = &mut self.load_state
        else {
            return;
        };
        let mut positions = layout_snapshot.positions.clone();
        if self.history.apply(&mut positions, redo) {
            layout_runner.stop();
            if let Err(error) = Arc::make_mut(layout_snapshot).restore_positions(&positions) {
                self.status_msg = error.to_string();
                return;
            }
            *layout_runner = LayoutRunner::from_layout(
                view.clone(),
                LayoutParams::default(),
                (**layout_snapshot).clone(),
                interval,
            );
            self.grabbed_phys = None;
            self.grab_world = None;
            self.status_msg = if redo {
                "Movement redone."
            } else {
                "Movement undone."
            }
            .into();
        }
    }

    pub(super) fn edit_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Edit", |ui| {
            let loaded = matches!(self.load_state, LoadState::Loaded { .. });
            if ui
                .add_enabled(
                    loaded && (self.history.can_undo() || self.pending_edit.is_some()),
                    egui::Button::new("Undo movement  Ctrl/Cmd+Z"),
                )
                .clicked()
            {
                self.undo_redo(false);
                ui.close();
            }
            if ui
                .add_enabled(
                    loaded && self.history.can_redo(),
                    egui::Button::new("Redo movement  Ctrl/Cmd+Shift+Z"),
                )
                .clicked()
            {
                self.undo_redo(true);
                ui.close();
            }
        });
    }

    pub(super) fn poll_workflows(&mut self, ctx: &Context) {
        let dropped = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .first()
                .map(|file| file.path().to_path_buf())
        });
        if let Some(path) = dropped {
            self.start_load(path);
        }
        if self
            .pending_rebuild
            .is_some_and(|changed| changed.elapsed() >= Duration::from_millis(250))
            && !ctx.input(|i| i.pointer.any_down())
        {
            self.pending_rebuild = None;
            self.rebuild_view();
        }
        if !ctx.text_edit_focused() && ctx.input(|i| i.modifiers.command && i.key_pressed(Key::Z)) {
            self.undo_redo(ctx.input(|i| i.modifiers.shift));
        }
        if self.output_job.is_some() && ctx.input(|input| input.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.status_msg = "Please wait for the current export to finish before closing.".into();
        }
        if matches!(self.load_state, LoadState::Loading(_))
            || self.output_job.is_some()
            || self.pending_rebuild.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    pub(super) fn diagnostics_window(&mut self, ctx: &Context) {
        if !self.show_diagnostics {
            return;
        }
        egui::Window::new("GFA loading diagnostics")
            .open(&mut self.show_diagnostics)
            .default_width(660.0)
            .show(ctx, |ui| {
                if let LoadState::Loaded { gfa, .. } = &self.load_state {
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
}
