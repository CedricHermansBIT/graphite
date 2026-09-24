//! Rendering: draws the assembly graph onto an egui canvas.
//!
//! Level-of-detail (LOD) strategy:
//!  - zoom < 0.3: draw dots only (no labels, simplified edges).
//!  - zoom 0.3–1.0: draw rectangles with colour; edges as lines.
//!  - zoom > 1.0: full detail with labels.

use egui::{Color32, FontId, Painter, Pos2, Rect, Stroke, Vec2};
use std::borrow::Cow;

use crate::filter::ColorMode;
use crate::gfa::{GfaGraph, PathConnection, Strand};
use crate::graph::{EdgeKind, NodeInfo, ViewGraph};
use crate::layout::Layout;
use crate::selection::Selection;

/// Geometry derived from a settled layout and reused while only pan/zoom changes.
/// The application discards it whenever layout positions change.
pub struct RenderCache {
    pub revision: usize,
    node_bounds: Vec<([f32; 2], [f32; 2])>,
    world_bounds: ([f32; 2], [f32; 2]),
    overview_nodes: Vec<usize>,
    edge_endpoints: Vec<([f32; 2], [f32; 2])>,
    /// Longest edges first, so subpixel edges can be skipped without scanning.
    edges_by_span: Vec<(f32, usize)>,
    /// Spatial representatives for the fitted overview, in painter order.
    overview_edges: Vec<usize>,
}

impl RenderCache {
    pub fn new(graph: &ViewGraph, layout: &Layout) -> Self {
        let mut world_low = [f32::INFINITY; 2];
        let mut world_high = [f32::NEG_INFINITY; 2];
        let node_bounds = (0..graph.node_count())
            .map(|node| {
                let mut low = [f32::INFINITY; 2];
                let mut high = [f32::NEG_INFINITY; 2];
                for &point in layout.pts(node) {
                    low[0] = low[0].min(point[0]);
                    low[1] = low[1].min(point[1]);
                    high[0] = high[0].max(point[0]);
                    high[1] = high[1].max(point[1]);
                }
                world_low[0] = world_low[0].min(low[0]);
                world_low[1] = world_low[1].min(low[1]);
                world_high[0] = world_high[0].max(high[0]);
                world_high[1] = world_high[1].max(high[1]);
                (low, high)
            })
            .collect::<Vec<_>>();

        // At a fitted overview, many nodes land in the same few pixels. Keep
        // one representative per world-space cell, but retain long segments
        // whose geometry spans a cell. Selection is added at draw time.
        const GRID_WIDTH: usize = 384;
        const GRID_HEIGHT: usize = 256;
        let cell_w = (world_high[0] - world_low[0]).max(1.0) / GRID_WIDTH as f32;
        let cell_h = (world_high[1] - world_low[1]).max(1.0) / GRID_HEIGHT as f32;
        let mut grid = vec![usize::MAX; GRID_WIDTH * GRID_HEIGHT];
        let mut overview_nodes = Vec::new();
        for (node, &(low, high)) in node_bounds.iter().enumerate() {
            if high[0] - low[0] > cell_w || high[1] - low[1] > cell_h {
                overview_nodes.push(node);
                continue;
            }
            let point = layout.center(node);
            let x = ((point[0] - world_low[0]) / cell_w) as usize;
            let y = ((point[1] - world_low[1]) / cell_h) as usize;
            let cell = &mut grid[y.min(GRID_HEIGHT - 1) * GRID_WIDTH + x.min(GRID_WIDTH - 1)];
            if *cell == usize::MAX {
                *cell = node;
            }
        }
        overview_nodes.extend(grid.into_iter().filter(|&node| node != usize::MAX));
        overview_nodes.sort_unstable();

        let edge_endpoints = graph
            .edges
            .iter()
            .map(|edge| {
                if edge.from >= layout.num_nodes() || edge.to >= layout.num_nodes() {
                    return ([f32::NAN; 2], [f32::NAN; 2]);
                }
                let from = match edge.from_strand {
                    Strand::Forward => layout.end(edge.from),
                    Strand::Reverse => layout.start(edge.from),
                };
                let to = match edge.to_strand {
                    Strand::Forward => layout.start(edge.to),
                    Strand::Reverse => layout.end(edge.to),
                };
                (from, to)
            })
            .collect::<Vec<_>>();
        let mut edges_by_span = edge_endpoints
            .iter()
            .enumerate()
            .map(|(index, (from, to))| {
                let dx = from[0] - to[0];
                let dy = from[1] - to[1];
                (dx * dx + dy * dy, index)
            })
            .collect::<Vec<_>>();
        edges_by_span.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
        let overview_edges = overview_edge_representatives(&edge_endpoints, world_low, world_high);
        Self {
            revision: layout.revision(),
            node_bounds,
            world_bounds: (world_low, world_high),
            overview_nodes,
            edge_endpoints,
            edges_by_span,
            overview_edges,
        }
    }
}

fn overview_edge_representatives(
    endpoints: &[([f32; 2], [f32; 2])],
    world_low: [f32; 2],
    world_high: [f32; 2],
) -> Vec<usize> {
    // A compact layout can make tens of thousands of links exceed one screen
    // pixel at fit zoom. Retain the longest link in each roughly 2.5-pixel
    // cell and direction for a 1280x720 viewport.
    const GRID_WIDTH: usize = 512;
    const GRID_HEIGHT: usize = 288;
    const DIRECTIONS: usize = 4;
    let cell_w = (world_high[0] - world_low[0]).max(1.0) / GRID_WIDTH as f32;
    let cell_h = (world_high[1] - world_low[1]).max(1.0) / GRID_HEIGHT as f32;
    let mut slots = vec![(f32::NEG_INFINITY, usize::MAX); GRID_WIDTH * GRID_HEIGHT * DIRECTIONS];
    for (index, &(from, to)) in endpoints.iter().enumerate() {
        let dx = to[0] - from[0];
        let dy = to[1] - from[1];
        let span_sq = dx * dx + dy * dy;
        if !span_sq.is_finite() {
            continue;
        }
        let middle = [from[0] + dx * 0.5, from[1] + dy * 0.5];
        let x = ((middle[0] - world_low[0]) / cell_w) as usize;
        let y = ((middle[1] - world_low[1]) / cell_h) as usize;
        let direction = usize::from(dx.abs() < dy.abs()) * 2 + usize::from(dx * dy < 0.0);
        let cell =
            (y.min(GRID_HEIGHT - 1) * GRID_WIDTH + x.min(GRID_WIDTH - 1)) * DIRECTIONS + direction;
        if span_sq > slots[cell].0 {
            slots[cell] = (span_sq, index);
        }
    }
    let mut selected = slots
        .into_iter()
        .filter_map(|(_, index)| (index != usize::MAX).then_some(index))
        .collect::<Vec<_>>();
    selected.sort_unstable();
    selected
}

fn is_fitted_overview(cache: &RenderCache, viewport: Rect, zoom: f32, node_count: usize) -> bool {
    let (low, high) = cache.world_bounds;
    let fit_zoom = (viewport.width() * 0.85 / (high[0] - low[0]).max(1.0))
        .min(viewport.height() * 0.85 / (high[1] - low[1]).max(1.0));
    node_count >= 100_000 && zoom <= fit_zoom * 1.05
}

/// A screen-sized occupancy map avoids a million hash probes at overview zoom.
struct DotCells {
    left: i32,
    top: i32,
    width: usize,
    height: usize,
    occupied: Vec<u8>,
}

impl DotCells {
    fn new(viewport: Rect) -> Self {
        const CELL: f32 = 3.0;
        const MARGIN: f32 = 45.0; // larger than the maximum dot radius
        let left = ((viewport.left() - MARGIN) / CELL).floor() as i32;
        let top = ((viewport.top() - MARGIN) / CELL).floor() as i32;
        let right = ((viewport.right() + MARGIN) / CELL).floor() as i32;
        let bottom = ((viewport.bottom() + MARGIN) / CELL).floor() as i32;
        let width = (right - left + 1).max(0) as usize;
        let height = (bottom - top + 1).max(0) as usize;
        Self {
            left,
            top,
            width,
            height,
            occupied: vec![0; width.saturating_mul(height)],
        }
    }

    fn insert(&mut self, point: Pos2) -> bool {
        let x = (point.x / 3.0).floor() as i32 - self.left;
        let y = (point.y / 3.0).floor() as i32 - self.top;
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return false;
        }
        let cell = &mut self.occupied[y as usize * self.width + x as usize];
        if *cell != 0 {
            return false;
        }
        *cell = 1;
        true
    }
}

pub struct RenderParams {
    pub zoom: f32,
    pub pan: Vec2,
    pub color_mode: ColorMode,
    pub show_labels: bool,
    pub edge_opacity: f32,
    pub edge_visible_min_zoom: f32,
    pub min_depth_color: f32,
    pub max_depth_color: f32,
    pub min_length_color: f32,
    pub max_length_color: f32,
    pub node_scale: f32,
    pub canvas_foreground: Color32,
    pub canvas_background: Color32,
}

impl Default for RenderParams {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan: Vec2::ZERO,
            color_mode: ColorMode::Depth,
            show_labels: true,
            edge_opacity: 0.6,
            edge_visible_min_zoom: 0.0,
            min_depth_color: 0.0,
            max_depth_color: 100.0,
            min_length_color: 1.0,
            max_length_color: 100_000.0,
            node_scale: 1.0,
            canvas_foreground: Color32::from_rgb(190, 205, 226),
            canvas_background: Color32::from_rgb(20, 22, 28),
        }
    }
}

fn draw_dashed_segment(painter: &Painter, start: Pos2, end: Pos2, stroke: Stroke) {
    let delta = end - start;
    let length = delta.length();
    if length <= f32::EPSILON {
        return;
    }
    let direction = delta / length;
    let dash = 6.0_f32;
    let gap = 4.0_f32;
    let mut offset = 0.0_f32;
    while offset < length {
        let dash_end = (offset + dash).min(length);
        painter.line_segment(
            [start + direction * offset, start + direction * dash_end],
            stroke,
        );
        offset += dash + gap;
    }
}

pub fn draw_graph(
    painter: &Painter,
    viewport: Rect,
    graph: &ViewGraph,
    layout: &Layout,
    selection: &Selection,
    params: &RenderParams,
) {
    draw_graph_inner(painter, viewport, graph, layout, selection, params, None);
}

pub fn draw_graph_cached(
    painter: &Painter,
    viewport: Rect,
    graph: &ViewGraph,
    layout: &Layout,
    selection: &Selection,
    params: &RenderParams,
    cache: &RenderCache,
) {
    draw_graph_inner(
        painter,
        viewport,
        graph,
        layout,
        selection,
        params,
        Some(cache),
    );
}

fn draw_graph_inner(
    painter: &Painter,
    viewport: Rect,
    graph: &ViewGraph,
    layout: &Layout,
    selection: &Selection,
    params: &RenderParams,
    cache: Option<&RenderCache>,
) {
    let transform = |world: [f32; 2]| -> Pos2 {
        Pos2::new(
            world[0] * params.zoom + params.pan.x + viewport.center().x,
            world[1] * params.zoom + params.pan.y + viewport.center().y,
        )
    };

    let lod = params.zoom;
    let num_nodes = layout.num_nodes();

    // Segment half-width in screen pixels. Bandage uses a fixed pixel width that
    // scales modestly with zoom so nodes stay visible when zoomed out.
    let half_h = (8.0 * params.node_scale * lod.sqrt()).clamp(1.5, 40.0);
    let margin = (half_h + 4.0) / lod.max(1e-9);
    let world_min = [
        (viewport.left() - viewport.center().x - params.pan.x) / lod.max(1e-9) - margin,
        (viewport.top() - viewport.center().y - params.pan.y) / lod.max(1e-9) - margin,
    ];
    let world_max = [
        (viewport.right() - viewport.center().x - params.pan.x) / lod.max(1e-9) + margin,
        (viewport.bottom() - viewport.center().y - params.pan.y) / lod.max(1e-9) + margin,
    ];

    // --- Draw edges first (under nodes) ---
    let edge_alpha = (params.edge_opacity * 255.0) as u8;
    if lod > params.edge_visible_min_zoom {
        // This is the same one-pixel test used below, evaluated against cached
        // world-space spans. Keep a small margin for floating-point rounding.
        let overview_edges = cache
            .filter(|cache| is_fitted_overview(cache, viewport, lod, graph.nodes.len()))
            .map(|cache| {
                if selection.edges.is_empty() {
                    Cow::Borrowed(cache.overview_edges.as_slice())
                } else {
                    // Selected links stay visible even if another link
                    // represents their overview cell.
                    let mut indices = cache.overview_edges.clone();
                    indices.extend(graph.edges.iter().enumerate().filter_map(|(index, edge)| {
                        selection
                            .edges
                            .contains(&(edge.from, edge.to))
                            .then_some(index)
                    }));
                    indices.sort_unstable();
                    indices.dedup();
                    Cow::Owned(indices)
                }
            });
        let long_edges = cache
            .filter(|_| overview_edges.is_none())
            .filter(|_| selection.edges.is_empty())
            .and_then(|cache| {
                let cutoff_sq = 0.9 / (lod * lod);
                let end = cache
                    .edges_by_span
                    .partition_point(|(span_sq, _)| *span_sq >= cutoff_sq);
                if end >= graph.edges.len() / 4 {
                    return None;
                }
                let mut indices = cache.edges_by_span[..end]
                    .iter()
                    .map(|&(_, index)| index)
                    .collect::<Vec<_>>();
                // Keep the original painter order at edge crossings.
                indices.sort_unstable();
                Some(indices)
            });
        let edge_count = overview_edges.as_ref().map_or_else(
            || long_edges.as_ref().map_or(graph.edges.len(), Vec::len),
            |edges| edges.len(),
        );
        for position in 0..edge_count {
            let edge_index = overview_edges.as_ref().map_or_else(
                || {
                    long_edges
                        .as_ref()
                        .map_or(position, |edges| edges[position])
                },
                |edges| edges[position],
            );
            let edge = &graph.edges[edge_index];
            if edge.from >= num_nodes || edge.to >= num_nodes {
                continue;
            }

            // Connect the correct endpoint of each segment based on strand:
            //   Forward exits from last physics node (end),
            //   Reverse exits from first physics node (start).
            let (p0_world, p1_world) = if let Some(cache) = cache {
                cache.edge_endpoints[edge_index]
            } else {
                (
                    match edge.from_strand {
                        Strand::Forward => layout.end(edge.from),
                        Strand::Reverse => layout.start(edge.from),
                    },
                    match edge.to_strand {
                        Strand::Forward => layout.start(edge.to),
                        Strand::Reverse => layout.end(edge.to),
                    },
                )
            };

            if p0_world[0].max(p1_world[0]) < world_min[0]
                || p0_world[1].max(p1_world[1]) < world_min[1]
                || p0_world[0].min(p1_world[0]) > world_max[0]
                || p0_world[1].min(p1_world[1]) > world_max[1]
            {
                continue;
            }

            let p0 = transform(p0_world);
            let p1 = transform(p1_world);

            let is_selected = selection.edges.contains(&(edge.from, edge.to));
            if !viewport.intersects(Rect::from_two_pos(p0, p1).expand(2.0))
                || (!is_selected && p0.distance_sq(p1) < 1.0)
            {
                continue;
            }
            let color = if is_selected {
                Color32::from_rgba_unmultiplied(255, 200, 50, 220)
            } else {
                Color32::from_rgba_unmultiplied(
                    params.canvas_foreground.r(),
                    params.canvas_foreground.g(),
                    params.canvas_foreground.b(),
                    edge_alpha,
                )
            };

            let edge_w = if is_selected { 2.0 } else { 1.0 };
            let stroke = Stroke::new(edge_w, color);
            if matches!(edge.kind, EdgeKind::Jump { .. }) {
                draw_dashed_segment(painter, p0, p1, stroke);
            } else {
                painter.line_segment([p0, p1], stroke);
            }
        }
    }

    let overview_indices = cache.and_then(|cache| {
        if !is_fitted_overview(cache, viewport, lod, graph.nodes.len()) {
            return None;
        }
        let mut indices = cache.overview_nodes.clone();
        indices.extend(selection.nodes.iter().copied());
        indices.sort_unstable();
        indices.dedup();
        Some(indices)
    });

    // Merge indistinguishable overview dots into screen cells.
    let mut occupied_dots = DotCells::new(viewport);
    // --- Draw nodes as thick polylines through all physics-node chain points ---
    let node_count = overview_indices
        .as_ref()
        .map_or(graph.nodes.len(), Vec::len);
    for position in 0..node_count {
        let ni = overview_indices
            .as_ref()
            .map_or(position, |nodes| nodes[position]);
        let node = &graph.nodes[ni];
        if ni >= num_nodes {
            continue;
        }

        let pts = layout.pts(ni);
        if pts.is_empty() {
            continue;
        }

        // Viewport culling: reuse settled-layout bounds while panning or zooming.
        let (bmin, bmax) = if let Some(cache) = cache {
            cache.node_bounds[ni]
        } else {
            let mut bmin = [f32::MAX; 2];
            let mut bmax = [f32::MIN; 2];
            for &p in pts {
                bmin[0] = bmin[0].min(p[0]);
                bmin[1] = bmin[1].min(p[1]);
                bmax[0] = bmax[0].max(p[0]);
                bmax[1] = bmax[1].max(p[1]);
            }
            (bmin, bmax)
        };
        if bmax[0] < world_min[0]
            || bmax[1] < world_min[1]
            || bmin[0] > world_max[0]
            || bmin[1] > world_max[1]
        {
            continue;
        }
        let screen_min = transform(bmin);
        let screen_max = transform(bmax);
        let bbox = Rect::from_two_pos(screen_min, screen_max).expand(half_h + 4.0);
        if !viewport.intersects(bbox) {
            continue;
        }

        let is_selected = selection.nodes.contains(&ni);
        let base_color = node_color(node, params);
        let border_color = if is_selected {
            Color32::from_rgb(255, 220, 50)
        } else {
            base_color.gamma_multiply(0.55)
        };
        let border_w = if is_selected { 2.5_f32 } else { 1.5_f32 };

        if pts.len() == 1 || (screen_max.x - screen_min.x).max(screen_max.y - screen_min.y) < 3.0 {
            // Single point or extreme zoom-out: dot.
            let sc = transform(pts[pts.len() / 2]);
            if is_selected || occupied_dots.insert(sc) {
                painter.circle_filled(
                    sc,
                    half_h,
                    if is_selected {
                        border_color
                    } else {
                        base_color
                    },
                );
            }
        } else {
            // Draw each chain segment as a filled quadrilateral (thick line segment).
            // At each interior joint, draw a filled circle to cap the gap.
            let screen_pts: Vec<Pos2> = pts.iter().map(|&w| transform(w)).collect();

            if lod <= 0.15 {
                // One polyline instead of a polygon and joint disc per sample.
                painter.add(egui::Shape::line(
                    screen_pts.clone(),
                    Stroke::new(
                        2.0 * half_h,
                        if is_selected {
                            border_color
                        } else {
                            base_color
                        },
                    ),
                ));
            } else {
                for i in 0..screen_pts.len().saturating_sub(1) {
                    let a = screen_pts[i];
                    let b = screen_pts[i + 1];
                    let seg_dir = b - a;
                    let seg_len = seg_dir.length();
                    if seg_len < 0.5 {
                        continue;
                    }
                    let n = Vec2::new(-seg_dir.y, seg_dir.x) / seg_len * half_h;

                    let quad = vec![a + n, a - n, b - n, b + n];
                    let stroke = Stroke::new(border_w, border_color);
                    painter.add(egui::Shape::convex_polygon(quad, base_color, stroke));
                }

                // Fill joints so there are no gaps between quads.
                for &sc in &screen_pts[1..screen_pts.len().saturating_sub(1)] {
                    painter.circle_filled(sc, half_h, base_color);
                }
            }

            // Arrowhead at the END endpoint to show strand direction.
            if lod > 0.15 {
                let last = screen_pts[screen_pts.len() - 1];
                let penult = screen_pts[screen_pts.len() - 2];
                draw_arrow_tip(painter, penult, last, half_h, border_color);
            }

            // Text label at midpoint.
            if params.show_labels {
                let mid = screen_pts[screen_pts.len() / 2];
                // Estimate on-screen segment length for label culling.
                let total_screen_len: f32 =
                    screen_pts.windows(2).map(|w| w[0].distance(w[1])).sum();
                if lod > 0.6 && total_screen_len > 30.0 {
                    let label = if lod > 1.5 {
                        format!("{}\n{}", node.name, format_bp(node.length))
                    } else {
                        node.name.to_string()
                    };
                    painter.text(
                        mid,
                        egui::Align2::CENTER_CENTER,
                        &label,
                        FontId::proportional((11.0 * lod).clamp(9.0, 14.0)),
                        params.canvas_foreground,
                    );
                } else if lod > 0.08 && total_screen_len > 12.0 {
                    painter.text(
                        mid,
                        egui::Align2::CENTER_CENTER,
                        format_bp(node.length),
                        FontId::monospace(7.0),
                        params.canvas_foreground,
                    );
                }
            }
        }
    }
}

/// Draw parsed GFA1 metadata overlays on top of the base assembly graph.
///
/// Paths and walks highlight their oriented segment traversal. Containments
/// attach to the corresponding fractional position along the drawn container
/// polyline. Filtered-out segments are skipped rather than forcing them back
/// into the current view.
#[allow(clippy::too_many_arguments)]
pub fn draw_gfa_overlays(
    painter: &Painter,
    viewport: Rect,
    gfa: &GfaGraph,
    graph: &ViewGraph,
    layout: &Layout,
    params: &RenderParams,
    selected_path: Option<usize>,
    selected_walk: Option<usize>,
    show_containments: bool,
) {
    let path_color = Color32::from_rgb(245, 170, 55);
    let walk_color = Color32::from_rgb(65, 205, 220);
    let containment_color = Color32::from_rgb(190, 125, 235);

    if show_containments {
        let stroke = Stroke::new(1.5, containment_color.gamma_multiply(0.85));
        for containment in &gfa.containments {
            let Some(&container_node) = graph.seg_to_node.get(&containment.container) else {
                continue;
            };
            let Some(&contained_node) = graph.seg_to_node.get(&containment.contained) else {
                continue;
            };
            if container_node >= layout.num_nodes() || contained_node >= layout.num_nodes() {
                continue;
            }

            let container_len = gfa
                .segments
                .get(containment.container)
                .map_or(1, |segment| segment.length.max(1));
            // GFA1 defines C.Pos on the container in its forward
            // sequence orientation, before ContainerOrient is applied.
            let fraction = (containment.position as f32 / container_len as f32).clamp(0.0, 1.0);

            let source = world_to_screen(
                layout.point_at_fraction(container_node, fraction),
                viewport,
                params,
            );
            let target = world_to_screen(
                oriented_entry(layout, contained_node, containment.contained_strand),
                viewport,
                params,
            );
            if !viewport.intersects(Rect::from_two_pos(source, target).expand(4.0)) {
                continue;
            }

            draw_dashed_segment_pattern(painter, source, target, stroke, 2.0, 4.0);
            painter.circle_filled(source, 2.5, containment_color);
        }
    }

    if let Some(path_index) = selected_path
        && let Some(path) = gfa.paths.get(path_index)
    {
        let steps = gfa.path_steps(path);
        draw_path_like_overlay(
            painter,
            viewport,
            graph,
            layout,
            params,
            steps.iter().map(|step| (step.segment, step.strand)),
            path_color,
            5.0,
        );

        for pair_index in 0..steps.len().saturating_sub(1) {
            let from = steps[pair_index];
            let to = steps[pair_index + 1];
            let (Some(&from_node), Some(&to_node)) = (
                graph.seg_to_node.get(&from.segment),
                graph.seg_to_node.get(&to.segment),
            ) else {
                continue;
            };
            if from_node >= layout.num_nodes() || to_node >= layout.num_nodes() {
                continue;
            }
            let a = world_to_screen(
                oriented_exit(layout, from_node, from.strand),
                viewport,
                params,
            );
            let b = world_to_screen(oriented_entry(layout, to_node, to.strand), viewport, params);
            let stroke = Stroke::new(3.0, path_color.gamma_multiply(0.9));
            if matches!(from.connection_to_next, Some(PathConnection::Jump)) {
                draw_dashed_segment(painter, a, b, stroke);
            } else {
                painter.line_segment([a, b], stroke);
            }
        }
    }

    if let Some(walk_index) = selected_walk
        && let Some(walk) = gfa.walks.get(walk_index)
    {
        let steps = gfa.walk_steps(walk);
        draw_path_like_overlay(
            painter,
            viewport,
            graph,
            layout,
            params,
            steps.iter().map(|step| (step.segment, step.strand)),
            walk_color,
            4.0,
        );

        for pair in steps.windows(2) {
            let from = pair[0];
            let to = pair[1];
            let (Some(&from_node), Some(&to_node)) = (
                graph.seg_to_node.get(&from.segment),
                graph.seg_to_node.get(&to.segment),
            ) else {
                continue;
            };
            if from_node >= layout.num_nodes() || to_node >= layout.num_nodes() {
                continue;
            }
            let a = world_to_screen(
                oriented_exit(layout, from_node, from.strand),
                viewport,
                params,
            );
            let b = world_to_screen(oriented_entry(layout, to_node, to.strand), viewport, params);
            painter.line_segment([a, b], Stroke::new(2.5, walk_color.gamma_multiply(0.85)));
        }
    }

    draw_overlay_legend(
        painter,
        viewport,
        gfa,
        selected_path,
        selected_walk,
        show_containments,
        path_color,
        walk_color,
        containment_color,
        params,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_path_like_overlay<I>(
    painter: &Painter,
    viewport: Rect,
    graph: &ViewGraph,
    layout: &Layout,
    params: &RenderParams,
    steps: I,
    color: Color32,
    width: f32,
) where
    I: Iterator<Item = (usize, Strand)>,
{
    for (segment, strand) in steps {
        let Some(&node) = graph.seg_to_node.get(&segment) else {
            continue;
        };
        if node >= layout.num_nodes() {
            continue;
        }
        let points = layout.pts(node);
        if points.len() < 2 {
            continue;
        }
        let screen_points: Vec<Pos2> = match strand {
            Strand::Forward => points
                .iter()
                .map(|&point| world_to_screen(point, viewport, params))
                .collect(),
            Strand::Reverse => points
                .iter()
                .rev()
                .map(|&point| world_to_screen(point, viewport, params))
                .collect(),
        };
        let mut min = screen_points[0];
        let mut max = screen_points[0];
        for point in &screen_points[1..] {
            min.x = min.x.min(point.x);
            min.y = min.y.min(point.y);
            max.x = max.x.max(point.x);
            max.y = max.y.max(point.y);
        }
        let bbox = Rect::from_min_max(min, max);
        if !viewport.intersects(bbox.expand(width + 3.0)) {
            continue;
        }

        painter.add(egui::Shape::line(
            screen_points.clone(),
            Stroke::new(width, color.gamma_multiply(0.86)),
        ));

        if params.zoom > 0.2 && screen_points.len() >= 2 {
            let tip = *screen_points.last().unwrap();
            let penult = screen_points[screen_points.len() - 2];
            draw_arrow_tip(painter, penult, tip, width * 1.15, color);
        }
    }
}

#[inline]
fn oriented_entry(layout: &Layout, node: usize, strand: Strand) -> [f32; 2] {
    match strand {
        Strand::Forward => layout.start(node),
        Strand::Reverse => layout.end(node),
    }
}

#[inline]
fn oriented_exit(layout: &Layout, node: usize, strand: Strand) -> [f32; 2] {
    match strand {
        Strand::Forward => layout.end(node),
        Strand::Reverse => layout.start(node),
    }
}

#[inline]
fn world_to_screen(world: [f32; 2], viewport: Rect, params: &RenderParams) -> Pos2 {
    Pos2::new(
        world[0] * params.zoom + params.pan.x + viewport.center().x,
        world[1] * params.zoom + params.pan.y + viewport.center().y,
    )
}

fn draw_dashed_segment_pattern(
    painter: &Painter,
    start: Pos2,
    end: Pos2,
    stroke: Stroke,
    dash: f32,
    gap: f32,
) {
    let delta = end - start;
    let length = delta.length();
    if length <= f32::EPSILON {
        return;
    }
    let direction = delta / length;
    let mut offset = 0.0_f32;
    while offset < length {
        let dash_end = (offset + dash).min(length);
        painter.line_segment(
            [start + direction * offset, start + direction * dash_end],
            stroke,
        );
        offset += dash + gap;
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_overlay_legend(
    painter: &Painter,
    viewport: Rect,
    gfa: &GfaGraph,
    selected_path: Option<usize>,
    selected_walk: Option<usize>,
    show_containments: bool,
    path_color: Color32,
    walk_color: Color32,
    containment_color: Color32,
    params: &RenderParams,
) {
    let mut entries: Vec<(Color32, String)> = Vec::new();
    if let Some(index) = selected_path
        && let Some(path) = gfa.paths.get(index)
    {
        entries.push((
            path_color,
            format!("Path: {}", truncate_overlay_label(&path.name, 38)),
        ));
    }
    if let Some(index) = selected_walk
        && let Some(walk) = gfa.walks.get(index)
    {
        entries.push((
            walk_color,
            format!(
                "Walk: {} / h{} / {}",
                truncate_overlay_label(&walk.sample_id, 18),
                walk.haplotype_index,
                truncate_overlay_label(&walk.sequence_id, 18)
            ),
        ));
    }
    if show_containments && !gfa.containments.is_empty() {
        entries.push((
            containment_color,
            format!("Containments: {}", gfa.containments.len()),
        ));
    }
    if entries.is_empty() {
        return;
    }

    let line_height = 17.0_f32;
    let origin =
        viewport.left_bottom() + Vec2::new(10.0, -(10.0 + line_height * entries.len() as f32));
    for (row, (color, label)) in entries.into_iter().enumerate() {
        let y = origin.y + row as f32 * line_height;
        painter.line_segment(
            [
                Pos2::new(origin.x, y + 7.0),
                Pos2::new(origin.x + 18.0, y + 7.0),
            ],
            Stroke::new(3.0, color),
        );
        painter.text(
            Pos2::new(origin.x + 25.0, y),
            egui::Align2::LEFT_TOP,
            label,
            FontId::proportional(11.0),
            params.canvas_foreground,
        );
    }
}

fn truncate_overlay_label(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

/// Hit-test: returns the node index under the given screen point.
pub fn hit_test_node(
    screen: Pos2,
    viewport: Rect,
    graph: &ViewGraph,
    layout: &Layout,
    params: &RenderParams,
) -> Option<usize> {
    let transform = |world: [f32; 2]| -> Pos2 {
        Pos2::new(
            world[0] * params.zoom + params.pan.x + viewport.center().x,
            world[1] * params.zoom + params.pan.y + viewport.center().y,
        )
    };

    let half_h = (8.0 * params.node_scale * params.zoom.sqrt()).clamp(1.5, 40.0) + 4.0;
    let num_nodes = layout.num_nodes();
    let mut best: Option<(usize, f32)> = None;

    for (ni, _node) in graph.nodes.iter().enumerate() {
        if ni >= num_nodes {
            continue;
        }

        let pts = layout.pts(ni);
        // Check distance from click to every segment of the polyline chain.
        let mut min_dist = f32::MAX;
        for pair in pts.windows(2) {
            let a = transform(pair[0]);
            let b = transform(pair[1]);
            let d = distance_to_segment(screen, a, b);
            if d < min_dist {
                min_dist = d;
            }
        }
        if pts.len() == 1 {
            min_dist = transform(pts[0]).distance(screen);
        }
        if min_dist <= half_h && best.is_none_or(|(_, d)| min_dist < d) {
            best = Some((ni, min_dist));
        }
    }
    best.map(|(ni, _)| ni)
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Draws a small arrowhead at `tip` pointing from `penult` → `tip`.
fn draw_arrow_tip(painter: &Painter, penult: Pos2, tip: Pos2, half_h: f32, color: Color32) {
    let dir = tip - penult;
    let len = dir.length();
    if len < 1.0 {
        return;
    }
    let norm = dir / len;
    let perp = Vec2::new(-norm.y, norm.x);
    let arrow_len = (half_h * 1.6).min(len * 0.8);
    let arrow_w = half_h * 0.9;
    let base = tip - norm * arrow_len;
    painter.add(egui::Shape::convex_polygon(
        vec![tip, base + perp * arrow_w, base - perp * arrow_w],
        color,
        Stroke::NONE,
    ));
}

/// Shortest distance from point `p` to line segment `[a, b]`.
fn distance_to_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let ab_len_sq = ab.length_sq();
    if ab_len_sq < 1e-6 {
        return a.distance(p);
    }
    let t = ((p - a).dot(ab) / ab_len_sq).clamp(0.0, 1.0);
    (a + ab * t).distance(p)
}

fn node_color(node: &NodeInfo, params: &RenderParams) -> Color32 {
    match params.color_mode {
        ColorMode::Depth => {
            let d = node.depth.unwrap_or(0.0) as f32;
            let min_val = params.min_depth_color.max(0.1);
            let max_val = params.max_depth_color.max(min_val + 1.0);
            let val = d.max(min_val);
            let t = ((val.ln() - min_val.ln()) / (max_val.ln() - min_val.ln())).clamp(0.0, 1.0);
            depth_colormap(t)
        }
        ColorMode::Length => {
            let min_val = params.min_length_color.max(1.0);
            let max_val = params.max_length_color.max(min_val + 1.0);
            let val = (node.length as f32).max(min_val);
            let t = if max_val > min_val {
                ((val.log2() - min_val.log2()) / (max_val.log2() - min_val.log2())).clamp(0.0, 1.0)
            } else {
                0.5
            };
            length_colormap(t)
        }
        ColorMode::Uniform => Color32::from_rgb(70, 130, 200),
        ColorMode::ReadCount => {
            let rc = node.read_count.unwrap_or(0) as f32;
            let min_val = params.min_depth_color.max(0.1);
            let max_val = params.max_depth_color.max(min_val + 1.0);
            let val = rc.max(min_val);
            let t = ((val.ln() - min_val.ln()) / (max_val.ln() - min_val.ln())).clamp(0.0, 1.0);
            depth_colormap(t)
        }
    }
}

/// Shared by on-screen rendering and publication figure export.
pub fn color_for_node(node: &NodeInfo, params: &RenderParams) -> Color32 {
    node_color(node, params)
}

fn depth_colormap(t: f32) -> Color32 {
    // Blue → Cyan → Green → Yellow → Red
    let (r, g, b) = if t < 0.25 {
        let s = t * 4.0;
        (0.0, s, 1.0)
    } else if t < 0.5 {
        let s = (t - 0.25) * 4.0;
        (0.0, 1.0, 1.0 - s)
    } else if t < 0.75 {
        let s = (t - 0.5) * 4.0;
        (s, 1.0, 0.0)
    } else {
        let s = (t - 0.75) * 4.0;
        (1.0, 1.0 - s, 0.0)
    };
    Color32::from_rgb((r * 220.0) as u8, (g * 220.0) as u8, (b * 220.0) as u8)
}

fn length_colormap(t: f32) -> Color32 {
    let r = (t * 200.0) as u8;
    let b = ((1.0 - t) * 200.0) as u8;
    Color32::from_rgb(r, 80, b)
}

pub fn depth_color_for_legend(t: f32) -> Color32 {
    depth_colormap(t)
}

pub fn format_bp(bp: usize) -> String {
    if bp >= 1_000_000 {
        format!("{:.1} Mbp", bp as f64 / 1_000_000.0)
    } else if bp >= 1_000 {
        format!("{:.1} kbp", bp as f64 / 1_000.0)
    } else {
        format!("{} bp", bp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn graph(n: usize, visual_len: f32) -> ViewGraph {
        ViewGraph {
            nodes: (0..n)
                .map(|i| NodeInfo {
                    seg_idx: i,
                    name: Arc::from("test"),
                    length: 1000,
                    depth: None,
                    read_count: None,
                    visual_len,
                })
                .collect(),
            edges: Vec::new(),
            seg_to_node: Default::default(),
            components: Vec::new(),
        }
    }

    fn paint(graph: &ViewGraph, layout: &Layout, zoom: f32) -> egui::FullOutput {
        paint_with_cache(graph, layout, zoom, None)
    }

    fn paint_with_cache(
        graph: &ViewGraph,
        layout: &Layout,
        zoom: f32,
        cache: Option<&RenderCache>,
    ) -> egui::FullOutput {
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let painter = ui.ctx().layer_painter(egui::LayerId::new(
                egui::Order::Middle,
                egui::Id::new("test"),
            ));
            let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::splat(200.0));
            let selection = Selection::default();
            let params = RenderParams {
                zoom,
                ..Default::default()
            };
            if let Some(cache) = cache {
                draw_graph_cached(
                    &painter, viewport, graph, layout, &selection, &params, cache,
                );
            } else {
                draw_graph(&painter, viewport, graph, layout, &selection, &params);
            }
        });

        // egui 0.36 requires texture deltas (typically the test font atlas)
        // to be consumed or explicitly cleared before FullOutput is dropped.
        output.textures_delta.clear();
        output
    }

    #[test]
    fn long_contig_keeps_geometry_at_extreme_zoom() {
        let graph = graph(1, 1_000_000.0);
        let layout = Layout::new_with_graph(&graph);
        let output = paint(&graph, &layout, 0.00005);
        assert!(output.shapes.iter().any(|s| match &s.shape {
            egui::Shape::Path(path) => path.points.last().unwrap().distance(path.points[0]) > 40.0,
            _ => false,
        }));
    }

    #[test]
    fn overlapping_overview_dots_are_bounded() {
        let graph = graph(1000, 1.0);
        let mut layout = Layout::new_with_graph(&graph);
        layout.positions.fill([0.0, 0.0]);
        let output = paint(&graph, &layout, 0.01);
        assert!(output.shapes.len() < 10);
    }

    #[test]
    fn overview_edges_keep_longest_and_spatially_distinct_links() {
        let endpoints = [
            ([45.0, 50.0], [55.0, 50.0]),
            ([40.0, 50.0], [60.0, 50.0]),
            ([45.0, 60.0], [55.0, 60.0]),
            ([50.0, 45.0], [50.0, 55.0]),
        ];
        assert_eq!(
            overview_edge_representatives(&endpoints, [0.0, 0.0], [100.0, 100.0]),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn cached_canvas_matches_uncached_geometry_at_detail_zoom() {
        let mut graph = graph(3, 100.0);
        graph.edges.push(crate::graph::EdgeInfo {
            from: 0,
            from_strand: Strand::Forward,
            to: 1,
            to_strand: Strand::Forward,
            kind: EdgeKind::Link,
        });
        let layout = Layout::new_with_graph(&graph);
        let cache = RenderCache::new(&graph, &layout);
        let uncached = paint(&graph, &layout, 1.0);
        let cached = paint_with_cache(&graph, &layout, 1.0, Some(&cache));
        assert_eq!(cached.shapes, uncached.shapes);
    }
}
