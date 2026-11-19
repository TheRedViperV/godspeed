//! Colors, type and small drawing helpers for the Space view. The chrome follows
//! the app's own palette (crate::theme); the treemap keeps its own quieter tones.

use crate::space::tree::Cat;
use eframe::egui::{
    self, pos2, vec2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Galley, Margin, Painter,
    Pos2, Rect, Response, RichText, Shape, Stroke, StrokeKind, TextStyle, Ui, Visuals,
};
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use std::sync::Arc;

#[derive(Clone, Copy)]
pub struct Pal {
    pub dark: bool,
    pub bg: Color32,
    pub chrome: Color32,
    pub raised: Color32,
    pub raised_hover: Color32,
    pub border: Color32,
    pub divider: Color32,
    pub text: Color32,
    pub weak: Color32,
    pub faint: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub sel: Color32,
    pub hover: Color32,
    pub track: Color32,
    pub bar: Color32,
    pub good: Color32,
    pub warn: Color32,
    pub danger: Color32,
    pub folder: Color32,
    pub tm_bg: Color32,
    pub tm_dir: Color32,
    pub tm_line: Color32,
    pub tm_text: Color32,
    pub tm_weak: Color32,
    pub tm_rest: Color32,
    pub tm_hover: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

fn alpha(rgb: u32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, a)
}

/// The app's palette, plus the colors only the Space view needs.
pub fn pal(dark: bool) -> Pal {
    let g = crate::theme::pal(dark);
    let mut p = base(dark);
    (p.bg, p.chrome, p.raised, p.raised_hover) = (g.bg, g.chrome, g.raised, g.raised_hover);
    (p.border, p.divider, p.text, p.weak, p.faint) = (g.border, g.divider, g.text, g.weak, g.faint);
    (p.accent, p.on_accent, p.sel, p.hover, p.track) = (g.accent, g.on_accent, g.sel, g.hover, g.track);
    (p.good, p.warn) = (g.good, g.warn);
    p.bar = mix(g.accent, g.accent2, 0.35);
    if dark {
        (p.tm_bg, p.tm_dir, p.tm_line) = (hex(0x0e0e12), hex(0x1f1f27), hex(0x0b0b0e));
    }
    p
}

fn base(dark: bool) -> Pal {
    if dark {
        Pal {
            dark,
            bg: hex(0x1c1c1c),
            chrome: hex(0x202020),
            raised: hex(0x2b2b2b),
            raised_hover: hex(0x323232),
            border: hex(0x3a3a3a),
            divider: hex(0x2c2c2c),
            text: hex(0xededed),
            weak: hex(0xa6a6a6),
            faint: hex(0x767676),
            accent: hex(0x4cc2ff),
            on_accent: hex(0x000000),
            sel: alpha(0xffffff, 16),
            hover: alpha(0xffffff, 9),
            track: hex(0x333333),
            bar: hex(0x3d9bd1),
            good: hex(0x6ccb5f),
            warn: hex(0xf2b84b),
            danger: hex(0xff99a4),
            folder: hex(0xd8ae58),
            tm_bg: hex(0x121212),
            tm_dir: hex(0x242424),
            tm_line: hex(0x101010),
            tm_text: hex(0xd6d6d6),
            tm_weak: hex(0x8f8f8f),
            tm_rest: hex(0x3a3d40),
            tm_hover: hex(0xffffff),
        }
    } else {
        Pal {
            dark,
            bg: hex(0xffffff),
            chrome: hex(0xf3f3f3),
            raised: hex(0xfbfbfb),
            raised_hover: hex(0xf2f2f2),
            border: hex(0xd9d9d9),
            divider: hex(0xebebeb),
            text: hex(0x1b1b1b),
            weak: hex(0x5f5f5f),
            faint: hex(0x9a9a9a),
            accent: hex(0x005fb8),
            on_accent: hex(0xffffff),
            sel: alpha(0x000000, 12),
            hover: alpha(0x000000, 8),
            track: hex(0xe6e6e6),
            bar: hex(0x3f86cf),
            good: hex(0x0f7b0f),
            warn: hex(0x9d5d00),
            danger: hex(0xc42b1c),
            folder: hex(0xe1a72b),
            tm_bg: hex(0xdadada),
            tm_dir: hex(0xf4f4f4),
            tm_line: hex(0xc4c4c4),
            tm_text: hex(0x2b2b2b),
            tm_weak: hex(0x6f6f6f),
            tm_rest: hex(0xc9ccd0),
            tm_hover: hex(0x000000),
        }
    }
}

pub fn cat_color(c: Cat, dark: bool) -> Color32 {
    let base = match c {
        Cat::Video => hex(0xd9765f),
        Cat::Image => hex(0x6fb59a),
        Cat::Audio => hex(0xe3bd6a),
        Cat::Archive => hex(0x9a88c6),
        Cat::Code => hex(0x5d96c9),
        Cat::Document => hex(0xd690a6),
        Cat::Program => hex(0x6aa9a6),
        Cat::Data => hex(0xb3926c),
        Cat::Game => hex(0x8fae5a),
        Cat::Other => hex(0xa3aab2),
    };
    if dark {
        mix(base, Color32::BLACK, 0.12)
    } else {
        base
    }
}

pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()), a.a())
}

pub const DANGER_FILL: Color32 = hex(0xc42b1c);

fn style(dark: bool) -> egui::Style {
    let p = pal(dark);
    let mut s = egui::Style::default();
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    v.panel_fill = p.bg;
    v.window_fill = if dark { hex(0x2b2b2b) } else { hex(0xffffff) };
    v.window_stroke = Stroke::new(1.0_f32, p.border);
    v.extreme_bg_color = if dark { hex(0x1f1f1f) } else { hex(0xffffff) };
    v.faint_bg_color = p.chrome;
    v.code_bg_color = p.raised;
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(6);
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = if dark { alpha(0x4cc2ff, 70) } else { alpha(0x005fb8, 60) };
    v.selection.stroke = Stroke::new(1.0_f32, p.accent);
    v.window_shadow.color = Color32::from_black_alpha(if dark { 90 } else { 40 });
    v.popup_shadow.color = Color32::from_black_alpha(if dark { 80 } else { 32 });

    let w = &mut v.widgets;
    for ws in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        ws.corner_radius = CornerRadius::same(4);
        ws.expansion = 0.0;
        ws.fg_stroke = Stroke::new(1.0_f32, p.text);
    }
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.divider);
    w.noninteractive.bg_fill = p.bg;
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.text);
    w.inactive.bg_fill = p.raised;
    w.inactive.weak_bg_fill = p.raised;
    w.inactive.bg_stroke = Stroke::new(1.0_f32, p.border);
    w.hovered.bg_fill = p.raised_hover;
    w.hovered.weak_bg_fill = p.raised_hover;
    w.hovered.bg_stroke = Stroke::new(1.0_f32, p.border);
    w.active.bg_fill = p.chrome;
    w.active.weak_bg_fill = p.chrome;
    w.active.bg_stroke = Stroke::new(1.0_f32, p.border);
    w.open.bg_fill = p.raised_hover;
    w.open.weak_bg_fill = p.raised_hover;
    w.open.bg_stroke = Stroke::new(1.0_f32, p.border);
    s.visuals = v;

    s.text_styles = [
        (TextStyle::Small, FontId::proportional(11.5)),
        (TextStyle::Body, FontId::proportional(13.0)),
        (TextStyle::Button, FontId::proportional(13.0)),
        (TextStyle::Heading, semibold(20.0)),
        (TextStyle::Monospace, FontId::monospace(12.5)),
    ]
    .into();
    s.spacing.item_spacing = vec2(8.0, 6.0);
    s.spacing.button_padding = vec2(12.0, 5.0);
    s.spacing.interact_size.y = 28.0;
    s.spacing.menu_margin = Margin::same(6);
    s.spacing.window_margin = Margin::same(20);
    s.spacing.scroll.bar_width = 8.0;
    s.spacing.scroll.floating = true;
    s.interaction.tooltip_delay = 0.2;
    s.interaction.show_tooltips_only_when_still = false;
    s
}

pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.set_style_of(egui::Theme::Dark, style(true));
    ctx.set_style_of(egui::Theme::Light, style(false));
}

/// Uses the fonts Windows itself uses, falling back to egui's built-in ones.
fn install_fonts(ctx: &egui::Context) {
    let dir = std::path::PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into())).join("Fonts");
    let mut defs = FontDefinitions::default();
    let mut add = |key: &str, files: &[&str]| -> bool {
        for f in files {
            if let Ok(bytes) = std::fs::read(dir.join(f)) {
                defs.font_data.insert(key.to_owned(), Arc::new(FontData::from_owned(bytes)));
                return true;
            }
        }
        false
    };
    let regular = add("ui", &["segoeui.ttf"]);
    let semi = add("ui-semibold", &["seguisb.ttf", "segoeuib.ttf"]);
    let mono = add("mono", &["CascadiaMono.ttf", "consola.ttf"]);

    let base_prop = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let base_mono = defs.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let with = |first: Option<&str>, rest: &[String]| -> Vec<String> {
        first.map(str::to_owned).into_iter().chain(rest.iter().cloned()).collect()
    };
    let prop = with(regular.then_some("ui"), &base_prop);
    let semibold = with(semi.then_some("ui-semibold").or(regular.then_some("ui")), &base_prop);
    defs.families.insert(FontFamily::Proportional, prop);
    defs.families.insert(FontFamily::Name("semibold".into()), semibold);
    defs.families.insert(FontFamily::Monospace, with(mono.then_some("mono"), &base_mono));
    ctx.set_fonts(defs);
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

pub fn font(size: f32) -> FontId {
    FontId::proportional(size)
}

/// Single-line text, cut off with an ellipsis past `max_w`.
pub fn galley(painter: &Painter, text: &str, font: FontId, color: Color32, max_w: f32) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(text.to_owned(), TextFormat { font_id: font, color, ..Default::default() });
    job.wrap = TextWrapping::truncate_at_width(max_w.max(1.0));
    painter.layout_job(job)
}

/// Draws elided text vertically centered on `cy`. Returns the width used.
pub fn text_at(painter: &Painter, x: f32, cy: f32, text: &str, font: FontId, color: Color32, max_w: f32) -> f32 {
    if max_w < 4.0 {
        return 0.0;
    }
    let g = galley(painter, text, font, color, max_w);
    let w = g.size().x;
    painter.galley(pos2(x, cy - g.size().y * 0.5), g, color);
    w
}

pub fn chevron(painter: &Painter, c: Pos2, open: bool, color: Color32) {
    let s = 3.5;
    let pts = if open {
        vec![pos2(c.x - s, c.y - s * 0.5), pos2(c.x, c.y + s * 0.5), pos2(c.x + s, c.y - s * 0.5)]
    } else {
        vec![pos2(c.x - s * 0.5, c.y - s), pos2(c.x + s * 0.5, c.y), pos2(c.x - s * 0.5, c.y + s)]
    };
    painter.add(Shape::line(pts, Stroke::new(1.4_f32, color)));
}

pub fn folder_icon(painter: &Painter, center: Pos2, color: Color32) {
    let body = Rect::from_center_size(center + vec2(0.0, 1.0), vec2(14.0, 10.0));
    let tab = Rect::from_min_size(body.min - vec2(0.0, 2.0), vec2(6.0, 4.0));
    painter.rect_filled(tab, 1.5, mix(color, Color32::BLACK, 0.18));
    painter.rect_filled(body, 1.5, color);
}

pub fn file_icon(painter: &Painter, center: Pos2, color: Color32) {
    let r = Rect::from_center_size(center, vec2(10.0, 13.0));
    painter.rect_filled(r, 1.5, color);
    let fold = Rect::from_min_size(pos2(r.max.x - 4.0, r.min.y), vec2(4.0, 4.0));
    painter.rect_filled(fold, 1.0, mix(color, Color32::WHITE, 0.35));
}

/// 0 = empty, 1 = checked, 2 = partly checked.
pub fn checkbox(painter: &Painter, center: Pos2, state: u8, p: &Pal) {
    let r = Rect::from_center_size(center, vec2(16.0, 16.0));
    if state == 0 {
        painter.rect_filled(r, 3.0, if p.dark { hex(0x262626) } else { hex(0xffffff) });
        painter.rect_stroke(r, 3.0, Stroke::new(1.0_f32, if p.dark { hex(0x8a8a8a) } else { hex(0x8a8a8a) }), StrokeKind::Inside);
        return;
    }
    painter.rect_filled(r, 3.0, p.accent);
    let stroke = Stroke::new(1.6_f32, p.on_accent);
    if state == 1 {
        let pts = vec![pos2(r.min.x + 4.0, r.center().y + 0.5), pos2(r.min.x + 7.0, r.max.y - 4.5), pos2(r.max.x - 3.5, r.min.y + 4.5)];
        painter.add(Shape::line(pts, stroke));
    } else {
        painter.line_segment([pos2(r.min.x + 4.0, r.center().y), pos2(r.max.x - 4.0, r.center().y)], stroke);
    }
}

pub fn bar(painter: &Painter, r: Rect, frac: f32, track: Color32, fill: Color32) {
    let radius = r.height() * 0.5;
    painter.rect_filled(r, radius, track);
    let w = (r.width() * frac.clamp(0.0, 1.0)).max(if frac > 0.0 { r.height() } else { 0.0 });
    if w > 0.0 {
        painter.rect_filled(Rect::from_min_size(r.min, vec2(w, r.height())), radius, fill);
    }
}

pub fn pill(painter: &Painter, left_center: Pos2, text: &str, fg: Color32, bg: Color32) -> Rect {
    let g = painter.layout_no_wrap(text.to_owned(), font(11.5), fg);
    let r = Rect::from_min_size(pos2(left_center.x, left_center.y - 9.0), vec2(g.size().x + 14.0, 18.0));
    painter.rect_filled(r, 9.0, bg);
    painter.galley(pos2(r.min.x + 7.0, r.center().y - g.size().y * 0.5), g, fg);
    r
}

pub fn brand_mark(painter: &Painter, r: Rect) {
    let x = |f: f32| r.min.x + r.width() * f;
    let y = |f: f32| r.min.y + r.height() * f;
    let g = 1.0;
    painter.rect_filled(Rect::from_min_max(pos2(x(0.0), y(0.0)), pos2(x(0.56) - g, y(1.0))), 2.0, hex(0x0078d4));
    painter.rect_filled(Rect::from_min_max(pos2(x(0.56), y(0.0)), pos2(x(1.0), y(0.5) - g)), 2.0, hex(0x4cc2ff));
    painter.rect_filled(Rect::from_min_max(pos2(x(0.56), y(0.5)), pos2(x(1.0), y(0.76) - g)), 1.5, hex(0x2b88d8));
    painter.rect_filled(Rect::from_min_max(pos2(x(0.56), y(0.76)), pos2(x(1.0), y(1.0))), 1.5, hex(0x9ad4ff));
}

pub fn primary(ui: &mut Ui, text: &str, p: &Pal) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(p.on_accent).font(semibold(13.0)))
            .fill(p.accent)
            .stroke(Stroke::NONE)
            .min_size(vec2(76.0, crate::app::CTRL_H)),
    )
}

pub fn secondary(ui: &mut Ui, text: &str, p: &Pal) -> Response {
    ui.add(egui::Button::new(RichText::new(text).color(p.text)).min_size(vec2(0.0, crate::app::CTRL_H)))
}

pub fn danger(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(Color32::WHITE).font(semibold(13.0)))
            .fill(DANGER_FILL)
            .stroke(Stroke::NONE)
            .min_size(vec2(84.0, 28.0)),
    )
}
