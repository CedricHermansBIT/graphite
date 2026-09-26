//! Browser adapter for Graphite's shared graph, layout, renderer and UI panels.
use eframe::egui::{self, Color32, Context, Key, Pos2, Rect, Vec2};
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
    export::{self, AssemblyStats, FigureOptions},
    filter::{ColorMode, FilterParams},
    gfa::{self, GfaGraph},
    graph::ViewGraph,
    history::{History, PendingEdit},
    layout::{Layout, LayoutBackend, LayoutParams},
    render::{self, RenderCache, RenderParams},
    selection::Selection,
    session::{self, Session},
    ui::{self, DisplayOptions, OverlayAction, OverlayOptions, ThemePreset},
    visuals::{configure_style, draw_minimap},
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

#[derive(Clone, Copy, PartialEq)]
enum InteractionMode {
    Pan,
    Select,
    Grab,
}
#[derive(Clone, Copy)]
enum OutputKind {
    Svg,
    Png,
    Fasta,
    Csv,
    Session,
}
#[derive(Default)]
struct ColorRanges {
    depth: Option<(f32, f32)>,
    read_count: Option<(f32, f32)>,
    length: Option<(f32, f32)>,
}
impl ColorRanges {
    fn from_graph(graph: &ViewGraph) -> Self {
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

struct WebApp {
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
    render_cache: Option<RenderCache>,
    color_ranges: ColorRanges,
    filter: FilterParams,
    applied_filter: FilterParams,
    display: DisplayOptions,
    overlays: OverlayOptions,
    selection: Selection,
    history: History,
    pending_edit: Option<PendingEdit>,
    pending_session: Option<Session>,
    pending_rebuild: Option<Instant>,
    mode: InteractionMode,
    zoom: f32,
    pan: Vec2,
    rubber_start: Option<Pos2>,
    grabbed_phys: Option<usize>,
    grab_offset: [f32; 2],
    focus_nodes: Option<Vec<usize>>,
    last_viewport: Option<Rect>,
    fit_pending: bool,
    running: bool,
    strict_parsing: bool,
    show_diagnostics: bool,
    show_filter: bool,
    show_display: bool,
    show_overlays: bool,
    show_stats: bool,
    show_selection: bool,
    component_query: String,
    component_page: usize,
    export_width: u32,
    export_height: u32,
    export_current_view: bool,
    status: String,
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
        Self {
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
            render_cache: None,
            color_ranges: ColorRanges::default(),
            filter: FilterParams::default(),
            applied_filter: FilterParams::default(),
            display: initial_display,
            overlays: OverlayOptions::default(),
            selection: Selection::default(),
            history: History::default(),
            pending_edit: None,
            pending_session: None,
            pending_rebuild: None,
            mode: InteractionMode::Pan,
            zoom: 1.0,
            pan: Vec2::ZERO,
            rubber_start: None,
            grabbed_phys: None,
            grab_offset: [0.0; 2],
            focus_nodes: None,
            last_viewport: None,
            fit_pending: false,
            running: true,
            strict_parsing: false,
            show_diagnostics: false,
            show_filter: true,
            show_display: true,
            show_overlays: true,
            show_stats: true,
            show_selection: true,
            component_query: String::new(),
            component_page: 0,
            export_width: 2400,
            export_height: 1600,
            export_current_view: false,
            status: "Open a GFA file to start.".into(),
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
            self.status = "A graph is already loading.".into();
            return;
        }

        let session = self.pending_session.take();
        let retry_session = session.clone();
        let filter = session
            .as_ref()
            .map(|saved| saved.filter.clone())
            .unwrap_or_else(|| self.filter.clone());
        let strict_parsing = self.strict_parsing;
        let queue = self.load_queue.clone();

        self.loading = true;
        self.error_message = None;
        self.status = format!("Loading {name}…");

        rayon::spawn(move || {
            let result = (|| -> anyhow::Result<PreparedWebGraph> {
                let gfa = gfa::parse_gfa_owned(bytes)?;
                anyhow::ensure!(
                    !strict_parsing || gfa.diagnostics.is_empty(),
                    "Strict parsing rejected {} warning(s)",
                    gfa.diagnostics.len()
                );
                if let Some(saved) = &session {
                    anyhow::ensure!(
                        saved.source_sha256 == session::fingerprint(&gfa),
                        "The selected GFA does not match this session"
                    );
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
                    saved.validate_graph(&gfa, &view, &layout)?;
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
                self.status = format!(
                    "{name}: {} segments, {} links, {} components, {} warnings",
                    view.node_count(),
                    view.edge_count(),
                    view.components.len(),
                    gfa.diagnostics.len()
                );
                self.error_message = None;
                self.filename = name;
                self.color_ranges = ColorRanges::from_graph(&view);
                self.applied_filter = session
                    .as_ref()
                    .map(|saved| saved.filter.clone())
                    .unwrap_or_else(|| self.filter.clone());
                self.filter = self.applied_filter.clone();
                self.stats = Some(stats);
                self.gfa = Some(gfa);
                self.view = Some(view);
                self.layout = Some(layout);
                self.render_cache = None;
                self.history.clear();
                self.selection.clear();
                self.component_query.clear();
                self.component_page = 0;

                if let Some(saved) = session {
                    self.display = saved.display;
                    self.overlays = saved.overlays;
                    self.selection.nodes.extend(saved.selection);
                    self.zoom = saved.zoom;
                    self.pan = Vec2::new(saved.pan[0], saved.pan[1]);
                    self.fit_pending = false;
                } else {
                    self.overlays = OverlayOptions::default();
                    self.fit_pending = true;
                }
            }
            Err(error) => {
                self.pending_session = finished.retry_session;
                self.status = format!("Could not load graph: {error}");
                self.error_message = Some(self.status.clone());
            }
        }
    }

    fn rebuild(&mut self) {
        if let Some(gfa) = &self.gfa {
            let view = ViewGraph::from_gfa(gfa, &self.filter);
            match Layout::try_new_with_graph_backend(
                &view,
                LayoutBackend::Rust,
                &AtomicBool::new(false),
            ) {
                Ok(layout) => {
                    self.status = format!(
                        "Showing {} segments and {} links",
                        view.node_count(),
                        view.edge_count()
                    );
                    self.color_ranges = ColorRanges::from_graph(&view);
                    self.view = Some(view);
                    self.layout = Some(layout);
                    self.render_cache = None;
                    self.selection.clear();
                    self.history.clear();
                    self.fit_pending = true;
                    self.applied_filter = self.filter.clone();
                }
                Err(error) => self.status = format!("Layout failed: {error:#}"),
            }
        }
    }

    fn fit(&mut self, viewport: Rect) {
        let Some(layout) = &self.layout else { return };
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

    fn focus(&mut self, nodes: &[usize], viewport: Rect) {
        let Some(layout) = &self.layout else { return };
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
        self.zoom = (viewport.width() * 0.7 / (max[0] - min[0]).max(1.0))
            .min(viewport.height() * 0.7 / (max[1] - min[1]).max(1.0))
            .clamp(0.00001, 1000.0);
        self.pan = Vec2::new(
            -(min[0] + max[0]) * 0.5 * self.zoom,
            -(min[1] + max[1]) * 0.5 * self.zoom,
        );
    }

    fn render_params(&self) -> RenderParams {
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

    fn output(&mut self, kind: OutputKind) {
        let (Some(gfa), Some(view), Some(layout)) = (&self.gfa, &self.view, &self.layout) else {
            return;
        };
        let options = FigureOptions {
            width: self.export_width,
            height: self.export_height,
            world_bounds: if self.export_current_view {
                self.last_viewport.map(|vp| {
                    let lo = (vp.min - vp.center() - self.pan) / self.zoom;
                    let hi = (vp.max - vp.center() - self.pan) / self.zoom;
                    [lo.x, lo.y, hi.x, hi.y]
                })
            } else {
                None
            },
        };
        let result: anyhow::Result<(&str, &str, Vec<u8>)> = (|| match kind {
            OutputKind::Svg => Ok((
                "graphite.svg",
                "image/svg+xml",
                export::generate_svg_with_options(
                    gfa,
                    view,
                    layout,
                    &self.render_params(),
                    self.overlays.selected_path,
                    self.overlays.selected_walk,
                    self.overlays.show_containments,
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
                    &self.render_params(),
                    self.overlays.selected_path,
                    self.overlays.selected_walk,
                    self.overlays.show_containments,
                    &options,
                )?,
            )),
            OutputKind::Fasta => Ok((
                "selection.fasta",
                "text/plain",
                export::generate_fasta(gfa, view, &self.selection),
            )),
            OutputKind::Csv => Ok((
                "graphite-stats.csv",
                "text/csv",
                export::generate_csv(gfa, view, &self.selection)?,
            )),
            OutputKind::Session => {
                let mut selection: Vec<_> = self.selection.nodes.iter().copied().collect();
                selection.sort_unstable();
                let session = Session {
                    format_version: 1,
                    source: self.filename.clone().into(),
                    source_sha256: session::fingerprint(gfa),
                    backend: "rust".into(),
                    filter: self.applied_filter.clone(),
                    display: self.display.clone(),
                    overlays: self.overlays.clone(),
                    node_names: view.nodes.iter().map(|n| n.name.to_string()).collect(),
                    point_counts: layout.node_pts_count.clone(),
                    positions: layout.positions.clone(),
                    selection,
                    zoom: self.zoom,
                    pan: [self.pan.x, self.pan.y],
                };
                Ok((
                    "graph.graphite.json",
                    "application/json",
                    serde_json::to_vec(&session)?,
                ))
            }
        })();
        match result {
            Ok((name, mime, bytes)) => match download(name, mime, &bytes) {
                Ok(()) => self.status = format!("Downloaded {name}."),
                Err(error) => self.status = format!("Download failed: {error:?}"),
            },
            Err(error) => self.status = format!("Export failed: {error:#}"),
        }
    }

    fn apply_overlay_action(&mut self, action: OverlayAction) {
        let (Some(gfa), Some(view)) = (&self.gfa, &self.view) else {
            return;
        };
        let (mut nodes, focus) = match action {
            OverlayAction::FocusPath(i) | OverlayAction::SelectPath(i) => (
                gfa.paths
                    .get(i)
                    .map(|p| {
                        gfa.path_steps(p)
                            .iter()
                            .filter_map(|s| view.seg_to_node.get(&s.segment).copied())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
                matches!(action, OverlayAction::FocusPath(_)),
            ),
            OverlayAction::FocusWalk(i) | OverlayAction::SelectWalk(i) => (
                gfa.walks
                    .get(i)
                    .map(|w| {
                        gfa.walk_steps(w)
                            .iter()
                            .filter_map(|s| view.seg_to_node.get(&s.segment).copied())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
                matches!(action, OverlayAction::FocusWalk(_)),
            ),
        };
        nodes.sort_unstable();
        nodes.dedup();
        if nodes.is_empty() {
            self.status = "No visible segments in that path or walk.".into();
            return;
        }
        if focus {
            self.focus_nodes = Some(nodes.clone());
        }
        self.selection.clear();
        self.selection.nodes.extend(nodes.iter().copied());
        self.status = format!("Selected {} visible segments.", nodes.len());
    }

    fn copy_sequences(&mut self, ctx: &Context) {
        if let (Some(gfa), Some(view)) = (&self.gfa, &self.view) {
            if let Some(value) = export::copy_sequence_to_clipboard(gfa, view, &self.selection) {
                ctx.copy_text(value);
                self.status = "Selected sequences copied.".into();
                return;
            }
        }
        self.status = "No selected segment with an embedded sequence.".into();
    }

    fn undo_redo(&mut self, redo: bool) {
        self.finish_edit();
        if let Some(layout) = &mut self.layout {
            let mut positions = layout.positions.clone();
            if self.history.apply(&mut positions, redo) {
                if let Err(error) = layout.restore_positions(&positions) {
                    self.status = error.to_string();
                    return;
                }
                self.render_cache = None;
                self.status = if redo {
                    "Movement redone."
                } else {
                    "Movement undone."
                }
                .into();
            }
        }
    }
    fn begin_edit(&mut self, pi: usize) {
        let (Some(layout), Some(view)) = (&self.layout, &self.view) else {
            return;
        };
        let node = layout
            .node_pts_start
            .partition_point(|&start| start <= pi)
            .saturating_sub(1);
        let Some(component) = view.components.iter().find(|c| c.nodes.contains(&node)) else {
            return;
        };
        let indices: Vec<_> = component
            .nodes
            .iter()
            .flat_map(|&n| {
                let start = layout.node_pts_start[n];
                start..start + layout.node_pts_count[n]
            })
            .collect();
        if !History::can_record(indices.len()) {
            self.history.clear();
            return;
        }
        let before = indices.iter().map(|&i| layout.positions[i]).collect();
        self.pending_edit = Some(PendingEdit { indices, before });
    }
    fn finish_edit(&mut self) {
        if let (Some(pending), Some(layout)) = (self.pending_edit.take(), &self.layout) {
            self.history.record(pending, &layout.positions);
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
                ui.menu_button("Edit", |ui| {
                    if ui
                        .add_enabled(
                            self.history.can_undo(),
                            egui::Button::new("Undo movement  Ctrl/Cmd+Z"),
                        )
                        .clicked()
                    {
                        self.undo_redo(false);
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            self.history.can_redo(),
                            egui::Button::new("Redo movement  Ctrl/Cmd+Shift+Z"),
                        )
                        .clicked()
                    {
                        self.undo_redo(true);
                        ui.close();
                    }
                });
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
                    for (kind, label, enabled) in [
                        (OutputKind::Svg, "Figure as SVG…", self.gfa.is_some()),
                        (OutputKind::Png, "Figure as PNG…", self.gfa.is_some()),
                        (
                            OutputKind::Fasta,
                            "Selected segments as FASTA…",
                            !self.selection.is_empty(),
                        ),
                        (
                            OutputKind::Csv,
                            "Graph statistics as CSV…",
                            self.gfa.is_some(),
                        ),
                    ] {
                        if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                            self.output(kind);
                            ui.close();
                        }
                    }
                });
                ui.menu_button("View", |ui| {
                    ui.checkbox(&mut self.show_filter, "Filter panel");
                    ui.checkbox(&mut self.show_display, "Display panel");
                    ui.checkbox(&mut self.show_overlays, "GFA overlay panel");
                    ui.checkbox(&mut self.show_stats, "Stats panel");
                    ui.checkbox(&mut self.show_selection, "Selection panel");
                    ui.separator();
                    if ui.button("Reset view").clicked() {
                        self.zoom = 1.0;
                        self.pan = Vec2::ZERO;
                        ui.close();
                    }
                    if ui.button("Fit to screen").clicked() {
                        self.fit_pending = true;
                        ui.close();
                    }
                });
                ui.menu_button("Select", |ui| {
                    if ui.button("Select all").clicked() {
                        if let Some(view) = &self.view {
                            self.selection.clear();
                            self.selection.nodes.extend(0..view.nodes.len());
                        }
                        ui.close();
                    }
                    if ui.button("Deselect all").clicked() {
                        self.selection.clear();
                        ui.close();
                    }
                    if ui.button("Invert selection").clicked() {
                        if let Some(view) = &self.view {
                            let old = self.selection.nodes.clone();
                            self.selection.clear();
                            self.selection
                                .nodes
                                .extend((0..view.nodes.len()).filter(|i| !old.contains(i)));
                        }
                        ui.close();
                    }
                });
                ui.menu_button("Settings", |ui| {
                    ui.checkbox(
                        &mut self.strict_parsing,
                        "Reject graphs with parser warnings",
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
                            }
                        }
                    });
                });
                ui.separator();
                for (mode, label, key) in [
                    (InteractionMode::Pan, "Pan", "P"),
                    (InteractionMode::Select, "Select", "S"),
                    (InteractionMode::Grab, "Move", "G"),
                ] {
                    if ui
                        .selectable_label(self.mode == mode, format!("{label}  {key}"))
                        .clicked()
                    {
                        self.mode = mode;
                    }
                }
                if ui.button("Fit  F").clicked() {
                    self.fit_pending = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(&self.status)
                            .small()
                            .color(Color32::GRAY),
                    );
                });
            })
        });
    }

    fn left_panel(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        Panel::left("left_panel")
            .resizable(true)
            .default_size(300.0)
            .size_range(270.0..=420.0)
            .show(root, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.show_filter {
                            if ui::filter_panel(ui, &mut self.filter) {
                                self.pending_rebuild = Some(Instant::now());
                            }
                            ui.separator();
                        }
                        if self.show_display {
                            let old = self.display.theme;
                            if ui::display_panel(ui, &mut self.display) {
                                if self.display.theme != old {
                                    configure_style(&ctx, self.display.theme);
                                }
                                ctx.request_repaint();
                            }
                            ui.separator();
                        }
                        if self.show_overlays {
                            let mut action = None;
                            if let Some(gfa) = &self.gfa {
                                if !gfa.paths.is_empty()
                                    || !gfa.walks.is_empty()
                                    || !gfa.containments.is_empty()
                                {
                                    let (_, a) = ui::overlays_panel(ui, gfa, &mut self.overlays);
                                    action = a;
                                    ui.separator();
                                }
                            }
                            if let Some(action) = action {
                                self.apply_overlay_action(action);
                            }
                        }
                        if self.show_stats {
                            if let (Some(stats), Some(view)) = (&self.stats, &self.view) {
                                ui::stats_panel(ui, stats, view);
                                ui.separator();
                            }
                        }
                    });
            });
    }

    fn right_panel(&mut self, root: &mut egui::Ui) {
        if !self.show_selection {
            return;
        }
        let (mut copy_seq, mut export_fasta, mut select_component) = (false, false, false);
        let mut focus = None;
        Panel::right("right_panel")
            .resizable(true)
            .default_size(280.0)
            .size_range(240.0..=420.0)
            .show(root, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if let (Some(gfa), Some(view)) = (&self.gfa, &self.view) {
                            focus = ui::component_table(
                                ui,
                                &view.components,
                                &mut self.component_query,
                                &mut self.component_page,
                            );
                            ui.add_space(10.0);
                            ui.separator();
                            ui.add_space(8.0);
                            ui::selection_panel(
                                ui,
                                &self.selection,
                                gfa,
                                view,
                                &mut copy_seq,
                                &mut export_fasta,
                                &mut select_component,
                            );
                        } else {
                            ui.label("No file loaded.");
                        }
                    });
            });
        if copy_seq {
            self.copy_sequences(&root.ctx().clone());
        }
        if export_fasta {
            self.output(OutputKind::Fasta);
        }
        if select_component {
            if let (Some(&start), Some(view)) = (self.selection.nodes.iter().next(), &self.view) {
                self.selection.select_component(start, view, false);
            }
        }
        if let Some(nodes) = focus {
            self.selection.clear();
            self.selection.nodes.extend(nodes.iter().copied());
            self.focus_nodes = Some(nodes);
        }
    }

    fn canvas(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        CentralPanel::default()
            .frame(egui::Frame::new().fill(self.display.theme.canvas_background()))
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
                self.last_viewport = Some(vp);
                if self.fit_pending {
                    self.fit(vp);
                    self.fit_pending = false;
                }
                if let Some(nodes) = self.focus_nodes.take() {
                    self.focus(&nodes, vp);
                }
                if response.clicked() || response.drag_started() {
                    response.request_focus();
                }
                if response.hovered() {
                    let pinch = ctx.input(|i| i.zoom_delta());
                    let scroll = ctx.input(|i| i.smooth_scroll_delta.y);
                    let factor = if pinch != 1.0 {
                        pinch
                    } else {
                        (scroll * 0.003).exp()
                    };
                    if factor != 1.0 {
                        let old = self.zoom;
                        self.zoom = (self.zoom * factor).clamp(0.00001, 1000.0);
                        let pointer = ctx.pointer_hover_pos().unwrap_or(vp.center());
                        let world = (pointer - vp.center() - self.pan) / old;
                        self.pan = pointer - vp.center() - world * self.zoom;
                    }
                }
                if self.mode == InteractionMode::Pan || ctx.input(|i| i.pointer.middle_down()) {
                    if response.dragged() {
                        self.pan += response.drag_delta();
                    }
                }
                if !ctx.text_edit_focused() {
                    ctx.input(|i| {
                        if i.modifiers.command || i.modifiers.alt {
                            return;
                        }
                        if i.key_pressed(Key::P) {
                            self.mode = InteractionMode::Pan;
                        }
                        if i.key_pressed(Key::S) {
                            self.mode = InteractionMode::Select;
                        }
                        if i.key_pressed(Key::G) {
                            self.mode = InteractionMode::Grab;
                        }
                        if i.key_pressed(Key::F) {
                            self.fit_pending = true;
                        }
                    });
                }
                if self.mode == InteractionMode::Select {
                    if response.clicked() {
                        let add = ctx.input(|i| i.modifiers.shift);
                        let pointer = response.interact_pointer_pos().unwrap_or_default();
                        if let (Some(view), Some(layout)) = (&self.view, &self.layout) {
                            if let Some(node) = render::hit_test_node(
                                pointer,
                                vp,
                                view,
                                layout,
                                &self.render_params(),
                            ) {
                                self.selection.select_node(node, add);
                                self.status = format!("Selected: {}", view.nodes[node].name);
                            } else if !add {
                                self.selection.clear();
                            }
                        }
                    }
                    if response.drag_started_by(egui::PointerButton::Primary) {
                        self.rubber_start = response.interact_pointer_pos();
                    }
                    if response.drag_stopped() {
                        if let (Some(start), Some(end), Some(view), Some(layout)) = (
                            self.rubber_start.take(),
                            response.interact_pointer_pos(),
                            &self.view,
                            &self.layout,
                        ) {
                            let rect = Rect::from_two_pos(start, end);
                            if rect.width() > 4.0 || rect.height() > 4.0 {
                                self.selection.rubber_band_select(
                                    rect,
                                    view,
                                    layout,
                                    self.zoom,
                                    self.pan,
                                    vp.center(),
                                    ctx.input(|i| i.modifiers.shift),
                                );
                            }
                        }
                    }
                }
                if self.mode == InteractionMode::Grab {
                    if response.drag_started_by(egui::PointerButton::Primary) {
                        self.finish_edit();
                        if let (Some(pointer), Some(view), Some(layout)) =
                            (response.interact_pointer_pos(), &self.view, &self.layout)
                        {
                            let world = (pointer - vp.center() - self.pan) / self.zoom;
                            self.grabbed_phys = render::hit_test_node(
                                pointer,
                                vp,
                                view,
                                layout,
                                &self.render_params(),
                            )
                            .and_then(|n| layout.nearest_physics_point(n, [world.x, world.y]));
                            if let Some(pi) = self.grabbed_phys {
                                self.grab_offset = [
                                    layout.positions[pi][0] - world.x,
                                    layout.positions[pi][1] - world.y,
                                ];
                            }
                        }
                        if let Some(pi) = self.grabbed_phys {
                            self.begin_edit(pi);
                        }
                    }
                    if response.dragged_by(egui::PointerButton::Primary) {
                        if let (Some(pi), Some(pointer), Some(layout)) = (
                            self.grabbed_phys,
                            response.interact_pointer_pos(),
                            &mut self.layout,
                        ) {
                            let world = (pointer - vp.center() - self.pan) / self.zoom;
                            layout.drag_preview_to(
                                [world.x + self.grab_offset[0], world.y + self.grab_offset[1]],
                                pi,
                            );
                            self.render_cache = None;
                            ctx.request_repaint();
                        }
                    }
                    if response.drag_stopped() {
                        self.grabbed_phys = None;
                        self.finish_edit();
                    }
                } else if self.grabbed_phys.take().is_some() {
                    self.finish_edit();
                }
                if let (Some(gfa), Some(view), Some(layout)) = (&self.gfa, &self.view, &self.layout)
                {
                    let painter = ui.painter_at(vp);
                    let rp = self.render_params();
                    if let Some(cache) = &self.render_cache {
                        render::draw_graph_cached(
                            &painter,
                            vp,
                            view,
                            layout,
                            &self.selection,
                            &rp,
                            cache,
                        );
                    } else {
                        render::draw_graph(&painter, vp, view, layout, &self.selection, &rp);
                    }
                    render::draw_gfa_overlays(
                        &painter,
                        vp,
                        gfa,
                        view,
                        layout,
                        &rp,
                        self.overlays.selected_path,
                        self.overlays.selected_walk,
                        self.overlays.show_containments,
                    );
                    if self.mode == InteractionMode::Select {
                        if let (Some(start), Some(end)) =
                            (self.rubber_start, response.interact_pointer_pos())
                        {
                            if response.dragged() {
                                let rect = Rect::from_two_pos(start, end);
                                painter.rect_stroke(
                                    rect,
                                    0.0,
                                    egui::Stroke::new(1.0, Color32::YELLOW),
                                    egui::StrokeKind::Inside,
                                );
                            }
                        }
                    }
                    painter.text(
                        vp.min + Vec2::new(8.0, 8.0),
                        egui::Align2::LEFT_TOP,
                        match self.mode {
                            InteractionMode::Pan => "Mode: Pan  [P/S/G]",
                            InteractionMode::Select => "Mode: Select  [P/S/G, F=fit]",
                            InteractionMode::Grab => {
                                "Mode: Grab  [P/S/G] — drag a contig to move it"
                            }
                        },
                        egui::FontId::proportional(12.0),
                        self.display.theme.canvas_foreground().gamma_multiply(0.8),
                    );
                    if self.running {
                        painter.text(
                            vp.min + Vec2::new(8.0, 28.0),
                            egui::Align2::LEFT_TOP,
                            if layout.converged {
                                "Layout settled".into()
                            } else {
                                format!("Layout: iter {}", layout.iteration)
                            },
                            egui::FontId::proportional(11.0),
                            Color32::from_rgb(100, 200, 100),
                        );
                    }
                    if let Some(target) =
                        draw_minimap(ui, vp, layout, self.zoom, self.pan, self.display.theme)
                    {
                        self.pan = Vec2::new(-target[0] * self.zoom, -target[1] * self.zoom);
                        ctx.request_repaint();
                    }
                }
            });
    }
}

impl eframe::App for WebApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let preferences = session::Preferences {
            display: self.display.clone(),
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
                                self.status = format!("Choose the GFA used by {name}.");
                            }
                            Err(error) => self.status = format!("Invalid session: {error:#}"),
                        },
                        Err(error) => self.status = format!("Invalid session: {error}"),
                    }
                }
                Err(error) => {
                    self.status = error.clone();
                    self.error_message = Some(error);
                }
            }
        }
        if self
            .pending_rebuild
            .is_some_and(|changed| changed.elapsed() >= Duration::from_millis(250))
            && !ctx.input(|i| i.pointer.any_down())
        {
            self.pending_rebuild = None;
            self.rebuild();
        }
        if !ctx.text_edit_focused() && ctx.input(|i| i.modifiers.command && i.key_pressed(Key::Z)) {
            self.undo_redo(ctx.input(|i| i.modifiers.shift));
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
        if self.show_diagnostics {
            egui::Window::new("GFA loading diagnostics")
                .open(&mut self.show_diagnostics)
                .show(&ctx, |ui| {
                    if let Some(gfa) = &self.gfa {
                        if gfa.diagnostics.is_empty() {
                            ui.label("No parser warnings.");
                        } else {
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                for d in &gfa.diagnostics {
                                    ui.label(format!("Line {}: {}", d.line, d.message));
                                }
                            });
                        }
                    }
                });
        }
        let copy = ctx.input(|i| {
            i.events.iter().any(|e| matches!(e, egui::Event::Copy))
                || (i.modifiers.command && i.key_pressed(Key::C))
        });
        if copy && !ctx.text_edit_focused() {
            self.copy_sequences(&ctx);
        }
        if self.running && self.grabbed_phys.is_none() {
            if let (Some(view), Some(layout)) = (&self.view, &mut self.layout) {
                let start = js_sys::Date::now();
                while !layout.converged && js_sys::Date::now() - start < 6.0 {
                    layout.step(view, &LayoutParams::default(), None);
                }
                if !layout.converged {
                    self.render_cache = None;
                    ctx.request_repaint();
                }
            }
        }
        if let (Some(view), Some(layout)) = (&self.view, &self.layout) {
            if layout.converged
                && self
                    .render_cache
                    .as_ref()
                    .is_none_or(|c| c.revision != layout.revision())
            {
                self.render_cache = Some(RenderCache::new(view, layout));
            }
        }
        if self.pending_rebuild.is_some() || self.loading {
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
