//! Keeps the last scan of each drive or folder on disk so the app can reopen it
//! instantly. Stored LZ4-compressed under %LOCALAPPDATA%\<app>\scans.

use crate::space::analysis::{self, Data};
use crate::space::tree::*;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAGIC: &[u8; 4] = b"SPLN";
const VERSION: u32 = 1;

fn dir() -> Option<PathBuf> {
    Some(crate::edition::data_dir()?.join("scans"))
}

/// Cache file for a scan root, e.g. `C:\` -> `c.scan`.
pub fn path_for(root: &str) -> Option<PathBuf> {
    let key: String = root
        .trim_end_matches('\\')
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string();
    Some(dir()?.join(format!("{key}.scan")))
}

/// The most recently saved scan, if any.
pub fn latest() -> Option<PathBuf> {
    fs::read_dir(dir()?)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "scan"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        let s = self.b.get(self.pos..self.pos + n).ok_or_else(|| bad("truncated scan file"))?;
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn str(&mut self) -> io::Result<String> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| bad("bad name in scan file"))
    }
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Serializes the live part of the tree. Cheap enough to run before handing the
/// data to the UI; compression and disk I/O happen in `write`.
pub fn encode(data: &Data) -> Vec<u8> {
    let tree = &data.tree;
    let mut w = Writer(Vec::with_capacity(tree.nodes.len() * 40));
    w.0.extend_from_slice(MAGIC);
    w.u32(VERSION);
    w.u8(match tree.method {
        Method::Mft => 0,
        Method::Walk => 1,
    });
    w.u64(data.scanned_at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()));
    w.u64(tree.elapsed.as_millis() as u64);
    w.u64(tree.skipped);
    w.str(data.note.as_deref().unwrap_or(""));

    // Pre-order, so every parent is written (and numbered) before its children.
    let count_at = w.0.len();
    w.u32(0);
    let mut new_index = vec![u32::MAX; tree.nodes.len()];
    let mut stack = vec![tree.root];
    let mut count = 0u32;
    while let Some(n) = stack.pop() {
        let nd = tree.node(n);
        new_index[n as usize] = count;
        let parent = if n == tree.root { 0 } else { new_index[nd.parent as usize] };
        w.u8(nd.kind);
        w.u32(parent);
        w.str(&nd.name);
        w.u64(nd.mtime);
        if nd.kind == FILE {
            w.u64(nd.size);
            w.u64(nd.alloc);
        }
        count += 1;
        if nd.kind == DIR {
            stack.extend(tree.kids(n));
        }
    }
    w.0[count_at..count_at + 4].copy_from_slice(&count.to_le_bytes());
    w.0
}

/// Compresses an encoded scan and saves it, replacing any older scan of the same root.
pub fn write(root: &str, raw: Vec<u8>) -> io::Result<()> {
    let path = path_for(root).ok_or_else(|| bad("no local app data folder"))?;
    fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, lz4_flex::compress_prepend_size(&raw))?;
    fs::rename(&tmp, &path)
}

pub fn save(data: &Data) -> io::Result<()> {
    let root = data.tree.node(data.tree.root).name.to_string();
    write(&root, encode(data))
}

pub fn load(path: &PathBuf) -> io::Result<Data> {
    let packed = fs::read(path)?;
    let raw = lz4_flex::decompress_size_prepended(&packed).map_err(|_| bad("damaged scan file"))?;
    let mut r = Reader { b: &raw, pos: 0 };
    if r.take(4)? != MAGIC || r.u32()? != VERSION {
        return Err(bad("scan file from another version"));
    }
    let method = if r.u8()? == 0 { Method::Mft } else { Method::Walk };
    let scanned_at = UNIX_EPOCH + Duration::from_secs(r.u64()?);
    let elapsed = Duration::from_millis(r.u64()?);
    let skipped = r.u64()?;
    let note = Some(r.str()?).filter(|s| !s.is_empty());
    let count = r.u32()? as usize;
    let mut nodes = Vec::with_capacity(count);
    for _ in 0..count {
        let kind = r.u8()?;
        let parent = r.u32()?;
        let name = r.str()?.into_boxed_str();
        let mtime = r.u64()?;
        let (size, alloc) = if kind == FILE { (r.u64()?, r.u64()?) } else { (0, 0) };
        if parent as usize >= nodes.len().max(1) {
            return Err(bad("damaged scan file"));
        }
        nodes.push(Node { name, size, alloc, mtime, parent, kind, rank: 1, ..Default::default() });
    }
    if nodes.is_empty() {
        return Err(bad("empty scan file"));
    }
    let mut tree = Tree::new(nodes, 0, method);
    tree.elapsed = elapsed;
    tree.skipped = skipped;
    Ok(analysis::analyze(tree, note, scanned_at))
}

/// "just now", "5 min ago", "yesterday at 14:02", …
pub fn age(t: SystemTime) -> String {
    let secs = SystemTime::now().duration_since(t).map_or(0, |d| d.as_secs());
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => {
            let h = secs / 3600;
            if h == 1 { "1 hour ago".into() } else { format!("{h} hours ago") }
        }
        86_400..=172_799 => "yesterday".into(),
        _ => format!("{} days ago", secs / 86_400),
    }
}
