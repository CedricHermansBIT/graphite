use super::*;
use crate::layout::LayoutParams;

impl GfaApp {
    pub(super) fn start_output(&mut self, path: PathBuf, kind: OutputKind) {
        if self.output_job.is_some() {
            self.core.status_msg = "An export is already running.".into();
            return;
        }
        let Some(source) = self.source_path.clone() else {
            return;
        };
        if let Err(error) = crate::session::ensure_distinct_output(&path, &source) {
            self.core.status_msg = error.to_string();
            return;
        }
        let options = self.core.figure_options();
        if matches!(kind, OutputKind::Svg | OutputKind::Png)
            && let Err(error) = options.validate()
        {
            self.core.status_msg = error.to_string();
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
        let selection = self.core.selection.clone();
        let params = self.core.render_params();
        let overlays = self.core.overlays.clone();
        let filter = self.core.applied_filter.clone();
        let display = self.core.display.clone();
        let backend = self.layout_backend.as_str().to_owned();
        let zoom = self.core.zoom;
        let pan = [self.core.pan.x, self.core.pan.y];
        self.core.status_msg = format!("Writing {}…", path.display());
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
                    Session::capture(
                        source,
                        backend,
                        &gfa,
                        &view,
                        layout.as_ref().unwrap(),
                        filter,
                        display,
                        overlays,
                        &selection,
                        zoom,
                        pan,
                    )
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
                    Err(error) => self.core.status_msg = format!("Cannot save example: {error}"),
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

    pub(super) fn finish_edit(&mut self) {
        if self.core.pending_edit.is_none() {
            return;
        }
        let (core, load_state) = (&mut self.core, &self.load_state);
        if let LoadState::Loaded {
            layout_snapshot, ..
        } = load_state
        {
            core.finish_edit(layout_snapshot);
        }
    }

    pub(super) fn undo_redo(&mut self, redo: bool) {
        self.finish_edit();
        let interval = self.layout_publish_interval();
        let (core, load_state) = (&mut self.core, &mut self.load_state);
        let LoadState::Loaded {
            view,
            layout_runner,
            layout_snapshot,
            ..
        } = load_state
        else {
            return;
        };
        if (redo && !core.history.can_redo()) || (!redo && !core.history.can_undo()) {
            return;
        }

        layout_runner.stop();
        match core.apply_history(Arc::make_mut(layout_snapshot), redo) {
            Ok(true) => {
                *layout_runner = LayoutRunner::from_layout(
                    view.clone(),
                    LayoutParams::default(),
                    (**layout_snapshot).clone(),
                    interval,
                );
            }
            Ok(false) => {}
            Err(error) => core.status_msg = error.to_string(),
        }
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
        if self.core.take_due_rebuild(ctx) {
            self.rebuild_view();
        }
        if self.output_job.is_some() && ctx.input(|input| input.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.core.status_msg =
                "Please wait for the current export to finish before closing.".into();
        }
        if matches!(self.load_state, LoadState::Loading(_))
            || self.output_job.is_some()
            || self.core.pending_rebuild.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }
}
