//! Browser adapter for Graphite's shared graph, layout, renderer and UI panels.
use eframe::egui::{self, Context, Pos2, Rect};
use egui::containers::panel::{CentralPanel, Panel};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::{Duration, Instant},
};
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::{JsFuture, spawn_local};

use crate::{
    app_core::{AppCore, OutputKind},
    export::{self, AssemblyStats},
    gfa::{self, GfaGraph},
    graph::ViewGraph,
    layout::{Layout, LayoutBackend, LayoutParams},
    session::{self, Session},
    ui::DisplayOptions,
    visuals::configure_style,
};

#[derive(Clone, Copy)]
enum InputMode {
    Graph,
    Session,
}
type Incoming = Rc<RefCell<Option<Result<(InputMode, String, Vec<u8>), String>>>>;

struct PreparedWebGraph {
    name: String,
    gfa: GfaGraph,
    view: ViewGraph,
    layout: Layout,
    stats: AssemblyStats,
    session: Option<Session>,
}

struct FinishedWebLoad {
    result: Result<PreparedWebGraph, String>,
    retry_session: Option<Session>,
}

type WebLoadQueue = Arc<Mutex<Option<FinishedWebLoad>>>;

#[wasm_bindgen]
pub struct WebHandle {
    runner: eframe::WebRunner,
}

#[wasm_bindgen]
impl WebHandle {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        console_error_panic_hook::set_once();
        Self {
            runner: eframe::WebRunner::new(),
        }
    }

    pub async fn start(&self, canvas: web_sys::HtmlCanvasElement) -> Result<(), JsValue> {
        self.runner
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(|cc| Ok(Box::new(WebApp::new(cc)))),
            )
            .await
    }
}

struct WebApp {
    core: AppCore,
    incoming: Incoming,
    load_queue: WebLoadQueue,
    loading: bool,
    input_mode: Rc<Cell<InputMode>>,
    file_targets: Rc<RefCell<Vec<(Rect, InputMode)>>>,
    file_pointer_opened: Rc<Cell<bool>>,
    filename: String,
    gfa: Option<GfaGraph>,
    view: Option<ViewGraph>,
    layout: Option<Layout>,
    stats: Option<AssemblyStats>,
    pending_session: Option<Session>,
    running: bool,
    error_message: Option<String>,
}

impl WebApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let mut initial_display = cc
            .storage
            .and_then(|storage| {
                eframe::get_value::<session::Preferences>(storage, "graphite.preferences.v1")
            })
            .map(|preferences| preferences.display)
            .unwrap_or_default();
        if !session::valid_display(&initial_display) {
            initial_display = DisplayOptions::default();
        }

        let incoming: Incoming = Rc::new(RefCell::new(None));
        let load_queue: WebLoadQueue = Arc::new(Mutex::new(None));
        let input_mode = Rc::new(Cell::new(InputMode::Graph));
        let file_targets: Rc<RefCell<Vec<(Rect, InputMode)>>> = Rc::new(RefCell::new(Vec::new()));
        let file_pointer_opened = Rc::new(Cell::new(false));
        let input: web_sys::HtmlInputElement = web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .get_element_by_id("gfa-file")
            .unwrap()
            .dyn_into()
            .unwrap();
        let queue = incoming.clone();
        let mode = input_mode.clone();
        let input_for_event = input.clone();
        configure_style(&ctx, initial_display.theme);
        let callback = Closure::<dyn FnMut(_)>::new(move |_event: web_sys::Event| {
            let Some(file) = input_for_event.files().and_then(|files| files.item(0)) else {
                return;
            };
            let name = file.name();
            let queue = queue.clone();
            let ctx = ctx.clone();
            let picked_mode = mode.get();
            let size = file.size();
            let limit = match picked_mode {
                InputMode::Graph => gfa::MAX_WEB_GFA_BYTES,
                InputMode::Session => session::MAX_WEB_SESSION_BYTES,
            };
            if !size.is_finite() || size > limit as f64 {
                *queue.borrow_mut() = Some(Err(format!(
                    "{name} is {:.1} MiB. The browser limit for this file type is {:.0} MiB.",
                    size / (1024.0 * 1024.0),
                    limit as f64 / (1024.0 * 1024.0)
                )));
                ctx.request_repaint();
                input_for_event.set_value("");
                return;
            }
            spawn_local(async move {
                let result = match JsFuture::from(file.array_buffer()).await {
                    Ok(buffer) => {
                        let array = js_sys::Uint8Array::new(&buffer);
                        let length = array.length() as usize;
                        if length > limit {
                            Err(format!(
                                "{name} exceeds the browser {:.0} MiB limit.",
                                limit as f64 / (1024.0 * 1024.0)
                            ))
                        } else {
                            let mut bytes = Vec::new();
                            match bytes.try_reserve_exact(length) {
                                Ok(()) => {
                                    bytes.resize(length, 0);
                                    array.copy_to(&mut bytes);
                                    Ok((picked_mode, name, bytes))
                                }
                                Err(_) => Err(format!(
                                    "Not enough browser memory to open {name}; use desktop Graphite."
                                )),
                            }
                        }
                    }
                    Err(error) => Err(format!(
                        "Could not read {name}: {error:?}. Try desktop Graphite for large files."
                    )),
                };
                *queue.borrow_mut() = Some(result);
                ctx.request_repaint();
            });
            input_for_event.set_value("");
        });
        input.set_onchange(Some(callback.as_ref().unchecked_ref()));
        callback.forget();
        // Open the browser picker from the original pointer event. Browser user
        // activation can expire before egui's next animation frame runs.
        let canvas = web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .get_element_by_id("graphite")
            .unwrap();
        let pointer_input = input.clone();
        let pointer_mode = input_mode.clone();
        let targets = file_targets.clone();
        let pointer_opened = file_pointer_opened.clone();
        let pointer_callback = Closure::<dyn FnMut(_)>::new(move |event: web_sys::PointerEvent| {
            let point = Pos2::new(event.offset_x() as f32, event.offset_y() as f32);
            if let Some((_, mode)) = targets
                .borrow()
                .iter()
                .find(|(rect, _)| rect.contains(point))
            {
                pointer_mode.set(*mode);
                pointer_input.set_accept(match mode {
                    InputMode::Graph => ".gfa,.gfa1,.gfa2,.gz",
                    InputMode::Session => ".json,.graphite",
                });
                pointer_opened.set(true);
                pointer_input.click();
            }
        });
        canvas
            .add_event_listener_with_callback(
                "pointerdown",
                pointer_callback.as_ref().unchecked_ref(),
            )
            .unwrap();
        pointer_callback.forget();
        let core = AppCore::new(initial_display, "Open a GFA file to start.".into());
        Self {
            core,
            incoming,
            load_queue,
            loading: false,
            input_mode,
            file_targets,
            file_pointer_opened,
            filename: String::new(),
            gfa: None,
            view: None,
            layout: None,
            stats: None,
            pending_session: None,
            running: true,
            error_message: None,
        }
    }

    fn pick_file(&self, mode: InputMode) {
        self.input_mode.set(mode);
        let input: web_sys::HtmlInputElement = web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .get_element_by_id("gfa-file")
            .unwrap()
            .dyn_into()
            .unwrap();
        input.set_accept(match mode {
            InputMode::Graph => ".gfa,.gfa1,.gfa2,.gz",
            InputMode::Session => ".json,.graphite",
        });
        input.click();
    }

    fn start_load(&mut self, name: String, bytes: Vec<u8>) {
        if self.loading {
            self.core.status_msg = "A graph is already loading.".into();
            return;
        }

        let session = self.pending_session.take();
        let retry_session = session.clone();
        let filter = session
            .as_ref()
            .map(|saved| saved.filter.clone())
            .unwrap_or_else(|| self.core.filter.clone());
        let strict_parsing = self.core.strict_parsing;
        let queue = self.load_queue.clone();

        self.loading = true;
        self.error_message = None;
        self.core.status_msg = format!("Loading {name}…");

        rayon::spawn(move || {
            let result = (|| -> anyhow::Result<PreparedWebGraph> {
                let gfa = gfa::parse_gfa_owned(bytes)?;
                anyhow::ensure!(
                    !strict_parsing || gfa.diagnostics.is_empty(),
                    "Strict parsing rejected {} warning(s)",
                    gfa.diagnostics.len()
                );
                if let Some(saved) = &session {
                    saved.validate_source(&gfa)?;
                }

                let stats = AssemblyStats::compute(&gfa);
                let view = ViewGraph::from_gfa(&gfa, &filter);
                let mut layout = if let Some(saved) = &session {
                    Layout::try_from_positions(&view, &saved.positions, &AtomicBool::new(false))?
                } else {
                    Layout::try_new_with_graph_backend(
                        &view,
                        LayoutBackend::Rust,
                        &AtomicBool::new(false),
                    )?
                };

                if let Some(saved) = &session {
                    saved.validate_graph_state(&gfa, &view, &layout)?;
                    layout.restore_positions(&saved.positions)?;
                }

                Ok(PreparedWebGraph {
                    name,
                    gfa,
                    view,
                    layout,
                    stats,
                    session,
                })
            })()
            .map_err(|error| format!("{error:#}"));

            if let Ok(mut slot) = queue.lock() {
                *slot = Some(FinishedWebLoad {
                    result,
                    retry_session,
                });
            }
        });
    }

    fn poll_load(&mut self) {
        let finished = self.load_queue.lock().ok().and_then(|mut slot| slot.take());
        let Some(finished) = finished else {
            return;
        };

        self.loading = false;
        match finished.result {
            Ok(prepared) => {
                let PreparedWebGraph {
                    name,
                    gfa,
                    view,
                    layout,
                    stats,
                    session,
                } = prepared;
                self.core.status_msg = format!(
                    "{name}: {} segments, {} links, {} components, {} warnings",
                    view.node_count(),
                    view.edge_count(),
                    view.components.len(),
                    gfa.diagnostics.len()
                );
                self.error_message = None;
                self.filename = name;
                let filter = session
                    .as_ref()
                    .map(|saved| saved.filter.clone())
                    .unwrap_or_else(|| self.core.filter.clone());
                self.core.reset_for_graph(&view, filter);
                if let Some(saved) = &session {
                    self.core.restore_session_ui(saved);
                }
                self.stats = Some(stats);
                self.gfa = Some(gfa);
                self.view = Some(view);
                self.layout = Some(layout);
            }
            Err(error) => {
                self.pending_session = finished.retry_session;
                self.core.status_msg = format!("Could not load graph: {error}");
                self.error_message = Some(self.core.status_msg.clone());
            }
        }
    }

    fn rebuild(&mut self) {
        if let Some(gfa) = &self.gfa {
            let view = ViewGraph::from_gfa(gfa, &self.core.filter);
            match Layout::try_new_with_graph_backend(
                &view,
                LayoutBackend::Rust,
                &AtomicBool::new(false),
            ) {
                Ok(layout) => {
                    self.core.status_msg = format!(
                        "Showing {} segments and {} links",
                        view.node_count(),
                        view.edge_count()
                    );
                    let filter = self.core.filter.clone();
                    self.core.reset_for_graph(&view, filter);
                    self.view = Some(view);
                    self.layout = Some(layout);
                }
                Err(error) => self.core.status_msg = format!("Layout failed: {error:#}"),
            }
        }
    }

    fn output(&mut self, kind: OutputKind) {
        let (Some(gfa), Some(view), Some(layout)) = (&self.gfa, &self.view, &self.layout) else {
            return;
        };
        let options = self.core.figure_options();
        let result: anyhow::Result<(&str, &str, Vec<u8>)> = (|| match kind {
            OutputKind::Svg => Ok((
                "graphite.svg",
                "image/svg+xml",
                export::generate_svg_with_options(
                    gfa,
                    view,
                    layout,
                    &self.core.render_params(),
                    self.core.overlays.selected_path,
                    self.core.overlays.selected_walk,
                    self.core.overlays.show_containments,
                    &options,
                )?,
            )),
            OutputKind::Png => Ok((
                "graphite.png",
                "image/png",
                export::generate_png_with_options(
                    gfa,
                    view,
                    layout,
                    &self.core.render_params(),
                    self.core.overlays.selected_path,
                    self.core.overlays.selected_walk,
                    self.core.overlays.show_containments,
                    &options,
                )?,
            )),
            OutputKind::Fasta => Ok((
                "selection.fasta",
                "text/plain",
                export::generate_fasta(gfa, view, &self.core.selection),
            )),
            OutputKind::Csv => Ok((
                "graphite-stats.csv",
                "text/csv",
                export::generate_csv(gfa, view, &self.core.selection)?,
            )),
            OutputKind::Session => {
                let session = Session::capture(
                    self.filename.clone().into(),
                    "rust".into(),
                    gfa,
                    view,
                    layout,
                    self.core.applied_filter.clone(),
                    self.core.display.clone(),
                    self.core.overlays.clone(),
                    &self.core.selection,
                    self.core.zoom,
                    [self.core.pan.x, self.core.pan.y],
                );
                let bytes = serde_json::to_vec(&session)?;
                anyhow::ensure!(
                    bytes.len() <= session::MAX_WEB_SESSION_BYTES,
                    "Browser sessions are limited to {} MiB",
                    session::MAX_WEB_SESSION_BYTES / (1024 * 1024)
                );
                Ok(("graph.graphite.json", "application/json", bytes))
            }
        })();
        match result {
            Ok((name, mime, bytes)) => match download(name, mime, &bytes) {
                Ok(()) => self.core.status_msg = format!("Downloaded {name}."),
                Err(error) => self.core.status_msg = format!("Download failed: {error:?}"),
            },
            Err(error) => self.core.status_msg = format!("Export failed: {error:#}"),
        }
    }

    fn undo_redo(&mut self, redo: bool) {
        self.finish_edit();
        let (core, layout) = (&mut self.core, &mut self.layout);
        let Some(layout) = layout else {
            return;
        };
        if let Err(error) = core.apply_history(layout, redo) {
            core.status_msg = error.to_string();
        }
    }

    fn finish_edit(&mut self) {
        if self.core.pending_edit.is_none() {
            return;
        }
        let (core, layout) = (&mut self.core, &self.layout);
        if let Some(layout) = layout {
            core.finish_edit(layout);
        }
    }

    fn top_menu(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        self.file_targets.borrow_mut().clear();
        Panel::top("menu_bar").show(root, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    let open_graph = ui.button(if self.pending_session.is_some() {
                        "Open session source GFA…"
                    } else {
                        "Open GFA…"
                    });
                    self.file_targets
                        .borrow_mut()
                        .push((open_graph.rect, InputMode::Graph));
                    if open_graph.clicked() {
                        if !self.file_pointer_opened.replace(false) {
                            self.pick_file(InputMode::Graph);
                        }
                        ui.close();
                    }
                    if ui
                        .add_enabled(!self.loading, egui::Button::new("Open example graph…"))
                        .clicked()
                    {
                        self.start_load(
                            "example.gfa".into(),
                            include_bytes!("../examples/example.gfa").to_vec(),
                        );
                        ui.close();
                    }
                    let open_session = ui.button("Open session…");
                    self.file_targets
                        .borrow_mut()
                        .push((open_session.rect, InputMode::Session));
                    if open_session.clicked() {
                        if !self.file_pointer_opened.replace(false) {
                            self.pick_file(InputMode::Session);
                        }
                        ui.close();
                    }
                    if ui
                        .add_enabled(self.gfa.is_some(), egui::Button::new("Save session…"))
                        .clicked()
                    {
                        self.output(OutputKind::Session);
                        ui.close();
                    }
                });

                let actions = self.core.common_top_menu(
                    ui,
                    &ctx,
                    self.gfa.is_some(),
                    true,
                    self.gfa.as_ref(),
                    self.view.as_ref(),
                );
                if let Some(redo) = actions.undo_redo {
                    self.undo_redo(redo);
                }
                if let Some(kind) = actions.output {
                    self.output(kind);
                }
                if actions.fit {
                    self.core.pending_fit = true;
                }
            });
        });
    }

    fn left_panel(&mut self, root: &mut egui::Ui) {
        if let Some(action) = self.core.show_left_panel(
            root,
            self.gfa.as_ref(),
            self.view.as_ref(),
            self.stats.as_ref(),
            true,
        ) && let (Some(gfa), Some(view)) = (&self.gfa, &self.view)
        {
            self.core.apply_overlay_action(gfa, view, action);
        }
    }

    fn right_panel(&mut self, root: &mut egui::Ui) {
        let mut actions = self
            .core
            .show_right_panel(root, self.gfa.as_ref(), self.view.as_ref());
        self.core
            .apply_right_panel_actions(self.view.as_ref(), &mut actions);

        if actions.copy_sequence {
            self.core.copy_selected_sequences_to_clipboard(
                &root.ctx().clone(),
                self.gfa.as_ref(),
                self.view.as_ref(),
            );
        }
        if actions.export_fasta {
            self.output(OutputKind::Fasta);
        }
    }

    fn canvas(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        CentralPanel::default()
            .frame(egui::Frame::new().fill(self.core.display.theme.canvas_background()))
            .show(root, |ui| {
                if self.gfa.is_none() {
                    ui.centered_and_justified(|ui| {
                        ui.label("Open a GFA file via File → Open GFA…");
                    });
                    return;
                }
                let response =
                    ui.allocate_response(ui.available_size(), egui::Sense::click_and_drag());
                let vp = response.rect;
                if let (Some(view), Some(layout)) = (&self.view, &self.layout) {
                    self.core
                        .prepare_canvas_interaction(&ctx, &response, view, layout);
                }
                let grab = if let (Some(view), Some(layout)) = (&self.view, &self.layout) {
                    self.core.update_grab_interaction(&response, view, layout)
                } else {
                    Default::default()
                };
                if let Some((target, physics_index)) = grab.target
                    && let Some(layout) = &mut self.layout
                {
                    layout.drag_preview_to(target, physics_index);
                    self.core.render_cache = None;
                }
                if grab.dragging {
                    ctx.request_repaint();
                }

                if let (Some(gfa), Some(view), Some(layout)) = (&self.gfa, &self.view, &self.layout)
                {
                    self.core
                        .draw_canvas_contents(ui, &response, gfa, view, layout, self.running);
                }
            });
    }
}

impl eframe::App for WebApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let preferences = session::Preferences {
            display: self.core.display.clone(),
            recent_files: Vec::new(),
        };
        eframe::set_value(storage, "graphite.preferences.v1", &preferences);
    }

    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        self.poll_load();
        let incoming = self.incoming.borrow_mut().take();
        if let Some(result) = incoming {
            match result {
                Ok((InputMode::Graph, name, bytes)) => self.start_load(name, bytes),
                Ok((InputMode::Session, name, bytes)) => {
                    match serde_json::from_slice::<Session>(&bytes) {
                        Ok(session) => match session.validate_basic() {
                            Ok(()) => {
                                self.pending_session = Some(session);
                                self.core.status_msg = format!("Choose the GFA used by {name}.");
                            }
                            Err(error) => {
                                self.core.status_msg = format!("Invalid session: {error:#}")
                            }
                        },
                        Err(error) => self.core.status_msg = format!("Invalid session: {error}"),
                    }
                }
                Err(error) => {
                    self.core.status_msg = error.clone();
                    self.error_message = Some(error);
                }
            }
        }
        if self.core.take_due_rebuild(&ctx) {
            self.rebuild();
        }
        let shortcuts = self.core.shortcut_actions(&ctx);
        if let Some(redo) = shortcuts.undo_redo {
            self.undo_redo(redo);
        }
        if shortcuts.copy {
            self.core.copy_selected_sequences_to_clipboard(
                &ctx,
                self.gfa.as_ref(),
                self.view.as_ref(),
            );
        }
        self.top_menu(root);
        self.left_panel(root);
        self.right_panel(root);
        self.canvas(root);
        if let Some(message) = self.error_message.clone() {
            egui::Window::new("Graphite Web input error")
                .collapsible(false)
                .show(&ctx, |ui| {
                    ui.label(message);
                    if ui.button("Close").clicked() {
                        self.error_message = None;
                    }
                });
        }
        self.core.diagnostics_window(&ctx, self.gfa.as_ref());
        if self.running && self.core.grabbed_phys.is_none() {
            if let (Some(view), Some(layout)) = (&self.view, &mut self.layout) {
                let start = js_sys::Date::now();
                while !layout.converged && js_sys::Date::now() - start < 6.0 {
                    layout.step(view, &LayoutParams::default(), None);
                }
                if !layout.converged {
                    self.core.render_cache = None;
                    ctx.request_repaint();
                }
            }
        }
        {
            let (core, view, layout) = (&mut self.core, &self.view, &self.layout);
            if let (Some(view), Some(layout)) = (view, layout) {
                let allow_cache = core.grabbed_phys.is_none();
                core.refresh_render_cache(view, layout, allow_cache);
            } else {
                core.render_cache = None;
            }
        }
        if self.core.pending_rebuild.is_some() || self.loading {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }
}

fn download(name: &str, mime: &str, bytes: &[u8]) -> Result<(), JsValue> {
    let array = js_sys::Uint8Array::from(bytes);
    let parts = js_sys::Array::new();
    parts.push(&array);
    let options = web_sys::BlobPropertyBag::new();
    options.set_type(mime);
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob)?;
    let anchor: web_sys::HtmlAnchorElement = web_sys::window()
        .unwrap()
        .document()
        .unwrap()
        .create_element("a")?
        .dyn_into()?;
    anchor.set_href(&url);
    anchor.set_download(name);
    anchor.click();
    let callback = Closure::once(move || {
        let _ = web_sys::Url::revoke_object_url(&url);
    });
    web_sys::window()
        .unwrap()
        .set_timeout_with_callback_and_timeout_and_arguments_0(
            callback.as_ref().unchecked_ref(),
            5000,
        )?;
    callback.forget();
    Ok(())
}
