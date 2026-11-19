//! Colors, type and drawing helpers: a dark-first palette with a violet-to-cyan
//! accent (matching the icon), Segoe UI type, and small animation helpers.

use crate::search::Cat;
use eframe::egui::{
    self, pos2, vec2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Painter, Pos2,
    Rect, Response, Sense, Shape, Stroke, TextStyle, Ui, Visuals,
};
use std::sync::Arc;

#[derive(Clone, Copy)]
pub struct Pal {
    pub dark: bool,
    pub bg: Color32,
    pub chrome: Color32,
    pub field: Color32,
    pub raised: Color32,
    pub raised_hover: Color32,
    pub border: Color32,
    pub divider: Color32,
    pub text: Color32,
    pub weak: Color32,
    pub faint: Color32,
    pub accent: Color32,
    pub accent2: Color32,
    pub on_accent: Color32,
    pub sel: Color32,
    pub hover: Color32,
    pub track: Color32,
    pub good: Color32,
    pub warn: Color32,
}

pub const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

fn alpha(rgb: u32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, a)
}

pub fn pal(dark: bool) -> Pal {
    if dark {
        Pal {
            dark,
            bg: hex(0x131318),
            chrome: hex(0x18181e),
            field: hex(0x1f1f27),
            raised: hex(0x1f1f27),
            raised_hover: hex(0x282832),
            border: hex(0x2e2e3a),
            divider: hex(0x23232c),
            text: hex(0xf1f1f5),
            weak: hex(0xa4a4b2),
            faint: hex(0x6c6c7c),
            accent: hex(0x8b7bff),
            accent2: hex(0x3fc8f5),
            on_accent: hex(0xffffff),
            sel: alpha(0x8b7bff, 34),
            hover: alpha(0xffffff, 8),
            track: hex(0x2a2a35),
            good: hex(0x4ade80),
            warn: hex(0xfbbf24),
        }
    } else {
        Pal {
            dark,
            bg: hex(0xf8f8fb),
            chrome: hex(0xf0f0f5),
            field: hex(0xffffff),
            raised: hex(0xffffff),
            raised_hover: hex(0xf4f4f9),
            border: hex(0xe0e0ea),
            divider: hex(0xe8e8f0),
            text: hex(0x17171d),
            weak: hex(0x5c5c6a),
            faint: hex(0x9696a6),
            accent: hex(0x5b4be0),
            accent2: hex(0x0ea5e9),
            on_accent: hex(0xffffff),
            sel: alpha(0x5b4be0, 24),
            hover: alpha(0x000000, 7),
            track: hex(0xe6e6ee),
            good: hex(0x16a34a),
            warn: hex(0xb45309),
        }
    }
}

pub fn cat_color(c: Option<Cat>, dark: bool) -> Color32 {
    let base = match c {
        Some(Cat::Folder) => hex(0xf2c14e),
        Some(Cat::Document) => hex(0x60a5fa),
        Some(Cat::Picture) => hex(0x34d399),
        Some(Cat::Video) => hex(0xf87171),
        Some(Cat::Audio) => hex(0xfbbf24),
        Some(Cat::Archive) => hex(0xa78bfa),
        Some(Cat::App) => hex(0x22d3ee),
        _ => hex(0x94a3b8),
    };
    if dark {
        base
    } else {
        mix(base, Color32::BLACK, 0.12)
    }
}

pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()), l(a.a(), b.a()))
}

/// A color at `a` opacity.
pub fn fade(c: Color32, a: f32) -> Color32 {
    c.gamma_multiply(a.clamp(0.0, 1.0))
}

/// Ease-out curve for animations, clamped to 0..1.
pub fn ease(t: f32) -> f32 {
    egui::emath::easing::cubic_out(t.clamp(0.0, 1.0))
}

fn style(dark: bool) -> egui::Style {
    let p = pal(dark);
    let mut s = egui::Style::default();
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    v.panel_fill = p.bg;
    v.window_fill = p.raised;
    v.window_stroke = Stroke::new(1.0_f32, p.border);
    v.extreme_bg_color = p.field;
    v.faint_bg_color = p.chrome;
    v.code_bg_color = p.raised;
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(10);
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = fade(p.accent, 0.35);
    v.selection.stroke = Stroke::new(1.0_f32, p.accent);
    v.text_cursor.stroke = Stroke::new(1.6_f32, p.accent);
    v.window_shadow.color = Color32::from_black_alpha(if dark { 110 } else { 36 });
    v.popup_shadow.color = Color32::from_black_alpha(if dark { 100 } else { 30 });

    let w = &mut v.widgets;
    for ws in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        ws.corner_radius = CornerRadius::same(8);
        ws.expansion = 0.0;
        ws.fg_stroke = Stroke::new(1.0_f32, p.text);
    }
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.divider);
    w.noninteractive.bg_fill = p.bg;
    w.inactive.bg_fill = p.raised;
    w.inactive.weak_bg_fill = p.raised;
    w.inactive.bg_stroke = Stroke::new(1.0_f32, p.border);
    w.hovered.bg_fill = p.raised_hover;
    w.hovered.weak_bg_fill = p.raised_hover;
    w.hovered.bg_stroke = Stroke::new(1.0_f32, mix(p.border, p.accent, 0.4));
    w.active.bg_fill = p.raised_hover;
    w.active.weak_bg_fill = p.raised_hover;
    w.active.bg_stroke = Stroke::new(1.0_f32, p.accent);
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
    s.spacing.button_padding = vec2(14.0, 6.0);
    s.spacing.interact_size.y = 30.0;
    s.spacing.menu_margin = Margin::same(6);
    s.spacing.window_margin = Margin::same(16);
    s.spacing.scroll.bar_width = 8.0;
    s.spacing.scroll.floating = true;
    s.interaction.tooltip_delay = 0.25;
    s.interaction.show_tooltips_only_when_still = false;
    s.animation_time = 0.16;
    s
}

pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.set_style_of(egui::Theme::Dark, style(true));
    ctx.set_style_of(egui::Theme::Light, style(false));
}

/// Light, dark, or following Windows; also tells Windows, so the title bar
/// matches.
pub fn apply_appearance(ctx: &egui::Context, a: crate::settings::Appearance) {
    use crate::settings::Appearance;
    let (pref, sys) = match a {
        Appearance::System => (egui::ThemePreference::System, egui::SystemTheme::SystemDefault),
        Appearance::Light => (egui::ThemePreference::Light, egui::SystemTheme::Light),
        Appearance::Dark => (egui::ThemePreference::Dark, egui::SystemTheme::Dark),
    };
    ctx.set_theme(pref);
    ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(sys));
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
    let bold = add("ui-bold", &["segoeuib.ttf", "seguisb.ttf"]);
    let symbols = add("symbols", &["seguisym.ttf"]);
    let mono = add("mono", &["CascadiaMono.ttf", "consola.ttf"]);

    let base_prop = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let base_mono = defs.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let with = |first: Option<&str>, rest: &[String]| -> Vec<String> {
        first
            .map(str::to_owned)
            .into_iter()
            .chain(symbols.then(|| "symbols".to_owned()))
            .chain(rest.iter().cloned())
            .collect()
    };
    let prop = with(regular.then_some("ui"), &base_prop);
    let semibold = with(semi.then_some("ui-semibold").or(regular.then_some("ui")), &base_prop);
    let boldf = with(bold.then_some("ui-bold").or(regular.then_some("ui")), &base_prop);
    defs.families.insert(FontFamily::Proportional, prop);
    defs.families.insert(FontFamily::Name("semibold".into()), semibold);
    defs.families.insert(FontFamily::Name("bold".into()), boldf);
    defs.families.insert(FontFamily::Monospace, with(mono.then_some("mono"), &base_mono));
    ctx.set_fonts(defs);
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}

pub fn font(size: f32) -> FontId {
    FontId::proportional(size)
}

/// Draws elided single-line text, vertically centered on `cy`.
pub fn text_at(painter: &Painter, x: f32, cy: f32, text: &str, font: FontId, color: Color32, max_w: f32) {
    if max_w < 4.0 {
        return;
    }
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat { font_id: font, color, ..Default::default() },
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w);
    let g = painter.layout_job(job);
    painter.galley(pos2(x, cy - g.size().y * 0.5), g, color);
}

/// Stand-in icons while the real ones load (or when there isn't one).
pub fn fallback_icon(painter: &Painter, r: Rect, dir: bool, color: Color32) {
    let s = r.width();
    if dir {
        let body = Rect::from_min_max(pos2(r.min.x + s * 0.06, r.min.y + s * 0.26), pos2(r.max.x - s * 0.06, r.max.y - s * 0.14));
        let tab = Rect::from_min_size(pos2(body.min.x, body.min.y - s * 0.1), vec2(s * 0.4, s * 0.16));
        painter.rect_filled(tab, s * 0.06, mix(color, Color32::BLACK, 0.2));
        painter.rect_filled(body, s * 0.08, color);
    } else {
        let body = Rect::from_min_max(pos2(r.min.x + s * 0.18, r.min.y + s * 0.08), pos2(r.max.x - s * 0.18, r.max.y - s * 0.08));
        painter.rect_filled(body, s * 0.08, color);
        let fold = Rect::from_min_size(pos2(body.max.x - s * 0.22, body.min.y), vec2(s * 0.22, s * 0.22));
        painter.rect_filled(fold, s * 0.05, mix(color, Color32::WHITE, 0.4));
        for k in 0..3 {
            let y = body.min.y + s * (0.42 + 0.13 * k as f32);
            painter.line_segment(
                [pos2(body.min.x + s * 0.12, y), pos2(body.max.x - s * 0.12, y)],
                Stroke::new((s * 0.045).max(1.0), mix(color, Color32::WHITE, 0.45)),
            );
        }
    }
}

pub fn magnifier(painter: &Painter, c: Pos2, color: Color32, scale: f32) {
    let stroke = Stroke::new(1.7 * scale.sqrt(), color);
    painter.circle_stroke(c - vec2(1.5, 1.5) * scale, 5.8 * scale, stroke);
    painter.line_segment([c + vec2(2.8, 2.8) * scale, c + vec2(6.8, 6.8) * scale], stroke);
}

/// The filter icon: three sliders.
pub fn sliders(painter: &Painter, c: Pos2, color: Color32, knob_fill: Color32) {
    let stroke = Stroke::new(1.6_f32, color);
    for (dy, kx) in [(-5.0, -3.5), (0.0, 3.5), (5.0, -1.0)] {
        let y = c.y + dy;
        painter.line_segment([pos2(c.x - 8.0, y), pos2(c.x + 8.0, y)], stroke);
        painter.circle(pos2(c.x + kx, y), 2.4, knob_fill, stroke);
    }
}

/// The settings icon: a cog with eight teeth around a hollow hub.
pub fn gear(painter: &Painter, c: Pos2, color: Color32, fill: Color32) {
    let (teeth, outer, inner) = (8, 8.0_f32, 6.2_f32);
    let mut pts = Vec::with_capacity(teeth * 4);
    for k in 0..teeth {
        let a = k as f32 / teeth as f32 * std::f32::consts::TAU;
        let half = std::f32::consts::TAU / teeth as f32 * 0.22;
        for (ang, r) in [(a - half * 1.6, inner), (a - half, outer), (a + half, outer), (a + half * 1.6, inner)] {
            pts.push(c + vec2(ang.cos(), ang.sin()) * r);
        }
    }
    painter.add(egui::Shape::closed_line(pts, Stroke::new(1.5_f32, color)));
    painter.circle(c, 2.6, fill, Stroke::new(1.5_f32, color));
}

/// A ring from `from` to `to` (fractions of a turn, starting at 12 o'clock),
/// shading from `a` to `b` along the way.
pub fn arc(painter: &Painter, c: Pos2, r: f32, from: f32, to: f32, width: f32, a: Color32, b: Color32) {
    let steps = (((to - from).abs() * 96.0).ceil() as usize).max(2);
    let point = |f: f32| {
        let ang = (f - 0.25) * std::f32::consts::TAU;
        c + vec2(ang.cos(), ang.sin()) * r
    };
    for k in 0..steps {
        let (f0, f1) = (from + (to - from) * k as f32 / steps as f32, from + (to - from) * (k + 1) as f32 / steps as f32);
        let color = mix(a, b, k as f32 / steps as f32);
        painter.line_segment([point(f0), point(f1 + 0.002)], Stroke::new(width, color));
    }
}

pub fn bar(painter: &Painter, r: Rect, frac: f32, track: Color32, fill: Color32, fill2: Color32) {
    let radius = r.height() * 0.5;
    painter.rect_filled(r, radius, track);
    let w = (r.width() * frac.clamp(0.0, 1.0)).max(if frac > 0.0 { r.height() } else { 0.0 });
    if w > 0.0 {
        let fr = Rect::from_min_size(r.min, vec2(w, r.height()));
        painter.rect_filled(Rect::from_min_size(fr.min, vec2(r.height(), r.height())), radius, fill);
        // A horizontal gradient for the body of the bar.
        let body = fr.shrink2(vec2(radius, 0.0));
        if body.width() > 0.0 {
            let mut m = egui::Mesh::default();
            let i = m.vertices.len() as u32;
            for (pos, col) in [
                (body.left_top(), fill),
                (body.right_top(), mix(fill, fill2, w / r.width())),
                (body.right_bottom(), mix(fill, fill2, w / r.width())),
                (body.left_bottom(), fill),
            ] {
                m.colored_vertex(pos, col);
            }
            m.add_triangle(i, i + 1, i + 2);
            m.add_triangle(i, i + 2, i + 3);
            painter.add(Shape::mesh(m));
        }
        painter.rect_filled(
            Rect::from_min_size(pos2(fr.max.x - r.height(), fr.min.y), vec2(r.height(), r.height())),
            radius,
            mix(fill, fill2, w / r.width()),
        );
    }
}

/// An on/off switch.
pub fn toggle(ui: &mut Ui, on: bool, p: &Pal) -> Response {
    let (r, resp) = ui.allocate_exact_size(vec2(36.0, 20.0), Sense::click());
    let t = ease(ui.ctx().animate_bool_with_time(resp.id, on, 0.15));
    let painter = ui.painter();
    painter.rect(r, 10.0, mix(p.track, p.accent, t), Stroke::new(1.0_f32, mix(p.border, p.accent, t)), egui::StrokeKind::Inside);
    let x = r.min.x + 10.0 + (r.width() - 20.0) * t;
    painter.circle_filled(pos2(x, r.center().y), 6.5, mix(p.weak, Color32::WHITE, t));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
