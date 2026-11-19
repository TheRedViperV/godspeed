//! Reads what an Explorer window is showing: which folder (from the shell's list
//! of open windows), and where its column header and rows sit on screen (from UI
//! Automation, the accessibility interface screen readers use).

use windows::core::{Interface, BSTR, PCWSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, IServiceProvider, CLSCTX_ALL, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::Shell::{
    IFolderView, IPersistFolder2, IShellBrowser, IShellWindows, IWebBrowser2, SHGetPathFromIDListW, ShellWindows,
    SID_STopLevelBrowser,
};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, GetClassNameW, IsWindowVisible};

/// One item row in the list, in screen pixels.
#[derive(Clone)]
pub struct Row {
    pub name: String,
    pub rect: RECT,
    /// Its place in the whole list (`Layout::all`).
    pub index: usize,
    /// Tiles view: where its name is written, when that's its only line (folders).
    pub text: Option<RECT>,
}

/// Explorer's status bar: where it is, and the empty stretch between its text
/// on the left and its buttons on the right.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Status {
    pub rect: RECT,
    pub free: (i32, i32),
}

impl Status {
    pub fn moved(&self, dx: i32, dy: i32) -> Status {
        let r = self.rect;
        Status {
            rect: RECT { left: r.left + dx, right: r.right + dx, top: r.top + dy, bottom: r.bottom + dy },
            free: (self.free.0 + dx, self.free.1 + dx),
        }
    }
}

/// How far the list is scrolled, in percent, and how much of it fits on screen.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Scroll {
    pub v: f64,
    pub h: f64,
    /// The part of the list showing, as a percentage of all of it.
    pub v_size: f64,
}

/// Where things are in the Details view of one Explorer tab.
#[derive(Clone)]
pub struct Layout {
    /// The item list, in screen pixels.
    pub view: RECT,
    /// The column header row.
    pub header: RECT,
    /// Right edge of the last column (the size column goes after it).
    pub cols_right: i32,
    /// The rows on screen.
    pub rows: Vec<Row>,
    /// Every item's name, in the order shown.
    pub all: std::sync::Arc<Vec<String>>,
    /// Rows are evenly spaced and in list order (no groups), so where they'll be
    /// after scrolling can be worked out without asking Explorer.
    pub pitch: Option<i32>,
    pub scroll: Option<Scroll>,
    /// Tiles view (no header, no columns): sizes go under each folder's name.
    pub tiles: bool,
}

impl Layout {
    /// Moved with the window.
    pub fn moved(&self, dx: i32, dy: i32) -> Layout {
        let mv = |r: RECT| RECT { left: r.left + dx, right: r.right + dx, top: r.top + dy, bottom: r.bottom + dy };
        let mut l = self.clone();
        l.view = mv(l.view);
        l.header = mv(l.header);
        l.cols_right += dx;
        for r in &mut l.rows {
            r.rect = mv(r.rect);
            r.text = r.text.map(mv);
        }
        l
    }

    /// The rows as they'll be once the list has scrolled to `now`, worked out from
    /// the even spacing; None when that can't be done (groups, sideways scrolling).
    pub fn scrolled(&self, now: Scroll) -> Option<Layout> {
        let then = self.scroll?;
        let pitch = self.pitch?;
        let first = self.rows.first()?;
        if now.h != then.h || then.v_size <= 0.0 || then.v_size >= 100.0 || pitch <= 0 {
            return None;
        }
        let viewport = (self.view.bottom - self.header.bottom) as f64;
        let range = viewport * 100.0 / then.v_size - viewport;
        let dy = ((now.v - then.v) / 100.0 * range).round() as i64;
        // Where item i is now: the first row's place, moved by whole rows, less the scroll.
        let y = |i: i64| first.rect.top as i64 + (i - first.index as i64) * pitch as i64 - dy;
        let start = first.index as i64 + (self.header.bottom as i64 - y(first.index as i64)).div_euclid(pitch as i64);
        let mut rows = Vec::new();
        let mut i = start.max(0);
        while (i as usize) < self.all.len() && y(i) < self.view.bottom as i64 {
            let top = y(i) as i32;
            rows.push(Row {
                name: self.all[i as usize].clone(),
                rect: RECT { left: first.rect.left, right: first.rect.right, top, bottom: top + (first.rect.bottom - first.rect.top) },
                index: i as usize,
                text: None,
            });
            i += 1;
        }
        Some(Layout { rows, scroll: Some(now), ..self.clone() })
    }
}

pub fn class_of(h: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(h, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..n])
}

/// The window's title: the folder's name in Explorer ("Downloads", "Local Disk (C:)").
pub fn title(h: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(h, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

pub fn is_explorer(h: HWND) -> bool {
    !h.is_invalid() && class_of(h) == "CabinetWClass"
}

/// The tab that's showing: Windows 11 keeps one hidden window per other tab.
pub fn active_tab(top: HWND) -> Option<HWND> {
    let class: Vec<u16> = "ShellTabWindowClass\0".encode_utf16().collect();
    let mut after: Option<HWND> = None;
    for _ in 0..64 {
        let h = unsafe { FindWindowExW(Some(top), after, PCWSTR(class.as_ptr()), PCWSTR::null()) }.ok()?;
        if unsafe { IsWindowVisible(h) }.as_bool() {
            return Some(h);
        }
        after = Some(h);
    }
    None
}

fn bstr(v: &str) -> VARIANT {
    VARIANT::from(BSTR::from(v))
}

pub struct Reader {
    uia: IUIAutomation,
    rows_cache: IUIAutomationCacheRequest,
    rect_cache: IUIAutomationCacheRequest,
    scroll_cache: IUIAutomationCacheRequest,
    is_view: IUIAutomationCondition,
    is_item: IUIAutomationCondition,
    is_header: IUIAutomationCondition,
    is_header_item: IUIAutomationCondition,
    is_property: IUIAutomationCondition,
    is_selected: IUIAutomationCondition,
    is_status: IUIAutomationCondition,
    anything: IUIAutomationCondition,
    name_cache: IUIAutomationCacheRequest,
    /// The item list found last time, for this tab.
    view: Option<(HWND, IUIAutomationElement)>,
    /// The status bar found last time, for this tab.
    status: Option<(HWND, IUIAutomationElement)>,
}

impl Reader {
    /// COM must already be started on this thread.
    pub fn new() -> windows::core::Result<Reader> {
        unsafe {
            let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
            // A busy Explorer mustn't hold the column up for long: give up after a second.
            if let Ok(u2) = uia.cast::<IUIAutomation2>() {
                let _ = u2.SetConnectionTimeout(1000);
                let _ = u2.SetTransactionTimeout(1000);
            }
            let rows_cache = uia.CreateCacheRequest()?;
            rows_cache.AddProperty(UIA_NamePropertyId)?;
            rows_cache.AddProperty(UIA_BoundingRectanglePropertyId)?;
            let rect_cache = uia.CreateCacheRequest()?;
            rect_cache.AddProperty(UIA_BoundingRectanglePropertyId)?;
            let scroll_cache = uia.CreateCacheRequest()?;
            scroll_cache.AddProperty(UIA_ScrollVerticalScrollPercentPropertyId)?;
            scroll_cache.AddProperty(UIA_ScrollHorizontalScrollPercentPropertyId)?;
            scroll_cache.AddProperty(UIA_ScrollVerticalViewSizePropertyId)?;
            let name_cache = uia.CreateCacheRequest()?;
            name_cache.AddProperty(UIA_NamePropertyId)?;
            let ct = |id: UIA_CONTROLTYPE_ID| uia.CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(id.0));
            let picked = uia.CreatePropertyCondition(UIA_SelectionItemIsSelectedPropertyId, &VARIANT::from(true))?;
            Ok(Reader {
                is_view: uia.CreatePropertyCondition(UIA_ClassNamePropertyId, &bstr("UIItemsView"))?,
                is_property: uia.CreatePropertyCondition(UIA_ClassNamePropertyId, &bstr("UIProperty"))?,
                is_selected: uia.CreateAndCondition(&ct(UIA_ListItemControlTypeId)?, &picked)?,
                is_status: uia.CreatePropertyCondition(UIA_ClassNamePropertyId, &bstr("StatusBarModuleInner"))?,
                anything: uia.CreateTrueCondition()?,
                name_cache,
                status: None,
                is_item: ct(UIA_ListItemControlTypeId)?,
                is_header: ct(UIA_HeaderControlTypeId)?,
                is_header_item: uia.CreatePropertyCondition(UIA_ClassNamePropertyId, &bstr("UIColumnHeader"))?,
                rows_cache,
                rect_cache,
                scroll_cache,
                uia,
                view: None,
            })
        }
    }

    fn items_view(&mut self, tab: HWND) -> Option<IUIAutomationElement> {
        if let Some((h, el)) = &self.view {
            if *h == tab && unsafe { el.CurrentBoundingRectangle() }.is_ok_and(|r| r.right > r.left) {
                return Some(el.clone());
            }
        }
        let root = unsafe { self.uia.ElementFromHandle(tab) }.ok()?;
        let el = unsafe { root.FindFirst(TreeScope_Descendants, &self.is_view) }.ok()?;
        self.view = Some((tab, el.clone()));
        Some(el)
    }

    /// How far the list is scrolled: one quick call to Explorer, made often while
    /// you scroll, to keep the column moving with the list.
    pub fn scroll(&mut self, tab: HWND) -> Option<Scroll> {
        let view = match &self.view {
            Some((h, el)) if *h == tab => el.clone(),
            _ => self.items_view(tab)?,
        };
        let cache = self.scroll_cache.clone();
        let read = |el: &IUIAutomationElement| -> Option<Scroll> {
            unsafe {
                let c = el.BuildUpdatedCache(&cache).ok()?;
                let f = |id| c.GetCachedPropertyValue(id).ok().and_then(|v| f64::try_from(&v).ok());
                Some(Scroll {
                    v: f(UIA_ScrollVerticalScrollPercentPropertyId)?,
                    h: f(UIA_ScrollHorizontalScrollPercentPropertyId)?,
                    v_size: f(UIA_ScrollVerticalViewSizePropertyId)?,
                })
            }
        };
        match read(&view) {
            Some(s) => Some(s),
            None => {
                // The list was replaced (another folder): find it again.
                self.view = None;
                read(&self.items_view(tab)?)
            }
        }
    }

    /// Tiles view: every tile, and for those with nothing but a name (folders),
    /// where that name is, so the size can go under it.
    fn tiles(&mut self, tab: HWND, view_el: &IUIAutomationElement, view: RECT) -> Option<Layout> {
        unsafe {
            let scroll = self.scroll(tab);
            let items = view_el.FindAllBuildCache(TreeScope_Descendants, &self.is_item, &self.rows_cache).ok()?;
            let props = view_el.FindAllBuildCache(TreeScope_Descendants, &self.is_property, &self.rect_cache).ok()?;
            let on_screen = |r: &RECT| r.bottom > view.top && r.top < view.bottom && r.bottom > r.top && r.right > r.left;
            let mut lines = Vec::new();
            for i in 0..props.Length().unwrap_or(0) {
                if let Ok(r) = props.GetElement(i).and_then(|p| p.CachedBoundingRectangle()) {
                    if on_screen(&r) {
                        lines.push(r);
                    }
                }
            }
            let n = items.Length().unwrap_or(0).max(0) as usize;
            let mut all = Vec::with_capacity(n);
            let mut rows = Vec::new();
            for i in 0..n {
                let Ok(el) = items.GetElement(i as i32) else { continue };
                let name = el.CachedName().map(|b| b.to_string()).unwrap_or_default();
                all.push(name.clone());
                let Ok(rect) = el.CachedBoundingRectangle() else { continue };
                if !on_screen(&rect) {
                    continue;
                }
                let inside = |r: &&RECT| {
                    let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
                    cx >= rect.left && cx < rect.right && cy >= rect.top && cy < rect.bottom
                };
                let mut own = lines.iter().filter(inside);
                let text = match (own.next(), own.next()) {
                    (Some(r), None) => Some(*r),
                    _ => None,
                };
                rows.push(Row { name, rect, index: all.len() - 1, text });
            }
            let header = RECT { left: view.left, top: view.top, right: view.right, bottom: view.top };
            Some(Layout { view, header, cols_right: view.left, rows, all: std::sync::Arc::new(all), pitch: None, scroll, tiles: true })
        }
    }

    /// Explorer's status bar, if it's showing.
    pub fn status(&mut self, tab: HWND) -> Option<Status> {
        unsafe {
            let cached = match &self.status {
                Some((h, el)) if *h == tab && el.CurrentBoundingRectangle().is_ok_and(|r| r.bottom > r.top) => Some(el.clone()),
                _ => None,
            };
            let el = match cached {
                Some(el) => el,
                None => {
                    // It sits two levels under the tab; looking any deeper would
                    // mean wading through the whole item list when it's switched off.
                    let root = self.uia.ElementFromHandle(tab).ok()?;
                    let kids = root.FindAll(TreeScope_Children, &self.anything).ok()?;
                    let mut found = root.FindFirst(TreeScope_Children, &self.is_status).ok();
                    for i in 0..kids.Length().unwrap_or(0).min(8) {
                        if found.is_some() {
                            break;
                        }
                        found = kids.GetElement(i).ok().and_then(|k| k.FindFirst(TreeScope_Children, &self.is_status).ok());
                    }
                    let el = found?;
                    self.status = Some((tab, el.clone()));
                    el
                }
            };
            let rect = el.CurrentBoundingRectangle().ok()?;
            if rect.bottom <= rect.top || rect.right <= rect.left {
                return None;
            }
            // Text on the left, the view buttons on the right: what's between is free.
            let mid = (rect.left + rect.right) / 2;
            let mut free = (rect.left, rect.right);
            let parts = el.FindAllBuildCache(TreeScope_Children, &self.anything, &self.rect_cache).ok()?;
            for i in 0..parts.Length().unwrap_or(0) {
                let Ok(r) = parts.GetElement(i).and_then(|p| p.CachedBoundingRectangle()) else { continue };
                if r.right <= r.left {
                    continue;
                }
                if (r.left + r.right) / 2 < mid {
                    free.0 = free.0.max(r.right);
                } else {
                    free.1 = free.1.min(r.left);
                }
            }
            Some(Status { rect, free })
        }
    }

    /// The names of the selected items.
    pub fn selected(&mut self, tab: HWND) -> Option<Vec<String>> {
        let view_el = self.items_view(tab)?;
        unsafe {
            let items = view_el.FindAllBuildCache(TreeScope_Descendants, &self.is_selected, &self.name_cache).ok()?;
            let n = items.Length().unwrap_or(0);
            Some((0..n).filter_map(|i| items.GetElement(i).and_then(|e| e.CachedName()).ok()).map(|b| b.to_string()).collect())
        }
    }

    /// Where things are in the tab: the Details view's rows and columns, or the
    /// Tiles view's tiles. None in other views (icons, list…) or when the list
    /// can't be read.
    pub fn layout(&mut self, top: HWND, tab: HWND) -> Option<Layout> {
        let view_el = self.items_view(tab)?;
        unsafe {
            let view = view_el.CurrentBoundingRectangle().ok()?;
            let Ok(header_el) = view_el.FindFirst(TreeScope_Children, &self.is_header) else {
                return if view_mode(top, tab) == Some(6) { self.tiles(tab, &view_el, view) } else { None };
            };
            let header = header_el.CurrentBoundingRectangle().ok()?;
            if header.bottom <= header.top {
                return None;
            }
            let cols = header_el.FindAllBuildCache(TreeScope_Children, &self.is_header_item, &self.rect_cache).ok()?;
            let mut cols_right = header.left;
            for i in 0..cols.Length().unwrap_or(0) {
                if let Ok(r) = cols.GetElement(i).and_then(|c| c.CachedBoundingRectangle()) {
                    cols_right = cols_right.max(r.right);
                }
            }
            // Rows, grouped or not, in the order shown. Off-screen ones are listed too.
            let scroll = self.scroll(tab);
            let items = view_el.FindAllBuildCache(TreeScope_Descendants, &self.is_item, &self.rows_cache).ok()?;
            let n = items.Length().unwrap_or(0).max(0) as usize;
            let mut all = Vec::with_capacity(n);
            let mut rows = Vec::new();
            for i in 0..n {
                let Ok(el) = items.GetElement(i as i32) else { continue };
                let name = el.CachedName().map(|b| b.to_string()).unwrap_or_default();
                all.push(name.clone());
                let Ok(rect) = el.CachedBoundingRectangle() else { continue };
                if rect.bottom <= header.bottom || rect.top >= view.bottom || rect.bottom <= rect.top {
                    continue;
                }
                rows.push(Row { name, rect, index: all.len() - 1, text: None });
            }
            // Evenly spaced, one after another in the list: no groups in the way.
            let pitch = match rows.as_slice() {
                [a, b, ..] => Some(b.rect.top - a.rect.top),
                _ => None,
            }
            .filter(|&p| {
                p > 0
                    && rows.windows(2).all(|w| w[1].index == w[0].index + 1 && (w[1].rect.top - w[0].rect.top - p).abs() <= 1)
                    && all.len() == n
            });
            Some(Layout { view, header, cols_right, rows, all: std::sync::Arc::new(all), pitch, scroll, tiles: false })
        }
    }
}

impl Reader {
    /// Selects the n-th item in the list and brings it into view.
    pub fn select(&mut self, tab: HWND, n: usize) -> bool {
        let Some(view) = self.items_view(tab) else { return false };
        unsafe {
            let Ok(items) = view.FindAll(TreeScope_Descendants, &self.is_item) else { return false };
            let Ok(el) = items.GetElement(n as i32) else { return false };
            let _ = el.GetCurrentPatternAs::<IUIAutomationScrollItemPattern>(UIA_ScrollItemPatternId).and_then(|p| p.ScrollIntoView());
            el.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
                .and_then(|p| p.Select())
                .is_ok()
        }
    }

    /// Adds the n-th item to the selection (for testing).
    pub fn select_also(&mut self, tab: HWND, n: usize) -> bool {
        let Some(view) = self.items_view(tab) else { return false };
        unsafe {
            let Ok(items) = view.FindAll(TreeScope_Descendants, &self.is_item) else { return false };
            let Ok(el) = items.GetElement(n as i32) else { return false };
            el.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
                .and_then(|p| p.AddToSelection())
                .is_ok()
        }
    }

    /// Scrolls the list to a percentage of the way down (for testing).
    pub fn scroll_to(&mut self, tab: HWND, percent: f64) -> bool {
        let Some(view) = self.items_view(tab) else { return false };
        unsafe {
            view.GetCurrentPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId)
                .and_then(|p| p.SetScrollPercent(UIA_ScrollPatternNoScroll, percent))
                .is_ok()
        }
    }

    /// The accessibility tree under the item list, for working out where things are.
    pub fn dump(&mut self, tab: HWND, depth: u32) -> String {
        let mut out = String::new();
        let Some(view) = self.items_view(tab) else { return "no items view".into() };
        let Ok(w) = (unsafe { self.uia.RawViewWalker() }) else { return out };
        let mut n = 0;
        walk(&w, &view, 0, depth, &mut out, &mut n);
        out
    }

    /// The accessibility tree of a whole window (for testing).
    pub fn dump_window(&mut self, top: HWND, depth: u32) -> String {
        let mut out = String::new();
        let Ok(root) = (unsafe { self.uia.ElementFromHandle(top) }) else { return "no window".into() };
        let Ok(w) = (unsafe { self.uia.ControlViewWalker() }) else { return out };
        let mut n = -400;
        walk(&w, &root, 0, depth, &mut out, &mut n);
        out
    }
}

/// The folder a tab is showing, as a file system path. None for places that
/// aren't folders on disk (Home, This PC, Network…).
pub fn folder_of(top: HWND, tab: HWND) -> Option<String> {
    unsafe { path_of(&browser_of(top, tab)?) }
}

/// How the tab lays its items out: 4 is Details, 6 Tiles, 1 icons, 3 List, 8 Content.
pub fn view_mode(top: HWND, tab: HWND) -> Option<u32> {
    unsafe {
        let fv: IFolderView = browser_of(top, tab)?.QueryActiveShellView().ok()?.cast().ok()?;
        fv.GetCurrentViewMode().ok()
    }
}

/// Switches the tab's layout (for testing).
pub fn set_view_mode(top: HWND, tab: HWND, mode: u32) -> bool {
    unsafe {
        let Some(b) = browser_of(top, tab) else { return false };
        let Ok(view) = b.QueryActiveShellView() else { return false };
        view.cast::<IFolderView>().and_then(|fv| fv.SetCurrentViewMode(mode)).is_ok()
    }
}

    fn walk(w: &IUIAutomationTreeWalker, el: &IUIAutomationElement, d: u32, max: u32, out: &mut String, n: &mut i32) {
        if *n > 80 {
            return;
        }
        *n += 1;
        unsafe {
            let ct = el.CurrentControlType().map(|c| c.0).unwrap_or(0);
            let class = el.CurrentClassName().map(|b| b.to_string()).unwrap_or_default();
            let name = el.CurrentName().map(|b| b.to_string()).unwrap_or_default();
            let r = el.CurrentBoundingRectangle().unwrap_or_default();
            out.push_str(&format!(
                "{}{ct} {class:?} {name:?} ({},{})-({},{})
",
                "  ".repeat(d as usize),
                r.left,
                r.top,
                r.right,
                r.bottom
            ));
            if d >= max {
                return;
            }
            let mut c = w.GetFirstChildElement(el).ok();
            let mut k = 0;
            while let Some(ch) = c {
                if k > 12 {
                    out.push_str(&format!("{}…
", "  ".repeat(d as usize + 1)));
                    break;
                }
                walk(w, &ch, d + 1, max, out, n);
                c = w.GetNextSiblingElement(&ch).ok();
                k += 1;
            }
        }
    }

/// The shell's handle on a tab, for asking what it shows.
fn browser_of(top: HWND, tab: HWND) -> Option<IShellBrowser> {
    unsafe {
        let wins: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).ok()?;
        let mut fallback = None;
        let mut same_window = 0;
        for i in 0..wins.Count().ok()? {
            let Ok(disp) = wins.Item(&VARIANT::from(i)) else { continue };
            let Ok(sp) = disp.cast::<IServiceProvider>() else { continue };
            let Ok(browser) = sp.QueryService::<IShellBrowser>(&SID_STopLevelBrowser) else { continue };
            let in_window = disp.cast::<IWebBrowser2>().and_then(|w| w.HWND()).is_ok_and(|h| h.0 as isize == top.0 as isize);
            // Windows 11 tabs: each tab is its own entry, and its browser's window is the tab.
            let exact = browser.GetWindow().is_ok_and(|w| w == tab);
            if !exact && !in_window {
                continue;
            }
            if exact {
                return Some(browser);
            }
            same_window += 1;
            fallback = Some(browser);
        }
        // Older Explorer: one entry per window.
        if same_window == 1 {
            fallback
        } else {
            None
        }
    }
}

unsafe fn path_of(browser: &IShellBrowser) -> Option<String> {
    let view = browser.QueryActiveShellView().ok()?;
    let fv: IFolderView = view.cast().ok()?;
    let pf: IPersistFolder2 = fv.GetFolder().ok()?;
    let pidl = pf.GetCurFolder().ok()?;
    let mut buf = [0u16; 260];
    let ok = SHGetPathFromIDListW(pidl, &mut buf).as_bool();
    CoTaskMemFree(Some(pidl as *const _));
    if !ok {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..n])).filter(|s| !s.is_empty())
}
