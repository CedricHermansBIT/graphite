use super::*;
use eframe::App;
use std::time::Instant;

fn empty_app(context: &Context) -> GfaApp {
    GfaApp::new(
        &eframe::CreationContext::_new_kittest(context.clone()),
        None,
        LayoutBackend::Rust,
        false,
    )
}

fn settle(app: &mut GfaApp, context: &Context) {
    let start = Instant::now();
    while matches!(app.load_state, LoadState::Loading(_)) || app.output_job.is_some() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{}",
            app.status_msg
        );
        app.check_loading(context);
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn loaded_app(context: &Context, directory: &std::path::Path) -> GfaApp {
    let source = directory.join("test.gfa");
    std::fs::write(&source, include_bytes!("../../examples/example.gfa")).unwrap();
    let mut app = empty_app(context);
    app.start_load(source);
    settle(&mut app, context);
    assert!(
        matches!(app.load_state, LoadState::Loaded { .. }),
        "{}",
        app.status_msg
    );
    app
}

#[test]
fn session_round_trip_restores_geometry_view_selection_and_rejects_changed_source() {
    let directory = tempfile::tempdir().unwrap();
    let context = Context::default();
    let mut app = loaded_app(&context, directory.path());
    let positions = if let LoadState::Loaded {
        layout_snapshot, ..
    } = &mut app.load_state
    {
        let positions: Vec<_> = layout_snapshot
            .positions
            .iter()
            .map(|p| [p[0] + 150.0, p[1] - 270.0])
            .collect();
        Arc::make_mut(layout_snapshot)
            .restore_positions(&positions)
            .unwrap();
        positions
    } else {
        unreachable!()
    };
    app.display.theme = ThemePreset::Paper;
    app.zoom = 2.5;
    app.pan = Vec2::new(31.0, -14.0);
    app.selection.nodes.insert(0);
    app.overlays.selected_path = Some(0);
    let session = directory.path().join("view.graphite.json");
    app.start_output(session.clone(), OutputKind::Session);
    settle(&mut app, &context);
    assert!(session.is_file(), "{}", app.status_msg);
    drop(app);

    let mut restored = empty_app(&context);
    restored.start_load(session.clone());
    settle(&mut restored, &context);
    let LoadState::Loaded {
        layout_snapshot, ..
    } = &restored.load_state
    else {
        panic!("{}", restored.status_msg)
    };
    assert_eq!(layout_snapshot.positions, positions);
    assert_eq!(restored.display.theme, ThemePreset::Paper);
    assert_eq!(restored.zoom, 2.5);
    assert_eq!(restored.pan, Vec2::new(31.0, -14.0));
    assert!(restored.selection.nodes.contains(&0));
    assert_eq!(restored.overlays.selected_path, Some(0));
    assert!(!restored.pending_fit);
    drop(restored);

    let source = directory.path().join("test.gfa");
    let changed = String::from_utf8(include_bytes!("../../examples/example.gfa").to_vec())
        .unwrap()
        .replace("DP:f:28", "DP:f:29");
    std::fs::write(source, changed).unwrap();
    let mut rejected = empty_app(&context);
    rejected.start_load(session);
    settle(&mut rejected, &context);
    assert!(!matches!(rejected.load_state, LoadState::Loaded { .. }));
    assert!(
        rejected.status_msg.contains("changed since"),
        "{}",
        rejected.status_msg
    );
}

#[test]
fn cancel_or_failed_load_retains_previous_graph() {
    let directory = tempfile::tempdir().unwrap();
    let context = Context::default();
    let mut app = loaded_app(&context, directory.path());
    let source = app.source_path.clone();
    app.start_load(directory.path().join("missing.gfa"));
    app.cancel_load();
    assert!(matches!(app.load_state, LoadState::Loaded { .. }));
    assert_eq!(source, app.source_path);
    app.start_load(directory.path().join("missing.gfa"));
    settle(&mut app, &context);
    assert!(matches!(app.load_state, LoadState::Loaded { .. }));
    assert!(app.status_msg.contains("failed"));
}

#[test]
fn warnings_are_visible_and_strict_mode_rejects_them() {
    let directory = tempfile::tempdir().unwrap();
    let context = Context::default();
    let path = directory.path().join("warnings.gfa");
    std::fs::write(&path, "S\ta\tACGT\nL\ta\t+\tmissing\t+\t0M\n").unwrap();
    let mut app = empty_app(&context);
    app.start_load(path.clone());
    settle(&mut app, &context);
    assert!(app.show_diagnostics);
    assert!(app.status_msg.contains("1 warnings"));
    let mut strict = empty_app(&context);
    strict.strict_parsing = true;
    strict.start_load(path);
    settle(&mut strict, &context);
    assert!(!matches!(strict.load_state, LoadState::Loaded { .. }));
    assert!(strict.status_msg.contains("Strict parsing rejected"));
}

#[test]
fn typing_into_search_does_not_change_mode_or_fit_view() {
    let directory = tempfile::tempdir().unwrap();
    let context = Context::default();
    let mut app = loaded_app(&context, directory.path());
    app.pending_fit = false;
    app.interaction_mode = InteractionMode::Pan;
    app.zoom = 1.75;
    let mut query = String::new();
    for key in [Key::G, Key::F, Key::S, Key::P] {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 700.0))),
            events: vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let mut output = context.run_ui(input, |ui| {
            Panel::top("test_search").show(ui, |ui| {
                ui.add(egui::TextEdit::singleline(&mut query).id(egui::Id::new("search")))
                    .request_focus();
            });
            app.canvas(ui);
        });
        output.textures_delta.clear();
        assert!(app.interaction_mode == InteractionMode::Pan);
        assert_eq!(app.zoom, 1.75);
    }
}

#[test]
fn appearance_and_recent_files_persist_without_sequences() {
    #[derive(Default)]
    struct MemoryStorage(std::collections::BTreeMap<String, String>);
    impl eframe::Storage for MemoryStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }
        fn remove_string(&mut self, key: &str) {
            self.0.remove(key);
        }
        fn flush(&mut self) {}
    }
    let context = Context::default();
    let mut app = empty_app(&context);
    app.display.theme = ThemePreset::Light;
    app.display.show_labels = false;
    app.preferences.remember(PathBuf::from("example.gfa"));
    let mut storage = MemoryStorage::default();
    app.save(&mut storage);
    let mut cc = eframe::CreationContext::_new_kittest(context);
    cc.storage = Some(&storage);
    let restored = GfaApp::new(&cc, None, LayoutBackend::Rust, false);
    assert_eq!(restored.display.theme, ThemePreset::Light);
    assert!(!restored.display.show_labels);
    assert_eq!(
        restored.preferences.recent_files,
        vec![PathBuf::from("example.gfa")]
    );
}

#[test]
fn background_session_uses_stable_snapshot_and_source_cannot_be_overwritten() {
    let directory = tempfile::tempdir().unwrap();
    let context = Context::default();
    let mut app = loaded_app(&context, directory.path());
    let source = app.source_path.clone().unwrap();
    let input = std::fs::read(&source).unwrap();
    app.start_output(source.clone(), OutputKind::Csv);
    assert!(app.output_job.is_none());
    assert_eq!(std::fs::read(source).unwrap(), input);

    let original = match &app.load_state {
        LoadState::Loaded {
            layout_snapshot, ..
        } => layout_snapshot.positions.clone(),
        _ => unreachable!(),
    };
    let session = directory.path().join("snapshot.json");
    app.start_output(session.clone(), OutputKind::Session);
    if let LoadState::Loaded {
        layout_snapshot, ..
    } = &mut app.load_state
    {
        let changed: Vec<_> = original.iter().map(|p| [p[0] + 400.0, p[1]]).collect();
        Arc::make_mut(layout_snapshot)
            .restore_positions(&changed)
            .unwrap();
    }
    settle(&mut app, &context);
    assert_eq!(Session::read(&session).unwrap().positions, original);

    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&session).unwrap()).unwrap();
    json["format_version"] = 99.into();
    std::fs::write(&session, serde_json::to_vec(&json).unwrap()).unwrap();
    assert!(
        Session::read(&session)
            .err()
            .unwrap()
            .to_string()
            .contains("Unsupported session version")
    );
}

#[test]
fn movement_undo_redo_restores_exact_positions() {
    let directory = tempfile::tempdir().unwrap();
    let context = Context::default();
    let mut app = loaded_app(&context, directory.path());
    app.grabbed_phys = Some(0);
    app.begin_edit();
    let pending = app.pending_edit.as_ref().unwrap();
    let indices = pending.indices.clone();
    let (original, changed) = match &mut app.load_state {
        LoadState::Loaded {
            layout_snapshot, ..
        } => {
            let original = layout_snapshot.positions.clone();
            let mut changed = original.clone();
            for i in indices {
                changed[i][0] += 73.0;
            }
            Arc::make_mut(layout_snapshot)
                .restore_positions(&changed)
                .unwrap();
            (original, changed)
        }
        _ => unreachable!(),
    };
    app.grabbed_phys = None;
    app.finish_edit();
    app.undo_redo(false);
    if let LoadState::Loaded {
        layout_snapshot, ..
    } = &app.load_state
    {
        assert_eq!(layout_snapshot.positions, original);
    }
    app.undo_redo(true);
    if let LoadState::Loaded {
        layout_snapshot, ..
    } = &app.load_state
    {
        assert_eq!(layout_snapshot.positions, changed);
    }
}
