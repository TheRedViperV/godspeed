//! Runs a scan and derives the views built on top of the tree.

use crate::space::clutter::{self, Group};
use crate::space::tree::*;
use crate::space::{ntfs, walk, win};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Instant, SystemTime};

pub struct TypeStat {
    pub ext: String,
    pub cat: Cat,
    pub alloc: u64,
    pub count: u64,
}

pub struct Data {
    pub tree: Tree,
    pub top: Vec<u32>,
    pub types: Vec<TypeStat>,
    pub types_gen: u64,
    pub clutter: Vec<Group>,
    pub volume: Option<(u64, u64)>,
    /// Why the fast scan wasn't used, if it wasn't.
    pub note: Option<String>,
    pub scanned_at: SystemTime,
    /// Tree generation last written to the scan cache.
    pub saved_gen: u64,
}

fn drive_letter(root: &str) -> Option<char> {
    let b = root.as_bytes();
    let plain_root = (b.len() == 2 || (b.len() == 3 && b[2] == b'\\')) && b[1] == b':';
    (plain_root && b[0].is_ascii_alphabetic()).then(|| b[0].to_ascii_uppercase() as char)
}

pub fn run(root: &str, force_walk: bool, progress: &Progress, cancel: &AtomicBool) -> Result<Data, String> {
    let t0 = Instant::now();
    let mut note = None;
    let mut tree = None;
    if let (false, Some(letter)) = (force_walk, drive_letter(root)) {
        match ntfs::scan(letter, progress, cancel) {
            Ok(t) => tree = Some(t),
            Err(e) if e == "Cancelled" => return Err(e),
            Err(e) => {
                note = Some(e);
                progress.items.store(0, Relaxed);
            }
        }
    }
    let mut tree = match tree {
        Some(t) => t,
        None => walk::scan(root, progress, cancel)?,
    };
    tree.elapsed = t0.elapsed();
    progress.phase.store(PH_ANALYZE, Relaxed);
    Ok(analyze(tree, note, SystemTime::now()))
}

/// Derives everything the views need from a tree, fresh or loaded from the cache.
pub fn analyze(tree: Tree, note: Option<String>, scanned_at: SystemTime) -> Data {
    let root = tree.node(tree.root).name.to_string();
    let top = top_files(&tree, 10_000);
    let types = type_stats(&tree);
    let clutter = clutter::detect(&tree);
    let volume = drive_letter(&root).and_then(|_| win::disk_space(&root));
    let generation = tree.generation;
    Data { types_gen: generation, saved_gen: generation, tree, top, types, clutter, volume, note, scanned_at }
}

fn largest(tree: &Tree, limit: usize, mut keep: impl FnMut(&Node) -> bool) -> Vec<u32> {
    let mut heap: BinaryHeap<Reverse<(u64, u32)>> = BinaryHeap::with_capacity(limit + 1);
    for (i, nd) in tree.nodes.iter().enumerate() {
        if nd.kind != FILE || !keep(nd) {
            continue;
        }
        if heap.len() == limit {
            if let Some(Reverse((min, _))) = heap.peek() {
                if nd.alloc <= *min {
                    continue;
                }
            }
        }
        heap.push(Reverse((nd.alloc, i as u32)));
        if heap.len() > limit {
            heap.pop();
        }
    }
    heap.into_sorted_vec().into_iter().map(|Reverse((_, i))| i).collect()
}

pub fn top_files(tree: &Tree, limit: usize) -> Vec<u32> {
    largest(tree, limit, |_| true)
}

pub fn files_with_ext(tree: &Tree, ext: &str, limit: usize) -> Vec<u32> {
    let mut buf = [0u8; 16];
    largest(tree, limit, |nd| lower_ext(&nd.name, &mut buf) == ext)
}

pub fn type_stats(tree: &Tree) -> Vec<TypeStat> {
    let mut map: HashMap<String, (u64, u64)> = HashMap::new();
    let mut buf = [0u8; 16];
    for nd in &tree.nodes {
        if nd.kind != FILE {
            continue;
        }
        let ext = lower_ext(&nd.name, &mut buf);
        match map.get_mut(ext) {
            Some(e) => {
                e.0 += nd.alloc;
                e.1 += 1;
            }
            None => {
                map.insert(ext.to_string(), (nd.alloc, 1));
            }
        }
    }
    let mut out: Vec<TypeStat> = map
        .into_iter()
        .map(|(ext, (alloc, count))| TypeStat { cat: category(&ext), ext, alloc, count })
        .collect();
    out.sort_unstable_by(|a, b| b.alloc.cmp(&a.alloc));
    out
}
