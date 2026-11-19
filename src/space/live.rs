//! Keeps a scan current while the app is open: watches the scanned folder for
//! changes and turns them into small updates the UI applies to the tree.

use crate::space::tree::Node;
use crate::space::walk;
use eframe::egui;
use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::fs;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// Events are collected for this long before being applied, so a burst of
/// activity (an install, an unzip) lands as one update.
const SETTLE: Duration = Duration::from_millis(700);

pub enum Change {
    /// Nothing exists at this path any more.
    Gone(String),
    /// Something exists at this path (current metadata, no contents for folders).
    Exists { path: String, is_dir: bool, size: u64, alloc: u64, mtime: u64 },
    /// Full contents of a folder that appeared; index 0 is the folder itself.
    Subtree { path: String, nodes: Vec<Node> },
    /// Windows dropped events because too much changed at once.
    Overflow,
}

pub struct Live {
    _watcher: notify::RecommendedWatcher,
    pub rx: Receiver<Vec<Change>>,
    tx: Sender<Vec<Change>>,
    ctx: egui::Context,
}

impl Live {
    pub fn start(root: &str, ctx: &egui::Context) -> Option<Live> {
        let (ev_tx, ev_rx) = mpsc::channel::<notify::Result<notify::Event>>();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = ev_tx.send(res);
        })
        .ok()?;
        watcher.watch(Path::new(root), RecursiveMode::Recursive).ok()?;
        let (tx, rx) = mpsc::channel();
        let (tx2, ctx2) = (tx.clone(), ctx.clone());
        std::thread::Builder::new()
            .name("watch".into())
            .spawn(move || pump(ev_rx, tx2, ctx2))
            .ok()?;
        Some(Live { _watcher: watcher, rx, tx, ctx: ctx.clone() })
    }

    /// Reads a folder that just appeared, off the UI thread.
    pub fn fetch_subtree(&self, path: String) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            if let Some(nodes) = walk::subtree(Path::new(&path)) {
                let _ = tx.send(vec![Change::Subtree { path, nodes }]);
                ctx.request_repaint();
            }
        });
    }
}

fn pump(events: Receiver<notify::Result<notify::Event>>, tx: Sender<Vec<Change>>, ctx: egui::Context) {
    loop {
        let Ok(first) = events.recv() else { return }; // watcher dropped
        let mut paths: HashSet<PathBuf> = HashSet::new();
        let mut overflow = false;
        let mut take = |res: notify::Result<notify::Event>| match res {
            Ok(ev) => {
                if ev.need_rescan() {
                    overflow = true;
                }
                if !matches!(ev.kind, EventKind::Access(_)) {
                    paths.extend(ev.paths);
                }
            }
            Err(_) => overflow = true,
        };
        take(first);
        let deadline = Instant::now() + SETTLE;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match events.recv_timeout(left) {
                Ok(res) => take(res),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        let mut changes: Vec<Change> = paths.into_iter().filter_map(|p| describe(&p)).collect();
        if overflow {
            changes.push(Change::Overflow);
        }
        if changes.is_empty() {
            continue;
        }
        if tx.send(changes).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

/// What's at `p` right now. Done here rather than when the event fired, so a file
/// created and deleted within the same burst simply comes out as gone.
fn describe(p: &Path) -> Option<Change> {
    let path = p.to_string_lossy().into_owned();
    match fs::symlink_metadata(p) {
        Ok(md) if md.file_type().is_symlink() => None,
        Ok(md) => Some(Change::Exists {
            path,
            is_dir: md.is_dir(),
            size: if md.is_dir() { 0 } else { md.len() },
            alloc: if md.is_dir() { 0 } else { walk::alloc_of(&md) },
            mtime: md.last_write_time(),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(Change::Gone(path)),
        Err(_) => None, // in use or access denied: leave it as it was
    }
}
