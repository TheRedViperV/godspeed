//! The column's other windows: the totals written into Explorer's status bar,
//! the card that shows when the pointer rests on a size, and the pop-up lists
//! (the folder's items biggest first, and the right-click menu).

use super::explorer::Status;
use super::surface::{self, mix, Font, Look, Surface};
use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, DT_CENTER, DT_LEFT, DT_PATH_ELLIPSIS, DT_RIGHT, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

unsafe extern "system" fn through(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_NCHITTEST {
        return LRESULT(HTTRANSPARENT as isize);
    }
    DefWindowProcW(h, msg, wp, lp)
}

fn hide(hwnd: HWND, shown: &mut bool) {
    if *shown {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        *shown = false;
    }
}

/// The part of the screen (the monitor at `p`) that windows may use.
fn work_area(p: POINT) -> RECT {
    unsafe {
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST), &mut mi);
        mi.rcWork
    }
}

/// A raised panel filling the picture but for a margin `m`: soft shadow, outline, fill.
fn panel(sf: &mut Surface, m: f32, look: Look) {
    let s = sf.s();
    let (w, h) = (sf.w as f32, sf.h as f32);
    let r = 8.0 * s;
    let mut c = sf.canvas();
    for k in 1..=8 {
        let k = k as f32 * s;
        c.round_rect(m - k, m - k + 3.0 * s, w - m + k, h - m + k + 3.0 * s, r + k, [0, 0, 0], if look.dark { 0.05 } else { 0.022 });
    }
    c.round_rect(m, m, w - m, h - m, r, look.panel_line, 1.0);
    c.round_rect(m + 1.0, m + 1.0, w - m - 1.0, h - m - 1.0, r - 1.0, look.panel, 1.0);
}

// ------------------------------------------------------------ status bar

/// One piece of the line in the status bar.
#[derive(Hash, Clone, PartialEq)]
pub enum Part {
    Text(String),
    Strong(String),
    /// A small gauge, filled this many thousandths.
    Meter { permille: u32, accent: bool },
}

/// The folder's totals, written into the empty stretch of Explorer's status bar.
pub struct Strip {
    hwnd: HWND,
    shown: bool,
    drawn: u64,
}

impl Strip {
    pub fn new() -> windows::core::Result<Strip> {
        Ok(Strip { hwnd: surface::window(w!("GodspeedSizeStrip"), through, WS_EX_TRANSPARENT)?, shown: false, drawn: 0 })
    }

    pub fn hide(&mut self) {
        hide(self.hwnd, &mut self.shown);
    }

    /// Shows the groups left to right against the right end of the free stretch,
    /// with thin dividers between them; groups that don't fit are left off, last first.
    pub fn show(&mut self, owner: HWND, status: &Status, groups: &[Vec<Part>], look: Look, dpi: u32) {
        let s = dpi as f32 / 96.0;
        let (x0, x1) = (status.free.0 + (16.0 * s) as i32, status.free.1 - (10.0 * s) as i32);
        let (w, h) = (x1 - x0, status.rect.bottom - status.rect.top);
        if groups.is_empty() || w < (80.0 * s) as i32 || h <= 0 {
            return self.hide();
        }
        let mut hs = std::collections::hash_map::DefaultHasher::new();
        (x0, x1, status.rect.top, h, groups, look, dpi).hash(&mut hs);
        let sig = hs.finish();
        if sig != self.drawn || !self.shown {
            let Some(mut sf) = Surface::new(w, h, dpi) else { return self.hide() };
            let (gap, sep, mark, meter) = ((8.0 * s) as i32, (21.0 * s) as i32, (7.0 * s) as i32, (56.0 * s) as i32);
            let mut total = mark + gap;
            let mut keep = 0;
            for g in groups {
                let mut gw = gap * (g.len() as i32 - 1).max(0);
                for p in g {
                    gw += match p {
                        Part::Text(t) => sf.measure(t, Font::Text),
                        Part::Strong(t) => sf.measure(t, Font::Text),
                        Part::Meter { .. } => meter,
                    };
                }
                let add = gw + if keep > 0 { sep } else { 0 };
                if total + add > w {
                    break;
                }
                total += add;
                keep += 1;
            }
            if keep == 0 {
                return self.hide();
            }
            let mut x = w - total;
            let cy = h as f32 / 2.0;
            sf.canvas().round_rect(x as f32, cy - mark as f32 / 2.0, (x + mark) as f32, cy + mark as f32 / 2.0, 2.0 * s, look.accent, 0.9);
            x += mark + gap;
            for (gi, g) in groups.iter().take(keep).enumerate() {
                if gi > 0 {
                    let lx = (x + sep / 2) as f32;
                    sf.canvas().round_rect(lx, cy - 6.0 * s, lx + 1.0, cy + 6.0 * s, 0.0, look.line, 1.0);
                    x += sep;
                }
                for (pi, p) in g.iter().enumerate() {
                    if pi > 0 {
                        x += gap;
                    }
                    match p {
                        Part::Text(t) | Part::Strong(t) => {
                            let tw = sf.measure(t, Font::Text);
                            let color = if matches!(p, Part::Strong(_)) { look.strong } else { look.text };
                            sf.text(t, RECT { left: x, top: 0, right: x + tw + 2, bottom: h }, DT_LEFT, color, Font::Text);
                            x += tw;
                        }
                        Part::Meter { permille, accent } => {
                            let (y0, y1) = (cy - 2.0 * s, cy + 2.0 * s);
                            let mut c = sf.canvas();
                            c.round_rect(x as f32, y0, (x + meter) as f32, y1, 2.0 * s, look.track, 1.0);
                            let fill = meter as f32 * (*permille).min(1000) as f32 / 1000.0;
                            c.round_rect(x as f32, y0, x as f32 + fill.max(3.0 * s), y1, 2.0 * s, if *accent { look.accent } else { look.text }, 1.0);
                            x += meter;
                        }
                    }
                }
            }
            if !sf.present(self.hwnd, x0, status.rect.top) {
                return self.hide();
            }
            self.drawn = sig;
        }
        surface::stack(self.hwnd, owner, self.shown);
        self.shown = true;
    }
}

impl Drop for Strip {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

// -------------------------------------------------------------- hover card

/// What the card says about one item.
#[derive(Hash, Clone, PartialEq, Default)]
pub struct CardModel {
    /// A one-line version (for the narrow column, which has no room for sizes).
    pub pill: bool,
    /// The size, large.
    pub title: String,
    /// "56% of this folder".
    pub side: String,
    /// The biggest things inside, biggest first, and the size they're parts of.
    pub parts: Vec<(String, String, u64)>,
    pub whole: u64,
    pub lines: Vec<String>,
    /// What changed lately.
    pub warm: Option<String>,
    pub hint: Option<String>,
}

pub struct Card {
    hwnd: HWND,
    shown: bool,
    drawn: u64,
}

impl Card {
    pub fn new() -> windows::core::Result<Card> {
        Ok(Card { hwnd: surface::window(w!("GodspeedSizeCard"), through, WS_EX_TRANSPARENT | WS_EX_TOPMOST)?, shown: false, drawn: 0 })
    }

    pub fn hide(&mut self) {
        hide(self.hwnd, &mut self.shown);
    }

    pub fn showing(&self) -> bool {
        self.shown
    }

    /// Shows the card by `anchor` (the item's place in the column, on screen):
    /// under it, or above when there's no room; the one-line version beside it.
    pub fn show(&mut self, anchor: RECT, m: &CardModel, look: Look, dpi: u32) {
        let mut hs = std::collections::hash_map::DefaultHasher::new();
        (anchor.left, anchor.top, anchor.right, anchor.bottom, m, look, dpi).hash(&mut hs);
        let sig = hs.finish();
        if sig == self.drawn && self.shown {
            return;
        }
        let s = dpi as f32 / 96.0;
        let u = |v: f32| (v * s).round() as i32;
        let margin = u(12.0);
        let area = work_area(POINT { x: anchor.right, y: anchor.top });
        let ok = if m.pill { self.pill(anchor, m, look, dpi, margin, area) } else { self.full(anchor, m, look, dpi, margin, area) };
        if !ok {
            return self.hide();
        }
        let _ = u;
        unsafe {
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW);
        }
        self.drawn = sig;
        self.shown = true;
    }

    fn pill(&mut self, anchor: RECT, m: &CardModel, look: Look, dpi: u32, margin: i32, area: RECT) -> bool {
        let s = dpi as f32 / 96.0;
        let u = |v: f32| (v * s).round() as i32;
        let Some(mut probe) = Surface::new(8, 8, dpi) else { return false };
        let (wt, ws) = (probe.measure(&m.title, Font::Bold), probe.measure(&m.side, Font::Text));
        drop(probe);
        let gap = u(8.0);
        let inner = u(10.0) * 2 + wt + if ws > 0 { ws + gap } else { 0 };
        let (w, h) = (inner + margin * 2, u(26.0) + margin * 2);
        let Some(mut sf) = Surface::new(w, h, dpi) else { return false };
        panel(&mut sf, margin as f32, look);
        let mut x = margin + u(10.0);
        let row = |x: i32, tw: i32| RECT { left: x, top: margin, right: x + tw + 2, bottom: h - margin };
        sf.text(&m.title, row(x, wt), DT_LEFT, look.strong, Font::Bold);
        x += wt + gap;
        sf.text(&m.side, row(x, ws), DT_LEFT, look.text, Font::Text);
        // Beside the bar, on its left.
        let left = (anchor.left - u(6.0) - inner - margin).max(area.left - margin);
        let top = (anchor.top + anchor.bottom) / 2 - h / 2;
        sf.present(self.hwnd, left, top)
    }

    fn full(&mut self, anchor: RECT, m: &CardModel, look: Look, dpi: u32, margin: i32, area: RECT) -> bool {
        let s = dpi as f32 / 96.0;
        let u = |v: f32| (v * s).round() as i32;
        let (pad_x, pad_y, line) = (u(16.0), u(13.0), u(19.0));
        let inner_w = u(272.0);
        let detail = !m.lines.is_empty() || m.warm.is_some();
        let mut inner_h = pad_y * 2 + u(30.0);
        if !m.parts.is_empty() {
            inner_h += u(8.0) + u(6.0) + u(9.0) + line * m.parts.len() as i32;
        }
        if detail {
            inner_h += u(9.0) + 1 + u(8.0) + line * (m.lines.len() as i32 + m.warm.is_some() as i32);
        }
        if m.hint.is_some() {
            inner_h += u(5.0) + line;
        }
        let (w, h) = (inner_w + margin * 2, inner_h + margin * 2);
        let Some(mut sf) = Surface::new(w, h, dpi) else { return false };
        panel(&mut sf, margin as f32, look);
        let (x0, x1) = (margin + pad_x, w - margin - pad_x);
        let mut y = margin + pad_y;
        // The size, and its share of the folder.
        let side_w = sf.measure(&m.side, Font::Text);
        sf.text(&m.title, RECT { left: x0, top: y, right: x1 - side_w - u(8.0), bottom: y + u(30.0) }, DT_LEFT, look.strong, Font::Big);
        sf.text(&m.side, RECT { left: x1 - side_w - 2, top: y + u(6.0), right: x1, bottom: y + u(30.0) }, DT_RIGHT, look.text, Font::Text);
        y += u(30.0);
        // Its biggest parts: one bar split by size, then each by name.
        if !m.parts.is_empty() {
            y += u(8.0);
            let shades = [look.accent, mix(look.accent, look.panel, 0.38), mix(look.accent, look.panel, 0.62)];
            let rest = mix(look.track, look.strong, 0.12);
            let span = (x1 - x0) as f32;
            let whole = m.whole.max(1) as f32;
            {
                let mut c = sf.canvas();
                let mut x = x0 as f32;
                let (by0, by1) = (y as f32, (y + u(6.0)) as f32);
                for (i, (_, _, size)) in m.parts.iter().enumerate() {
                    let wd = (span * *size as f32 / whole).min(x1 as f32 - x);
                    if wd >= 1.5 {
                        c.round_rect(x, by0, x + wd - (2.0 * s).min(wd - 1.0), by1, 3.0 * s, shades[i.min(2)], 1.0);
                    }
                    x += wd;
                }
                if x1 as f32 - x >= 2.0 {
                    c.round_rect(x, by0, x1 as f32, by1, 3.0 * s, rest, 1.0);
                }
            }
            y += u(6.0) + u(9.0);
            for (i, (name, size, _)) in m.parts.iter().enumerate() {
                let cy = y as f32 + line as f32 / 2.0;
                let sq = 7.0 * s;
                sf.canvas().round_rect(x0 as f32, cy - sq / 2.0, x0 as f32 + sq, cy + sq / 2.0, 2.0 * s, shades[i.min(2)], 1.0);
                let sw = sf.measure(size, Font::Text);
                sf.text(name, RECT { left: x0 + u(15.0), top: y, right: x1 - sw - u(10.0), bottom: y + line }, DT_LEFT, look.strong, Font::Text);
                sf.text(size, RECT { left: x1 - sw - 2, top: y, right: x1, bottom: y + line }, DT_RIGHT, look.text, Font::Text);
                y += line;
            }
        }
        if detail {
            y += u(9.0);
            sf.canvas().round_rect(x0 as f32, y as f32, x1 as f32, y as f32 + 1.0, 0.0, look.panel_line, 1.0);
            y += 1 + u(8.0);
            for t in &m.lines {
                sf.text(t, RECT { left: x0, top: y, right: x1, bottom: y + line }, DT_LEFT, look.text, Font::Text);
                y += line;
            }
            if let Some(t) = &m.warm {
                let color = if look.dark { mix(look.warm, [255, 255, 255], 0.25) } else { look.warm };
                sf.canvas().trend(x0 as f32, (y as f32 + line as f32 / 2.0).round(), 11.0 * s, color);
                sf.text(t, RECT { left: x0 + u(15.0), top: y, right: x1, bottom: y + line }, DT_LEFT, color, Font::Text);
                y += line;
            }
        }
        if let Some(t) = &m.hint {
            y += u(5.0);
            sf.text(t, RECT { left: x0, top: y, right: x1, bottom: y + line }, DT_LEFT, look.faint, Font::Text);
        }
        // Under the row, its right edge on the column's; above when the screen ends first.
        let left = (anchor.right - inner_w - margin).max(area.left - margin);
        let below = anchor.bottom + u(4.0) - margin;
        let top = if below + h - margin <= area.bottom { below } else { anchor.top - u(4.0) - inner_h - margin };
        sf.present(self.hwnd, left, top)
    }
}

impl Drop for Card {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

// ------------------------------------------------------------ pop-up lists

/// What choosing a line does.
#[derive(Clone, PartialEq, Debug)]
pub enum Act {
    /// Select this item in Explorer and scroll to it.
    Jump(String),
    /// Open a folder in the app's Space view.
    Space(String),
    /// Show a file in its folder.
    Reveal(String),
    /// Put this text on the clipboard.
    Copy(String),
    /// List the biggest files under a folder.
    BiggestFiles(String),
    ToggleShare,
    ToggleGrowth,
    /// Colour the bars by size, or by share of the folder.
    ToggleColour,
    HideColumn,
}

#[derive(Clone)]
pub enum Line {
    /// A caption: no action.
    Title { left: String, right: String },
    Item { label: String, right: String, check: bool, act: Act },
    /// One of a list ordered by size, with a bar behind it (its share of the folder, 0..1).
    /// `note` follows the name, fainter: where a file is.
    Ranked { n: usize, name: String, note: String, size: String, frac: f32, act: Act },
    Divider,
    Footer { note: String, link: String, act: Act },
}

impl Line {
    fn height(&self) -> f32 {
        match self {
            Line::Title { .. } => 28.0,
            Line::Item { .. } => 32.0,
            Line::Ranked { .. } => 30.0,
            Line::Divider => 11.0,
            Line::Footer { .. } => 30.0,
        }
    }

    fn act(&self) -> Option<&Act> {
        match self {
            Line::Item { act, .. } | Line::Ranked { act, .. } | Line::Footer { act, .. } => Some(act),
            _ => None,
        }
    }
}

/// Where a pop-up goes.
pub enum Place {
    /// Hanging under a point, its right edge there.
    Under { right: i32, top: i32 },
    /// Its top left at the pointer.
    At(POINT),
}

enum Input {
    Move(i32, i32),
    Leave,
    Click(i32, i32),
}

thread_local! {
    static INPUT: RefCell<Vec<Input>> = const { RefCell::new(Vec::new()) };
    static TRACKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

const WM_MOUSELEAVE: u32 = 0x02A3;

unsafe extern "system" fn popup_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let xy = || ((lp.0 & 0xffff) as i16 as i32, ((lp.0 >> 16) & 0xffff) as i16 as i32);
    match msg {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_MOUSEMOVE => {
            if !TRACKING.get() {
                let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
                TRACKING.set(TrackMouseEvent(&mut t).is_ok());
            }
            let (x, y) = xy();
            INPUT.with(|q| q.borrow_mut().push(Input::Move(x, y)));
        }
        WM_MOUSELEAVE => {
            TRACKING.set(false);
            INPUT.with(|q| q.borrow_mut().push(Input::Leave));
        }
        WM_LBUTTONUP => {
            let (x, y) = xy();
            INPUT.with(|q| q.borrow_mut().push(Input::Click(x, y)));
        }
        _ => {}
    }
    DefWindowProcW(h, msg, wp, lp)
}

/// Which pop-up is open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// The folder's items, biggest first (from the header).
    List,
    /// The right-click menu, and what it leads to.
    Menu,
}

/// A pop-up list that takes clicks: one at a time.
pub struct Popup {
    hwnd: HWND,
    pub open: Option<Kind>,
    lines: Vec<Line>,
    /// Each line's top and bottom, in the window.
    spans: Vec<(i32, i32)>,
    hover: Option<usize>,
    rect: RECT,
    margin: i32,
    look: Look,
    dpi: u32,
}

impl Popup {
    pub fn new() -> windows::core::Result<Popup> {
        Ok(Popup {
            hwnd: surface::window(w!("GodspeedSizePopup"), popup_proc, WS_EX_TOPMOST)?,
            open: None,
            lines: Vec::new(),
            spans: Vec::new(),
            hover: None,
            rect: RECT::default(),
            margin: 0,
            look: Look::current(),
            dpi: 96,
        })
    }

    pub fn close(&mut self) {
        if self.open.take().is_some() {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            super::clicks::set_popup(None);
            INPUT.with(|q| q.borrow_mut().clear());
        }
    }

    /// Opens with these lines, `width` wide (in design units, before scaling).
    pub fn show(&mut self, kind: Kind, lines: Vec<Line>, place: Place, width: f32, look: Look, dpi: u32) {
        let s = dpi as f32 / 96.0;
        let u = |v: f32| (v * s).round() as i32;
        let margin = u(12.0);
        let pad = u(if kind == Kind::Menu { 5.0 } else { 8.0 });
        let mut y = margin + pad;
        self.spans = lines
            .iter()
            .map(|l| {
                let h = u(l.height());
                y += h;
                (y - h, y)
            })
            .collect();
        let (w, h) = (u(width) + margin * 2, y + pad + margin);
        let at = match place {
            Place::Under { right, top } => POINT { x: right - (w - margin), y: top - margin },
            Place::At(p) => POINT { x: p.x - margin, y: p.y - margin },
        };
        let area = work_area(POINT { x: at.x + w / 2, y: at.y + margin });
        let x = at.x.min(area.right - (w - margin)).max(area.left - margin);
        let y = at.y.min(area.bottom - (h - margin)).max(area.top - margin);
        self.rect = RECT { left: x, top: y, right: x + w, bottom: y + h };
        (self.lines, self.margin, self.look, self.dpi, self.hover) = (lines, margin, look, dpi, None);
        if !self.paint() {
            return self.close();
        }
        unsafe {
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW);
        }
        INPUT.with(|q| q.borrow_mut().clear());
        self.open = Some(kind);
        // Clicks in the shadow around it don't count as inside.
        super::clicks::set_popup(Some(RECT { left: x + margin, top: y + margin, right: x + w - margin, bottom: y + h - margin }));
    }

    fn line_at(&self, x: i32, y: i32) -> Option<usize> {
        let w = self.rect.right - self.rect.left;
        if x < self.margin || x >= w - self.margin {
            return None;
        }
        let i = self.spans.iter().position(|&(t, b)| y >= t && y < b)?;
        self.lines[i].act().map(|_| i)
    }

    /// Follows the pointer; when a line is clicked, closes and says what it does.
    pub fn poll(&mut self) -> Option<Act> {
        self.open?;
        let input: Vec<Input> = INPUT.with(|q| std::mem::take(&mut *q.borrow_mut()));
        let mut hover = self.hover;
        let mut chosen = None;
        for i in input {
            match i {
                Input::Move(x, y) => hover = self.line_at(x, y),
                Input::Leave => hover = None,
                Input::Click(x, y) => chosen = self.line_at(x, y),
            }
        }
        if let Some(i) = chosen {
            let act = self.lines[i].act().cloned();
            self.close();
            return act;
        }
        if hover != self.hover {
            self.hover = hover;
            self.paint();
        }
        None
    }

    /// For testing: acts as if the pointer were on the n-th line.
    pub fn point_at(&mut self, n: usize) {
        if self.open.is_some() && n < self.lines.len() {
            self.hover = Some(n);
            self.paint();
        }
    }

    /// For testing: what choosing the n-th line would do.
    pub fn act_of(&self, n: usize) -> Option<Act> {
        self.lines.get(n).and_then(|l| l.act().cloned())
    }

    fn paint(&mut self) -> bool {
        let (w, h) = (self.rect.right - self.rect.left, self.rect.bottom - self.rect.top);
        let Some(mut sf) = Surface::new(w, h, self.dpi) else { return false };
        let look = self.look;
        let s = sf.s();
        let u = |v: f32| (v * s).round() as i32;
        let m = self.margin;
        panel(&mut sf, m as f32, look);
        let inset = u(5.0);
        let (x0, x1) = (m + inset, w - m - inset);
        let (tx0, tx1) = (x0 + u(10.0), x1 - u(10.0));
        for (i, l) in self.lines.iter().enumerate() {
            let (top, bottom) = self.spans[i];
            let row = |left: i32, right: i32| RECT { left, top, right, bottom };
            let lit = self.hover == Some(i);
            let glow = |sf: &mut Surface| {
                if lit {
                    sf.canvas().round_rect(x0 as f32, top as f32 + 1.0, x1 as f32, bottom as f32 - 1.0, 4.0 * s, look.hover, 1.0);
                }
            };
            match l {
                Line::Title { left, right } => {
                    let rw = sf.measure(right, Font::Text);
                    sf.text(left, row(tx0, tx1 - rw - u(8.0)), DT_LEFT, look.text, Font::Text);
                    sf.text(right, row(tx1 - rw - 2, tx1), DT_RIGHT, look.text, Font::Text);
                }
                Line::Item { label, right, check, .. } => {
                    glow(&mut sf);
                    let rw = if *check { u(16.0) } else { sf.measure(right, Font::Text) };
                    sf.text(label, row(tx0, tx1 - rw - u(8.0)), DT_LEFT, look.strong, Font::Text);
                    if *check {
                        sf.text(surface::CHECK, row(tx1 - rw, tx1), DT_CENTER, look.accent, Font::Icon);
                    } else {
                        sf.text(right, row(tx1 - rw - 2, tx1), DT_RIGHT, look.text, Font::Text);
                    }
                }
                Line::Ranked { n, name, note, size, frac, .. } => {
                    glow(&mut sf);
                    let span = (x1 - x0) as f32 * frac.clamp(0.0, 1.0);
                    if span >= 2.0 {
                        sf.canvas().round_rect(x0 as f32, top as f32 + 3.0 * s, x0 as f32 + span, bottom as f32 - 3.0 * s, 4.0 * s, look.accent, 0.18);
                    }
                    let color = if *n <= 3 { look.accent } else { look.faint };
                    sf.text(&n.to_string(), row(tx0 - u(2.0), tx0 + u(16.0)), DT_CENTER, color, Font::SmallBold);
                    let sw = sf.measure(size, Font::Text);
                    let (left, right) = (tx0 + u(24.0), tx1 - sw - u(10.0));
                    sf.text(name, row(left, right), DT_LEFT, look.strong, Font::Text);
                    let after = left + sf.measure(name, Font::Text) + u(10.0);
                    if !note.is_empty() && right - after > u(40.0) {
                        sf.text(note, row(after, right), DT_LEFT | DT_PATH_ELLIPSIS, look.faint, Font::Text);
                    }
                    sf.text(size, row(tx1 - sw - 2, tx1), DT_RIGHT, look.text, Font::Text);
                }
                Line::Divider => {
                    let y = (top + bottom) as f32 / 2.0;
                    sf.canvas().round_rect(tx0 as f32 - 4.0 * s, y, tx1 as f32 + 4.0 * s, y + 1.0, 0.0, look.panel_line, 1.0);
                }
                Line::Footer { note, link, .. } => {
                    glow(&mut sf);
                    let lw = sf.measure(link, Font::Text);
                    sf.text(note, row(tx0, tx1 - lw - u(10.0)), DT_LEFT, look.faint, Font::Text);
                    sf.text(link, row(tx1 - lw - 2, tx1), DT_RIGHT, look.accent, Font::Text);
                }
            }
        }
        sf.present(self.hwnd, self.rect.left, self.rect.top)
    }
}

impl Drop for Popup {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
