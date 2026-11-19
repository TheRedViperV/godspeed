//! In-memory model of a scanned volume or folder, shared by both scanners.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicU8};
use std::time::Duration;

pub const NONE: u8 = 0;
pub const FILE: u8 = 1;
pub const DIR: u8 = 2;

#[derive(Default, Clone)]
pub struct Node {
    pub name: Box<str>,
    /// Logical size in bytes (recursive for directories).
    pub size: u64,
    /// Bytes allocated on disk (recursive for directories).
    pub alloc: u64,
    /// Last write time as a Windows FILETIME.
    pub mtime: u64,
    pub parent: u32,
    pub child_start: u32,
    pub child_count: u32,
    /// Number of files (recursive for directories, 1 for files).
    pub files: u32,
    pub kind: u8,
    /// Quality of the name we picked; used while merging MFT records.
    pub rank: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    Mft,
    Walk,
}

impl Method {
    pub fn label(self) -> &'static str {
        match self {
            Method::Mft => "NTFS file table",
            Method::Walk => "Folder walk",
        }
    }
}

pub struct Tree {
    pub nodes: Vec<Node>,
    pub children: Vec<u32>,
    pub root: u32,
    pub method: Method,
    pub elapsed: Duration,
    /// Folders that couldn't be opened during a folder walk.
    pub skipped: u64,
    pub dirs: u64,
    /// Bumped whenever sizes change, so cached views can refresh.
    pub generation: u64,
    /// Children added after the scan (live updates), on top of `children`.
    added: HashMap<u32, Vec<u32>>,
}

impl Tree {
    /// Builds child lists and recursive totals. `nodes[i].parent` must already be set;
    /// anything not reachable from `root` is dropped.
    pub fn new(nodes: Vec<Node>, root: u32, method: Method) -> Tree {
        let mut t = Tree {
            nodes,
            children: Vec::new(),
            root,
            method,
            elapsed: Duration::ZERO,
            skipped: 0,
            dirs: 0,
            generation: 0,
            added: HashMap::new(),
        };
        t.finalize();
        t
    }

    fn finalize(&mut self) {
        let n = self.nodes.len();
        let root = self.root as usize;
        let linked = |nodes: &[Node], i: usize| -> Option<usize> {
            let nd = &nodes[i];
            if nd.kind == NONE || i == root {
                return None;
            }
            let p = nd.parent as usize;
            (p < nodes.len() && p != i && nodes[p].kind == DIR).then_some(p)
        };

        let mut counts = vec![0u32; n];
        for i in 0..n {
            if let Some(p) = linked(&self.nodes, i) {
                counts[p] += 1;
            }
        }
        let mut start = vec![0u32; n];
        let mut acc = 0u32;
        for i in 0..n {
            start[i] = acc;
            acc += counts[i];
        }
        let mut children = vec![0u32; acc as usize];
        let mut fill = start.clone();
        for i in 0..n {
            if let Some(p) = linked(&self.nodes, i) {
                children[fill[p] as usize] = i as u32;
                fill[p] += 1;
            }
        }
        for i in 0..n {
            let nd = &mut self.nodes[i];
            nd.child_start = start[i];
            nd.child_count = counts[i];
            if nd.kind == DIR {
                nd.size = 0;
                nd.alloc = 0;
                nd.files = 0;
            } else if nd.kind == FILE {
                nd.files = 1;
            }
        }

        // Pre-order walk from the root; walking it backwards visits children before parents.
        let mut order = Vec::with_capacity(n);
        let mut reach = vec![false; n];
        let mut stack = vec![self.root];
        while let Some(i) = stack.pop() {
            let iu = i as usize;
            if reach[iu] {
                continue;
            }
            reach[iu] = true;
            order.push(i);
            let nd = &self.nodes[iu];
            if nd.kind == DIR {
                let s = nd.child_start as usize;
                stack.extend_from_slice(&children[s..s + nd.child_count as usize]);
            }
        }
        let mut dirs = 0;
        for i in 0..n {
            if !reach[i] {
                let nd = &mut self.nodes[i];
                nd.kind = NONE;
                nd.name = Box::default();
            } else if self.nodes[i].kind == DIR {
                dirs += 1;
            }
        }
        for &i in order.iter().rev() {
            if i == self.root {
                continue;
            }
            let (s, a, f, p) = {
                let nd = &self.nodes[i as usize];
                (nd.size, nd.alloc, nd.files, nd.parent as usize)
            };
            let pn = &mut self.nodes[p];
            pn.size += s;
            pn.alloc += a;
            pn.files += f;
        }

        let nodes = &self.nodes;
        for nd in nodes.iter() {
            if nd.kind == DIR && nd.child_count > 1 {
                let s = nd.child_start as usize;
                children[s..s + nd.child_count as usize].sort_unstable_by(|&a, &b| {
                    let (na, nb) = (&nodes[a as usize], &nodes[b as usize]);
                    nb.alloc.cmp(&na.alloc).then(nb.size.cmp(&na.size))
                });
            }
        }
        self.children = children;
        self.dirs = dirs;
    }

    #[inline]
    pub fn node(&self, i: u32) -> &Node {
        &self.nodes[i as usize]
    }

    /// All children ever linked to `i`, including removed ones.
    fn raw_kids(&self, i: u32) -> impl Iterator<Item = u32> + '_ {
        let nd = &self.nodes[i as usize];
        let s = nd.child_start as usize;
        let scanned = self.children[s..s + nd.child_count as usize].iter().copied();
        scanned.chain(self.added.get(&i).into_iter().flatten().copied())
    }

    /// Live children. Largest first as of the scan; live updates can change sizes
    /// and append children, so callers that need order should sort.
    pub fn kids(&self, i: u32) -> impl Iterator<Item = u32> + '_ {
        self.raw_kids(i).filter(move |&c| self.nodes[c as usize].kind != NONE)
    }

    /// Live children sorted largest first.
    pub fn sorted_kids(&self, i: u32) -> Vec<u32> {
        let mut v: Vec<u32> = self.kids(i).collect();
        v.sort_by(|&a, &b| self.node(b).alloc.cmp(&self.node(a).alloc));
        v
    }

    pub fn child_named(&self, parent: u32, name: &str) -> Option<u32> {
        self.kids(parent).find(|&c| eq_ci(&self.nodes[c as usize].name, name))
    }

    /// Adds size, allocation and file-count deltas to `from` and every ancestor.
    fn bubble(&mut self, from: u32, size: i64, alloc: i64, files: i64) {
        let add = |v: u64, d: i64| (v as i64).saturating_add(d).max(0) as u64;
        let mut cur = from;
        loop {
            let nd = &mut self.nodes[cur as usize];
            nd.size = add(nd.size, size);
            nd.alloc = add(nd.alloc, alloc);
            nd.files = add(nd.files as u64, files) as u32;
            if cur == self.root {
                break;
            }
            cur = nd.parent;
        }
        self.generation += 1;
    }

    /// Links a new file or empty folder under `parent` (from a live update).
    pub fn insert(&mut self, parent: u32, name: &str, kind: u8, size: u64, alloc: u64, mtime: u64) -> u32 {
        let i = self.nodes.len() as u32;
        self.nodes.push(Node { name: name.into(), parent, kind, mtime, rank: 1, ..Default::default() });
        self.added.entry(parent).or_default().push(i);
        if kind == DIR {
            self.dirs += 1;
            self.generation += 1;
        } else {
            self.nodes[i as usize].size = size;
            self.nodes[i as usize].alloc = alloc;
            self.nodes[i as usize].files = 1;
            self.bubble(parent, size as i64, alloc as i64, 1);
        }
        i
    }

    /// New size for an existing file (from a live update).
    pub fn update_file(&mut self, i: u32, size: u64, alloc: u64, mtime: u64) {
        let nd = &mut self.nodes[i as usize];
        nd.mtime = mtime;
        if nd.size == size && nd.alloc == alloc {
            return;
        }
        let (ds, da) = (size as i64 - nd.size as i64, alloc as i64 - nd.alloc as i64);
        nd.size = size;
        nd.alloc = alloc;
        let parent = nd.parent;
        self.bubble(parent, ds, da, 0);
    }

    pub fn has_kids(&self, i: u32) -> bool {
        self.kids(i).next().is_some()
    }

    /// Nodes from the root down to `i`, inclusive.
    pub fn ancestors(&self, i: u32) -> Vec<u32> {
        let mut chain = vec![i];
        let mut cur = i;
        while cur != self.root && chain.len() < 4096 {
            cur = self.nodes[cur as usize].parent;
            chain.push(cur);
        }
        chain.reverse();
        chain
    }

    pub fn path(&self, i: u32) -> String {
        let chain = self.ancestors(i);
        let mut s = self.nodes[self.root as usize].name.to_string();
        for &n in &chain[1..] {
            if !s.ends_with('\\') {
                s.push('\\');
            }
            s.push_str(&self.nodes[n as usize].name);
        }
        s
    }

    /// Case-insensitive lookup of an absolute path inside the tree.
    pub fn find(&self, path: &str) -> Option<u32> {
        let root_path = &*self.nodes[self.root as usize].name;
        let prefix = root_path.trim_end_matches('\\');
        if path.len() < prefix.len()
            || !path.is_char_boundary(prefix.len())
            || !eq_ci(&path[..prefix.len()], prefix)
        {
            return None;
        }
        let rest = &path[prefix.len()..];
        if !rest.is_empty() && !rest.starts_with('\\') {
            return None;
        }
        let mut cur = self.root;
        for seg in rest.split('\\').filter(|s| !s.is_empty()) {
            cur = self.kids(cur).find(|&c| eq_ci(&self.nodes[c as usize].name, seg))?;
        }
        Some(cur)
    }

    /// Drops a node (and everything below it) and subtracts it from its ancestors.
    pub fn remove(&mut self, i: u32) {
        if i == self.root || self.nodes[i as usize].kind == NONE {
            return;
        }
        let (size, alloc, files) = {
            let nd = &self.nodes[i as usize];
            (nd.size, nd.alloc, nd.files)
        };
        let mut stack = vec![i];
        while let Some(n) = stack.pop() {
            if self.nodes[n as usize].kind == NONE {
                continue;
            }
            let kids: Vec<u32> = self.raw_kids(n).collect();
            stack.extend(kids);
            if self.nodes[n as usize].kind == DIR {
                self.dirs = self.dirs.saturating_sub(1);
            }
            self.nodes[n as usize].kind = NONE;
        }
        let parent = self.nodes[i as usize].parent;
        self.bubble(parent, -(size as i64), -(alloc as i64), -(files as i64));
    }

    /// Records a new on-disk size for `i` after part of it was deleted.
    pub fn shrink_to(&mut self, i: u32, alloc: u64) {
        let old = self.nodes[i as usize].alloc;
        if alloc >= old {
            return;
        }
        let delta = old - alloc;
        let mut cur = i;
        loop {
            let nd = &mut self.nodes[cur as usize];
            nd.alloc = nd.alloc.saturating_sub(delta);
            nd.size = nd.size.saturating_sub(delta);
            if cur == self.root {
                break;
            }
            cur = nd.parent;
        }
        self.generation += 1;
    }
}

pub fn eq_ci(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        a.eq_ignore_ascii_case(b)
    } else {
        a.to_lowercase() == b.to_lowercase()
    }
}

pub const PH_MFT: u8 = 1;
pub const PH_WALK: u8 = 2;
pub const PH_ANALYZE: u8 = 3;

#[derive(Default)]
pub struct Progress {
    pub phase: AtomicU8,
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub items: AtomicU64,
}

/// File extension, if it looks like one (short, plain ASCII).
pub fn ext_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(p) if p > 0 && p + 1 < name.len() && name.len() - p - 1 <= 10 => {
            let e = &name[p + 1..];
            if e.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
                e
            } else {
                ""
            }
        }
        _ => "",
    }
}

/// Lower-cased extension written into `buf`.
pub fn lower_ext<'a>(name: &str, buf: &'a mut [u8; 16]) -> &'a str {
    let e = ext_of(name).as_bytes();
    for (i, b) in e.iter().enumerate() {
        buf[i] = b.to_ascii_lowercase();
    }
    std::str::from_utf8(&buf[..e.len()]).unwrap_or("")
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Cat {
    Video = 0,
    Image,
    Audio,
    Archive,
    Code,
    Document,
    Program,
    Data,
    Game,
    Other,
}

impl Cat {
    pub const ALL: [Cat; 10] = [
        Cat::Video,
        Cat::Image,
        Cat::Audio,
        Cat::Archive,
        Cat::Code,
        Cat::Document,
        Cat::Program,
        Cat::Data,
        Cat::Game,
        Cat::Other,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Cat::Video => "Video",
            Cat::Image => "Images",
            Cat::Audio => "Audio",
            Cat::Archive => "Archives & disk images",
            Cat::Code => "Code",
            Cat::Document => "Documents",
            Cat::Program => "Programs",
            Cat::Data => "Data & models",
            Cat::Game => "Game data",
            Cat::Other => "Other",
        }
    }
}

pub fn category(ext: &str) -> Cat {
    match ext {
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "m4v" | "webm" | "flv" | "mpg" | "mpeg" | "ts"
        | "m2ts" | "mts" | "3gp" | "vob" => Cat::Video,
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "tif" | "tiff" | "webp" | "heic" | "heif"
        | "raw" | "cr2" | "cr3" | "nef" | "arw" | "dng" | "psd" | "svg" | "ico" | "avif" => {
            Cat::Image
        }
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" | "opus" | "aiff" | "mid" => {
            Cat::Audio
        }
        "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "iso" | "img"
        | "vhd" | "vhdx" | "vmdk" | "qcow2" | "vdi" | "cab" | "wim" | "esd" | "dmg" => {
            Cat::Archive
        }
        "rs" | "py" | "js" | "mjs" | "cjs" | "tsx" | "jsx" | "java" | "c" | "cc"
        | "cpp" | "h" | "hpp" | "cs" | "go" | "rb" | "php" | "swift" | "kt" | "json" | "yaml"
        | "yml" | "toml" | "xml" | "html" | "css" | "scss" | "md" | "lock" | "map" | "pyc"
        | "class" | "o" | "obj" | "pdb" | "lib" | "a" | "rlib" | "rmeta" | "ipynb" | "d" => {
            Cat::Code
        }
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp"
        | "txt" | "rtf" | "csv" | "epub" | "one" => Cat::Document,
        "exe" | "dll" | "sys" | "msi" | "msix" | "appx" | "msp" | "drv" | "ocx" | "mui"
        | "so" | "node" | "jar" | "apk" | "efi" => Cat::Program,
        "pak" | "ucas" | "utoc" | "rpf" | "rpkg" | "archive" | "vpk" | "bsa" | "ba2" | "forge" | "bundle"
        | "assets" | "resource" | "big" | "wad" | "pck" | "arc" | "cpk" | "obb" | "ff" | "sb" | "cas"
        | "toc" | "nca" => Cat::Game,
        "db" | "sqlite" | "sqlite3" | "mdb" | "accdb" | "ldf" | "mdf" | "bak" | "dat" | "bin"
        | "log" | "etl" | "evtx" | "edb" | "ost" | "pst" | "blob" | "gguf"
        | "safetensors" | "ckpt" | "pt" | "pth" | "onnx" | "parquet" | "npy" | "npz" => {
            Cat::Data
        }
        _ => Cat::Other,
    }
}

pub fn cat_of(name: &str) -> Cat {
    let mut buf = [0u8; 16];
    category(lower_ext(name, &mut buf))
}
