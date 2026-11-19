//! Query parsing and matching. A search sweeps every name in the index in parallel,
//! so results come back in milliseconds even across millions of files.
//!
//! Syntax: words must all appear in the name; `*` and `?` match the whole name;
//! `ext:jpg;png`, `size:>1gb`, `file:` / `folder:`; a word with `\` matches the full
//! path; `!word` leaves matches out; "quotes" keep spaces together.

use crate::history::History;
use crate::index::{lock, now, Entry, Index, Shared, Volume, DIR, FILE, LINK, NONE};
use memchr::memmem::{self, Finder};
use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cat {
    All,
    Folder,
    Document,
    Picture,
    Video,
    Audio,
    Archive,
    App,
}

impl Cat {
    pub const ALL: [Cat; 8] =
        [Cat::All, Cat::Folder, Cat::Document, Cat::Picture, Cat::Video, Cat::Audio, Cat::Archive, Cat::App];

    pub fn label(self) -> &'static str {
        match self {
            Cat::All => "All",
            Cat::Folder => "Folders",
            Cat::Document => "Documents",
            Cat::Picture => "Pictures",
            Cat::Video => "Videos",
            Cat::Audio => "Music",
            Cat::Archive => "Archives",
            Cat::App => "Apps",
        }
    }

    /// Position in `ALL`, and in `Counts`.
    pub fn index(self) -> usize {
        self as usize
    }
}

/// Matches per category, indexed like `Cat::ALL`; `[0]` is every match.
pub type Counts = [u64; 8];

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum When {
    #[default]
    Any,
    Day,
    Week,
    Month,
    Year,
    Older,
}

impl When {
    pub const ALL: [When; 6] = [When::Any, When::Day, When::Week, When::Month, When::Year, When::Older];

    pub fn label(self) -> &'static str {
        match self {
            When::Any => "Any time",
            When::Day => "Past 24 hours",
            When::Week => "Past week",
            When::Month => "Past month",
            When::Year => "Past year",
            When::Older => "Over a year ago",
        }
    }

    /// Modified-time range, Unix seconds.
    fn range(self, now: u32) -> (u32, u32) {
        let ago = |days: u32| now.saturating_sub(days * 86_400);
        match self {
            When::Any => (0, u32::MAX),
            When::Day => (ago(1), u32::MAX),
            When::Week => (ago(7), u32::MAX),
            When::Month => (ago(30), u32::MAX),
            When::Year => (ago(365), u32::MAX),
            When::Older => (0, ago(365)),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SizeF {
    #[default]
    Any,
    Small,
    Medium,
    Large,
    Huge,
}

impl SizeF {
    pub const ALL: [SizeF; 5] = [SizeF::Any, SizeF::Small, SizeF::Medium, SizeF::Large, SizeF::Huge];

    pub fn label(self) -> &'static str {
        match self {
            SizeF::Any => "Any size",
            SizeF::Small => "Under 1 MB",
            SizeF::Medium => "1 – 100 MB",
            SizeF::Large => "100 MB – 1 GB",
            SizeF::Huge => "Over 1 GB",
        }
    }

    fn range(self) -> (u64, u64) {
        const MB: u64 = 1 << 20;
        match self {
            SizeF::Any => (0, u64::MAX),
            SizeF::Small => (0, MB - 1),
            SizeF::Medium => (MB, 100 * MB - 1),
            SizeF::Large => (100 * MB, 1024 * MB - 1),
            SizeF::Huge => (1024 * MB, u64::MAX),
        }
    }
}

/// Filters picked in the filter panel, on top of whatever the query says.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Filters {
    pub when: When,
    pub size: SizeF,
    pub drive: Option<char>,
    pub folder: Option<String>,
    pub hide_system: bool,
}

impl Filters {
    /// How many filters are switched on.
    pub fn count(&self) -> usize {
        (self.when != When::Any) as usize
            + (self.size != SizeF::Any) as usize
            + self.drive.is_some() as usize
            + self.folder.is_some() as usize
            + self.hide_system as usize
    }

    /// Whether these filters alone pick out a set of results worth listing.
    pub fn narrows(&self) -> bool {
        self.when != When::Any || self.size != SizeF::Any || self.drive.is_some() || self.folder.is_some()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sort {
    Best,
    Name,
    Folder,
    Size,
    Modified,
}

impl Sort {
    pub const ALL: [Sort; 5] = [Sort::Best, Sort::Name, Sort::Folder, Sort::Size, Sort::Modified];

    pub fn title(self) -> &'static str {
        match self {
            Sort::Best => "Best match",
            Sort::Name => "Name",
            Sort::Folder => "Folder",
            Sort::Size => "Size",
            Sort::Modified => "Date modified",
        }
    }
}

/// One search result: an entry on one drive.
#[derive(Clone, Copy, Debug)]
pub struct Hit {
    pub idx: u32,
    /// Drive letter.
    pub vol: u8,
    /// Lower is better.
    pub rank: u8,
}

impl Hit {
    pub fn same(self, o: Hit) -> bool {
        self.idx == o.idx && self.vol == o.vol
    }
}

pub fn extension(l: &[u8]) -> Option<&[u8]> {
    memchr::memrchr(b'.', l).filter(|&p| p > 0).map(|p| &l[p + 1..])
}

/// The category of a file, from its lowercase name.
pub fn file_cat(l: &[u8]) -> Option<Cat> {
    Some(match extension(l)? {
        b"pdf" | b"doc" | b"docx" | b"txt" | b"md" | b"rtf" | b"odt" | b"xls" | b"xlsx" | b"csv" | b"ppt"
        | b"pptx" | b"epub" | b"ods" | b"odp" | b"pages" | b"numbers" | b"key" => Cat::Document,
        b"jpg" | b"jpeg" | b"png" | b"gif" | b"bmp" | b"webp" | b"heic" | b"heif" | b"tif" | b"tiff" | b"svg"
        | b"ico" | b"raw" | b"cr2" | b"cr3" | b"nef" | b"arw" | b"dng" | b"psd" | b"avif" => Cat::Picture,
        b"mp4" | b"mkv" | b"avi" | b"mov" | b"wmv" | b"webm" | b"m4v" | b"flv" | b"mpg" | b"mpeg" | b"3gp"
        | b"mts" | b"m2ts" => Cat::Video,
        b"mp3" | b"wav" | b"flac" | b"aac" | b"ogg" | b"m4a" | b"wma" | b"opus" | b"aiff" | b"mid" | b"midi" => {
            Cat::Audio
        }
        b"zip" | b"rar" | b"7z" | b"tar" | b"gz" | b"bz2" | b"xz" | b"tgz" | b"iso" | b"cab" | b"zst" => {
            Cat::Archive
        }
        b"exe" | b"msi" | b"lnk" | b"bat" | b"cmd" | b"ps1" | b"appx" | b"msix" | b"appinstaller" => Cat::App,
        _ => return None,
    })
}

/// Folders count as their own category.
pub fn entry_cat(v: &Volume, i: u32, e: &Entry) -> Option<Cat> {
    if e.kind == DIR {
        Some(Cat::Folder)
    } else {
        file_cat(v.lower(i))
    }
}

#[derive(Clone)]
enum Pat {
    Sub(Finder<'static>),
    Glob(Vec<u8>),
    Path(Finder<'static>),
}

#[derive(Clone)]
struct Term {
    pat: Pat,
    neg: bool,
}

#[derive(Clone)]
pub struct Query {
    terms: Vec<Term>,
    exts: Vec<Vec<u8>>,
    kind: Option<u8>,
    min: u64,
    max: u64,
    after: u32,
    before: u32,
    cat: Cat,
    drive: Option<u8>,
    /// Only inside this folder (drive letter, entry).
    within: Option<(u8, u32)>,
    /// A folder filter that names a folder that isn't there.
    missing_folder: bool,
    hide_noisy: bool,
    /// The first plain word, used to rank results.
    first: Option<Vec<u8>>,
    /// A category asked for in words ("photos"), on top of the chips.
    nl_cat: Option<Cat>,
    /// How plain-language words were read, to show back ("Pictures", "Past week").
    pub understood: Vec<String>,
    /// The plain words run together ("android projects" → "androidprojects"),
    /// for names that write them differently.
    squashed: Vec<u8>,
    /// Second pass: forgive typos.
    fuzzy: bool,
    now: u32,
}

fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for ch in text.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let n: f64 = s[..split].parse().ok()?;
    let mult: u64 = match s[split..].trim() {
        "" | "b" => 1,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        "t" | "tb" => 1 << 40,
        _ => return None,
    };
    Some((n * mult as f64) as u64)
}

/// `>1gb`, `>=10mb`, `<500kb`, `1mb..10mb`. A bare size means "at least".
fn size_range(s: &str) -> Option<(u64, u64)> {
    if let Some((a, b)) = s.split_once("..") {
        let lo = if a.is_empty() { 0 } else { bytes(a)? };
        let hi = if b.is_empty() { u64::MAX } else { bytes(b)? };
        return Some((lo, hi));
    }
    if let Some(r) = s.strip_prefix(">=") {
        return Some((bytes(r)?, u64::MAX));
    }
    if let Some(r) = s.strip_prefix('>') {
        return Some((bytes(r)?.saturating_add(1), u64::MAX));
    }
    if let Some(r) = s.strip_prefix("<=") {
        return Some((0, bytes(r)?));
    }
    if let Some(r) = s.strip_prefix('<') {
        return Some((0, bytes(r)?.saturating_sub(1)));
    }
    Some((bytes(s.strip_prefix('=').unwrap_or(s))?, u64::MAX))
}

/// Plain-language words: "photos from last week", "big videos", "pdf invoices
/// this month". Only in searches of two or more words, so a lone "photos" still
/// finds folders called Photos.
fn natural(q: &mut Query, toks: &mut Vec<String>) {
    let ago = |days: u32| q.now.saturating_sub(days * 86_400);
    let mut used = vec![false; toks.len()];
    for i in 0..toks.len() {
        let prev = if i > 0 { toks[i - 1].as_str() } else { "" };
        let (after, cat, ext, size, label): (u32, Option<Cat>, Option<&[u8]>, (u64, u64), &str) = match toks[i].as_str() {
            "today" => (ago(1), None, None, (0, u64::MAX), "Past 24 hours"),
            "yesterday" => (ago(2), None, None, (0, u64::MAX), "Since yesterday"),
            "week" if matches!(prev, "last" | "this" | "past") => (ago(7), None, None, (0, u64::MAX), "Past week"),
            "month" if matches!(prev, "last" | "this" | "past") => (ago(30), None, None, (0, u64::MAX), "Past month"),
            "year" if matches!(prev, "last" | "this" | "past") => (ago(365), None, None, (0, u64::MAX), "Past year"),
            "photos" | "photo" | "pictures" | "pics" | "images" | "screenshots" => (0, Some(Cat::Picture), None, (0, u64::MAX), "Pictures"),
            "videos" | "video" | "movies" | "clips" => (0, Some(Cat::Video), None, (0, u64::MAX), "Videos"),
            "music" | "songs" | "audio" => (0, Some(Cat::Audio), None, (0, u64::MAX), "Music"),
            "documents" | "docs" => (0, Some(Cat::Document), None, (0, u64::MAX), "Documents"),
            "archives" | "zips" => (0, Some(Cat::Archive), None, (0, u64::MAX), "Archives"),
            "folders" => (0, Some(Cat::Folder), None, (0, u64::MAX), "Folders"),
            "pdf" | "pdfs" => (0, None, Some(b"pdf".as_slice()), (0, u64::MAX), "PDF files"),
            "big" | "large" | "huge" => (0, None, None, (100 << 20, u64::MAX), "Over 100 MB"),
            "small" | "tiny" => (0, None, None, (0, (1 << 20) - 1), "Under 1 MB"),
            _ => continue,
        };
        used[i] = true;
        if after > 0 && i > 0 && matches!(prev, "last" | "this" | "past") {
            used[i - 1] = true;
        }
        q.after = q.after.max(after);
        q.nl_cat = cat.or(q.nl_cat);
        q.exts.extend(ext.map(|e| e.to_vec()));
        q.min = q.min.max(size.0);
        q.max = q.max.min(size.1);
        q.understood.push(label.to_string());
    }
    if q.understood.is_empty() {
        return;
    }
    // Glue words only mean something next to the words above.
    for (i, t) in toks.iter().enumerate() {
        if matches!(t.as_str(), "from" | "in" | "of" | "the" | "my" | "all" | "modified" | "changed" | "edited" | "with" | "and") {
            used[i] = true;
        }
    }
    let mut k = 0;
    toks.retain(|_| {
        k += 1;
        !used[k - 1]
    });
}

pub fn parse(text: &str, cat: Cat, f: &Filters) -> Query {
    let (min, max) = f.size.range();
    let now = now();
    let (after, before) = f.when.range(now);
    let mut q = Query {
        terms: Vec::new(),
        exts: Vec::new(),
        kind: None,
        min,
        max,
        after,
        before,
        cat,
        drive: f.drive.map(|d| d as u8),
        within: None,
        missing_folder: false,
        hide_noisy: f.hide_system,
        first: None,
        nl_cat: None,
        understood: Vec::new(),
        squashed: Vec::new(),
        fuzzy: false,
        now,
    };
    let mut toks = tokens(&text.to_lowercase());
    if toks.len() >= 2 {
        natural(&mut q, &mut toks);
    }
    for tok in toks {
        let (neg, t) = match tok.strip_prefix('!') {
            Some(r) if !r.is_empty() => (true, r.to_string()),
            _ => (false, tok),
        };
        if let Some(r) = t.strip_prefix("ext:") {
            let exts = r.split([';', ',']).map(|e| e.trim_start_matches('.')).filter(|e| !e.is_empty());
            q.exts.extend(exts.map(|e| e.as_bytes().to_vec()));
            continue;
        }
        if let Some(r) = t.strip_prefix("size:") {
            if let Some((lo, hi)) = size_range(r) {
                q.min = q.min.max(lo);
                q.max = q.max.min(hi);
            }
            continue;
        }
        let t = if let Some(r) = t.strip_prefix("folder:") {
            q.kind = Some(DIR);
            r.to_string()
        } else if let Some(r) = t.strip_prefix("file:") {
            q.kind = Some(FILE);
            r.to_string()
        } else {
            t
        };
        if t.is_empty() {
            continue;
        }
        let pat = if t.contains(['\\', '/']) {
            Pat::Path(Finder::new(t.replace('/', "\\").as_bytes()).into_owned())
        } else if t.contains(['*', '?']) {
            Pat::Glob(t.into_bytes())
        } else {
            if !neg {
                if q.first.is_none() {
                    q.first = Some(t.as_bytes().to_vec());
                }
                q.squashed.extend(t.bytes().filter(u8::is_ascii_alphanumeric));
            }
            Pat::Sub(Finder::new(t.as_bytes()).into_owned())
        };
        q.terms.push(Term { pat, neg });
    }
    // Path terms build the full path, so check them last.
    q.terms.sort_by_key(|t| matches!(t.pat, Pat::Path(_)));
    q
}

/// The word to highlight in result names.
pub fn highlight(text: &str) -> Option<String> {
    let q = parse(text, Cat::All, &Filters::default());
    q.first.map(|w| String::from_utf8_lossy(&w).into_owned())
}

/// Whether `pat` appears in `text` with at most `k` typos (a wrong, missing,
/// extra or swapped letter each). Sellers' algorithm with transpositions.
fn approx_contains(text: &[u8], pat: &[u8], k: usize) -> bool {
    const MAX: usize = 32;
    let m = pat.len();
    if m == 0 || m > MAX || text.len() + k < m {
        return false;
    }
    // Cheap check first: typos keep most of the word's letters.
    let present = pat.iter().filter(|c| memchr::memchr(**c, text).is_some()).count();
    if present + k < m {
        return false;
    }
    let (mut prev2, mut prev, mut cur) = ([0usize; MAX + 1], [0usize; MAX + 1], [0usize; MAX + 1]);
    for (j, p) in prev.iter_mut().enumerate().take(m + 1) {
        *p = j;
    }
    for i in 1..=text.len() {
        cur[0] = 0;
        for j in 1..=m {
            let cost = (text[i - 1] != pat[j - 1]) as usize;
            let mut v = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && text[i - 1] == pat[j - 2] && text[i - 2] == pat[j - 1] {
                v = v.min(prev2[j - 2] + 1);
            }
            cur[j] = v;
        }
        if cur[m] <= k {
            return true;
        }
        prev2 = prev;
        prev = cur;
    }
    false
}

/// Whether the name contains `sq` once spaces and punctuation are dropped.
fn squashed_contains(l: &[u8], sq: &[u8]) -> bool {
    let mut buf = [0u8; 512];
    let mut n = 0;
    for &c in l.iter().filter(|c| c.is_ascii_alphanumeric()) {
        if n == buf.len() {
            break;
        }
        buf[n] = c;
        n += 1;
    }
    memmem::find(&buf[..n], sq).is_some()
}

/// Typos allowed for a word of this length.
fn typos(word: &[u8]) -> usize {
    match word.len() {
        0..=3 => 0,
        4..=7 => 1,
        _ => 2,
    }
}

/// `*` and `?` wildcards against the whole name.
fn glob(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            pi += 1;
            mark = si;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// How well a name matches the first word, best first:
/// 0 the name is the word (with or without extension),
/// 1 starts with it as a whole word ("Android Projects"),
/// 2 starts with it ("androidx"),
/// 3 has it as a whole word later on ("My Android"),
/// 4 a later word starts with it, 5 anywhere else.
fn rank(l: &[u8], first: Option<&[u8]>) -> u8 {
    let Some(t) = first else { return 5 };
    let stem = memchr::memrchr(b'.', l).filter(|&p| p > 0).map_or(l, |p| &l[..p]);
    if l == t || stem == t {
        return 0;
    }
    let word_end = |p: usize| l.get(p + t.len()).is_none_or(|c| !c.is_ascii_alphanumeric());
    if l.starts_with(t) {
        return if word_end(0) { 1 } else { 2 };
    }
    let mut best = 5;
    for p in memmem::find_iter(l, t) {
        if !l[p - 1].is_ascii_alphanumeric() {
            best = best.min(if word_end(p) { 3 } else { 4 });
        }
    }
    best
}

/// A result's score, lower is better: how well the name matches, then whose
/// files these are, then how deep they're buried, with a nudge for anything
/// changed in the last few days.
fn score(v: &Volume, i: u32, e: &Entry, rank: u8, has_word: bool, now: u32) -> u8 {
    let place = if e.noisy() {
        40
    } else if e.user() {
        0
    } else {
        20
    };
    if !has_word {
        return place;
    }
    let fresh = match now.checked_sub(e.mtime) {
        Some(0..86_400) => 6,
        Some(86_400..604_800) => 3,
        _ => 0,
    };
    (rank * 4 + place + (v.depth(i).min(12) as u8) * 3).saturating_sub(fresh)
}

fn search_has_word(q: &Query) -> bool {
    q.first.is_some()
}

/// Whether an entry sits somewhere below a folder.
fn inside(v: &Volume, e: &Entry, dir: u32) -> bool {
    let mut j = e.parent;
    for _ in 0..1024 {
        if j == dir {
            return true;
        }
        if j == v.root {
            return false;
        }
        let p = v.entry(j);
        if p.kind == NONE {
            return false;
        }
        j = p.parent;
    }
    false
}

impl Query {
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
            && self.exts.is_empty()
            && self.kind.is_none()
            && self.min == 0
            && self.max == u64::MAX
            && self.after == 0
            && self.before == u32::MAX
            && self.cat == Cat::All
            && self.drive.is_none()
            && self.within.is_none()
            && !self.missing_folder
    }

    /// Everything except the category: (rank, category) if the entry matches.
    fn test(&self, v: &Volume, i: u32, e: &Entry) -> Option<(u8, Option<Cat>)> {
        if (self.hide_noisy && e.noisy()) || self.kind.is_some_and(|k| e.kind != k) {
            return None;
        }
        let l = v.lower(i);
        if !self.exts.is_empty() {
            let ext = extension(l).filter(|_| e.kind == FILE)?;
            if !self.exts.iter().any(|x| x == ext) {
                return None;
            }
        }
        if (self.min > 0 || self.max < u64::MAX) && (e.kind != FILE || e.size < self.min || e.size > self.max) {
            return None;
        }
        if (self.after > 0 || self.before < u32::MAX) && (e.mtime < self.after || e.mtime > self.before) {
            return None;
        }
        let mut path: Option<String> = None;
        let exact = |f: &Finder| f.find(l).is_some();
        // In the typo pass, a name that spells the words run together also counts.
        let squashed = self.fuzzy && self.squashed.len() >= 4 && squashed_contains(l, &self.squashed);
        let mut typo = false;
        for t in &self.terms {
            let hit = match &t.pat {
                Pat::Sub(f) if self.fuzzy && !t.neg && !squashed => {
                    exact(f) || {
                        typo = true;
                        approx_contains(l, f.needle(), typos(f.needle()))
                    }
                }
                Pat::Sub(_) if squashed && !t.neg => true,
                Pat::Sub(f) => exact(f),
                Pat::Glob(g) => glob(g, l),
                Pat::Path(f) => {
                    let p = path.get_or_insert_with(|| v.path(i).to_lowercase());
                    f.find(p.as_bytes()).is_some()
                }
            };
            if hit == t.neg {
                return None;
            }
        }
        if let Some((_, dir)) = self.within {
            if !inside(v, e, dir) {
                return None;
            }
        }
        let cat = if e.kind == DIR { Some(Cat::Folder) } else { file_cat(l) };
        if self.nl_cat.is_some_and(|c| cat != Some(c)) {
            return None;
        }
        let first = self.first.as_deref();
        let r = if typo || squashed { 6 } else { rank(l, first) };
        Some((score(v, i, e, r, first.is_some(), self.now), cat))
    }

    /// A second, typo-forgiving pass is worth it: only plain words, long enough to
    /// guess at.
    fn can_fuzz(&self) -> bool {
        !self.terms.is_empty()
            && self.terms.iter().all(|t| matches!(t.pat, Pat::Sub(_)))
            && self.terms.iter().any(|t| !t.neg && matches!(&t.pat, Pat::Sub(f) if f.needle().len() >= 4))
    }
}

const CHUNK: usize = 1 << 14;

/// Runs a query over the whole index: the hits in the chosen category, how many
/// matches each category has, and whether typos had to be forgiven to find
/// anything. Returns None if `stop` said to give up.
pub fn run(
    ix: &Index,
    q: &Query,
    sort: Sort,
    desc: bool,
    stop: &(dyn Fn() -> bool + Sync),
) -> Option<(Vec<Hit>, Counts, bool)> {
    let (mut hits, mut counts) = sweep(ix, q, stop);
    let mut fuzzy = false;
    // Next to nothing found: try again, forgiving a typo or two.
    if counts[0] < 5 && q.can_fuzz() && !stop() {
        let fq = Query { fuzzy: true, ..q.clone() };
        let (h, c) = sweep(ix, &fq, stop);
        if c[0] > counts[0] {
            (hits, counts, fuzzy) = (h, c, true);
        }
    }
    if stop() {
        return None;
    }
    sort_hits(ix, &mut hits, sort, desc, q.first.is_none());
    Some((hits, counts, fuzzy))
}

fn sweep(ix: &Index, q: &Query, stop: &(dyn Fn() -> bool + Sync)) -> (Vec<Hit>, Counts) {
    let mut hits: Vec<Hit> = Vec::new();
    let mut counts: Counts = [0; 8];
    if q.missing_folder {
        return (hits, counts);
    }
    for v in &ix.vols {
        let letter = v.letter as u8;
        if q.drive.is_some_and(|d| d != letter) || q.within.is_some_and(|(d, _)| d != letter) {
            continue;
        }
        for (entries, tag) in [(&v.entries, 0), (&v.links, LINK)] {
            let parts: Vec<(Vec<Hit>, Counts)> = entries
                .par_chunks(CHUNK)
                .enumerate()
                .map(|(c, chunk)| {
                    let mut out = Vec::new();
                    let mut cnt: Counts = [0; 8];
                    if !stop() {
                        for (k, e) in chunk.iter().enumerate() {
                            if !e.live() {
                                continue;
                            }
                            let idx = (c * CHUNK + k) as u32 | tag;
                            let Some((rank, cat)) = q.test(v, idx, e) else { continue };
                            cnt[0] += 1;
                            if let Some(c) = cat {
                                cnt[c.index()] += 1;
                            }
                            if q.cat == Cat::All || cat == Some(q.cat) {
                                out.push(Hit { idx, vol: letter, rank });
                            }
                        }
                    }
                    (out, cnt)
                })
                .collect();
            for (h, cnt) in parts {
                hits.extend(h);
                for k in 0..8 {
                    counts[k] += cnt[k];
                }
            }
        }
    }
    (hits, counts)
}

/// Lifts what you've opened before (see `History`) within the top of the list.
fn boost_hits(ix: &Index, hits: &mut [Hit], history: &History, query: &str) {
    let now = now();
    let n = hits.len().min(GROUP_WITHIN);
    let mut changed = false;
    for h in &mut hits[..n] {
        let Some(v) = ix.vol(h.vol as char) else { continue };
        let b = history.boost(&v.path(h.idx).to_lowercase(), query, now);
        if b > 0 {
            h.rank = h.rank.saturating_sub(b);
            changed = true;
        }
    }
    if changed {
        hits[..n].sort_by_key(|h| h.rank); // stable: equal scores keep their order
    }
}

/// `newest_first`: there's no word to rank by, so recent files lead.
fn sort_hits(ix: &Index, hits: &mut [Hit], sort: Sort, desc: bool, newest_first: bool) {
    let mut slot = [0usize; 26];
    for (i, v) in ix.vols.iter().enumerate() {
        slot[(v.letter as u8 - b'A') as usize] = i;
    }
    let vol = |h: &Hit| &ix.vols[slot[(h.vol - b'A') as usize]];
    let name = |h: &Hit| vol(h).lower(h.idx);
    let entry = |h: &Hit| vol(h).entry(h.idx);
    match sort {
        Sort::Best if newest_first => hits.par_sort_unstable_by(|a, b| {
            a.rank.cmp(&b.rank).then(entry(b).mtime.cmp(&entry(a).mtime)).then_with(|| name(a).cmp(name(b)))
        }),
        Sort::Best => hits.par_sort_unstable_by(|a, b| {
            let (x, y) = (name(a), name(b));
            a.rank.cmp(&b.rank).then(x.len().cmp(&y.len())).then_with(|| x.cmp(y))
        }),
        Sort::Name => hits.par_sort_unstable_by(|a, b| name(a).cmp(name(b))),
        Sort::Size => hits.par_sort_unstable_by(|a, b| entry(a).size.cmp(&entry(b).size).then_with(|| name(a).cmp(name(b)))),
        Sort::Modified => {
            hits.par_sort_unstable_by(|a, b| entry(a).mtime.cmp(&entry(b).mtime).then_with(|| name(a).cmp(name(b))))
        }
        Sort::Folder => {
            let mut dirs: HashMap<(u8, u32), String> = HashMap::new();
            for h in hits.iter() {
                let p = entry(h).parent;
                dirs.entry((h.vol, p)).or_insert_with(|| vol(h).path(p).to_lowercase());
            }
            let dir = |h: &Hit| dirs[&(h.vol, entry(h).parent)].as_str();
            hits.par_sort_unstable_by(|a, b| dir(a).cmp(dir(b)).then_with(|| name(a).cmp(name(b))));
        }
    }
    if desc && sort != Sort::Best {
        hits.reverse();
    }
}

/// A run of results shown as one row: `len` hits starting at `start`. A group
/// (len > 1) is a name that repeats inside one app or system folder, like the
/// dozens of "Android" folders inside a game engine.
pub type Row = (u32, u32);

/// Only the top of the list is grouped; nobody scrolls past this far.
const GROUP_WITHIN: usize = 5000;

/// Gathers look-alike results outside your own files into groups. Reorders
/// `hits` so each group is contiguous, led by its best member.
pub fn group(ix: &Index, hits: &mut Vec<Hit>) -> Vec<Row> {
    let n = hits.len().min(GROUP_WITHIN);
    let mut groups: Vec<Vec<Hit>> = Vec::new();
    let mut seen: HashMap<(u8, u32, &[u8]), usize> = HashMap::new();
    for h in &hits[..n] {
        let Some(v) = ix.vol(h.vol as char) else { continue };
        if v.entry(h.idx).user() {
            groups.push(vec![*h]);
            continue;
        }
        // Same name, under the same folder three levels down (C:\Program Files\Epic Games).
        let key = (h.vol, v.ancestor(h.idx, 3), v.lower(h.idx));
        match seen.get(&key) {
            Some(&g) => groups[g].push(*h),
            None => {
                seen.insert(key, groups.len());
                groups.push(vec![*h]);
            }
        }
    }
    let mut out = Vec::with_capacity(hits.len());
    let mut rows = Vec::new();
    for g in groups {
        if g.len() >= 3 {
            rows.push((out.len() as u32, g.len() as u32));
            out.extend(g);
        } else {
            for h in g {
                rows.push((out.len() as u32, 1));
                out.push(h);
            }
        }
    }
    for &h in &hits[n..] {
        rows.push((out.len() as u32, 1));
        out.push(h);
    }
    *hits = out;
    rows
}

/// The overview on the home screen: what kinds of files there are, and what changed last.
#[derive(Clone, Default)]
pub struct Stats {
    pub counts: Counts,
    /// Most recently modified files, newest first: (drive, entry).
    pub recent: Vec<(char, u32)>,
    /// Your own files changed per week, this week first.
    pub weeks: [u32; WEEKS],
    /// Your own files changed per hour over the past day, the past hour last.
    pub hours: [u32; 24],
    /// The biggest of your files made or changed in the past week.
    pub biggest: Option<(char, u32)>,
    /// Folders with the most changes in the past month: (drive, folder, changes), busiest first.
    pub folders: Vec<(char, u32, u32)>,
}

const RECENT: usize = 12;
pub const WEEKS: usize = 26;
const FOLDERS: usize = 9;
const DAY: u32 = 86_400;

/// What one chunk of entries adds to `Stats`.
#[derive(Default)]
struct Part {
    counts: Counts,
    top: Vec<(u32, char, u32)>,
    weeks: [u32; WEEKS],
    hours: [u32; 24],
    /// (size, drive, entry)
    biggest: Option<(u64, char, u32)>,
    folders: HashMap<u32, u32>,
}

/// Whether a changed file is something a person made or saved (a document, photo,
/// download...) rather than a log, cache file or app internals.
fn worth_showing(v: &Volume, i: u32) -> bool {
    let l = v.lower(i);
    let Some(cat) = file_cat(l) else { return false };
    let stem = memchr::memrchr(b'.', l).map_or(l, |p| &l[..p]);
    let keep = match cat {
        Cat::App => matches!(extension(l), Some(b"exe" | b"msi")),
        Cat::Document => memmem::find(stem, b"log").is_none(),
        _ => true,
    };
    // Hash-like names (cache entries): long runs of hex digits.
    let hashy = stem.len() >= 16 && stem.iter().all(|c| c.is_ascii_hexdigit() || *c == b'-' || *c == b'_');
    keep && !hashy && !in_generated_folder(v, i)
}

/// Inside an app's own folders, or a build's output, rather than someone's files.
const BUILD_DIRS: &[&[u8]] = &[b"target", b"build", b"obj", b"bin", b"out", b"dist", b"deps", b"debug", b"release"];

fn in_generated_folder(v: &Volume, i: u32) -> bool {
    const BUILD: &[&[u8]] = BUILD_DIRS;
    const TOP: &[&[u8]] = &[b"program files", b"program files (x86)", b"windows", b"programdata"];
    let mut j = v.entry(i).parent;
    for _ in 0..1024 {
        if j == v.root {
            return false;
        }
        let e = v.entry(j);
        if e.kind == NONE {
            return true;
        }
        let name = v.lower(j);
        if BUILD.contains(&name) || (e.parent == v.root && TOP.contains(&name)) {
            return true;
        }
        j = e.parent;
    }
    true
}

fn push_recent(top: &mut Vec<(u32, char, u32)>, item: (u32, char, u32)) {
    if top.len() < RECENT {
        top.push(item);
    } else if let Some(min) = top.iter_mut().min_by_key(|t| t.0) {
        if item.0 > min.0 {
            *min = item;
        }
    }
}

pub fn stats(ix: &Index) -> Stats {
    let now = now();
    let horizon = now.saturating_add(DAY); // ignore files dated in the future
    let mut out = Stats::default();
    let mut top = Vec::new();
    let mut biggest: Option<(u64, char, u32)> = None;
    let mut folders: HashMap<(char, u32), u32> = HashMap::new();
    for v in &ix.vols {
        for (entries, tag) in [(&v.entries, 0), (&v.links, LINK)] {
            let parts: Vec<Part> = entries
                .par_chunks(CHUNK)
                .enumerate()
                .map(|(c, chunk)| {
                    let mut part = Part::default();
                    let mut generated: HashMap<u32, bool> = HashMap::new();
                    for (k, e) in chunk.iter().enumerate() {
                        if !e.live() {
                            continue;
                        }
                        let idx = (c * CHUNK + k) as u32 | tag;
                        part.counts[0] += 1;
                        if let Some(cat) = entry_cat(v, idx, e) {
                            part.counts[cat.index()] += 1;
                        }
                        if e.kind != FILE || e.noisy() || e.mtime > horizon || tag != 0 {
                            continue;
                        }
                        let top = &part.top;
                        let beats = top.len() < RECENT || top.iter().any(|t| e.mtime > t.0);
                        if beats && worth_showing(v, idx) {
                            push_recent(&mut part.top, (e.mtime, v.letter, idx));
                        }
                        // Activity: your own files only, not a build's output (checked once per folder).
                        let age = now.saturating_sub(e.mtime);
                        if !e.user() || age >= WEEKS as u32 * 7 * DAY {
                            continue;
                        }
                        if *generated.entry(e.parent).or_insert_with(|| in_generated_folder(v, idx) || is_build_dir(v.lower(e.parent))) {
                            continue;
                        }
                        if let Some(w) = part.weeks.get_mut((age / (7 * DAY)) as usize) {
                            *w += 1;
                        }
                        if age < DAY {
                            part.hours[23 - (age / 3600) as usize] += 1;
                        }
                        if age < 7 * DAY && part.biggest.is_none_or(|b| e.size > b.0) && worth_showing(v, idx) {
                            part.biggest = Some((e.size, v.letter, idx));
                        }
                        if age < 30 * DAY {
                            *part.folders.entry(e.parent).or_default() += 1;
                        }
                    }
                    part
                })
                .collect();
            for part in parts {
                for k in 0..8 {
                    out.counts[k] += part.counts[k];
                }
                for item in part.top {
                    push_recent(&mut top, item);
                }
                for k in 0..WEEKS {
                    out.weeks[k] += part.weeks[k];
                }
                for k in 0..24 {
                    out.hours[k] += part.hours[k];
                }
                if part.biggest.is_some_and(|b| biggest.is_none_or(|g| b.0 > g.0)) {
                    biggest = part.biggest;
                }
                for (dir, n) in part.folders {
                    *folders.entry((v.letter, dir)).or_default() += n;
                }
            }
        }
    }
    top.sort_by(|a, b| b.0.cmp(&a.0));
    out.recent = top.into_iter().map(|(_, l, i)| (l, i)).collect();
    out.biggest = biggest.map(|(_, l, i)| (l, i));
    let mut busy: Vec<(char, u32, u32)> = folders.into_iter().map(|((l, d), n)| (l, d, n)).collect();
    busy.sort_by(|a, b| b.2.cmp(&a.2));
    // Not a build's output or an app's own folder.
    out.folders = busy
        .into_iter()
        .filter(|&(l, d, _)| {
            ix.vol(l).is_some_and(|v| {
                let e = v.entry(d);
                d != v.root && e.kind == DIR && !e.noisy() && !in_generated_folder(v, d) && !is_build_dir(v.lower(d))
            })
        })
        .take(FOLDERS)
        .collect();
    out
}

fn is_build_dir(name: &[u8]) -> bool {
    BUILD_DIRS.contains(&name)
}

const STATS_EVERY: Duration = Duration::from_secs(3);

/// Keeps `Stats` current in the background as the index changes.
pub fn start_stats(shared: Arc<Shared>) -> Arc<Mutex<Stats>> {
    let out = Arc::new(Mutex::new(Stats::default()));
    let o = out.clone();
    std::thread::spawn(move || {
        crate::win::low_priority_thread();
        let mut seen = u64::MAX;
        let mut last = Instant::now() - STATS_EVERY;
        loop {
            std::thread::sleep(Duration::from_millis(500));
            // Only for someone looking, and not more than every few seconds.
            if !shared.foreground() || last.elapsed() < STATS_EVERY {
                continue;
            }
            let generation = shared.read().generation;
            if generation != seen {
                let t = Instant::now();
                let s = stats(&shared.read());
                crate::trace::log(format!(
                    "stats in {:.1} ms: {} today, {} this week, biggest {:?}, {} busy folders",
                    t.elapsed().as_secs_f64() * 1000.0,
                    s.hours.iter().sum::<u32>(),
                    s.weeks[0],
                    s.biggest,
                    s.folders.len()
                ));
                *lock(&o) = s;
                seen = generation;
                last = Instant::now();
                shared.notify();
            }
        }
    });
    out
}

pub struct Request {
    pub id: u64,
    pub text: String,
    pub cat: Cat,
    pub filters: Filters,
    pub sort: Sort,
    pub desc: bool,
    /// A new search rather than a refresh of the current one.
    pub reset: bool,
}

pub struct Answer {
    pub id: u64,
    pub hits: Vec<Hit>,
    /// How hits are shown; empty means one row each.
    pub rows: Vec<Row>,
    pub counts: Counts,
    /// Nothing matched exactly; these allow for a typo or two.
    pub fuzzy: bool,
    /// How plain-language words in the search were read.
    pub understood: Vec<String>,
    pub took: Duration,
    /// Index generation the results reflect.
    pub generation: u64,
    pub reset: bool,
}

/// Runs searches off the UI thread. Only the newest request matters: older ones are
/// skipped or abandoned midway.
pub struct Worker {
    tx: Sender<Request>,
    pub rx: Receiver<Answer>,
    latest: Arc<AtomicU64>,
}

impl Worker {
    pub fn start(shared: Arc<Shared>, history: Arc<Mutex<History>>) -> Worker {
        let (tx, requests) = mpsc::channel::<Request>();
        let (answers, rx) = mpsc::channel();
        let latest = Arc::new(AtomicU64::new(0));
        let newest = latest.clone();
        std::thread::spawn(move || {
            crate::win::low_priority_thread();
            let mut folders: HashMap<String, Option<(u8, u32)>> = HashMap::new();
            while let Ok(mut req) = requests.recv() {
                while let Ok(r) = requests.try_recv() {
                    req = r;
                }
                let t0 = Instant::now();
                let mut q = parse(&req.text, req.cat, &req.filters);
                let ix = shared.read();
                if let Some(path) = &req.filters.folder {
                    let found = *folders.entry(path.clone()).or_insert_with(|| {
                        let letter = path.chars().next()?.to_ascii_uppercase();
                        Some((letter as u8, ix.vol(letter)?.find_dir(path)?))
                    });
                    match found {
                        Some(w) => q.within = Some(w),
                        None => q.missing_folder = true,
                    }
                }
                let generation = ix.generation;
                let res = if q.is_empty() {
                    Some((Vec::new(), [0; 8], false))
                } else {
                    run(&ix, &q, req.sort, req.desc, &|| newest.load(Relaxed) != req.id)
                };
                let Some((mut hits, counts, fuzzy)) = res else { continue };
                let best = req.sort == Sort::Best && search_has_word(&q);
                if best {
                    let h = lock(&history);
                    if !h.is_empty() {
                        boost_hits(&ix, &mut hits, &h, &req.text);
                    }
                }
                let rows = if best { group(&ix, &mut hits) } else { Vec::new() };
                drop(ix);
                let a = Answer {
                    id: req.id,
                    hits,
                    rows,
                    counts,
                    fuzzy,
                    understood: q.understood.clone(),
                    took: t0.elapsed(),
                    generation,
                    reset: req.reset,
                };
                if answers.send(a).is_err() {
                    return;
                }
                shared.notify();
            }
        });
        Worker { tx, rx, latest }
    }

    pub fn ask(&self, req: Request) {
        self.latest.store(req.id, Relaxed);
        let _ = self.tx.send(req);
    }
}
