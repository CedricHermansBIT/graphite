//! Export functions: FASTA sequences, CSV stats, etc.

use std::{fs, io::Write, path::Path};
use anyhow::Result;
use egui::Color32;
use image::{ImageBuffer, Rgba};

use crate::gfa::{GfaGraph, GfaVersion};
use crate::graph::{EdgeKind, ViewGraph};
use crate::layout::Layout;
use crate::render::{color_for_node, RenderParams};
use crate::selection::Selection;

/// Export selected node sequences to a FASTA file.
pub fn export_fasta(
    path: &Path,
    gfa: &GfaGraph,
    graph: &ViewGraph,
    selection: &Selection,
) -> Result<()> {
    let mut f = fs::File::create(path)?;
    for &ni in &selection.nodes {
        if ni >= graph.nodes.len() { continue; }
        let node = &graph.nodes[ni];
        let seg = &gfa.segments[node.seg_idx];
        let seq = seg.sequence(&gfa.mmap);
        if seq.is_empty() {
            continue;
        }
        writeln!(f, ">{} len={}", node.name, node.length)?;
        // Write in 80-char lines.
        for chunk in seq.chunks(80) {
            f.write_all(chunk)?;
            writeln!(f)?;
        }
    }
    Ok(())
}

/// Export statistics for all (or selected) segments to CSV.
pub fn export_csv(
    path: &Path,
    gfa: &GfaGraph,
    graph: &ViewGraph,
    selection: &Selection,
) -> Result<()> {
    let mut f = fs::File::create(path)?;
    writeln!(f, "name,length,depth,read_count")?;
    let indices: Box<dyn Iterator<Item = usize>> = if selection.is_empty() {
        Box::new(0..graph.nodes.len())
    } else {
        let mut v: Vec<usize> = selection.nodes.iter().cloned().collect();
        v.sort_unstable();
        Box::new(v.into_iter())
    };
    for ni in indices {
        if ni >= graph.nodes.len() { continue; }
        let node = &graph.nodes[ni];
        let seg = &gfa.segments[node.seg_idx];
        writeln!(
            f,
            "{},{},{},{}",
            node.name,
            node.length,
            seg.depth.map_or("".to_string(), |d| format!("{:.2}", d)),
            seg.read_count.map_or("".to_string(), |r| r.to_string()),
        )?;
    }
    Ok(())
}

/// Copy all selected embedded sequences as FASTA records to the clipboard.
pub fn copy_sequence_to_clipboard(
    gfa: &GfaGraph,
    graph: &ViewGraph,
    selection: &Selection,
) -> Option<String> {
    let mut nodes: Vec<_> = selection.nodes.iter().copied().collect();
    nodes.sort_unstable();
    let mut fasta = String::new();
    for ni in nodes {
        let Some(node) = graph.nodes.get(ni) else {
            continue;
        };
        let Some(seg) = gfa.segments.get(node.seg_idx) else {
            continue;
        };
        let sequence = seg.sequence(&gfa.mmap);
        if sequence.is_empty() {
            continue;
        }
        fasta.push('>');
        fasta.push_str(&node.name);
        fasta.push('\n');
        fasta.push_str(&String::from_utf8_lossy(sequence));
        fasta.push('\n');
    }
    (!fasta.is_empty()).then_some(fasta)
}

/// Export the current graph view as a self-contained SVG figure.
///
/// This base variant is used by the benchmark runner and intentionally omits
/// interactive metadata overlays.
pub fn export_svg(
    path: &Path,
    graph: &ViewGraph,
    layout: &Layout,
    params: &RenderParams,
) -> Result<()> {
    export_svg_impl(path, None, graph, layout, params, None, None, false)
}

/// Export the current interactive view, including active GFA metadata overlays.
pub fn export_svg_with_overlays(
    path: &Path,
    gfa: &GfaGraph,
    graph: &ViewGraph,
    layout: &Layout,
    params: &RenderParams,
    selected_path: Option<usize>,
    selected_walk: Option<usize>,
    show_containments: bool,
) -> Result<()> {
    export_svg_impl(
        path,
        Some(gfa),
        graph,
        layout,
        params,
        selected_path,
        selected_walk,
        show_containments,
    )
}

#[allow(clippy::too_many_arguments)]
fn export_svg_impl(
    path: &Path,
    gfa: Option<&GfaGraph>,
    graph: &ViewGraph,
    layout: &Layout,
    params: &RenderParams,
    selected_path: Option<usize>,
    selected_walk: Option<usize>,
    show_containments: bool,
) -> Result<()> {
    let figure = FigureTransform::new(layout, 2400.0, 1600.0)?;
    let mut output = String::new();
    output.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\">\n",
        figure.width, figure.height, figure.width, figure.height
    ));
    output.push_str(&format!(
        "<rect width=\"100%\" height=\"100%\" fill=\"{}\"/>\n",
        svg_color(params.canvas_background, 255)
    ));

    for edge in &graph.edges {
        if edge.from >= layout.num_nodes() || edge.to >= layout.num_nodes() {
            continue;
        }
        let a = figure.point(match edge.from_strand {
            crate::gfa::Strand::Forward => layout.end(edge.from),
            crate::gfa::Strand::Reverse => layout.start(edge.from),
        });
        let b = figure.point(match edge.to_strand {
            crate::gfa::Strand::Forward => layout.start(edge.to),
            crate::gfa::Strand::Reverse => layout.end(edge.to),
        });
        let dash = if matches!(edge.kind, EdgeKind::Jump { .. }) {
            " stroke-dasharray=\"8 6\""
        } else {
            ""
        };
        output.push_str(&format!(
            "<line x1=\"{:.2}\" y1=\"{:.2}\" x2=\"{:.2}\" y2=\"{:.2}\" stroke=\"{}\" stroke-opacity=\"{:.3}\" stroke-width=\"1.2\"{dash}/>\n",
            a.0, a.1, b.0, b.1, svg_color(params.canvas_foreground, 255), params.edge_opacity
        ));
    }

    for (index, node) in graph.nodes.iter().enumerate() {
        if index >= layout.num_nodes() {
            continue;
        }
        let points = layout.pts(index);
        let color = color_for_node(node, params);
        output.push_str("<polyline fill=\"none\" stroke-linecap=\"round\" stroke-linejoin=\"round\"");
        output.push_str(&format!(
            " stroke=\"{}\" stroke-width=\"5\" points=\"",
            svg_color(color, 255)
        ));
        for &point in points {
            let (x, y) = figure.point(point);
            output.push_str(&format!("{x:.2},{y:.2} "));
        }
        output.push_str("\"/>\n");
    }

    if let Some(gfa) = gfa {
        append_svg_overlays(
            &mut output,
            &figure,
            gfa,
            graph,
            layout,
            selected_path,
            selected_walk,
            show_containments,
        );
    }

    if params.show_labels && graph.nodes.len() <= 2_000 {
        for (index, node) in graph.nodes.iter().enumerate() {
            if index >= layout.num_nodes() {
                continue;
            }
            let (x, y) = figure.point(layout.center(index));
            output.push_str(&format!(
                "<text x=\"{x:.2}\" y=\"{y:.2}\" text-anchor=\"middle\" font-family=\"sans-serif\" font-size=\"11\" fill=\"{}\">{}</text>\n",
                svg_color(params.canvas_foreground, 255),
                escape_xml(&node.name)
            ));
        }
    }
    output.push_str("</svg>\n");
    fs::write(path, output)?;
    Ok(())
}

/// Export the current graph view as a PNG figure.
///
/// This base variant is used by the benchmark runner and omits interactive
/// metadata overlays.
pub fn export_png(
    path: &Path,
    graph: &ViewGraph,
    layout: &Layout,
    params: &RenderParams,
) -> Result<()> {
    export_png_impl(path, None, graph, layout, params, None, None, false)
}

/// Export the current interactive view, including active GFA metadata overlays.
pub fn export_png_with_overlays(
    path: &Path,
    gfa: &GfaGraph,
    graph: &ViewGraph,
    layout: &Layout,
    params: &RenderParams,
    selected_path: Option<usize>,
    selected_walk: Option<usize>,
    show_containments: bool,
) -> Result<()> {
    export_png_impl(
        path,
        Some(gfa),
        graph,
        layout,
        params,
        selected_path,
        selected_walk,
        show_containments,
    )
}

#[allow(clippy::too_many_arguments)]
fn export_png_impl(
    path: &Path,
    gfa: Option<&GfaGraph>,
    graph: &ViewGraph,
    layout: &Layout,
    params: &RenderParams,
    selected_path: Option<usize>,
    selected_walk: Option<usize>,
    show_containments: bool,
) -> Result<()> {
    let figure = FigureTransform::new(layout, 2400.0, 1600.0)?;
    let mut image = ImageBuffer::from_pixel(
        figure.width as u32,
        figure.height as u32,
        Rgba([
            params.canvas_background.r(),
            params.canvas_background.g(),
            params.canvas_background.b(),
            255,
        ]),
    );
    let edge_alpha = (params.edge_opacity * 255.0) as u8;

    for edge in &graph.edges {
        if edge.from >= layout.num_nodes() || edge.to >= layout.num_nodes() {
            continue;
        }
        let a = figure.point(match edge.from_strand {
            crate::gfa::Strand::Forward => layout.end(edge.from),
            crate::gfa::Strand::Reverse => layout.start(edge.from),
        });
        let b = figure.point(match edge.to_strand {
            crate::gfa::Strand::Forward => layout.start(edge.to),
            crate::gfa::Strand::Reverse => layout.end(edge.to),
        });
        if matches!(edge.kind, EdgeKind::Jump { .. }) {
            draw_dashed_line(
                &mut image,
                a,
                b,
                params.canvas_foreground,
                edge_alpha,
                1,
            );
        } else {
            draw_line(
                &mut image,
                a,
                b,
                params.canvas_foreground,
                edge_alpha,
                1,
            );
        }
    }

    for (index, node) in graph.nodes.iter().enumerate() {
        if index >= layout.num_nodes() {
            continue;
        }
        let points = layout.pts(index);
        let color = color_for_node(node, params);
        for pair in points.windows(2) {
            draw_line(
                &mut image,
                figure.point(pair[0]),
                figure.point(pair[1]),
                color,
                255,
                5,
            );
        }
    }

    if let Some(gfa) = gfa {
        draw_png_overlays(
            &mut image,
            &figure,
            gfa,
            graph,
            layout,
            selected_path,
            selected_walk,
            show_containments,
        );
    }

    image.save(path)?;
    Ok(())
}

fn append_svg_overlays(
    output: &mut String,
    figure: &FigureTransform,
    gfa: &GfaGraph,
    graph: &ViewGraph,
    layout: &Layout,
    selected_path: Option<usize>,
    selected_walk: Option<usize>,
    show_containments: bool,
) {
    const PATH: &str = "#f5aa37";
    const WALK: &str = "#41cddc";
    const CONTAINMENT: &str = "#be7deb";

    if show_containments {
        for containment in &gfa.containments {
            let (Some(&container), Some(&contained)) = (
                graph.seg_to_node.get(&containment.container),
                graph.seg_to_node.get(&containment.contained),
            ) else {
                continue;
            };
            if container >= layout.num_nodes() || contained >= layout.num_nodes() {
                continue;
            }
            let container_len = gfa
                .segments
                .get(containment.container)
                .map_or(1, |segment| segment.length.max(1));
            let fraction =
                (containment.position as f32 / container_len as f32).clamp(0.0, 1.0);
            let a = figure.point(layout.point_at_fraction(container, fraction));
            let b = figure.point(oriented_entry(
                layout,
                contained,
                containment.contained_strand,
            ));
            output.push_str(&format!(
                "<line x1=\"{:.2}\" y1=\"{:.2}\" x2=\"{:.2}\" y2=\"{:.2}\" stroke=\"{CONTAINMENT}\" stroke-width=\"2\" stroke-dasharray=\"2 5\"/>\n",
                a.0, a.1, b.0, b.1
            ));
            output.push_str(&format!(
                "<circle cx=\"{:.2}\" cy=\"{:.2}\" r=\"3\" fill=\"{CONTAINMENT}\"/>\n",
                a.0, a.1
            ));
        }
    }

    if let Some(index) = selected_path {
        if let Some(path) = gfa.paths.get(index) {
            let steps = gfa.path_steps(path);
            append_svg_step_polylines(output, figure, graph, layout, steps.iter().map(|s| (s.segment, s.strand)), PATH, 6.0);
            for i in 0..steps.len().saturating_sub(1) {
                let from = steps[i];
                let to = steps[i + 1];
                let (Some(&from_node), Some(&to_node)) = (
                    graph.seg_to_node.get(&from.segment),
                    graph.seg_to_node.get(&to.segment),
                ) else {
                    continue;
                };
                let a = figure.point(oriented_exit(layout, from_node, from.strand));
                let b = figure.point(oriented_entry(layout, to_node, to.strand));
                let dash = if matches!(
                    from.connection_to_next,
                    Some(crate::gfa::PathConnection::Jump)
                ) {
                    " stroke-dasharray=\"8 6\""
                } else {
                    ""
                };
                output.push_str(&format!(
                    "<line x1=\"{:.2}\" y1=\"{:.2}\" x2=\"{:.2}\" y2=\"{:.2}\" stroke=\"{PATH}\" stroke-width=\"4\"{dash}/>\n",
                    a.0, a.1, b.0, b.1
                ));
            }
        }
    }

    if let Some(index) = selected_walk {
        if let Some(walk) = gfa.walks.get(index) {
            let steps = gfa.walk_steps(walk);
            append_svg_step_polylines(output, figure, graph, layout, steps.iter().map(|s| (s.segment, s.strand)), WALK, 5.0);
            for pair in steps.windows(2) {
                let (Some(&from_node), Some(&to_node)) = (
                    graph.seg_to_node.get(&pair[0].segment),
                    graph.seg_to_node.get(&pair[1].segment),
                ) else {
                    continue;
                };
                let a = figure.point(oriented_exit(layout, from_node, pair[0].strand));
                let b = figure.point(oriented_entry(layout, to_node, pair[1].strand));
                output.push_str(&format!(
                    "<line x1=\"{:.2}\" y1=\"{:.2}\" x2=\"{:.2}\" y2=\"{:.2}\" stroke=\"{WALK}\" stroke-width=\"3\"/>\n",
                    a.0, a.1, b.0, b.1
                ));
            }
        }
    }
}

fn append_svg_step_polylines<I>(
    output: &mut String,
    figure: &FigureTransform,
    graph: &ViewGraph,
    layout: &Layout,
    steps: I,
    color: &str,
    width: f32,
) where
    I: Iterator<Item = (usize, crate::gfa::Strand)>,
{
    for (segment, strand) in steps {
        let Some(&node) = graph.seg_to_node.get(&segment) else {
            continue;
        };
        if node >= layout.num_nodes() {
            continue;
        }
        output.push_str(&format!(
            "<polyline fill=\"none\" stroke=\"{color}\" stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"{width:.1}\" points=\""
        ));
        match strand {
            crate::gfa::Strand::Forward => {
                for &point in layout.pts(node) {
                    let (x, y) = figure.point(point);
                    output.push_str(&format!("{x:.2},{y:.2} "));
                }
            }
            crate::gfa::Strand::Reverse => {
                for &point in layout.pts(node).iter().rev() {
                    let (x, y) = figure.point(point);
                    output.push_str(&format!("{x:.2},{y:.2} "));
                }
            }
        }
        output.push_str("\"/>\n");
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_png_overlays(
    image: &mut ImageBuffer<Rgba<u8>, Vec<u8>>,
    figure: &FigureTransform,
    gfa: &GfaGraph,
    graph: &ViewGraph,
    layout: &Layout,
    selected_path: Option<usize>,
    selected_walk: Option<usize>,
    show_containments: bool,
) {
    let path_color = Color32::from_rgb(245, 170, 55);
    let walk_color = Color32::from_rgb(65, 205, 220);
    let containment_color = Color32::from_rgb(190, 125, 235);

    if show_containments {
        for containment in &gfa.containments {
            let (Some(&container), Some(&contained)) = (
                graph.seg_to_node.get(&containment.container),
                graph.seg_to_node.get(&containment.contained),
            ) else {
                continue;
            };
            if container >= layout.num_nodes() || contained >= layout.num_nodes() {
                continue;
            }
            let container_len = gfa
                .segments
                .get(containment.container)
                .map_or(1, |segment| segment.length.max(1));
            let fraction =
                (containment.position as f32 / container_len as f32).clamp(0.0, 1.0);
            let a = figure.point(layout.point_at_fraction(container, fraction));
            let b = figure.point(oriented_entry(
                layout,
                contained,
                containment.contained_strand,
            ));
            draw_dashed_line(image, a, b, containment_color, 230, 2);
        }
    }

    if let Some(index) = selected_path {
        if let Some(path) = gfa.paths.get(index) {
            let steps = gfa.path_steps(path);
            draw_png_step_polylines(
                image,
                figure,
                graph,
                layout,
                steps.iter().map(|s| (s.segment, s.strand)),
                path_color,
                6,
            );
            for i in 0..steps.len().saturating_sub(1) {
                let from = steps[i];
                let to = steps[i + 1];
                let (Some(&from_node), Some(&to_node)) = (
                    graph.seg_to_node.get(&from.segment),
                    graph.seg_to_node.get(&to.segment),
                ) else {
                    continue;
                };
                let a = figure.point(oriented_exit(layout, from_node, from.strand));
                let b = figure.point(oriented_entry(layout, to_node, to.strand));
                if matches!(
                    from.connection_to_next,
                    Some(crate::gfa::PathConnection::Jump)
                ) {
                    draw_dashed_line(image, a, b, path_color, 240, 4);
                } else {
                    draw_line(image, a, b, path_color, 240, 4);
                }
            }
        }
    }

    if let Some(index) = selected_walk {
        if let Some(walk) = gfa.walks.get(index) {
            let steps = gfa.walk_steps(walk);
            draw_png_step_polylines(
                image,
                figure,
                graph,
                layout,
                steps.iter().map(|s| (s.segment, s.strand)),
                walk_color,
                5,
            );
            for pair in steps.windows(2) {
                let (Some(&from_node), Some(&to_node)) = (
                    graph.seg_to_node.get(&pair[0].segment),
                    graph.seg_to_node.get(&pair[1].segment),
                ) else {
                    continue;
                };
                let a = figure.point(oriented_exit(layout, from_node, pair[0].strand));
                let b = figure.point(oriented_entry(layout, to_node, pair[1].strand));
                draw_line(image, a, b, walk_color, 225, 3);
            }
        }
    }
}

fn draw_png_step_polylines<I>(
    image: &mut ImageBuffer<Rgba<u8>, Vec<u8>>,
    figure: &FigureTransform,
    graph: &ViewGraph,
    layout: &Layout,
    steps: I,
    color: Color32,
    thickness: i32,
) where
    I: Iterator<Item = (usize, crate::gfa::Strand)>,
{
    for (segment, strand) in steps {
        let Some(&node) = graph.seg_to_node.get(&segment) else {
            continue;
        };
        if node >= layout.num_nodes() {
            continue;
        }
        let points = layout.pts(node);
        match strand {
            crate::gfa::Strand::Forward => {
                for pair in points.windows(2) {
                    draw_line(
                        image,
                        figure.point(pair[0]),
                        figure.point(pair[1]),
                        color,
                        230,
                        thickness,
                    );
                }
            }
            crate::gfa::Strand::Reverse => {
                for pair in points.windows(2).rev() {
                    draw_line(
                        image,
                        figure.point(pair[1]),
                        figure.point(pair[0]),
                        color,
                        230,
                        thickness,
                    );
                }
            }
        }
    }
}

#[inline]
fn oriented_entry(
    layout: &Layout,
    node: usize,
    strand: crate::gfa::Strand,
) -> [f32; 2] {
    match strand {
        crate::gfa::Strand::Forward => layout.start(node),
        crate::gfa::Strand::Reverse => layout.end(node),
    }
}

#[inline]
fn oriented_exit(
    layout: &Layout,
    node: usize,
    strand: crate::gfa::Strand,
) -> [f32; 2] {
    match strand {
        crate::gfa::Strand::Forward => layout.end(node),
        crate::gfa::Strand::Reverse => layout.start(node),
    }
}

struct FigureTransform {
    min_x: f32,
    min_y: f32,
    scale: f32,
    width: f32,
    height: f32,
    margin: f32,
}

impl FigureTransform {
    fn new(layout: &Layout, width: f32, height: f32) -> Result<Self> {
        if layout.positions.is_empty() {
            anyhow::bail!("The graph has no layout positions to export");
        }
        let mut min_x = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        for &[x, y] in &layout.positions {
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
        let margin = 72.0;
        let scale = ((width - margin * 2.0) / (max_x - min_x).max(1.0))
            .min((height - margin * 2.0) / (max_y - min_y).max(1.0));
        Ok(Self { min_x, min_y, scale, width, height, margin })
    }

    fn point(&self, point: [f32; 2]) -> (f32, f32) {
        (
            self.margin + (point[0] - self.min_x) * self.scale,
            self.margin + (point[1] - self.min_y) * self.scale,
        )
    }
}

fn draw_dashed_line(
    image: &mut ImageBuffer<Rgba<u8>, Vec<u8>>,
    from: (f32, f32),
    to: (f32, f32),
    color: Color32,
    alpha: u8,
    thickness: i32,
) {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let length = (dx * dx + dy * dy).sqrt();
    if length <= f32::EPSILON {
        return;
    }
    let ux = dx / length;
    let uy = dy / length;
    let dash = 8.0_f32;
    let gap = 6.0_f32;
    let mut offset = 0.0_f32;
    while offset < length {
        let end = (offset + dash).min(length);
        draw_line(
            image,
            (from.0 + ux * offset, from.1 + uy * offset),
            (from.0 + ux * end, from.1 + uy * end),
            color,
            alpha,
            thickness,
        );
        offset += dash + gap;
    }
}

fn draw_line(
    image: &mut ImageBuffer<Rgba<u8>, Vec<u8>>,
    from: (f32, f32),
    to: (f32, f32),
    color: Color32,
    alpha: u8,
    thickness: i32,
) {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let steps = dx.abs().max(dy.abs()).ceil() as usize;
    for step in 0..=steps.max(1) {
        let t = step as f32 / steps.max(1) as f32;
        let x = (from.0 + dx * t).round() as i32;
        let y = (from.1 + dy * t).round() as i32;
        for oy in -thickness / 2..=thickness / 2 {
            for ox in -thickness / 2..=thickness / 2 {
                let px = x + ox;
                let py = y + oy;
                if px < 0 || py < 0 || px >= image.width() as i32 || py >= image.height() as i32 {
                    continue;
                }
                let dst = image.get_pixel_mut(px as u32, py as u32);
                let blend = alpha as f32 / 255.0;
                dst.0[0] = (dst.0[0] as f32 * (1.0 - blend) + color.r() as f32 * blend) as u8;
                dst.0[1] = (dst.0[1] as f32 * (1.0 - blend) + color.g() as f32 * blend) as u8;
                dst.0[2] = (dst.0[2] as f32 * (1.0 - blend) + color.b() as f32 * blend) as u8;
            }
        }
    }
}

fn svg_color(color: Color32, alpha: u8) -> String {
    if alpha == 255 {
        format!("#{:02x}{:02x}{:02x}", color.r(), color.g(), color.b())
    } else {
        format!("rgba({}, {}, {}, {:.3})", color.r(), color.g(), color.b(), alpha as f32 / 255.0)
    }
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Compute basic assembly statistics.
pub struct AssemblyStats {
    pub gfa_version: GfaVersion,
    pub num_segments: usize,
    pub total_length: usize,
    pub num_links: usize,
    pub num_jumps: usize,
    pub num_containments: usize,
    pub num_paths: usize,
    pub num_walks: usize,
    pub n50: usize,
    pub l50: usize,
    pub max_length: usize,
    pub min_length: usize,
    pub mean_depth: f64,
}

impl AssemblyStats {
    pub fn compute(gfa: &GfaGraph) -> Self {
        let segs = &gfa.segments;
        let num_segments = segs.len();
        let num_links = gfa.links.len();
        let num_jumps = gfa.jumps.len();
        let num_containments = gfa.containments.len();
        let num_paths = gfa.paths.len();
        let num_walks = gfa.walks.len();
        let total_length: usize = segs.iter().map(|s| s.length).sum();
        let max_length = segs.iter().map(|s| s.length).max().unwrap_or(0);
        let min_length = segs.iter().map(|s| s.length).min().unwrap_or(0);

        let mut lengths: Vec<usize> = segs.iter().map(|s| s.length).collect();
        lengths.sort_unstable_by(|a, b| b.cmp(a));

        let (n50, l50) = {
            let half = total_length / 2;
            let mut cum = 0;
            let mut n50 = 0;
            let mut l50 = 0;
            for (i, &l) in lengths.iter().enumerate() {
                cum += l;
                if cum >= half && n50 == 0 {
                    n50 = l;
                    l50 = i + 1;
                }
            }
            (n50, l50)
        };

        let depth_vals: Vec<f64> = segs.iter().filter_map(|s| s.depth).collect();
        let mean_depth = if depth_vals.is_empty() {
            0.0
        } else {
            depth_vals.iter().sum::<f64>() / depth_vals.len() as f64
        };

        Self {
            gfa_version: gfa.version,
            num_segments,
            total_length,
            num_links,
            num_jumps,
            num_containments,
            num_paths,
            num_walks,
            n50,
            l50,
            max_length,
            min_length,
            mean_depth,
        }
    }
}
