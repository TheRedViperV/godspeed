//! Fallback indexer: lists folders in parallel through the regular file APIs.
//! Works on any file system and without admin rights, just slower than reading the MFT.

use crate::index::{filetime_to_unix, unix_time, Job, Raw, DIR, FILE};
use crate::ntfs::{ATTR_HIDDEN, ATTR_SYSTEM};
use rayon::prelude::*;
use std::fs;
use std::os::windows::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;

struct WDir {
    files: Vec<Raw>,
    dirs: Vec<(Raw, WDir)>,
}

/// Walks run on threads of their own. Listing folders is mostly waiting on the
/// disk, and on the shared pool that waiting would hold up searches and stats
/// for the whole walk (and, through the index lock, the window with them).
fn pool(threads: usize) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .start_handler(|_| crate::win::low_priority_thread())
        .build()
        .expect("couldn't start the folder walk threads")
}

fn local_pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| pool(8))
}

/// Network listings mostly wait on round-trips, so many run at once.
fn network_pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| pool(32))
}

/// Lists a whole local drive. Entry 0 is the root.
pub fn scan(letter: char, job: &Job) -> Result<Vec<Raw>, String> {
    local_pool().install(|| scan_on(letter, job))
}

/// Lists a whole network drive, many folders at a time.
pub fn scan_network(letter: char, job: &Job) -> Result<Vec<Raw>, String> {
    network_pool().install(|| scan_on(letter, job))
}

fn scan_on(letter: char, job: &Job) -> Result<Vec<Raw>, String> {
    let root = format!("{letter}:\\");
    let meta = fs::metadata(&root).map_err(|e| format!("can't open {root} ({e})"))?;
    let top = read_dir(Path::new(&root), job);
    let mut out =
        vec![Raw { name: format!("{letter}:").into(), mtime: unix_time(meta.modified()), kind: DIR, ..Default::default() }];
    let mut stack = vec![(top, 0u32)];
    while let Some((dir, parent)) = stack.pop() {
        for mut f in dir.files {
            f.parent = parent;
            out.push(f);
        }
        for (mut d, sub) in dir.dirs {
            d.parent = parent;
            stack.push((sub, out.len() as u32));
            out.push(d);
        }
    }
    Ok(out)
}

/// Everything inside one folder, for a folder that just appeared. Each entry's
/// `parent` is the position of its folder in the list, or `u32::MAX` for the
/// folder itself; folders always come before their contents.
pub fn subtree(path: &Path) -> Vec<Raw> {
    let job = Job::new('?', false);
    let mut out = Vec::new();
    let top = local_pool().install(|| read_dir(path, &job));
    let mut stack = vec![(top, u32::MAX)];
    while let Some((dir, parent)) = stack.pop() {
        for mut f in dir.files {
            f.parent = parent;
            out.push(f);
        }
        for (mut d, sub) in dir.dirs {
            d.parent = parent;
            stack.push((sub, out.len() as u32));
            out.push(d);
        }
    }
    out
}

fn read_dir(path: &Path, job: &Job) -> WDir {
    let mut files = Vec::new();
    let mut subs: Vec<(Raw, PathBuf)> = Vec::new();
    if job.cancel.load(Relaxed) {
        return WDir { files, dirs: Vec::new() };
    }
    if let Ok(rd) = fs::read_dir(path) {
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            let ft = md.file_type();
            let raw = Raw {
                name: e.file_name().to_string_lossy().into(),
                size: if ft.is_file() { md.len() } else { 0 },
                mtime: filetime_to_unix(md.last_write_time()),
                kind: if md.is_dir() || ft.is_symlink_dir() { DIR } else { FILE },
                hidden: md.file_attributes() & (ATTR_HIDDEN | ATTR_SYSTEM) != 0,
                ..Default::default()
            };
            // Links to folders are listed but not followed, so nothing is counted twice.
            if md.is_dir() {
                subs.push((raw, e.path()));
            } else {
                files.push(raw);
            }
        }
    }
    job.items.fetch_add((files.len() + subs.len()) as u64, Relaxed);
    let dirs = subs
        .into_par_iter()
        .map(|(raw, p)| {
            let sub = read_dir(&p, job);
            (raw, sub)
        })
        .collect();
    WDir { files, dirs }
}
