//! Remembers what you open, so it comes first next time. Things you use often
//! and recently rank higher ("frecency"), and so does whatever you picked for
//! the same search before. Kept in %LOCALAPPDATA%\<app>\history.tsv.

use std::collections::HashMap;
use std::path::PathBuf;

const MAX_ITEMS: usize = 2000;
const MAX_QUERIES: usize = 8;

struct Item {
    /// Full path as shown (or an app's name).
    path: String,
    count: u32,
    last: u32,
    /// Searches it was picked from: (query, times, last time).
    queries: Vec<(String, u32, u32)>,
}

#[derive(Default)]
pub struct History {
    /// By key: a lowercase path, or `app:` and an app's lowercase name.
    items: HashMap<String, Item>,
}

fn file() -> Option<PathBuf> {
    Some(crate::edition::data_dir()?.join("history.tsv"))
}

fn normalize(q: &str) -> String {
    q.trim().to_lowercase()
}

/// Recent uses count for more than old ones.
fn weight(last: u32, now: u32) -> f32 {
    match now.saturating_sub(last) / 86_400 {
        0 => 1.0,
        1..=6 => 0.7,
        7..=29 => 0.45,
        30..=89 => 0.25,
        _ => 0.1,
    }
}

impl History {
    pub fn load() -> History {
        let mut h = History::default();
        let Some(text) = file().and_then(|f| std::fs::read_to_string(f).ok()) else { return h };
        for line in text.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            let num = |i: usize| f.get(i).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
            match f.first() {
                Some(&"i") if f.len() >= 5 => {
                    let item = Item { path: f[2].to_string(), count: num(3), last: num(4), queries: Vec::new() };
                    h.items.insert(f[1].to_string(), item);
                }
                Some(&"q") if f.len() >= 5 => {
                    if let Some(item) = h.items.get_mut(f[1]) {
                        item.queries.push((f[2].to_string(), num(3), num(4)));
                    }
                }
                _ => {}
            }
        }
        h
    }

    fn save(&self) {
        let Some(path) = file() else { return };
        let mut out = String::new();
        for (key, it) in &self.items {
            out.push_str(&format!("i\t{key}\t{}\t{}\t{}\n", it.path, it.count, it.last));
            for (q, n, last) in &it.queries {
                out.push_str(&format!("q\t{key}\t{q}\t{n}\t{last}\n"));
            }
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, out).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Notes that something was opened, after searching for `query`.
    pub fn record(&mut self, key: &str, path: &str, query: &str) {
        let now = crate::index::now();
        let it = self.items.entry(key.to_string()).or_insert_with(|| Item {
            path: path.to_string(),
            count: 0,
            last: 0,
            queries: Vec::new(),
        });
        it.count += 1;
        it.last = now;
        it.path = path.to_string();
        let q = normalize(query);
        if q.chars().count() >= 2 && !q.contains('\t') {
            match it.queries.iter_mut().find(|e| e.0 == q) {
                Some(e) => {
                    e.1 += 1;
                    e.2 = now;
                }
                None => it.queries.push((q, 1, now)),
            }
            it.queries.sort_by(|a, b| b.2.cmp(&a.2));
            it.queries.truncate(MAX_QUERIES);
        }
        if self.items.len() > MAX_ITEMS {
            let mut keys: Vec<(f32, String)> =
                self.items.iter().map(|(k, it)| (it.count as f32 * weight(it.last, now), k.clone())).collect();
            keys.sort_by(|a, b| a.0.total_cmp(&b.0));
            for (_, k) in keys.into_iter().take(self.items.len() - MAX_ITEMS) {
                self.items.remove(&k);
            }
        }
        self.save();
    }

    /// How much to lift a result for `query`: 0 if it's never been opened, up to 80
    /// for something opened a lot, lately, from this very search.
    pub fn boost(&self, key: &str, query: &str, now: u32) -> u8 {
        let Some(it) = self.items.get(key) else { return 0 };
        let mut b = (it.count as f32).sqrt() * 8.0 * weight(it.last, now);
        let q = normalize(query);
        if q.chars().count() >= 2 {
            for (pq, n, last) in &it.queries {
                // The same search, or one that's still being typed.
                let m = if *pq == q {
                    40.0
                } else if pq.starts_with(&q) || q.starts_with(pq.as_str()) {
                    28.0
                } else {
                    0.0
                };
                b += m * weight(*last, now) * (1.0 + (*n as f32).ln().max(0.0) * 0.3);
            }
        }
        b.min(80.0) as u8
    }

    /// The most used things, best first: (key, path).
    pub fn top(&self, n: usize) -> Vec<(String, String)> {
        let now = crate::index::now();
        let mut all: Vec<(f32, &String, &Item)> =
            self.items.iter().map(|(k, it)| ((it.count as f32).sqrt() * weight(it.last, now), k, it)).collect();
        all.sort_by(|a, b| b.0.total_cmp(&a.0));
        all.into_iter().take(n).map(|(_, k, it)| (k.clone(), it.path.clone())).collect()
    }
}
