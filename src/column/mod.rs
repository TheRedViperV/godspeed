//! The size column in Windows Explorer. Explorer can't be given new columns for
//! folders any more, so this draws one: a click-through window laid over the
//! empty space right of Explorer's last column in Details view, lined up with
//! each row and showing that item's size from the search index. It follows the
//! Explorer window you're using. Around it: the folder's totals in Explorer's
//! status bar, a card with more when the pointer rests on a size, a list of the
//! folder's items biggest first (from the column's header), a right-click menu,
//! and in Tiles view the sizes under the folders' names.

mod clicks;
mod explorer;
mod overlay;
mod panels;
mod sizes;
mod surface;

use crate::index::Shared;
use clicks::{Event, Kind, Spot};
use explorer::{Layout, Reader, Status};
use overlay::{Cell, Fill, Frame, Mode, Overlay};
use panels::{Act, Card, CardModel, Line, Part, Place, Popup, Strip};
use sizes::{Listing, Sizes, State as Counted};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use surface::{explorer_size, Look};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, GetLastInputInfo, LASTINPUTINFO, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::*;

/// How often the thread looks at Explorer: once a screen refresh, so hovering,
/// selecting and scrolling show up in the column as they do in Explorer. Most
/// looks are a few cheap calls; the full read of the rows is far rarer.
const TICK: Duration = Duration::from_millis(16);
/// While you're using the list, how often the scroll position is asked for.
const SCROLL_EVERY: Duration = Duration::from_millis(30);
/// Scrolling keeps going (smooth scrolling) for a while after the wheel stops.
const SCROLL_TAIL: Duration = Duration::from_millis(600);
/// After scrolling, moving or resizing settles, the rows are read again in full
/// to correct any drift.
const SETTLE: Duration = Duration::from_millis(250);
/// Rows are re-read at least this often, even when nothing seems to change.
const RECHECK: Duration = Duration::from_millis(1500);
/// While Explorer isn't in front, it changes rarely.
const RECHECK_BEHIND: Duration = Duration::from_millis(3000);
/// The folder is looked up again when the window's title or first row changes
/// (a different folder), and at least this often anyway.
const PATH_EVERY: Duration = Duration::from_secs(5);
/// After a click or a key, how long before asking Explorer what's selected.
const SELECT_WAIT: Duration = Duration::from_millis(150);
/// How long the pointer rests on a size before its card shows.
const CARD_WAIT: Duration = Duration::from_millis(450);
/// In the narrow column: the one-line card comes quickly, the full one later.
const PILL_WAIT: Duration = Duration::from_millis(120);
const PILL_THEN_CARD: Duration = Duration::from_millis(900);
/// While sizes are being read from disk, how often the column catches up.
const WALK_EVERY: Duration = Duration::from_millis(250);

/// Turns the column on or off while the app runs.
pub static ON: AtomicBool = AtomicBool::new(true);
/// Sizes are written as a share of the folder ("56%") rather than in bytes.
pub static SHARE: AtomicBool = AtomicBool::new(false);
/// The arrow by a size means "grew this week" rather than "today".
pub static WEEK: AtomicBool = AtomicBool::new(false);
/// Bars are coloured by their share of the folder rather than by their size.
pub static BY_SHARE: AtomicBool = AtomicBool::new(false);
/// For testing: follow this Explorer window whether or not it's in front.
static PIN: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
/// For testing: where the pointer is taken to be.
static POINTER: Mutex<Option<(i32, i32)>> = Mutex::new(None);
/// For testing: things to do to the open pop-up.
static TEST: Mutex<Vec<TestCmd>> = Mutex::new(Vec::new());
static TEST_LOG: Mutex<String> = Mutex::new(String::new());

enum TestCmd {
    /// Rest the pointer on the n-th line.
    Point(usize),
    /// Choose the first line whose action is called this.
    Choose(&'static str),
    Close,
}

/// For testing: keep notes of what was done, and don't wait for a turn to draw.
static TESTING: AtomicBool = AtomicBool::new(false);

fn note(s: String) {
    if TESTING.load(Relaxed) {
        if let Ok(mut l) = TEST_LOG.lock() {
            let _ = writeln!(l, "{s}");
        }
    }
}

/// Starts the column's thread. It runs for as long as the app does.
pub fn start(shared: Arc<Shared>, on: bool, share: bool, week: bool, by_share: bool) {
    ON.store(on, Relaxed);
    SHARE.store(share, Relaxed);
    WEEK.store(week, Relaxed);
    BY_SHARE.store(by_share, Relaxed);
    let _ = std::thread::Builder::new().name("size-column".into()).spawn(move || {
        // One column at a time: Godspeed and LordSpeed both running would draw two.
        // Whichever started first draws it; the other takes over if that one quits.
        let _turn = if TESTING.load(Relaxed) { None } else { wait_turn() }; // tests draw their own
        clicks::start();
        // Setting up can fail while Windows is still starting up: try again a few times.
        for attempt in 0..5 {
            match run(&shared) {
                Ok(()) => return,
                Err(e) => crate::trace::log(format!("size column stopped (try {attempt}): {e}")),
            }
            std::thread::sleep(Duration::from_secs(2 + attempt * 3));
        }
    });
}

/// Explorer's highlight on each row (hover or selection), read off the screen
/// near the row's right end, or None where the row is plain.
///
/// Also says whether Explorer is showing the rows at all: while it loads another
/// folder it clears the list (before its title changes), and the column has to go
/// with it. Told by the item icons, which are never the plain background.
fn highlights(l: &Layout, dpi: u32) -> (Vec<Option<Fill>>, bool) {
    let Some(first) = l.rows.first() else { return (Vec::new(), false) };
    let s = dpi as f32 / 96.0;
    // Near the top of the row, clear of the text, inside the rounded end.
    let x = first.rect.right - (8.0 * s) as i32;
    let dy = (3.0 * s) as i32;
    let px = surface::sample(x, l.header.bottom, l.view.bottom);
    let at = |y: i32| px.get((y - l.header.bottom) as usize).copied();
    let rows: Vec<Option<[u8; 3]>> = l.rows.iter().map(|r| at(r.rect.top + dy)).collect();
    // The list's own background: below the last row when there's room, else the
    // colour most rows have.
    let last = l.rows.iter().map(|r| r.rect.bottom).max().unwrap_or(0);
    let bg = if last + 4 < l.view.bottom - 2 {
        at(l.view.bottom - 3)
    } else {
        let mut counts = std::collections::HashMap::new();
        for c in rows.iter().flatten() {
            *counts.entry(*c).or_insert(0) += 1;
        }
        counts.into_iter().max_by_key(|(_, n)| *n).map(|(c, _)| c)
    };
    let Some(bg) = bg else { return (vec![None; l.rows.len()], true) };
    // Each highlight's exact extent, and its outline (the keyboard focus) if any:
    // the first and last pixels down from the row's top that aren't background.
    let fills: Vec<Option<Fill>> = l
        .rows
        .iter()
        .zip(rows)
        .map(|(r, c)| {
            let color = c.filter(|c| *c != bg)?;
            let (mut top, mut bottom) = (r.rect.top, r.rect.bottom);
            while top > r.rect.top - 3 && at(top - 1).is_some_and(|p| p != bg) {
                top -= 1;
            }
            while top < r.rect.top + 3 && at(top).is_some_and(|p| p == bg) {
                top += 1;
            }
            while bottom < r.rect.bottom + 3 && at(bottom).is_some_and(|p| p != bg) {
                bottom += 1;
            }
            while bottom > r.rect.bottom - 3 && at(bottom - 1).is_some_and(|p| p == bg) {
                bottom -= 1;
            }
            let outline = at(top).filter(|&p| p != color && p != bg);
            Some(Fill { color, outline, top, bottom })
        })
        .collect();
    let icons = surface::sample(first.rect.left + (12.0 * s) as i32, l.header.bottom, l.view.bottom);
    let icon_at = |y: i32| icons.get((y - l.header.bottom) as usize).copied();
    let showing = l.rows.iter().zip(&fills).any(|(r, fill)| {
        icon_at((r.rect.top + r.rect.bottom) / 2).is_some_and(|c| c != bg && Some(c) != fill.map(|f| f.color))
    });
    (fills, showing)
}

/// Waits until no other copy of the app is drawing the column, and holds the
/// right to for as long as this thread runs.
fn wait_turn() -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
    let name = windows::core::w!(r"Local\SpeedExplorerSizeColumn");
    loop {
        // Created by an elevated copy, it can't be opened by the other: that also means "taken".
        if let Ok(m) = unsafe { CreateMutexW(None, false, name) } {
            let r = unsafe { WaitForSingleObject(m, 0) };
            if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
                return Some(m);
            }
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(m);
            }
        }
        std::thread::sleep(Duration::from_secs(10));
    }
}

/// What to draw for one Explorer layout, or None if there's no room for it.
fn frame(
    l: &Layout,
    listing: Option<&Listing>,
    fills: &[Option<Fill>],
    cloud: &HashMap<String, bool>,
    dpi: u32,
    share: bool,
    by_share: bool,
    open: bool,
) -> Option<Frame> {
    let s = dpi as f32 / 96.0;
    let px = |v: f32| (v * s) as i32;
    let item = |name: &str| {
        listing.and_then(|x| x.find(name)).map(|mut i| {
            i.cloud = cloud.get(&name.to_lowercase()).copied().unwrap_or(false);
            i
        })
    };
    let total = listing.map_or(0, |x| x.total);
    if l.tiles {
        // Under each folder's name, where the tile has room (files fill theirs
        // with their type and size already).
        let line = px(16.0);
        let cells: Vec<Cell> = l
            .rows
            .iter()
            .filter_map(|r| {
                let t = r.text?;
                let it = item(&r.name).filter(|i| i.dir)?;
                let top = t.bottom + px(1.0);
                (top + line <= r.rect.bottom).then(|| Cell {
                    name: r.name.clone(),
                    top,
                    bottom: top + line,
                    x: Some((t.left, t.right.min(t.left + px(190.0)))),
                    item: Some(it),
                    fill: None,
                })
            })
            .collect();
        if cells.is_empty() {
            return None;
        }
        return Some(Frame {
            mode: Mode::Tiles,
            left: l.view.left,
            lead: 0,
            top: l.view.top,
            right: l.view.right - px(16.0),
            bottom: l.view.bottom,
            header_bottom: l.view.top,
            cells,
            total,
            share,
            by_share,
            open: false,
            dpi,
        });
    }
    // Leave the scroll bar alone.
    let edge = l.view.right - px(16.0);
    let col = l.cols_right.max(l.view.left) + 1;
    let room = edge - col;
    if room < px(30.0) {
        return None;
    }
    // With room: a bar and the size. Squeezed: a narrow strip of bars alone.
    let (mode, right) = if room >= px(124.0) { (Mode::Wide, edge.min(col + px(196.0))) } else { (Mode::Narrow, edge.min(col + px(56.0))) };
    // Start a little early, over the rounded end of Explorer's row highlight.
    let rows_end = l.rows.iter().map(|r| r.rect.right).min().unwrap_or(col);
    let lead = (col - (rows_end - px(4.0))).clamp(0, px(12.0));
    let cells = l
        .rows
        .iter()
        .enumerate()
        .map(|(i, r)| Cell {
            name: r.name.clone(),
            top: r.rect.top,
            bottom: r.rect.bottom,
            x: None,
            item: item(&r.name),
            fill: fills.get(i).copied().flatten(),
        })
        .collect();
    Some(Frame {
        mode,
        left: col - lead,
        lead,
        top: l.header.top,
        right,
        bottom: l.view.bottom,
        header_bottom: l.header.bottom,
        cells,
        total,
        share,
        by_share,
        open,
        dpi,
    })
}

fn cloaked(h: HWND) -> bool {
    let mut v = 0u32;
    let r = unsafe { DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut v as *mut u32 as *mut _, 4) };
    r.is_ok() && v != 0
}

fn last_input() -> u32 {
    let mut li = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    unsafe {
        let _ = GetLastInputInfo(&mut li);
    }
    li.dwTime
}

fn window_rect(h: HWND) -> RECT {
    let mut r = RECT::default();
    unsafe {
        let _ = GetWindowRect(h, &mut r);
    }
    r
}

fn inside(r: &RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

/// Where the mouse pointer is.
fn pointer() -> POINT {
    if let Some((x, y)) = POINTER.lock().ok().and_then(|p| *p) {
        return POINT { x, y };
    }
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    p
}

/// Another program's window lies over the Explorer window at this point, so
/// what's there isn't the column. Worked out from the stacking order alone:
/// asking Windows which window is at a point can wait on a busy app.
fn covered(top: HWND, p: POINT) -> bool {
    PIN.load(Relaxed) == 0 && covering(top, p).is_some()
}

/// The window lying over the Explorer window at this point, if any.
fn covering(top: HWND, p: POINT) -> Option<HWND> {
    unsafe {
        let mut w = GetWindow(top, GW_HWNDPREV).ok();
        for _ in 0..500 {
            let h = w?;
            if !surface::is_ours(h) && IsWindowVisible(h).as_bool() && !IsIconic(h).as_bool() && inside(&window_rect(h), p) {
                let see_through = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT.0 != 0;
                if !see_through && !cloaked(h) {
                    return Some(h);
                }
            }
            w = GetWindow(h, GW_HWNDPREV).ok();
        }
    }
    None
}

/// Kept online by OneDrive and the like rather than stored on this PC.
fn is_cloud(path: &std::path::Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const DIRECTORY: u32 = 0x10;
    const OFFLINE: u32 = 0x1000;
    const RECALL_ON_OPEN: u32 = 0x4_0000;
    const UNPINNED: u32 = 0x10_0000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    std::fs::symlink_metadata(path).is_ok_and(|m| {
        let a = m.file_attributes();
        let online = OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS;
        a & if a & DIRECTORY != 0 { online | UNPINNED } else { online } != 0
    })
}

/// Puts text on the clipboard.
fn copy_text(text: &str) {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    const CF_UNICODETEXT: u32 = 13;
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(None).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        if let Ok(mem) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
            let p = GlobalLock(mem) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                let _ = GlobalUnlock(mem);
                let _ = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(mem.0)));
            }
        }
        let _ = CloseClipboard();
    }
}

/// What's showing for an Explorer tab.
struct Seen {
    tab: HWND,
    title: String,
    rect: RECT,
    /// The rows as last read in full.
    base: Layout,
    /// The rows as drawn: `base`, moved by scrolling since.
    shown: Layout,
    listing: Option<Arc<Listing>>,
    fills: Vec<Option<Fill>>,
    /// The folder showing.
    path: String,
    /// Explorer's status bar, when it has one.
    status: Option<Status>,
    /// The names of the selected items.
    selected: Vec<String>,
}

/// Where the column was last drawn.
#[derive(Clone, Copy)]
struct Geom {
    right: i32,
    header_bottom: i32,
    mode: Mode,
}

struct State {
    reader: Reader,
    overlay: Overlay,
    strip: Strip,
    card: Card,
    popup: Popup,
    sizes: Sizes,
    /// The Explorer window the column follows.
    target: Option<HWND>,
    /// When the rows were last read in full.
    full_at: Option<Instant>,
    /// Something changed (scrolled, moved, opened another folder) that a full read
    /// should catch up with, once it has settled: when it last changed.
    unsettled: Option<Instant>,
    seen_input: u32,
    /// Keyboard or mouse use, recently: when.
    used_at: Option<Instant>,
    /// Re-read the highlights from the screen for this many more ticks.
    resample: u8,
    scroll_at: Option<Instant>,
    /// Scrolling, last seen: when.
    scrolled_at: Option<Instant>,
    last: Option<Seen>,
    /// The folder showing, and what it was worked out from: the tab, the window
    /// title and the first row's name.
    path: Option<((HWND, String, String), Instant, Option<String>)>,
    look: (Instant, Look),
    geom: Option<Geom>,
    /// Where a click or the resting pointer means something, as last drawn.
    spots: Vec<Spot>,
    /// Which of the folder's items are kept online: the folder, and by lowercase name.
    cloud: (String, HashMap<String, bool>),
    /// The drive showing: its letter, size and free space; and when that was asked.
    free: Option<(char, u64, u64)>,
    free_at: Option<Instant>,
    /// A click or key since the selection was last read: when.
    select_due: Option<Instant>,
    seen_pointer: POINT,
    /// The size the pointer is on, and since when.
    hover: Option<(String, Instant)>,
    /// The card showing: for which item, and whether it's the one-line version.
    card_for: Option<(String, bool)>,
    walk_at: Option<Instant>,
    /// Where the right-click menu was opened.
    menu_at: POINT,
}

fn run(shared: &Shared) -> windows::core::Result<()> {
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // Accessibility calls are made from a free-threaded apartment, as Microsoft
        // advises for clients (a single-threaded one can stall on callbacks).
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        crate::win::low_priority_thread();
        let step = |what: &'static str| move |e: windows::core::Error| windows::core::Error::new(e.code(), format!("{what}: {}", e.message()));
        let mut st = State {
            reader: Reader::new().map_err(step("accessibility"))?,
            overlay: Overlay::new().map_err(step("column window"))?,
            strip: Strip::new().map_err(step("status bar window"))?,
            card: Card::new().map_err(step("card window"))?,
            popup: Popup::new().map_err(step("list window"))?,
            sizes: Sizes::default(),
            target: None,
            full_at: None,
            unsettled: None,
            seen_input: 0,
            used_at: None,
            resample: 0,
            scroll_at: None,
            scrolled_at: None,
            last: None,
            path: None,
            look: (Instant::now(), Look::current()),
            geom: None,
            spots: Vec::new(),
            cloud: (String::new(), HashMap::new()),
            free: None,
            free_at: None,
            select_due: None,
            seen_pointer: POINT::default(),
            hover: None,
            card_for: None,
            walk_at: None,
            menu_at: POINT::default(),
        };
        SetTimer(None, 0, TICK.as_millis() as u32, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_TIMER && msg.hwnd.is_invalid() {
                // A bug here mustn't take the column down for good.
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tick(&mut st, shared)));
                if let Err(p) = r {
                    let what = p.downcast_ref::<String>().cloned().or(p.downcast_ref::<&str>().map(|s| s.to_string()));
                    crate::trace::log(format!("size column: tick panicked: {what:?}"));
                    note(format!("tick panicked: {what:?}"));
                    hide(&mut st);
                }
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// Full reads of the rows are at least this far apart.
const MIN_FULL: Duration = Duration::from_millis(120);

fn hide(st: &mut State) {
    st.overlay.hide();
    st.strip.hide();
    st.card.hide();
    st.popup.close();
    st.last = None;
    st.geom = None;
    st.spots.clear();
    st.hover = None;
    st.card_for = None;
    clicks::clear();
}

/// The line for Explorer's status bar: what's selected, the folder's total and
/// the drive's free space; or, while sizes are being read from disk, how far that is.
fn strip_groups(l: &Seen, free: Option<(char, u64, u64)>) -> Vec<Vec<Part>> {
    let Some(x) = &l.listing else { return Vec::new() };
    let mut groups = Vec::new();
    if let Some((done, of)) = x.progress {
        let permille = if of > 0 { done * 1000 / of } else { 0 };
        groups.push(vec![Part::Strong(format!("Reading folder sizes, {done} of {of} done")), Part::Meter { permille, accent: true }]);
        groups.push(vec![Part::Text(format!("{} so far", explorer_size(x.total)))]);
        return groups;
    }
    // Explorer says a single file's size itself; folders, and several items, it doesn't add up.
    let picked: Vec<sizes::Item> = l.selected.iter().filter_map(|n| x.find(n)).collect();
    let selection = picked.len() >= 2 || picked.iter().any(|i| i.dir);
    if selection {
        groups.push(vec![Part::Strong(format!("{} selected", explorer_size(picked.iter().map(|i| i.size).sum())))]);
    }
    let folder = format!("{} in this folder", explorer_size(x.total));
    groups.push(vec![if selection { Part::Text(folder) } else { Part::Strong(folder) }]);
    if let Some((letter, total, free)) = free.filter(|f| sizes::letter_of(&l.path) == Some(f.0) && f.1 > 0) {
        let permille = (total.saturating_sub(free) as u128 * 1000 / total as u128) as u32;
        groups.push(vec![Part::Meter { permille, accent: false }, Part::Text(format!("{} free on {letter}:", explorer_size(free)))]);
    }
    groups
}

/// Draws what's in `st.last` (if anything).
fn draw(st: &mut State, top: HWND) {
    let Some(l) = &st.last else { return };
    let dpi = unsafe { GetDpiForWindow(top) }.max(96);
    let open = st.popup.open == Some(panels::Kind::List);
    let f = frame(&l.shown, l.listing.as_deref(), &l.fills, &st.cloud.1, dpi, SHARE.load(Relaxed), BY_SHARE.load(Relaxed), open);
    // Where a click or the resting pointer means something: the header, and each size.
    let mut spots = Vec::new();
    match &f {
        Some(f) => {
            let x0 = f.left + f.lead;
            if f.mode != Mode::Tiles {
                spots.push(Spot { rect: RECT { left: x0, right: f.right, top: f.top, bottom: f.header_bottom }, kind: Kind::Header });
            }
            for c in &f.cells {
                let Some(it) = &c.item else { continue };
                let rect = match c.x {
                    Some((a, b)) => RECT { left: a, right: b, top: c.top.max(f.top), bottom: c.bottom.min(f.bottom) },
                    None => RECT { left: x0, right: f.right, top: c.top.max(f.header_bottom), bottom: c.bottom.min(f.bottom) },
                };
                if rect.bottom > rect.top {
                    let path = std::path::Path::new(&l.path).join(&c.name).to_string_lossy().into_owned();
                    spots.push(Spot { rect, kind: Kind::Row { name: c.name.clone(), path, dir: it.dir } });
                }
            }
            st.overlay.show(top, f, st.look.1);
            st.geom = Some(Geom { right: f.right, header_bottom: f.header_bottom, mode: f.mode });
        }
        None => {
            st.overlay.hide();
            st.geom = None;
        }
    }
    if spots.is_empty() {
        clicks::clear();
    } else {
        clicks::set(top, spots.clone());
    }
    st.spots = spots;
    let groups = strip_groups(l, st.free);
    match &l.status {
        Some(s) if !groups.is_empty() => st.strip.show(top, s, &groups, st.look.1, dpi),
        _ => st.strip.hide(),
    }
}

fn since(t: Option<Instant>) -> Duration {
    t.map_or(Duration::MAX, |t| t.elapsed())
}

/// The mouse pointer is over this window's area. (Only by position: asking which
/// window is there can wait on a busy app.)
fn pointer_over(top: HWND) -> bool {
    inside(&window_rect(top), pointer())
}

fn tick(st: &mut State, shared: &Shared) {
    let events = clicks::take();
    if !ON.load(Relaxed) {
        hide(st);
        st.target = None;
        return;
    }
    let fg = unsafe { GetForegroundWindow() };
    let mut front = explorer::is_explorer(fg);
    if front && st.target != Some(fg) {
        st.target = Some(fg);
        hide(st);
    }
    let pin = PIN.load(Relaxed);
    if pin != 0 {
        st.target = Some(HWND(pin as *mut _));
        front = true;
        st.used_at = Some(Instant::now()); // tests act without keyboard or mouse
    }
    let Some(top) = st.target else { return };
    let gone = unsafe { !IsWindow(Some(top)).as_bool() || !IsWindowVisible(top).as_bool() || IsIconic(top).as_bool() };
    if gone || cloaked(top) {
        hide(st);
        if gone {
            st.target = None;
        }
        return;
    }

    // Cheap checks, every tick.
    let title = explorer::title(top);
    let rect = window_rect(top);
    if let Some(l) = &mut st.last {
        if l.title != title {
            // Another folder: Explorer is clearing the list. Out of the way until it's drawn.
            hide(st);
            st.unsettled = Some(Instant::now());
        } else if l.rect != rect {
            let same_size = l.rect.right - l.rect.left == rect.right - rect.left && l.rect.bottom - l.rect.top == rect.bottom - rect.top;
            if same_size {
                // Dragged: move along with it.
                let (dx, dy) = (rect.left - l.rect.left, rect.top - l.rect.top);
                l.base = l.base.moved(dx, dy);
                l.shown = l.shown.moved(dx, dy);
                l.status = l.status.map(|s| s.moved(dx, dy));
                l.rect = rect;
                // What's under the mouse pointer changed with it.
                st.resample = 3;
                st.card.hide();
                st.card_for = None;
                st.popup.close();
            } else {
                // Resized: the columns are moving about. Hidden until it settles.
                hide(st);
            }
            st.unsettled = Some(Instant::now());
        }
    }
    let input = last_input();
    let at = pointer();
    let still = at == st.seen_pointer;
    st.seen_pointer = at;
    if input != st.seen_input {
        st.seen_input = input;
        if front || pointer_over(top) {
            st.used_at = Some(Instant::now());
            // Hover and selection follow the mouse and keys: Explorer repaints within a
            // frame or two, so look again for a few ticks.
            st.resample = 3;
        }
        // A key (the pointer didn't move): the selection may have changed.
        if front && still {
            st.select_due = Some(Instant::now());
        }
    }
    if clicks::CLICKED.swap(false, Relaxed) && front {
        st.select_due = Some(Instant::now());
    }

    // Scrolling: keep the rows moving with the list.
    let active = since(st.used_at) < SCROLL_TAIL || since(st.scrolled_at) < SCROLL_TAIL;
    if st.last.is_some() && front && active && since(st.scroll_at) >= SCROLL_EVERY {
        st.scroll_at = Some(Instant::now());
        let tab = st.last.as_ref().map(|l| l.tab).unwrap();
        let now = st.reader.scroll(tab);
        let l = st.last.as_mut().unwrap();
        if now.is_some() && now != l.shown.scroll {
            st.scrolled_at = Some(Instant::now());
            st.unsettled = Some(Instant::now());
            match now.and_then(|s| l.base.scrolled(s)) {
                Some(moved) => {
                    l.shown = moved;
                    st.resample = st.resample.max(2);
                }
                None => {
                    // Can't be worked out (groups, tiles): out of the way until it stops.
                    hide(st);
                }
            }
        }
    }
    // Highlights, and whether the list is still there, read off the screen: for a
    // few frames after any change, and while you're using the list.
    if st.last.as_ref().is_some_and(|l| !l.shown.tiles) && (st.resample > 0 || since(st.used_at) < SCROLL_TAIL) {
        st.resample = st.resample.saturating_sub(1);
        let dpi = unsafe { GetDpiForWindow(top) }.max(96);
        let l = st.last.as_mut().unwrap();
        let (fills, showing) = highlights(&l.shown, dpi);
        if showing {
            l.fills = fills;
        } else {
            // Explorer cleared the list (another folder is loading).
            hide(st);
            st.unsettled = Some(Instant::now());
        }
    }
    // Sizes being read from disk: show how far they've got.
    if let Some(l) = &mut st.last {
        if l.listing.as_ref().is_some_and(|x| x.progress.is_some()) && since(st.walk_at) >= WALK_EVERY {
            st.walk_at = Some(Instant::now());
            l.listing = st.sizes.listing(shared, &l.path, WEEK.load(Relaxed));
        }
    }
    // What's selected, a moment after a click or a key.
    if st.select_due.is_some_and(|t| t.elapsed() >= SELECT_WAIT) {
        st.select_due = None;
        if let Some(l) = &mut st.last {
            if let Some(names) = st.reader.selected(l.tab) {
                l.selected = names;
            }
            // Explorer's own words in the status bar change with the selection.
            l.status = st.reader.status(l.tab);
        }
    }
    if st.last.is_some() {
        draw(st, top);
    }
    interact(st, shared, top, front, events);

    // A full read of the rows: when there's nothing showing yet, once things have
    // settled after a change, and now and then anyway (sorting, renaming, new files).
    let settled = st.unsettled.is_some_and(|t| t.elapsed() >= SETTLE);
    let routine = since(st.full_at) >= if front { RECHECK } else { RECHECK_BEHIND };
    let want = st.last.is_none() || settled || routine;
    if !want || since(st.full_at) < MIN_FULL || since(st.scrolled_at) < SETTLE {
        return;
    }
    full_read(st, shared, top, title, rect);
}

fn full_read(st: &mut State, shared: &Shared, top: HWND, title: String, rect: RECT) {
    st.full_at = Some(Instant::now());
    st.unsettled = None;
    if st.look.0.elapsed() > Duration::from_secs(2) {
        st.look = (Instant::now(), Look::current());
    }
    let t0 = Instant::now();
    let Some(tab) = explorer::active_tab(top) else { return hide(st) };
    let Some(layout) = st.reader.layout(top, tab) else { return hide(st) };
    if layout.rows.is_empty() && layout.all.is_empty() {
        // Still loading, or an empty folder: nothing to line up with yet.
        return hide(st);
    }
    let t_layout = t0.elapsed();
    let key = (tab, title.clone(), layout.rows.first().map(|r| r.name.clone()).unwrap_or_default());
    let path = match &st.path {
        Some((k, at, p)) if *k == key && at.elapsed() < PATH_EVERY => p.clone(),
        _ => {
            // From the shell's window list; failing that (it can be out of reach when
            // running as administrator), from the title and the names on show.
            let p = explorer::folder_of(top, tab).or_else(|| {
                let names: Vec<String> = layout.rows.iter().take(40).map(|r| r.name.clone()).collect();
                sizes::guess_folder(shared, &title, &names)
            });
            st.path = Some((key, Instant::now(), p.clone()));
            p
        }
    };
    let Some(path) = path else { return hide(st) };
    let listing = st.sizes.listing(shared, &path, WEEK.load(Relaxed));
    let dpi = unsafe { GetDpiForWindow(top) }.max(96);
    let fills = if layout.tiles {
        vec![None; layout.rows.len()]
    } else {
        let (fills, showing) = highlights(&layout, dpi);
        if !showing {
            // Explorer hasn't drawn the rows yet: try again shortly.
            st.unsettled = Some(Instant::now());
            return hide(st);
        }
        fills
    };
    // On a drive of this PC: which of the items on show are kept online, and how
    // full the drive is.
    let local = sizes::letter_of(&path).filter(|&l| !shared.is_network(l));
    if st.cloud.0 != path {
        st.cloud = (path.clone(), HashMap::new());
    }
    match local {
        Some(letter) => {
            for r in &layout.rows {
                let known = &mut st.cloud.1;
                if !known.contains_key(&r.name.to_lowercase()) {
                    known.insert(r.name.to_lowercase(), is_cloud(&std::path::Path::new(&path).join(&r.name)));
                }
            }
            if st.free.is_none_or(|f| f.0 != letter) || since(st.free_at) > Duration::from_secs(15) {
                st.free_at = Some(Instant::now());
                st.free = crate::win::disk_space(letter).map(|(total, free)| (letter, total, free));
            }
        }
        None => st.free = None,
    }
    let status = st.reader.status(tab);
    // The selection carries over while the folder stays; in a folder just opened
    // it's asked for straight away.
    let selected = match &st.last {
        Some(l) if l.tab == tab && l.path == path => l.selected.clone(),
        _ => {
            st.select_due = Some(Instant::now());
            Vec::new()
        }
    };
    st.last = Some(Seen { tab, title, rect, base: layout.clone(), shown: layout, listing, fills, path: path.clone(), status, selected });
    draw(st, top);
    crate::trace::log(format!(
        "size column: read {path} in {:.1} ms (rows {:.1} ms)",
        t0.elapsed().as_secs_f64() * 1000.0,
        t_layout.as_secs_f64() * 1000.0
    ));
}

// ------------------------------------------- clicks, the card and the lists

/// Acts on clicks, keeps the open pop-up going, and shows the card for the size
/// the pointer rests on.
fn interact(st: &mut State, shared: &Shared, top: HWND, front: bool, events: Vec<Event>) {
    if st.last.is_none() {
        st.popup.close();
        st.card.hide();
        st.card_for = None;
        return;
    }
    let list_was_open = st.popup.open == Some(panels::Kind::List);
    for e in events {
        match e {
            Event::Outside => st.popup.close(),
            Event::Left(spot, at) if !covered(top, at) => match spot.kind {
                // A second click on the header closes its list again.
                Kind::Header if list_was_open => st.popup.close(),
                Kind::Header => open_list(st, top),
                Kind::Row { path, dir: true, .. } => crate::tray::request(crate::tray::Signal::Analyze(path)),
                Kind::Row { .. } => {}
            },
            Event::Right(spot, at) if !covered(top, at) => open_menu(st, top, &spot.kind, at),
            _ => {}
        }
    }
    if st.popup.open.is_some() {
        let escape = unsafe { GetAsyncKeyState(VK_ESCAPE.0 as i32) } as u16 & 0x8000 != 0;
        for cmd in TEST.lock().map(|mut t| std::mem::take(&mut *t)).unwrap_or_default() {
            match cmd {
                TestCmd::Point(n) => st.popup.point_at(n),
                TestCmd::Close => st.popup.close(),
                TestCmd::Choose(what) => {
                    let act = (0..40).filter_map(|i| st.popup.act_of(i)).find(|a| format!("{a:?}").starts_with(what));
                    note(format!("chose {act:?}"));
                    st.popup.close();
                    if let Some(a) = act {
                        perform(st, shared, top, a);
                    }
                }
            }
        }
        if !front || escape {
            st.popup.close();
        } else if let Some(act) = st.popup.poll() {
            perform(st, shared, top, act);
        }
    }
    hover(st, shared, top, front);
}

/// The card for the size the pointer has been resting on.
fn hover(st: &mut State, shared: &Shared, top: HWND, front: bool) {
    let at = pointer();
    // Not in Tiles view: Explorer shows its own tip for the tile there.
    let tiles = st.geom.is_some_and(|g| g.mode == Mode::Tiles);
    let quiet = st.popup.open.is_none() && front && !tiles && since(st.scrolled_at) > SCROLL_TAIL;
    let target = st.spots.iter().filter(|_| quiet).find(|s| inside(&s.rect, at)).and_then(|s| match &s.kind {
        Kind::Row { name, .. } => Some((name.clone(), s.rect)),
        Kind::Header => None,
    });
    let Some((name, rect)) = target else {
        st.hover = None;
        st.card_for = None;
        st.card.hide();
        return;
    };
    if st.hover.as_ref().map(|h| &h.0) != Some(&name) {
        st.hover = Some((name.clone(), Instant::now()));
        st.card_for = None;
        st.card.hide();
    }
    let rested = st.hover.as_ref().map_or(Duration::ZERO, |h| h.1.elapsed());
    let narrow = st.geom.is_some_and(|g| g.mode == Mode::Narrow);
    // The narrow column has no room for the size: that comes first, in one line.
    let pill = narrow && rested < PILL_THEN_CARD;
    if rested < if narrow { PILL_WAIT } else { CARD_WAIT } {
        return;
    }
    if st.card_for.as_ref() == Some(&(name.clone(), pill)) && st.card.showing() {
        return;
    }
    if covered(top, at) {
        return;
    }
    match card_model(st, shared, &name, pill) {
        Some(m) => {
            let dpi = unsafe { GetDpiForWindow(top) }.max(96);
            st.card.show(rect, &m, st.look.1, dpi);
            st.card_for = Some((name, pill));
        }
        None => {
            st.card.hide();
            st.card_for = Some((name, pill));
        }
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", crate::fmt::count(n), if n == 1 { one } else { many })
}

/// What the card says about one item of the folder showing. None for what has
/// nothing to add (a file whose size is already on its row).
fn card_model(st: &mut State, shared: &Shared, name: &str, pill: bool) -> Option<CardModel> {
    let l = st.last.as_ref()?;
    let x = l.listing.clone()?;
    let folder = l.path.clone();
    let it = x.find(name)?;
    if !it.dir && !pill {
        return None;
    }
    let mut m = CardModel { pill, ..Default::default() };
    m.title = match it.state {
        Counted::Waiting => "Not counted yet".into(),
        Counted::Counting => format!("{}…", explorer_size(it.size)),
        Counted::Known => explorer_size(it.size),
    };
    if it.state != Counted::Known {
        if it.state == Counted::Counting && !pill {
            m.lines.push("Still being counted".into());
        }
        return Some(m);
    }
    if x.total > 0 && x.progress.is_none() {
        m.side = format!("{} of this folder", surface::share(it.size, x.total));
    }
    if pill {
        return Some(m);
    }
    if let Some(d) = st.sizes.details(shared, &folder, name) {
        m.whole = it.size;
        m.parts = d.top.iter().map(|(n, s)| (n.clone(), explorer_size(*s), *s)).collect();
        if d.files + d.dirs > 0 {
            m.lines.push(match d.dirs {
                0 => plural(d.files, "file", "files"),
                _ => format!("{} in {}", plural(d.files, "file", "files"), plural(d.dirs, "folder", "folders")),
            });
        }
    }
    if st.cloud.1.get(&name.to_lowercase()).copied().unwrap_or(false) {
        m.lines.push("Kept online, not on this PC".into());
    }
    if it.recent > 0 {
        m.warm = Some(format!("{} changed {}", explorer_size(it.recent), if WEEK.load(Relaxed) { "this week" } else { "today" }));
    }
    m.hint = Some("Click the size to open it in Space".into());
    Some(m)
}

/// The folder's items, biggest first, hanging from the column's header.
fn open_list(st: &mut State, top: HWND) {
    let (Some(l), Some(g)) = (&st.last, st.geom) else { return };
    let Some(x) = &l.listing else { return };
    let share = SHARE.load(Relaxed);
    let n = x.sorted.len();
    let mut lines = vec![Line::Title {
        left: "Biggest first".into(),
        right: format!("{}, {}", plural(n as u64, "item", "items"), explorer_size(x.total)),
    }];
    for (i, (name, it)) in x.sorted.iter().take(12).enumerate() {
        lines.push(Line::Ranked {
            n: i + 1,
            name: name.clone(),
            note: String::new(),
            size: overlay::size_text(it, share, x.total),
            frac: if x.total > 0 { it.size as f32 / x.total as f32 } else { 0.0 },
            act: Act::Jump(name.clone()),
        });
    }
    if n > 12 {
        lines.push(Line::Title { left: format!("and {} smaller", plural(n as u64 - 12, "item", "items")), right: String::new() });
    }
    lines.push(Line::Divider);
    lines.push(Line::Footer { note: "Click an item to jump to it".into(), link: "Open in Space".into(), act: Act::Space(l.path.clone()) });
    let dpi = unsafe { GetDpiForWindow(top) }.max(96);
    st.popup.show(panels::Kind::List, lines, Place::Under { right: g.right - 2, top: g.header_bottom + 2 }, 308.0, st.look.1, dpi);
    st.card.hide();
    st.card_for = None;
}

/// The right-click menu: for one item's size (or the whole selection's), and the
/// column's own choices.
fn open_menu(st: &mut State, top: HWND, kind: &Kind, at: POINT) {
    let Some(l) = &st.last else { return };
    let item = |label: &str, right: String, act: Act| Line::Item { label: label.into(), right, check: false, act };
    let mut lines = Vec::new();
    if let (Kind::Row { name, path, dir }, Some(x)) = (kind, &l.listing) {
        // One of several selected items stands for all of them.
        let all = l.selected.len() > 1 && l.selected.iter().any(|s| s.eq_ignore_ascii_case(name));
        let picked: Vec<sizes::Item> = if all { l.selected.iter().filter_map(|n| x.find(n)).collect() } else { x.find(name).into_iter().collect() };
        if !picked.is_empty() && picked.iter().all(|i| i.state == Counted::Known) {
            let size = explorer_size(picked.iter().map(|i| i.size).sum());
            lines.push(item("Copy size", size.clone(), Act::Copy(size)));
        }
        if *dir && !all {
            lines.push(item("Open in Space", String::new(), Act::Space(path.clone())));
            if !x.walked {
                lines.push(item("Show biggest files", String::new(), Act::BiggestFiles(path.clone())));
            }
        }
        if !lines.is_empty() {
            lines.push(Line::Divider);
        }
    }
    lines.push(Line::Item { label: "Show as share of folder".into(), right: String::new(), check: SHARE.load(Relaxed), act: Act::ToggleShare });
    lines.push(item("Highlight growth", if WEEK.load(Relaxed) { "This week" } else { "Today" }.into(), Act::ToggleGrowth));
    lines.push(item("Colour bars by", if BY_SHARE.load(Relaxed) { "Share of folder" } else { "Size" }.into(), Act::ToggleColour));
    lines.push(Line::Divider);
    lines.push(item("Hide this column", String::new(), Act::HideColumn));
    let dpi = unsafe { GetDpiForWindow(top) }.max(96);
    st.menu_at = at;
    st.popup.show(panels::Kind::Menu, lines, Place::At(at), 244.0, st.look.1, dpi);
    st.card.hide();
    st.card_for = None;
}

/// Does what a chosen line of a pop-up stands for.
fn perform(st: &mut State, shared: &Shared, top: HWND, act: Act) {
    let changed = || crate::tray::request(crate::tray::Signal::ColumnPrefs);
    match act {
        Act::Jump(name) => {
            let Some(l) = &st.last else { return };
            // Explorer may be hiding extensions: then it shows the name without its ending.
            let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s);
            let found = l.base.all.iter().position(|n| n.eq_ignore_ascii_case(&name)).or_else(|| l.base.all.iter().position(|n| n.eq_ignore_ascii_case(stem)));
            if let Some(i) = found {
                let ok = st.reader.select(l.tab, i);
                note(format!("jump to {name}: row {i}, {ok}"));
                st.unsettled = Some(Instant::now());
                st.select_due = Some(Instant::now());
            }
        }
        Act::Space(path) => crate::tray::request(crate::tray::Signal::Analyze(path)),
        Act::Reveal(path) => crate::win::reveal(&path),
        Act::Copy(text) => copy_text(&text),
        Act::BiggestFiles(path) => {
            let files = st.sizes.biggest_files(shared, &path, 12);
            let folder = std::path::Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(path.clone());
            // Against the biggest of them: one file is too thin a slice of a whole
            // folder for its share to show.
            let whole = files.first().map_or(0, |f| f.2);
            let mut lines = vec![Line::Title { left: "Biggest files".into(), right: folder }];
            for (i, (full, name, size)) in files.iter().enumerate() {
                // Where under the folder it is: files of one name turn up in many places.
                let inside = full.get(path.len()..).unwrap_or("").trim_start_matches('\\');
                let note = inside.rsplit_once('\\').map_or(String::new(), |(dir, _)| dir.to_string());
                lines.push(Line::Ranked {
                    n: i + 1,
                    name: name.clone(),
                    note,
                    size: explorer_size(*size),
                    frac: if whole > 0 { *size as f32 / whole as f32 } else { 0.0 },
                    act: Act::Reveal(full.clone()),
                });
            }
            if files.is_empty() {
                lines.push(Line::Title { left: "No files in here".into(), right: String::new() });
            }
            lines.push(Line::Divider);
            lines.push(Line::Footer { note: "Click a file to show it in its folder".into(), link: "Open in Space".into(), act: Act::Space(path) });
            let dpi = unsafe { GetDpiForWindow(top) }.max(96);
            st.popup.show(panels::Kind::Menu, lines, Place::At(st.menu_at), 460.0, st.look.1, dpi);
        }
        Act::ToggleShare => {
            SHARE.fetch_xor(true, Relaxed);
            changed();
        }
        Act::ToggleGrowth => {
            WEEK.fetch_xor(true, Relaxed);
            // The sizes are worked out again with the other span.
            st.full_at = None;
            changed();
        }
        Act::ToggleColour => {
            BY_SHARE.fetch_xor(true, Relaxed);
            changed();
        }
        Act::HideColumn => {
            ON.store(false, Relaxed);
            changed();
        }
    }
}

// ------------------------------------------------------------------ testing

/// For testing the real update loop: `--column-live <folder> <out-dir>` opens
/// its own Explorer window on the folder and runs the column on it through its
/// paces (selecting, hovering, the lists, a narrow window, Tiles view), saving a
/// picture after each, then closes it.
pub fn live_test(args: &[String]) {
    let folder = args.first().cloned().unwrap_or_else(|| "C:\\".into());
    let dir = std::path::PathBuf::from(args.get(1).cloned().unwrap_or_else(|| ".".into()));
    let mut log = String::new();
    std::panic::set_hook(Box::new(|info| crate::trace::log(format!("panic: {info}"))));
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let Some(top) = open_explorer(&folder) else {
        let _ = std::fs::write(dir.join("live.txt"), "no new Explorer window appeared");
        return;
    };
    // Leave it time to fill in, then put it on top, at a known size.
    pump(Duration::from_millis(1500));
    let place = |w: i32| unsafe {
        let _ = ShowWindow(top, SW_RESTORE);
        let _ = SetWindowPos(top, Some(HWND_TOPMOST), 120, 80, w, 820, SWP_NOACTIVATE);
        let _ = SetWindowPos(top, Some(HWND_NOTOPMOST), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE);
    };
    place(1300);
    let shared = Shared::new(Box::new(|| {}));
    let letter = folder.chars().next().unwrap_or('C').to_ascii_uppercase();
    let walk = args.get(2).is_some_and(|a| a == "walk");
    if walk {
        let _ = writeln!(log, "sizes come from reading the disk");
    } else if let Some(v) = crate::index::Volume::load_cache(letter) {
        shared.write().put(v);
    } else {
        let _ = writeln!(log, "no saved index for {letter}: sizes come from reading the disk");
    }
    let mut reader = Reader::new().ok();
    let tab = explorer::active_tab(top);
    if let Some(t) = tab {
        let _ = writeln!(log, "view mode {:?}; back to Details: {}", explorer::view_mode(top, t), explorer::set_view_mode(top, t, 4));
    }
    TESTING.store(true, Relaxed);
    PIN.store(top.0 as isize, Relaxed);
    let away =|| *POINTER.lock().unwrap() = Some((-9000, -9000));
    let rest_on = |n: usize| *POINTER.lock().unwrap() = clicks::middle(n).map(|p| (p.x, p.y));
    let tell = |c: TestCmd| TEST.lock().unwrap().push(c);
    away();
    start(shared, true, false, false, false);
    let shot = |name: &str, log: &mut String| match screenshot(top) {
        Some((w, h, px)) => {
            let _ = std::fs::write(dir.join(name), crate::icon::encode_png(w, h, &px));
            let _ = writeln!(log, "saved {name}");
        }
        None => {
            let _ = writeln!(log, "couldn't take {name}");
        }
    };
    pump(Duration::from_millis(900));
    shot("00-early.png", &mut log);
    pump(Duration::from_millis(2600));
    shot("01-start.png", &mut log);
    if let (Some(r), Some(t)) = (reader.as_mut(), tab) {
        let _ = writeln!(log, "scroll: {}", r.scroll_to(t, 40.0));
        pump(Duration::from_millis(1200));
        shot("01b-scrolled.png", &mut log);
        r.scroll_to(t, 0.0);
        pump(Duration::from_millis(1200));
    }

    if let (Some(r), Some(t)) = (reader.as_mut(), tab) {
        let _ = writeln!(log, "select rows 2 and 3: {} {}", r.select(t, 1), r.select_also(t, 2));
        clicks::CLICKED.store(true, Relaxed);
    }
    pump(Duration::from_millis(1200));
    shot("02-selected.png", &mut log);

    rest_on(1);
    pump(Duration::from_millis(1100));
    shot("03-card.png", &mut log);
    away();

    let _ = writeln!(log, "click on the header: {:?}", clicks::fake(0, false));
    pump(Duration::from_millis(500));
    tell(TestCmd::Point(3));
    pump(Duration::from_millis(300));
    shot("04-list.png", &mut log);
    tell(TestCmd::Choose("Jump"));
    pump(Duration::from_millis(1500));
    shot("05-jumped.png", &mut log);

    let _ = writeln!(log, "right-click on the second row: {:?}", clicks::fake(2, true));
    pump(Duration::from_millis(500));
    tell(TestCmd::Point(1));
    pump(Duration::from_millis(300));
    shot("06-menu.png", &mut log);
    tell(TestCmd::Choose("ToggleShare"));
    pump(Duration::from_millis(600));
    shot("07-share.png", &mut log);
    SHARE.store(false, Relaxed);

    clicks::fake(2, true);
    pump(Duration::from_millis(400));
    tell(TestCmd::Choose("ToggleGrowth"));
    pump(Duration::from_millis(2500));
    shot("08-week.png", &mut log);
    WEEK.store(false, Relaxed);

    clicks::fake(2, true);
    pump(Duration::from_millis(400));
    shot("08b-menu-colour.png", &mut log);
    tell(TestCmd::Choose("ToggleColour"));
    pump(Duration::from_millis(900));
    shot("08c-by-share.png", &mut log);
    BY_SHARE.store(false, Relaxed);

    clicks::fake(1, true);
    pump(Duration::from_millis(400));
    tell(TestCmd::Choose("BiggestFiles"));
    pump(Duration::from_millis(900));
    shot("09-biggest-files.png", &mut log);
    tell(TestCmd::Close);
    pump(Duration::from_millis(300));

    // Squeezed: the strip of bars, the one-line card, then the full one.
    place(args.get(3).and_then(|w| w.parse().ok()).unwrap_or(970));
    pump(Duration::from_millis(1500));
    shot("10-narrow.png", &mut log);
    rest_on(1);
    pump(Duration::from_millis(500));
    shot("11-narrow-pill.png", &mut log);
    pump(Duration::from_millis(900));
    shot("12-narrow-card.png", &mut log);
    away();

    // Tiles view, and back (Explorer remembers the view for the folder).
    place(1300);
    if let Some(t) = tab {
        let _ = writeln!(log, "to Tiles: {}", explorer::set_view_mode(top, t, 6));
        pump(Duration::from_millis(2500));
        shot("13-tiles.png", &mut log);
        let _ = writeln!(log, "back to Details: {}", explorer::set_view_mode(top, t, 4));
        pump(Duration::from_millis(1500));
    }
    shot("14-end.png", &mut log);
    PIN.store(0, Relaxed);
    unsafe {
        let _ = PostMessageW(Some(top), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    let _ = writeln!(log, "\n{}", TEST_LOG.lock().map(|l| l.clone()).unwrap_or_default());
    let _ = std::fs::write(dir.join("live.txt"), log);
}

/// For testing with the real mouse: `--column-real <folder> <out-dir>` opens its
/// own Explorer window, then moves the pointer and clicks for a few seconds:
/// resting on a size, the right-click menu and a choice from it, the header's
/// list and a jump from it, and Esc. Pictures are saved after each step.
pub fn real_test(args: &[String]) {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let folder = args.first().cloned().unwrap_or_else(|| "C:\\".into());
    let dir = std::path::PathBuf::from(args.get(1).cloned().unwrap_or_else(|| ".".into()));
    let mut log = String::new();
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let Some(top) = open_explorer(&folder) else {
        let _ = std::fs::write(dir.join("real.txt"), "no new Explorer window appeared");
        return;
    };
    pump(Duration::from_millis(1500));
    unsafe {
        let _ = ShowWindow(top, SW_RESTORE);
        let _ = SetWindowPos(top, None, 120, 80, 1300, 820, SWP_NOZORDER);
    }
    let shared = Shared::new(Box::new(|| {}));
    let letter = folder.chars().next().unwrap_or('C').to_ascii_uppercase();
    if let Some(v) = crate::index::Volume::load_cache(letter) {
        shared.write().put(v);
    }
    TESTING.store(true, Relaxed);
    start(shared, true, false, false, false);
    let mut home = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut home);
    }
    let s = unsafe { GetDpiForWindow(top) }.max(96) as f32 / 96.0;
    let move_to = |x: i32, y: i32| unsafe {
        let _ = SetCursorPos(x, y);
    };
    // Only clicks if the pointer is still where it was put (nobody else moved it).
    let click = |x: i32, y: i32, right: bool, log: &mut String| unsafe {
        let _ = SetCursorPos(x, y);
        std::thread::sleep(Duration::from_millis(80));
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        if (p.x - x).abs() > 2 || (p.y - y).abs() > 2 {
            let _ = writeln!(log, "the pointer was moved: no click at {x},{y}");
            return;
        }
        let (down, up) = if right { (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP) } else { (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP) };
        for flags in [down, up] {
            let input = INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dwFlags: flags, ..Default::default() } } };
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            std::thread::sleep(Duration::from_millis(40));
        }
    };
    let shot = |name: &str, log: &mut String| match screenshot(top) {
        Some((w, h, px)) => {
            let _ = std::fs::write(dir.join(name), crate::icon::encode_png(w, h, &px));
            let _ = writeln!(log, "saved {name}");
        }
        None => {
            let _ = writeln!(log, "couldn't take {name}");
        }
    };
    // A click on its title bar brings the window in front.
    let r = window_rect(top);
    click(r.left + 500, r.top + 14, false, &mut log);
    pump(Duration::from_millis(3000));
    let _ = writeln!(log, "in front: {}", unsafe { GetForegroundWindow() } == top);
    shot("r1-start.png", &mut log);
    let (Some(header), Some(row)) = (clicks::middle(0), clicks::middle(2)) else {
        let _ = writeln!(log, "the column didn't show");
        let _ = std::fs::write(dir.join("real.txt"), log);
        return;
    };
    let over = covering(top, row).map(|h| format!("{} {:?}", explorer::class_of(h), explorer::title(h)));
    let _ = writeln!(log, "over the column at {},{}: {over:?}", row.x, row.y);

    move_to(row.x, row.y);
    pump(Duration::from_millis(1000));
    shot("r2-card.png", &mut log);

    click(row.x, row.y, true, &mut log);
    pump(Duration::from_millis(600));
    shot("r3-menu.png", &mut log);
    // Down the menu to "Show as share of folder": three items, a divider, then it.
    let share = (row.x + (70.0 * s) as i32, row.y + ((5.0 + 96.0 + 11.0 + 16.0) * s) as i32);
    move_to(share.0, share.1);
    pump(Duration::from_millis(400));
    shot("r4-menu-hover.png", &mut log);
    click(share.0, share.1, false, &mut log);
    pump(Duration::from_millis(700));
    shot("r5-share.png", &mut log);
    let _ = writeln!(log, "share on: {}", SHARE.load(Relaxed));
    SHARE.store(false, Relaxed);

    click(header.x, header.y, false, &mut log);
    pump(Duration::from_millis(600));
    shot("r6-list.png", &mut log);
    // The second item in the list under the header.
    let hb = clicks::middle(0).map(|p| p.y).unwrap_or(header.y) + (13.0 * s) as i32;
    let second = (header.x, hb + ((2.0 + 8.0 + 28.0 + 30.0 + 15.0) * s) as i32);
    move_to(second.0, second.1);
    pump(Duration::from_millis(400));
    shot("r7-list-hover.png", &mut log);
    click(second.0, second.1, false, &mut log);
    pump(Duration::from_millis(1200));
    shot("r8-jumped.png", &mut log);

    click(header.x, header.y, false, &mut log);
    pump(Duration::from_millis(500));
    unsafe {
        for flags in [KEYBD_EVENT_FLAGS(0), KEYEVENTF_KEYUP] {
            let input = INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_ESCAPE, dwFlags: flags, ..Default::default() } } };
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            std::thread::sleep(Duration::from_millis(60));
        }
    }
    pump(Duration::from_millis(500));
    shot("r9-after-esc.png", &mut log);
    move_to(home.x, home.y);
    unsafe {
        let _ = PostMessageW(Some(top), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    let _ = writeln!(log, "\n{}", TEST_LOG.lock().map(|l| l.clone()).unwrap_or_default());
    let _ = std::fs::write(dir.join("real.txt"), log);
}

/// For testing: `--column-tree <folder> <out.txt> [view mode]` opens its own
/// Explorer window on the folder (in that layout), and writes down its
/// accessibility tree and where things are.
pub fn tree(args: &[String]) {
    let folder = args.first().cloned().unwrap_or_else(|| "C:\\".into());
    let out = args.get(1).cloned().unwrap_or_else(|| "column-tree.txt".into());
    let mode = args.get(2).and_then(|m| m.parse::<u32>().ok());
    let mut s = String::new();
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let Some(top) = open_explorer(&folder) else {
        let _ = std::fs::write(out, "no new Explorer window appeared");
        return;
    };
    pump(Duration::from_millis(1500));
    if let (Some(tab), Ok(mut reader)) = (explorer::active_tab(top), Reader::new()) {
        let _ = writeln!(s, "view mode {:?}", explorer::view_mode(top, tab));
        if let Some(m) = mode {
            let _ = writeln!(s, "set view mode {m}: {}", explorer::set_view_mode(top, tab, m));
            pump(Duration::from_millis(1200));
        }
        let _ = writeln!(s, "select row 1: {}", reader.select(tab, 0));
        pump(Duration::from_millis(500));
        let _ = writeln!(s, "\nwindow:\n{}", reader.dump_window(top, 9));
        let _ = writeln!(s, "\nitems:\n{}", reader.dump(tab, 3));
    }
    unsafe {
        let _ = PostMessageW(Some(top), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    let _ = std::fs::write(out, s);
}

/// Opens an Explorer window on a folder and waits for it to appear.
fn open_explorer(folder: &str) -> Option<HWND> {
    let explorers = || {
        let mut v = Vec::new();
        let mut after: Option<HWND> = None;
        while let Ok(h) = unsafe { FindWindowExW(None, after, windows::core::w!("CabinetWClass"), None) } {
            v.push(h.0 as isize);
            after = Some(h);
        }
        v
    };
    let before = explorers();
    let _ = std::process::Command::new("explorer.exe").arg(folder).spawn();
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(h) = explorers().into_iter().find(|h| !before.contains(h)) {
            return Some(HWND(h as *mut _));
        }
    }
    None
}

fn pump(d: Duration) {
    let end = Instant::now() + d;
    let mut msg = MSG::default();
    while Instant::now() < end {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// What's on screen where the window is (layered windows included), as RGBA.
fn screenshot(h: HWND) -> Option<(u32, u32, Vec<u8>)> {
    use windows::Win32::Graphics::Gdi::*;
    let r = window_rect(h);
    let (w, ht) = (r.right - r.left, r.bottom - r.top);
    if w <= 0 || ht <= 0 {
        return None;
    }
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, w, ht);
        let old = SelectObject(dc, bmp.into());
        let ok = BitBlt(dc, 0, 0, w, ht, Some(screen), r.left, r.top, SRCCOPY | CAPTUREBLT).is_ok();
        let mut info = BITMAPINFO::default();
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -ht,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut px = vec![0u8; (w * ht * 4) as usize];
        let got = GetDIBits(dc, bmp, 0, ht as u32, Some(px.as_mut_ptr() as *mut _), &mut info, DIB_RGB_COLORS);
        SelectObject(dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        if !ok || got == 0 {
            return None;
        }
        for p in px.chunks_exact_mut(4) {
            p.swap(0, 2);
            p[3] = 255;
        }
        Some((w as u32, ht as u32, px))
    }
}
