//! The floating search bar the hotkey opens, and moving the one window between
//! it, the full app and the tray.
//!
//! The bar opens as just a search line with a few one-click filters, and grows
//! a results list once there's something to show. It can be dragged anywhere
//! (and opens there next time), folded down to the search line, and pinned so
//! it stays open when you click elsewhere.

use super::*;
use egui::{Pos2, ViewportCommand as V};

/// One-click filters under the search line.
#[derive(Clone, Copy)]
enum Quick {
    When(When),
    Size(SizeF),
    Cat(Cat),
    Downloads,
}

const QUICK: [(&str, Quick); 8] = [
    ("Today", Quick::When(When::Day)),
    ("This week", Quick::When(When::Week)),
    ("Over 1 GB", Quick::Size(SizeF::Huge)),
    ("Pictures", Quick::Cat(Cat::Picture)),
    ("Documents", Quick::Cat(Cat::Document)),
    ("Videos", Quick::Cat(Cat::Video)),
    ("Apps", Quick::Cat(Cat::App)),
    ("Downloads", Quick::Downloads),
];

impl App {
    /// The hotkey and the tray icon.
    pub(super) fn signals(&mut self, ctx: &egui::Context) {
        if std::mem::take(&mut self.bar_next) {
            self.show_bar(ctx);
        }
        let Some(rx) = &self.signals else { return };
        let got: Vec<Signal> = rx.try_iter().collect();
        for s in got {
            trace::log(format!("{} while {:?}", s.name(), self.mode));
            match s {
                Signal::Hotkey => match self.mode {
                    Mode::Bar if self.bar_collapsed => self.expand_bar(ctx),
                    Mode::Bar => self.hide(ctx),
                    Mode::Hidden => self.show_bar(ctx),
                    // The full app is open: bring it forward, ready to type.
                    Mode::Full => {
                        ctx.send_viewport_cmd(V::Minimized(false));
                        ctx.send_viewport_cmd(V::Focus);
                        self.focus = true;
                    }
                },
                Signal::Bar if self.mode != Mode::Bar => self.show_bar(ctx),
                Signal::Bar => {}
                Signal::Open => self.show_full(ctx),
                Signal::Quit => self.quit(ctx),
                Signal::Analyze(path) => {
                    self.show_full(ctx);
                    self.analyze(ctx, path);
                }
                Signal::ColumnPrefs => {
                    self.settings.explorer_column = crate::column::ON.load(Relaxed);
                    self.settings.column_share = crate::column::SHARE.load(Relaxed);
                    self.settings.column_week = crate::column::WEEK.load(Relaxed);
                    self.settings.column_by_share = crate::column::BY_SHARE.load(Relaxed);
                    self.settings.save();
                }
            }
        }
    }

    /// Closing the window leaves the app in the tray (unless that's switched off).
    pub(super) fn closing(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        if tray::QUITTING.load(Relaxed) || !self.settings.background || self.signals.is_none() {
            return;
        }
        ctx.send_viewport_cmd(V::CancelClose);
        self.hide(ctx);
        if !self.settings.told {
            self.settings.told = true;
            self.settings.save();
            let name = edition::get().name;
            let key = self.settings.hotkey;
            tray::balloon(
                &format!("{name} is still running"),
                &format!("Press {key} to search from anywhere. To quit, right-click the icon in the tray."),
            );
        }
    }

    /// Saves what needs saving and exits. (Closing the window the usual way takes
    /// more frames than a hidden window gets.)
    pub(super) fn quit(&mut self, _ctx: &egui::Context) {
        tray::QUITTING.store(true, Relaxed);
        tray::remove();
        self.close_space();
        self.shared.save_walked();
        std::process::exit(0);
    }

    fn hide(&mut self, ctx: &egui::Context) {
        if self.mode == Mode::Bar && !self.bar_collapsed {
            // It opens where it was left.
            if let Some(r) = ctx.input(|i| i.viewport().outer_rect) {
                self.settings.bar_pos = Some([r.min.x, r.min.y]);
                self.settings.save();
            }
        }
        ctx.send_viewport_cmd(V::Visible(false));
        self.mode = Mode::Hidden;
        self.release_space();
        self.show_wait = 0;
    }

    /// The window's size for what it's showing: the floating button, the search
    /// line with its quick filters, or results too.
    fn bar_size(&self) -> Vec2 {
        if self.bar_collapsed {
            Vec2::splat(BUBBLE)
        } else if !self.active() {
            vec2(BAR_W, BAR_LINE + BAR_CHIPS)
        } else {
            vec2(BAR_W, BAR_LINE + BAR_CHIPS + BAR_ROWS as f32 * BAR_ROW_H + 8.0 + BAR_FOOT)
        }
    }

    /// Turns the window into the search bar. It's kept out of sight (cloaked)
    /// while it changes shape, and shows a couple of frames later, already drawn.
    /// (No tray::poke here: its size report could land after the new size and undo it.)
    fn show_bar(&mut self, ctx: &egui::Context) {
        if self.mode != Mode::Hidden {
            // It gets its taskbar button (or not) as it's shown: hide it first.
            self.hide(ctx);
            self.bar_next = true;
            tray::poke();
            return;
        }
        win::cloak(self.hwnd, true);
        // No taskbar button, set before it's shown (see win::set_tool_window).
        win::set_tool_window(self.hwnd, true);
        // A fresh search each time.
        self.query.clear();
        self.cat = Cat::All;
        self.filters = Filters::default();
        self.filters_open = false;
        self.selected = None;
        self.settings_open = false;
        self.bar_collapsed = false;
        let size = self.bar_size();
        self.bar_dims = size;
        ctx.send_viewport_cmd(V::Visible(true));
        ctx.send_viewport_cmd(V::Maximized(false));
        ctx.send_viewport_cmd(V::Decorations(false));
        ctx.send_viewport_cmd(V::MinInnerSize(Vec2::splat(BUBBLE)));
        ctx.send_viewport_cmd(V::InnerSize(size));
        ctx.send_viewport_cmd(V::WindowLevel(egui::WindowLevel::AlwaysOnTop));
        let ppp = ctx.pixels_per_point();
        let saved = self.settings.bar_pos.filter(|&[x, y]| win::on_screen((x * ppp) as i32 + 40, (y * ppp) as i32 + 20));
        let pos = saved.map(|[x, y]| pos2(x, y)).or_else(|| {
            let [l, t, r, b] = win::work_area_at_cursor()?;
            let (l, t, r, b) = (l as f32 / ppp, t as f32 / ppp, r as f32 / ppp, b as f32 / ppp);
            Some(pos2((l + r - size.x) / 2.0, t + (b - t) * 0.2))
        });
        if let Some(pos) = pos {
            ctx.send_viewport_cmd(V::OuterPosition(pos));
        }
        self.mode = Mode::Bar;
        self.show_wait = 2;
        self.bar_sel = 0;
        self.bar_focused = false;
        self.focus = true;
        ctx.request_repaint();
    }

    /// Back to the full app, keeping what was typed in the bar.
    pub(super) fn show_full(&mut self, ctx: &egui::Context) {
        let from = self.mode;
        self.mode = Mode::Full;
        self.focus = true;
        if from == Mode::Full {
            ctx.send_viewport_cmd(V::Minimized(false));
            ctx.send_viewport_cmd(V::Focus);
            return;
        }
        win::cloak(self.hwnd, true);
        // Hidden and shown again, so the taskbar gives it its button back.
        ctx.send_viewport_cmd(V::Visible(false));
        ctx.send_viewport_cmd(V::Visible(true));
        ctx.send_viewport_cmd(V::WindowLevel(egui::WindowLevel::Normal));
        ctx.send_viewport_cmd(V::Decorations(true));
        ctx.send_viewport_cmd(V::MinInnerSize(MIN_SIZE.into()));
        // From the bar it needs its normal size back; resizing a maximized window
        // would leave it marked maximized but not filling the screen.
        if !ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
            ctx.send_viewport_cmd(V::InnerSize(vec2(1240.0, 800.0)));
        }
        ctx.send_viewport_cmd(V::Maximized(true));
        self.show_wait = 3;
        ctx.request_repaint();
    }

    /// Uncloaks the window once it has been drawn in its new shape.
    pub(super) fn appear_when_ready(&mut self, ctx: &egui::Context) {
        if self.show_wait == 0 {
            return;
        }
        self.show_wait -= 1;
        if self.show_wait > 0 {
            ctx.request_repaint();
            return;
        }
        if self.mode == Mode::Bar {
            win::round_corners(self.hwnd);
        }
        // Showing it reset its styles; floating, it stays out of Alt+Tab.
        if self.mode == Mode::Bar {
            win::set_tool_window(self.hwnd, true);
        }
        win::cloak(self.hwnd, false);
        trace::log(format!("shown as {:?}, drawing at {:?}", self.mode, ctx.screen_rect().size()));
        ctx.send_viewport_cmd(V::Focus);
        self.bar_at = Instant::now();
        ctx.request_repaint();
    }

    /// Moves the window with the mouse. Done here rather than by Windows, whose
    /// own window dragging offers snap layouts: wrong for something floating.
    fn drag_window(&mut self, ctx: &egui::Context, r: &Response) {
        if r.drag_started() {
            let at = ctx.input(|i| i.viewport().outer_rect).map(|r| r.min);
            self.drag_from = at.map(|p| (win::cursor_pos(), p));
        }
        let Some(((x0, y0), start)) = self.drag_from else { return };
        if r.dragged() {
            let (x, y) = win::cursor_pos();
            let ppp = ctx.pixels_per_point();
            ctx.send_viewport_cmd(V::OuterPosition(start + vec2((x - x0) as f32, (y - y0) as f32) / ppp));
        } else {
            self.drag_from = None;
        }
    }

    /// Shrinks the bar to a floating button where its shrink button was.
    fn collapse_bar(&mut self, ctx: &egui::Context) {
        let Some(r) = ctx.input(|i| i.viewport().outer_rect) else { return };
        self.settings.bar_pos = Some([r.min.x, r.min.y]);
        self.settings.save();
        self.bar_collapsed = true;
        self.bar_dims = self.bar_size();
        ctx.send_viewport_cmd(V::OuterPosition(pos2(r.max.x - BUBBLE - 8.0, r.min.y + 4.0)));
        ctx.send_viewport_cmd(V::InnerSize(self.bar_dims));
        ctx.request_repaint();
    }

    /// The floating button opens back into the bar, growing to its left, on screen.
    fn expand_bar(&mut self, ctx: &egui::Context) {
        self.bar_collapsed = false;
        let size = self.bar_size();
        if let Some(r) = ctx.input(|i| i.viewport().outer_rect) {
            let ppp = ctx.pixels_per_point();
            let mut x = r.max.x + 8.0 - size.x;
            if let Some([l, _, rt, _]) = win::work_area_at_cursor() {
                let (l, rt) = (l as f32 / ppp, rt as f32 / ppp);
                x = x.clamp(l, (rt - size.x).max(l));
            }
            ctx.send_viewport_cmd(V::OuterPosition(pos2(x, r.min.y - 4.0)));
        }
        self.bar_dims = size;
        ctx.send_viewport_cmd(V::InnerSize(size));
        ctx.send_viewport_cmd(V::Focus);
        self.focus = true;
        self.bar_focused = false;
        ctx.request_repaint();
    }

    /// Shrunk to a floating button: click to search, drag to move, right-click to close.
    fn bubble(&mut self, ctx: &egui::Context) {
        let p = theme::pal(ctx.style().visuals.dark_mode);
        let (mut open, mut close) = (false, false);
        egui::CentralPanel::default()
            .frame(Frame::new().fill(p.chrome).stroke(Stroke::new(1.0_f32, p.border)))
            .show(ctx, |ui| {
                let r = ui.max_rect();
                let resp = ui.interact(r, Id::new("bubble"), Sense::click_and_drag()).on_hover_cursor(CursorIcon::PointingHand);
                self.drag_window(ui.ctx(), &resp);
                let ht = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
                if ht > 0.0 {
                    ui.painter().rect_stroke(r.shrink(1.0), 8.0, Stroke::new(2.0_f32, fade(p.accent, ht)), StrokeKind::Inside);
                }
                let logo = Rect::from_center_size(r.center(), Vec2::splat(32.0 + 3.0 * ht));
                ui.painter().image(self.logo.id(), logo, uv(), Color32::WHITE);
                open = resp.clicked();
                close = resp.secondary_clicked();
            });
        if close {
            self.hide(ctx);
        } else if open {
            self.expand_bar(ctx);
        }
    }

    /// Matching apps first, then files.
    fn bar_items(&self) -> Vec<BarItem> {
        if !self.active() {
            return Vec::new();
        }
        let apps = self.app_hits.iter().take(if self.show_apps() { 3 } else { 0 }).cloned().map(BarItem::App);
        let files = self.visible.iter().map(|v| BarItem::Hit(self.hits[v.hit as usize]));
        apps.chain(files).take(BAR_ROWS).collect()
    }

    fn quick_on(&self, q: Quick) -> bool {
        match q {
            Quick::When(w) => self.filters.when == w,
            Quick::Size(s) => self.filters.size == s,
            Quick::Cat(c) => self.cat == c,
            Quick::Downloads => self.filters.folder.as_deref() == Some(&self.downloads()),
        }
    }

    fn quick_toggle(&mut self, q: Quick) {
        let on = self.quick_on(q);
        match q {
            Quick::When(w) => self.filters.when = if on { When::Any } else { w },
            Quick::Size(s) => self.filters.size = if on { SizeF::Any } else { s },
            Quick::Cat(c) => self.cat = if on { Cat::All } else { c },
            Quick::Downloads => self.filters.folder = (!on).then(|| self.downloads()),
        }
        self.focus = true;
    }

    fn downloads(&self) -> String {
        format!("{}\\Downloads", self.home)
    }

    pub(super) fn bar(&mut self, ctx: &egui::Context) {
        let p = theme::pal(ctx.style().visuals.dark_mode);
        self.icons.poll(ctx);
        self.poll();
        // Clicking anywhere else puts it away, like the Start menu (unless pinned).
        if self.show_wait == 0 {
            if ctx.input(|i| i.viewport().focused.unwrap_or(false)) {
                self.bar_focused = true;
            } else if self.bar_focused && !self.settings.bar_pinned && !self.bar_collapsed {
                self.hide(ctx);
                return;
            }
        }
        if self.query != self.bar_query {
            self.bar_query = self.query.clone();
            self.bar_sel = 0;
        }
        // Grow and shrink with what there is to show.
        let size = self.bar_size();
        if size != self.bar_dims {
            self.bar_dims = size;
            ctx.send_viewport_cmd(V::InnerSize(size));
        }
        if self.bar_collapsed {
            self.bubble(ctx);
            return;
        }
        let expanded = self.active();
        let items = if expanded { self.bar_items() } else { Vec::new() };
        let n = items.len();
        let (down, up, enter, reveal, tab, esc) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::COMMAND, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Tab),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        if esc {
            // Pinned, Esc clears first; the hotkey or the close button puts it away.
            if self.settings.bar_pinned && self.active() {
                self.query.clear();
                self.cat = Cat::All;
                self.filters = Filters::default();
            } else {
                self.hide(ctx);
                return;
            }
        }
        if tab {
            self.show_full(ctx);
            return;
        }
        if down {
            self.bar_sel += 1;
        }
        if up {
            self.bar_sel = self.bar_sel.saturating_sub(1);
        }
        self.bar_sel = self.bar_sel.min(n.saturating_sub(1));
        let mut pick = ((enter || reveal) && n > 0).then_some((self.bar_sel, reveal));
        let mut full = false;
        let mut close = false;
        let mut shrink = false;
        let mut quick = None;

        let a = ease(self.bar_at.elapsed().as_secs_f32() / 0.15);
        if a < 1.0 {
            ctx.request_repaint();
        }
        egui::CentralPanel::default()
            .frame(Frame::new().fill(p.chrome).stroke(Stroke::new(1.0_f32, p.border)))
            .show(ctx, |ui| {
                ui.multiply_opacity(0.6 + 0.4 * a);
                // Everything is placed exactly; default gaps would push things off the bottom.
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                let whole = ui.max_rect();
                let w = whole.width();
                let painter = ui.painter().clone();

                // The search line. Its empty parts (and the filters' row) move the bar.
                let (row, _) = ui.allocate_exact_size(vec2(w, BAR_LINE), Sense::hover());
                let chips_shown = expanded || !self.active();
                let grab = Rect::from_min_max(whole.min, pos2(whole.max.x, row.max.y + if chips_shown { BAR_CHIPS } else { 0.0 }));
                let drag = ui.interact(grab, Id::new("bar-drag"), Sense::click_and_drag());
                self.drag_window(ui.ctx(), &drag);
                theme::magnifier(&painter, pos2(row.min.x + 28.0, row.center().y), p.accent, 1.2);

                // On the right: close, pin, fold, and the way into the full app.
                let cy = row.center().y;
                let mut x = row.max.x - 12.0;
                let mut icon_button = |ui: &mut Ui, id: &str, tip: &str, on: bool, draw: &dyn Fn(&Painter, Pos2, Color32)| {
                    x -= 30.0;
                    let r = Rect::from_center_size(pos2(x + 15.0, cy), vec2(28.0, 28.0));
                    let b = ui.interact(r, Id::new(id), Sense::click()).on_hover_cursor(CursorIcon::PointingHand).on_hover_text(tip);
                    if on {
                        painter.rect_filled(r, 8.0, fade(p.accent, 0.22));
                    } else if b.hovered() {
                        painter.rect_filled(r, 8.0, p.hover);
                    }
                    draw(&painter, r.center(), if on { p.accent } else if b.hovered() { p.text } else { p.weak });
                    x -= 2.0;
                    b.clicked()
                };
                close = icon_button(ui, "bar-close", "Close (Esc)", false, &|pt, c, color| {
                    let s = 4.5;
                    pt.line_segment([c - vec2(s, s), c + vec2(s, s)], Stroke::new(1.5_f32, color));
                    pt.line_segment([c + vec2(-s, s), c + vec2(s, -s)], Stroke::new(1.5_f32, color));
                });
                let pinned = self.settings.bar_pinned;
                let pin_tip = if pinned { "Pinned: stays open when you click elsewhere" } else { "Pin: keep it open when you click elsewhere" };
                if icon_button(ui, "bar-pin", pin_tip, pinned, &pin_glyph) {
                    self.settings.bar_pinned = !pinned;
                    self.settings.save();
                }
                if icon_button(ui, "bar-shrink", "Shrink to a floating button", false, &shrink_glyph) {
                    shrink = true;
                }
                let label = format!("Open {}", edition::get().name);
                let tw = painter.layout_no_wrap(label.clone(), theme::font(12.5), p.text).size().x;
                let bw = 12.0 + 18.0 + 8.0 + tw + 10.0 + 34.0 + 8.0;
                let br = Rect::from_min_size(pos2(x - 6.0 - bw, cy - 16.0), vec2(bw, 32.0));
                let b = ui.interact(br, Id::new("bar-full"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
                let b = b.on_hover_text("The full app, with filters and previews (Tab)");
                let ht = ui.ctx().animate_bool_with_time(b.id, b.hovered(), 0.12);
                painter.rect(br, 16.0, mix(p.raised, p.raised_hover, ht), Stroke::new(1.0_f32, mix(p.border, p.accent, ht)), StrokeKind::Inside);
                let logo = Rect::from_center_size(pos2(br.min.x + 21.0, cy), vec2(18.0, 18.0));
                painter.image(self.logo.id(), logo, uv(), Color32::WHITE);
                painter.text(pos2(br.min.x + 38.0, cy), Align2::LEFT_CENTER, &label, theme::font(12.5), p.text);
                let cap = Rect::from_min_size(pos2(br.max.x - 42.0, cy - 10.0), vec2(34.0, 20.0));
                painter.rect(cap, 5.0, p.field, Stroke::new(1.0_f32, p.border), StrokeKind::Inside);
                painter.text(cap.center(), Align2::CENTER_CENTER, "Tab", theme::font(10.5), p.weak);
                full = b.clicked();

                let count = if !self.active() {
                    String::new()
                } else if self.searching && self.hits.is_empty() {
                    "Searching…".to_string()
                } else {
                    format!("{} results", fmt::count(self.counts[0]))
                };
                let cw = painter.layout_no_wrap(count.clone(), theme::font(12.0), p.faint).size().x;
                painter.text(pos2(br.min.x - 12.0, cy), Align2::RIGHT_CENTER, count, theme::font(12.0), p.faint);
                let text_rect = Rect::from_min_max(pos2(row.min.x + 52.0, row.min.y), pos2(br.min.x - 24.0 - cw, row.max.y));
                let resp = ui.put(
                    text_rect,
                    TextEdit::singleline(&mut self.query)
                        .id(Id::new("bar-search"))
                        .frame(false)
                        .font(theme::font(19.0))
                        .text_color(p.text)
                        .hint_text(RichText::new("Search files, folders and apps").color(p.faint))
                        .margin(Margin::symmetric(0, 17))
                        .desired_width(text_rect.width()),
                );
                // Typing always goes to the search line.
                if std::mem::take(&mut self.focus) || !resp.has_focus() {
                    resp.request_focus();
                }

                // Quick filters
                if chips_shown {
                    let chips = Rect::from_min_size(pos2(whole.min.x, row.max.y), vec2(w, BAR_CHIPS));
                    let mut cx = chips.min.x + 16.0;
                    for (label, q) in QUICK {
                        let dot = match q {
                            Quick::Cat(c) => Some(theme::cat_color(Some(c), p.dark)),
                            _ => None,
                        };
                        let lw = painter.layout_no_wrap(label.to_string(), theme::font(12.5), p.text).size().x;
                        let cw = 24.0 + if dot.is_some() { 13.0 } else { 0.0 } + lw;
                        if cx + cw > chips.max.x - 12.0 {
                            break;
                        }
                        let r = Rect::from_min_size(pos2(cx, chips.min.y + 4.0), vec2(cw, 28.0));
                        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(r));
                        if chip(&mut child, Id::new(("quick", label)), label, None, dot, self.quick_on(q), 28.0, &p).clicked() {
                            quick = Some(q);
                        }
                        cx += cw + 6.0;
                    }
                    if expanded {
                        painter.line_segment([pos2(whole.min.x, chips.max.y), pos2(whole.max.x, chips.max.y)], Stroke::new(1.0_f32, p.divider));
                    }
                }
                if !expanded {
                    return;
                }

                // Results
                let shared = self.shared.clone();
                let ix = shared.read();
                let moved = ui.input(|i| i.pointer.delta() != Vec2::ZERO);
                let top = row.max.y + BAR_CHIPS + 4.0;
                if items.is_empty() {
                    let msg = if self.searching { "" } else { "Nothing matches. Press Tab to search with more filters." };
                    painter.text(pos2(whole.center().x, top + 50.0), Align2::CENTER_CENTER, msg, theme::font(13.0), p.faint);
                }
                for (k, item) in items.iter().enumerate() {
                    let r = Rect::from_min_size(pos2(whole.min.x, top + k as f32 * BAR_ROW_H), vec2(w, BAR_ROW_H));
                    let resp = ui.interact(r, Id::new(("bar-row", k)), Sense::click());
                    if resp.hovered() && moved {
                        self.bar_sel = k;
                    }
                    let sel = k == self.bar_sel;
                    row_bg(&painter, r.shrink2(vec2(6.0, 0.0)), sel, resp.hovered(), &p);
                    let (title, sub, tex, dir, cat) = match item {
                        BarItem::App(app) => (
                            app.name.clone(),
                            app.kind.label().to_string(),
                            self.icons.icon(&apps::icon_key(app), || apps::icon_path(app)),
                            false,
                            Some(Cat::App),
                        ),
                        BarItem::Hit(h) => {
                            let Some(v) = ix.vol(h.vol as char) else { continue };
                            let e = v.entry(h.idx);
                            if e.kind == NONE {
                                continue;
                            }
                            (
                                v.name(h.idx).to_string(),
                                pretty_path(&v.path(e.parent), &self.home),
                                entry_icon(&mut self.icons, v, h.idx, &e),
                                e.kind == DIR,
                                search::entry_cat(v, h.idx, &e),
                            )
                        }
                    };
                    let cy = r.center().y;
                    let ir = Rect::from_center_size(pos2(r.min.x + 32.0, cy), vec2(26.0, 26.0));
                    icon_or_fallback(&painter, ir, tex, dir, theme::cat_color(cat, p.dark), 1.0);
                    let tw = w - 180.0;
                    let g = painter.layout_job(name_job(&title, self.highlight.as_deref(), p.text, p.accent, tw));
                    painter.galley(pos2(r.min.x + 58.0, cy - 1.0 - g.size().y), g, p.text);
                    theme::text_at(&painter, r.min.x + 58.0, cy + 9.0, &sub, theme::font(11.5), p.faint, tw);
                    if sel {
                        let hint = if dir { "Open folder  ↵" } else { "Open  ↵" };
                        painter.text(pos2(r.max.x - 20.0, cy), Align2::RIGHT_CENTER, hint, theme::font(11.5), p.weak);
                    }
                    if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                        pick = Some((k, false));
                    }
                }
                drop(ix);

                // Footer, pinned to the bottom edge.
                let foot = Rect::from_min_max(pos2(whole.min.x, whole.max.y - BAR_FOOT), whole.max);
                painter.line_segment([foot.left_top(), foot.right_top()], Stroke::new(1.0_f32, p.divider));
                let hints = "↑↓ move     ↵ open     Ctrl+↵ show in folder     Tab full app     Esc close";
                painter.text(pos2(foot.min.x + 20.0, foot.center().y), Align2::LEFT_CENTER, hints, theme::font(11.5), p.faint);
            });

        if let Some(q) = quick {
            self.quick_toggle(q);
        }
        if shrink {
            self.collapse_bar(ctx);
        } else if close {
            self.hide(ctx);
        } else if full {
            self.show_full(ctx);
        } else if let Some((k, reveal)) = pick {
            if let Some(item) = items.into_iter().nth(k) {
                self.bar_open(ctx, item, reveal);
            }
        }
        self.ask_if_needed(ctx);
    }

    /// Opens what was picked, and puts the bar away unless it's pinned.
    fn bar_open(&mut self, ctx: &egui::Context, item: BarItem, reveal: bool) {
        if !self.settings.bar_pinned {
            self.hide(ctx);
        }
        let act = match item {
            BarItem::App(app) => Act::Launch(app),
            BarItem::Hit(h) if reveal => Act::Reveal(h),
            BarItem::Hit(h) => Act::Open(h),
        };
        self.act(ctx, act);
    }
}

/// A push pin, tilted.
fn pin_glyph(pt: &Painter, c: Pos2, color: Color32) {
    let head = c + vec2(2.5, -2.5);
    let dir = vec2(-0.7071, 0.7071);
    let across = vec2(0.7071, 0.7071);
    pt.line_segment([head - across * 4.5, head + across * 4.5], Stroke::new(2.0_f32, color));
    pt.line_segment([head - dir * 3.5 - across * 2.5, head - dir * 3.5 + across * 2.5], Stroke::new(3.0_f32, color));
    pt.line_segment([head, head + dir * 8.0], Stroke::new(1.5_f32, color));
}

/// Two corners pointing inward: shrink.
fn shrink_glyph(pt: &Painter, c: Pos2, color: Color32) {
    let s = Stroke::new(1.5_f32, color);
    pt.line_segment([c + vec2(1.5, -5.5), c + vec2(1.5, -1.5)], s);
    pt.line_segment([c + vec2(1.5, -1.5), c + vec2(5.5, -1.5)], s);
    pt.line_segment([c + vec2(-1.5, 5.5), c + vec2(-1.5, 1.5)], s);
    pt.line_segment([c + vec2(-1.5, 1.5), c + vec2(-5.5, 1.5)], s);
}