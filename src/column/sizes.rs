//! Sizes for the items in a folder. From the search index where the folder is
//! indexed: every folder's size is the sum of the files below it, worked out for a
//! whole drive at once and redone (at most every few seconds) as the index
//! changes. Anywhere else (a network share, say) by reading the folder from disk
//! in the background, one subfolder at a time, with the sizes filling in as they
//! become known.

use crate::index::{Shared, Volume, DIR, FILE, NONE};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often a drive's folder totals may be redone while files keep changing.
const REDO: Duration = Duration::from_secs(3);
/// Changed within this long counts as "recent" (drawn in the warm colour).
const DAY: u32 = 24 * 3600;
/// Sizes read from disk are kept this long before being read again.
const WALK_KEEP: Duration = Duration::from_secs(600);

/// How far along an item's size is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub enum State {
    #[default]
    Known,
    /// Being added up right now: the size so far.
    Counting,
    /// Not started yet.
    Waiting,
}

/// One item in the folder being shown.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Item {
    pub size: u64,
    /// Bytes in files changed lately (the last day, or week).
    pub recent: u64,
    pub dir: bool,
    pub state: State,
    /// Kept online (OneDrive and the like), not on this PC.
    pub cloud: bool,
}

/// What's in one folder, by name.
#[derive(Default)]
pub struct Listing {
    /// Lowercase name -> item.
    pub by_name: HashMap<String, Item>,
    /// Lowercase name without its extension -> item (None when two share it), for
    /// Explorer set to hide extensions.
    pub by_stem: HashMap<String, Option<Item>>,
    /// Everything in the folder, added up.
    pub total: u64,
    /// While sizes are still being read from disk: folders done, and how many there are.
    pub progress: Option<(u32, u32)>,
    /// Every item, biggest first.
    pub sorted: Vec<(String, Item)>,
    /// Read from disk rather than the index.
    pub walked: bool,
}

impl Listing {
    pub fn build(mut items: Vec<(String, Item)>, progress: Option<(u32, u32)>, walked: bool) -> Listing {
        items.sort_by(|a, b| b.1.size.cmp(&a.1.size).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
        let mut l = Listing { progress, walked, ..Default::default() };
        for (name, item) in &items {
            let lower = name.to_lowercase();
            if let Some((stem, _)) = lower.rsplit_once('.').filter(|(s, _)| !s.is_empty()) {
                l.by_stem.entry(stem.to_string()).and_modify(|e| *e = None).or_insert(Some(*item));
            }
            l.total += item.size;
            l.by_name.insert(lower, *item);
        }
        l.sorted = items;
        l
    }

    /// The item Explorer shows under this name.
    pub fn find(&self, shown: &str) -> Option<Item> {
        let lower = shown.to_lowercase();
        self.by_name.get(&lower).copied().or_else(|| self.by_stem.get(&lower).copied().flatten())
    }
}

/// More about one folder, for the card shown when the pointer rests on it.
#[derive(Clone, Default, Hash, PartialEq)]
pub struct Details {
    pub files: u64,
    pub dirs: u64,
    /// The biggest things directly inside, biggest first.
    pub top: Vec<(String, u64)>,
}

struct Totals {
    generation: u64,
    at: Instant,
    len: usize,
    week: bool,
    size: Vec<u64>,
    recent: Vec<u64>,
}

#[derive(Default)]
pub struct Sizes {
    totals: HashMap<char, Totals>,
    /// The last listing made from the index: the folder, the index generation and
    /// the "recent" span it was made with.
    last: Option<(String, u64, bool, Arc<Listing>)>,
    walks: Vec<Walk>,
    details: Option<(String, u64, Option<Details>)>,
}

/// The drive letter of a path like `C:\Users`; None for network paths.
pub fn letter_of(path: &str) -> Option<char> {
    let letter = path.chars().next()?.to_ascii_uppercase();
    (path.as_bytes().get(1) == Some(&b':') && letter.is_ascii_alphabetic()).then_some(letter)
}

impl Sizes {
    /// The folder's items with their sizes: from the index, or failing that by
    /// reading the disk (sizes then arrive bit by bit: ask again). `week` picks
    /// what counts as recent: the last week rather than the last day.
    pub fn listing(&mut self, shared: &Shared, path: &str, week: bool) -> Option<Arc<Listing>> {
        if let Some(l) = self.indexed(shared, path, week) {
            // In the index now: no need to go on reading it from disk.
            self.walks.retain(|w| {
                let other = !w.path.eq_ignore_ascii_case(path);
                if !other {
                    w.job.cancel.store(true, Relaxed);
                }
                other
            });
            return Some(l);
        }
        // A drive that's about to be indexed: wait for that rather than read it twice.
        if let Some(letter) = letter_of(path) {
            let pending = shared.read().vol(letter).is_none() && shared.wanted(letter) && !shared.jobs().is_empty();
            if pending {
                return None;
            }
        }
        self.walked(path, week)
    }

    fn indexed(&mut self, shared: &Shared, path: &str, week: bool) -> Option<Arc<Listing>> {
        let letter = letter_of(path)?;
        let ix = shared.read();
        let generation = ix.generation;
        if let Some((p, g, w, l)) = &self.last {
            let fresh = *g == generation || self.totals.get(&letter).is_some_and(|t| t.at.elapsed() < REDO);
            if p.eq_ignore_ascii_case(path) && *w == week && fresh {
                return Some(l.clone());
            }
        }
        let vol = ix.vol(letter)?;
        let dir = vol.find_dir(path)?;
        self.refresh_totals(vol, letter, generation, week);
        let t = &self.totals[&letter];
        let mut items = Vec::new();
        for (i, e) in vol.entries.iter().enumerate() {
            let i = i as u32;
            if e.parent != dir || i == dir || e.kind == NONE || !e.live() {
                continue;
            }
            let item = if e.kind == DIR {
                let k = i as usize;
                Item { size: t.size.get(k).copied().unwrap_or(0), recent: t.recent.get(k).copied().unwrap_or(0), dir: true, ..Default::default() }
            } else {
                // Only folders say what grew: a file changed today is just a file.
                Item { size: e.size, ..Default::default() }
            };
            items.push((vol.name(i).to_string(), item));
        }
        let listing = Arc::new(Listing::build(items, None, false));
        self.last = Some((path.to_string(), generation, week, listing.clone()));
        Some(listing)
    }

    fn refresh_totals(&mut self, vol: &Volume, letter: char, generation: u64, week: bool) {
        let stale = match self.totals.get(&letter) {
            None => true,
            Some(t) => t.len != vol.entries.len() || t.week != week || (t.generation != generation && t.at.elapsed() >= REDO),
        };
        if stale {
            let t = Instant::now();
            let (size, recent) = folder_totals(&vol.entries, vol.root, if week { 7 * DAY } else { DAY });
            crate::trace::log(format!("size column: {letter}: folder totals in {:.0} ms", t.elapsed().as_secs_f64() * 1000.0));
            self.totals.insert(letter, Totals { generation, at: Instant::now(), len: vol.entries.len(), week, size, recent });
        }
    }

    /// Sizes read from disk, as far as they've got.
    fn walked(&mut self, path: &str, week: bool) -> Option<Arc<Listing>> {
        // Anything still being read for a folder no longer shown is dropped.
        self.walks.retain(|w| {
            let keep = w.path.eq_ignore_ascii_case(path) || w.job.done.load(Relaxed);
            if !keep {
                w.job.cancel.store(true, Relaxed);
            }
            keep
        });
        let stale = |w: &Walk| w.week != week || (w.job.done.load(Relaxed) && w.at.elapsed() > WALK_KEEP);
        if let Some(i) = self.walks.iter().position(|w| w.path.eq_ignore_ascii_case(path)) {
            if stale(&self.walks[i]) {
                self.walks.remove(i).job.cancel.store(true, Relaxed);
            }
        }
        if !self.walks.iter().any(|w| w.path.eq_ignore_ascii_case(path)) {
            if !std::path::Path::new(path).is_dir() {
                return None;
            }
            if self.walks.len() >= 6 {
                self.walks.remove(0);
            }
            let since = crate::index::now().saturating_sub(if week { 7 * DAY } else { DAY });
            self.walks.push(Walk { path: path.to_string(), week, at: Instant::now(), job: start_walk(path.to_string(), since), built: None });
        }
        let w = self.walks.iter_mut().find(|w| w.path.eq_ignore_ascii_case(path))?;
        if let Some(l) = &w.built {
            return Some(l.clone());
        }
        let done = w.job.done.load(Relaxed);
        let items = crate::index::lock(&w.job.items).clone();
        let dirs = items.iter().filter(|i| i.dir).count() as u32;
        let counted = items.iter().filter(|i| i.dir && i.state == State::Known).count() as u32;
        let list = items
            .iter()
            .map(|i| (i.name.clone(), Item { size: i.size, recent: if i.dir { i.recent } else { 0 }, dir: i.dir, state: i.state, ..Default::default() }))
            .collect();
        let l = Arc::new(Listing::build(list, (!done).then_some((counted, dirs)), true));
        if done {
            w.built = Some(l.clone());
        }
        Some(l)
    }

    /// What's inside one of the folder's subfolders: how many files and folders,
    /// and its biggest parts. None for files, and where that isn't known.
    pub fn details(&mut self, shared: &Shared, folder: &str, name: &str) -> Option<Details> {
        let path = std::path::Path::new(folder).join(name).to_string_lossy().into_owned();
        // Read from disk: just the counts.
        if let Some(w) = self.walks.iter().find(|w| w.path.eq_ignore_ascii_case(folder)) {
            let items = crate::index::lock(&w.job.items);
            let it = items.iter().find(|i| i.dir && i.name.eq_ignore_ascii_case(name) && i.state == State::Known)?;
            return Some(Details { files: it.files, dirs: it.dirs, top: Vec::new() });
        }
        let letter = letter_of(&path)?;
        let ix = shared.read();
        if let Some((p, g, d)) = &self.details {
            if *g == ix.generation && p.eq_ignore_ascii_case(&path) {
                return d.clone();
            }
        }
        let t = Instant::now();
        let found = (|| {
            let vol = ix.vol(letter)?;
            let dir = vol.find_dir(&path)?;
            let totals = self.totals.get(&letter)?;
            let inside = inside_flags(vol, dir);
            let mut d = Details::default();
            let mut top: Vec<(u32, u64)> = Vec::new();
            for (i, e) in vol.entries.iter().enumerate() {
                if inside[i] != INSIDE || e.kind == NONE || !e.live() {
                    continue;
                }
                match e.kind {
                    DIR => d.dirs += 1,
                    _ => d.files += 1,
                }
                if e.parent == dir {
                    let size = if e.kind == DIR { totals.size.get(i).copied().unwrap_or(0) } else { e.size };
                    top.push((i as u32, size));
                }
            }
            top.sort_by(|a, b| b.1.cmp(&a.1));
            d.top = top.iter().take(3).filter(|(_, s)| *s > 0).map(|&(i, s)| (vol.name(i).to_string(), s)).collect();
            Some(d)
        })();
        crate::trace::log(format!("size column: details of {path} in {:.0} ms", t.elapsed().as_secs_f64() * 1000.0));
        self.details = Some((path, ix.generation, found.clone()));
        found
    }

    /// The biggest files anywhere under a folder, biggest first: (full path, name,
    /// size). Empty where the folder isn't in the index.
    pub fn biggest_files(&self, shared: &Shared, path: &str, n: usize) -> Vec<(String, String, u64)> {
        let Some(letter) = letter_of(path) else { return Vec::new() };
        let ix = shared.read();
        let Some(vol) = ix.vol(letter) else { return Vec::new() };
        let Some(dir) = vol.find_dir(path) else { return Vec::new() };
        let inside = inside_flags(vol, dir);
        let mut files: Vec<(u64, u32)> = vol
            .entries
            .iter()
            .enumerate()
            .filter(|(i, e)| e.kind == FILE && e.size > 0 && inside[*i] == INSIDE && e.live())
            .map(|(i, e)| (e.size, i as u32))
            .collect();
        files.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        files.truncate(n);
        files.into_iter().map(|(size, i)| (vol.path(i), vol.name(i).to_string(), size)).collect()
    }
}

const INSIDE: u8 = 1;
const OUTSIDE: u8 = 2;

/// For every entry on the drive, whether it lies somewhere under `dir`.
fn inside_flags(vol: &Volume, dir: u32) -> Vec<u8> {
    let entries = &vol.entries;
    let n = entries.len();
    let (d, root) = (dir as usize, vol.root as usize);
    let mut flag = vec![0u8; n];
    let mut chain: Vec<usize> = Vec::new();
    for start in 0..n {
        if flag[start] != 0 || start == d {
            continue;
        }
        // Up through the parents until the folder, the drive's root, or an entry
        // already settled; everything passed on the way gets the same answer.
        chain.clear();
        let mut i = start;
        let verdict = loop {
            if i == d {
                break INSIDE;
            }
            if flag[i] != 0 {
                break flag[i];
            }
            chain.push(i);
            let up = entries[i].parent as usize;
            if up >= n || up == i || i == root || chain.len() > 512 {
                break OUTSIDE;
            }
            i = up;
        };
        for &c in &chain {
            flag[c] = verdict;
        }
    }
    flag
}

/// Works out which folder Explorer is showing from its title (the folder's name)
/// and the names of the items on show: the indexed folder of that name holding
/// most of them. For when the shell won't say.
pub fn guess_folder(shared: &Shared, title: &str, names: &[String]) -> Option<String> {
    let title = title.trim().trim_end_matches(" - File Explorer").trim();
    // A drive: "Local Disk (C:)".
    if let Some(l) = title.strip_suffix(":)").and_then(|t| t.chars().last()).filter(|c| c.is_ascii_alphabetic()) {
        return Some(format!("{}:\\", l.to_ascii_uppercase()));
    }
    let want = title.to_lowercase();
    let names: std::collections::HashSet<String> = names.iter().map(|n| n.to_lowercase()).collect();
    if want.is_empty() || names.is_empty() {
        return None;
    }
    let ix = shared.read();
    let mut best: Option<(usize, String)> = None;
    for vol in &ix.vols {
        let named: std::collections::HashSet<u32> = (0..vol.entries.len() as u32)
            .filter(|&i| {
                let e = &vol.entries[i as usize];
                e.kind == DIR && e.live() && vol.lower(i) == want.as_bytes()
            })
            .collect();
        if named.is_empty() {
            continue;
        }
        let mut hits: HashMap<u32, usize> = HashMap::new();
        for (i, e) in vol.entries.iter().enumerate() {
            if !named.contains(&e.parent) || !e.live() {
                continue;
            }
            let n = std::str::from_utf8(vol.lower(i as u32)).unwrap_or("");
            let stem = n.rsplit_once('.').map_or("", |(s, _)| s);
            if names.contains(n) || names.contains(stem) {
                *hits.entry(e.parent).or_default() += 1;
            }
        }
        if let Some((&dir, &n)) = hits.iter().max_by_key(|(_, n)| **n) {
            if best.as_ref().is_none_or(|b| n > b.0) {
                best = Some((n, vol.path(dir)));
            }
        }
    }
    let (n, path) = best?;
    // Most of what's on show has to be there.
    (n * 10 >= names.len() * 6).then_some(path)
}

/// Every folder's total size (and how much of it changed lately): each file's
/// size added to all the folders above it.
fn folder_totals(entries: &[crate::index::Entry], root: u32, span: u32) -> (Vec<u64>, Vec<u64>) {
    let n = entries.len();
    let (mut size, mut recent) = (vec![0u64; n], vec![0u64; n]);
    let since = crate::index::now().saturating_sub(span);
    for e in entries {
        if e.kind != FILE || e.size == 0 || !e.live() {
            continue;
        }
        let new = e.mtime >= since;
        let mut p = e.parent as usize;
        for _ in 0..512 {
            if p >= n {
                break;
            }
            size[p] += e.size;
            if new {
                recent[p] += e.size;
            }
            let up = entries[p].parent as usize;
            if p == root as usize || up == p {
                break;
            }
            p = up;
        }
    }
    (size, recent)
}

// ------------------------------------------------------- reading from disk

/// One folder being read from disk (or already read).
struct Walk {
    path: String,
    week: bool,
    at: Instant,
    job: Arc<Job>,
    /// The finished listing.
    built: Option<Arc<Listing>>,
}

struct Job {
    cancel: AtomicBool,
    done: AtomicBool,
    items: Mutex<Vec<Walked>>,
}

#[derive(Clone)]
struct Walked {
    name: String,
    dir: bool,
    size: u64,
    recent: u64,
    state: State,
    files: u64,
    dirs: u64,
}

/// Reads a folder on a thread of its own: its files at once, then each subfolder
/// added up in turn. Quietly: lowest CPU and disk priority.
fn start_walk(path: String, since: u32) -> Arc<Job> {
    let job = Arc::new(Job { cancel: AtomicBool::new(false), done: AtomicBool::new(false), items: Mutex::new(Vec::new()) });
    let j = job.clone();
    let _ = std::thread::Builder::new().name("size-column-walk".into()).spawn(move || {
        crate::win::background_thread();
        let t = Instant::now();
        walk(&path, since, &j);
        j.done.store(true, Relaxed);
        crate::trace::log(format!("size column: read {path} from disk in {:.0} ms", t.elapsed().as_secs_f64() * 1000.0));
    });
    job
}

fn walk(path: &str, since: u32, job: &Job) {
    let Ok(rd) = std::fs::read_dir(path) else { return };
    let mut items = Vec::new();
    for e in rd.flatten() {
        if job.cancel.load(Relaxed) {
            return;
        }
        let Ok(md) = e.metadata() else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        let dir = md.is_dir(); // links to folders aren't followed: they count as nothing
        items.push(Walked {
            name,
            dir,
            size: if dir { 0 } else { md.len() },
            recent: 0,
            state: if dir { State::Waiting } else { State::Known },
            files: 0,
            dirs: 0,
        });
    }
    *crate::index::lock(&job.items) = items.clone();
    for (i, it) in items.iter().enumerate() {
        if !it.dir {
            continue;
        }
        crate::index::lock(&job.items)[i].state = State::Counting;
        // (bytes, changed lately, files, folders) so far.
        let mut sum = (0u64, 0u64, 0u64, 0u64);
        let mut told = Instant::now();
        let mut stack = vec![std::path::Path::new(path).join(&it.name)];
        while let Some(dir) = stack.pop() {
            if job.cancel.load(Relaxed) {
                return;
            }
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let Ok(md) = e.metadata() else { continue };
                if md.is_dir() {
                    sum.3 += 1;
                    stack.push(e.path());
                } else {
                    sum.0 += md.len();
                    sum.2 += 1;
                    if crate::index::unix_time(md.modified()) >= since {
                        sum.1 += md.len();
                    }
                }
            }
            if told.elapsed() > Duration::from_millis(150) {
                told = Instant::now();
                let mut items = crate::index::lock(&job.items);
                (items[i].size, items[i].recent, items[i].files, items[i].dirs) = sum;
            }
        }
        let mut items = crate::index::lock(&job.items);
        (items[i].size, items[i].recent, items[i].files, items[i].dirs) = sum;
        items[i].state = State::Known;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_is_added_up_and_sorted_biggest_first() {
        let item = |size| Item { size, ..Default::default() };
        let l = Listing::build(vec![("a".into(), item(5)), ("b".into(), item(50)), ("c".into(), item(20)), ("d".into(), item(1)), ("e".into(), item(0))], None, false);
        assert_eq!(l.total, 76);
        assert_eq!(l.sorted.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["b", "c", "a", "d", "e"]);
        assert_eq!(l.find("C").unwrap().size, 20);
    }
}
