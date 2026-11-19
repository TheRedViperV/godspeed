//! The home screen, all four ideas in one: a launchpad of what you open most,
//! a bento of numbers, your activity week by week, and a map of where you work.

use super::*;
use crate::search::WEEKS;

const GAP: f32 = 14.0;

/// "Good evening", by the clock on this PC.
fn greeting() -> &'static str {
    const EPOCH: u64 = 11_644_473_600; // 1601 to 1970, in seconds
    let utc = (index::now() as u64 + EPOCH) * 10_000_000;
    let local = (win::to_local_filetime(utc) / 10_000_000).saturating_sub(EPOCH);
    match (local % 86_400) / 3600 {
        5..=11 => "Good morning",
        12..=16 => "Good afternoon",
        17..=21 => "Good evening",
        _ => "Hello, night owl",
    }
}

fn panel(painter: &Painter, r: Rect, p: &Pal) {
    painter.rect(r, 16.0, p.raised, Stroke::new(1.0_f32, p.border), StrokeKind::Inside);
}

/// A card's heading, with an optional note on the right.
fn heading(painter: &Painter, r: Rect, title: &str, note: &str, p: &Pal) {
    painter.text(pos2(r.min.x + 16.0, r.min.y + 22.0), Align2::LEFT_CENTER, title, theme::semibold(14.0), p.text);
    if !note.is_empty() {
        painter.text(pos2(r.max.x - 16.0, r.min.y + 22.0), Align2::RIGHT_CENTER, note, theme::font(11.5), p.faint);
    }
}

/// Colors for the folder map, taken in turn.
fn bubble_color(i: usize, dark: bool) -> Color32 {
    const CATS: [Cat; 7] = [Cat::Document, Cat::Picture, Cat::Folder, Cat::Archive, Cat::Video, Cat::App, Cat::Audio];
    theme::cat_color(Some(CATS[i % CATS.len()]), dark)
}

impl App {
    pub(super) fn landing(&mut self, ui: &mut Ui, p: &Pal) -> Option<Act> {
        let mut act = None;
        let k = ease(self.opened_at.elapsed().as_secs_f32() / 1.0);
        if k < 1.0 {
            ui.ctx().request_repaint();
        }
        if self.space_at.is_none_or(|t| t.elapsed() > Duration::from_secs(10)) {
            // Not for disconnected network drives: asking can hang for seconds.
            let letters: Vec<char> = self.shared.read().vols.iter().filter(|v| !v.offline).map(|v| v.letter).collect();
            self.space = letters.into_iter().filter_map(|l| Some((l, win::disk_space(l)?))).collect();
            self.space_at = Some(Instant::now());
        }
        let stats = lock(&self.stats).clone();
        let shared = self.shared.clone();
        let ix = shared.read();

        ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            let avail = ui.available_width();
            let col = (avail - 48.0).clamp(300.0, 1080.0);
            let margin = ((avail - col) * 0.5).max(0.0);
            ui.horizontal(|ui| {
                ui.add_space(margin);
                ui.vertical(|ui| {
                    ui.set_width(col);
                    ui.add_space(22.0);
                    self.hero(ui, p, &ix, k);
                    ui.add_space(20.0);
                    if let Some(a) = self.launchpad(ui, p, k) {
                        act = Some(a);
                    }
                    ui.add_space(GAP);
                    if let Some(a) = self.bento(ui, p, &stats, &ix, col, k) {
                        act = Some(a);
                    }
                    ui.add_space(GAP);
                    if let Some(a) = self.activity(ui, p, &stats, &ix, col, k) {
                        act = Some(a);
                    }
                    ui.add_space(GAP + 4.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
                        ui.label(RichText::new("Try").size(12.5).color(p.faint));
                        for (ex, what) in TIPS {
                            if tip(ui, ex, what, p).clicked() {
                                act = Some(Act::Query(ex.to_string()));
                            }
                        }
                    });
                    ui.add_space(24.0);
                });
            });
        });
        act
    }

    /// Greeting, how much is indexed, and the hotkey.
    fn hero(&mut self, ui: &mut Ui, p: &Pal, ix: &index::Index, k: f32) {
        let items = ix.items();
        let drives = ix.vols.len();
        let live = ix.vols.iter().any(|v| v.live);
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(72.0, 72.0), Sense::hover());
            let s = 0.85 + 0.15 * k;
            ui.painter().image(self.logo_big.id(), Rect::from_center_size(r.center(), vec2(64.0, 64.0) * s), uv(), fade(Color32::WHITE, k));
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.add_space(8.0);
                ui.label(RichText::new(greeting()).font(theme::bold(26.0)).color(p.text));
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let shown = (items as f64 * k as f64) as u64;
                    let ds = if drives == 1 { "drive" } else { "drives" };
                    let text = format!("{} files and folders on {drives} {ds}", fmt::count(shown));
                    ui.label(RichText::new(text).size(14.0).color(p.weak));
                    if live {
                        ui.label(RichText::new("·").color(p.faint));
                        live_dot(ui, p, self.pulse_at);
                        ui.label(RichText::new("updating live").size(14.0).color(p.weak));
                    }
                });
            });
            if self.signals.is_some() {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let text = format!("⌨  {}  search from anywhere", self.settings.hotkey);
                    let b = egui::Button::new(RichText::new(text).size(12.5).color(p.weak))
                        .fill(p.raised)
                        .stroke(Stroke::new(1.0_f32, p.border))
                        .corner_radius(17.0)
                        .min_size(vec2(0.0, 34.0));
                    let b = ui.add(b).on_hover_text("Opens a search bar over any app. Click to change it.");
                    if b.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                        self.settings_open = true;
                    }
                });
            }
        });
    }

    /// Big tiles for what you open most.
    fn launchpad(&mut self, ui: &mut Ui, p: &Pal, k: f32) -> Option<Act> {
        let mut act = None;
        let frequent = lock(&self.history).top(8);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Jump back in").font(theme::semibold(14.0)).color(p.text));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new("What you open most").size(11.5).color(p.faint));
            });
        });
        ui.add_space(8.0);
        if frequent.is_empty() {
            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 64.0), Sense::hover());
            let painter = ui.painter();
            painter.rect(r, 14.0, Color32::TRANSPARENT, Stroke::new(1.0_f32, p.border), StrokeKind::Inside);
            let text = "Files, folders and apps you open from here show up as tiles";
            painter.text(r.center(), Align2::CENTER_CENTER, text, theme::font(12.5), p.faint);
            return None;
        }
        let apps = lock(&self.apps);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(10.0, 10.0);
            let w = ((ui.available_width() - 10.0 * 7.0) / 8.0).clamp(92.0, 140.0);
            for (i, (key, path)) in frequent.iter().enumerate() {
                let app = key.strip_prefix("app:").and_then(|n| apps.iter().find(|a| a.lower == n));
                if key.starts_with("app:") && app.is_none() {
                    continue; // not loaded yet, or uninstalled
                }
                let label = app.map_or_else(|| short_dir(path).to_string(), |a| a.name.clone());
                let tex = match app {
                    Some(a) => self.icons.icon(&apps::icon_key(a), || apps::icon_path(a)),
                    None => self.icons.icon(&format!("path:{key}"), || path.clone()),
                };
                let (r, resp) = ui.allocate_exact_size(vec2(w, 96.0), Sense::click());
                let resp = resp.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(path.as_str());
                let ht = ease(ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.14));
                let a = ease(k * 1.4 - i as f32 * 0.06);
                let card = r.translate(vec2(0.0, (1.0 - a) * 8.0 - ht * 2.0));
                let painter = ui.painter();
                painter.rect(
                    card,
                    14.0,
                    fade(mix(p.raised, p.raised_hover, ht), a),
                    Stroke::new(1.0_f32, fade(mix(p.border, p.accent, ht * 0.6), a)),
                    StrokeKind::Inside,
                );
                let ir = Rect::from_center_size(pos2(card.center().x, card.min.y + 34.0), vec2(34.0, 34.0));
                icon_or_fallback(painter, ir, tex, app.is_none(), theme::cat_color(None, p.dark), a);
                let mut job = LayoutJob::simple(label, theme::font(12.0), fade(p.text, a), w - 16.0);
                job.halign = Align::Center;
                job.wrap.max_rows = 2;
                let g = painter.layout_job(job);
                painter.galley(pos2(card.center().x, card.min.y + 60.0), g, p.text);
                if resp.clicked() {
                    act = Some(match app {
                        Some(a) => Act::Launch(a.clone()),
                        None => Act::OpenPath(path.clone()),
                    });
                }
            }
        });
        act
    }

    /// What's indexed, what changed today, the biggest new file, and the drives.
    fn bento(&mut self, ui: &mut Ui, p: &Pal, stats: &Stats, ix: &index::Index, col: f32, k: f32) -> Option<Act> {
        let mut act = None;
        let h = 244.0;
        let (area, _) = ui.allocate_exact_size(vec2(col, h), Sense::hover());
        let painter = ui.painter().clone();
        let lw = (col - GAP) * 0.36;
        let left = Rect::from_min_size(area.min, vec2(lw, h));
        let rx = left.max.x + GAP;
        let rw = area.max.x - rx;
        let (th, tw) = ((h - GAP) / 2.0, (rw - GAP) / 2.0);
        let today = Rect::from_min_size(pos2(rx, area.min.y), vec2(tw, th));
        let big = Rect::from_min_size(pos2(rx + tw + GAP, area.min.y), vec2(tw, th));
        let disks = Rect::from_min_size(pos2(rx, area.min.y + th + GAP), vec2(rw, th));

        // What's indexed: a ring by kind, with a legend.
        panel(&painter, left, p);
        heading(&painter, left, "What's indexed", "", p);
        let total = stats.counts[0].max(1) as f32;
        let known: u64 = stats.counts[1..].iter().sum();
        let rows: Vec<(&str, u64, Color32)> = Cat::ALL[1..]
            .iter()
            .map(|&c| (c.label(), stats.counts[c.index()], theme::cat_color(Some(c), p.dark)))
            .chain(std::iter::once(("Other", stats.counts[0].saturating_sub(known), theme::cat_color(None, p.dark))))
            .collect();
        let rad = ((lw - 32.0) * 0.21).clamp(34.0, 58.0);
        let c = pos2(left.min.x + 20.0 + rad, left.min.y + 44.0 + (h - 44.0) * 0.5 - 4.0);
        painter.circle_stroke(c, rad, Stroke::new(16.0_f32, p.track));
        let mut f = 0.0;
        for &(_, n, color) in &rows {
            let frac = n as f32 / total * k;
            if frac > 0.004 {
                theme::arc(&painter, c, rad, f, f + frac - 0.004, 16.0, color, color);
            }
            f += frac;
        }
        painter.text(c - vec2(0.0, 7.0), Align2::CENTER_CENTER, fmt::count(stats.counts[0]), theme::bold(15.0), p.text);
        painter.text(c + vec2(0.0, 11.0), Align2::CENTER_CENTER, "items", theme::font(11.0), p.faint);
        let lx = c.x + rad + 24.0;
        let step = ((h - 60.0) / rows.len() as f32).min(24.0);
        let ly = c.y - step * (rows.len() as f32 - 1.0) * 0.5;
        for (i, (label, n, color)) in rows.iter().enumerate() {
            let y = ly + i as f32 * step;
            painter.circle_filled(pos2(lx + 4.0, y), 3.5, *color);
            theme::text_at(&painter, lx + 14.0, y, label, theme::font(12.0), p.weak, left.max.x - lx - 80.0);
            painter.text(pos2(left.max.x - 16.0, y), Align2::RIGHT_CENTER, fmt::count(*n), theme::font(12.0), p.text);
        }

        // Changed today, hour by hour.
        panel(&painter, today, p);
        let changed: u32 = stats.hours.iter().sum();
        painter.text(pos2(today.min.x + 16.0, today.min.y + 22.0), Align2::LEFT_CENTER, "Changed today", theme::font(12.0), p.weak);
        let n = (changed as f32 * k) as u64;
        painter.text(pos2(today.min.x + 16.0, today.min.y + 52.0), Align2::LEFT_CENTER, fmt::count(n), theme::bold(28.0), p.text);
        painter.text(pos2(today.max.x - 16.0, today.min.y + 22.0), Align2::RIGHT_CENTER, "past 24 h", theme::font(11.0), p.faint);
        let chart = Rect::from_min_max(pos2(today.min.x + 16.0, today.min.y + 74.0), pos2(today.max.x - 16.0, today.max.y - 12.0));
        let max = stats.hours.iter().copied().max().unwrap_or(0).max(1) as f32;
        let bw = chart.width() / 24.0;
        for (i, &v) in stats.hours.iter().enumerate() {
            let hgt = (v as f32 / max * chart.height() * k).max(2.0);
            let x = chart.min.x + i as f32 * bw;
            let bar = Rect::from_min_max(pos2(x + 1.0, chart.max.y - hgt), pos2(x + bw - 1.0, chart.max.y));
            painter.rect_filled(bar, 2.0, fade(p.accent, 0.3 + 0.7 * (i as f32 / 23.0)));
        }
        ui.interact(today, Id::new("home-today"), Sense::hover())
            .on_hover_text("Your own files created or changed in the past 24 hours, hour by hour");

        // The biggest new file this week.
        panel(&painter, big, p);
        painter.text(pos2(big.min.x + 16.0, big.min.y + 22.0), Align2::LEFT_CENTER, "Biggest new this week", theme::font(12.0), p.weak);
        let found = stats.biggest.and_then(|(l, i)| {
            let v = ix.vol(l)?;
            let e = v.entry(i);
            (e.kind == FILE).then_some((v, l, i, e))
        });
        match found {
            Some((v, l, i, e)) => {
                let resp = ui.interact(big, Id::new("home-big"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
                let resp = resp.on_hover_text(v.path(i));
                if resp.hovered() {
                    painter.rect_filled(big.shrink(1.0), 15.0, p.hover);
                }
                let cy = big.min.y + 64.0;
                let ir = Rect::from_center_size(pos2(big.min.x + 34.0, cy), vec2(32.0, 32.0));
                let cat = search::entry_cat(v, i, &e);
                icon_or_fallback(&painter, ir, entry_icon(&mut self.icons, v, i, &e), false, theme::cat_color(cat, p.dark), 1.0);
                let tx = big.min.x + 60.0;
                theme::text_at(&painter, tx, cy - 9.0, v.name(i), theme::semibold(13.5), p.text, big.max.x - tx - 14.0);
                let sub = format!("{} · {}", fmt::size(e.size), short_dir(&v.path(e.parent)));
                theme::text_at(&painter, tx, cy + 10.0, &sub, theme::font(12.0), p.faint, big.max.x - tx - 14.0);
                if resp.clicked() {
                    act = Some(Act::Open(Hit { idx: i, vol: l as u8, rank: 0 }));
                }
            }
            None => {
                painter.text(big.center() + vec2(0.0, 10.0), Align2::CENTER_CENTER, "Nothing big this week", theme::font(12.5), p.faint);
            }
        }

        // Drives, side by side.
        panel(&painter, disks, p);
        let vols: Vec<&Volume> = ix.vols.iter().take(4).collect();
        let cw = (disks.width() - 32.0) / vols.len().max(1) as f32;
        for (n, v) in vols.iter().enumerate() {
            let x = disks.min.x + 16.0 + n as f32 * cw;
            let w = cw - 20.0;
            let y = disks.min.y + 24.0;
            // Each drive opens in Space, to see what's filling it.
            let cell = Rect::from_min_size(pos2(x - 8.0, disks.min.y + 6.0), vec2(cw - 4.0, disks.height() - 12.0));
            if !v.offline {
                let resp = ui
                    .interact(cell, Id::new(("home-drive", v.letter)), Sense::click())
                    .on_hover_cursor(CursorIcon::PointingHand)
                    .on_hover_text(format!("See what's taking space on {}:", v.letter));
                if resp.hovered() {
                    painter.rect_filled(cell, 10.0, p.hover);
                }
                if resp.clicked() {
                    act = Some(Act::AnalyzePath(format!("{}:\\", v.letter)));
                }
            }
            painter.text(pos2(x, y), Align2::LEFT_CENTER, format!("{}:", v.letter), theme::bold(16.0), p.text);
            let dot = pos2(x + 30.0, y);
            painter.circle_filled(dot, 3.5, if v.offline { p.warn } else if v.live { p.good } else { p.faint });
            let bar = Rect::from_min_size(pos2(x, y + 22.0), vec2(w, 8.0));
            match self.space.get(&v.letter) {
                Some(&(total, free)) => {
                    let used = 1.0 - free as f32 / total.max(1) as f32;
                    theme::bar(&painter, bar, used * k, p.track, p.accent, p.accent2);
                    let text = format!("{} free of {}", fmt::size(free), fmt::size(total));
                    theme::text_at(&painter, x, y + 46.0, &text, theme::font(12.0), p.weak, w);
                }
                None => {
                    painter.rect_filled(bar, 4.0, p.track);
                    let text = if v.offline { "Offline" } else { "" };
                    theme::text_at(&painter, x, y + 46.0, text, theme::font(12.0), p.warn, w);
                }
            }
            let line = format!("{} items · indexed in {}", fmt::count(v.items), fmt::duration(v.elapsed));
            theme::text_at(&painter, x, y + 66.0, &line, theme::font(11.5), p.faint, w);
        }
        act
    }

    /// Your activity week by week with what changed lately, and the folders
    /// you've been busiest in.
    fn activity(&mut self, ui: &mut Ui, p: &Pal, stats: &Stats, ix: &index::Index, col: f32, k: f32) -> Option<Act> {
        let mut act = None;
        let h = 316.0;
        let (area, _) = ui.allocate_exact_size(vec2(col, h), Sense::hover());
        let painter = ui.painter().clone();
        let lw = (col - GAP) * 0.58;
        let left = Rect::from_min_size(area.min, vec2(lw, h));
        let right = Rect::from_min_max(pos2(left.max.x + GAP, area.min.y), area.max);

        // Weeks: oldest on the left, this week on the right.
        panel(&painter, left, p);
        heading(&painter, left, "Your activity", "files you changed, week by week", p);
        let strip = Rect::from_min_max(pos2(left.min.x + 16.0, left.min.y + 44.0), pos2(left.max.x - 16.0, left.min.y + 72.0));
        let max = stats.weeks.iter().copied().max().unwrap_or(0).max(1) as f32;
        let cw = strip.width() / WEEKS as f32;
        for slot in 0..WEEKS {
            let week = WEEKS - 1 - slot;
            let n = stats.weeks[week];
            let cell = Rect::from_min_size(pos2(strip.min.x + slot as f32 * cw, strip.min.y), vec2(cw - 3.0, strip.height()));
            let shown = ease(k * 1.6 - slot as f32 * 0.02);
            let fill = if n == 0 { p.track } else { fade(p.accent, (0.18 + 0.82 * (n as f32 / max)) * shown) };
            painter.rect_filled(cell, 4.0, fill);
            let when = match week {
                0 => "this week".to_string(),
                1 => "last week".to_string(),
                w => format!("{w} weeks ago"),
            };
            ui.interact(cell, Id::new(("home-week", week)), Sense::hover())
                .on_hover_text(format!("{} files changed {when}", fmt::count(n as u64)));
        }
        let ly = strip.max.y + 12.0;
        painter.text(pos2(strip.min.x, ly), Align2::LEFT_CENTER, "6 months ago", theme::font(11.0), p.faint);
        painter.text(pos2(strip.max.x, ly), Align2::RIGHT_CENTER, "this week", theme::font(11.0), p.faint);

        // What changed lately, grouped by day.
        let now = index::now();
        let mut y = left.min.y + 104.0;
        let mut last_group = "";
        let row_h = 30.0;
        if stats.recent.is_empty() {
            painter.text(pos2(left.center().x, y + 40.0), Align2::CENTER_CENTER, "Nothing yet", theme::font(12.5), p.faint);
        }
        for (n, &(letter, idx)) in stats.recent.iter().enumerate() {
            if y + row_h > left.max.y - 10.0 {
                break;
            }
            let Some(v) = ix.vol(letter) else { continue };
            let e = v.entry(idx);
            if e.kind == NONE {
                continue;
            }
            let age = now.saturating_sub(e.mtime);
            let group = match age {
                a if a < 86_400 => "Today",
                a if a < 2 * 86_400 => "Yesterday",
                a if a < 7 * 86_400 => "This week",
                _ => "Earlier",
            };
            let r = Rect::from_min_size(pos2(left.min.x + 10.0, y), vec2(lw - 20.0, row_h));
            if group != last_group {
                let color = if group == "Today" { p.accent } else { p.weak };
                painter.text(pos2(r.min.x + 6.0, r.center().y), Align2::LEFT_CENTER, group, theme::semibold(12.0), color);
                last_group = group;
            }
            let h = Hit { idx, vol: letter as u8, rank: 0 };
            let resp = ui.interact(r, Id::new(("home-recent", n)), Sense::click());
            let resp = resp.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(v.path(idx));
            let row = Rect::from_min_max(pos2(r.min.x + 78.0, r.min.y), r.max);
            if resp.hovered() {
                painter.rect_filled(row, 8.0, p.hover);
            }
            let cy = r.center().y;
            let ir = Rect::from_center_size(pos2(row.min.x + 16.0, cy), vec2(20.0, 20.0));
            let cat = search::entry_cat(v, idx, &e);
            icon_or_fallback(&painter, ir, entry_icon(&mut self.icons, v, idx, &e), false, theme::cat_color(cat, p.dark), 1.0);
            let ago = fmt::ago(e.mtime);
            let aw = painter.layout_no_wrap(ago.clone(), theme::font(12.0), p.faint).size().x;
            let name_w = row.width() - 50.0 - aw;
            theme::text_at(&painter, row.min.x + 34.0, cy, v.name(idx), theme::font(13.0), p.text, name_w * 0.6);
            let folder = short_dir(&v.path(e.parent)).to_string();
            let nw = painter.layout_no_wrap(v.name(idx).to_string(), theme::font(13.0), p.text).size().x.min(name_w * 0.6);
            theme::text_at(&painter, row.min.x + 44.0 + nw, cy, &folder, theme::font(11.5), p.faint, name_w - nw - 10.0);
            painter.text(pos2(row.max.x - 8.0, cy), Align2::RIGHT_CENTER, ago, theme::font(12.0), p.faint);
            if resp.clicked() {
                act = Some(Act::Open(h));
            }
            resp.context_menu(|ui| {
                if let Some(a) = item_menu(ui, h, false) {
                    act = Some(a);
                }
            });
            y += row_h;
        }

        // Where you work: your busiest folders around you.
        panel(&painter, right, p);
        heading(&painter, right, "Where you work", "busiest folders this month", p);
        let field = Rect::from_min_max(pos2(right.min.x + 12.0, right.min.y + 42.0), pos2(right.max.x - 12.0, right.max.y - 14.0));
        let c = field.center();
        let rad = field.width().min(field.height()) * 0.5;
        painter.circle_stroke(c, rad * 0.5, Stroke::new(1.0_f32, p.track));
        painter.circle_stroke(c, rad * 0.9, Stroke::new(1.0_f32, p.divider));
        painter.circle_filled(c, 24.0, fade(p.accent, 0.22));
        painter.text(c, Align2::CENTER_CENTER, "You", theme::semibold(12.0), p.text);
        if stats.folders.is_empty() {
            let text = "Folders you work in will show up here";
            painter.text(pos2(c.x, field.max.y - 10.0), Align2::CENTER_CENTER, text, theme::font(12.0), p.faint);
        }
        let most = stats.folders.first().map_or(1, |f| f.2).max(1) as f32;
        let inner = stats.folders.len().min(4);
        let outer = stats.folders.len().saturating_sub(4);
        for (i, &(letter, dir, n)) in stats.folders.iter().enumerate() {
            let Some(v) = ix.vol(letter) else { continue };
            let (ring, slot, count, turn) = if i < 4 { (0.5, i, inner, 0.0) } else { (0.9, i - 4, outer, 0.5) };
            let ang = (slot as f32 + turn) / count.max(1) as f32 * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
            let a = ease(k * 1.5 - i as f32 * 0.07);
            let pos = c + vec2(ang.cos(), ang.sin()) * rad * ring * (0.6 + 0.4 * a);
            let r = (7.0 + 15.0 * (n as f32 / most).sqrt()) * a;
            let color = bubble_color(i, p.dark);
            let hit = Rect::from_center_size(pos, Vec2::splat((r * 2.0).max(16.0)));
            let path = v.path(dir);
            let resp = ui.interact(hit, Id::new(("home-folder", i)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            let resp = resp.on_hover_text(format!("{path}\n{} changes this month. Click to search inside.", fmt::count(n as u64)));
            if resp.hovered() {
                painter.circle_stroke(pos, r + 4.0, Stroke::new(1.5_f32, fade(color, 0.6)));
            }
            painter.line_segment([c, pos], Stroke::new(1.0_f32, fade(color, 0.18 * a)));
            painter.circle_filled(pos, r, fade(color, 0.9 * a));
            let mut job = LayoutJob::simple_singleline(v.name(dir).to_string(), theme::font(11.0), fade(p.weak, a));
            job.wrap = TextWrapping::truncate_at_width(90.0);
            let g = painter.layout_job(job);
            painter.galley(pos2(pos.x - g.size().x / 2.0, pos.y + r + 3.0), g, p.weak);
            if resp.clicked() {
                act = Some(Act::SearchIn(Hit { idx: dir, vol: letter as u8, rank: 0 }));
            }
        }
        act
    }
}
