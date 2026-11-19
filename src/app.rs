//! The window: a search box with filters, live match counts per type, matching
//! apps, a results list with real Windows icons, a preview pane, and a home
//! screen that shows what's indexed and what just changed. Beside search sits
//! the Space view (`crate::space`): where the disk space went.

use crate::apps::{self, StartApp};
use crate::history::History;
use crate::index::{self, lock, Entry, Shared, Volume, DIR, FILE, NONE};
use crate::search::{self, Cat, Counts, Filters, Hit, Request, Row, SizeF, Sort, Stats, When, Worker};
use crate::settings::Settings;
use crate::shell::Icons;
use crate::space;
use crate::theme::{self, ease, fade, mix, Pal};
use crate::tray::{self, Hotkey, HotkeyState, Signal};
use crate::{edition, fmt, icon, trace, update, win};
use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, ColorImage, CursorIcon, Frame, Id, Key, Layout, Margin, Modifiers,
    Painter, Rect, Response, RichText, ScrollArea, Sense, Stroke, StrokeKind, TextEdit, TextureHandle, TextureId,
    TextureOptions, Ui, Vec2,
};
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod bar;
mod home;
mod prefs;

pub const MIN_SIZE: [f32; 2] = [820.0, 520.0];
const ROW_H: f32 = 50.0;
/// The floating search bar.
const BAR_W: f32 = 720.0;
const BAR_ROWS: usize = 8;
const BAR_ROW_H: f32 = 44.0;
const BAR_LINE: f32 = 60.0;
const BAR_CHIPS: f32 = 46.0;
const BAR_FOOT: f32 = 34.0;
/// The floating button the bar shrinks to.
const BUBBLE: f32 = 52.0;
/// How often results refresh while files change on disk.
const REFRESH: Duration = Duration::from_millis(250);
const PREVIEW_W: f32 = 340.0;
const SIDE: f32 = 48.0;

const TIPS: [(&str, &str); 5] = [
    ("*.psd", "wildcards"),
    ("ext:pdf;docx", "file types"),
    ("size:>1gb", "big files"),
    ("\\downloads\\ zip", "inside a path"),
    ("report !draft", "leave words out"),
];

fn uv() -> Rect {
    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))
}

#[derive(Clone, PartialEq)]
struct Asked {
    text: String,
    cat: Cat,
    filters: Filters,
    sort: Sort,
    desc: bool,
}

enum Act {
    Open(Hit),
    Reveal(Hit),
    CopyPath(Hit),
    CopyName(Hit),
    SearchIn(Hit),
    Launch(StartApp),
    OpenPath(String),
    Query(String),
    /// See what's taking space in a folder (the Space view).
    Analyze(Hit),
    AnalyzePath(String),
}

/// One line in the results list.
#[derive(Clone, Copy)]
struct Vis {
    /// Position in `hits`.
    hit: u32,
    /// Size of the group this row belongs to, if any.
    group: Option<u32>,
    /// Shown under its group's first row (the group is open).
    member: bool,
}

/// Apps and settings are re-read when the window comes back to the front, at
/// most this often.
const APPS_STALE: Duration = Duration::from_secs(600);

/// One window, three ways: the full app, the floating search bar (the hotkey),
/// or hidden in the tray.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Full,
    Bar,
    Hidden,
}

/// A line in the search bar.
enum BarItem {
    App(StartApp),
    Hit(Hit),
}

pub struct App {
    shared: Arc<Shared>,
    worker: Worker,
    updater: update::Updater,
    history: Arc<Mutex<History>>,
    /// The current results allow for typos (nothing matched exactly).
    fuzzy: bool,
    /// Plain-language words read as filters ("Pictures", "Past week").
    understood: Vec<String>,
    icons: Icons,
    apps: Arc<Mutex<Vec<StartApp>>>,
    stats: Arc<Mutex<Stats>>,
    elevated: bool,

    query: String,
    cat: Cat,
    filters: Filters,
    filters_open: bool,
    sort: Sort,
    desc: bool,

    asked: Option<Asked>,
    asked_at: Instant,
    next_id: u64,
    hits: Vec<Hit>,
    /// How `hits` are grouped into rows (empty: one row each).
    rows: Vec<Row>,
    /// Groups opened by the user, by their first hit.
    expanded: HashSet<(u8, u32)>,
    /// What the list shows, with groups folded or opened.
    visible: Vec<Vis>,
    home: String,
    counts: Counts,
    hits_gen: u64,
    took: Duration,
    searching: bool,
    highlight: Option<String>,
    results_at: Instant,
    /// Matching apps, settings and tools.
    app_hits: Vec<StartApp>,
    apps_at: Instant,
    /// The window is in front (not behind a game, not minimized).
    focused: bool,

    selected: Option<Hit>,
    preview: Option<Hit>,
    scroll_to: Option<usize>,
    scroll_top: bool,
    scroll_off: f32,
    view_h: f32,
    focus: bool,

    logo: TextureHandle,
    logo_big: TextureHandle,
    toast: Option<(String, Instant)>,
    opened_at: Instant,
    last_gen: u64,
    pulse_at: Instant,
    space: HashMap<char, (u64, u64)>,
    space_at: Option<Instant>,
    /// For the timing trace (SPEED_TRACE).
    frames: u64,
    marks: Vec<String>,
    last_frame: Option<Instant>,

    settings: Settings,
    /// From the hotkey and the tray icon (None if they couldn't start).
    signals: Option<Receiver<Signal>>,
    hwnd: isize,
    mode: Mode,
    /// Frames left before the window appears: it's painted in its new shape first.
    show_wait: u8,
    bar_at: Instant,
    bar_sel: usize,
    bar_query: String,
    /// The bar has had the keyboard since it appeared (losing it closes the bar).
    bar_focused: bool,
    /// Shrunk to a floating button.
    bar_collapsed: bool,
    /// The bar's size as last set.
    bar_dims: Vec2,
    /// Moving the bar: where the mouse (in pixels) and the window were when it started.
    drag_from: Option<((i32, i32), egui::Pos2)>,
    /// Turn into the bar on the next frame (once hidden).
    bar_next: bool,
    settings_open: bool,
    /// Waiting for the keys of a new hotkey.
    recording: bool,
    settings_note: Option<String>,
    /// The Space view, made the first time it's opened. It holds a whole size
    /// tree, so it's let go again while the app sits in the tray.
    space_view: Option<space::App>,
}

impl App {
    pub fn new(ctx: egui::Context, hwnd: isize, background: bool) -> App {
        let settings = Settings::load();
        theme::apply_appearance(&ctx, settings.appearance);
        let wake = ctx.clone();
        tray::set_app_window(hwnd);
        if !background {
            win::cloak(hwnd, true);
        }
        let signals = tray::start(Some(settings.hotkey), move || wake.request_repaint());
        let repaint = ctx.clone();
        let shared = Shared::new(Box::new(move || repaint.request_repaint()));
        let elevated = win::is_elevated();
        index::watch_drives(shared.clone(), elevated);
        crate::column::start(shared.clone(), settings.explorer_column, settings.column_share, settings.column_week, settings.column_by_share);

        let apps = Arc::new(Mutex::new(Vec::new()));
        load_apps(&apps, &ctx);
        let history = Arc::new(Mutex::new(History::load()));
        let texture = |name: &str, n: usize| {
            let img = ColorImage::from_rgba_unmultiplied([n, n], &icon::rgba(n as u32, edition::get().palette));
            ctx.load_texture(name, img, TextureOptions::LINEAR)
        };
        App {
            worker: Worker::start(shared.clone(), history.clone()),
            updater: update::Updater::start(ctx.clone()),
            history,
            fuzzy: false,
            understood: Vec::new(),
            stats: search::start_stats(shared.clone()),
            icons: Icons::start(ctx.clone()),
            shared,
            apps,
            elevated,
            query: String::new(),
            cat: Cat::All,
            filters: Filters::default(),
            filters_open: false,
            sort: Sort::Best,
            desc: false,
            asked: None,
            asked_at: Instant::now(),
            next_id: 1,
            hits: Vec::new(),
            rows: Vec::new(),
            expanded: HashSet::new(),
            visible: Vec::new(),
            home: std::env::var("USERPROFILE").unwrap_or_default(),
            counts: [0; 8],
            hits_gen: 0,
            took: Duration::ZERO,
            searching: false,
            highlight: None,
            results_at: Instant::now(),
            app_hits: Vec::new(),
            apps_at: Instant::now(),
            focused: true,
            selected: None,
            preview: None,
            scroll_to: None,
            scroll_top: false,
            scroll_off: 0.0,
            view_h: 0.0,
            focus: true,
            logo: texture("logo", 64),
            logo_big: texture("logo-big", 192),
            toast: None,
            opened_at: Instant::now(),
            last_gen: 0,
            pulse_at: Instant::now() - Duration::from_secs(10),
            space: HashMap::new(),
            space_at: None,
            frames: 0,
            marks: Vec::new(),
            last_frame: None,
            settings,
            signals,
            hwnd,
            mode: if background { Mode::Hidden } else { Mode::Full },
            // Shown full size from the start: drawn unseen until it's maximized.
            show_wait: if background { 0 } else { 5 },
            bar_at: Instant::now(),
            bar_sel: 0,
            bar_query: String::new(),
            bar_focused: false,
            bar_collapsed: false,
            bar_dims: Vec2::ZERO,
            drag_from: None,
            bar_next: false,
            settings_open: false,
            recording: false,
            settings_note: None,
            space_view: None,
        }
    }

    fn active(&self) -> bool {
        !self.query.trim().is_empty() || self.cat != Cat::All || self.filters.narrows()
    }

    fn show_apps(&self) -> bool {
        !self.app_hits.is_empty() && matches!(self.cat, Cat::All | Cat::App) && !self.filters.narrows()
    }

    fn poll(&mut self) {
        while let Ok(a) = self.worker.rx.try_recv() {
            if a.id + 1 != self.next_id {
                continue; // superseded
            }
            self.hits = a.hits;
            self.rows = a.rows;
            self.counts = a.counts;
            self.took = a.took;
            self.fuzzy = a.fuzzy;
            self.understood = a.understood;
            self.hits_gen = a.generation;
            self.searching = false;
            if a.reset {
                self.scroll_top = true;
                self.selected = None;
                self.expanded.clear();
                self.results_at = Instant::now();
            }
            self.rebuild_visible();
        }
    }

    fn rebuild_visible(&mut self) {
        self.visible.clear();
        if self.rows.is_empty() {
            self.visible.extend((0..self.hits.len() as u32).map(|hit| Vis { hit, group: None, member: false }));
            return;
        }
        for &(start, len) in &self.rows {
            if len <= 1 {
                self.visible.push(Vis { hit: start, group: None, member: false });
                continue;
            }
            let lead = self.hits[start as usize];
            self.visible.push(Vis { hit: start, group: Some(len), member: false });
            if self.expanded.contains(&(lead.vol, lead.idx)) {
                self.visible.extend((start + 1..start + len).map(|hit| Vis { hit, group: Some(len), member: true }));
            }
        }
    }

    /// Sends a new search when the query changed, or a refresh when files changed.
    fn ask_if_needed(&mut self, ctx: &egui::Context) {
        let want = Asked {
            text: self.query.trim().to_string(),
            cat: self.cat,
            filters: self.filters.clone(),
            sort: self.sort,
            desc: self.desc,
        };
        let generation = self.shared.read().generation;
        let changed = self.asked.as_ref() != Some(&want);
        // Refreshing for changes on disk only happens while someone's looking.
        let outdated = !changed && self.focused && self.active() && generation != self.hits_gen && !self.searching;
        if !changed && !outdated {
            return;
        }
        if !changed && self.asked_at.elapsed() < REFRESH {
            ctx.request_repaint_after(REFRESH - self.asked_at.elapsed());
            return;
        }
        let reset = changed
            && self.asked.as_ref().is_none_or(|a| a.text != want.text || a.cat != want.cat || a.filters != want.filters);
        if changed {
            let apps = lock(&self.apps);
            let history = lock(&self.history);
            let now = index::now();
            let boost = |a: &StartApp| history.boost(&apps::history_key(a), &want.text, now);
            self.app_hits = apps::find(&apps, &want.text, &boost).into_iter().map(|i| apps[i].clone()).collect();
        }
        self.worker.ask(Request {
            id: self.next_id,
            text: want.text.clone(),
            cat: want.cat,
            filters: want.filters.clone(),
            sort: want.sort,
            desc: want.desc,
            reset,
        });
        self.next_id += 1;
        self.searching = true;
        self.asked_at = Instant::now();
        self.highlight = search::highlight(&want.text);
        self.asked = Some(want);
    }

    fn keyboard(&mut self, ctx: &egui::Context) -> Option<Act> {
        let (down, up, page_down, page_up, reveal, enter, esc, find, copy) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::PageDown),
                i.consume_key(Modifiers::NONE, Key::PageUp),
                i.consume_key(Modifiers::COMMAND, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
                i.consume_key(Modifiers::COMMAND, Key::F) | i.consume_key(Modifiers::COMMAND, Key::L),
                i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::C),
            )
        });
        if find {
            self.focus = true;
        }
        if esc {
            if self.filters_open {
                self.filters_open = false;
            } else if self.selected.is_some() {
                self.selected = None;
            } else {
                self.query.clear();
            }
            self.focus = true;
        }
        if !self.active() {
            return None;
        }
        let step: i64 = if down {
            1
        } else if up {
            -1
        } else if page_down {
            12
        } else if page_up {
            -12
        } else {
            0
        };
        // Arrow keys move through the rows on screen (folded groups count as one).
        let n = self.visible.len();
        if step != 0 && n > 0 {
            let hits = &self.hits;
            let cur = self.selected.and_then(|s| self.visible.iter().position(|v| hits[v.hit as usize].same(s)));
            let next = match cur {
                Some(r) => (r as i64 + step).clamp(0, n as i64 - 1) as usize,
                None if step > 0 => 0,
                None => n - 1,
            };
            self.selected = Some(self.hits[self.visible[next].hit as usize]);
            self.scroll_to = Some(next);
        }
        let chosen = self.selected.filter(|s| self.hits.iter().any(|h| h.same(*s)));
        if enter {
            // With nothing picked, Enter starts the best matching app, like the Start menu.
            if chosen.is_none() && self.show_apps() {
                return Some(Act::Launch(self.app_hits[0].clone()));
            }
            return chosen.or(self.hits.first().copied()).map(Act::Open);
        }
        let target = chosen.or(self.hits.first().copied());
        if reveal {
            return target.map(Act::Reveal);
        }
        if copy {
            return target.map(Act::CopyPath);
        }
        None
    }

    /// Notes what was opened from this search, so it ranks higher next time.
    fn remember(&self, key: &str, shown: &str) {
        lock(&self.history).record(key, shown, &self.query);
    }

    fn act(&mut self, ctx: &egui::Context, a: Act) {
        let h = match &a {
            Act::Launch(app) => {
                self.remember(&apps::history_key(app), &app.name);
                apps::launch(app);
                return;
            }
            Act::OpenPath(path) => {
                self.remember(&path.to_lowercase(), path);
                win::open(path);
                return;
            }
            Act::Query(q) => {
                self.query = q.clone();
                self.focus = true;
                return;
            }
            Act::AnalyzePath(path) => {
                self.analyze(ctx, path.clone());
                return;
            }
            Act::Open(h) | Act::Reveal(h) | Act::CopyPath(h) | Act::CopyName(h) | Act::SearchIn(h) | Act::Analyze(h) => *h,
        };
        let (path, name) = {
            let ix = self.shared.read();
            let Some(v) = ix.vol(h.vol as char) else { return };
            if v.entry(h.idx).kind == NONE {
                return;
            }
            (v.path(h.idx), v.name(h.idx).to_string())
        };
        if matches!(a, Act::Open(_) | Act::Reveal(_) | Act::CopyPath(_)) {
            self.remember(&path.to_lowercase(), &path);
        }
        match a {
            Act::Open(_) => win::open(&path),
            Act::Reveal(_) => win::reveal(&path),
            Act::CopyPath(_) => {
                ctx.copy_text(path);
                self.toast = Some(("Copied the full path".into(), Instant::now()));
            }
            Act::CopyName(_) => {
                ctx.copy_text(name);
                self.toast = Some(("Copied the name".into(), Instant::now()));
            }
            Act::SearchIn(_) => {
                self.filters.folder = Some(path);
                self.filters.drive = None;
                self.selected = None;
                self.focus = true;
            }
            Act::Analyze(_) => self.analyze(ctx, path),
            _ => {}
        }
    }

    // ----------------------------------------------------------------- top bar

    fn top(&mut self, ui: &mut Ui, p: &Pal) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            header_logo(ui, self.logo.id());
            let w = (ui.available_width() - TRAIL_W - 10.0).max(200.0);
            if self.search_box(ui, p, w) {
                self.filters_open = !self.filters_open;
            }
            let (to_space, gear) = header_trail(ui, false, self.settings_open, p);
            if to_space == Some(true) {
                self.set_view(ui.ctx(), true);
            }
            if gear {
                self.toggle_settings();
            }
        });
        ui.add_space(10.0);
        self.chip_row(ui, p);
        self.composition(ui, p);
        let t = ease(ui.ctx().animate_bool_with_time(Id::new("filters-open"), self.filters_open, 0.22));
        if t > 0.001 {
            self.filter_panel(ui, p, t);
        }
    }

    /// The search field, with the filter button inside its right end. True
    /// when the filter button was clicked.
    fn search_box(&mut self, ui: &mut Ui, p: &Pal, w: f32) -> bool {
        let id = Id::new("search");
        let (rect, _) = ui.allocate_exact_size(vec2(w, HEADER_H), Sense::hover());
        let focused = ui.memory(|m| m.has_focus(id));
        let t = ease(ui.ctx().animate_bool_with_time(Id::new("search-glow"), focused, 0.25));
        let painter = ui.painter().clone();
        for k in 1..=5 {
            let e = k as f32 * 1.6;
            painter.rect_filled(rect.expand(e), 12.0 + e, fade(p.accent, 0.06 * t / k as f32));
        }
        painter.rect(rect, 12.0, p.field, Stroke::new(1.0_f32, mix(p.border, p.accent, t)), StrokeKind::Inside);
        theme::magnifier(&painter, pos2(rect.min.x + 22.0, rect.center().y), mix(p.weak, p.accent, t), 1.0);
        // Right end, inside the field: [clear ✕] | [filters].
        let filter_r = Rect::from_center_size(pos2(rect.max.x - 4.0 - CTRL_H * 0.5, rect.center().y), Vec2::splat(CTRL_H));
        let filter = filter_button(ui, filter_r, self.filters.count(), self.filters_open, p);
        let sep_x = filter_r.min.x - 6.0;
        painter.line_segment(
            [pos2(sep_x, rect.center().y - 9.0), pos2(sep_x, rect.center().y + 9.0)],
            Stroke::new(1.0_f32, p.border),
        );
        let clear_c = pos2(sep_x - 6.0 - 14.0, rect.center().y);
        let text_rect = Rect::from_min_max(pos2(rect.min.x + 44.0, rect.min.y), pos2(clear_c.x - 18.0, rect.max.y));
        // A child ui, not `put`: `put` would move this row's cursor to the end
        // of the text, and the next item would land on top of the field.
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(text_rect).layout(Layout::left_to_right(Align::Center)));
        let resp = child.add(
            TextEdit::singleline(&mut self.query)
                .id(id)
                .frame(false)
                .font(theme::font(16.0))
                .text_color(p.text)
                .hint_text(RichText::new("Search files, folders and apps").color(p.faint))
                .margin(Margin::symmetric(0, 9))
                .desired_width(text_rect.width()),
        );
        if std::mem::take(&mut self.focus) {
            resp.request_focus();
        }
        if !self.query.is_empty() {
            let r = Rect::from_center_size(clear_c, vec2(28.0, 28.0));
            let clear = ui.interact(r, Id::new("clear"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            if clear.hovered() {
                painter.rect_filled(r, 8.0, p.hover);
            }
            let (c, s) = (r.center(), 4.5);
            let stroke = Stroke::new(1.5_f32, if clear.hovered() { p.text } else { p.weak });
            painter.line_segment([c - vec2(s, s), c + vec2(s, s)], stroke);
            painter.line_segment([c + vec2(-s, s), c + vec2(s, -s)], stroke);
            if clear.clicked() {
                self.query.clear();
                self.focus = true;
            }
        }
        filter.clicked()
    }

    fn chip_row(&mut self, ui: &mut Ui, p: &Pal) {
        let active = self.active();
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
            ui.add_space(SIDE);
            for c in Cat::ALL {
                let count = active.then_some(self.counts[c.index()]);
                let dot = (c != Cat::All).then(|| theme::cat_color(Some(c), p.dark));
                if chip(ui, Id::new(("chip", c.index())), c.label(), count, dot, self.cat == c, 30.0, p).clicked() {
                    self.cat = c;
                    self.focus = true;
                }
            }
            // Filters that are on, each removable with a click.
            let f = &mut self.filters;
            if f.when != When::Any && pill(ui, f.when.label(), p).clicked() {
                f.when = When::Any;
            }
            if f.size != SizeF::Any && pill(ui, f.size.label(), p).clicked() {
                f.size = SizeF::Any;
            }
            if let Some(d) = f.drive {
                if pill(ui, &format!("On {d}:"), p).clicked() {
                    f.drive = None;
                }
            }
            if let Some(dir) = &f.folder {
                if pill(ui, &format!("In {}", short_dir(dir)), p).on_hover_text(dir.as_str()).clicked() {
                    f.folder = None;
                }
            }
            if f.hide_system && pill(ui, "No system files", p).clicked() {
                f.hide_system = false;
            }
        });
    }

    /// A thin bar under the chips showing what the matches are made of.
    fn composition(&self, ui: &mut Ui, p: &Pal) {
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 12.0), Sense::hover());
        let bar = Rect::from_min_max(pos2(r.min.x + SIDE, r.center().y - 1.5), pos2(r.max.x, r.center().y + 1.5));
        let active = self.active() && self.counts[0] > 0;
        let shown = ui.ctx().animate_bool_with_time(Id::new("comp-shown"), active, 0.3);
        if shown <= 0.0 {
            return;
        }
        let painter = ui.painter();
        painter.rect_filled(bar, 1.5, fade(p.track, shown));
        let total = self.counts[0].max(1) as f32;
        let known: u64 = self.counts[1..].iter().sum();
        let mut x = bar.min.x;
        let segs = Cat::ALL[1..]
            .iter()
            .map(|&c| (c.index(), self.counts[c.index()], theme::cat_color(Some(c), p.dark)))
            .chain(std::iter::once((0, self.counts[0].saturating_sub(known), theme::cat_color(None, p.dark))));
        for (k, n, color) in segs {
            let frac = ui.ctx().animate_value_with_time(Id::new(("seg", k)), if active { n as f32 / total } else { 0.0 }, 0.35);
            let w = frac * bar.width();
            if w > 1.0 {
                let seg = Rect::from_min_size(pos2(x, bar.min.y), vec2(w - 1.0, bar.height()));
                painter.rect_filled(seg, 1.5, fade(color, shown));
            }
            x += w;
        }
    }

    fn filter_panel(&mut self, ui: &mut Ui, p: &Pal, t: f32) {
        let drives: Vec<char> = self.shared.read().vols.iter().map(|v| v.letter).collect();
        let network = self.shared.network_drives();
        let indexing: Vec<char> = self.shared.jobs().iter().map(|j| j.letter).collect();
        let offline: Vec<char> = self.shared.read().vols.iter().filter(|v| v.offline).map(|v| v.letter).collect();
        let hidden_maps = self.elevated && network.is_empty();
        let mut switched: Option<(char, bool)> = None;
        let full_h = 202.0;
        ui.allocate_ui(vec2(ui.available_width(), full_h * t), |ui| {
            ui.set_clip_rect(ui.max_rect().intersect(ui.clip_rect()));
            ui.multiply_opacity(t);
            ui.add_space(6.0);
            let f = &mut self.filters;

            filter_row(ui, "Modified", p, |ui| {
                for w in When::ALL {
                    if chip(ui, Id::new(("when", w as u8)), w.label(), None, None, f.when == w, 26.0, p).clicked() {
                        f.when = w;
                    }
                }
            });
            filter_row(ui, "Size", p, |ui| {
                for s in SizeF::ALL {
                    if chip(ui, Id::new(("size", s as u8)), s.label(), None, None, f.size == s, 26.0, p).clicked() {
                        f.size = s;
                    }
                }
            });
            filter_row(ui, "Location", p, |ui| {
                let everywhere = f.drive.is_none() && f.folder.is_none();
                if chip(ui, Id::new("loc-all"), "Everywhere", None, None, everywhere, 26.0, p).clicked() {
                    f.drive = None;
                    f.folder = None;
                }
                for d in &drives {
                    let on = f.drive == Some(*d) && f.folder.is_none();
                    if chip(ui, Id::new(("loc", *d)), &format!("{d}:"), None, None, on, 26.0, p).clicked() {
                        f.drive = Some(*d);
                        f.folder = None;
                    }
                }
                let label = match &f.folder {
                    Some(dir) => format!("In {}", short_dir(dir)),
                    None => "Choose folder…".to_string(),
                };
                if chip(ui, Id::new("loc-folder"), &label, None, None, f.folder.is_some(), 26.0, p).clicked() {
                    if let Some(dir) = rfd::FileDialog::new().set_title("Search inside a folder").pick_folder() {
                        f.folder = Some(dir.to_string_lossy().into_owned());
                        f.drive = None;
                    }
                }
            });
            // Network drives are opt-in: walking a big work share takes a while and
            // puts load on the server.
            filter_row(ui, "Network", p, |ui| {
                if network.is_empty() {
                    let text = if hidden_maps {
                        "No network drives visible. Windows hides mapped drives from apps running as administrator; LordSpeed sees them."
                    } else {
                        "No network drives mapped"
                    };
                    ui.label(RichText::new(text).size(12.5).color(p.faint));
                }
                for d in &network {
                    if theme::toggle(ui, d.on, p).on_hover_text("Index this drive so it shows up in searches").clicked() {
                        switched = Some((d.letter, !d.on));
                    }
                    ui.label(RichText::new(format!("{}:", d.letter)).font(theme::semibold(12.5)).color(p.text));
                    let state = if !d.on {
                        ""
                    } else if offline.contains(&d.letter) || !d.online {
                        "  · offline"
                    } else if indexing.contains(&d.letter) {
                        "  · indexing…"
                    } else {
                        "  · indexed"
                    };
                    ui.label(RichText::new(format!("{}{state}", d.remote)).size(12.0).color(p.weak));
                    ui.add_space(14.0);
                }
            });
            filter_row(ui, "Hide", p, |ui| {
                if theme::toggle(ui, f.hide_system, p).clicked() {
                    f.hide_system = !f.hide_system;
                }
                ui.label(RichText::new("System, hidden and app-internal files").size(12.5).color(p.weak));
                if f.count() > 0 {
                    ui.add_space(12.0);
                    let reset = ui.add(egui::Button::new(RichText::new("Reset all").size(12.5).color(p.accent)).frame(false));
                    if reset.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                        *f = Filters::default();
                    }
                }
            });
        });
        if let Some((letter, on)) = switched {
            self.shared.set_network(letter, on);
        }
    }

    // -------------------------------------------------------------- status bar

    /// Returns true if the user asked to restart as administrator.
    fn status(&mut self, ui: &mut Ui, p: &Pal) -> bool {
        let vols: Vec<_> =
            self.shared.read().vols.iter().map(|v| (v.letter, v.items, v.method, v.elapsed, v.live && !v.offline, v.offline)).collect();
        let jobs = self.shared.jobs();
        let notes = self.shared.notes();
        let mut restart = false;
        let any_live = vols.iter().any(|v| v.4);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            if any_live {
                let r = ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    live_dot(ui, p, self.pulse_at);
                    ui.label(RichText::new("Live").size(12.0).color(p.weak));
                });
                r.response.on_hover_text("New, renamed and deleted files show up in results within a fraction of a second.");
            }
            for (letter, items, method, took, live, offline) in vols {
                if offline {
                    ui.label(RichText::new(format!("{letter}:  {}  · offline", fmt::count(items))).size(12.0).color(p.warn))
                        .on_hover_text("Disconnected. Its files stay searchable from the last scan and catch up when it's back.");
                    continue;
                }
                let restart = format!("not live: restart {} to pick up changes", edition::get().name);
                let how = if live { "kept current as files change" } else { restart.as_str() };
                ui.label(RichText::new(format!("{letter}:  {}", fmt::count(items))).size(12.0).color(p.faint))
                    .on_hover_text(format!("Indexed from the {} in {}, {how}.", method.label(), fmt::duration(took)));
            }
            for j in &jobs {
                let (done, total) = (j.done.load(Relaxed), j.total.load(Relaxed));
                // Already searchable from the saved index: this is just a quiet re-check.
                let refreshing = self.shared.read().vol(j.letter).is_some();
                let what = if refreshing { "Refreshing" } else { "Indexing" };
                let text = if total > 0 {
                    format!("{what} {}:  {}%", j.letter, done * 100 / total)
                } else {
                    format!("{what} {}:  {} items", j.letter, fmt::count(j.items.load(Relaxed)))
                };
                let color = if refreshing { p.faint } else { p.accent };
                ui.label(RichText::new(text).size(12.0).color(color));
            }
            if !notes.is_empty() {
                let s = if notes.len() == 1 { "" } else { "s" };
                ui.label(RichText::new(format!("⚠ {} note{s}", notes.len())).size(12.0).color(p.warn))
                    .on_hover_text(notes.join("\n"));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(format!("v{}", update::version())).size(11.5).color(p.faint));
                if self.signals.is_some() {
                    self.hotkey_chip(ui, p);
                }
                match self.updater.state() {
                    update::State::Available(r) => {
                        let label = RichText::new(format!("✦ Update to v{}", r.version)).size(12.0).color(p.on_accent);
                        let b = egui::Button::new(label).fill(p.accent).corner_radius(10.0).min_size(vec2(0.0, 22.0));
                        let resp = ui.add(b).on_hover_text("Downloads the new version, checks it, and restarts. Takes a few seconds.");
                        if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                            self.updater.install(ui.ctx());
                        }
                    }
                    update::State::Installing => {
                        ui.label(RichText::new("Updating…").size(12.0).color(p.accent));
                    }
                    update::State::Failed(e) => {
                        ui.label(RichText::new("⚠ Update failed").size(12.0).color(p.warn)).on_hover_text(e);
                    }
                    update::State::Idle | update::State::UpToDate => {}
                }
                if !self.elevated && edition::get().admin {
                    restart = ui
                        .add(egui::Button::new(RichText::new("Restart as administrator").size(12.0).color(p.text)))
                        .clicked();
                    ui.label(RichText::new("Running without admin: slower indexing, no live updates").size(12.0).color(p.warn));
                } else if let Some((msg, at)) = self.toast.as_ref().filter(|(_, at)| at.elapsed() < Duration::from_secs(3)) {
                    let a = 1.0 - ease((at.elapsed().as_secs_f32() - 2.4) / 0.6);
                    ui.label(RichText::new(format!("✓ {msg}")).size(12.0).color(fade(p.good, a)));
                } else {
                    ui.label(RichText::new("↑↓ select   Enter open   Ctrl+Enter show in folder   Esc clear").size(11.5).color(p.faint));
                }
            });
        });
        restart
    }

    // ---------------------------------------------------------------- indexing

    fn indexing(&self, ui: &mut Ui, p: &Pal) {
        let jobs = self.shared.jobs();
        let time = ui.input(|i| i.time) as f32;
        ui.ctx().request_repaint();
        ui.add_space((ui.available_height() * 0.22).max(24.0));
        ui.vertical_centered(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(120.0, 120.0), Sense::hover());
            let painter = ui.painter();
            let c = r.center();
            painter.image(self.logo_big.id(), Rect::from_center_size(c, vec2(72.0, 72.0)), uv(), Color32::WHITE);
            painter.circle_stroke(c, 52.0, Stroke::new(3.0_f32, p.track));
            let spin = (time * 0.7).fract();
            theme::arc(painter, c, 52.0, spin, spin + 0.3, 3.0, fade(p.accent, 0.1), p.accent2);
            ui.add_space(14.0);
            if jobs.is_empty() {
                ui.label(RichText::new("Nothing to search yet").font(theme::semibold(22.0)).color(p.text));
                ui.add_space(6.0);
                for n in self.shared.notes() {
                    ui.label(RichText::new(n).color(p.weak));
                }
                return;
            }
            ui.label(RichText::new("Building your index").font(theme::semibold(22.0)).color(p.text));
            ui.add_space(4.0);
            let sub = if self.elevated {
                "Reading each drive's file table. This takes a few seconds."
            } else {
                if edition::get().admin {
                    "Listing every folder. Without administrator rights this can take a few minutes."
                } else {
                    "First run: listing every folder, which takes a little while. After this it opens instantly."
                }
            };
            ui.label(RichText::new(sub).color(p.weak));
            ui.add_space(24.0);
            for j in jobs {
                let (r, _) = ui.allocate_exact_size(vec2(420.0, 44.0), Sense::hover());
                let painter = ui.painter();
                let (done, total, items) = (j.done.load(Relaxed), j.total.load(Relaxed), j.items.load(Relaxed));
                let how = if j.mft { "file table" } else { "folders" };
                painter.text(pos2(r.min.x, r.min.y + 10.0), Align2::LEFT_CENTER, format!("{}:", j.letter), theme::semibold(14.0), p.text);
                painter.text(pos2(r.min.x + 26.0, r.min.y + 10.0), Align2::LEFT_CENTER, how, theme::font(12.5), p.faint);
                painter.text(pos2(r.max.x, r.min.y + 10.0), Align2::RIGHT_CENTER, format!("{} items", fmt::count(items)), theme::font(12.5), p.weak);
                let bar = Rect::from_min_size(pos2(r.min.x, r.min.y + 26.0), vec2(r.width(), 6.0));
                if total > 0 {
                    theme::bar(painter, bar, done as f32 / total as f32, p.track, p.accent, p.accent2);
                } else {
                    painter.rect_filled(bar, 3.0, p.track);
                    let w = bar.width() * 0.25;
                    let x = bar.min.x + (bar.width() + w) * (time * 0.6).fract() - w;
                    let seg = Rect::from_min_max(pos2(x.max(bar.min.x), bar.min.y), pos2((x + w).min(bar.max.x), bar.max.y));
                    if seg.width() > 0.0 {
                        painter.rect_filled(seg, 3.0, p.accent);
                    }
                }
            }
        });
    }

    // ----------------------------------------------------------------- results

    fn results(&mut self, ui: &mut Ui, p: &Pal) -> Option<Act> {
        let mut act = None;
        if self.show_apps() {
            if let Some(a) = self.apps_strip(ui, p) {
                act = Some(a);
            }
        }
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(RichText::new("Files and folders").font(theme::semibold(13.5)).color(p.text));
            ui.label(RichText::new(fmt::count(self.hits.len() as u64)).size(12.5).color(p.faint));
            // How the search was read: typos forgiven, or words taken as filters.
            if self.fuzzy && !self.hits.is_empty() {
                ui.add_space(6.0);
                ui.label(RichText::new("✦ No exact matches, showing close spellings").size(12.0).color(p.warn));
            }
            if !self.understood.is_empty() {
                ui.add_space(6.0);
                let text = format!("✦ Understood as {}", self.understood.join(" · "));
                ui.label(RichText::new(text).size(12.0).color(p.accent));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(6.0);
                ui.label(RichText::new(fmt::duration(self.took)).size(12.0).color(p.faint));
                let arrow = match (self.sort, self.desc) {
                    (Sort::Best, _) => "",
                    (_, true) => "  ↓",
                    (_, false) => "  ↑",
                };
                let title = RichText::new(format!("Sort: {}{arrow}  ▾", self.sort.title())).size(12.5).color(p.weak);
                ui.menu_button(title, |ui| {
                    ui.set_min_width(170.0);
                    for s in Sort::ALL {
                        if ui.selectable_label(self.sort == s, s.title()).clicked() {
                            if self.sort == s && s != Sort::Best {
                                self.desc = !self.desc;
                            } else {
                                self.sort = s;
                                self.desc = matches!(s, Sort::Size | Sort::Modified);
                            }
                            ui.close_menu();
                        }
                    }
                });
            });
        });
        ui.add_space(2.0);

        if self.hits.is_empty() {
            ui.add_space(50.0);
            ui.vertical_centered(|ui| {
                if self.searching {
                    ui.label(RichText::new("Searching…").color(p.weak));
                    return;
                }
                let (r, _) = ui.allocate_exact_size(vec2(64.0, 64.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 30.0, p.raised);
                theme::magnifier(ui.painter(), r.center(), p.faint, 2.0);
                ui.add_space(10.0);
                ui.label(RichText::new("No matches").font(theme::semibold(17.0)).color(p.text));
                ui.add_space(2.0);
                ui.label(RichText::new("Try fewer words, another type, or looser filters.").color(p.weak));
                if self.filters.count() > 0 {
                    ui.add_space(8.0);
                    if ui.button("Clear filters").clicked() {
                        self.filters = Filters::default();
                    }
                }
            });
            return act;
        }

        let mut sa = ScrollArea::vertical().id_salt("hits").auto_shrink([false; 2]);
        if std::mem::take(&mut self.scroll_top) {
            sa = sa.vertical_scroll_offset(0.0);
        } else if let Some(r) = self.scroll_to.take() {
            let y = r as f32 * ROW_H;
            if y < self.scroll_off {
                sa = sa.vertical_scroll_offset(y);
            } else if y + ROW_H > self.scroll_off + self.view_h {
                sa = sa.vertical_scroll_offset(y + ROW_H - self.view_h);
            }
        }

        let age = self.results_at.elapsed().as_secs_f32();
        let mut animating = false;
        let mut sel = self.selected;
        let mut toggle: Option<(u8, u32)> = None;
        let (hits, visible, expanded) = (&self.hits, &self.visible, &self.expanded);
        let hl = self.highlight.as_deref();
        let home = self.home.as_str();
        let icons = &mut self.icons;
        let shared = self.shared.clone();
        let ix = shared.read();
        let out = ui
            .scope(|ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                sa.show_rows(ui, ROW_H, visible.len(), |ui, range| {
                    for i in range {
                        let vis = visible[i];
                        let h = hits[vis.hit as usize];
                        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                        let Some(v) = ix.vol(h.vol as char) else { continue };
                        let e = v.entry(h.idx);
                        // New results slide in, a few rows at a time.
                        let a = if i < 24 { ease((age - i as f32 * 0.016) / 0.26) } else { 1.0 };
                        animating |= a < 1.0;
                        let rect_a = rect.translate(vec2((1.0 - a) * 14.0, 0.0));
                        let selected = sel.is_some_and(|s| s.same(h));
                        // The "+37" / "Hide" pill on a group's first row.
                        let group = vis.group.filter(|_| !vis.member).map(|len| {
                            let key = (h.vol, h.idx);
                            let open = expanded.contains(&key);
                            let pill = Rect::from_min_size(pos2(rect.max.x - 14.0 - 112.0 - 92.0, rect.center().y - 12.0), vec2(80.0, 24.0));
                            let pr = ui.interact(pill, Id::new(("group", key)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
                            if pr.clicked() {
                                toggle = Some(key);
                            }
                            let area = if open { String::new() } else { pretty_path(&v.path(v.ancestor(h.idx, 3)), home) };
                            GroupDraw { more: len - 1, open, area, pill, hovered: pr.hovered() }
                        });
                        let painter = ui.painter();
                        row_bg(painter, rect, selected, resp.hovered(), p);
                        draw_row(painter, rect_a, v, h, &e, hl, icons, p, a, home, group.as_ref(), vis.member);
                        if resp.clicked() || resp.secondary_clicked() {
                            sel = Some(h);
                        }
                        if resp.double_clicked() {
                            act = Some(Act::Open(h));
                        }
                        if e.kind != NONE {
                            resp.context_menu(|ui| {
                                if let Some(a) = item_menu(ui, h, e.kind == DIR) {
                                    act = Some(a);
                                }
                            });
                        }
                    }
                })
            })
            .inner;
        drop(ix);
        if animating {
            ui.ctx().request_repaint();
        }
        if let Some(key) = toggle {
            if !self.expanded.remove(&key) {
                self.expanded.insert(key);
            }
            self.rebuild_visible();
        }
        self.scroll_off = out.state.offset.y;
        self.view_h = out.inner_rect.height();
        self.selected = sel;
        act
    }

    fn apps_strip(&mut self, ui: &mut Ui, p: &Pal) -> Option<Act> {
        let mut act = None;
        let age = self.results_at.elapsed().as_secs_f32();
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(RichText::new("Apps & settings").font(theme::semibold(13.5)).color(p.text));
        });
        ui.add_space(4.0);
        let mut animating = false;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(10.0, 10.0);
            ui.add_space(4.0);
            for (n, app) in self.app_hits.iter().enumerate() {
                let name = &app.name;
                let (r, resp) = ui.allocate_exact_size(vec2(128.0, 108.0), Sense::click());
                let resp = resp.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(format!("Open {name}"));
                let a = ease((age - n as f32 * 0.04) / 0.3);
                animating |= a < 1.0;
                let ht = ease(ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.14));
                let card = r.translate(vec2(0.0, (1.0 - a) * 10.0 - ht * 2.0));
                let painter = ui.painter();
                let first = n == 0;
                if ht > 0.0 {
                    painter.rect_filled(card.translate(vec2(0.0, 4.0)).expand(1.0), 14.0, fade(Color32::BLACK, 0.18 * ht));
                }
                let edge = if first { 0.65 } else { ht * 0.6 };
                painter.rect(
                    card,
                    14.0,
                    fade(mix(p.raised, p.raised_hover, ht), a),
                    Stroke::new(1.0_f32, fade(mix(p.border, p.accent, edge), a)),
                    StrokeKind::Inside,
                );
                let ic = Rect::from_center_size(pos2(card.center().x, card.min.y + 32.0), vec2(38.0, 38.0));
                match self.icons.icon(&apps::icon_key(app), || apps::icon_path(app)) {
                    Some(tex) => {
                        painter.image(tex, ic, uv(), fade(Color32::WHITE, a));
                    }
                    None => {
                        painter.rect_filled(ic.shrink(3.0), 9.0, fade(theme::cat_color(Some(Cat::App), p.dark), a));
                    }
                }
                let mut job = LayoutJob::simple(name.clone(), theme::font(12.0), fade(p.text, a), 112.0);
                job.halign = Align::Center;
                job.wrap.max_rows = 2;
                let g = painter.layout_job(job);
                painter.galley(pos2(card.center().x, card.min.y + 58.0), g, p.text);
                painter.text(
                    pos2(card.center().x, card.max.y - 11.0),
                    Align2::CENTER_CENTER,
                    app.kind.label(),
                    theme::font(10.5),
                    fade(p.faint, a),
                );
                if first {
                    let tag = Rect::from_min_size(pos2(card.max.x - 28.0, card.min.y + 7.0), vec2(21.0, 16.0));
                    painter.rect_filled(tag, 4.0, fade(p.accent, 0.18 * a));
                    painter.text(tag.center(), Align2::CENTER_CENTER, "↵", theme::font(11.0), fade(p.accent, a));
                }
                if resp.clicked() {
                    act = Some(Act::Launch(app.clone()));
                }
            }
        });
        if animating {
            ui.ctx().request_repaint();
        }
        ui.add_space(12.0);
        act
    }

    // ----------------------------------------------------------------- preview

    fn preview_panel(&mut self, ui: &mut Ui, p: &Pal) -> Option<Act> {
        let h = self.preview?;
        let (name, path, folder, e, cat) = {
            let ix = self.shared.read();
            let v = ix.vol(h.vol as char)?;
            let e = v.entry(h.idx);
            if e.kind == NONE {
                return None;
            }
            (v.name(h.idx).to_string(), v.path(h.idx), v.path(e.parent), e, search::entry_cat(v, h.idx, &e))
        };
        let mut act = None;
        let w = PREVIEW_W - 32.0;
        ui.set_width(w);
        ui.add_space(4.0);

        // Close button
        ui.horizontal(|ui| {
            ui.label(RichText::new("Preview").size(12.0).color(p.faint));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(egui::Button::new(RichText::new("✕").size(12.0).color(p.weak)).frame(false)).clicked() {
                    self.selected = None;
                }
            });
        });
        ui.add_space(6.0);

        let (r, _) = ui.allocate_exact_size(vec2(w, 220.0), Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(r, 16.0, p.raised);
        let tint = theme::cat_color(cat, p.dark);
        painter.circle_filled(r.center(), 80.0, fade(tint, 0.06));
        match self.icons.thumb(&path) {
            Some((tex, size)) => {
                let area = r.shrink(16.0);
                let scale = (area.width() / size.x).min(area.height() / size.y).min(1.0);
                let img = Rect::from_center_size(r.center(), size * scale);
                painter.image(tex, img, uv(), Color32::WHITE);
            }
            None => theme::fallback_icon(painter, Rect::from_center_size(r.center(), vec2(80.0, 80.0)), e.kind == DIR, tint),
        }
        ui.add_space(14.0);
        ui.add(egui::Label::new(RichText::new(&name).font(theme::semibold(17.0)).color(p.text)).wrap());
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let (d, _) = ui.allocate_exact_size(vec2(8.0, 16.0), Sense::hover());
            ui.painter().circle_filled(d.center(), 4.0, tint);
            ui.label(RichText::new(kind_label(&name, e.kind, cat)).size(12.5).color(p.weak));
        });
        ui.add_space(14.0);

        let info = |ui: &mut Ui, label: &str, value: &str| {
            ui.horizontal_top(|ui| {
                let (lr, _) = ui.allocate_exact_size(vec2(72.0, 18.0), Sense::hover());
                ui.painter().text(pos2(lr.min.x, lr.center().y), Align2::LEFT_CENTER, label, theme::font(12.0), p.faint);
                ui.add(egui::Label::new(RichText::new(value).size(12.5).color(p.text)).wrap());
            });
            ui.add_space(2.0);
        };
        if e.kind == FILE {
            info(ui, "Size", &format!("{}  ({} bytes)", fmt::size(e.size), fmt::count(e.size)));
        }
        info(ui, "Modified", &format!("{}  ·  {}", fmt::date(e.mtime), fmt::ago(e.mtime)));
        info(ui, "Location", &folder);
        ui.add_space(16.0);

        if primary_button(ui, if e.kind == DIR { "Open folder" } else { "Open" }, w, p).clicked() {
            act = Some(Act::Open(h));
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let half = (w - 6.0) / 2.0;
            if secondary_button(ui, "Show in folder", half, p).clicked() {
                act = Some(Act::Reveal(h));
            }
            if secondary_button(ui, "Copy path", half, p).clicked() {
                act = Some(Act::CopyPath(h));
            }
        });
        if e.kind == DIR {
            ui.add_space(6.0);
            if secondary_button(ui, "Search in this folder", w, p).clicked() {
                act = Some(Act::SearchIn(h));
            }
            ui.add_space(6.0);
            if secondary_button(ui, "See what's taking space", w, p).clicked() {
                act = Some(Act::Analyze(h));
            }
        }
        act
    }
}

impl eframe::App for App {
    /// Saves walked indexes, so the next start opens them instantly.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        tray::remove();
        self.close_space();
        self.shared.save_walked();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let t0 = Instant::now();
        if trace::on() {
            if let Some(last) = self.last_frame {
                let gap = last.elapsed().as_secs_f64() * 1000.0;
                if gap > 150.0 {
                    trace::log(format!("{gap:.0} ms since the previous frame (focused: {})", self.focused));
                }
            }
            self.last_frame = Some(t0);
        }
        // Closing first: a close asked for while hidden mustn't close what a signal just opened.
        self.closing(ctx);
        self.signals(ctx);
        self.track_focus(ctx);
        self.appear_when_ready(ctx);
        match self.mode {
            Mode::Full => self.frame(ctx),
            Mode::Bar => self.bar(ctx),
            Mode::Hidden => {
                trace::log("frame while hidden");
                self.poll();
            }
        }
        // Started in the tray: eframe shows the window after the first frame.
        if self.frames == 0 && self.mode == Mode::Hidden {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        // Open maximized. eframe keeps the window hidden until a frame has been
        // painted, and maximizing a hidden window doesn't stick, so do it once it shows.
        if self.frames == 2 && self.mode == Mode::Full {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
        }
        // With SPEED_TRACE set: the first frames, and any slow one.
        self.frames += 1;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        if trace::on() && (self.frames <= 5 || ms > 30.0) {
            trace::log(format!("frame {}: {ms:.1} ms  {}", self.frames, std::mem::take(&mut self.marks).join("  ")));
        }
        self.marks.clear();
    }
}

impl App {
    /// Notes how long the frame has taken so far, for the trace.
    fn mark(&mut self, what: &str, since: Instant) {
        if trace::on() {
            self.marks.push(format!("{what} {:.1}", since.elapsed().as_secs_f64() * 1000.0));
        }
    }

    /// Behind a game, minimized or in the tray: drop to idle priority and stop
    /// redrawing for changes on disk until the window is back in front.
    fn track_focus(&mut self, ctx: &egui::Context) {
        let focused = self.mode != Mode::Hidden
            && ctx.input(|i| i.viewport().focused.unwrap_or(true) && !i.viewport().minimized.unwrap_or(false));
        if focused != self.focused {
            trace::log(format!("focused: {focused} ({:?}, drawing at {:?})", self.mode, ctx.screen_rect().size()));
            self.focused = focused;
            self.shared.set_foreground(focused);
            win::set_idle(!focused);
            if focused && self.apps_at.elapsed() > APPS_STALE {
                load_apps(&self.apps, ctx);
                self.apps_at = Instant::now();
            }
        }
    }

    fn frame(&mut self, ctx: &egui::Context) {
        if self.settings.space {
            return self.space_frame(ctx);
        }
        let t0 = Instant::now();
        let p = theme::pal(ctx.style().visuals.dark_mode);
        let focused = self.focused;
        self.icons.poll(ctx);
        self.poll();
        let generation = self.shared.read().generation;
        if generation != self.last_gen {
            self.last_gen = generation;
            // One ripple every few seconds at most, however busy the disk is.
            if focused && self.pulse_at.elapsed() > Duration::from_millis(2500) {
                self.pulse_at = Instant::now();
            }
        }
        self.settings_keys(ctx);
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Num2)) {
            self.set_view(ctx, true);
        }
        let mut act = self.keyboard(ctx);
        self.mark("prep", t0);

        egui::TopBottomPanel::top("top")
            .frame(Frame::new().fill(p.chrome).inner_margin(Margin { left: 14, right: 14, top: 10, bottom: 4 }))
            .show_separator_line(false)
            .show(ctx, |ui| self.top(ui, &p));
        self.mark("top", t0);
        let restart = egui::TopBottomPanel::bottom("status")
            .frame(Frame::new().fill(p.chrome).inner_margin(Margin::symmetric(16, 7)))
            .show_separator_line(false)
            .show(ctx, |ui| self.status(ui, &p))
            .inner;
        self.mark("status", t0);

        if self.selected.is_some() {
            self.preview = self.selected;
        }
        let open = self.selected.is_some() && self.active();
        let t = ease(ctx.animate_bool_with_time(Id::new("preview-open"), open, 0.24));
        if t > 0.001 && self.preview.is_some() {
            egui::SidePanel::right("preview")
                .resizable(false)
                .exact_width(PREVIEW_W * t)
                .show_separator_line(false)
                .frame(Frame::new().fill(p.chrome).inner_margin(Margin { left: 16, right: 16, top: 12, bottom: 12 }))
                .show(ctx, |ui| {
                    ui.multiply_opacity(t);
                    ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                        if let Some(a) = self.preview_panel(ui, &p) {
                            act = Some(a);
                        }
                    });
                });
        }

        egui::CentralPanel::default()
            .frame(Frame::new().fill(p.bg).inner_margin(Margin { left: 12, right: 12, top: 10, bottom: 0 }))
            .show(ctx, |ui| {
                let ready = !self.shared.read().vols.is_empty();
                let a = if !ready {
                    self.indexing(ui, &p);
                    None
                } else if !self.active() {
                    self.landing(ui, &p)
                } else {
                    self.results(ui, &p)
                };
                if a.is_some() {
                    act = a;
                }
            });
        self.mark("central", t0);
        self.settings_card(ctx, &p);

        if let Some(a) = act {
            self.act(ctx, a);
        }
        if restart && win::relaunch_as_admin() {
            self.quit(ctx);
        }
        self.ask_if_needed(ctx);
        self.mark("ask", t0);
        if !self.shared.jobs().is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if focused && self.pulse_at.elapsed() < Duration::from_millis(900) {
            ctx.request_repaint();
        }
        if let Some((_, at)) = &self.toast {
            let left = Duration::from_secs(3).saturating_sub(at.elapsed());
            if !left.is_zero() {
                ctx.request_repaint_after(left.min(Duration::from_millis(50)));
            }
        }
    }
}

// ---------------------------------------------------------------------- space

impl App {
    /// Switches between search and the Space view, and remembers which was showing.
    fn set_view(&mut self, ctx: &egui::Context, space: bool) {
        if self.settings.space == space {
            return;
        }
        self.settings.space = space;
        self.settings.save();
        if !space {
            self.focus = true;
        }
        ctx.request_repaint();
    }

    /// Opens the Space view on a drive or folder and scans it (or reopens its last scan).
    pub(super) fn analyze(&mut self, ctx: &egui::Context, path: String) {
        self.set_view(ctx, true);
        self.space_view.get_or_insert_with(|| space::App::new(ctx.clone(), None, None)).scan_path(path);
    }

    /// Lets go of the Space view's size tree, saving what live updates changed.
    /// Not while it's scanning or cleaning up, which would be cut short.
    pub(super) fn release_space(&mut self) {
        if self.space_view.as_ref().is_some_and(|s| s.idle()) {
            self.close_space();
        }
    }

    pub(super) fn close_space(&mut self) {
        if let Some(mut s) = self.space_view.take() {
            s.shutdown();
        }
    }

    fn space_frame(&mut self, ctx: &egui::Context) {
        let p = theme::pal(ctx.style().visuals.dark_mode);
        self.settings_keys(ctx);
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Num1) || i.consume_key(Modifiers::COMMAND, Key::F)) {
            self.set_view(ctx, false);
            return;
        }
        let logo = self.logo.id();
        let (mut to_search, mut gear) = (false, false);
        let settings_open = self.settings_open;
        let view = self.space_view.get_or_insert_with(|| space::App::new(ctx.clone(), None, None));
        view.show(ctx, &mut |ui: &mut Ui| header_logo(ui, logo), &mut |ui: &mut Ui| {
            let (v, g) = header_trail(ui, true, settings_open, &p);
            to_search |= v == Some(false);
            gear |= g;
        });
        let search_in = view.search_in.take();
        let want_admin = std::mem::take(&mut view.want_admin);
        if gear {
            self.toggle_settings();
        }
        if let Some(dir) = search_in {
            self.filters.folder = Some(dir);
            self.filters.drive = None;
            self.selected = None;
            to_search = true;
        }
        if to_search {
            self.set_view(ctx, false);
        }
        self.settings_card(ctx, &p);
        if want_admin {
            self.close_space();
            if win::relaunch_as_admin() {
                self.quit(ctx);
            }
        }
    }
}

const SWITCH_W: f32 = 168.0;

/// Search | Space, the app's two views. Returns the one clicked (true: Space).
fn view_switch(ui: &mut Ui, space: bool, h: f32, p: &Pal) -> Option<bool> {
    let segs = [("Search", "Find files, folders and apps  (Ctrl+1)"), ("Space", "See what's taking disk space  (Ctrl+2)")];
    segmented(ui, "view-switch", &segs, space as usize, SWITCH_W, h, p).map(|i| i == 1)
}

/// A row of options with a sliding highlight on the chosen one. Each option
/// is (label, tooltip); an empty tooltip shows none. Returns the index of a
/// newly clicked option.
pub(crate) fn segmented(ui: &mut Ui, id: &str, segs: &[(&str, &str)], chosen: usize, w: f32, h: f32, p: &Pal) -> Option<usize> {
    let (r, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let painter = ui.painter().clone();
    let radius = 8.0_f32.min(h * 0.5);
    painter.rect(r, radius, p.field, Stroke::new(1.0_f32, p.border), StrokeKind::Inside);
    let n = segs.len().max(1) as f32;
    let at = ui.ctx().animate_value_with_time(Id::new(id), chosen as f32, 0.18);
    let seg_w = (r.width() - 6.0) / n;
    let knob = Rect::from_min_size(pos2(r.min.x + 3.0 + seg_w * at, r.min.y + 3.0), vec2(seg_w, h - 6.0));
    painter.rect_filled(knob, radius - 2.0, p.accent);
    let mut picked = None;
    for (k, (label, tip)) in segs.iter().enumerate() {
        let seg = Rect::from_min_size(pos2(r.min.x + 3.0 + seg_w * k as f32, r.min.y + 3.0), vec2(seg_w, h - 6.0));
        let mut resp = ui.interact(seg, Id::new((id, k)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        if !tip.is_empty() {
            resp = resp.on_hover_text(*tip);
        }
        let on = k == chosen;
        if resp.hovered() && !on {
            painter.rect_filled(seg, radius - 2.0, p.hover);
        }
        let near = (1.0 - (at - k as f32).abs()).clamp(0.0, 1.0);
        let idle = if resp.hovered() { p.text } else { p.weak };
        painter.text(seg.center(), Align2::CENTER_CENTER, *label, theme::semibold(13.0), mix(idle, p.on_accent, ease(near)));
        if resp.clicked() && !on {
            picked = Some(k);
        }
    }
    picked
}

// --------------------------------------------------------------------- pieces

fn short_dir(path: &str) -> &str {
    let t = path.trim_end_matches('\\');
    t.rsplit('\\').next().filter(|s| !s.is_empty()).unwrap_or(path)
}

fn kind_label(name: &str, kind: u8, cat: Option<Cat>) -> String {
    if kind == DIR {
        return "Folder".into();
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_uppercase()).filter(|e| !e.is_empty() && e.len() <= 8);
    let noun = match cat {
        Some(Cat::Picture) => "image",
        Some(Cat::Video) => "video",
        Some(Cat::Audio) => "audio",
        Some(Cat::Document) => "document",
        Some(Cat::Archive) => "archive",
        Some(Cat::App) => "program",
        _ => "file",
    };
    match ext {
        Some(e) => format!("{e} {noun}"),
        None => "File".into(),
    }
}

/// The real Windows icon for an entry: one per file type, one per program or shortcut.
fn entry_icon(icons: &mut Icons, v: &Volume, i: u32, e: &Entry) -> Option<TextureId> {
    if e.kind == DIR {
        return icons.icon("dir", || std::env::temp_dir().to_string_lossy().into_owned());
    }
    let ext = search::extension(v.lower(i)).map(|x| String::from_utf8_lossy(x).into_owned()).unwrap_or_default();
    if matches!(ext.as_str(), "exe" | "lnk" | "ico" | "url" | "msc" | "cpl" | "scr" | "appref-ms") {
        let path = v.path(i);
        return icons.icon(&path.clone(), || path);
    }
    icons.icon(&format!("ext:{ext}"), || v.path(i))
}

fn icon_or_fallback(painter: &Painter, r: Rect, tex: Option<TextureId>, dir: bool, color: Color32, a: f32) {
    match tex {
        Some(t) => {
            painter.image(t, r, uv(), fade(Color32::WHITE, a));
        }
        None => theme::fallback_icon(painter, r, dir, fade(color, a)),
    }
}

/// How a group's first row is drawn.
struct GroupDraw {
    more: u32,
    open: bool,
    /// Where the rest are, shown while collapsed.
    area: String,
    pill: Rect,
    hovered: bool,
}

/// `C:\Users\me\Documents\x` as `Home › Documents › x`.
fn pretty_path(path: &str, home: &str) -> String {
    let n = home.len();
    let in_home = n > 0
        && path.len() >= n
        && path.is_char_boundary(n)
        && path[..n].eq_ignore_ascii_case(home)
        && (path.len() == n || path.as_bytes()[n] == b'\\');
    let path = if in_home { format!("Home{}", &path[n..]) } else { path.to_string() };
    path.split('\\').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("  ›  ")
}

#[allow(clippy::too_many_arguments)]
fn draw_row(
    painter: &Painter,
    rect: Rect,
    v: &Volume,
    h: Hit,
    e: &Entry,
    hl: Option<&str>,
    icons: &mut Icons,
    p: &Pal,
    a: f32,
    home: &str,
    group: Option<&GroupDraw>,
    member: bool,
) {
    let cy = rect.center().y;
    // Members of an open group sit indented under it, along a guide line.
    let x0 = rect.min.x + if member { 40.0 } else { 14.0 };
    if member {
        let x = rect.min.x + 28.0;
        painter.line_segment([pos2(x, rect.min.y), pos2(x, rect.max.y)], Stroke::new(1.0_f32, fade(p.border, a)));
    }
    let (size_w, date_w) = (86.0, 112.0);
    let right = rect.max.x - 14.0;
    let text_w = (right - date_w - size_w - x0 - 44.0).max(60.0);
    if e.kind == NONE {
        theme::text_at(painter, x0 + 42.0, cy, "Deleted", theme::font(13.5), fade(p.faint, a), text_w);
        return;
    }
    let ir = Rect::from_center_size(pos2(x0 + 14.0, cy), vec2(28.0, 28.0));
    let cat = search::entry_cat(v, h.idx, e);
    if group.is_some_and(|g| !g.open) {
        // A little stack behind the icon: there's more where this came from.
        for k in [2.0, 1.0] {
            let r = ir.translate(vec2(k * 3.0, -k * 3.0)).shrink(2.0);
            painter.rect_filled(r, 5.0, fade(p.raised_hover, a * (1.0 - k * 0.25)));
        }
    }
    icon_or_fallback(painter, ir, entry_icon(icons, v, h.idx, e), e.kind == DIR, theme::cat_color(cat, p.dark), a);

    let name_color = if e.noisy() { mix(p.text, p.weak, 0.35) } else { p.text };
    let g = painter.layout_job(name_job(v.name(h.idx), hl, fade(name_color, a), fade(p.accent, a), text_w));
    painter.galley(pos2(x0 + 42.0, cy - 2.0 - g.size().y), g, p.text);
    match group.filter(|g| !g.open) {
        Some(gd) => {
            let text = format!("{} more like this in {}", fmt::count(gd.more as u64), gd.area);
            theme::text_at(painter, x0 + 42.0, cy + 9.0, &text, theme::font(11.5), fade(p.weak, a), text_w);
        }
        None => {
            let path = pretty_path(&v.path(e.parent), home);
            theme::text_at(painter, x0 + 42.0, cy + 9.0, &path, theme::font(11.5), fade(p.faint, a), text_w);
        }
    }

    match group {
        Some(gd) => {
            let fill = fade(p.accent, if gd.hovered { 0.26 } else { 0.14 } * a);
            painter.rect(gd.pill, 12.0, fill, Stroke::new(1.0_f32, fade(p.accent, 0.5 * a)), StrokeKind::Inside);
            let label = if gd.open { "Hide  ▴".to_string() } else { format!("+{}  ▾", fmt::count(gd.more as u64)) };
            painter.text(gd.pill.center(), Align2::CENTER_CENTER, label, theme::semibold(12.0), fade(p.accent, a));
        }
        None if e.kind == FILE => {
            painter.text(pos2(right - date_w - 12.0, cy), Align2::RIGHT_CENTER, fmt::size(e.size), theme::font(12.5), fade(p.weak, a));
        }
        None => {}
    }
    painter.text(pos2(right, cy), Align2::RIGHT_CENTER, fmt::ago(e.mtime), theme::font(12.0), fade(p.faint, a));
}

fn row_bg(painter: &Painter, rect: Rect, selected: bool, hovered: bool, p: &Pal) {
    let r = rect.shrink2(vec2(4.0, 2.0));
    if selected {
        painter.rect_filled(r, 10.0, p.sel);
        let pill = Rect::from_center_size(pos2(r.min.x + 3.0, r.center().y), vec2(3.0, 20.0));
        painter.rect_filled(pill, 1.5, p.accent);
    } else if hovered {
        painter.rect_filled(r, 10.0, p.hover);
    }
}

/// A result's name, with the searched word picked out.
fn name_job(name: &str, needle: Option<&str>, color: Color32, hl: Color32, max_w: f32) -> LayoutJob {
    let normal = TextFormat { font_id: theme::font(13.5), color, ..Default::default() };
    let span = needle
        .and_then(|n| {
            let lower = name.to_lowercase();
            // Only when lowercasing kept byte offsets the same.
            (lower.len() == name.len()).then(|| lower.find(n).map(|s| (s, s + n.len()))).flatten()
        })
        .filter(|&(s, e)| name.is_char_boundary(s) && name.is_char_boundary(e));
    let mut job = LayoutJob::default();
    match span {
        Some((s, e)) => {
            job.append(&name[..s], 0.0, normal.clone());
            job.append(&name[s..e], 0.0, TextFormat { font_id: theme::semibold(13.5), color: hl, ..Default::default() });
            job.append(&name[e..], 0.0, normal);
        }
        None => job.append(name, 0.0, normal),
    }
    job.wrap = TextWrapping::truncate_at_width(max_w.max(1.0));
    job
}

fn item_menu(ui: &mut Ui, h: Hit, dir: bool) -> Option<Act> {
    ui.set_min_width(200.0);
    let mut act = None;
    if ui.button("Open").clicked() {
        act = Some(Act::Open(h));
    }
    if ui.button("Show in folder").clicked() {
        act = Some(Act::Reveal(h));
    }
    if dir && ui.button("Search in this folder").clicked() {
        act = Some(Act::SearchIn(h));
    }
    if dir && ui.button("See what's taking space").clicked() {
        act = Some(Act::Analyze(h));
    }
    ui.separator();
    if ui.button("Copy full path").clicked() {
        act = Some(Act::CopyPath(h));
    }
    if ui.button("Copy name").clicked() {
        act = Some(Act::CopyName(h));
    }
    if act.is_some() {
        ui.close_menu();
    }
    act
}

#[allow(clippy::too_many_arguments)]
fn chip(ui: &mut Ui, id: Id, label: &str, count: Option<u64>, dot: Option<Color32>, active: bool, h: f32, p: &Pal) -> Response {
    let font = theme::font(12.5);
    let lw = ui.painter().layout_no_wrap(label.to_owned(), font.clone(), p.text).size().x;
    let ct = count.map(fmt::count);
    let cw = ct.as_ref().map_or(0.0, |c| ui.painter().layout_no_wrap(c.clone(), theme::font(11.5), p.faint).size().x + 7.0);
    let w = 24.0 + if dot.is_some() { 13.0 } else { 0.0 } + lw + cw;
    let (r, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let t = ease(ui.ctx().animate_bool_with_time(id, active, 0.16));
    let hover = ui.ctx().animate_bool_with_time(id.with("hover"), resp.hovered(), 0.1);
    let painter = ui.painter();
    let base = mix(p.raised, p.raised_hover, hover);
    let fill = mix(base, p.accent, t);
    painter.rect(r, h / 2.0, fill, Stroke::new(1.0_f32, mix(mix(p.border, p.accent, hover * 0.4), p.accent, t)), StrokeKind::Inside);
    let mut x = r.min.x + 12.0;
    if let Some(c) = dot {
        painter.circle_filled(pos2(x + 3.5, r.center().y), 3.5, mix(c, Color32::WHITE, t * 0.85));
        x += 13.0;
    }
    painter.text(pos2(x, r.center().y), Align2::LEFT_CENTER, label, font, mix(p.text, p.on_accent, t));
    if let Some(c) = ct {
        let color = if count == Some(0) { mix(p.faint, p.on_accent, t * 0.6) } else { mix(p.weak, p.on_accent, t * 0.85) };
        painter.text(pos2(x + lw + 7.0, r.center().y), Align2::LEFT_CENTER, c, theme::font(11.5), color);
    }
    resp
}

/// A filter that's on, shown next to the chips; clicking it turns it off.
fn pill(ui: &mut Ui, label: &str, p: &Pal) -> Response {
    let text = format!("{label}   ✕");
    let w = ui.painter().layout_no_wrap(text.clone(), theme::font(12.0), p.text).size().x + 22.0;
    let (r, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand).on_hover_text("Remove this filter");
    let painter = ui.painter();
    let fill = if resp.hovered() { fade(p.accent, 0.22) } else { fade(p.accent, 0.12) };
    painter.rect(r, 15.0, fill, Stroke::new(1.0_f32, fade(p.accent, 0.6)), StrokeKind::Inside);
    painter.text(r.center(), Align2::CENTER_CENTER, text, theme::font(12.0), p.text);
    resp
}

fn filter_row(ui: &mut Ui, title: &str, p: &Pal, add: impl FnOnce(&mut Ui)) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
        ui.add_space(SIDE);
        let (r, _) = ui.allocate_exact_size(vec2(74.0, 26.0), Sense::hover());
        ui.painter().text(pos2(r.min.x, r.center().y), Align2::LEFT_CENTER, title, theme::font(12.5), p.faint);
        add(ui);
    });
    ui.add_space(5.0);
}

/// The filter button inside the search field: a quiet icon that tints when
/// the panel is open, with a count of the filters that are on.
fn filter_button(ui: &mut Ui, r: Rect, count: usize, open: bool, p: &Pal) -> Response {
    let resp = ui.interact(r, Id::new("filter-button"), Sense::click());
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand).on_hover_text("Filters");
    let t = ease(ui.ctx().animate_bool_with_time(resp.id, open, 0.16));
    let hover = ui.ctx().animate_bool_with_time(resp.id.with("hover"), resp.hovered(), 0.1);
    let painter = ui.painter();
    let fill = mix(mix(p.field, p.hover, hover), fade(p.accent, 0.22), t);
    painter.rect_filled(r, 8.0, fill);
    theme::sliders(painter, r.center(), mix(mix(p.weak, p.text, hover), p.accent, t), fill);
    if count > 0 {
        let c = pos2(r.max.x - 4.0, r.min.y + 4.0);
        painter.circle(c, 7.0, p.accent, Stroke::new(2.0_f32, p.field));
        painter.text(c, Align2::CENTER_CENTER, count.to_string(), theme::bold(9.5), p.on_accent);
    }
    resp
}

/// A square icon button in the header (settings), same size and states as
/// the filter button.
fn icon_button(ui: &mut Ui, id: &str, on: bool, tip: &str, p: &Pal, draw: impl FnOnce(&egui::Painter, egui::Pos2, Color32, Color32)) -> Response {
    let (r, resp) = ui.allocate_exact_size(Vec2::splat(CTRL_H), Sense::click());
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(tip);
    let t = ease(ui.ctx().animate_bool_with_time(Id::new(id), on, 0.16));
    let hover = ui.ctx().animate_bool_with_time(Id::new(id).with("hover"), resp.hovered(), 0.1);
    let fill = mix(mix(p.field, p.raised_hover, hover), mix(p.field, p.accent, 0.22), t);
    let painter = ui.painter();
    painter.rect(r, 8.0, fill, Stroke::new(1.0_f32, mix(p.border, p.accent, t)), StrokeKind::Inside);
    draw(painter, r.center(), mix(mix(p.weak, p.text, hover), p.accent, t), fill);
    resp
}

/// Header height, and the height of the controls in it. Both views share them
/// so switching views doesn't shift anything.
pub(crate) const HEADER_H: f32 = 40.0;
pub(crate) const CTRL_H: f32 = 32.0;
/// Width of the header's right end: the view switch and the settings button.
const TRAIL_W: f32 = SWITCH_W + 8.0 + CTRL_H;

/// The logo at the header's left, in a slot as wide as the results' left
/// gutter, so whatever follows lines up with the list below.
pub(crate) fn header_logo(ui: &mut Ui, logo: egui::TextureId) {
    let (r, _) = ui.allocate_exact_size(vec2(SIDE - 10.0, HEADER_H), Sense::hover());
    ui.painter().image(logo, Rect::from_center_size(r.center(), vec2(28.0, 28.0)), uv(), Color32::WHITE);
}

/// The header's right end, the same in both views: Search | Space, then the
/// settings button. Works in a left-to-right or right-to-left row. Returns
/// the view clicked (true: Space) and whether settings was clicked.
pub(crate) fn header_trail(ui: &mut Ui, space: bool, settings_open: bool, p: &Pal) -> (Option<bool>, bool) {
    ui.spacing_mut().item_spacing.x = 8.0;
    let gear = |ui: &mut Ui| icon_button(ui, "settings-button", settings_open, "Settings", p, theme::gear).clicked();
    if ui.layout().prefer_right_to_left() {
        let g = gear(ui);
        (view_switch(ui, space, CTRL_H, p), g)
    } else {
        let v = view_switch(ui, space, CTRL_H, p);
        (v, gear(ui))
    }
}

/// Green dot, with a ripple when the index changes.
fn live_dot(ui: &mut Ui, p: &Pal, pulse_at: Instant) {
    let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
    let painter = ui.painter();
    let c = r.center();
    let t = pulse_at.elapsed().as_secs_f32() / 0.9;
    if t < 1.0 {
        painter.circle_stroke(c, 3.5 + 4.0 * ease(t), Stroke::new(1.5_f32, fade(p.good, 1.0 - t)));
    }
    painter.circle_filled(c, 3.5, p.good);
}

/// Reads installed apps, settings and tools in the background.
fn load_apps(apps: &Arc<Mutex<Vec<StartApp>>>, ctx: &egui::Context) {
    let (apps, ctx) = (apps.clone(), ctx.clone());
    std::thread::spawn(move || {
        win::low_priority_thread();
        let list = apps::load();
        trace::log(format!("apps list loaded ({})", list.len()));
        *lock(&apps) = list;
        ctx.request_repaint();
    });
}

fn tip(ui: &mut Ui, ex: &str, what: &str, p: &Pal) -> Response {
    let g = ui.painter().layout_no_wrap(ex.to_owned(), egui::FontId::monospace(12.0), p.text);
    let wg = ui.painter().layout_no_wrap(what.to_owned(), theme::font(12.0), p.faint);
    let w = g.size().x + wg.size().x + 30.0;
    let (r, resp) = ui.allocate_exact_size(vec2(w, 28.0), Sense::click());
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let painter = ui.painter();
    let fill = if resp.hovered() { p.raised_hover } else { p.raised };
    painter.rect(r, 8.0, fill, Stroke::new(1.0_f32, p.border), StrokeKind::Inside);
    painter.galley(pos2(r.min.x + 10.0, r.center().y - g.size().y * 0.5), g.clone(), p.text);
    painter.galley(pos2(r.min.x + 20.0 + g.size().x, r.center().y - wg.size().y * 0.5), wg, p.faint);
    resp
}

fn primary_button(ui: &mut Ui, text: &str, w: f32, p: &Pal) -> Response {
    let (r, resp) = ui.allocate_exact_size(vec2(w, 38.0), Sense::click());
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let hover = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let painter = ui.painter();
    // Gradient fill, matching the icon.
    let mut m = egui::Mesh::default();
    let inner = r.shrink2(vec2(10.0, 0.0));
    let (a, b) = (mix(p.accent, Color32::WHITE, 0.1 * hover), mix(p.accent2, Color32::WHITE, 0.1 * hover));
    painter.rect_filled(Rect::from_min_size(r.min, vec2(20.0, r.height())), 10.0, a);
    painter.rect_filled(Rect::from_min_size(pos2(r.max.x - 20.0, r.min.y), vec2(20.0, r.height())), 10.0, b);
    for (pos, col) in [(inner.left_top(), a), (inner.right_top(), b), (inner.right_bottom(), b), (inner.left_bottom(), a)] {
        m.colored_vertex(pos, col);
    }
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    painter.add(egui::Shape::mesh(m));
    painter.text(r.center(), Align2::CENTER_CENTER, text, theme::semibold(13.5), p.on_accent);
    resp
}

fn secondary_button(ui: &mut Ui, text: &str, w: f32, p: &Pal) -> Response {
    let (r, resp) = ui.allocate_exact_size(vec2(w, 34.0), Sense::click());
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let hover = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let painter = ui.painter();
    painter.rect(r, 10.0, mix(p.raised, p.raised_hover, hover), Stroke::new(1.0_f32, mix(p.border, p.accent, hover * 0.5)), StrokeKind::Inside);
    painter.text(r.center(), Align2::CENTER_CENTER, text, theme::font(13.0), p.text);
    resp
}
