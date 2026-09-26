//! Export functions: FASTA sequences, CSV stats, etc.

use ab_glyph::{Font, FontRef, ScaleFont};
use anyhow::Result;
use egui::Color32;
use image::{ImageBuffer, Rgba};
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

use crate::gfa::{GfaGraph, GfaVersion};
use crate::graph::{EdgeKind, ViewGraph};
use crate::layout::Layout;
use crate::render::{RenderParams, color_for_node};
use crate::selection::Selection;

/// Export selected node sequences to a FASTA file.
pub fn export_fasta(
    path: &Path,
    gfa: &GfaGraph,
    graph: &ViewGraph,
    selection: &Selection,
) -> Result<()> {
    let mut nodes: Vec<_> = selection.nodes.iter().copied().collect();
    nodes.sort_unstable();
    atomic_write(path, |file| {
        let mut writer = BufWriter::new(file);
        for ni in nodes {
            let Some(node) = graph.nodes.get(ni) else {
                continue;
            };
            let seg = &gfa.segments[node.seg_idx];
            let seq = seg.sequence(&gfa.mmap);
            if seq.is_empty() {
                continue;
            }
            writeln!(writer, ">{} len={}", node.name, node.length)?;
            for chunk in seq.chunks(80) {
                writer.write_all(chunk)?;
                writeln!(writer)?;
            }
        }
        writer.flush()?;
        Ok(())
    })
}

/// Export statistics for all (or selected) segments to CSV.
pub fn export_csv(
    path: &Path,
    gfa: &GfaGraph,
    graph: &ViewGraph,
    selection: &Selection,
) -> Result<()> {
    let mut indices: Vec<_> = if selection.is_empty() {
        (0..graph.nodes.len()).collect()
    } else {
        selection.nodes.iter().copied().collect()
    };
    indices.sort_unstable();
    atomic_write(path, |file| {
        let mut writer = csv::Writer::from_writer(file);
        writer.write_record(["name", "length", "depth", "read_count"])?;
        for ni in indices {
            let Some(node) = graph.nodes.get(ni) else {
                continue;
            };
            let seg = &gfa.segments[node.seg_idx];
            writer.write_record([
                node.name.to_string(),
                node.length.to_string(),
                seg.depth
                    .map_or_else(String::new, |depth| format!("{depth:.2}")),
                seg.read_count
                    .map_or_else(String::new, |count| count.to_string()),
            ])?;
        }
        writer.flush()?;
        Ok(())
    })
}

#[cfg(target_arch = "wasm32")]
pub fn generate_fasta(gfa: &GfaGraph, graph: &ViewGraph, selection: &Selection) -> Vec<u8> {
    let mut nodes: Vec<_> = selection.nodes.iter().copied().collect();
    nodes.sort_unstable();
    let mut output = Vec::new();
    for ni in nodes {
        let Some(node) = graph.nodes.get(ni) else {
            continue;
        };
        let seg = &gfa.segments[node.seg_idx];
        let seq = seg.sequence(&gfa.mmap);
        if seq.is_empty() {
            continue;
        }
        writeln!(output, ">{} len={}", node.name, node.length).unwrap();
        for chunk in seq.chunks(80) {
            output.extend_from_slice(chunk);
            output.push(b'\n');
        }
    }
    output
}

#[cfg(target_arch = "wasm32")]
pub fn generate_csv(gfa: &GfaGraph, graph: &ViewGraph, selection: &Selection) -> Result<Vec<u8>> {
    let mut indices: Vec<_> = if selection.is_empty() {
        (0..graph.nodes.len()).collect()
    } else {
        selection.nodes.iter().copied().collect()
    };
    indices.sort_unstable();
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record(["name", "length", "depth", "read_count"])?;
    for ni in indices {
        let Some(node) = graph.nodes.get(ni) else {
            continue;
        };
        let seg = &gfa.segments[node.seg_idx];
        writer.write_record([
            node.name.to_string(),
            node.length.to_string(),
            seg.depth.map_or_else(String::new, |d| format!("{d:.2}")),
            seg.read_count.map_or_else(String::new, |n| n.to_string()),
        ])?;
    }
    writer.flush()?;
    Ok(writer.into_inner()?)
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

/// Output size and optional world-space crop: [min_x, min_y, max_x, max_y].
#[derive(Clone, Debug)]
pub struct FigureOptions {
    pub width: u32,
    pub height: u32,
    pub world_bounds: Option<[f32; 4]>,
}

impl Default for FigureOptions {
    fn default() -> Self {
        Self {
            width: 2400,
            height: 1600,
            world_bounds: None,
        }
    }
}

impl FigureOptions {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (64..=16_384).contains(&self.width) && (64..=16_384).contains(&self.height),
            "Figure dimensions must be between 64 and 16384 pixels"
        );
        anyhow::ensure!(
            u64::from(self.width) * u64::from(self.height) <= 64_000_000,
            "Figure dimensions must not exceed 64 million pixels"
        );
        if let Some([min_x, min_y, max_x, max_y]) = self.world_bounds {
            anyhow::ensure!(
                [min_x, min_y, max_x, max_y].iter().all(|v| v.is_finite())
                    && max_x > min_x
                    && max_y > min_y
                    && (max_x - min_x).is_finite()
                    && (max_y - min_y).is_finite(),
                "Figure bounds must be finite and have positive width and height"
            );
        }
        Ok(())
    }
}

/// Write alongside the destination, then replace it only after a successful write.
/// In particular, an error cannot leave an existing export truncated.
fn atomic_write(path: &Path, write: impl FnOnce(&mut File) -> Result<()>) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    write(temporary.as_file_mut())?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
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
    export_svg_impl(
        path,
        None,
        graph,
        layout,
        params,
        None,
        None,
        false,
        &FigureOptions::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn export_svg_with_options(
    path: &Path,
    gfa: &GfaGraph,
    graph: &ViewGraph,
    layout: &Layout,
    params: &RenderParams,
    selected_path: Option<usize>,
    selected_walk: Option<usize>,
    show_containments: bool,
    options: &FigureOptions,
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
        options,
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
    options: &FigureOptions,
) -> Result<()> {
    let output = render_svg_impl(
        gfa,
        graph,
        layout,
        params,
        selected_path,
        selected_walk,
        show_containments,
        options,
    )?;
    atomic_write(path, |file| {
        file.write_all(output.as_bytes())?;
        Ok(())
    })
}

#[cfg(target_arch = "wasm32")]
#[allow(clippy::too_many_arguments)]
pub fn generate_svg_with_options(
    gfa: &GfaGraph,
    graph: &ViewGraph,
    layout: &Layout,
    params: &RenderParams,
    selected_path: Option<usize>,
    selected_walk: Option<usize>,
    show_containments: bool,
    options: &FigureOptions,
) -> Result<Vec<u8>> {
    Ok(render_svg_impl(
        Some(gfa),
        graph,
        layout,
        params,
        selected_path,
        selected_walk,
        show_containments,
        options,
    )?
    .into_bytes())
}

#[allow(clippy::too_many_arguments)]
fn render_svg_impl(
    gfa: Option<&GfaGraph>,
    graph: &ViewGraph,
    layout: &Layout,
    params: &RenderParams,
    selected_path: Option<usize>,
    selected_walk: Option<usize>,
    show_containments: bool,
    options: &FigureOptions,
) -> Result<String> {
    let figure = FigureTransform::new(layout, options)?;
    let mut output = String::new();
    output.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\">\n",
        figure.width, figure.height, figure.width, figure.height
    ));
    output.push_str(&format!(
        "<rect width=\"100%\" height=\"100%\" fill=\"{}\"/>\n",
        svg_color(params.canvas_background, 255)
    ));

    output.push_str(&format!(
        "<defs><clipPath id=\"figure-crop\"><rect x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\"/></clipPath></defs>\n<g clip-path=\"url(#figure-crop)\">\n",
        figure.clip[0], figure.clip[1], figure.clip[2] - figure.clip[0], figure.clip[3] - figure.clip[1]
    ));
    let node_width = node_width(params);

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
        output
            .push_str("<polyline fill=\"none\" stroke-linecap=\"round\" stroke-linejoin=\"round\"");
        output.push_str(&format!(
            " stroke=\"{}\" stroke-width=\"{node_width:.1}\" points=\"",
            svg_color(color, 255)
        ));
        for &point in points {
            let (x, y) = figure.point(point);
            output.push_str(&format!("{x:.2},{y:.2} "));
        }
        output.push_str("\"/>\n");
        if points.len() >= 2 {
            append_svg_arrow(
                &mut output,
                figure.point(points[points.len() - 2]),
                figure.point(points[points.len() - 1]),
                node_width,
                &svg_color(color, 255),
            );
        } else if let Some(&point) = points.first() {
            let (x, y) = figure.point(point);
            output.push_str(&format!(
                "<circle cx=\"{x:.2}\" cy=\"{y:.2}\" r=\"{:.2}\" fill=\"{}\"/>\n",
                node_width * 0.5,
                svg_color(color, 255)
            ));
        }
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

    if params.show_labels {
        for (index, node) in graph.nodes.iter().enumerate() {
            if index >= layout.num_nodes() {
                continue;
            }
            let (x, y) = figure.point(layout.center(index));
            if !figure.contains((x, y)) {
                continue;
            }
            output.push_str(&format!(
                "<text x=\"{x:.2}\" y=\"{y:.2}\" text-anchor=\"middle\" dominant-baseline=\"central\" font-family=\"sans-serif\" font-size=\"11\" fill=\"{}\">{}</text>\n",
                svg_color(params.canvas_foreground, 255),
                escape_xml(&node.name)
            ));
        }
    }
    output.push_str("</g>\n</svg>\n");
    Ok(output)
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
    export_png_impl(
        path,
        None,
        graph,
        layout,
        params,
        None,
        None,
        false,
        &FigureOptions::default(),
    )
}

/// Export the current interactive view, including active GFA metadata overlays.
#[allow(clippy::too_many_arguments)]
pub fn export_png_with_options(
    path: &Path,
    gfa: &GfaGraph,
    graph: &ViewGraph,
    layout: &Layout,
    params: &RenderParams,
    selected_path: Option<usize>,
    selected_walk: Option<usize>,
    show_containments: bool,
    options: &FigureOptions,
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
        options,
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
    options: &FigureOptions,
) -> Result<()> {
    let image = render_png_impl(
        gfa,
        graph,
        layout,
        params,
        selected_path,
        selected_walk,
        show_containments,
        options,
    )?;
    atomic_write(path, |file| {
        let mut writer = BufWriter::new(file);
        image.write_to(&mut writer, image::ImageFormat::Png)?;
        writer.flush()?;
        Ok(())
    })
}

#[cfg(target_arch = "wasm32")]
#[allow(clippy::too_many_arguments)]
pub fn generate_png_with_options(
    gfa: &GfaGraph,
    graph: &ViewGraph,
    layout: &Layout,
    params: &RenderParams,
    selected_path: Option<usize>,
    selected_walk: Option<usize>,
    show_containments: bool,
    options: &FigureOptions,
) -> Result<Vec<u8>> {
    let image = render_png_impl(
        Some(gfa),
        graph,
        layout,
        params,
        selected_path,
        selected_walk,
        show_containments,
        options,
    )?;
    let mut output = std::io::Cursor::new(Vec::new());
    image.write_to(&mut output, image::ImageFormat::Png)?;
    Ok(output.into_inner())
}

#[allow(clippy::too_many_arguments)]
fn render_png_impl(
    gfa: Option<&GfaGraph>,
    graph: &ViewGraph,
    layout: &Layout,
    params: &RenderParams,
    selected_path: Option<usize>,
    selected_walk: Option<usize>,
    show_containments: bool,
    options: &FigureOptions,
) -> Result<ImageBuffer<Rgba<u8>, Vec<u8>>> {
    let figure = FigureTransform::new(layout, options)?;
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
            draw_dashed_line(&mut image, a, b, params.canvas_foreground, edge_alpha, 1);
        } else {
            draw_line(&mut image, a, b, params.canvas_foreground, edge_alpha, 1);
        }
    }

    let node_width = node_width(params);
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
                node_width.round() as i32,
            );
        }
        if points.len() >= 2 {
            draw_png_arrow(
                &mut image,
                figure.point(points[points.len() - 2]),
                figure.point(points[points.len() - 1]),
                node_width,
                color,
            );
        } else if let Some(&point) = points.first() {
            let point = figure.point(point);
            draw_line(
                &mut image,
                point,
                point,
                color,
                255,
                node_width.round() as i32,
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

    if params.show_labels {
        let definitions = egui::FontDefinitions::default();
        let font_name = definitions
            .families
            .get(&egui::FontFamily::Proportional)
            .and_then(|fonts| fonts.first())
            .ok_or_else(|| anyhow::anyhow!("No bundled font available for figure labels"))?;
        let data = &definitions.font_data[font_name];
        let font = FontRef::try_from_slice(&data.font)?;
        for (index, node) in graph.nodes.iter().enumerate() {
            if index >= layout.num_nodes() {
                continue;
            }
            let center = figure.point(layout.center(index));
            if figure.contains(center) {
                draw_png_label(
                    &mut image,
                    &font,
                    &node.name,
                    center,
                    params.canvas_foreground,
                );
            }
        }
    }
    // Preserve the requested crop when output and viewport aspect ratios differ.
    if options.world_bounds.is_some() {
        let background = Rgba([
            params.canvas_background.r(),
            params.canvas_background.g(),
            params.canvas_background.b(),
            255,
        ]);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            if !figure.contains((x as f32 + 0.5, y as f32 + 0.5)) {
                *pixel = background;
            }
        }
    }

    Ok(image)
}

#[allow(clippy::too_many_arguments)]
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
            let fraction = (containment.position as f32 / container_len as f32).clamp(0.0, 1.0);
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

    if let Some(index) = selected_path
        && let Some(path) = gfa.paths.get(index)
    {
        let steps = gfa.path_steps(path);
        append_svg_step_polylines(
            output,
            figure,
            graph,
            layout,
            steps.iter().map(|s| (s.segment, s.strand)),
            PATH,
            6.0,
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

    if let Some(index) = selected_walk
        && let Some(walk) = gfa.walks.get(index)
    {
        let steps = gfa.walk_steps(walk);
        append_svg_step_polylines(
            output,
            figure,
            graph,
            layout,
            steps.iter().map(|s| (s.segment, s.strand)),
            WALK,
            5.0,
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
            output.push_str(&format!(
                    "<line x1=\"{:.2}\" y1=\"{:.2}\" x2=\"{:.2}\" y2=\"{:.2}\" stroke=\"{WALK}\" stroke-width=\"3\"/>\n",
                    a.0, a.1, b.0, b.1
                ));
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
        let points = layout.pts(node);
        if points.len() >= 2 {
            let (from, to) = match strand {
                crate::gfa::Strand::Forward => (points[points.len() - 2], points[points.len() - 1]),
                crate::gfa::Strand::Reverse => (points[1], points[0]),
            };
            append_svg_arrow(output, figure.point(from), figure.point(to), width, color);
        }
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
            let fraction = (containment.position as f32 / container_len as f32).clamp(0.0, 1.0);
            let a = figure.point(layout.point_at_fraction(container, fraction));
            let b = figure.point(oriented_entry(
                layout,
                contained,
                containment.contained_strand,
            ));
            draw_dashed_line(image, a, b, containment_color, 230, 2);
        }
    }

    if let Some(index) = selected_path
        && let Some(path) = gfa.paths.get(index)
    {
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

    if let Some(index) = selected_walk
        && let Some(walk) = gfa.walks.get(index)
    {
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
        if points.len() >= 2 {
            let (from, to) = match strand {
                crate::gfa::Strand::Forward => (points[points.len() - 2], points[points.len() - 1]),
                crate::gfa::Strand::Reverse => (points[1], points[0]),
            };
            draw_png_arrow(
                image,
                figure.point(from),
                figure.point(to),
                thickness as f32,
                color,
            );
        }
    }
}

#[inline]
fn oriented_entry(layout: &Layout, node: usize, strand: crate::gfa::Strand) -> [f32; 2] {
    match strand {
        crate::gfa::Strand::Forward => layout.start(node),
        crate::gfa::Strand::Reverse => layout.end(node),
    }
}

#[inline]
fn oriented_exit(layout: &Layout, node: usize, strand: crate::gfa::Strand) -> [f32; 2] {
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
    offset_x: f32,
    offset_y: f32,
    clip: [f32; 4],
}

impl FigureTransform {
    fn new(layout: &Layout, options: &FigureOptions) -> Result<Self> {
        options.validate()?;
        anyhow::ensure!(
            !layout.positions.is_empty(),
            "The graph has no layout positions to export"
        );
        let [min_x, min_y, max_x, max_y] = if let Some(bounds) = options.world_bounds {
            bounds
        } else {
            let mut bounds = [
                f32::INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::NEG_INFINITY,
            ];
            for &[x, y] in &layout.positions {
                anyhow::ensure!(
                    x.is_finite() && y.is_finite(),
                    "The layout contains non-finite positions"
                );
                bounds[0] = bounds[0].min(x);
                bounds[1] = bounds[1].min(y);
                bounds[2] = bounds[2].max(x);
                bounds[3] = bounds[3].max(y);
            }
            bounds
        };
        let width = options.width as f32;
        let height = options.height as f32;
        let margin = if options.world_bounds.is_some() {
            0.0
        } else {
            72.0_f32.min(width.min(height) * 0.045)
        };
        let minimum_extent = if options.world_bounds.is_some() {
            f32::MIN_POSITIVE
        } else {
            1.0
        };
        let world_width = (max_x - min_x).max(minimum_extent);
        let world_height = (max_y - min_y).max(minimum_extent);
        let scale =
            ((width - margin * 2.0) / world_width).min((height - margin * 2.0) / world_height);
        anyhow::ensure!(
            scale.is_finite() && scale > 0.0,
            "The layout bounds cannot be exported"
        );
        let offset_x = (width - world_width * scale) * 0.5;
        let offset_y = (height - world_height * scale) * 0.5;
        let clip = if options.world_bounds.is_some() {
            [
                offset_x,
                offset_y,
                offset_x + world_width * scale,
                offset_y + world_height * scale,
            ]
        } else {
            [0.0, 0.0, width, height]
        };
        Ok(Self {
            min_x,
            min_y,
            scale,
            width,
            height,
            offset_x,
            offset_y,
            clip,
        })
    }

    fn point(&self, point: [f32; 2]) -> (f32, f32) {
        (
            self.offset_x + (point[0] - self.min_x) * self.scale,
            self.offset_y + (point[1] - self.min_y) * self.scale,
        )
    }

    fn contains(&self, point: (f32, f32)) -> bool {
        point.0 >= self.clip[0]
            && point.0 <= self.clip[2]
            && point.1 >= self.clip[1]
            && point.1 <= self.clip[3]
    }
}

fn node_width(params: &RenderParams) -> f32 {
    (5.0 * params.node_scale).clamp(1.0, 80.0)
}

fn arrow_triangle(from: (f32, f32), tip: (f32, f32), width: f32) -> Option<[(f32, f32); 3]> {
    let dx = tip.0 - from.0;
    let dy = tip.1 - from.1;
    let length = dx.hypot(dy);
    if !length.is_finite() || length < 3.0 {
        return None;
    }
    let size = (width * 1.8).min(length);
    let ux = dx / length;
    let uy = dy / length;
    let base = (tip.0 - ux * size, tip.1 - uy * size);
    Some([
        tip,
        (base.0 - uy * width, base.1 + ux * width),
        (base.0 + uy * width, base.1 - ux * width),
    ])
}

fn append_svg_arrow(
    output: &mut String,
    from: (f32, f32),
    to: (f32, f32),
    width: f32,
    color: &str,
) {
    if let Some([tip, left, right]) = arrow_triangle(from, to, width) {
        output.push_str(&format!(
            "<polygon fill=\"{color}\" points=\"{:.2},{:.2} {:.2},{:.2} {:.2},{:.2}\"/>\n",
            tip.0, tip.1, left.0, left.1, right.0, right.1
        ));
    }
}

fn draw_png_arrow(
    image: &mut ImageBuffer<Rgba<u8>, Vec<u8>>,
    from: (f32, f32),
    to: (f32, f32),
    width: f32,
    color: Color32,
) {
    let Some(vertices) = arrow_triangle(from, to, width) else {
        return;
    };
    let min_x = vertices
        .iter()
        .map(|p| p.0)
        .fold(f32::INFINITY, f32::min)
        .floor()
        .max(0.0) as u32;
    let max_x = vertices
        .iter()
        .map(|p| p.0)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .max(0.0)
        .min(image.width() as f32) as u32;
    let min_y = vertices
        .iter()
        .map(|p| p.1)
        .fold(f32::INFINITY, f32::min)
        .floor()
        .max(0.0) as u32;
    let max_y = vertices
        .iter()
        .map(|p| p.1)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .max(0.0)
        .min(image.height() as f32) as u32;
    for y in min_y..max_y {
        for x in min_x..max_x {
            let mut negative = false;
            let mut positive = false;
            for i in 0..3 {
                let a = vertices[i];
                let b = vertices[(i + 1) % 3];
                let cross =
                    (x as f32 + 0.5 - a.0) * (b.1 - a.1) - (y as f32 + 0.5 - a.1) * (b.0 - a.0);
                negative |= cross < 0.0;
                positive |= cross > 0.0;
            }
            if !(negative && positive) {
                image.put_pixel(x, y, Rgba([color.r(), color.g(), color.b(), 255]));
            }
        }
    }
}

fn draw_png_label(
    image: &mut ImageBuffer<Rgba<u8>, Vec<u8>>,
    font: &FontRef<'_>,
    text: &str,
    center: (f32, f32),
    color: Color32,
) {
    let scaled = font.as_scaled(11.0);
    let mut width = 0.0;
    let mut previous = None;
    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(previous) = previous {
            width += scaled.kern(previous, id);
        }
        width += scaled.h_advance(id);
        previous = Some(id);
    }
    let mut x = center.0 - width * 0.5;
    let baseline = center.1 + (scaled.ascent() + scaled.descent()) * 0.5;
    previous = None;
    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(previous) = previous {
            x += scaled.kern(previous, id);
        }
        let glyph = id.with_scale_and_position(11.0, ab_glyph::point(x, baseline));
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, coverage| {
                let px = bounds.min.x as i64 + i64::from(gx);
                let py = bounds.min.y as i64 + i64::from(gy);
                if px >= 0
                    && py >= 0
                    && px < i64::from(image.width())
                    && py < i64::from(image.height())
                {
                    let pixel = image.get_pixel_mut(px as u32, py as u32);
                    for (channel, value) in [color.r(), color.g(), color.b()].iter().enumerate() {
                        pixel.0[channel] = (pixel.0[channel] as f32 * (1.0 - coverage)
                            + *value as f32 * coverage)
                            .round() as u8;
                    }
                }
            });
        }
        x += scaled.h_advance(id);
        previous = Some(id);
    }
}

/// Clip before rasterization: a crop can put endpoints very far outside the image.
fn clip_line(
    from: (f32, f32),
    to: (f32, f32),
    width: u32,
    height: u32,
    padding: f32,
) -> Option<((f32, f32), (f32, f32))> {
    if ![from.0, from.1, to.0, to.1].iter().all(|v| v.is_finite()) {
        return None;
    }
    let dx = to.0 as f64 - from.0 as f64;
    let dy = to.1 as f64 - from.1 as f64;
    let mut entry = 0.0_f64;
    let mut exit = 1.0_f64;
    for (p, q) in [
        (-dx, from.0 as f64 + padding as f64),
        (dx, width as f64 + padding as f64 - from.0 as f64),
        (-dy, from.1 as f64 + padding as f64),
        (dy, height as f64 + padding as f64 - from.1 as f64),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else if p < 0.0 {
            entry = entry.max(q / p);
        } else {
            exit = exit.min(q / p);
        }
        if entry > exit {
            return None;
        }
    }
    Some((
        (
            (from.0 as f64 + entry * dx) as f32,
            (from.1 as f64 + entry * dy) as f32,
        ),
        (
            (from.0 as f64 + exit * dx) as f32,
            (from.1 as f64 + exit * dy) as f32,
        ),
    ))
}

fn draw_dashed_line(
    image: &mut ImageBuffer<Rgba<u8>, Vec<u8>>,
    from: (f32, f32),
    to: (f32, f32),
    color: Color32,
    alpha: u8,
    thickness: i32,
) {
    let Some((from, to)) = clip_line(from, to, image.width(), image.height(), thickness as f32)
    else {
        return;
    };
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
    let Some((from, to)) = clip_line(from, to, image.width(), image.height(), thickness as f32)
    else {
        return;
    };
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
        format!(
            "rgba({}, {}, {}, {:.3})",
            color.r(),
            color.g(),
            color.b(),
            alpha as f32 / 255.0
        )
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

        let (n50, l50) = if total_length == 0 {
            (0, 0)
        } else {
            let half = total_length.div_ceil(2);
            let mut cumulative = 0;
            lengths
                .iter()
                .enumerate()
                .find_map(|(index, &length)| {
                    cumulative += length;
                    (cumulative >= half).then_some((length, index + 1))
                })
                .unwrap_or((0, 0))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::FilterParams;

    fn fixture(text: &str) -> (GfaGraph, ViewGraph, Layout) {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(text.as_bytes()).unwrap();
        let gfa = crate::gfa::parse_gfa(file.path()).unwrap();
        let view = ViewGraph::from_gfa(&gfa, &FilterParams::default());
        let layout = Layout::new_with_graph(&view);
        (gfa, view, layout)
    }

    #[test]
    fn csv_round_trips_quoted_segment_names_and_selection() {
        let (gfa, view, _) =
            fixture("S\tname,with,commas\tACGT\tDP:f:3.5\nS\tname\"with\"quotes\tAT\tRC:i:7\n");
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stats.csv");
        export_csv(&path, &gfa, &view, &Selection::default()).unwrap();
        let records: Vec<_> = csv::Reader::from_path(&path)
            .unwrap()
            .records()
            .map(Result::unwrap)
            .collect();
        assert_eq!(records.len(), 2);
        assert!(
            records
                .iter()
                .any(|r| &r[0] == "name,with,commas" && &r[1] == "4" && &r[2] == "3.50")
        );
        assert!(
            records
                .iter()
                .any(|r| &r[0] == "name\"with\"quotes" && &r[3] == "7")
        );
        assert!(records.iter().all(|r| r.len() == 4));
        let mut selection = Selection::default();
        selection.nodes.insert(1);
        export_csv(&path, &gfa, &view, &selection).unwrap();
        assert_eq!(csv::Reader::from_path(&path).unwrap().records().count(), 1);
    }

    #[test]
    fn fasta_export_uses_stable_view_order_and_skips_missing_sequence() {
        let (gfa, view, _) = fixture("S\ta\tACGT\nS\tb\t*\tLN:i:2\nS\tc\tTT\n");
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("selection.fa");
        let mut selection = Selection::default();
        selection.nodes.extend([2, 0, 1]);
        export_fasta(&path, &gfa, &view, &selection).unwrap();
        let first = std::fs::read(&path).unwrap();
        let expected: String = view
            .nodes
            .iter()
            .filter_map(|node| {
                let sequence = gfa.segments[node.seg_idx].sequence(&gfa.mmap);
                (!sequence.is_empty()).then(|| {
                    format!(
                        ">{} len={}\n{}\n",
                        node.name,
                        node.length,
                        String::from_utf8_lossy(sequence)
                    )
                })
            })
            .collect();
        assert_eq!(String::from_utf8(first.clone()).unwrap(), expected);
        selection.nodes.clear();
        selection.nodes.extend([1, 0, 2]);
        export_fasta(&path, &gfa, &view, &selection).unwrap();
        assert_eq!(first, std::fs::read(path).unwrap());
    }

    #[test]
    fn n50_rounds_odd_totals_up_and_handles_zero_length_assemblies() {
        let (gfa, _, _) = fixture("S\ta\tAAAA\nS\tb\tAAA\nS\tc\tAA\n");
        let stats = AssemblyStats::compute(&gfa);
        assert_eq!((stats.total_length, stats.n50, stats.l50), (9, 3, 2));
        let (mut gfa, _, _) = fixture("S\ta\t*\tLN:i:0\nS\tb\t*\tLN:i:0\n");
        let stats = AssemblyStats::compute(&gfa);
        assert_eq!((stats.total_length, stats.n50, stats.l50), (0, 0, 0));
        gfa.segments.clear();
        let stats = AssemblyStats::compute(&gfa);
        assert_eq!((stats.total_length, stats.n50, stats.l50), (0, 0, 0));
    }

    #[test]
    fn failed_atomic_export_preserves_existing_file_and_cleans_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("existing.txt");
        std::fs::write(&path, b"previous successful export").unwrap();
        let result = atomic_write(&path, |writer| {
            writer.write_all(b"partial replacement")?;
            anyhow::bail!("simulated export failure")
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"previous successful export");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        atomic_write(&path, |writer| {
            writer.write_all(b"complete replacement")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"complete replacement");
    }

    #[test]
    fn figure_options_reject_invalid_sizes_and_bounds_before_overwriting() {
        let (gfa, view, layout) = fixture("S\tone\tACGT\n");
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("figure.svg");
        std::fs::write(&path, "previous figure").unwrap();
        for options in [
            FigureOptions {
                width: 0,
                ..Default::default()
            },
            FigureOptions {
                height: 16_385,
                ..Default::default()
            },
            FigureOptions {
                width: 16_384,
                height: 16_384,
                ..Default::default()
            },
            FigureOptions {
                world_bounds: Some([0.0, 0.0, 0.0, 1.0]),
                ..Default::default()
            },
            FigureOptions {
                world_bounds: Some([0.0, 0.0, f32::NAN, 1.0]),
                ..Default::default()
            },
        ] {
            assert!(
                export_svg_with_options(
                    &path,
                    &gfa,
                    &view,
                    &layout,
                    &RenderParams::default(),
                    None,
                    None,
                    false,
                    &options
                )
                .is_err()
            );
            assert!(
                export_png_with_options(
                    &path,
                    &gfa,
                    &view,
                    &layout,
                    &RenderParams::default(),
                    None,
                    None,
                    false,
                    &options
                )
                .is_err()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "previous figure");
        }
    }

    #[test]
    fn svg_and_png_preserve_dimensions_scale_arrows_and_labels() {
        let (gfa, view, layout) = fixture("S\tcontig_label\tACGTACGT\n");
        let directory = tempfile::tempdir().unwrap();
        let svg_path = directory.path().join("figure.svg");
        let png_path = directory.path().join("figure.png");
        let options = FigureOptions {
            width: 320,
            height: 240,
            ..Default::default()
        };
        let params = RenderParams {
            node_scale: 2.0,
            ..Default::default()
        };
        export_svg_with_options(
            &svg_path, &gfa, &view, &layout, &params, None, None, false, &options,
        )
        .unwrap();
        let svg = std::fs::read_to_string(svg_path).unwrap();
        assert!(svg.contains("width=\"320\" height=\"240\""));
        assert!(svg.contains("stroke-width=\"10.0\""));
        assert!(svg.contains("<polygon"));
        assert!(svg.contains(">contig_label</text>"));
        export_png_with_options(
            &png_path, &gfa, &view, &layout, &params, None, None, false, &options,
        )
        .unwrap();
        let labeled = image::open(&png_path).unwrap().into_rgba8();
        assert_eq!(labeled.dimensions(), (320, 240));
        let params = RenderParams {
            show_labels: false,
            node_scale: 2.0,
            ..Default::default()
        };
        export_png_with_options(
            &png_path, &gfa, &view, &layout, &params, None, None, false, &options,
        )
        .unwrap();
        let unlabeled = image::open(png_path).unwrap().into_rgba8();
        assert!(
            labeled
                .pixels()
                .zip(unlabeled.pixels())
                .filter(|(a, b)| a != b)
                .count()
                > 20
        );
    }

    #[test]
    fn crop_is_centered_and_offscreen_lines_are_clipped_before_rasterization() {
        let (_, _, layout) = fixture("S\ta\tACGT\n");
        let options = FigureOptions {
            width: 400,
            height: 200,
            world_bounds: Some([10.0, 20.0, 110.0, 120.0]),
        };
        let figure = FigureTransform::new(&layout, &options).unwrap();
        assert_eq!(figure.point([60.0, 70.0]), (200.0, 100.0));
        assert_eq!(figure.clip, [100.0, 0.0, 300.0, 200.0]);
        assert!(!figure.contains(figure.point([0.0, 70.0])));
        let mut image = ImageBuffer::from_pixel(32, 32, Rgba([0, 0, 0, 255]));
        draw_line(
            &mut image,
            (-1e9, 16.0),
            (1e9, 16.0),
            Color32::WHITE,
            255,
            1,
        );
        assert!(
            image
                .rows()
                .nth(16)
                .unwrap()
                .all(|p| p.0 == [255, 255, 255, 255])
        );
        assert!(clip_line((-100.0, -100.0), (-50.0, -50.0), 32, 32, 1.0).is_none());
    }
}
