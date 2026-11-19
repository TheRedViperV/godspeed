//! Live updates without admin rights: Windows' folder-change notifications (the
//! ones Explorer uses), for drives indexed by walking their folders.
//!
//! A walked drive opens from its saved index right away, gets re-walked quietly in
//! the background, and from then on follows changes as they happen. Changes made
//! during a re-walk are replayed onto the fresh index, so nothing is lost.

use crate::index::{unix_time, Job, Method, Shared, Volume, DIR, FILE};
use crate::ntfs::{ATTR_HIDDEN, ATTR_SYSTEM};
use crate::{walk, win};
use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::fs::Metadata;
use std::os::windows::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often changes are applied while the window is in front, and while it isn't.
const POLL: Duration = Duration::from_millis(250);
const POLL_AWAY: Duration = Duration::from_secs(3);
/// If Windows drops notifications (too much changed at once), re-walk, but not
/// more often than this.
const RESCAN_GAP: Duration = Duration::from_secs(600);
/// Network drives are re-walked this often, whatever the notifications say.
const NETWORK_RESCAN: Duration = Duration::from_secs(45 * 60);
/// Save the index now and then, not just at exit, in case the app is killed.
const SAVE_EVERY: Duration = Duration::from_secs(900);

struct Watch {
    _watcher: RecommendedWatcher,
    rx: Receiver<notify::Result<notify::Event>>,
}

fn start(letter: char) -> Option<Watch> {
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .ok()?;
    watcher.watch(Path::new(&format!("{letter}:\\")), RecursiveMode::Recursive).ok()?;
    Some(Watch { _watcher: watcher, rx })
}

#[derive(Clone)]
enum Op {
    Changed(PathBuf),
    Removed(PathBuf),
    Moved(PathBuf, PathBuf),
}

/// Indexes a drive by walking it, starting from the saved index when there is
/// one, then follows its changes for as long as the drive is there.
pub fn index_by_walk(shared: Arc<Shared>, letter: char, job: Arc<Job>) {
    // Watch first, so nothing that changes from here on is missed.
    let watch = start(letter);
    crate::trace::log(format!("{letter}: watching, loading saved index"));
    if let Some(mut v) = Volume::load_cache(letter) {
        crate::trace::log(format!("{letter}: saved index loaded ({} items)", v.items));
        v.live = watch.is_some();
        shared.write().install(v);
        crate::trace::log(format!("{letter}: saved index in use"));
        shared.notify();
    }
    let refreshing = Arc::new(AtomicBool::new(true));
    rewalk(&shared, letter, job, &refreshing, watch.is_some());
    if let Some(w) = watch {
        follow(shared, letter, w, refreshing);
    }
}

/// Follows a drive that has just been walked some other way.
pub fn follow_only(shared: Arc<Shared>, letter: char) {
    if let Some(w) = start(letter) {
        if let Some(v) = shared.write().vol_mut(letter) {
            v.live = true;
        }
        follow(shared, letter, w, Arc::new(AtomicBool::new(false)));
    }
}

/// Walks the drive again in the background and swaps the result in.
fn rewalk(shared: &Arc<Shared>, letter: char, job: Arc<Job>, refreshing: &Arc<AtomicBool>, live: bool) {
    refreshing.store(true, Relaxed);
    let (shared, refreshing) = (shared.clone(), refreshing.clone());
    std::thread::spawn(move || {
        win::background_thread();
        let t0 = Instant::now();
        let walked = if shared.is_network(letter) { walk::scan_network(letter, &job) } else { walk::scan(letter, &job) };
        match walked {
            // Switched off while walking: drop it.
            Ok(_) if job.cancel.load(Relaxed) || !shared.wanted(letter) => {}
            Ok(raws) => {
                crate::trace::log(format!("{letter}: walked {} items", raws.len()));
                let mut v = Volume::build(letter, Method::Walk, raws, 0);
                v.elapsed = t0.elapsed();
                v.live = live;
                let _ = v.save_cache();
                shared.write().install(v);
                crate::trace::log(format!("{letter}: fresh index in use"));
            }
            Err(e) => {
                if shared.read().vol(letter).is_none() {
                    shared.note(letter, format!("{letter}: couldn't be indexed ({e})"));
                }
            }
        }
        refreshing.store(false, Relaxed);
        shared.end_job(&job);
    });
}

fn follow(shared: Arc<Shared>, letter: char, w: Watch, refreshing: Arc<AtomicBool>) {
    win::low_priority_thread();
    // A reconnected network drive starts a new follower; this one then bows out.
    let token = shared.claim_follow(letter);
    let network = shared.is_network(letter);
    let mut replay: Vec<Op> = Vec::new();
    let mut seen_epoch = shared.read().vol(letter).map(|v| v.epoch());
    let mut last_rescan = Instant::now();
    let mut last_save = Instant::now();
    loop {
        std::thread::sleep(if shared.foreground() { POLL } else { POLL_AWAY });
        if !shared.holds_follow(letter, token) {
            return;
        }
        let mut overflow = false;
        let ops = drain(&w.rx, &mut overflow);
        let epoch = shared.read().vol(letter).map(|v| v.epoch());
        let walking = refreshing.load(Relaxed);
        if epoch.is_none() && !walking {
            return; // the drive went away
        }
        if walking {
            replay.extend(ops.iter().cloned());
        }
        if let Some(e) = epoch {
            if seen_epoch != Some(e) {
                // A freshly walked index: catch it up on what changed during the walk.
                seen_epoch = Some(e);
                let caught_up = std::mem::take(&mut replay);
                apply(&shared, letter, e, &caught_up);
            }
            if !ops.is_empty() {
                apply(&shared, letter, e, &ops);
            }
        }
        // Many file servers and NAS boxes report changes unreliably, so network
        // drives also get a quiet re-walk now and then.
        let net_due = network && last_rescan.elapsed() > NETWORK_RESCAN;
        let offline = shared.read().vol(letter).is_some_and(|v| v.offline);
        if (overflow || net_due) && !walking && !offline && last_rescan.elapsed() > RESCAN_GAP {
            last_rescan = Instant::now();
            let job = Arc::new(Job::new(letter, false));
            shared.add_job(&job);
            rewalk(&shared, letter, job, &refreshing, true);
        }
        if last_save.elapsed() > SAVE_EVERY {
            last_save = Instant::now();
            shared.save_walked();
        }
    }
}

/// Turns raw notifications into changes, pairing a rename's old and new names.
fn drain(rx: &Receiver<notify::Result<notify::Event>>, overflow: &mut bool) -> Vec<Op> {
    let mut ops = Vec::new();
    let mut from: Option<PathBuf> = None;
    while let Ok(res) = rx.try_recv() {
        let Ok(ev) = res else {
            *overflow = true;
            continue;
        };
        if ev.need_rescan() {
            *overflow = true;
            continue;
        }
        let mut paths = ev.paths.into_iter();
        match ev.kind {
            EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                if let Some(old) = std::mem::replace(&mut from, paths.next()) {
                    ops.push(Op::Removed(old));
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                let Some(to) = paths.next() else { continue };
                ops.push(match from.take() {
                    Some(old) => Op::Moved(old, to),
                    None => Op::Changed(to),
                });
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
                if let (Some(a), Some(b)) = (paths.next(), paths.next()) {
                    ops.push(Op::Moved(a, b));
                }
            }
            EventKind::Remove(_) => ops.extend(paths.map(Op::Removed)),
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any => ops.extend(paths.map(Op::Changed)),
            _ => {}
        }
    }
    if let Some(old) = from {
        ops.push(Op::Removed(old));
    }
    ops
}

/// `C:\a\b.txt` → (`C:\a`, `b.txt`).
fn split(path: &str) -> Option<(&str, &str)> {
    path.rsplit_once('\\').filter(|(_, name)| !name.is_empty())
}

/// Adds or updates what's at `path`; a folder that's new gets its contents listed later.
fn add(v: &mut Volume, path: &Path, md: &Metadata, new_dirs: &mut Vec<(PathBuf, u32)>) {
    let text = path.to_string_lossy();
    let Some((dir, name)) = split(&text) else { return };
    let Some(parent) = v.ensure_dir(dir) else { return };
    let ft = md.file_type();
    let is_dir = md.is_dir() || ft.is_symlink_dir();
    let size = if ft.is_file() { md.len() } else { 0 };
    let hidden = md.file_attributes() & (ATTR_HIDDEN | ATTR_SYSTEM) != 0;
    let (i, fresh) = v.upsert(parent, name, if is_dir { DIR } else { FILE }, size, unix_time(md.modified()), hidden);
    // Links to folders are listed but not followed, as in the first walk.
    if fresh && md.is_dir() {
        new_dirs.push((path.to_path_buf(), i));
    }
}

fn apply(shared: &Shared, letter: char, epoch: u64, ops: &[Op]) {
    if ops.is_empty() {
        return;
    }
    // Look at what's on disk now, outside the lock.
    let mut disk: HashMap<&Path, Option<Metadata>> = HashMap::new();
    for op in ops {
        if let Op::Changed(p) | Op::Moved(_, p) = op {
            disk.entry(p.as_path()).or_insert_with(|| std::fs::symlink_metadata(p).ok());
        }
    }
    let mut new_dirs = Vec::new();
    {
        let mut ix = shared.write();
        let Some(v) = ix.vol_mut(letter).filter(|v| v.epoch() == epoch) else { return };
        let mut relink = false;
        for op in ops {
            match op {
                Op::Removed(p) => {
                    if let Some(i) = v.find_path(&p.to_string_lossy()) {
                        relink |= v.remove_entry(i);
                    }
                }
                Op::Moved(from, to) => {
                    let now = disk.get(to.as_path()).and_then(|m| m.as_ref());
                    match (v.find_path(&from.to_string_lossy()), now) {
                        (Some(i), Some(_)) => {
                            let to_text = to.to_string_lossy();
                            let target = split(&to_text).and_then(|(dir, name)| Some((v.ensure_dir(dir)?, name)));
                            relink |= match target {
                                Some((parent, name)) => v.move_entry(i, parent, name),
                                None => v.remove_entry(i),
                            };
                        }
                        (Some(i), None) => relink |= v.remove_entry(i),
                        (None, Some(md)) => add(v, to, md, &mut new_dirs),
                        (None, None) => {}
                    }
                }
                Op::Changed(p) => match disk.get(p.as_path()).and_then(|m| m.as_ref()) {
                    Some(md) => add(v, p, md, &mut new_dirs),
                    None => {
                        if let Some(i) = v.find_path(&p.to_string_lossy()) {
                            relink |= v.remove_entry(i);
                        }
                    }
                },
            }
        }
        // Folders that moved or went away take their contents with them.
        if relink {
            v.relink();
        }
        ix.generation += 1;
    }
    // A new folder (copied in, unzipped...): list what's inside, outside the lock.
    for (path, dir) in new_dirs {
        let raws = walk::subtree(&path);
        if raws.is_empty() {
            continue;
        }
        let mut ix = shared.write();
        let Some(v) = ix.vol_mut(letter).filter(|v| v.epoch() == epoch) else { return };
        let mut placed: Vec<u32> = Vec::with_capacity(raws.len());
        for r in &raws {
            let parent = if r.parent == u32::MAX { dir } else { placed[r.parent as usize] };
            placed.push(v.upsert(parent, &r.name, r.kind, r.size, r.mtime, r.hidden).0);
        }
        ix.generation += 1;
    }
    shared.notify();
}
