//! The searchable index: every file and folder on every indexed drive, kept in
//! compact arrays so a search can sweep millions of names in milliseconds.

use crate::{edition, ntfs, usn, walk, win};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const NONE: u8 = 0;
pub const FILE: u8 = 1;
pub const DIR: u8 = 2;

const LIVE: u8 = 1;
const HIDDEN: u8 = 2;
const NOISY: u8 = 4;
const USER: u8 = 8;

/// How often the change journal is checked while the window is in front. Reading
/// it when nothing changed is a cheap no-op, so this can be frequent.
const JOURNAL_POLL: Duration = Duration::from_millis(100);
/// While something else is in front (a game, say), changes are gathered in
/// bigger, rarer batches.
const JOURNAL_POLL_AWAY: Duration = Duration::from_secs(2);
const DRIVE_POLL: Duration = Duration::from_secs(2);
const DRIVE_POLL_AWAY: Duration = Duration::from_secs(10);

/// Folders that are mostly app or system internals. Matches inside them still
/// show up, just below everything else.
const NOISE: &[&[u8]] = &[
    b"winsxs",
    b"appdata",
    b"node_modules",
    b".git",
    b"site-packages",
    b"dist-packages",
    b"__pycache__",
    b"$recycle.bin",
    b"system volume information",
    b".cache",
    b"cache",
    b"caches",
    b"temp",
    b"tmp",
    b".gradle",
    b".nuget",
    b".venv",
    b"venv",
    b"driverstore",
    b"servicing",
    b"installer",
    b"assembly",
    b"steamapps",
    b"logs",
    b"shadercache",
    b"crashdumps",
    b"gpucache",
    b"code cache",
    b"intermediates",
    b"generated",
    b"obj",
];
/// The same, but only right under a drive root.
const TOP_NOISE: &[&[u8]] = &[
    b"windows",
    b"programdata",
    b"recovery",
    b"perflogs",
    b"$windows.~bt",
    b"$windows.~ws",
    b"$winreagent",
    b"$sysreset",
    b"msocache",
    b"onedrivetemp",
    b"config.msi",
];

/// Top-level folders on any drive that hold software rather than someone's files.
const SOFTWARE_TOP: &[&[u8]] = &[
    b"program files",
    b"program files (x86)",
    b"windowsapps",
    b"xboxgames",
    b"users",
    b"windows",
    b"programdata",
];

/// Profiles under C:\Users that aren't a person.
const NOT_PEOPLE: &[&[u8]] = &[b"public", b"default", b"default user", b"all users"];

fn is_noise(lower: &[u8], top: bool) -> bool {
    // Dot-folders (.cargo, .android, .vscode...) hold tool caches and settings.
    lower.first() == Some(&b'.') || NOISE.contains(&lower) || (top && TOP_NOISE.contains(&lower))
}

/// One file or folder. Its name lives in the volume's shared byte arena.
#[derive(Clone, Copy, Default)]
pub struct Entry {
    pub size: u64,
    /// Last write time, Unix seconds.
    pub mtime: u32,
    pub parent: u32,
    name: u32,
    lower: u32,
    name_len: u16,
    lower_len: u16,
    pub kind: u8,
    flags: u8,
}

impl Entry {
    /// Reachable from the drive root, so it shows up in searches.
    pub fn live(&self) -> bool {
        self.flags & LIVE != 0
    }

    /// Hidden, or inside system or app folders: ranked below everything else.
    pub fn noisy(&self) -> bool {
        self.flags & NOISY != 0
    }

    /// Someone's own files: under a user profile (C:\Users\me), or anywhere on a
    /// data drive outside software folders. These rank first.
    pub fn user(&self) -> bool {
        self.flags & USER != 0
    }
}

/// A file or folder as a scanner reports it, before it is packed into the arena.
#[derive(Default)]
pub struct Raw {
    pub name: Box<str>,
    pub size: u64,
    pub mtime: u32,
    pub parent: u32,
    pub kind: u8,
    /// Hidden or system attribute.
    pub hidden: bool,
    /// Quality of the name picked; used while merging MFT records.
    pub rank: u8,
    /// Hard links in other folders: (name, parent, rank).
    pub links: Vec<(Box<str>, u32, u8)>,
}

/// Marks an entry index as one of a volume's hard links rather than a record.
pub const LINK: u32 = 1 << 31;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    Mft,
    Walk,
}

impl Method {
    pub fn label(self) -> &'static str {
        match self {
            Method::Mft => "NTFS file table",
            Method::Walk => "folder walk",
        }
    }
}

pub struct Volume {
    pub letter: char,
    pub method: Method,
    /// For MFT volumes, indexed by MFT record number.
    pub entries: Vec<Entry>,
    /// Extra names of hard-linked files, addressed as `LINK | n`.
    pub links: Vec<Entry>,
    /// The record each link belongs to, in ascending order.
    link_base: Vec<u32>,
    /// Every name, followed by its lowercase form when that differs.
    names: Vec<u8>,
    pub root: u32,
    /// Number of searchable entries.
    pub items: u64,
    pub elapsed: Duration,
    /// Following the change journal, so the index stays current.
    pub live: bool,
    /// The drive Windows runs from, where only user profiles count as personal files.
    system: bool,
    epoch: u64,
    /// Finds a folder's child by name: open addressing over entry indexes (+1, 0
    /// empty), keyed by (parent, lowercase name). Only for walked volumes, which
    /// are kept current by path rather than by MFT record number.
    table: Vec<u32>,
    table_used: usize,
    /// Changed since it was last saved to disk.
    pub dirty: bool,
    /// A network drive that's disconnected: still searchable from its saved index.
    pub offline: bool,
}

/// Where a drive's walked index is saved between runs.
fn cache_file(letter: char) -> Option<std::path::PathBuf> {
    Some(edition::data_dir()?.join("index").join(format!("{}.idx", letter.to_ascii_lowercase())))
}

const CACHE_MAGIC: &[u8; 4] = b"GSIX";
const CACHE_VERSION: u32 = 1;

fn slot_of(parent: u32, lower: &[u8], mask: usize) -> usize {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ parent as u64;
    for &b in lower {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    (h ^ (h >> 29)) as usize & mask
}

fn put_entry(b: &mut Vec<u8>, e: &Entry) {
    b.extend_from_slice(&e.size.to_le_bytes());
    for v in [e.mtime, e.parent, e.name, e.lower] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(&e.name_len.to_le_bytes());
    b.extend_from_slice(&e.lower_len.to_le_bytes());
    b.push(e.kind);
    b.push(e.flags);
}

/// Reads the saved index back, refusing anything that doesn't add up.
struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.p..self.p.checked_add(n)?)?;
        self.p += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn entry(&mut self) -> Option<Entry> {
        Some(Entry {
            size: self.u64()?,
            mtime: self.u32()?,
            parent: self.u32()?,
            name: self.u32()?,
            lower: self.u32()?,
            name_len: self.u16()?,
            lower_len: self.u16()?,
            kind: self.u8()?,
            flags: self.u8()?,
        })
    }
    fn entries(&mut self) -> Option<Vec<Entry>> {
        let n = self.u64()? as usize;
        let mut v = Vec::with_capacity(n.min(self.b.len() / 30));
        for _ in 0..n {
            v.push(self.entry()?);
        }
        Some(v)
    }
}

fn push_name(names: &mut Vec<u8>, name: &str) -> (u32, u16, u32, u16) {
    let name = if name.len() > u16::MAX as usize { "" } else { name };
    let off = names.len() as u32;
    let len = name.len() as u16;
    names.extend_from_slice(name.as_bytes());
    if name.is_ascii() {
        if !name.bytes().any(|b| b.is_ascii_uppercase()) {
            return (off, len, off, len);
        }
        let lo = names.len();
        names.extend_from_slice(name.as_bytes());
        names[lo..].make_ascii_lowercase();
        return (off, len, lo as u32, len);
    }
    let lower = name.to_lowercase();
    if lower == name || lower.len() > u16::MAX as usize {
        return (off, len, off, len);
    }
    let lo = names.len() as u32;
    names.extend_from_slice(lower.as_bytes());
    (off, len, lo, lower.len() as u16)
}

impl Volume {
    pub fn build(letter: char, method: Method, raws: Vec<Raw>, root: u32) -> Volume {
        let mut names = Vec::with_capacity(raws.len() * 24);
        let mut entries = Vec::with_capacity(raws.len());
        let (mut links, mut link_base) = (Vec::new(), Vec::new());
        let entry = |names: &mut Vec<u8>, r: &Raw, name: &str, parent: u32| {
            let (name, name_len, lower, lower_len) = push_name(names, name);
            let flags = if r.hidden { HIDDEN } else { 0 };
            Entry { size: r.size, mtime: r.mtime, parent, name, lower, name_len, lower_len, kind: r.kind, flags }
        };
        for (i, r) in raws.iter().enumerate() {
            if r.kind == NONE {
                entries.push(Entry::default());
                continue;
            }
            entries.push(entry(&mut names, r, &r.name, r.parent));
            for (name, parent, _) in &r.links {
                links.push(entry(&mut names, r, name, *parent));
                link_base.push(i as u32);
            }
        }
        drop(raws);
        names.shrink_to_fit();
        let system_drive = std::env::var("SystemDrive").ok().and_then(|s| s.chars().next()).unwrap_or('C');
        let mut v = Volume {
            letter,
            method,
            system: system_drive.eq_ignore_ascii_case(&letter),
            entries,
            links,
            link_base,
            names,
            root,
            items: 0,
            elapsed: Duration::ZERO,
            live: false,
            epoch: 0,
            table: Vec::new(),
            table_used: 0,
            dirty: false,
            offline: false,
        };
        v.relink();
        if method == Method::Walk {
            v.build_table();
        }
        v
    }

    /// A record or a link (`LINK | n`); an empty entry if there's no such thing.
    pub fn entry(&self, i: u32) -> Entry {
        let e = if i & LINK != 0 { self.links.get((i & !LINK) as usize) } else { self.entries.get(i as usize) };
        e.copied().unwrap_or_default()
    }

    pub fn name(&self, i: u32) -> &str {
        let e = self.entry(i);
        let (s, n) = (e.name as usize, e.name_len as usize);
        std::str::from_utf8(&self.names[s..s + n]).unwrap_or("")
    }

    pub fn lower(&self, i: u32) -> &[u8] {
        let e = self.entry(i);
        let (s, n) = (e.lower as usize, e.lower_len as usize);
        &self.names[s..s + n]
    }

    pub fn names_len(&self) -> usize {
        self.names.len()
    }

    /// Positions in `links` of a record's hard links.
    fn links_of(&self, record: u32) -> std::ops::Range<usize> {
        let start = self.link_base.partition_point(|&b| b < record);
        let end = start + self.link_base[start..].partition_point(|&b| b == record);
        start..end
    }

    /// New size and date for a file and all its hard links.
    pub fn set_size(&mut self, record: u32, size: u64, mtime: u32) {
        let links = self.links_of(record);
        let all = self.entries.get_mut(record as usize).into_iter().chain(&mut self.links[links]);
        for e in all.filter(|e| e.kind == FILE) {
            e.size = size;
            if mtime != 0 {
                e.mtime = mtime;
            }
        }
    }

    /// Full path, like `C:\Users\me\file.txt` (or `C:\` for the root).
    pub fn path(&self, i: u32) -> String {
        let mut parts = Vec::new();
        let mut j = i;
        while j != self.root && parts.len() < 1024 {
            let e = self.entry(j);
            if e.kind == NONE {
                break;
            }
            parts.push(j);
            j = e.parent;
        }
        let mut s = format!("{}:", self.letter);
        for &k in parts.iter().rev() {
            s.push('\\');
            s.push_str(self.name(k));
        }
        if parts.is_empty() {
            s.push('\\');
        }
        s
    }

    /// How many folders deep an entry is.
    pub fn depth(&self, i: u32) -> u16 {
        let (mut j, mut d) = (i, 0u16);
        while j != self.root && d < 1024 {
            let e = self.entry(j);
            if e.kind == NONE {
                break;
            }
            j = e.parent;
            d += 1;
        }
        d
    }

    /// The folder at a path like `C:\Users\me`, if it's in the index.
    pub fn find_dir(&self, path: &str) -> Option<u32> {
        self.find_path(path).filter(|&i| self.entry(i).kind == DIR)
    }

    /// The file or folder at a full path, if it's in the index.
    pub fn find_path(&self, path: &str) -> Option<u32> {
        let rest = path.get(2..)?;
        let mut cur = self.root;
        for comp in rest.split(['\\', '/']).filter(|c| !c.is_empty()) {
            cur = self.child(cur, comp.to_lowercase().as_bytes())?;
        }
        Some(cur)
    }

    /// A folder's child by (lowercase) name.
    fn child(&self, parent: u32, lower: &[u8]) -> Option<u32> {
        let matches = |i: u32| {
            let e = &self.entries[i as usize];
            e.kind != NONE && e.parent == parent && i != parent && self.lower(i) == lower
        };
        if self.table.is_empty() {
            // No lookup table (MFT volumes): look through everything.
            return (0..self.entries.len() as u32).into_par_iter().find_any(|&i| matches(i));
        }
        let mask = self.table.len() - 1;
        let mut s = slot_of(parent, lower, mask);
        while self.table[s] != 0 {
            let i = self.table[s] - 1;
            if matches(i) {
                return Some(i);
            }
            s = (s + 1) & mask;
        }
        None
    }

    fn build_table(&mut self) {
        let size = (self.entries.len() * 2).next_power_of_two().max(1 << 12);
        let mask = size - 1;
        let mut table = vec![0u32; size];
        let mut used = 0;
        for i in 0..self.entries.len() as u32 {
            let e = self.entries[i as usize];
            if e.kind == NONE {
                continue;
            }
            let mut s = slot_of(e.parent, self.lower(i), mask);
            while table[s] != 0 {
                s = (s + 1) & mask;
            }
            table[s] = i + 1;
            used += 1;
        }
        self.table = table;
        self.table_used = used;
    }

    fn table_add(&mut self, i: u32) {
        if self.table.is_empty() {
            return;
        }
        if (self.table_used + 1) * 10 > self.table.len() * 7 {
            self.build_table(); // grows it, and includes `i`
            return;
        }
        let e = self.entries[i as usize];
        let mask = self.table.len() - 1;
        let mut s = slot_of(e.parent, self.lower(i), mask);
        while self.table[s] != 0 {
            s = (s + 1) & mask;
        }
        self.table[s] = i + 1;
        self.table_used += 1;
    }

    /// Works out a new or moved entry's flags from its folder.
    fn settle(&mut self, i: u32) {
        let e = self.entries[i as usize];
        let p = self.entries.get(e.parent as usize).copied().unwrap_or_default();
        let live = p.live() && p.kind == DIR;
        let noisy = live && (p.noisy() || self.self_noisy(i));
        let user = live && (p.user() || self.self_user(i));
        self.entries[i as usize].flags = (e.flags & HIDDEN) | if live { LIVE } else { 0 } | tag_flags(noisy, user);
        self.items = self.items + live as u64 - e.live() as u64;
    }

    /// Adds `name` to folder `parent`, or updates it if it's already there.
    /// Returns its index and whether it's new. Walked volumes only.
    pub fn upsert(&mut self, parent: u32, name: &str, kind: u8, size: u64, mtime: u32, hidden: bool) -> (u32, bool) {
        self.dirty = true;
        if let Some(i) = self.child(parent, name.to_lowercase().as_bytes()) {
            let e = &mut self.entries[i as usize];
            e.kind = kind;
            if kind == FILE {
                e.size = size;
            }
            if mtime != 0 {
                e.mtime = mtime;
            }
            e.flags = (e.flags & !HIDDEN) | if hidden { HIDDEN } else { 0 };
            return (i, false);
        }
        let (n, name_len, lower, lower_len) = push_name(&mut self.names, name);
        let i = self.entries.len() as u32;
        let flags = if hidden { HIDDEN } else { 0 };
        self.entries.push(Entry { size, mtime, parent, name: n, lower, name_len, lower_len, kind, flags });
        self.settle(i);
        self.table_add(i);
        (i, true)
    }

    /// Takes an entry out. Returns whether it was a folder (its contents then need relinking).
    pub fn remove_entry(&mut self, i: u32) -> bool {
        let Some(e) = self.entries.get_mut(i as usize) else { return false };
        let dir = e.kind == DIR;
        self.items -= e.live() as u64;
        *e = Entry::default();
        self.dirty = true;
        dir
    }

    /// Renames or moves an entry. Returns whether it's a folder.
    pub fn move_entry(&mut self, i: u32, parent: u32, name: &str) -> bool {
        let (n, name_len, lower, lower_len) = push_name(&mut self.names, name);
        let e = &mut self.entries[i as usize];
        (e.parent, e.name, e.name_len, e.lower, e.lower_len) = (parent, n, name_len, lower, lower_len);
        let dir = e.kind == DIR;
        self.settle(i);
        self.table_add(i);
        self.dirty = true;
        dir
    }

    /// The folder at `path`, adding any part of it that isn't indexed yet.
    pub fn ensure_dir(&mut self, path: &str) -> Option<u32> {
        let rest = path.get(2..)?;
        let mut cur = self.root;
        for comp in rest.split(['\\', '/']).filter(|c| !c.is_empty()) {
            cur = match self.child(cur, comp.to_lowercase().as_bytes()) {
                Some(i) => i,
                None => self.upsert(cur, comp, DIR, 0, 0, false).0,
            };
        }
        Some(cur)
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Saves a walked index, so the next start can open it instantly. Returns the
    /// file size.
    pub fn save_cache(&self) -> std::io::Result<u64> {
        let path = cache_file(self.letter).ok_or_else(|| std::io::Error::other("no data folder"))?;
        let mut b = Vec::with_capacity((self.entries.len() + self.links.len()) * 30 + self.names.len() + 64);
        b.extend_from_slice(CACHE_MAGIC);
        b.extend_from_slice(&CACHE_VERSION.to_le_bytes());
        b.extend_from_slice(&(self.letter as u32).to_le_bytes());
        b.extend_from_slice(&self.root.to_le_bytes());
        for list in [&self.entries, &self.links] {
            b.extend_from_slice(&(list.len() as u64).to_le_bytes());
            for e in list.iter() {
                put_entry(&mut b, e);
            }
        }
        b.extend_from_slice(&(self.link_base.len() as u64).to_le_bytes());
        for v in &self.link_base {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&(self.names.len() as u64).to_le_bytes());
        b.extend_from_slice(&self.names);
        let packed = lz4_flex::compress_prepend_size(&b);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &packed)?;
        std::fs::rename(&tmp, &path)?;
        Ok(packed.len() as u64)
    }

    /// The index saved by `save_cache`, if there is one and it reads back cleanly.
    pub fn load_cache(letter: char) -> Option<Volume> {
        let packed = std::fs::read(cache_file(letter)?).ok()?;
        let b = lz4_flex::decompress_size_prepended(&packed).ok()?;
        let mut r = Reader { b: &b, p: 0 };
        if r.take(4)? != CACHE_MAGIC || r.u32()? != CACHE_VERSION || r.u32()? != letter as u32 {
            return None;
        }
        let root = r.u32()?;
        let entries = r.entries()?;
        let links = r.entries()?;
        let n = r.u64()? as usize;
        let link_base = (0..n).map(|_| r.u32()).collect::<Option<Vec<u32>>>()?;
        let n = r.u64()? as usize;
        let names = r.take(n)?.to_vec();
        let fits = |e: &Entry| {
            e.name as usize + e.name_len as usize <= names.len() && e.lower as usize + e.lower_len as usize <= names.len()
        };
        if root as usize >= entries.len() || !entries.iter().chain(&links).all(fits) || link_base.len() != links.len() {
            return None;
        }
        let system_drive = std::env::var("SystemDrive").ok().and_then(|s| s.chars().next()).unwrap_or('C');
        let mut v = Volume {
            letter,
            method: Method::Walk,
            system: system_drive.eq_ignore_ascii_case(&letter),
            entries,
            links,
            link_base,
            names,
            root,
            items: 0,
            elapsed: Duration::ZERO,
            live: false,
            epoch: 0,
            table: Vec::new(),
            table_used: 0,
            dirty: false,
            offline: false,
        };
        // Worked out again rather than trusted, so ranking rules can change between versions.
        v.relink();
        v.build_table();
        Some(v)
    }

    /// Brings entries in whose folder showed up later in the same batch of changes.
    fn adopt(&mut self, orphans: &[u32]) -> Vec<u32> {
        let mut files = Vec::new();
        for &i in orphans {
            let e = self.entries[i as usize];
            let parent = self.entries.get(e.parent as usize).copied().unwrap_or_default();
            if e.kind == NONE || e.live() || !parent.live() || parent.kind != DIR {
                continue;
            }
            let noisy = parent.noisy() || self.self_noisy(i);
            let user = parent.user() || self.self_user(i);
            self.entries[i as usize].flags = (e.flags & HIDDEN) | LIVE | tag_flags(noisy, user);
            self.items += 1;
            if e.kind == FILE {
                files.push(i);
            }
        }
        files
    }

    fn self_noisy(&self, i: u32) -> bool {
        let e = self.entry(i);
        e.flags & HIDDEN != 0 || is_noise(self.lower(i), e.parent == self.root)
    }

    /// Where someone's own files start: a profile folder (C:\Users\me), or a
    /// top-level folder on a data drive that isn't for software.
    fn self_user(&self, i: u32) -> bool {
        let e = self.entry(i);
        let name = self.lower(i);
        if e.parent == self.root {
            return !self.system && !SOFTWARE_TOP.contains(&name) && !TOP_NOISE.contains(&name);
        }
        let p = self.entry(e.parent);
        p.parent == self.root && self.lower(e.parent) == b"users" && !NOT_PEOPLE.contains(&name)
    }

    /// The folder `depth` levels below the root that an entry is in (its parent,
    /// if it isn't that deep). Used to group look-alike results.
    pub fn ancestor(&self, i: u32, depth: usize) -> u32 {
        let mut chain = Vec::new();
        let mut j = self.entry(i).parent;
        while j != self.root && chain.len() < 1024 {
            let e = self.entry(j);
            if e.kind == NONE {
                break;
            }
            chain.push(j);
            j = e.parent;
        }
        match chain.len() {
            0 => self.root,
            n if n >= depth => chain[n - depth],
            _ => chain[0],
        }
    }

    /// Marks which entries hang off the root (orphans, NTFS internals and anything
    /// under a broken parent chain stay out of search results), and which are
    /// inside system or app folders.
    pub fn relink(&mut self) {
        let n = self.entries.len();
        let root = self.root as usize;
        // 0 unknown, 1 live, 2 dead, 3 on the current walk
        let mut state = vec![0u8; n];
        if root < n {
            state[root] = 1;
        }
        let mut stack = Vec::new();
        for i in 0..n {
            if state[i] != 0 {
                continue;
            }
            let mut j = i;
            let verdict = loop {
                if j >= n || self.entries[j].kind == NONE {
                    break 2;
                }
                match state[j] {
                    1 | 2 => break state[j],
                    3 => break 2, // a loop in the parent chain
                    _ => {}
                }
                state[j] = 3;
                stack.push(j);
                j = self.entries[j].parent as usize;
            };
            for k in stack.drain(..) {
                state[k] = verdict;
            }
        }
        // Noise and ownership are inherited from the folders above. Tags: bit 0 worked
        // out, bit 1 noisy, bit 2 someone's own files.
        let mut tags = vec![0u8; n];
        if root < n {
            tags[root] = 1;
        }
        for i in 0..n {
            if tags[i] != 0 || state[i] != 1 {
                continue;
            }
            let mut j = i;
            let mut above = loop {
                if tags[j] != 0 {
                    break tags[j];
                }
                stack.push(j);
                j = self.entries[j].parent as usize;
            };
            while let Some(k) = stack.pop() {
                let noisy = above & 2 != 0 || self.self_noisy(k as u32);
                let user = above & 4 != 0 || self.self_user(k as u32);
                above = 1 | (noisy as u8) << 1 | (user as u8) << 2;
                tags[k] = above;
            }
        }
        let mut items = 0;
        for (i, e) in self.entries.iter_mut().enumerate() {
            let live = state[i] == 1 && e.kind != NONE;
            let tags = if live { tag_flags(tags[i] & 2 != 0, tags[i] & 4 != 0) } else { 0 };
            e.flags = (e.flags & HIDDEN) | if live { LIVE } else { 0 } | tags;
            items += live as u64;
        }
        for k in 0..self.links.len() {
            let e = self.links[k];
            let parent = self.entries.get(e.parent as usize).copied().unwrap_or_default();
            let live = e.kind != NONE && parent.live();
            let noisy = live
                && (parent.noisy() || e.flags & HIDDEN != 0 || is_noise(self.lower(LINK | k as u32), e.parent == self.root));
            let user = live && parent.user();
            self.links[k].flags = (e.flags & HIDDEN) | if live { LIVE } else { 0 } | tag_flags(noisy, user);
            items += live as u64;
        }
        self.items = items;
    }

    /// Applies one change from the journal.
    pub fn apply(&mut self, c: &usn::Change) -> Applied {
        let i = c.index as usize;
        if i < ntfs::FIRST_USER_RECORD {
            return Applied::Done;
        }
        let links = self.links_of(c.index);
        if c.deleted {
            // A deleted folder's contents were deleted first, so nothing needs relinking.
            let Some(e) = self.entries.get_mut(i) else { return Applied::Done };
            for e in std::iter::once(e).chain(&mut self.links[links]) {
                self.items -= e.live() as u64;
                *e = Entry::default();
            }
            return Applied::Done;
        }
        // A change made through one of a file's other hard links.
        if let Some(k) = links.clone().find(|&k| self.links[k].parent == c.parent) {
            if self.name(LINK | k as u32) != c.name {
                let (name, name_len, lower, lower_len) = push_name(&mut self.names, &c.name);
                let l = &mut self.links[k];
                (l.name, l.name_len, l.lower, l.lower_len) = (name, name_len, lower, lower_len);
            }
            self.links[k].mtime = c.time;
            return if self.entries.get(i).is_some_and(|e| e.live()) { Applied::Stat } else { Applied::Done };
        }
        if i >= self.entries.len() {
            self.entries.resize(i + 1, Entry::default());
        }
        let old = self.entries[i];
        let parent = self.entries.get(c.parent as usize).copied().unwrap_or_default();
        let parent_live = parent.live() && parent.kind == DIR;
        let keep = old.kind != NONE && self.name(c.index) == c.name;
        let (name, name_len, lower, lower_len) = if keep {
            (old.name, old.name_len, old.lower, old.lower_len)
        } else {
            push_name(&mut self.names, &c.name)
        };
        let lower_bytes = &self.names[lower as usize..lower as usize + lower_len as usize];
        let noisy = parent_live && (parent.noisy() || c.hidden || is_noise(lower_bytes, c.parent == self.root));
        let flags = if c.hidden { HIDDEN } else { 0 } | if parent_live { LIVE } else { 0 } | tag_flags(noisy, false);
        self.entries[i] = Entry {
            size: if c.dir || !keep { 0 } else { old.size },
            mtime: c.time,
            parent: c.parent,
            name,
            lower,
            name_len,
            lower_len,
            kind: if c.dir { DIR } else { FILE },
            flags,
        };
        let user = parent_live && (parent.user() || self.self_user(c.index));
        if user {
            self.entries[i].flags |= USER;
        }
        self.items = self.items + parent_live as u64 - old.live() as u64;
        if c.dir && old.kind == DIR && (old.live() != parent_live || old.noisy() != noisy || old.user() != user) {
            // A folder that moved in or out of view takes its contents with it.
            Applied::Relink
        } else if !parent_live {
            // Its folder may come later in the same batch.
            Applied::Orphan
        } else if c.dir {
            Applied::Done
        } else {
            Applied::Stat
        }
    }
}

fn tag_flags(noisy: bool, user: bool) -> u8 {
    (if noisy { NOISY } else { 0 }) | if user { USER } else { 0 }
}

/// What applying a journal change left to do.
pub enum Applied {
    Done,
    /// A file whose size and date should be read again.
    Stat,
    /// Its folder isn't in the index (yet).
    Orphan,
    /// A folder moved; liveness below it must be worked out again.
    Relink,
}

#[derive(Default)]
pub struct Index {
    pub vols: Vec<Volume>,
    /// Bumped on every change, so results can be refreshed.
    pub generation: u64,
    epoch: u64,
}

impl Index {
    pub fn items(&self) -> u64 {
        self.vols.iter().map(|v| v.items).sum()
    }

    pub fn vol(&self, letter: char) -> Option<&Volume> {
        self.vols.iter().find(|v| v.letter == letter)
    }

    pub(crate) fn vol_mut(&mut self, letter: char) -> Option<&mut Volume> {
        self.vols.iter_mut().find(|v| v.letter == letter)
    }

    /// Puts a volume in (replacing any with the same letter) under a fresh epoch,
    /// which tells threads following the old one to stop. Returns the epoch.
    pub(crate) fn install(&mut self, mut v: Volume) -> u64 {
        self.epoch += 1;
        v.epoch = self.epoch;
        self.put(v);
        self.epoch
    }

    pub fn put(&mut self, v: Volume) {
        match self.vols.iter().position(|x| x.letter == v.letter) {
            Some(i) => self.vols[i] = v,
            None => {
                self.vols.push(v);
                self.vols.sort_by_key(|v| v.letter);
            }
        }
        self.generation += 1;
    }

    fn remove(&mut self, letter: char) {
        self.vols.retain(|v| v.letter != letter);
        self.generation += 1;
    }
}

/// Progress of one drive being indexed.
pub struct Job {
    pub letter: char,
    pub mft: bool,
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub items: AtomicU64,
    /// Stop early (a network drive was switched off).
    pub cancel: AtomicBool,
}

impl Job {
    pub fn new(letter: char, mft: bool) -> Job {
        Job {
            letter,
            mft,
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            items: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
        }
    }
}

/// State shared between the window, the search worker and the indexing threads.
pub struct Shared {
    index: RwLock<Index>,
    jobs: Mutex<Vec<Arc<Job>>>,
    notes: Mutex<Vec<(char, String)>>,
    notify: Box<dyn Fn() + Send + Sync>,
    /// The window is in front. When it isn't, background work slows down and the
    /// window isn't redrawn.
    foreground: AtomicBool,
    net: Mutex<Network>,
    /// Which thread follows each walked drive's changes: a newer one takes over.
    followers: Mutex<HashMap<char, u64>>,
}

/// Network drives: indexed only when switched on, since walking a big work share
/// takes a while and puts load on the server.
#[derive(Default)]
struct Network {
    /// Switched on (saved between runs).
    on: HashSet<char>,
    /// Mapped right now.
    drives: Vec<NetDrive>,
}

#[derive(Clone)]
pub struct NetDrive {
    pub letter: char,
    /// `\\server\share`
    pub remote: String,
    pub online: bool,
    pub on: bool,
}

fn network_file() -> Option<std::path::PathBuf> {
    Some(edition::data_dir()?.join("network.txt"))
}

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    pub fn new(notify: Box<dyn Fn() + Send + Sync>) -> Arc<Shared> {
        // Network drives switched on last time: one letter per line.
        let on = network_file()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .map(|t| t.lines().filter_map(|l| l.trim().chars().next()).map(|c| c.to_ascii_uppercase()).collect())
            .unwrap_or_default();
        Arc::new(Shared {
            index: RwLock::new(Index::default()),
            jobs: Mutex::new(Vec::new()),
            notes: Mutex::new(Vec::new()),
            notify,
            foreground: AtomicBool::new(true),
            net: Mutex::new(Network { on, drives: Vec::new() }),
            followers: Mutex::new(HashMap::new()),
        })
    }

    /// Mapped network drives, and whether each is switched on.
    pub fn network_drives(&self) -> Vec<NetDrive> {
        lock(&self.net).drives.clone()
    }

    pub fn is_network(&self, letter: char) -> bool {
        lock(&self.net).drives.iter().any(|d| d.letter == letter)
    }

    /// Whether a drive should be in the index: local drives always, network
    /// drives only when switched on.
    pub fn wanted(&self, letter: char) -> bool {
        let net = lock(&self.net);
        !net.drives.iter().any(|d| d.letter == letter) || net.on.contains(&letter)
    }

    /// Switches indexing of a network drive on or off. Off also forgets it: its
    /// results go and its saved index is deleted.
    pub fn set_network(self: &Arc<Self>, letter: char, on: bool) {
        let online = {
            let mut net = lock(&self.net);
            if on {
                net.on.insert(letter);
            } else {
                net.on.remove(&letter);
            }
            for d in net.drives.iter_mut().filter(|d| d.letter == letter) {
                d.on = on;
            }
            if let Some(f) = network_file() {
                let text: String = net.on.iter().map(|c| format!("{c}\n")).collect();
                let _ = f.parent().map(std::fs::create_dir_all);
                let _ = std::fs::write(f, text);
            }
            net.drives.iter().any(|d| d.letter == letter && d.online)
        };
        if on {
            if online {
                index_drive(self, letter, false);
            } else if let Some(mut v) = Volume::load_cache(letter) {
                v.offline = true;
                self.write().install(v);
            }
        } else {
            for j in lock(&self.jobs).iter().filter(|j| j.letter == letter) {
                j.cancel.store(true, Relaxed);
            }
            self.write().remove(letter);
            if let Some(f) = cache_file(letter) {
                let _ = std::fs::remove_file(f);
            }
            lock(&self.notes).retain(|(l, _)| *l != letter);
        }
        self.notify();
    }

    /// Makes the calling thread the one following `letter`; returns its token.
    pub(crate) fn claim_follow(&self, letter: char) -> u64 {
        let mut f = lock(&self.followers);
        let token = f.get(&letter).copied().unwrap_or(0) + 1;
        f.insert(letter, token);
        token
    }

    /// Whether this thread is still the one following `letter`.
    pub(crate) fn holds_follow(&self, letter: char, token: u64) -> bool {
        lock(&self.followers).get(&letter) == Some(&token)
    }

    pub fn foreground(&self) -> bool {
        self.foreground.load(Relaxed)
    }

    pub fn set_foreground(&self, on: bool) {
        self.foreground.store(on, Relaxed);
    }

    pub fn read(&self) -> RwLockReadGuard<'_, Index> {
        let t = Instant::now();
        let g = self.index.read().unwrap_or_else(|e| e.into_inner());
        if crate::trace::on() && t.elapsed() > Duration::from_millis(15) {
            crate::trace::log(format!("waited {:.1} ms to read the index", t.elapsed().as_secs_f64() * 1000.0));
        }
        g
    }

    pub(crate) fn write(&self) -> RwLockWriteGuard<'_, Index> {
        let t = Instant::now();
        let g = self.index.write().unwrap_or_else(|e| e.into_inner());
        if crate::trace::on() && t.elapsed() > Duration::from_millis(15) {
            crate::trace::log(format!("waited {:.1} ms to write the index", t.elapsed().as_secs_f64() * 1000.0));
        }
        g
    }

    pub fn jobs(&self) -> Vec<Arc<Job>> {
        lock(&self.jobs).clone()
    }

    pub(crate) fn add_job(&self, job: &Arc<Job>) {
        lock(&self.jobs).push(job.clone());
        self.notify();
    }

    pub(crate) fn end_job(&self, job: &Arc<Job>) {
        lock(&self.jobs).retain(|j| !Arc::ptr_eq(j, job));
        self.notify();
    }

    pub(crate) fn note(&self, letter: char, text: String) {
        lock(&self.notes).push((letter, text));
    }

    /// Saves every walked index that changed, so the next start is instant.
    pub fn save_walked(&self) {
        for v in self.read().vols.iter().filter(|v| v.method == Method::Walk && v.dirty) {
            let _ = v.save_cache();
        }
    }

    /// Things worth telling the user, like why a drive couldn't use the fast path.
    pub fn notes(&self) -> Vec<String> {
        lock(&self.notes).iter().map(|(_, n)| n.clone()).collect()
    }

    /// Asks the window to redraw, unless it's in the background: it catches up
    /// when it comes back to the front.
    pub fn notify(&self) {
        if self.foreground() {
            (self.notify)()
        } else {
            crate::trace::log("skipped a redraw: window not in front");
        }
    }
}

pub fn filetime_to_unix(ft: u64) -> u32 {
    (ft / 10_000_000).saturating_sub(11_644_473_600).min(u32::MAX as u64) as u32
}

pub fn unix_time(t: std::io::Result<SystemTime>) -> u32 {
    t.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs().min(u32::MAX as u64) as u32)
}

pub fn now() -> u32 {
    unix_time(Ok(SystemTime::now()))
}

/// Indexes one drive: the MFT when allowed (and possible), otherwise a folder walk.
pub fn build(letter: char, mft: bool, job: &Job) -> Result<(Volume, Option<usn::Cursor>, Option<String>), String> {
    let t0 = Instant::now();
    let mut note = None;
    if mft {
        // Note the journal position first, so nothing that changes during the scan is missed.
        let cursor = usn::Cursor::open(letter);
        match ntfs::scan(letter, job) {
            Ok(raws) => {
                let mut v = Volume::build(letter, Method::Mft, raws, ntfs::ROOT);
                v.elapsed = t0.elapsed();
                let cursor = match cursor {
                    Ok(c) => Some(c),
                    Err(e) => {
                        note = Some(format!("{letter}: live updates are off ({e})"));
                        None
                    }
                };
                v.live = cursor.is_some();
                return Ok((v, cursor, note));
            }
            Err(e) => note = Some(format!("{letter}: {e}, so it was indexed by walking its folders")),
        }
    }
    let raws = walk::scan(letter, job)?;
    let mut v = Volume::build(letter, Method::Walk, raws, 0);
    v.elapsed = t0.elapsed();
    Ok((v, None, note))
}

/// Indexes a drive in the background and keeps it current afterwards: through the
/// NTFS change journal when reading the file table, or folder-change
/// notifications when walking (see `watch`).
pub fn index_drive(shared: &Arc<Shared>, letter: char, mft: bool) {
    let job = Arc::new(Job::new(letter, mft));
    lock(&shared.notes).retain(|(l, _)| *l != letter);
    shared.add_job(&job);
    let shared = shared.clone();
    std::thread::spawn(move || {
        win::background_thread();
        if !mft {
            crate::watch::index_by_walk(shared, letter, job);
            return;
        }
        crate::trace::log(format!("{letter}: reading the file table"));
        match build(letter, mft, &job) {
            Ok((mut vol, cursor, note)) => {
                crate::trace::log(format!("{letter}: file table read ({} items)", vol.items));
                if let Some(n) = note {
                    shared.note(letter, n);
                }
                let walked = vol.method == Method::Walk;
                vol.dirty = walked;
                let epoch = shared.write().install(vol);
                shared.end_job(&job);
                if let Some(c) = cursor {
                    follow(shared, letter, epoch, c);
                } else if walked {
                    // The file table couldn't be read after all: follow folder changes instead.
                    shared.save_walked();
                    crate::watch::follow_only(shared, letter);
                }
            }
            Err(e) => {
                shared.note(letter, format!("{letter}: couldn't be indexed ({e})"));
                shared.end_job(&job);
            }
        }
    });
}

/// Indexes every drive now, then keeps watching: drives that get plugged in are
/// indexed right away, and ones that go away drop out of the results.
pub fn watch_drives(shared: Arc<Shared>, elevated: bool) {
    std::thread::spawn(move || {
        win::low_priority_thread();
        // Each drive's letter, and whether it was reachable last time we looked.
        let mut known: HashMap<char, bool> = HashMap::new();
        loop {
            let drives = win::drives();
            {
                let mut net = lock(&shared.net);
                let on = net.on.clone();
                net.drives = drives
                    .iter()
                    .filter(|d| d.network)
                    .map(|d| NetDrive { letter: d.letter, remote: d.remote.clone(), online: d.online, on: on.contains(&d.letter) })
                    .collect();
            }
            for d in &drives {
                let before = known.get(&d.letter).copied();
                if before == Some(d.online) {
                    continue;
                }
                if !d.network {
                    index_drive(&shared, d.letter, elevated && d.fs.eq_ignore_ascii_case("NTFS"));
                } else if shared.wanted(d.letter) {
                    if d.online {
                        // Connected (or reconnected): catch up by walking it again.
                        index_drive(&shared, d.letter, false);
                    } else {
                        go_offline(&shared, d.letter);
                    }
                }
            }
            let letters: HashSet<char> = drives.iter().map(|d| d.letter).collect();
            for gone in known.keys().filter(|l| !letters.contains(l)) {
                if lock(&shared.net).on.contains(gone) {
                    go_offline(&shared, *gone); // unmapped for now: keep what we had
                } else {
                    shared.write().remove(*gone);
                    lock(&shared.notes).retain(|(l, _)| l != gone);
                }
                shared.notify();
            }
            known = drives.iter().map(|d| (d.letter, d.online)).collect();
            std::thread::sleep(if shared.foreground() { DRIVE_POLL } else { DRIVE_POLL_AWAY });
        }
    });
}

/// A network drive that's out of reach keeps its results, from the saved index if
/// it isn't loaded already.
fn go_offline(shared: &Shared, letter: char) {
    let mut ix = shared.write();
    match ix.vol_mut(letter) {
        Some(v) => v.offline = true,
        None => {
            if let Some(mut v) = Volume::load_cache(letter) {
                v.offline = true;
                ix.install(v);
            }
        }
    }
    ix.generation += 1;
}

/// Applies the change journal to a volume as changes happen.
fn follow(shared: Arc<Shared>, letter: char, epoch: u64, mut cursor: usn::Cursor) {
    win::low_priority_thread();
    loop {
        std::thread::sleep(if shared.foreground() { JOURNAL_POLL } else { JOURNAL_POLL_AWAY });
        let changes = match cursor.read() {
            Ok(c) => c,
            Err(_) => {
                // The journal was reset or wrapped past us: start over with a fresh scan.
                let current = shared.read().vol(letter).is_some_and(|v| v.epoch == epoch);
                if current {
                    index_drive(&shared, letter, true);
                }
                return;
            }
        };
        if changes.is_empty() {
            if shared.read().vol(letter).is_none_or(|v| v.epoch != epoch) {
                return; // replaced by a rescan, or the drive went away
            }
            continue;
        }
        let mut stat = Vec::new();
        {
            let mut ix = shared.write();
            let Some(v) = ix.vol_mut(letter).filter(|v| v.epoch == epoch) else { return };
            let mut relink = false;
            let (mut touched, mut orphans) = (Vec::new(), Vec::new());
            for c in &changes {
                match v.apply(c) {
                    Applied::Done => {}
                    Applied::Stat => touched.push(c.index),
                    Applied::Orphan => orphans.push(c.index),
                    Applied::Relink => relink = true,
                }
            }
            // A full pass only when a folder moved; everything else is patched in place.
            if relink {
                v.relink();
            } else if !orphans.is_empty() {
                orphans.sort_unstable();
                orphans.dedup();
                touched.extend(v.adopt(&orphans));
            }
            touched.sort_unstable();
            touched.dedup();
            for i in touched {
                let e = v.entries[i as usize];
                if e.live() && e.kind == FILE {
                    stat.push((i, v.path(i)));
                }
            }
            ix.generation += 1;
        }
        shared.notify();
        // The journal doesn't carry sizes, so ask the file system (outside the lock).
        let stats: Vec<(u32, u64, u32)> = stat
            .into_iter()
            .filter_map(|(i, p)| {
                let m = std::fs::symlink_metadata(&p).ok()?;
                Some((i, m.len(), unix_time(m.modified())))
            })
            .collect();
        if !stats.is_empty() {
            let mut ix = shared.write();
            if let Some(v) = ix.vol_mut(letter).filter(|v| v.epoch == epoch) {
                for (i, size, mtime) in stats {
                    v.set_size(i, size, mtime);
                }
            }
            ix.generation += 1;
            drop(ix);
            shared.notify();
        }
    }
}
