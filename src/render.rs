//! Rendering: draws the assembly graph onto an egui canvas.
//!
//! Level-of-detail (LOD) strategy:
//!  - zoom < 0.3: draw dots only (no labels, simplified edges).
//!  - zoom 0.3–1.0: draw rectangles with colour; edges as lines.
//!  - zoom > 1.0: full detail with labels.

use egui::{Color32, FontId, Painter, Pos2, Rect, Stroke, Vec2};

use crate::filter::ColorMode;
use crate::gfa::Strand;
use crate::graph::{NodeInfo, ViewGraph};
use crate::layout::Layout;
use crate::selection::Selection;

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

pub fn draw_graph(
    painter: &Painter,
    viewport: Rect,
    graph: &ViewGraph,
    layout: &Layout,
    selection: &Selection,
    params: &RenderParams,
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

    // --- Draw edges first (under nodes) ---
    let edge_alpha = (params.edge_opacity * 255.0) as u8;
    if lod > params.edge_visible_min_zoom {
        for edge in &graph.edges {
            if edge.from >= num_nodes || edge.to >= num_nodes {
                continue;
            }

            // Connect the correct endpoint of each segment based on strand:
            //   Forward exits from last physics node (end),
            //   Reverse exits from first physics node (start).
            let p0_world = match edge.from_strand {
                Strand::Forward => layout.end(edge.from),
                Strand::Reverse => layout.start(edge.from),
            };
            let p1_world = match edge.to_strand {
                Strand::Forward => layout.start(edge.to),
                Strand::Reverse => layout.end(edge.to),
            };

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
            painter.line_segment([p0, p1], Stroke::new(edge_w, color));
        }
    }

    // Merge indistinguishable overview dots into screen cells.
    let mut occupied_dots = ahash::AHashSet::default();
    // --- Draw nodes as thick polylines through all physics-node chain points ---
    for (ni, node) in graph.nodes.iter().enumerate() {
        if ni >= num_nodes {
            continue;
        }

        let pts = layout.pts(ni);
        if pts.is_empty() {
            continue;
        }

        // Viewport culling: bounding box of all physics points.
        let mut bmin = [f32::MAX; 2];
        let mut bmax = [f32::MIN; 2];
        for &p in pts {
            bmin[0] = bmin[0].min(p[0]);
            bmin[1] = bmin[1].min(p[1]);
            bmax[0] = bmax[0].max(p[0]);
            bmax[1] = bmax[1].max(p[1]);
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
            let cell = ((sc.x / 3.0).floor() as i32, (sc.y / 3.0).floor() as i32);
            if is_selected || occupied_dots.insert(cell) {
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
                        &format_bp(node.length),
                        FontId::monospace(7.0),
                        params.canvas_foreground,
                    );
                }
            }
        }
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
        if min_dist <= half_h {
            if best.map_or(true, |(_, d)| min_dist < d) {
                best = Some((ni, min_dist));
            }
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
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let painter = ui.ctx().layer_painter(egui::LayerId::new(
                egui::Order::Middle,
                egui::Id::new("test"),
            ));
            draw_graph(
                &painter,
                Rect::from_min_size(Pos2::ZERO, Vec2::splat(200.0)),
                graph,
                layout,
                &Selection::default(),
                &RenderParams {
                    zoom,
                    ..Default::default()
                },
            );
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
}
