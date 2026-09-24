//! UI panels rendered each frame by egui.

use serde::{Deserialize, Serialize};

use egui::{Color32, Grid, RichText, ScrollArea, Ui};

use crate::export::AssemblyStats;
use crate::filter::{
    ColorMode, ComponentSort, ComponentSortOrder, ComponentTopology, FilterParams,
};
use crate::gfa::GfaGraph;
use crate::graph::{ComponentSummary, ViewGraph};
use crate::render::format_bp;
use crate::selection::Selection;

// ── Filter Panel ──────────────────────────────────────────────────────────────

fn panel_header(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.label(RichText::new(title).size(19.0).strong());
    ui.label(
        RichText::new(subtitle)
            .small()
            .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(5.0);
}

fn section<R>(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    let fill = ui.visuals().faint_bg_color;
    let accent = ui.visuals().selection.stroke.color;
    egui::Frame::new()
        .fill(fill)
        .corner_radius(7.0)
        .inner_margin(egui::Margin::same(11))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(title.to_uppercase())
                    .small()
                    .strong()
                    .color(accent),
            );
            ui.add_space(3.0);
            add(ui)
        })
        .inner
}

fn hint(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .small()
            .color(ui.visuals().weak_text_color()),
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemePreset {
    Graphite,
    Midnight,
    Light,
    Paper,
}

impl ThemePreset {
    pub fn label(self) -> &'static str {
        match self {
            Self::Graphite => "Graphite",
            Self::Midnight => "Midnight blue",
            Self::Light => "Clean light",
            Self::Paper => "Warm paper",
        }
    }

    pub fn canvas_background(self) -> Color32 {
        match self {
            Self::Graphite => Color32::from_rgb(20, 22, 28),
            Self::Midnight => Color32::from_rgb(8, 14, 29),
            Self::Light => Color32::from_rgb(250, 252, 255),
            Self::Paper => Color32::from_rgb(252, 249, 241),
        }
    }

    pub fn canvas_foreground(self) -> Color32 {
        match self {
            Self::Graphite => Color32::from_rgb(190, 205, 226),
            Self::Midnight => Color32::from_rgb(170, 202, 235),
            Self::Light => Color32::from_rgb(48, 59, 74),
            Self::Paper => Color32::from_rgb(69, 62, 52),
        }
    }
}

/// Returns true if filters changed.
pub fn filter_panel(ui: &mut Ui, filter: &mut FilterParams) -> bool {
    let mut changed = false;

    panel_header(
        ui,
        "Filters",
        "Choose which segments and components are shown.",
    );

    section(ui, "Segments", |ui| {
        ui.label("Name contains");
        changed |= ui
            .add(
                egui::TextEdit::singleline(&mut filter.name_contains)
                    .desired_width(f32::INFINITY)
                    .hint_text("Segment name…"),
            )
            .changed();

        ui.add_space(3.0);
        Grid::new("segment_filter_grid")
            .num_columns(2)
            .spacing([12.0, 7.0])
            .show(ui, |ui| {
                ui.label("Minimum length");
                let mut min_l = filter.min_length as u64;
                if ui
                    .add_sized(
                        [105.0, 24.0],
                        egui::DragValue::new(&mut min_l)
                            .speed(100.0)
                            .range(0u64..=u64::MAX)
                            .suffix(" bp"),
                    )
                    .changed()
                {
                    filter.min_length = min_l as usize;
                    changed = true;
                }
                ui.end_row();

                ui.label("Maximum length");
                let mut max_l = filter.max_length.unwrap_or(0) as u64;
                if ui
                    .add_sized(
                        [105.0, 24.0],
                        egui::DragValue::new(&mut max_l)
                            .speed(100.0)
                            .range(0u64..=u64::MAX)
                            .suffix(" bp"),
                    )
                    .changed()
                {
                    filter.max_length = (max_l > 0).then_some(max_l as usize);
                    changed = true;
                }
                ui.end_row();
            });
        hint(ui, "Set a maximum of 0 to disable it.");

        ui.add_space(5.0);
        ui.label("Depth / coverage range");
        Grid::new("depth_filter_grid")
            .num_columns(3)
            .spacing([7.0, 6.0])
            .show(ui, |ui| {
                let mut enabled = filter.min_depth.is_some();
                let mut value = filter.min_depth.unwrap_or(0.0);
                let toggled = ui.checkbox(&mut enabled, "Minimum").changed();
                let edited = ui
                    .add_enabled(
                        enabled,
                        egui::DragValue::new(&mut value)
                            .speed(0.5)
                            .range(0.0..=f64::MAX),
                    )
                    .changed();
                ui.label("×");
                if toggled || edited {
                    filter.min_depth = enabled.then_some(value);
                    changed = true;
                }
                ui.end_row();

                let mut enabled = filter.max_depth.is_some();
                let mut value = filter.max_depth.unwrap_or(100.0);
                let toggled = ui.checkbox(&mut enabled, "Maximum").changed();
                let edited = ui
                    .add_enabled(
                        enabled,
                        egui::DragValue::new(&mut value)
                            .speed(0.5)
                            .range(0.0..=f64::MAX),
                    )
                    .changed();
                ui.label("×");
                if toggled || edited {
                    filter.max_depth = enabled.then_some(value);
                    changed = true;
                }
                ui.end_row();
            });
    });

    ui.add_space(8.0);
    section(ui, "Components", |ui| {
        ui.label("Topology");
        egui::ComboBox::from_id_salt("component_topology_filter")
            .width(ui.available_width())
            .selected_text(filter.component_topology.label())
            .show_ui(ui, |ui| {
                for topology in [
                    ComponentTopology::All,
                    ComponentTopology::Circular,
                    ComponentTopology::Linear,
                    ComponentTopology::Branched,
                ] {
                    changed |= ui
                        .selectable_value(
                            &mut filter.component_topology,
                            topology,
                            topology.label(),
                        )
                        .changed();
                }
            });

        Grid::new("component_size_grid")
            .num_columns(2)
            .spacing([12.0, 7.0])
            .show(ui, |ui| {
                ui.label("Minimum segments");
                let mut value = filter.min_component_segments as u64;
                if ui
                    .add_sized(
                        [90.0, 24.0],
                        egui::DragValue::new(&mut value)
                            .speed(1.0)
                            .range(1..=u64::MAX),
                    )
                    .changed()
                {
                    filter.min_component_segments = value as usize;
                    changed = true;
                }
                ui.end_row();

                ui.label("Maximum segments");
                let mut value = filter.max_component_segments.unwrap_or(0) as u64;
                if ui
                    .add_sized(
                        [90.0, 24.0],
                        egui::DragValue::new(&mut value)
                            .speed(1.0)
                            .range(0..=u64::MAX),
                    )
                    .changed()
                {
                    filter.max_component_segments = (value > 0).then_some(value as usize);
                    changed = true;
                }
                ui.end_row();
            });
        hint(ui, "A maximum of 0 shows components of any size.");
    });

    ui.add_space(8.0);
    section(ui, "Order & limit", |ui| {
        ui.label("Sort components by");
        egui::ComboBox::from_id_salt("component_sort")
            .width(ui.available_width())
            .selected_text(filter.component_sort.label())
            .show_ui(ui, |ui| {
                for sort in [
                    ComponentSort::SegmentCount,
                    ComponentSort::TotalLength,
                    ComponentSort::MeanDepth,
                    ComponentSort::TotalReadCount,
                ] {
                    changed |= ui
                        .selectable_value(&mut filter.component_sort, sort, sort.label())
                        .changed();
                }
            });

        egui::ComboBox::from_id_salt("component_sort_order")
            .width(ui.available_width())
            .selected_text(filter.component_sort_order.label())
            .show_ui(ui, |ui| {
                for order in [
                    ComponentSortOrder::Descending,
                    ComponentSortOrder::Ascending,
                ] {
                    changed |= ui
                        .selectable_value(&mut filter.component_sort_order, order, order.label())
                        .changed();
                }
            });

        ui.horizontal(|ui| {
            ui.label("Show first");
            let mut value = filter.top_components as u64;
            if ui
                .add(
                    egui::DragValue::new(&mut value)
                        .speed(1.0)
                        .range(0..=100_000)
                        .suffix(" components"),
                )
                .changed()
            {
                filter.top_components = value as usize;
                changed = true;
            }
        });
        hint(ui, "0 shows every matching component.");
    });

    ui.add_space(8.0);
    if ui
        .add_sized(
            [ui.available_width(), 30.0],
            egui::Button::new("Reset filters"),
        )
        .clicked()
    {
        *filter = FilterParams::default();
        changed = true;
    }

    changed
}

// ── Display Panel ─────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayOptions {
    pub theme: ThemePreset,
    pub color_mode: ColorMode,
    pub auto_color_scale: bool,
    pub show_labels: bool,
    pub edge_opacity: f32,
    pub min_depth_color: f32,
    pub max_depth_color: f32,
    pub node_scale: f32,
}

impl Default for DisplayOptions {
    fn default() -> Self {
        Self {
            theme: ThemePreset::Graphite,
            color_mode: ColorMode::Depth,
            auto_color_scale: true,
            show_labels: true,
            edge_opacity: 0.6,
            min_depth_color: 0.0,
            max_depth_color: 100.0,
            node_scale: 1.0,
        }
    }
}

pub fn display_panel(ui: &mut Ui, opts: &mut DisplayOptions) -> bool {
    let mut changed = false;

    panel_header(ui, "Appearance", "Adjust graph color and visual density.");
    section(ui, "Color", |ui| {
        ui.label("Color segments by");
        egui::ComboBox::from_id_salt("color_mode")
            .width(ui.available_width())
            .selected_text(opts.color_mode.label())
            .show_ui(ui, |ui| {
                for mode in [
                    ColorMode::Depth,
                    ColorMode::Length,
                    ColorMode::Uniform,
                    ColorMode::ReadCount,
                ] {
                    changed |= ui
                        .selectable_value(&mut opts.color_mode, mode.clone(), mode.label())
                        .changed();
                }
            });

        if opts.color_mode == ColorMode::Depth || opts.color_mode == ColorMode::ReadCount {
            changed |= ui
                .checkbox(&mut opts.auto_color_scale, "Fit scale to visible data")
                .changed();
            ui.add_enabled_ui(!opts.auto_color_scale, |ui| {
                ui.label("Minimum");
                changed |= ui
                    .add(egui::Slider::new(&mut opts.min_depth_color, 0.0..=500.0).show_value(true))
                    .changed();
                ui.label("Maximum");
                changed |= ui
                    .add(
                        egui::Slider::new(&mut opts.max_depth_color, 1.0..=1000.0).show_value(true),
                    )
                    .changed();
            });
        }
        ui.add_space(4.0);
        draw_legend(ui, opts);
    });

    ui.add_space(8.0);
    section(ui, "Graph", |ui| {
        changed |= ui
            .checkbox(&mut opts.show_labels, "Show segment labels")
            .changed();
        ui.label("Segment thickness");
        changed |= ui
            .add(egui::Slider::new(&mut opts.node_scale, 0.2..=5.0))
            .changed();
        ui.label("Link opacity");
        changed |= ui
            .add(egui::Slider::new(&mut opts.edge_opacity, 0.0..=1.0))
            .changed();
    });

    changed
}

// ── GFA metadata overlays ────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayAction {
    FocusPath(usize),
    SelectPath(usize),
    FocusWalk(usize),
    SelectWalk(usize),
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayOptions {
    pub selected_path: Option<usize>,
    pub selected_walk: Option<usize>,
    pub show_containments: bool,
    pub path_query: String,
    pub walk_query: String,
}

pub fn overlays_panel(
    ui: &mut Ui,
    gfa: &GfaGraph,
    opts: &mut OverlayOptions,
) -> (bool, Option<OverlayAction>) {
    let mut changed = false;
    let mut action = None;
    panel_header(
        ui,
        "GFA overlays",
        "Highlight paths, haplotype walks and containment relationships.",
    );

    if !gfa.containments.is_empty() {
        section(ui, "Containments", |ui| {
            changed |= ui
                .checkbox(
                    &mut opts.show_containments,
                    format!("Show {} containments", gfa.containments.len()),
                )
                .changed();
            hint(
                ui,
                "Dotted connectors attach at the recorded position inside the container segment.",
            );
        });
        ui.add_space(8.0);
    }

    if !gfa.paths.is_empty() {
        section(ui, "Path overlay", |ui| {
            if let Some(index) = opts.selected_path
                && let Some(path) = gfa.paths.get(index)
            {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(path.name.as_ref()).strong());
                    if ui.small_button("Clear").clicked() {
                        opts.selected_path = None;
                        changed = true;
                    }
                });
                hint(ui, &format!("{} oriented steps", path.steps.len()));
                ui.horizontal(|ui| {
                    if ui.small_button("Focus").clicked() {
                        action = Some(OverlayAction::FocusPath(index));
                    }
                    if ui.small_button("Select segments").clicked() {
                        action = Some(OverlayAction::SelectPath(index));
                    }
                });
                ui.add_space(5.0);
            }

            ui.add(
                egui::TextEdit::singleline(&mut opts.path_query).hint_text("Filter path names…"),
            );
            let query = opts.path_query.trim().to_ascii_lowercase();
            let matches: Vec<_> = gfa
                .paths
                .iter()
                .enumerate()
                .filter(|(_, path)| {
                    query.is_empty() || path.name.as_ref().to_ascii_lowercase().contains(&query)
                })
                .take(20)
                .collect();

            ScrollArea::vertical().max_height(145.0).show(ui, |ui| {
                for (index, path) in matches {
                    if ui
                        .selectable_label(
                            opts.selected_path == Some(index),
                            format!("{}  ·  {} steps", path.name.as_ref(), path.steps.len()),
                        )
                        .clicked()
                    {
                        opts.selected_path = Some(index);
                        changed = true;
                    }
                }
            });
            hint(
                ui,
                "Showing up to 20 matches. Type part of a name to narrow the list.",
            );
        });
        ui.add_space(8.0);
    }

    if !gfa.walks.is_empty() {
        section(ui, "Walk overlay", |ui| {
            if let Some(index) = opts.selected_walk
                && let Some(walk) = gfa.walks.get(index)
            {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(walk_label(walk)).strong());
                    if ui.small_button("Clear").clicked() {
                        opts.selected_walk = None;
                        changed = true;
                    }
                });
                hint(ui, &format!("{} oriented steps", walk.steps.len()));
                ui.horizontal(|ui| {
                    if ui.small_button("Focus").clicked() {
                        action = Some(OverlayAction::FocusWalk(index));
                    }
                    if ui.small_button("Select segments").clicked() {
                        action = Some(OverlayAction::SelectWalk(index));
                    }
                });
                ui.add_space(5.0);
            }

            ui.add(
                egui::TextEdit::singleline(&mut opts.walk_query)
                    .hint_text("Filter sample / sequence…"),
            );
            let query = opts.walk_query.trim().to_ascii_lowercase();
            let matches: Vec<_> = gfa
                .walks
                .iter()
                .enumerate()
                .filter(|(_, walk)| {
                    if query.is_empty() {
                        true
                    } else {
                        walk.sample_id
                            .as_ref()
                            .to_ascii_lowercase()
                            .contains(&query)
                            || walk
                                .sequence_id
                                .as_ref()
                                .to_ascii_lowercase()
                                .contains(&query)
                            || walk.haplotype_index.to_string().contains(&query)
                    }
                })
                .take(20)
                .collect();

            ScrollArea::vertical().max_height(145.0).show(ui, |ui| {
                for (index, walk) in matches {
                    if ui
                        .selectable_label(
                            opts.selected_walk == Some(index),
                            format!("{}  ·  {} steps", walk_label(walk), walk.steps.len()),
                        )
                        .clicked()
                    {
                        opts.selected_walk = Some(index);
                        changed = true;
                    }
                }
            });
            hint(
                ui,
                "Showing up to 20 matches. Search by sample, haplotype or sequence.",
            );
        });
    }

    (changed, action)
}

fn walk_label(walk: &crate::gfa::Walk) -> String {
    let coordinates = match (walk.sequence_start, walk.sequence_end) {
        (Some(start), Some(end)) => format!(":{start}-{end}"),
        _ => String::new(),
    };
    format!(
        "{} · h{} · {}{}",
        walk.sample_id.as_ref(),
        walk.haplotype_index,
        walk.sequence_id.as_ref(),
        coordinates
    )
}

// ── Stats Panel ───────────────────────────────────────────────────────────────

pub fn stats_panel(ui: &mut Ui, stats: &AssemblyStats, view_graph: &ViewGraph) {
    panel_header(ui, "Assembly", "Summary statistics for the loaded graph.");
    section(ui, "Overview", |ui| {
        Grid::new("stats_grid")
            .num_columns(2)
            .spacing([16.0, 7.0])
            .show(ui, |ui| {
                let mut row = |label: &str, value: String| {
                    ui.label(RichText::new(label).color(ui.visuals().weak_text_color()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(value).monospace().strong());
                    });
                    ui.end_row();
                };

                row("GFA version", stats.gfa_version.label().to_string());
                row("Segments shown", format_count(view_graph.node_count()));
                row("Segments total", format_count(stats.num_segments));
                row("Connections shown", format_count(view_graph.edge_count()));
                row("Links total", format_count(stats.num_links));
                if stats.num_jumps > 0 {
                    row("Jumps", format_count(stats.num_jumps));
                }
                if stats.num_containments > 0 {
                    row("Containments", format_count(stats.num_containments));
                }
                if stats.num_paths > 0 {
                    row("Paths", format_count(stats.num_paths));
                }
                if stats.num_walks > 0 {
                    row("Walks", format_count(stats.num_walks));
                }
                row("Total length", format_bp(stats.total_length));
                row("N50", format_bp(stats.n50));
                row("L50", format_count(stats.l50));
                row("Longest", format_bp(stats.max_length));
                row("Shortest", format_bp(stats.min_length));
                if stats.mean_depth > 0.0 {
                    row("Mean coverage", format!("{:.1}×", stats.mean_depth));
                }
            });
    });
}

/// Draw a bounded, current-order component table. Returns the component to
/// select and focus when the user activates a row.
pub fn component_table(
    ui: &mut Ui,
    components: &[ComponentSummary],
    query: &mut String,
    page: &mut usize,
) -> Option<Vec<usize>> {
    let mut focus = None;
    panel_header(
        ui,
        "Components",
        "Current sort order. Click a row to select and focus that component.",
    );
    section(ui, "Browser", |ui| {
        ui.horizontal(|ui| {
            ui.label("Find");
            if ui
                .add(
                    egui::TextEdit::singleline(query)
                        .desired_width(f32::INFINITY)
                        .hint_text("Circular, linear, branched, or #…"),
                )
                .changed()
            {
                *page = 0;
            }
        });
        hint(
            ui,
            "Click a row to focus it. Use Top N to reduce the graph.",
        );
        ui.add_space(4.0);
        const PAGE_SIZE: usize = 12;
        let query = query.trim().to_ascii_lowercase();
        let matches = |index: usize, component: &ComponentSummary| {
            query.is_empty()
                || (index + 1).to_string().contains(&query)
                || component.kind.label().to_ascii_lowercase().contains(&query)
        };
        let matching_count = components
            .iter()
            .enumerate()
            .filter(|(index, component)| matches(*index, component))
            .count();
        let page_count = matching_count.div_ceil(PAGE_SIZE).max(1);
        *page = (*page).min(page_count - 1);
        let first = *page * PAGE_SIZE;
        Grid::new("component_table")
            .num_columns(6)
            .spacing([7.0, 5.0])
            .striped(true)
            .show(ui, |ui| {
                for heading in ["#", "Type", "Segs", "Length", "Coverage", "Reads"] {
                    ui.label(RichText::new(heading).small().strong());
                }
                ui.end_row();
                for (index, component) in components
                    .iter()
                    .enumerate()
                    .filter(|(index, component)| matches(*index, component))
                    .skip(first)
                    .take(PAGE_SIZE)
                {
                    if ui
                        .selectable_label(false, format!("#{}", index + 1))
                        .on_hover_text("Select and focus this component")
                        .clicked()
                    {
                        focus = Some(component.nodes.clone());
                    }
                    ui.label(RichText::new(component.kind.label()).small());
                    ui.label(
                        RichText::new(format_count(component.nodes.len()))
                            .small()
                            .monospace(),
                    );
                    ui.label(
                        RichText::new(format_bp(component.total_length))
                            .small()
                            .monospace(),
                    );
                    ui.label(
                        RichText::new(
                            component
                                .mean_depth
                                .map(|depth| format!("{depth:.1}×"))
                                .unwrap_or_else(|| "—".to_string()),
                        )
                        .small()
                        .monospace(),
                    );
                    ui.label(
                        RichText::new(
                            component
                                .total_read_count
                                .map(|count| format_count(count as usize))
                                .unwrap_or_else(|| "—".to_string()),
                        )
                        .small()
                        .monospace(),
                    );
                    ui.end_row();
                }
            });
        ui.add_space(5.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(*page > 0, egui::Button::new("Previous"))
                .clicked()
            {
                *page -= 1;
            }
            ui.label(RichText::new(format!("Page {} / {}", *page + 1, page_count)).small());
            if ui
                .add_enabled(*page + 1 < page_count, egui::Button::new("Next"))
                .clicked()
            {
                *page += 1;
            }
        });
    });
    focus
}

// ── Selection Info Panel ──────────────────────────────────────────────────────

pub fn selection_panel(
    ui: &mut Ui,
    selection: &Selection,
    gfa: &GfaGraph,
    graph: &ViewGraph,
    on_copy_seq: &mut bool,
    on_export_fasta: &mut bool,
    on_select_component: &mut bool,
) {
    panel_header(ui, "Selection", "Inspect and export selected segments.");

    if selection.is_empty() {
        section(ui, "No selection", |ui| {
            ui.label(
                RichText::new("Click a segment in Select mode to inspect it.")
                    .color(ui.visuals().weak_text_color()),
            );
            ui.add_space(4.0);
            hint(ui, "S  Select mode");
            hint(ui, "Ctrl + click  Add to selection");
            hint(ui, "Drag  Select an area");
        });
        if gfa.sequence_segment_count == 0 {
            ui.add_space(8.0);
            section(ui, "Sequence unavailable", |ui| {
                ui.label(
                    RichText::new(
                        "This graph contains structure and segment lengths, but no embedded nucleotide sequences. Copy and FASTA export are unavailable for .noseq GFA files.",
                    )
                    .color(ui.visuals().weak_text_color()),
                );
            });
        }
        return;
    }

    let total_len = selection.total_length(graph);
    let selected_sequence_count = selection
        .nodes
        .iter()
        .filter_map(|&node_index| graph.nodes.get(node_index))
        .filter_map(|node| gfa.segments.get(node.seg_idx))
        .filter(|segment| !segment.seq_range.is_empty())
        .count();
    section(ui, "Summary", |ui| {
        Grid::new("selection_summary")
            .num_columns(2)
            .spacing([16.0, 7.0])
            .show(ui, |ui| {
                ui.label(RichText::new("Segments").color(ui.visuals().weak_text_color()));
                ui.label(
                    RichText::new(format_count(selection.node_count()))
                        .monospace()
                        .strong(),
                );
                ui.end_row();
                ui.label(RichText::new("Total length").color(ui.visuals().weak_text_color()));
                ui.label(RichText::new(format_bp(total_len)).monospace().strong());
                ui.end_row();
            });
    });

    // Compute depth stats from selected segments.
    let mut depths: Vec<f64> = Vec::new();
    for &ni in selection.nodes.iter() {
        if ni < graph.nodes.len() {
            let seg_idx = graph.nodes[ni].seg_idx;
            if seg_idx < gfa.segments.len()
                && let Some(d) = gfa.segments[seg_idx].depth
            {
                depths.push(d);
            }
        }
    }

    // Show depth info when we have depth data.
    if !depths.is_empty() {
        ui.add_space(8.0);
        let min_d = depths.iter().cloned().fold(f64::INFINITY, f64::min);
        let max_d = depths.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let sum_d: f64 = depths.iter().sum();
        let avg_d = sum_d / depths.len() as f64;

        section(ui, "Depth / coverage", |ui| {
            Grid::new("selection_depth")
                .num_columns(3)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    for (label, value) in [("Min", min_d), ("Mean", avg_d), ("Max", max_d)] {
                        ui.label(RichText::new(label).color(ui.visuals().weak_text_color()));
                        ui.label(RichText::new(format!("{value:.2}×")).monospace().strong());
                    }
                    ui.end_row();
                });
        });
    }

    // Show details for single-node selection.
    if selection.node_count() == 1
        && let Some(&ni) = selection.nodes.iter().next()
        && ni < graph.nodes.len()
    {
        let node = &graph.nodes[ni];
        let seg = &gfa.segments[node.seg_idx];
        ui.add_space(8.0);
        section(ui, "Segment details", |ui| {
            Grid::new("sel_grid")
                .num_columns(2)
                .spacing([14.0, 7.0])
                .show(ui, |ui| {
                    ui.label("Name:");
                    ui.label(RichText::new(node.name.as_ref()).monospace().strong());
                    ui.end_row();
                    ui.label("Length:");
                    ui.label(format_bp(node.length));
                    ui.end_row();
                    if let Some(d) = node.depth {
                        ui.label("Coverage:");
                        ui.label(format!("{:.2}×", d));
                        ui.end_row();
                    }
                    if let Some(rc) = node.read_count {
                        ui.label("Read count:");
                        ui.label(format_count(rc as usize));
                        ui.end_row();
                    }
                });

            // Sequence preview.
            let seq = seg.sequence(&gfa.mmap);
            if !seq.is_empty() {
                ui.add_space(7.0);
                hint(ui, "Sequence preview · first 80 bp");
                let preview = String::from_utf8_lossy(&seq[..seq.len().min(80)]);
                ScrollArea::horizontal().show(ui, |ui| {
                    ui.label(RichText::new(preview.as_ref()).monospace().small());
                });
            }
        });
    }

    ui.add_space(8.0);
    let has_selected_sequence = selected_sequence_count > 0;
    let unavailable_reason = if gfa.sequence_segment_count == 0 {
        "This graph contains no embedded nucleotide sequences. Sequence actions are unavailable for .noseq GFA files."
    } else {
        "The current selection contains no embedded nucleotide sequence."
    };
    if !has_selected_sequence {
        section(ui, "Sequence unavailable", |ui| {
            ui.label(RichText::new(unavailable_reason).color(ui.visuals().weak_text_color()));
        });
        ui.add_space(8.0);
    } else if selected_sequence_count < selection.node_count() {
        hint(
            ui,
            &format!(
                "{} of {} selected segments contain sequence; FASTA export skips the others.",
                format_count(selected_sequence_count),
                format_count(selection.node_count())
            ),
        );
        ui.add_space(4.0);
    }

    let copy_response = ui
        .add_enabled(
            has_selected_sequence,
            egui::Button::new("Copy sequence").min_size(egui::vec2(ui.available_width(), 30.0)),
        )
        .on_disabled_hover_text(unavailable_reason);
    if copy_response.clicked() {
        *on_copy_seq = true;
    }
    let export_response = ui
        .add_enabled(
            has_selected_sequence,
            egui::Button::new("Export FASTA…").min_size(egui::vec2(ui.available_width(), 30.0)),
        )
        .on_disabled_hover_text(unavailable_reason);
    if export_response.clicked() {
        *on_export_fasta = true;
    }
    if ui
        .add_sized(
            [ui.available_width(), 30.0],
            egui::Button::new("Select entire component"),
        )
        .clicked()
    {
        *on_select_component = true;
    }
}

// ── Legend ────────────────────────────────────────────────────────────────────

pub fn draw_legend(ui: &mut Ui, opts: &DisplayOptions) {
    match opts.color_mode {
        ColorMode::Depth => {
            color_ramp(ui, crate::render::depth_color_for_legend);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("{:.0}×", opts.min_depth_color))
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{:.0}×", opts.max_depth_color))
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                });
            });
        }
        ColorMode::Length => {
            color_ramp(ui, |t| {
                Color32::from_rgb((t * 200.0) as u8, 80, ((1.0 - t) * 200.0) as u8)
            });
            ui.horizontal(|ui| {
                hint(ui, "Short");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    hint(ui, "Long")
                });
            });
        }
        ColorMode::ReadCount => {
            color_ramp(ui, crate::render::depth_color_for_legend);
            ui.horizontal(|ui| {
                hint(ui, "Fewer reads");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    hint(ui, "More reads")
                });
            });
        }
        ColorMode::Uniform => hint(ui, "All segments use the same color."),
    }
}

fn color_ramp(ui: &mut Ui, color: impl Fn(f32) -> Color32) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 9.0), egui::Sense::hover());
    let steps = 48;
    for i in 0..steps {
        let x0 = egui::lerp(rect.x_range(), i as f32 / steps as f32);
        let x1 = egui::lerp(rect.x_range(), (i + 1) as f32 / steps as f32);
        ui.painter().rect_filled(
            egui::Rect::from_min_max(egui::pos2(x0, rect.top()), egui::pos2(x1, rect.bottom())),
            if i == 0 || i + 1 == steps { 2.0 } else { 0.0 },
            color(i as f32 / (steps - 1) as f32),
        );
    }
}

fn format_count(value: usize) -> String {
    let digits = value.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(ch);
    }
    formatted
}
