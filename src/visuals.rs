use crate::{layout::Layout, ui::ThemePreset};
use egui::{Color32, Context, Pos2, Rect, Vec2};

/// A sampled overview for very large assemblies. Sampling bounds the per-frame
/// cost while retaining enough geometry to navigate dense graphs.
pub fn draw_minimap(
    ui: &mut egui::Ui,
    viewport: Rect,
    layout: &Layout,
    zoom: f32,
    pan: Vec2,
    theme: ThemePreset,
) -> Option<[f32; 2]> {
    if layout.positions.is_empty() {
        return None;
    }
    let size = Vec2::new(viewport.width().min(190.0), viewport.height().min(130.0));
    if size.x < 80.0 || size.y < 60.0 {
        return None;
    }
    let max = viewport.max - Vec2::splat(14.0);
    let rect = Rect::from_min_max(max - size, max);
    let step = (layout.positions.len() / 12_000).max(1);
    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for &[x, y] in layout.positions.iter().step_by(step) {
        min_x = min_x.min(x);
        max_x = max_x.max(x);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    }
    if !min_x.is_finite() {
        return None;
    }
    let width = (max_x - min_x).max(1.0);
    let height = (max_y - min_y).max(1.0);
    let scale = (rect.width() / width).min(rect.height() / height) * 0.92;
    let offset =
        rect.center() - Vec2::new((min_x + max_x) * 0.5 * scale, (min_y + max_y) * 0.5 * scale);
    let to_map =
        |point: [f32; 2]| Pos2::new(point[0] * scale + offset.x, point[1] * scale + offset.y);
    let from_map = |point: Pos2| [(point.x - offset.x) / scale, (point.y - offset.y) / scale];

    let painter = ui.painter();
    painter.rect_filled(
        rect,
        6.0,
        Color32::from_rgba_unmultiplied(
            theme.canvas_background().r(),
            theme.canvas_background().g(),
            theme.canvas_background().b(),
            235,
        ),
    );
    painter.rect_stroke(
        rect,
        6.0,
        egui::Stroke::new(1.0, theme.canvas_foreground().gamma_multiply(0.55)),
        egui::StrokeKind::Inside,
    );
    for &point in layout.positions.iter().step_by(step) {
        painter.circle_filled(
            to_map(point),
            0.7,
            theme.canvas_foreground().gamma_multiply(0.72),
        );
    }

    let center = viewport.center();
    let visible_min = [
        (viewport.min.x - center.x - pan.x) / zoom,
        (viewport.min.y - center.y - pan.y) / zoom,
    ];
    let visible_max = [
        (viewport.max.x - center.x - pan.x) / zoom,
        (viewport.max.y - center.y - pan.y) / zoom,
    ];
    painter.rect_stroke(
        Rect::from_two_pos(to_map(visible_min), to_map(visible_max)).intersect(rect),
        2.0,
        egui::Stroke::new(1.5, Color32::from_rgb(255, 205, 70)),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.min + Vec2::new(6.0, 5.0),
        egui::Align2::LEFT_TOP,
        "Overview",
        egui::FontId::proportional(10.0),
        theme.canvas_foreground().gamma_multiply(0.8),
    );

    let response = ui.interact(
        rect,
        ui.id().with("assembly_minimap"),
        egui::Sense::click_and_drag(),
    );
    if (response.clicked() || response.dragged()) && response.interact_pointer_pos().is_some() {
        return response.interact_pointer_pos().map(from_map);
    }
    None
}

pub fn configure_style(ctx: &Context, theme: ThemePreset) {
    ctx.set_theme(match theme {
        ThemePreset::Graphite | ThemePreset::Midnight => egui::Theme::Dark,
        ThemePreset::Light | ThemePreset::Paper => egui::Theme::Light,
    });

    let (mut visuals, panel, extreme, card, inactive, hovered, active, accent) = match theme {
        ThemePreset::Graphite => (
            egui::Visuals::dark(),
            Color32::from_rgb(24, 28, 36),
            Color32::from_rgb(14, 17, 22),
            Color32::from_rgb(31, 36, 46),
            Color32::from_rgb(38, 44, 55),
            Color32::from_rgb(48, 57, 70),
            Color32::from_rgb(39, 104, 132),
            Color32::from_rgb(112, 211, 255),
        ),
        ThemePreset::Midnight => (
            egui::Visuals::dark(),
            Color32::from_rgb(15, 23, 42),
            Color32::from_rgb(7, 12, 25),
            Color32::from_rgb(23, 34, 58),
            Color32::from_rgb(29, 43, 70),
            Color32::from_rgb(39, 57, 91),
            Color32::from_rgb(28, 86, 148),
            Color32::from_rgb(105, 183, 255),
        ),
        ThemePreset::Light => (
            egui::Visuals::light(),
            Color32::from_rgb(244, 247, 251),
            Color32::from_rgb(255, 255, 255),
            Color32::from_rgb(229, 235, 243),
            Color32::from_rgb(222, 228, 237),
            Color32::from_rgb(207, 220, 234),
            Color32::from_rgb(164, 207, 224),
            Color32::from_rgb(20, 118, 158),
        ),
        ThemePreset::Paper => (
            egui::Visuals::light(),
            Color32::from_rgb(247, 243, 234),
            Color32::from_rgb(255, 252, 245),
            Color32::from_rgb(235, 229, 215),
            Color32::from_rgb(228, 220, 204),
            Color32::from_rgb(218, 207, 185),
            Color32::from_rgb(213, 171, 139),
            Color32::from_rgb(161, 75, 52),
        ),
    };
    visuals.panel_fill = panel;
    visuals.window_fill = panel;
    visuals.extreme_bg_color = extreme;
    visuals.faint_bg_color = card;
    visuals.selection.bg_fill = active;
    visuals.selection.stroke = egui::Stroke::new(1.0, accent);
    visuals.widgets.noninteractive.bg_fill = card;
    visuals.widgets.inactive.bg_fill = inactive;
    visuals.widgets.hovered.bg_fill = hovered;
    visuals.widgets.active.bg_fill = active;
    visuals.widgets.open.bg_fill = hovered;
    visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(5);
    visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(5);
    visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(5);
    visuals.widgets.active.corner_radius = egui::CornerRadius::same(5);
    visuals.widgets.open.corner_radius = egui::CornerRadius::same(5);
    visuals.window_corner_radius = egui::CornerRadius::same(7);
    ctx.set_visuals(visuals);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 7.0);
        style.spacing.button_padding = Vec2::new(10.0, 6.0);
        style.spacing.indent = 16.0;
        style.spacing.slider_width = 150.0;
    });
}
