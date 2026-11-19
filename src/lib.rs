//! Instant file search and a disk space analyzer (the Space view) for Windows.
//! Everything is here; `src/main.rs` (Godspeed)
//! and `lordspeed/` (LordSpeed) just pick an edition and start it.

mod app;
mod apps;
mod column;
pub mod edition;
mod fmt;
mod history;
mod icon;
mod index;
mod ntfs;
mod search;
mod settings;
mod shell;
mod space;
mod theme;
mod trace;
mod tray;
mod update;
mod usn;
mod walk;
mod watch;
mod win;

use eframe::egui;
use edition::Edition;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub type Result = eframe::Result<()>;

pub fn start(ed: &'static Edition) -> Result {
    trace::init();
    trace::log(format!("start, version {}", update::version()));
    edition::set(ed);
    update::clean_up();
    // Parallel work (indexing, searching) leaves a core or two free and always
    // yields to whatever else is running.
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads((cores * 3 / 4).max(2))
        .start_handler(|_| win::low_priority_thread())
        .build_global();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--bench") => {
            bench(&args[2..]);
            return Ok(());
        }
        Some("--self-update") => {
            update::cli(args.get(2).map_or("update-report.txt", String::as_str));
            return Ok(());
        }
        Some("--column-tree") => {
            column::tree(&args[2..]);
            return Ok(());
        }
        Some("--column-real") => {
            column::real_test(&args[2..]);
            return Ok(());
        }
        Some("--column-live") => {
            column::live_test(&args[2..]);
            return Ok(());
        }
        Some("--quit") => {
            tray::quit_running();
            return Ok(());
        }
        Some("--icon-preview") => {
            let dir = std::path::PathBuf::from(args.get(2).map_or(".", String::as_str));
            let _ = std::fs::write(dir.join("godspeed.png"), icon::png(256, icon::GOD));
            let _ = std::fs::write(dir.join("lordspeed.png"), icon::png(256, icon::LORD));
            return Ok(());
        }
        _ => {}
    }
    let flag = |f: &str| args.iter().any(|a| a == f);
    let background = flag("--background");
    // Already running (in the tray, say): bring that one up instead. Not for the
    // copy started to take over, after an update or to get admin rights.
    if !flag("--elevated") && !flag("--updated") && tray::hand_over(!background) {
        return Ok(());
    }
    // Reading the file table needs admin rights: Godspeed asks once (the UAC prompt)
    // and carries on with the slower folder walk if that's declined.
    if ed.admin && !win::is_elevated() && !flag("--elevated") && !flag("--no-admin") && win::relaunch_as_admin() {
        return Ok(());
    }
    if flag("--opengl") {
        return run(eframe::Renderer::Glow, background);
    }
    run(eframe::Renderer::Wgpu, background).or_else(|_| run(eframe::Renderer::Glow, background))
}

/// `background`: start in the tray, with no window showing.
fn run(renderer: eframe::Renderer, background: bool) -> Result {
    let ed = edition::get();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(ed.name)
        .with_inner_size([1240.0, 800.0])
        .with_min_inner_size(app::MIN_SIZE)
        .with_icon(Arc::new(egui::IconData { rgba: icon::rgba(64, ed.palette), width: 64, height: 64 }));
    if background {
        // eframe shows the window after its first frame, before it can be hidden
        // again: keep that off screen.
        viewport = viewport.with_position([-32000.0, -32000.0]);
    }
    let options = eframe::NativeOptions { renderer, viewport, ..Default::default() };
    trace::log(format!("run_native ({renderer:?})"));
    eframe::run_native(
        ed.name,
        options,
        Box::new(|cc| {
            trace::log("window ready, installing fonts");
            theme::install(&cc.egui_ctx);
            trace::log("fonts installed, creating app");
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let hwnd = match cc.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
                _ => 0,
            };
            let app = app::App::new(cc.egui_ctx.clone(), hwnd, background);
            trace::log("app created");
            Ok(Box::new(app))
        }),
    )
}

/// Makes, renames and deletes files while following a walked drive, and reports
/// whether the index kept up.
fn watch_test(v: index::Volume, letter: char) -> String {
    let shared = index::Shared::new(Box::new(|| {}));
    shared.write().install(v);
    let s = shared.clone();
    std::thread::spawn(move || watch::follow_only(s, letter));
    std::thread::sleep(Duration::from_millis(500));
    // Somewhere on the drive being tested (the temp folder, if it's on that drive).
    let temp = std::env::temp_dir();
    let base = if temp.to_string_lossy().to_ascii_uppercase().starts_with(letter) {
        temp
    } else {
        std::path::PathBuf::from(format!("{letter}:\\"))
    };
    let dir = base.join(format!("speed-watch-test-{}", std::process::id()));
    let has = |p: &std::path::Path| {
        std::thread::sleep(Duration::from_millis(900));
        shared.read().vol(letter).and_then(|v| v.find_path(&p.to_string_lossy())).is_some()
    };
    let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&a, b"hello");
    let created = has(&a);
    let _ = std::fs::rename(&a, &b);
    let renamed = has(&b) && !has(&a);
    let _ = std::fs::remove_dir_all(&dir);
    let deleted = !has(&b) && !has(&dir);
    format!("created {created}, renamed {renamed}, deleted {deleted}")
}

/// Headless run for testing: `godspeed --bench C: report.txt [--walk]`.
fn bench(args: &[String]) {
    use search::{Cat, Filters, Sort};
    let letter = args.first().and_then(|s| s.chars().next()).unwrap_or('C').to_ascii_uppercase();
    let out = args.get(1).cloned().unwrap_or_else(|| "godspeed-bench.txt".into());
    let mft = !args.iter().any(|a| a == "--walk");
    let job = index::Job::new(letter, mft);
    let mut s = String::new();
    let t0 = Instant::now();
    let network = win::drives().iter().any(|d| d.letter == letter && d.network);
    let built = if network {
        walk::scan_network(letter, &job).map(|raws| (index::Volume::build(letter, index::Method::Walk, raws, 0), None, None))
    } else {
        index::build(letter, mft, &job)
    };
    let _ = writeln!(s, "network drive: {network}");
    match built {
        Ok((vol, cursor, note)) => {
            let _ = writeln!(s, "drive: {letter}:  elevated: {}", win::is_elevated());
            let _ = writeln!(s, "method: {:?}  live: {}  note: {:?}", vol.method, vol.live, note);
            let _ = writeln!(s, "index time: {}", fmt::duration(t0.elapsed()));
            let mem = (vol.entries.len() + vol.links.len()) * std::mem::size_of::<index::Entry>() + vol.names_len();
            let _ = writeln!(
                s,
                "items: {}  records: {}  links: {}  memory: {}",
                fmt::count(vol.items),
                fmt::count(vol.entries.len() as u64),
                fmt::count(vol.links.len() as u64),
                fmt::size(mem as u64)
            );
            if !mft {
                // The saved index: how long it takes to write and read back.
                let t = Instant::now();
                let saved = vol.save_cache();
                let save_time = t.elapsed();
                let t = Instant::now();
                let loaded = index::Volume::load_cache(letter);
                let _ = writeln!(
                    s,
                    "saved index: {:?} in {}, loaded in {} ({} items)",
                    saved.map(fmt::size),
                    fmt::duration(save_time),
                    fmt::duration(t.elapsed()),
                    loaded.as_ref().map_or(0, |v| v.items)
                );
                let t = Instant::now();
                let found = vol.find_path(&format!("{letter}:\\Windows\\System32\\kernel32.dll"));
                let _ = writeln!(s, "find kernel32.dll by path: {:?} in {}", found.is_some(), fmt::duration(t.elapsed()));
                // Stats and a search while another walk is going: they must not wait for it.
                if let Some(copy) = index::Volume::load_cache(letter) {
                    let mut ix = index::Index::default();
                    ix.put(copy);
                    let walking = std::thread::spawn(move || {
                        let job = index::Job::new(letter, false);
                        let t = Instant::now();
                        let _ = walk::scan(letter, &job);
                        t.elapsed()
                    });
                    std::thread::sleep(Duration::from_millis(500));
                    let t = Instant::now();
                    let _ = search::stats(&ix);
                    let stats_time = t.elapsed();
                    let t = Instant::now();
                    let n = search::run(&ix, &search::parse("report", Cat::All, &Filters::default()), Sort::Best, false, &|| false)
                        .map_or(0, |r| r.0.len());
                    let search_time = t.elapsed();
                    let walk_time = walking.join().unwrap_or_default();
                    let _ = writeln!(
                        s,
                        "during a walk ({}): stats {}, search {} ({n} hits)",
                        fmt::duration(walk_time),
                        fmt::duration(stats_time),
                        fmt::duration(search_time)
                    );
                }
                if let Some(v) = loaded {
                    let _ = writeln!(s, "folder-change notifications: {}", watch_test(v, letter));
                }
            }
            let mut ix = index::Index::default();
            ix.put(vol);
            let none = Filters::default();
            let _ = writeln!(s, "\nsearches (best match):");
            {
                let mut hits = search::run(&ix, &search::parse("android", Cat::All, &none), Sort::Best, false, &|| false)
                    .map(|r| r.0)
                    .unwrap_or_default();
                let rows = search::group(&ix, &mut hits);
                let _ = writeln!(s, "\n\"android\": {} hits in {} rows; top rows:", hits.len(), rows.len());
                for &(start, len) in rows.iter().take(12) {
                    let h = hits[start as usize];
                    let more = if len > 1 { format!("   (+{} more)", len - 1) } else { String::new() };
                    let _ = writeln!(s, "  [{:>3}] {}{more}", h.rank, ix.vols[0].path(h.idx));
                }
            }
            for q in [
                "andriod",
                "androidprojects",
                "photos from last week",
                "big videos",
                "a",
                "e",
                "remote desktop",
                "report",
                "*.dll",
                "ext:jpg;png",
                "windows\\system32 kernel",
                "size:>1gb",
                "folder:users",
                "zzqqxxjj",
            ] {
                let t = Instant::now();
                let parsed = search::parse(q, Cat::All, &none);
                let (hits, counts, fuzzy) =
                    search::run(&ix, &parsed, Sort::Best, false, &|| false).unwrap_or((Vec::new(), [0; 8], false));
                let _ = writeln!(
                    s,
                    "  {:<26} {:>10} hits  {:>7}   folders {} docs {} pics {}{}{}",
                    q,
                    fmt::count(hits.len() as u64),
                    fmt::duration(t.elapsed()),
                    counts[1],
                    counts[2],
                    counts[3],
                    if fuzzy { "   [typo-tolerant]" } else { "" },
                    if parsed.understood.is_empty() { String::new() } else { format!("   understood: {:?}", parsed.understood) }
                );
                for h in hits.iter().take(3) {
                    let _ = writeln!(s, "      {}", ix.vols[0].path(h.idx));
                }
            }
            let t = Instant::now();
            let st = search::stats(&ix);
            let _ = writeln!(s, "\nstats in {}: {:?}", fmt::duration(t.elapsed()), st.counts);
            for &(l, i) in st.recent.iter().take(5) {
                let _ = writeln!(s, "  recent {l}: {}", ix.vols[0].path(i));
            }
            let t = Instant::now();
            let dir = ix.vols[0].find_dir("C:\\Windows\\System32");
            let _ = writeln!(s, "find C:\\Windows\\System32: {:?} in {}", dir.is_some(), fmt::duration(t.elapsed()));
            let _ = writeln!(s, "\nsorts of \"e\":");
            for sort in [Sort::Name, Sort::Size, Sort::Modified, Sort::Folder] {
                let t = Instant::now();
                let n = search::run(&ix, &search::parse("e", Cat::All, &none), sort, true, &|| false)
                    .map_or(0, |h| h.0.len());
                let _ = writeln!(s, "  {:<14} {:>10} hits  {}", sort.title(), fmt::count(n as u64), fmt::duration(t.elapsed()));
            }
            if let Some(mut c) = cursor {
                // Create and delete a file, then check the journal reports both.
                let name = format!("godspeed-journal-test-{}.txt", std::process::id());
                let path = std::env::temp_dir().join(&name);
                let _ = c.read();
                let _ = std::fs::write(&path, b"hello");
                std::thread::sleep(Duration::from_millis(150));
                let created = c.read().map(|v| v.iter().any(|x| x.name == name && !x.deleted));
                let _ = std::fs::remove_file(&path);
                std::thread::sleep(Duration::from_millis(150));
                let deleted = c.read().map(|v| v.iter().any(|x| x.name == name && x.deleted));
                let _ = writeln!(s, "\njournal: saw create {created:?}, saw delete {deleted:?}");
            }
            let t = Instant::now();
            let apps = apps::load();
            let _ = writeln!(s, "\napps, settings and tools: {} in {}", apps.len(), fmt::duration(t.elapsed()));
            for q in ["remote desktop", "group policy", "wallpaper", "device manager", "dark mode", "env", "rdc", "vsc"] {
                let found: Vec<String> = apps::find(&apps, q, &|_| 0)
                    .into_iter()
                    .map(|i| format!("{} ({})", apps[i].name, apps[i].kind.label()))
                    .collect();
                let _ = writeln!(s, "  {q:<16} -> {found:?}");
            }
        }
        Err(e) => {
            let _ = writeln!(s, "error: {e}");
        }
    }
    let _ = std::fs::write(out, s);
}
