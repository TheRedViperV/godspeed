//! The Space view: toolbar, tabs and the four views (folders, largest files,
//! file types, cleanup). The host app draws its own controls at the toolbar's left.

use crate::space::analysis::{self, Data};
use crate::space::cache;
use crate::space::clutter::{self, Action, Group, Safety, Status};
use crate::space::fmt;
use crate::space::live::{self, Change};
use crate::space::theme::{self, Pal};
use crate::space::tree::*;
use crate::space::treemap::{self, Cell, REST};
use crate::space::walk;
use crate::space::win;
use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, CursorIcon, Frame, Id, Key, Layout, Margin, Painter, PointerButton,
    Pos2, Rect, RichText, ScrollArea, Sense, Stroke, Ui, Vec2,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const ROW_H: f32 = 24.0;
const HEAD_H: f32 = 30.0;
const PAD: f32 = 8.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Folders,
    Files,
    Types,
    Cleanup,
}

struct ScanJob {
    root: String,
    /// Reopening a saved scan rather than scanning.
    loading: bool,
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    rx: mpsc::Receiver<Result<Data, String>>,
    started: Instant,
}

#[derive(Default)]
struct Explorer {
    expanded: HashSet<u32>,
    selected: Option<u32>,
    rows: Vec<(u32, u16)>,
    dirty: bool,
    scroll_node: Option<u32>,
    scroll_to: Option<usize>,
    visible: (usize, usize),
    tm_root: u32,
    tm: Option<TmCache>,
    legend: Option<(u32, u64, Vec<(Cat, u64)>)>,
    menu_node: Option<u32>,
}

struct TmCache {
    root: u32,
    gen: u64,
    size: (i32, i32),
    cells: Vec<Cell>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileSort {
    Name,
    Alloc,
    Size,
    Modified,
}

struct FilesView {
    filter: String,
    ext: Option<String>,
    sort: FileSort,
    desc: bool,
    rows: Vec<u32>,
    key: Option<(String, Option<String>, FileSort, bool, u64)>,
    selected: Option<u32>,
}

impl Default for FilesView {
    fn default() -> Self {
        FilesView {
            filter: String::new(),
            ext: None,
            sort: FileSort::Alloc,
            desc: true,
            rows: Vec::new(),
            key: None,
            selected: None,
        }
    }
}

enum ConfirmKind {
    Cleanup(Vec<(usize, usize)>),
    Node(u32),
}

struct Confirm {
    kind: ConfirmKind,
    permanent: bool,
    bytes: u64,
    count: usize,
    name: String,
    empties_bin: bool,
}

enum JobMsg {
    Item { g: usize, i: usize, status: Status, remaining: u64 },
    Node { node: u32, result: Result<(), String> },
    Finished(Option<String>),
}

/// Things views ask for while drawing; applied once the frame's UI is built.
#[derive(Default)]
struct Intents {
    select: Option<(u32, bool)>,
    zoom: Option<u32>,
    reveal: Option<String>,
    open: Option<String>,
    copy: Option<String>,
    recycle: Option<u32>,
    show_in_tree: Option<u32>,
    search_in: Option<String>,
    ext_filter: Option<String>,
    cleanup: Option<bool>,
}

pub struct App {
    drives: Vec<win::Drive>,
    target: String,
    elevated: bool,
    scan: Option<ScanJob>,
    data: Option<Data>,
    error: Option<String>,
    tab: Tab,
    ex: Explorer,
    fv: FilesView,
    show_all: HashSet<usize>,
    confirm: Option<Confirm>,
    job: Option<mpsc::Receiver<JobMsg>>,
    toast: Option<(String, Instant)>,
    /// Tab to open after the first scan (from `--tab`).
    start_tab: Option<Tab>,
    /// Watches the scanned folder and feeds changes into the tree.
    live: Option<live::Live>,
    ctx: egui::Context,
    /// Asked to restart with admin rights (the host does it).
    pub want_admin: bool,
    /// Asked to search inside this folder (the host switches to search).
    pub search_in: Option<String>,
}

impl App {
    pub fn new(ctx: egui::Context, initial: Option<String>, tab: Option<&str>) -> Self {
        let start_tab = match tab {
            Some("files") => Some(Tab::Files),
            Some("types") => Some(Tab::Types),
            Some("cleanup") => Some(Tab::Cleanup),
            _ => None,
        };
        let drives = win::drives();
        let target = initial
            .clone()
            .or_else(|| {
                drives
                    .iter()
                    .find(|d| d.root.eq_ignore_ascii_case("C:\\"))
                    .or(drives.first())
                    .map(|d| d.root.clone())
            })
            .unwrap_or_else(|| "C:\\".into());
        let mut app = App {
            drives,
            target: target.clone(),
            elevated: win::is_elevated(),
            scan: None,
            data: None,
            error: None,
            tab: Tab::Folders,
            ex: Explorer::default(),
            fv: FilesView::default(),
            show_all: HashSet::new(),
            confirm: None,
            job: None,
            toast: None,
            start_tab,
            live: None,
            ctx,
            want_admin: false,
            search_in: None,
        };
        if initial.is_some() {
            app.start_scan(target);
        } else if let Some(path) = cache::latest() {
            app.start_load(path);
        }
        app
    }

    fn start_scan(&mut self, root: String) {
        if self.scan.is_some() || self.job.is_some() {
            return;
        }
        self.target = root.clone();
        self.save_if_changed();
        self.data = None; // free the old tree before building a new one
        self.error = None;
        let progress = Arc::new(Progress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (p2, c2, r2) = (progress.clone(), cancel.clone(), root.clone());
        let spawned = std::thread::Builder::new().name("scan".into()).spawn(move || match analysis::run(&r2, false, &p2, &c2) {
            Ok(data) => {
                // Hand the result to the UI first; compressing and writing can happen after.
                let raw = cache::encode(&data);
                let root = data.tree.node(data.tree.root).name.to_string();
                let _ = tx.send(Ok(data));
                let _ = cache::write(&root, raw);
            }
            Err(e) => {
                let _ = tx.send(Err(e));
            }
        });
        if spawned.is_err() {
            self.error = Some("Couldn't start the scan.".into());
            return;
        }
        self.live = None;
        self.scan = Some(ScanJob { root, loading: false, progress, cancel, rx, started: Instant::now() });
    }

    /// Reopens a saved scan.
    fn start_load(&mut self, path: PathBuf) {
        if self.scan.is_some() || self.job.is_some() {
            return;
        }
        self.save_if_changed();
        self.data = None;
        self.error = None;
        self.live = None;
        let progress = Arc::new(Progress::default());
        progress.phase.store(PH_ANALYZE, Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let p2 = path.clone();
        let spawned = std::thread::Builder::new().name("load".into()).spawn(move || {
            let _ = tx.send(cache::load(&p2).map_err(|e| format!("Couldn't open the saved scan: {e}")));
        });
        if spawned.is_ok() {
            let root = path.file_stem().map(|s| s.to_string_lossy().to_uppercase()).unwrap_or_default();
            self.scan = Some(ScanJob { root, loading: true, progress, cancel, rx, started: Instant::now() });
        }
    }

    /// Writes the current tree back to the scan cache if live updates or cleanups changed it.
    fn save_if_changed(&mut self) {
        if let Some(d) = self.data.as_mut() {
            if d.saved_gen != d.tree.generation && cache::save(d).is_ok() {
                d.saved_gen = d.tree.generation;
            }
        }
    }

    fn install(&mut self, data: Data) {
        let root = data.tree.root;
        self.ex = Explorer { tm_root: root, selected: Some(root), dirty: true, ..Default::default() };
        self.ex.expanded.insert(root);
        self.fv = FilesView::default();
        self.show_all.clear();
        self.tab = self.start_tab.take().unwrap_or(Tab::Folders);
        self.target = data.tree.node(root).name.to_string();
        self.live = live::Live::start(&self.target, &self.ctx);
        self.data = Some(data);
    }

    fn poll(&mut self) {
        if let Some(job) = &self.scan {
            match job.rx.try_recv() {
                Ok(Ok(data)) => {
                    self.scan = None;
                    self.install(data);
                }
                Ok(Err(e)) => {
                    self.scan = None;
                    if e != "Cancelled" {
                        self.error = Some(e);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.scan = None;
                    self.error = Some("The scan stopped unexpectedly.".into());
                }
            }
        }
        let mut msgs = Vec::new();
        if let Some(rx) = &self.job {
            loop {
                match rx.try_recv() {
                    Ok(m) => msgs.push(m),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.job = None;
                        break;
                    }
                }
            }
        }
        for m in msgs {
            self.on_job(m);
        }
        let mut changes = Vec::new();
        if let Some(live) = &self.live {
            while let Ok(batch) = live.rx.try_recv() {
                changes.extend(batch);
            }
        }
        if !changes.is_empty() {
            self.apply_changes(changes);
        }
    }

    fn apply_changes(&mut self, changes: Vec<Change>) {
        let (Some(data), Some(live)) = (self.data.as_mut(), self.live.as_ref()) else { return };
        let tree = &mut data.tree;
        let before = tree.generation;
        for c in changes {
            match c {
                Change::Gone(path) => {
                    if let Some(n) = tree.find(&path).filter(|&n| n != tree.root) {
                        self.ex.forget(tree, n);
                        tree.remove(n);
                    }
                }
                Change::Exists { path, is_dir, size, alloc, mtime } => match tree.find(&path) {
                    Some(n) if tree.node(n).kind == FILE && !is_dir => tree.update_file(n, size, alloc, mtime),
                    Some(n) if (tree.node(n).kind == DIR) == is_dir => {}
                    Some(n) => {
                        // A file was replaced by a folder of the same name, or the reverse.
                        self.ex.forget(tree, n);
                        tree.remove(n);
                        add_new(tree, live, &path, is_dir, size, alloc, mtime);
                    }
                    None => add_new(tree, live, &path, is_dir, size, alloc, mtime),
                },
                Change::Subtree { path, nodes } => merge_subtree(tree, &path, nodes),
                Change::Overflow => {
                    let msg = "Too much changed at once to follow. Rescan for exact numbers.";
                    let recent = self.toast.as_ref().is_some_and(|(m, at)| m == msg && at.elapsed() < Duration::from_secs(60));
                    if !recent {
                        self.toast = Some((msg.into(), Instant::now()));
                    }
                }
            }
        }
        if tree.generation != before {
            self.ex.dirty = true;
        }
    }

    fn on_job(&mut self, m: JobMsg) {
        let Some(data) = self.data.as_mut() else { return };
        let tree = &mut data.tree;
        match m {
            JobMsg::Item { g, i, status, remaining } => {
                let action = data.clutter[g].action;
                let item = &mut data.clutter[g].items[i];
                item.status = status;
                let node = item.node;
                if tree.node(node).kind != NONE {
                    if remaining == 0 && action == Action::Item {
                        self.ex.forget(tree, node);
                        tree.remove(node);
                    } else {
                        let gone: Vec<u32> =
                            tree.kids(node).filter(|&k| !Path::new(&tree.path(k)).exists()).collect();
                        for k in gone {
                            self.ex.forget(tree, k);
                            tree.remove(k);
                        }
                        tree.shrink_to(node, remaining);
                    }
                }
                self.ex.dirty = true;
            }
            JobMsg::Node { node, result } => match result {
                Ok(()) => {
                    self.ex.forget(tree, node);
                    tree.remove(node);
                    self.ex.dirty = true;
                    self.toast = Some(("Moved to the Recycle Bin".into(), Instant::now()));
                }
                Err(e) => self.toast = Some((e, Instant::now())),
            },
            JobMsg::Finished(summary) => {
                self.job = None;
                if let Some(s) = summary {
                    self.toast = Some((s, Instant::now()));
                }
            }
        }
    }

    fn target_is_drive(&self) -> bool {
        self.drives.iter().any(|d| d.root.eq_ignore_ascii_case(&self.target))
    }

    fn target_label(&self) -> String {
        if let Some(d) = self.drives.iter().find(|d| d.root.eq_ignore_ascii_case(&self.target)) {
            return format!("{}  {}", d.letter(), d.name());
        }
        let chars: Vec<char> = self.target.chars().collect();
        if chars.len() > 34 {
            format!("…{}", chars[chars.len() - 33..].iter().collect::<String>())
        } else {
            self.target.clone()
        }
    }

    // ---------------------------------------------------------------- chrome

    fn toolbar(&mut self, ui: &mut Ui, p: &Pal, lead: &mut dyn FnMut(&mut Ui), trail: &mut dyn FnMut(&mut Ui)) {
        let busy = self.scan.is_some() || self.job.is_some();
        let mut new_target: Option<String> = None;
        let (mut scan, mut pick, mut relaunch) = (false, false, false);
        ui.horizontal(|ui| {
            // Same row height and control height as the Search view's header.
            ui.set_min_height(crate::app::HEADER_H);
            ui.spacing_mut().interact_size.y = crate::app::CTRL_H;
            ui.spacing_mut().item_spacing.x = 10.0;
            lead(ui);
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.add_enabled_ui(!busy, |ui| {
                egui::ComboBox::from_id_salt("target")
                    .width(240.0)
                    .selected_text(RichText::new(self.target_label()).color(p.text))
                    .show_ui(ui, |ui| {
                        ui.set_min_width(320.0);
                        for d in &self.drives {
                            let mut job = egui::text::LayoutJob::default();
                            let fmt_main = egui::TextFormat { font_id: theme::font(13.0), color: p.text, ..Default::default() };
                            let fmt_weak = egui::TextFormat { font_id: theme::font(12.0), color: p.faint, ..Default::default() };
                            job.append(&format!("{}  {}", d.letter(), d.name()), 0.0, fmt_main);
                            job.append(&format!("{} free of {}", fmt::size(d.free), fmt::size(d.total)), 14.0, fmt_weak);
                            if ui.selectable_label(self.target.eq_ignore_ascii_case(&d.root), job).clicked() {
                                new_target = Some(d.root.clone());
                            }
                        }
                        if !self.target_is_drive() {
                            let _ = ui.selectable_label(true, self.target.as_str());
                        }
                    });
                let rescan = self
                    .data
                    .as_ref()
                    .is_some_and(|d| eq_ci(&d.tree.node(d.tree.root).name, &self.target));
                if theme::primary(ui, if rescan { "Rescan" } else { "Scan" }, p).clicked() {
                    scan = true;
                }
                if theme::secondary(ui, "Scan a folder…", p).clicked() {
                    pick = true;
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                trail(ui);
                ui.add_space(8.0);
                if self.elevated {
                    ui.label(RichText::new("Administrator").size(12.0).color(p.faint))
                        .on_hover_text("Drive scans read the NTFS file table directly.");
                } else if crate::edition::get().admin
                    && theme::secondary(ui, "Restart as administrator", p)
                        .on_hover_text("Reads the NTFS file table directly, so a full drive scans in seconds instead of minutes.")
                    .clicked()
                {
                    relaunch = true;
                }
            });
        });
        if let Some(t) = new_target {
            let showing = self.data.as_ref().is_some_and(|d| eq_ci(&d.tree.node(d.tree.root).name, &t));
            match cache::path_for(&t).filter(|p| p.exists()) {
                Some(saved) if !showing => self.start_load(saved),
                _ => self.target = t,
            }
        }
        if scan {
            self.start_scan(self.target.clone());
        }
        if pick {
            if let Some(dir) = rfd::FileDialog::new().set_title("Choose a folder to scan").pick_folder() {
                self.start_scan(dir.to_string_lossy().into_owned());
            }
        }
        if relaunch {
            self.want_admin = true;
        }
    }

    fn tab_bar(&mut self, ui: &mut Ui, p: &Pal) {
        let Some(d) = &self.data else { return };
        let tree = &d.tree;
        let cleanable: u64 = d.clutter.iter().filter(|g| g.deletable()).map(|g| g.total(tree)).sum();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let cleanup_extra = fmt::size(cleanable);
            let tabs = [
                (Tab::Folders, "Folders", None),
                (Tab::Files, "Largest files", None),
                (Tab::Types, "File types", None),
                (Tab::Cleanup, "Cleanup", Some(cleanup_extra.as_str())),
            ];
            for (t, label, extra) in tabs {
                if tab_button(ui, label, extra, self.tab == t, p).clicked() {
                    self.tab = t;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let root = tree.node(tree.root);
                let mut summary = format!("{} on disk · {} files", fmt::size(root.alloc), fmt::count(root.files as u64));
                if let Some((total, free)) = d.volume {
                    summary.push_str(&format!(" · {} free of {}", fmt::size(free), fmt::size(total)));
                }
                ui.label(RichText::new(summary).size(12.5).color(p.weak));
                ui.add(egui::Label::new(RichText::new(&*root.name).font(theme::semibold(13.0)).color(p.text)).truncate())
                    .on_hover_text(&*root.name);
            });
        });
    }

    fn status_bar(&mut self, ui: &mut Ui, p: &Pal) {
        let small = |s: String, c: Color32| RichText::new(s).size(12.0).color(c);
        ui.horizontal(|ui| {
            if let Some(d) = &self.data {
                let t = &d.tree;
                let root = t.node(t.root);
                ui.label(small(
                    format!(
                        "Scanned {} with {} in {} · {} files · {} folders",
                        cache::age(d.scanned_at),
                        t.method.label().to_lowercase().replace("ntfs", "NTFS"),
                        fmt::duration(t.elapsed),
                        fmt::count(root.files as u64),
                        fmt::count(t.dirs),
                    ),
                    p.weak,
                ))
                .on_hover_text("Click Rescan to start over from scratch.");
                if self.live.is_some() {
                    let r = ui.label(small("· Updating live".into(), p.good));
                    r.on_hover_text("Changes to files are picked up while the app is open. Changes made while it was closed need a rescan.");
                }
                if t.skipped > 0 {
                    ui.label(small(format!("· {} folders couldn't be read", fmt::count(t.skipped)), p.faint));
                }
                if let Some(n) = &d.note {
                    ui.label(small(format!("· Folder walk used because {n}"), p.faint));
                }
            } else if self.scan.is_some() {
                ui.label(small("Scanning…".into(), p.weak));
            } else {
                ui.label(small("Ready".into(), p.weak));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some((msg, at)) = &self.toast {
                    if at.elapsed() < Duration::from_secs(6) {
                        ui.label(small(msg.clone(), p.text));
                    }
                }
            });
        });
    }

    // ---------------------------------------------------------------- states

    fn landing(&mut self, ui: &mut Ui, p: &Pal) {
        let mut scan: Option<String> = None;
        let mut pick = false;
        let w = 640.0f32.min(ui.available_width() - 48.0);
        ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            ui.add_space(56.0);
            ui.horizontal(|ui| {
                ui.add_space(((ui.available_width() - w) * 0.5).max(24.0));
                ui.vertical(|ui| {
                    ui.set_width(w);
                    ui.label(RichText::new("Choose a drive").font(theme::semibold(22.0)).color(p.text));
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new("See every folder and file by the space it takes, and clear out what you don't need.")
                            .color(p.weak),
                    );
                    if let Some(e) = &self.error {
                        ui.add_space(10.0);
                        ui.label(RichText::new(e).color(p.danger));
                    }
                    ui.add_space(18.0);
                    for d in &self.drives {
                        if drive_row(ui, d, w, p).clicked() {
                            scan = Some(d.root.clone());
                        }
                        ui.add_space(8.0);
                    }
                    ui.add_space(4.0);
                    if theme::secondary(ui, "Scan a folder…", p).clicked() {
                        pick = true;
                    }
                    if !self.elevated && crate::edition::get().admin {
                        ui.add_space(26.0);
                        ui.label(RichText::new("Running without administrator rights").font(theme::semibold(13.0)).color(p.text));
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new(
                                "Drives are scanned folder by folder, which can take a few minutes on a full disk. \
                                 Restart as administrator (top right) and the NTFS file table is read instead, \
                                 usually in a few seconds.",
                            )
                            .color(p.weak),
                        );
                    }
                });
            });
        });
        if let Some(root) = scan {
            self.start_scan(root);
        }
        if pick {
            if let Some(dir) = rfd::FileDialog::new().set_title("Choose a folder to scan").pick_folder() {
                self.start_scan(dir.to_string_lossy().into_owned());
            }
        }
    }

    fn progress_view(&mut self, ui: &mut Ui, p: &Pal) {
        let Some(job) = &self.scan else { return };
        let pr = &job.progress;
        let phase = pr.phase.load(Relaxed);
        let items = pr.items.load(Relaxed);
        let (title, detail, frac) = match phase {
            PH_MFT => {
                let total = pr.total.load(Relaxed).max(1);
                let f = pr.done.load(Relaxed) as f32 / total as f32;
                ("Reading the NTFS file table", format!("{} records", fmt::count(items)), Some(f))
            }
            PH_WALK => ("Walking folders", format!("{} items found", fmt::count(items)), None),
            _ if job.loading => ("Reading saved results", String::new(), None),
            PH_ANALYZE => ("Adding up folder sizes", String::new(), None),
            _ => ("Starting", String::new(), None),
        };
        let heading = if job.loading { "Opening your last scan".to_string() } else { format!("Scanning {}", job.root) };
        let elapsed = fmt::duration(job.started.elapsed());
        let detail = if detail.is_empty() { elapsed } else { format!("{detail} · {elapsed}") };
        let mut cancel = false;
        ui.add_space((ui.available_height() * 0.34).max(40.0));
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(heading).font(theme::semibold(18.0)).color(p.text));
            ui.add_space(4.0);
            ui.label(RichText::new(title).color(p.weak));
            ui.add_space(16.0);
            let (r, _) = ui.allocate_exact_size(vec2(360.0, 4.0), Sense::hover());
            let painter = ui.painter();
            match frac {
                Some(f) => theme::bar(painter, r, f, p.track, p.accent),
                None => {
                    painter.rect_filled(r, 2.0, p.track);
                    let t = (ui.input(|i| i.time) as f32 * 0.7).fract();
                    let seg = r.width() * 0.28;
                    let x0 = r.min.x - seg + (r.width() + seg) * t;
                    let s = Rect::from_min_max(pos2(x0.max(r.min.x), r.min.y), pos2((x0 + seg).min(r.max.x), r.max.y));
                    painter.rect_filled(s, 2.0, p.accent);
                }
            }
            ui.add_space(12.0);
            ui.label(RichText::new(detail).size(12.0).color(p.faint));
            ui.add_space(18.0);
            if theme::secondary(ui, "Cancel", p).clicked() {
                cancel = true;
            }
        });
        if cancel {
            job.cancel.store(true, Relaxed);
        }
    }

    // ---------------------------------------------------------------- folders

    fn folders_view(&mut self, ui: &mut Ui, p: &Pal, it: &mut Intents) {
        let Some(data) = self.data.as_ref() else { return };
        let tree = &data.tree;
        let ex = &mut self.ex;
        if ex.dirty {
            rebuild_rows(tree, ex);
        }
        if let Some(n) = ex.scroll_node.take() {
            ex.scroll_to = ex.rows.iter().position(|r| r.0 == n);
        }
        let total_w = ui.available_width();
        egui::SidePanel::left("space-tree")
            .resizable(true)
            .default_width((total_w * 0.5).max(420.0))
            .width_range(380.0..=(total_w - 320.0).max(380.0))
            .frame(Frame::new().fill(p.bg))
            .show_inside(ui, |ui| tree_table(ui, tree, ex, p, it));
        egui::CentralPanel::default()
            .frame(Frame::new().fill(p.bg).inner_margin(Margin { left: 14, right: 14, top: 6, bottom: 10 }))
            .show_inside(ui, |ui| treemap_panel(ui, tree, ex, p, it));
    }

    // ---------------------------------------------------------------- files

    fn files_view(&mut self, ui: &mut Ui, p: &Pal, it: &mut Intents) {
        let Some(data) = self.data.as_ref() else { return };
        let tree = &data.tree;
        let fv = &mut self.fv;

        Frame::new().inner_margin(Margin { left: 16, right: 16, top: 10, bottom: 8 }).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut fv.filter)
                        .hint_text("Filter by name or folder")
                        .desired_width(300.0),
                );
                if let Some(ext) = fv.ext.clone() {
                    let label = if ext.is_empty() { "No extension".to_string() } else { format!(".{ext}") };
                    if ui
                        .add(egui::Button::new(RichText::new(format!("{label}   ×")).size(12.5).color(p.text)))
                        .on_hover_text("Show all file types")
                        .clicked()
                    {
                        fv.ext = None;
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let what = if fv.ext.is_some() || !fv.filter.is_empty() { "matching files" } else { "largest files" };
                    ui.label(RichText::new(format!("{} {what}", fmt::count(fv.rows.len() as u64))).size(12.5).color(p.weak));
                });
            });
        });

        let key = (fv.filter.clone(), fv.ext.clone(), fv.sort, fv.desc, tree.generation);
        if fv.key.as_ref() != Some(&key) {
            let mut rows = match &fv.ext {
                Some(e) => analysis::files_with_ext(tree, e, 10_000),
                // Recomputed rather than taken from the scan so live updates show up.
                None => analysis::top_files(tree, 10_000),
            };
            rows.retain(|&n| tree.node(n).kind == FILE);
            let q = fv.filter.trim().to_lowercase();
            if !q.is_empty() {
                rows.retain(|&n| tree.path(n).to_lowercase().contains(&q));
            }
            match fv.sort {
                FileSort::Name => rows.sort_by_cached_key(|&n| tree.node(n).name.to_lowercase()),
                FileSort::Alloc => rows.sort_by_key(|&n| tree.node(n).alloc),
                FileSort::Size => rows.sort_by_key(|&n| tree.node(n).size),
                FileSort::Modified => rows.sort_by_key(|&n| tree.node(n).mtime),
            }
            if fv.desc {
                rows.reverse();
            }
            fv.rows = rows;
            fv.key = Some(key);
            ui.ctx().request_repaint();
        }

        let full = ui.available_rect_before_wrap();
        let spec = [
            (Col::Name, W::Flex(0.42)),
            (Col::Alloc, W::Fixed(88.0)),
            (Col::Size, W::Fixed(88.0)),
            (Col::Modified, W::Fixed(98.0)),
            (Col::Folder, W::Flex(0.58)),
        ];
        let cols = place(&spec, full.left(), full.right());
        let sort_col = match fv.sort {
            FileSort::Name => Col::Name,
            FileSort::Alloc => Col::Alloc,
            FileSort::Size => Col::Size,
            FileSort::Modified => Col::Modified,
        };
        if let Some(c) = header(ui, &cols, p, Some((sort_col, fv.desc)), true) {
            let s = match c {
                Col::Name => Some(FileSort::Name),
                Col::Alloc => Some(FileSort::Alloc),
                Col::Size => Some(FileSort::Size),
                Col::Modified => Some(FileSort::Modified),
                _ => None,
            };
            if let Some(s) = s {
                if s == fv.sort {
                    fv.desc = !fv.desc;
                } else {
                    fv.sort = s;
                    fv.desc = s != FileSort::Name;
                }
            }
        }
        if fv.rows.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| ui.label(RichText::new("No files match.").color(p.weak)));
            return;
        }
        let n = fv.rows.len();
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            ScrollArea::vertical().id_salt("files").auto_shrink([false; 2]).show_rows(ui, ROW_H, n, |ui, range| {
                for i in range {
                    let node = fv.rows[i];
                    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                    let nd = tree.node(node);
                    let painter = ui.painter();
                    row_bg(painter, rect, fv.selected == Some(node), resp.hovered(), p);
                    let cy = rect.center().y;
                    for &(c, x0, x1) in &cols {
                        match c {
                            Col::Name => {
                                theme::file_icon(painter, pos2(x0 + PAD + 6.0, cy), theme::cat_color(cat_of(&nd.name), p.dark));
                                let tx = x0 + PAD + 20.0;
                                theme::text_at(painter, tx, cy, &nd.name, theme::font(13.0), p.text, x1 - tx - 6.0);
                            }
                            Col::Alloc => right_text(painter, x1, cy, fmt::size(nd.alloc), p.text),
                            Col::Size => right_text(painter, x1, cy, fmt::size(nd.size), p.weak),
                            Col::Modified => left_text(painter, x0, x1, cy, &fmt::date(nd.mtime), p.weak),
                            Col::Folder => left_text(painter, x0, x1, cy, &tree.path(nd.parent), p.faint),
                            _ => {}
                        }
                    }
                    if resp.clicked() || resp.secondary_clicked() {
                        fv.selected = Some(node);
                    }
                    if resp.double_clicked() {
                        it.reveal = Some(tree.path(node));
                    }
                    resp.context_menu(|ui| node_menu(ui, tree, node, it, true));
                }
            });
        });
    }

    // ---------------------------------------------------------------- types

    fn types_view(&mut self, ui: &mut Ui, p: &Pal, it: &mut Intents) {
        let Some(data) = self.data.as_mut() else { return };
        if data.types_gen != data.tree.generation {
            data.types = analysis::type_stats(&data.tree);
            data.types_gen = data.tree.generation;
        }
        let total = data.tree.node(data.tree.root).alloc;
        ui.add_space(4.0);
        let full = ui.available_rect_before_wrap();
        let spec = [
            (Col::Type, W::Flex(1.0)),
            (Col::Kind, W::Fixed(190.0)),
            (Col::Alloc, W::Fixed(90.0)),
            (Col::Total, W::Fixed(140.0)),
            (Col::Files, W::Fixed(96.0)),
            (Col::Avg, W::Fixed(96.0)),
        ];
        let cols = place(&spec, full.left(), full.right());
        header(ui, &cols, p, None, false);
        let types = &data.types;
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            ScrollArea::vertical().id_salt("types").auto_shrink([false; 2]).show_rows(ui, ROW_H, types.len(), |ui, range| {
                for i in range {
                    let t = &types[i];
                    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                    let painter = ui.painter();
                    row_bg(painter, rect, false, resp.hovered(), p);
                    let cy = rect.center().y;
                    for &(c, x0, x1) in &cols {
                        match c {
                            Col::Type => {
                                let sw = Rect::from_center_size(pos2(x0 + PAD + 5.0, cy), vec2(10.0, 10.0));
                                painter.rect_filled(sw, 2.0, theme::cat_color(t.cat, p.dark));
                                let label = if t.ext.is_empty() { "No extension".to_string() } else { format!(".{}", t.ext) };
                                theme::text_at(painter, x0 + PAD + 18.0, cy, &label, theme::font(13.0), p.text, x1 - x0 - 30.0);
                            }
                            Col::Kind => left_text(painter, x0, x1, cy, t.cat.name(), p.weak),
                            Col::Alloc => right_text(painter, x1, cy, fmt::size(t.alloc), p.text),
                            Col::Total => share_cell(painter, x0, x1, cy, t.alloc, total, p),
                            Col::Files => right_text(painter, x1, cy, fmt::count(t.count), p.weak),
                            Col::Avg => right_text(painter, x1, cy, fmt::size(t.alloc / t.count.max(1)), p.weak),
                            _ => {}
                        }
                    }
                    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
                    if resp.clicked() {
                        it.ext_filter = Some(t.ext.clone());
                    }
                }
            });
        });
    }

    // ---------------------------------------------------------------- cleanup

    fn cleanup_view(&mut self, ui: &mut Ui, p: &Pal, it: &mut Intents) {
        let busy = self.job.is_some();
        let show_all = &mut self.show_all;
        let Some(data) = self.data.as_mut() else { return };
        let tree = &data.tree;
        let groups = &mut data.clutter;
        // Sizes can change after the scan (live updates, partial cleanups).
        for item in groups.iter_mut().flat_map(|g| g.items.iter_mut()) {
            if item.status == Status::Pending && tree.node(item.node).kind != NONE {
                item.alloc = tree.node(item.node).alloc;
            }
        }

        let (mut sel_n, mut sel_b) = (0usize, 0u64);
        for g in groups.iter().filter(|g| g.deletable()) {
            for item in g.live(tree).filter(|i| i.selected && i.status == Status::Pending) {
                sel_n += 1;
                sel_b += item.alloc;
            }
        }

        egui::TopBottomPanel::bottom("space-cleanup-bar")
            .frame(Frame::new().fill(p.chrome).inner_margin(Margin::symmetric(18, 10)))
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    if sel_n == 0 {
                        ui.label(RichText::new("Select items to clean up").color(p.weak));
                    } else {
                        let what = if sel_n == 1 { "1 item selected".to_string() } else { format!("{sel_n} items selected") };
                        ui.label(RichText::new(what).font(theme::semibold(13.0)).color(p.text));
                        ui.label(RichText::new(fmt::size(sel_b)).color(p.weak));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let enabled = sel_n > 0 && !busy;
                        if ui.add_enabled_ui(enabled, |ui| theme::primary(ui, "Delete…", p)).inner.clicked() {
                            it.cleanup = Some(true);
                        }
                        if ui.add_enabled_ui(enabled, |ui| theme::secondary(ui, "Move to Recycle Bin…", p)).inner.clicked() {
                            it.cleanup = Some(false);
                        }
                        if busy {
                            ui.label(RichText::new("Cleaning up…").color(p.weak));
                            ui.add(egui::Spinner::new().size(14.0).color(p.weak));
                        }
                    });
                });
            });

        egui::CentralPanel::default().frame(Frame::new().fill(p.bg)).show_inside(ui, |ui| {
            ScrollArea::vertical().id_salt("cleanup").auto_shrink([false; 2]).show(ui, |ui| {
                Frame::new().inner_margin(Margin { left: 20, right: 20, top: 18, bottom: 24 }).show(ui, |ui| {
                    let sum = |s: Safety| -> u64 { groups.iter().filter(|g| g.safety == s).map(|g| g.total(tree)).sum() };
                    let (safe, review) = (sum(Safety::Safe), sum(Safety::Review));
                    if groups.is_empty() {
                        ui.label(RichText::new("Nothing to clean up").font(theme::semibold(20.0)).color(p.text));
                        ui.label(RichText::new("No caches, build output or leftovers worth listing were found.").color(p.weak));
                        return;
                    }
                    ui.label(
                        RichText::new(format!("{} can most likely be freed", fmt::size(safe)))
                            .font(theme::semibold(20.0))
                            .color(p.text),
                    );
                    ui.add_space(2.0);
                    let mut sub = String::new();
                    if review > 0 {
                        sub.push_str(&format!("Another {} is worth reviewing. ", fmt::size(review)));
                    }
                    sub.push_str("Nothing is removed until you select items and confirm.");
                    ui.label(RichText::new(sub).color(p.weak));
                    ui.add_space(16.0);
                    for (gi, g) in groups.iter_mut().enumerate() {
                        group_box(ui, gi, g, tree, p, it, busy, show_all);
                        ui.add_space(10.0);
                    }
                });
            });
        });
    }

    // ---------------------------------------------------------------- input & actions

    fn keyboard(&mut self, ctx: &egui::Context, it: &mut Intents) {
        if self.tab != Tab::Folders || self.confirm.is_some() || self.scan.is_some() {
            return;
        }
        let Some(data) = self.data.as_ref() else { return };
        if ctx.memory(|m| m.focused().is_some() || m.any_popup_open()) {
            return;
        }
        let tree = &data.tree;
        let ex = &mut self.ex;
        let n = ex.rows.len();
        if n == 0 {
            return;
        }
        let k = |key: Key| ctx.input(|i| i.key_pressed(key));
        let idx = ex.selected.and_then(|s| ex.rows.iter().position(|r| r.0 == s));
        let mut go = |to: usize| it.select = Some((ex.rows[to.min(n - 1)].0, false));
        if k(Key::ArrowDown) {
            go(idx.map_or(0, |i| i + 1));
        }
        if k(Key::ArrowUp) {
            go(idx.map_or(0, |i| i.saturating_sub(1)));
        }
        if k(Key::PageDown) {
            go(idx.map_or(0, |i| i + 20));
        }
        if k(Key::PageUp) {
            go(idx.map_or(0, |i| i.saturating_sub(20)));
        }
        if k(Key::Home) {
            go(0);
        }
        if k(Key::End) {
            go(n - 1);
        }
        let Some(sel) = ex.selected else { return };
        let nd = tree.node(sel);
        if nd.kind == NONE {
            return;
        }
        if k(Key::ArrowRight) && nd.kind == DIR && tree.has_kids(sel) {
            if ex.expanded.insert(sel) {
                ex.dirty = true;
            } else if let Some(i) = idx.filter(|i| i + 1 < n) {
                it.select = Some((ex.rows[i + 1].0, false));
            }
        }
        if k(Key::ArrowLeft) {
            if ex.expanded.remove(&sel) {
                ex.dirty = true;
            } else if sel != tree.root {
                it.select = Some((nd.parent, false));
            }
        }
        if k(Key::Enter) {
            if nd.kind == DIR {
                it.open = Some(tree.path(sel));
            } else {
                it.reveal = Some(tree.path(sel));
            }
        }
        if k(Key::Delete) && sel != tree.root {
            it.recycle = Some(sel);
        }
        if k(Key::Backspace) && ex.tm_root != tree.root {
            it.zoom = Some(tree.node(ex.tm_root).parent);
        }
    }

    fn apply(&mut self, ctx: &egui::Context, it: Intents) {
        if let Some(s) = it.copy {
            ctx.copy_text(s);
            self.toast = Some(("Path copied".into(), Instant::now()));
        }
        if let Some(s) = it.reveal {
            win::reveal(&s);
        }
        if let Some(s) = it.open {
            win::open(&s);
        }
        if let Some(dir) = it.search_in {
            self.search_in = Some(dir);
        }
        if let Some(e) = it.ext_filter {
            self.fv.ext = Some(e);
            self.fv.filter.clear();
            self.tab = Tab::Files;
        }
        let Some(data) = self.data.as_ref() else { return };
        let tree = &data.tree;
        if let Some(n) = it.show_in_tree {
            self.tab = Tab::Folders;
            self.ex.select(tree, n, false);
        }
        if let Some((n, from_treemap)) = it.select {
            self.ex.select(tree, n, from_treemap);
        }
        if let Some(n) = it.zoom {
            self.ex.select(tree, n, true);
            self.ex.tm_root = n;
        }
        if let Some(n) = it.recycle {
            let nd = tree.node(n);
            self.confirm = Some(Confirm {
                kind: ConfirmKind::Node(n),
                permanent: false,
                bytes: nd.alloc,
                count: 1,
                name: nd.name.to_string(),
                empties_bin: false,
            });
        }
        if let Some(permanent) = it.cleanup {
            let (mut items, mut bytes, mut empties_bin) = (Vec::new(), 0, false);
            for (gi, g) in data.clutter.iter().enumerate().filter(|(_, g)| g.deletable()) {
                for (ii, item) in g.items.iter().enumerate() {
                    if item.selected && item.status == Status::Pending && tree.node(item.node).kind != NONE {
                        items.push((gi, ii));
                        bytes += item.alloc;
                        empties_bin |= g.action == Action::EmptyBin;
                    }
                }
            }
            if !items.is_empty() {
                self.confirm = Some(Confirm {
                    count: items.len(),
                    kind: ConfirmKind::Cleanup(items),
                    permanent,
                    bytes,
                    name: String::new(),
                    empties_bin,
                });
            }
        }
    }

    fn confirm_modal(&mut self, ctx: &egui::Context, p: &Pal) {
        let Some(c) = &self.confirm else { return };
        let items = if c.count == 1 { "1 item".to_string() } else { format!("{} items", c.count) };
        let (title, mut body, button) = match (&c.kind, c.permanent) {
            (ConfirmKind::Cleanup(_), true) => (
                format!("Delete {items}?"),
                format!("{} will be deleted permanently. Anything that's in use is skipped. This can't be undone.", fmt::size(c.bytes)),
                "Delete",
            ),
            (ConfirmKind::Cleanup(_), false) => (
                format!("Move {items} to the Recycle Bin?"),
                format!("{} will be moved to the Recycle Bin. The space is only freed once you empty it.", fmt::size(c.bytes)),
                "Move to Recycle Bin",
            ),
            (ConfirmKind::Node(_), _) => (
                format!("Move “{}” to the Recycle Bin?", c.name),
                format!("{} will be moved to the Recycle Bin. You can restore it from there.", fmt::size(c.bytes)),
                "Move to Recycle Bin",
            ),
        };
        if c.empties_bin {
            body.push_str(" The Recycle Bin will also be emptied.");
        }
        let mut decision: Option<bool> = None;
        let modal = egui::Modal::new(Id::new("confirm")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.label(RichText::new(title).font(theme::semibold(17.0)).color(p.text));
            ui.add_space(8.0);
            ui.label(RichText::new(body).color(p.weak));
            ui.add_space(22.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if theme::danger(ui, button).clicked() {
                    decision = Some(true);
                }
                if theme::secondary(ui, "Cancel", p).clicked() {
                    decision = Some(false);
                }
            });
        });
        if decision.is_none() && modal.should_close() {
            decision = Some(false);
        }
        match decision {
            Some(true) => {
                if let Some(c) = self.confirm.take() {
                    self.execute(c);
                }
            }
            Some(false) => self.confirm = None,
            None => {}
        }
    }

    fn execute(&mut self, c: Confirm) {
        let Some(data) = self.data.as_mut() else { return };
        let (tx, rx) = mpsc::channel();
        match c.kind {
            ConfirmKind::Cleanup(items) => {
                let mut work = Vec::new();
                for (g, i) in items {
                    let group = &mut data.clutter[g];
                    let action = group.action;
                    let item = &mut group.items[i];
                    item.status = Status::Working;
                    item.selected = false;
                    work.push((g, i, item.path.clone(), action, item.alloc));
                }
                let permanent = c.permanent;
                std::thread::spawn(move || {
                    let (mut freed, mut left) = (0u64, 0u64);
                    for (g, i, path, action, before) in work {
                        let out = clutter::delete(&path, action, permanent);
                        let remaining = walk::measure(Path::new(&path));
                        let gone = before.saturating_sub(remaining);
                        freed += gone;
                        left += out.failed;
                        let status = match (&out.error, out.failed) {
                            (Some(e), _) if gone == 0 => Status::Failed(short(e)),
                            (_, 0) if permanent || action == Action::EmptyBin => Status::Done(format!("Freed {}", fmt::size(gone))),
                            (_, 0) => Status::Done("Moved to Recycle Bin".into()),
                            (_, n) => Status::Done(format!("{} cleared, {} in use", fmt::size(gone), fmt::count(n))),
                        };
                        let _ = tx.send(JobMsg::Item { g, i, status, remaining });
                    }
                    let mut summary = if permanent {
                        format!("Freed {}", fmt::size(freed))
                    } else {
                        format!("Moved {} to the Recycle Bin", fmt::size(freed))
                    };
                    if left > 0 {
                        summary.push_str(&format!(" · skipped {} in use", fmt::count(left)));
                    }
                    let _ = tx.send(JobMsg::Finished(Some(summary)));
                });
            }
            ConfirmKind::Node(n) => {
                let path = data.tree.path(n);
                std::thread::spawn(move || {
                    let result = trash::delete(&path).map_err(|e| short(&format!("Couldn't move it to the Recycle Bin: {e}")));
                    let _ = tx.send(JobMsg::Node { node: n, result });
                    let _ = tx.send(JobMsg::Finished(None));
                });
            }
        }
        self.job = Some(rx);
    }
}

fn short(s: &str) -> String {
    let s = s.lines().next().unwrap_or("");
    if s.chars().count() > 90 {
        format!("{}…", s.chars().take(89).collect::<String>())
    } else {
        s.to_string()
    }
}

impl App {
    /// Nothing running that dropping the view would cut short (a scan, a cleanup).
    pub fn idle(&self) -> bool {
        self.scan.is_none() && self.job.is_none() && self.confirm.is_none()
    }

    /// Scans a folder (from search's "See what's taking space").
    pub fn scan_path(&mut self, path: String) {
        let showing = self.data.as_ref().is_some_and(|d| eq_ci(&d.tree.node(d.tree.root).name, &path));
        if !showing {
            self.start_scan(path);
        }
    }

    /// Draws the whole view. `lead` draws the host's controls at the toolbar's left.
    pub fn show(&mut self, ctx: &egui::Context, lead: &mut dyn FnMut(&mut Ui), trail: &mut dyn FnMut(&mut Ui)) {
        let p = theme::pal(ctx.style().visuals.dark_mode);
        self.poll();
        let mut it = Intents::default();

        egui::TopBottomPanel::top("space-toolbar")
            .frame(Frame::new().fill(p.chrome).inner_margin(Margin { left: 14, right: 14, top: 10, bottom: 10 }))
            .show_separator_line(false)
            .show(ctx, |ui| self.toolbar(ui, &p, lead, trail));
        egui::TopBottomPanel::bottom("space-status")
            .frame(Frame::new().fill(p.chrome).inner_margin(Margin::symmetric(16, 7)))
            .show_separator_line(false)
            .show(ctx, |ui| self.status_bar(ui, &p));
        if self.data.is_some() && self.scan.is_none() {
            egui::TopBottomPanel::top("space-tabs")
                .frame(Frame::new().fill(p.bg).inner_margin(Margin { left: 8, right: 14, top: 0, bottom: 0 }))
                .show(ctx, |ui| self.tab_bar(ui, &p));
        }
        egui::CentralPanel::default().frame(Frame::new().fill(p.bg)).show(ctx, |ui| {
            if self.scan.is_some() {
                self.progress_view(ui, &p);
            } else if self.data.is_none() {
                self.landing(ui, &p);
            } else {
                match self.tab {
                    Tab::Folders => self.folders_view(ui, &p, &mut it),
                    Tab::Files => self.files_view(ui, &p, &mut it),
                    Tab::Types => self.types_view(ui, &p, &mut it),
                    Tab::Cleanup => self.cleanup_view(ui, &p, &mut it),
                }
            }
        });
        self.keyboard(ctx, &mut it);
        self.apply(ctx, it);
        self.confirm_modal(ctx, &p);

        if self.scan.is_some() || self.job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(60));
        } else if let Some((_, at)) = &self.toast {
            let left = Duration::from_secs(6).saturating_sub(at.elapsed());
            if !left.is_zero() {
                ctx.request_repaint_after(left);
            }
        }
    }

    /// Stops watching and saves what live updates changed.
    pub fn shutdown(&mut self) {
        self.live = None;
        self.save_if_changed();
    }
}

/// A node's name for breadcrumbs: a scanned folder's root shows just its own name.
fn short_name(tree: &Tree, n: u32) -> &str {
    let name = &*tree.node(n).name;
    if n == tree.root {
        if let Some((_, last)) = name.trim_end_matches('\\').rsplit_once('\\') {
            if !last.is_empty() {
                return last;
            }
        }
    }
    name
}

/// Splits `C:\a\b` into (`C:\a`, `b`). None for a drive root.
fn split_path(path: &str) -> Option<(String, &str)> {
    let (parent, name) = path.trim_end_matches('\\').rsplit_once('\\')?;
    if name.is_empty() {
        return None;
    }
    let parent = if parent.len() == 2 && parent.ends_with(':') { format!("{parent}\\") } else { parent.to_string() };
    Some((parent, name))
}

/// Finds a folder in the tree, creating it (and any missing parents) if it appeared
/// after the scan. New folders get their contents read in the background.
fn ensure_dir(tree: &mut Tree, live: &live::Live, path: &str) -> Option<u32> {
    if let Some(n) = tree.find(path) {
        return (tree.node(n).kind == DIR).then_some(n);
    }
    let (parent_path, name) = split_path(path)?;
    let parent = ensure_dir(tree, live, &parent_path)?;
    let n = tree.insert(parent, name, DIR, 0, 0, 0);
    live.fetch_subtree(path.to_string());
    Some(n)
}

#[allow(clippy::too_many_arguments)]
fn add_new(tree: &mut Tree, live: &live::Live, path: &str, is_dir: bool, size: u64, alloc: u64, mtime: u64) {
    let Some((parent_path, name)) = split_path(path) else { return };
    let Some(parent) = ensure_dir(tree, live, &parent_path) else { return };
    if tree.child_named(parent, name).is_some() {
        return; // already added, e.g. by a folder read that finished first
    }
    if is_dir {
        tree.insert(parent, name, DIR, 0, 0, mtime);
        live.fetch_subtree(path.to_string());
    } else {
        tree.insert(parent, name, FILE, size, alloc, mtime);
    }
}

/// Adds a freshly read folder's contents, skipping entries already present.
fn merge_subtree(tree: &mut Tree, path: &str, nodes: Vec<Node>) {
    let Some(dir) = tree.find(path).filter(|&d| tree.node(d).kind == DIR) else { return };
    let mut map = vec![u32::MAX; nodes.len()];
    // Folders created during this merge start empty, so their children need no lookup.
    let mut fresh = vec![false; nodes.len()];
    map[0] = dir;
    fresh[0] = !tree.has_kids(dir);
    for (i, nd) in nodes.iter().enumerate().skip(1) {
        let lp = nd.parent as usize;
        let parent = map[lp];
        if parent == u32::MAX {
            continue;
        }
        let existing = if fresh[lp] { None } else { tree.child_named(parent, &nd.name) };
        map[i] = match existing {
            Some(e) if (tree.node(e).kind == DIR) == (nd.kind == DIR) => {
                if nd.kind == FILE {
                    tree.update_file(e, nd.size, nd.alloc, nd.mtime);
                }
                e
            }
            Some(_) => continue,
            None => {
                fresh[i] = true;
                tree.insert(parent, &nd.name, nd.kind, nd.size, nd.alloc, nd.mtime)
            }
        };
    }
}

impl Explorer {
    fn select(&mut self, tree: &Tree, n: u32, from_treemap: bool) {
        if tree.node(n).kind == NONE {
            return;
        }
        self.selected = Some(n);
        if !from_treemap {
            let nd = tree.node(n);
            self.tm_root = if nd.kind == DIR { n } else { nd.parent };
        }
        for a in tree.ancestors(n) {
            if a != n && self.expanded.insert(a) {
                self.dirty = true;
            }
        }
        self.scroll_node = Some(n);
    }

    /// Moves selection and the treemap out of a subtree that's about to be removed.
    fn forget(&mut self, tree: &Tree, n: u32) {
        let parent = tree.node(n).parent;
        if self.selected.is_some_and(|s| tree.ancestors(s).contains(&n)) {
            self.selected = Some(parent);
        }
        if tree.ancestors(self.tm_root).contains(&n) {
            self.tm_root = parent;
        }
    }
}

fn rebuild_rows(tree: &Tree, ex: &mut Explorer) {
    ex.rows.clear();
    let mut stack = vec![(tree.root, 0u16)];
    while let Some((n, depth)) = stack.pop() {
        ex.rows.push((n, depth));
        if tree.node(n).kind == DIR && ex.expanded.contains(&n) {
            let kids = tree.sorted_kids(n);
            stack.extend(kids.into_iter().rev().map(|k| (k, depth + 1)));
        }
    }
    ex.dirty = false;
}

// -------------------------------------------------------------------- columns

#[derive(Clone, Copy, PartialEq, Eq)]
enum Col {
    Name,
    Share,
    Total,
    Alloc,
    Size,
    Files,
    Modified,
    Folder,
    Type,
    Kind,
    Avg,
}

impl Col {
    fn title(self) -> &'static str {
        match self {
            Col::Name => "Name",
            Col::Share => "% of parent",
            Col::Total => "% of total",
            Col::Alloc => "On disk",
            Col::Size => "Size",
            Col::Files => "Files",
            Col::Modified => "Modified",
            Col::Folder => "Folder",
            Col::Type => "Type",
            Col::Kind => "Kind",
            Col::Avg => "Average",
        }
    }

    fn right(self) -> bool {
        matches!(self, Col::Alloc | Col::Size | Col::Files | Col::Avg)
    }
}

#[derive(Clone, Copy)]
enum W {
    Fixed(f32),
    Flex(f32),
}

fn place(spec: &[(Col, W)], left: f32, right: f32) -> Vec<(Col, f32, f32)> {
    let fixed: f32 = spec.iter().map(|s| if let W::Fixed(f) = s.1 { f } else { 0.0 }).sum();
    let weights: f32 = spec.iter().map(|s| if let W::Flex(f) = s.1 { f } else { 0.0 }).sum();
    let free = (right - left - fixed - PAD * 2.0).max(80.0);
    let mut x = left + PAD;
    spec.iter()
        .map(|&(c, w)| {
            let width = match w {
                W::Fixed(f) => f,
                W::Flex(k) => free * k / weights.max(0.001),
            };
            let out = (c, x, x + width);
            x += width;
            out
        })
        .collect()
}

/// Column headers. Returns the column clicked, if sorting is enabled.
fn header(ui: &mut Ui, cols: &[(Col, f32, f32)], p: &Pal, sorted: Option<(Col, bool)>, sortable: bool) -> Option<Col> {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), HEAD_H), Sense::hover());
    let mut clicked = None;
    for &(c, x0, x1) in cols {
        let cell = Rect::from_x_y_ranges(x0..=x1, r.y_range());
        let mut color = p.weak;
        if sortable && c != Col::Folder {
            let resp = ui.interact(cell, ui.id().with(("col", c.title())), Sense::click());
            if resp.hovered() {
                color = p.text;
            }
            if resp.clicked() {
                clicked = Some(c);
            }
        }
        let active = sorted.is_some_and(|s| s.0 == c);
        if active {
            color = p.text;
        }
        let painter = ui.painter();
        let font = theme::font(12.0);
        let cy = r.center().y;
        let w = painter.layout_no_wrap(c.title().to_owned(), font.clone(), color).size().x;
        let tx = if c.right() { x1 - PAD - w } else { x0 + PAD };
        painter.text(pos2(tx, cy), Align2::LEFT_CENTER, c.title(), font, color);
        if let Some((_, desc)) = sorted.filter(|_| active) {
            let ax = if c.right() { tx - 9.0 } else { tx + w + 8.0 };
            let s = 3.0;
            let pts = if desc {
                vec![pos2(ax - s, cy - 1.5), pos2(ax + s, cy - 1.5), pos2(ax, cy + 2.0)]
            } else {
                vec![pos2(ax - s, cy + 1.5), pos2(ax + s, cy + 1.5), pos2(ax, cy - 2.0)]
            };
            painter.add(egui::Shape::convex_polygon(pts, p.weak, Stroke::NONE));
        }
    }
    ui.painter().hline(r.x_range(), r.max.y - 0.5, Stroke::new(1.0_f32, p.divider));
    clicked
}

fn row_bg(painter: &Painter, rect: Rect, selected: bool, hovered: bool, p: &Pal) {
    if selected {
        painter.rect_filled(rect.shrink2(vec2(4.0, 1.0)), 4.0, p.sel);
        let pill = Rect::from_center_size(pos2(rect.min.x + 5.5, rect.center().y), vec2(3.0, 14.0));
        painter.rect_filled(pill, 1.5, p.accent);
    } else if hovered {
        painter.rect_filled(rect.shrink2(vec2(4.0, 1.0)), 4.0, p.hover);
    }
}

fn right_text(painter: &Painter, x1: f32, cy: f32, text: String, color: Color32) {
    painter.text(pos2(x1 - PAD, cy), Align2::RIGHT_CENTER, text, theme::font(13.0), color);
}

fn left_text(painter: &Painter, x0: f32, x1: f32, cy: f32, text: &str, color: Color32) {
    theme::text_at(painter, x0 + PAD, cy, text, theme::font(12.5), color, x1 - x0 - PAD * 2.0);
}

fn share_cell(painter: &Painter, x0: f32, x1: f32, cy: f32, part: u64, whole: u64, p: &Pal) {
    let frac = if whole == 0 { 0.0 } else { part as f32 / whole as f32 };
    let bar = Rect::from_min_size(pos2(x0 + PAD, cy - 3.0), vec2(56.0, 6.0));
    theme::bar(painter, bar, frac, p.track, p.bar);
    painter.text(pos2(x1 - PAD, cy), Align2::RIGHT_CENTER, fmt::percent(part, whole), theme::font(12.0), p.weak);
}

// -------------------------------------------------------------------- widgets

fn tab_button(ui: &mut Ui, label: &str, extra: Option<&str>, active: bool, p: &Pal) -> egui::Response {
    let g = ui.painter().layout_no_wrap(label.to_owned(), theme::font(13.5), Color32::PLACEHOLDER);
    let eg = extra.map(|e| ui.painter().layout_no_wrap(e.to_owned(), theme::font(12.0), p.faint));
    let text_w = g.size().x + eg.as_ref().map_or(0.0, |e| e.size().x + 7.0);
    let (r, resp) = ui.allocate_exact_size(vec2(text_w + 28.0, 40.0), Sense::click());
    let painter = ui.painter();
    let color = if active || resp.hovered() { p.text } else { p.weak };
    let x = r.min.x + 14.0;
    let gy = r.center().y - g.size().y * 0.5;
    let gw = g.size().x;
    painter.galley(pos2(x, gy), g, color);
    if let Some(e) = eg {
        let ey = r.center().y - e.size().y * 0.5 + 0.5;
        painter.galley(pos2(x + gw + 7.0, ey), e, p.faint);
    }
    if active {
        let u = Rect::from_min_size(pos2(x, r.max.y - 3.0), vec2(text_w, 3.0));
        painter.rect_filled(u, 1.5, p.accent);
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

fn drive_row(ui: &mut Ui, d: &win::Drive, w: f32, p: &Pal) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(w, 68.0), Sense::click());
    let painter = ui.painter();
    painter.rect_filled(r, 6.0, if resp.hovered() { p.raised_hover } else { p.raised });
    painter.rect_stroke(r, 6.0, Stroke::new(1.0_f32, p.border), egui::StrokeKind::Inside);
    let x = r.min.x + 18.0;
    let top = r.min.y + 24.0;
    let lw = painter.layout_no_wrap(d.letter().to_owned(), theme::semibold(16.0), p.text).size().x;
    painter.text(pos2(x, top), Align2::LEFT_CENTER, d.letter(), theme::semibold(16.0), p.text);
    theme::text_at(painter, x + lw + 10.0, top, d.name(), theme::font(13.0), p.text, 150.0);
    painter.text(
        pos2(x, r.min.y + 46.0),
        Align2::LEFT_CENTER,
        format!("{} · {}", d.fs, fmt::size(d.total)),
        theme::font(12.0),
        p.faint,
    );
    let (bx0, bx1) = (r.min.x + 230.0, r.max.x - 96.0);
    if bx1 - bx0 > 80.0 {
        let used = d.total.saturating_sub(d.free);
        let frac = used as f32 / d.total.max(1) as f32;
        painter.text(pos2(bx0, top), Align2::LEFT_CENTER, format!("{} used", fmt::size(used)), theme::font(12.5), p.weak);
        painter.text(pos2(bx1, top), Align2::RIGHT_CENTER, format!("{} free", fmt::size(d.free)), theme::font(12.5), p.weak);
        let bar = Rect::from_min_size(pos2(bx0, r.min.y + 40.0), vec2(bx1 - bx0, 6.0));
        theme::bar(painter, bar, frac, p.track, if frac > 0.9 { p.warn } else { p.bar });
    }
    painter.text(pos2(r.max.x - 22.0, r.center().y), Align2::RIGHT_CENTER, "Scan", theme::semibold(13.0), p.accent);
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

fn node_menu(ui: &mut Ui, tree: &Tree, n: u32, it: &mut Intents, show_in_tree: bool) {
    let nd = tree.node(n);
    if nd.kind == DIR && ui.button("Open folder").clicked() {
        it.open = Some(tree.path(n));
        ui.close_menu();
    }
    if ui.button("Show in Explorer").clicked() {
        it.reveal = Some(tree.path(n));
        ui.close_menu();
    }
    if ui.button("Copy path").clicked() {
        it.copy = Some(tree.path(n));
        ui.close_menu();
    }
    if nd.kind == DIR && ui.button("Search in this folder").clicked() {
        it.search_in = Some(tree.path(n));
        ui.close_menu();
    }
    if show_in_tree && ui.button("Show in Folders").clicked() {
        it.show_in_tree = Some(n);
        ui.close_menu();
    }
    if nd.kind == DIR && tree.has_kids(n) && ui.button("Zoom treemap here").clicked() {
        it.show_in_tree = Some(n);
        it.zoom = Some(n);
        ui.close_menu();
    }
    if n != tree.root {
        ui.separator();
        if ui.button("Move to Recycle Bin…").clicked() {
            it.recycle = Some(n);
            ui.close_menu();
        }
    }
}

fn node_tooltip(ui: &mut Ui, tree: &Tree, n: u32, p: &Pal) {
    let nd = tree.node(n);
    ui.label(RichText::new(&*nd.name).font(theme::semibold(13.0)).color(p.text));
    ui.label(RichText::new(tree.path(n)).size(12.0).color(p.weak));
    ui.add_space(4.0);
    let line = if nd.kind == DIR {
        format!("{} on disk · {} files", fmt::size(nd.alloc), fmt::count(nd.files as u64))
    } else {
        format!("{} on disk · {} · modified {}", fmt::size(nd.alloc), cat_of(&nd.name).name(), fmt::date(nd.mtime))
    };
    ui.label(RichText::new(line).size(12.0).color(p.text));
}

// -------------------------------------------------------------------- folder tree

fn tree_table(ui: &mut Ui, tree: &Tree, ex: &mut Explorer, p: &Pal, it: &mut Intents) {
    let full = ui.available_rect_before_wrap();
    let mut spec = vec![(Col::Name, W::Flex(1.0)), (Col::Share, W::Fixed(124.0)), (Col::Alloc, W::Fixed(84.0))];
    if full.width() > 700.0 {
        spec.push((Col::Size, W::Fixed(84.0)));
    }
    spec.push((Col::Files, W::Fixed(80.0)));
    if full.width() > 820.0 {
        spec.push((Col::Modified, W::Fixed(96.0)));
    }
    let cols = place(&spec, full.left(), full.right());
    header(ui, &cols, p, None, false);

    let mut sa = ScrollArea::vertical().id_salt("tree_rows").auto_shrink([false; 2]);
    if let Some(i) = ex.scroll_to.take() {
        if i <= ex.visible.0 || i + 2 > ex.visible.1 {
            let view = ui.available_height();
            sa = sa.vertical_scroll_offset((i as f32 * ROW_H - view * 0.4).max(0.0));
        }
    }
    let n = ex.rows.len();
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        sa.show_rows(ui, ROW_H, n, |ui, range| {
            ex.visible = (range.start, range.end);
            for ri in range {
                let (node, depth) = ex.rows[ri];
                tree_row(ui, tree, ex, node, depth, &cols, p, it);
            }
        });
    });
}

#[allow(clippy::too_many_arguments)]
fn tree_row(
    ui: &mut Ui,
    tree: &Tree,
    ex: &mut Explorer,
    node: u32,
    depth: u16,
    cols: &[(Col, f32, f32)],
    p: &Pal,
    it: &mut Intents,
) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    let nd = tree.node(node);
    if nd.kind == NONE {
        return;
    }
    let painter = ui.painter();
    row_bg(painter, rect, ex.selected == Some(node), resp.hovered(), p);
    let cy = rect.center().y;
    let is_dir = nd.kind == DIR;
    let expandable = is_dir && tree.has_kids(node);
    let open = ex.expanded.contains(&node);
    let parent_alloc = if node == tree.root { nd.alloc } else { tree.node(nd.parent).alloc };
    let mut chevron_hit = Rect::NOTHING;
    for &(c, x0, x1) in cols {
        match c {
            Col::Name => {
                let x = x0 + PAD + depth as f32 * 16.0;
                if expandable {
                    let cc = pos2(x + 5.0, cy);
                    theme::chevron(painter, cc, open, p.weak);
                    chevron_hit = Rect::from_center_size(cc, vec2(20.0, ROW_H));
                }
                let icon = pos2(x + 22.0, cy);
                if is_dir {
                    theme::folder_icon(painter, icon, p.folder);
                } else {
                    theme::file_icon(painter, icon, theme::cat_color(cat_of(&nd.name), p.dark));
                }
                let tx = x + 36.0;
                theme::text_at(painter, tx, cy, &nd.name, theme::font(13.0), p.text, x1 - tx - 6.0);
            }
            Col::Share => share_cell(painter, x0, x1, cy, nd.alloc, parent_alloc, p),
            Col::Alloc => right_text(painter, x1, cy, fmt::size(nd.alloc), p.text),
            Col::Size => right_text(painter, x1, cy, fmt::size(nd.size), p.weak),
            Col::Files if is_dir => right_text(painter, x1, cy, fmt::count(nd.files as u64), p.weak),
            Col::Modified => left_text(painter, x0, x1, cy, &fmt::date(nd.mtime), p.weak),
            _ => {}
        }
    }
    let on_chevron = resp.interact_pointer_pos().is_some_and(|pt| chevron_hit.contains(pt));
    if resp.clicked() {
        if on_chevron {
            toggle(ex, node);
        } else {
            it.select = Some((node, false));
        }
    }
    if resp.double_clicked() && !on_chevron {
        if expandable {
            toggle(ex, node);
        } else if !is_dir {
            it.reveal = Some(tree.path(node));
        }
    }
    if resp.secondary_clicked() {
        it.select = Some((node, false));
    }
    resp.context_menu(|ui| node_menu(ui, tree, node, it, false));
}

fn toggle(ex: &mut Explorer, n: u32) {
    if !ex.expanded.remove(&n) {
        ex.expanded.insert(n);
    }
    ex.dirty = true;
}

// -------------------------------------------------------------------- treemap

/// The child of `root` on the way down to `n`, if it's a folder.
fn step_toward(tree: &Tree, root: u32, n: u32) -> Option<u32> {
    let chain = tree.ancestors(n);
    let pos = chain.iter().position(|&a| a == root)?;
    chain.get(pos + 1).copied().filter(|&c| tree.node(c).kind == DIR)
}

fn treemap_panel(ui: &mut Ui, tree: &Tree, ex: &mut Explorer, p: &Pal, it: &mut Intents) {
    let root = ex.tm_root;
    ui.horizontal(|ui| {
        ui.set_min_height(HEAD_H - 6.0);
        ui.spacing_mut().item_spacing.x = 2.0;
        let chain = tree.ancestors(root);
        let shown: Vec<Option<u32>> = if chain.len() > 6 {
            let mut v = vec![Some(chain[0]), None];
            v.extend(chain[chain.len() - 4..].iter().map(|&n| Some(n)));
            v
        } else {
            chain.iter().map(|&n| Some(n)).collect()
        };
        for (k, e) in shown.iter().enumerate() {
            if k > 0 {
                ui.label(RichText::new(" › ").color(p.faint));
            }
            let Some(n) = *e else {
                ui.label(RichText::new("…").color(p.faint));
                continue;
            };
            let name = short_name(tree, n);
            if n == root {
                ui.add(egui::Label::new(RichText::new(name).font(theme::semibold(13.0)).color(p.text)).truncate());
            } else {
                let r = ui
                    .add(egui::Label::new(RichText::new(name).color(p.weak)).sense(Sense::click()).selectable(false))
                    .on_hover_cursor(CursorIcon::PointingHand);
                if r.clicked() {
                    it.zoom = Some(n);
                }
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let nd = tree.node(root);
            ui.label(
                RichText::new(format!("{} · {} files", fmt::size(nd.alloc), fmt::count(nd.files as u64)))
                    .size(12.5)
                    .color(p.weak),
            );
        });
    });
    ui.add_space(4.0);

    let legend_h = 30.0;
    let avail = ui.available_size();
    let (rect, resp) = ui.allocate_exact_size(vec2(avail.x, (avail.y - legend_h).max(80.0)), Sense::click());
    let key = (rect.width().round() as i32, rect.height().round() as i32);
    let stale = ex.tm.as_ref().is_none_or(|c| c.root != root || c.gen != tree.generation || c.size != key);
    if stale {
        let area = Rect::from_min_size(Pos2::ZERO, rect.size()).shrink(1.0);
        ex.tm = Some(TmCache { root, gen: tree.generation, size: key, cells: treemap::build(tree, root, area) });
    }
    let cells = &ex.tm.as_ref().unwrap().cells;
    let off = rect.min.to_vec2();
    let hover = resp.hover_pos().and_then(|pos| treemap::hit(cells, pos - off));
    let hovered_node = hover.map(|h| cells[h].node).filter(|&n| n != REST);
    treemap::paint(&ui.painter_at(rect), off, cells, tree, p, ex.selected, hover);
    if cells.is_empty() {
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, "Empty", theme::font(13.0), p.faint);
    }

    if resp.clicked() {
        if let Some(n) = hovered_node {
            it.select = Some((n, true));
        }
    }
    if resp.double_clicked() {
        if let Some(next) = hovered_node.and_then(|n| step_toward(tree, root, n)) {
            it.zoom = Some(next);
        }
    }
    if resp.secondary_clicked() {
        ex.menu_node = hovered_node;
        if let Some(n) = hovered_node {
            it.select = Some((n, true));
        }
    }
    if resp.hovered() && root != tree.root && ui.input(|i| i.pointer.button_clicked(PointerButton::Extra1)) {
        it.zoom = Some(tree.node(root).parent);
    }
    let menu_node = ex.menu_node;
    resp.context_menu(|ui| {
        if let Some(n) = menu_node {
            node_menu(ui, tree, n, it, false);
        }
    });
    let menu_open = ui.ctx().memory(|m| m.any_popup_open());
    if let (Some(n), false) = (hovered_node, menu_open) {
        resp.on_hover_ui_at_pointer(|ui| node_tooltip(ui, tree, n, p));
    }

    if ex.legend.as_ref().is_none_or(|l| l.0 != root || l.1 != tree.generation) {
        ex.legend = Some((root, tree.generation, treemap::legend(tree, root)));
    }
    ui.add_space(8.0);
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::hover());
    let painter = ui.painter();
    let mut x = row.min.x;
    for &(cat, bytes) in &ex.legend.as_ref().unwrap().2 {
        let name = painter.layout_no_wrap(cat.name().to_owned(), theme::font(12.0), p.text);
        let size = painter.layout_no_wrap(fmt::size(bytes), theme::font(12.0), p.faint);
        let w = 16.0 + name.size().x + 6.0 + size.size().x;
        if x + w > row.max.x {
            break; // only show what fits; the legend is sorted largest first
        }
        let cy = row.center().y;
        painter.rect_filled(Rect::from_center_size(pos2(x + 5.0, cy), vec2(10.0, 10.0)), 2.0, theme::cat_color(cat, p.dark));
        let nw = name.size().x;
        painter.galley(pos2(x + 16.0, cy - name.size().y * 0.5), name, p.text);
        painter.galley(pos2(x + 22.0 + nw, cy - size.size().y * 0.5), size, p.faint);
        x += w + 18.0;
    }
}

// -------------------------------------------------------------------- cleanup

fn safety_tag(s: Safety, p: &Pal) -> (&'static str, Color32, Color32) {
    let tint = |c: Color32| Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), if p.dark { 34 } else { 24 });
    match s {
        Safety::Safe => ("Safe to delete", p.good, tint(p.good)),
        Safety::Review => ("Review first", p.warn, tint(p.warn)),
        Safety::Info => ("For information", p.weak, tint(p.weak)),
    }
}

#[allow(clippy::too_many_arguments)]
fn group_box(
    ui: &mut Ui,
    gi: usize,
    g: &mut Group,
    tree: &Tree,
    p: &Pal,
    it: &mut Intents,
    busy: bool,
    show_all: &mut HashSet<usize>,
) {
    let deletable = g.deletable();
    let visible: Vec<usize> = (0..g.items.len())
        .filter(|&i| g.items[i].status != Status::Pending || tree.node(g.items[i].node).kind != NONE)
        .collect();
    let pending: Vec<usize> = visible.iter().copied().filter(|&i| g.items[i].status == Status::Pending).collect();
    let n_sel = pending.iter().filter(|&&i| g.items[i].selected).count();
    let state = if n_sel == 0 { 0 } else if n_sel == pending.len() { 1 } else { 2 };
    let total = g.total(tree);

    Frame::new().fill(p.raised).stroke(Stroke::new(1.0_f32, p.border)).corner_radius(6).show(ui, |ui| {
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        let w = ui.available_width();
        let (r, resp) = ui.allocate_exact_size(vec2(w, 46.0), Sense::click());
        let painter = ui.painter();
        if resp.hovered() {
            painter.rect_filled(r, 6.0, p.hover);
        }
        let cy = r.center().y;
        theme::chevron(painter, pos2(r.min.x + 18.0, cy), g.open, p.weak);
        let check = pos2(r.min.x + 42.0, cy);
        let checkable = deletable && !pending.is_empty();
        if checkable {
            theme::checkbox(painter, check, state, p);
        }
        let x = r.min.x + 62.0;
        let tw = theme::text_at(painter, x, cy, g.title, theme::semibold(14.0), p.text, w * 0.45);
        let (tag, fg, bg) = safety_tag(g.safety, p);
        theme::pill(painter, pos2(x + tw + 10.0, cy), tag, fg, bg);
        painter.text(pos2(r.max.x - 18.0, cy), Align2::RIGHT_CENTER, fmt::size(total), theme::semibold(14.0), p.text);
        let count = if visible.len() == 1 { "1 item".to_string() } else { format!("{} items", visible.len()) };
        painter.text(pos2(r.max.x - 110.0, cy), Align2::RIGHT_CENTER, count, theme::font(12.5), p.faint);
        if resp.clicked() {
            let on_check = checkable && resp.interact_pointer_pos().is_some_and(|pt| (pt.x - check.x).abs() < 13.0);
            if on_check {
                if !busy {
                    let v = state != 1;
                    for &i in &pending {
                        g.items[i].selected = v;
                    }
                }
            } else {
                g.open = !g.open;
            }
        }
        let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
        drop(resp);

        if g.open {
            Frame::new().inner_margin(Margin { left: 62, right: 18, top: 0, bottom: 10 }).show(ui, |ui| {
                ui.label(RichText::new(g.blurb).size(12.5).color(p.weak));
            });
            let limit = if show_all.contains(&gi) { usize::MAX } else { 40 };
            for &i in visible.iter().take(limit) {
                item_row(ui, g, i, deletable, p, it, busy);
            }
            if visible.len() > limit {
                let (r, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
                ui.painter().text(
                    pos2(r.min.x + 62.0, r.center().y),
                    Align2::LEFT_CENTER,
                    format!("Show all {} items", visible.len()),
                    theme::font(12.5),
                    p.accent,
                );
                if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    show_all.insert(gi);
                }
            }
            ui.add_space(6.0);
        }
    });
}

fn item_row(ui: &mut Ui, g: &mut Group, i: usize, deletable: bool, p: &Pal, it: &mut Intents, busy: bool) {
    let w = ui.available_width();
    let (r, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
    let painter = ui.painter();
    let item = &mut g.items[i];
    if resp.hovered() {
        painter.rect_filled(r.shrink2(vec2(6.0, 0.0)), 4.0, p.hover);
    }
    let cy = r.center().y;
    let pending = item.status == Status::Pending;
    if deletable && pending {
        theme::checkbox(painter, pos2(r.min.x + 42.0, cy), item.selected as u8, p);
    }
    let x = r.min.x + 62.0;
    let size_x = r.max.x - 18.0;
    let label_w = (w * 0.3).clamp(160.0, 320.0);
    theme::text_at(painter, x, cy, &item.label, theme::font(13.0), if pending { p.text } else { p.weak }, label_w - 12.0);

    let mut detail_end = size_x - 92.0;
    let status = match &item.status {
        Status::Pending => None,
        Status::Working => Some(("Working…".to_string(), p.weak)),
        Status::Done(s) => Some((s.clone(), p.good)),
        Status::Failed(s) => Some((s.clone(), p.danger)),
    };
    if let Some((s, c)) = status {
        let g = theme::galley(painter, &s, theme::font(12.5), c, 300.0);
        let sw = g.size().x;
        painter.galley(pos2(detail_end - sw, cy - g.size().y * 0.5), g, c);
        detail_end -= sw + 14.0;
    }
    let detail: &str = if item.note.is_empty() { &item.path } else { item.note };
    theme::text_at(painter, x + label_w, cy, detail, theme::font(12.5), p.faint, detail_end - (x + label_w));
    painter.text(
        pos2(size_x, cy),
        Align2::RIGHT_CENTER,
        fmt::size(item.alloc),
        theme::font(13.0),
        if pending { p.text } else { p.faint },
    );

    if resp.clicked() && deletable && pending && !busy {
        item.selected = !item.selected;
    }
    let (path, node) = (item.path.clone(), item.node);
    resp.context_menu(|ui| {
        if ui.button("Show in Explorer").clicked() {
            it.reveal = Some(path.clone());
            ui.close_menu();
        }
        if ui.button("Copy path").clicked() {
            it.copy = Some(path.clone());
            ui.close_menu();
        }
        if pending && ui.button("Show in Folders").clicked() {
            it.show_in_tree = Some(node);
            ui.close_menu();
        }
    });
}
