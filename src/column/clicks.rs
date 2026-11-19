//! Clicks on the column. The column lets every left click through to Explorer (so
//! rows still select as usual); a low-level mouse hook just notices the clicks
//! that land on a size or on the column's header, and passes them on to the
//! column's own thread to act on. Right clicks on the column are kept from
//! Explorer, so the column's own menu opens rather than the file's.

use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::*;

/// What's under a spot.
#[derive(Clone, PartialEq, Debug)]
pub enum Kind {
    /// The column's header.
    Header,
    /// One item's size.
    Row { name: String, path: String, dir: bool },
}

/// A place on screen where a click means something.
#[derive(Clone, Debug)]
pub struct Spot {
    pub rect: RECT,
    pub kind: Kind,
}

/// The spots showing now, and the Explorer window they lie over.
pub struct Zones {
    pub owner: isize,
    pub spots: Vec<Spot>,
}

pub enum Event {
    Left(Spot, POINT),
    Right(Spot, POINT),
    /// A click or a turn of the wheel somewhere other than the open pop-up.
    Outside,
}

static ZONES: Mutex<Option<Zones>> = Mutex::new(None);
/// The pop-up list that's open, if any: clicks inside it are its own business.
static POPUP: Mutex<Option<RECT>> = Mutex::new(None);
static EVENTS: Mutex<Vec<Event>> = Mutex::new(Vec::new());
/// Pressed on a spot, waiting for the release on the same one.
static DOWN: Mutex<Option<(usize, Instant)>> = Mutex::new(None);
/// A right press that was kept from Explorer: its release is kept too.
static RIGHT_DOWN: Mutex<Option<Option<usize>>> = Mutex::new(None);
/// A mouse button was released somewhere (the selection may have changed).
pub static CLICKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn inside(r: &RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

/// The spot under `p`, by position alone. Called from the hook, so nothing here
/// may wait or talk to other windows.
fn spot_index(p: POINT) -> Option<usize> {
    let zones = ZONES.try_lock().ok()?;
    zones.as_ref()?.spots.iter().position(|s| inside(&s.rect, p))
}

fn spot(i: usize) -> Option<Spot> {
    ZONES.try_lock().ok()?.as_ref()?.spots.get(i).cloned()
}

fn push(e: Event) {
    if let Ok(mut q) = EVENTS.try_lock() {
        if q.len() < 64 {
            q.push(e);
        }
    }
}

/// The column's Explorer window is the one being used.
fn owner_in_front() -> bool {
    let owner = ZONES.try_lock().ok().and_then(|z| z.as_ref().map(|z| z.owner));
    owner.is_some_and(|o| unsafe { GetForegroundWindow() }.0 as isize == o)
}

unsafe extern "system" fn hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // Every mouse event on the PC passes through here: only quick checks, and
    // anything more on the column's own thread.
    if code >= 0 {
        let info = &*(lp.0 as *const MSLLHOOKSTRUCT);
        let msg = wp.0 as u32;
        let popup = POPUP.try_lock().ok().and_then(|p| *p);
        let in_popup = popup.is_some_and(|r| inside(&r, info.pt));
        if popup.is_some() && !in_popup && matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_MOUSEWHEEL) {
            push(Event::Outside);
        }
        if !in_popup {
            match msg {
                WM_LBUTTONDOWN => {
                    if let Ok(mut d) = DOWN.try_lock() {
                        *d = spot_index(info.pt).map(|i| (i, Instant::now()));
                    }
                }
                WM_LBUTTONUP => {
                    CLICKED.store(true, std::sync::atomic::Ordering::Relaxed);
                    let down = DOWN.try_lock().ok().and_then(|mut d| d.take());
                    if let (Some((i, at)), Some(j)) = (down, spot_index(info.pt)) {
                        if i == j && at.elapsed() < Duration::from_millis(700) {
                            if let Some(s) = spot(i) {
                                push(Event::Left(s, info.pt));
                            }
                        }
                    }
                }
                WM_RBUTTONDOWN => {
                    if let Some(i) = spot_index(info.pt).filter(|_| owner_in_front()) {
                        if let Ok(mut d) = RIGHT_DOWN.try_lock() {
                            *d = Some(Some(i));
                            return LRESULT(1);
                        }
                    }
                }
                WM_RBUTTONUP => {
                    CLICKED.store(true, std::sync::atomic::Ordering::Relaxed);
                    let down = RIGHT_DOWN.try_lock().ok().and_then(|mut d| d.take());
                    if let Some(i) = down {
                        if let (Some(i), Some(j)) = (i, spot_index(info.pt)) {
                            if i == j {
                                if let Some(s) = spot(i) {
                                    push(Event::Right(s, info.pt));
                                }
                            }
                        }
                        return LRESULT(1);
                    }
                }
                _ => {}
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

/// Starts watching for clicks, on a thread of its own that does nothing else.
pub fn start() {
    let _ = std::thread::Builder::new().name("size-column-clicks".into()).spawn(|| unsafe {
        let Ok(h) = SetWindowsHookExW(WH_MOUSE_LL, Some(hook), None, 0) else { return };
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        let _ = UnhookWindowsHookEx(h);
    });
}

/// Nothing to click (the column is hidden).
pub fn clear() {
    if let Ok(mut z) = ZONES.lock() {
        *z = None;
    }
}

pub fn set(owner: HWND, spots: Vec<Spot>) {
    if let Ok(mut z) = ZONES.lock() {
        *z = Some(Zones { owner: owner.0 as isize, spots });
    }
}

/// Tells the hook where the open pop-up is (None: there isn't one).
pub fn set_popup(rect: Option<RECT>) {
    if let Ok(mut p) = POPUP.lock() {
        *p = rect;
    }
}

/// The clicks since last asked.
pub fn take() -> Vec<Event> {
    EVENTS.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
}

/// For testing: acts as if the n-th spot was clicked, in its middle.
pub fn fake(n: usize, right: bool) -> Option<Kind> {
    let s = ZONES.lock().ok()?.as_ref()?.spots.get(n).cloned()?;
    let p = POINT { x: (s.rect.left + s.rect.right) / 2, y: (s.rect.top + s.rect.bottom) / 2 };
    let kind = s.kind.clone();
    if let Ok(mut q) = EVENTS.lock() {
        q.push(if right { Event::Right(s, p) } else { Event::Left(s, p) });
    }
    Some(kind)
}

/// For testing: the middle of the n-th spot.
pub fn middle(n: usize) -> Option<POINT> {
    let z = ZONES.lock().ok()?;
    let s = z.as_ref()?.spots.get(n)?;
    Some(POINT { x: (s.rect.left + s.rect.right) / 2, y: (s.rect.top + s.rect.bottom) / 2 })
}
