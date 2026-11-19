//! Fallback scanner: lists folders in parallel through the regular file APIs.
//! Works on any file system and without admin rights, just slower than reading the MFT.

use crate::space::tree::*;
use rayon::prelude::*;
use std::fs;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

/// OFFLINE, RECALL_ON_OPEN, RECALL_ON_DATA_ACCESS: cloud placeholders that take no local space.
const CLOUD_ONLY: u32 = 0x1000 | 0x40000 | 0x400000;
const CLUSTER: u64 = 4096;

struct WFile {
    name: Box<str>,
    size: u64,
    alloc: u64,
    mtime: u64,
}

struct WDir {
    name: Box<str>,
    mtime: u64,
    files: Vec<WFile>,
    dirs: Vec<WDir>,
}

pub fn scan(root: &str, progress: &Progress, cancel: &AtomicBool) -> Result<Tree, String> {
    progress.phase.store(PH_WALK, Relaxed);
    let meta = fs::metadata(root).map_err(|e| format!("Can't open {root}: {e}"))?;
    if !meta.is_dir() {
        return Err(format!("{root} is not a folder"));
    }
    let skipped = AtomicU64::new(0);
    let top = read_dir(Path::new(root), root.into(), meta.last_write_time(), progress, cancel, &skipped);
    if cancel.load(Relaxed) {
        return Err("Cancelled".into());
    }
    progress.phase.store(PH_ANALYZE, Relaxed);
    let mut nodes = Vec::new();
    flatten(top, 0, &mut nodes);
    let mut tree = Tree::new(nodes, 0, Method::Walk);
    tree.skipped = skipped.into_inner();
    Ok(tree)
}

/// A folder's contents as flat nodes (index 0 is the folder, parents are local
/// indices, sizes not yet summed). Used for folders that appear while watching.
pub fn subtree(path: &Path) -> Option<Vec<Node>> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return None;
    }
    let name: Box<str> = path.file_name()?.to_string_lossy().into();
    let (progress, cancel, skipped) = (Progress::default(), AtomicBool::new(false), AtomicU64::new(0));
    let top = read_dir(path, name, meta.last_write_time(), &progress, &cancel, &skipped);
    let mut nodes = Vec::new();
    flatten(top, 0, &mut nodes);
    Some(nodes)
}

pub fn alloc_of(md: &fs::Metadata) -> u64 {
    if md.file_attributes() & CLOUD_ONLY != 0 {
        0
    } else {
        md.len().div_ceil(CLUSTER) * CLUSTER
    }
}

fn read_dir(
    path: &Path,
    name: Box<str>,
    mtime: u64,
    progress: &Progress,
    cancel: &AtomicBool,
    skipped: &AtomicU64,
) -> WDir {
    let mut d = WDir { name, mtime, files: Vec::new(), dirs: Vec::new() };
    if cancel.load(Relaxed) {
        return d;
    }
    let Ok(entries) = fs::read_dir(path) else {
        skipped.fetch_add(1, Relaxed);
        return d;
    };
    let mut subdirs: Vec<(PathBuf, Box<str>, u64)> = Vec::new();
    for e in entries.flatten() {
        // On Windows this comes from the directory listing itself; no extra call per file.
        let Ok(md) = e.metadata() else { continue };
        if md.file_type().is_symlink() {
            continue; // symlinks and junctions point at data counted elsewhere
        }
        let name: Box<str> = e.file_name().to_string_lossy().into();
        if md.is_dir() {
            subdirs.push((e.path(), name, md.last_write_time()));
        } else {
            d.files.push(WFile { name, size: md.len(), alloc: alloc_of(&md), mtime: md.last_write_time() });
        }
    }
    progress.items.fetch_add(d.files.len() as u64 + 1, Relaxed);
    d.dirs = subdirs
        .into_par_iter()
        .map(|(p, n, m)| read_dir(&p, n, m, progress, cancel, skipped))
        .collect();
    d
}

fn flatten(d: WDir, parent: u32, nodes: &mut Vec<Node>) {
    let idx = nodes.len() as u32;
    nodes.push(Node { name: d.name, mtime: d.mtime, parent, kind: DIR, rank: 1, ..Default::default() });
    for f in d.files {
        nodes.push(Node {
            name: f.name,
            size: f.size,
            alloc: f.alloc,
            mtime: f.mtime,
            parent: idx,
            kind: FILE,
            rank: 1,
            ..Default::default()
        });
    }
    for sub in d.dirs {
        flatten(sub, idx, nodes);
    }
}

/// On-disk size of a file or folder right now (used to report space freed).
pub fn measure(p: &Path) -> u64 {
    let Ok(md) = fs::symlink_metadata(p) else { return 0 };
    if md.file_type().is_symlink() {
        return 0;
    }
    if !md.is_dir() {
        return alloc_of(&md);
    }
    let mut total = 0;
    if let Ok(entries) = fs::read_dir(p) {
        for e in entries.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.file_type().is_symlink() {
                continue;
            }
            total += if m.is_dir() { measure(&e.path()) } else { alloc_of(&m) };
        }
    }
    total
}
