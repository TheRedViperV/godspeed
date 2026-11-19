//! The column itself: a see-through, click-through window laid over Explorer's
//! list, kept just above the Explorer window in the stacking order. Painted by
//! hand (bars with soft edges, text with Explorer's font) and handed to Windows
//! as a layered window.

use super::sizes::{Item, State};
use super::surface::{self, explorer_size, Font, Look, Surface};
use std::hash::{Hash, Hasher};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{DT_CENTER, DT_LEFT, DT_RIGHT};
use windows::Win32::UI::WindowsAndMessaging::*;

/// How the sizes are laid out.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Mode {
    /// Details view: a bar and the size on every row.
    Wide,
    /// Details view with little room left: bars only.
    Narrow,
    /// Tiles view: a small bar and the size under each folder's name.
    Tiles,
}

/// One item's size, in screen pixels.
#[derive(Hash)]
pub struct Cell {
    /// The item's name, as Explorer shows it.
    pub name: String,
    pub top: i32,
    pub bottom: i32,
    /// Tiles: where its line runs, left to right.
    pub x: Option<(i32, i32)>,
    /// None: not known (not indexed, and not being counted).
    pub item: Option<Item>,
    /// Explorer's highlight on this row (hovered or selected), carried on across
    /// the column so the row reads as one.
    pub fill: Option<Fill>,
}

/// A row highlight as Explorer drew it: its colour, the outline it has when the
/// row has the keyboard focus, and its exact top and bottom on screen.
#[derive(Clone, Copy, Hash, PartialEq, Debug)]
pub struct Fill {
    pub color: [u8; 3],
    pub outline: Option<[u8; 3]>,
    pub top: i32,
    pub bottom: i32,
}

/// Everything drawn in one go.
#[derive(Hash)]
pub struct Frame {
    pub mode: Mode,
    /// The column's area on screen: from the header's top to the list's bottom.
    pub left: i32,
    /// Pixels at the left that only carry row highlights (over the rounded end of
    /// Explorer's own), before the column proper.
    pub lead: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    /// The header row's bottom, in screen pixels.
    pub header_bottom: i32,
    pub cells: Vec<Cell>,
    /// Everything in the folder: bars and shares are worked out against it, so
    /// a bar shows how much of the folder its item takes up.
    pub total: u64,
    /// Sizes are written as a share of the folder ("56%").
    pub share: bool,
    /// Bars are coloured by their share of the folder rather than by their size.
    pub by_share: bool,
    /// The header is pressed in (its list is open).
    pub open: bool,
    pub dpi: u32,
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_NCHITTEST {
        return LRESULT(HTTRANSPARENT as isize);
    }
    DefWindowProcW(h, msg, wp, lp)
}

pub struct Overlay {
    pub hwnd: HWND,
    owner: Option<HWND>,
    shown: bool,
    drawn: u64,
}

impl Overlay {
    pub fn new() -> windows::core::Result<Overlay> {
        let hwnd = surface::window(w!("GodspeedSizeColumn"), wndproc, WS_EX_TRANSPARENT)?;
        Ok(Overlay { hwnd, owner: None, shown: false, drawn: 0 })
    }

    pub fn hide(&mut self) {
        if self.shown {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.shown = false;
        }
    }

    /// Draws the frame over `owner` (an Explorer window), skipping the work when
    /// nothing changed.
    pub fn show(&mut self, owner: HWND, frame: &Frame, look: Look) {
        if self.owner != Some(owner) {
            self.owner = Some(owner);
            self.drawn = 0;
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        frame.hash(&mut h);
        look.hash(&mut h);
        let sig = h.finish();
        if sig != self.drawn || !self.shown {
            if !paint(self.hwnd, frame, look) {
                self.hide();
                return;
            }
            self.drawn = sig;
        }
        surface::stack(self.hwnd, owner, self.shown);
        self.shown = true;
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// What a row says about its item: the size, or its share of the folder.
pub fn size_text(it: &Item, share: bool, total: u64) -> String {
    match it.state {
        State::Waiting => "–".into(),
        State::Counting => format!("{}…", explorer_size(it.size)),
        State::Known if share => surface::share(it.size, total),
        State::Known => explorer_size(it.size),
    }
}

/// Renders the frame and puts it on screen. False if Windows wouldn't give us a bitmap.
fn paint(hwnd: HWND, f: &Frame, look: Look) -> bool {
    let Some(mut sf) = Surface::new(f.right - f.left, f.bottom - f.top, f.dpi) else { return false };
    match f.mode {
        Mode::Tiles => paint_tiles(&mut sf, f, look),
        _ => paint_rows(&mut sf, f, look),
    }
    sf.present(hwnd, f.left, f.top)
}

/// How heavy an item is, 0 (faint) to 4 (strong): by its size on disk (under
/// 100 MB, 1 GB, 10 GB, 100 GB, or more), or by its share of the folder
/// (under 2%, 10%, 25%, 50%, or more).
pub fn weight(size: u64, whole: u64, by_share: bool) -> usize {
    const MB: u64 = 1 << 20;
    let steps = if by_share { [whole / 50, whole / 10, whole / 4, whole / 2] } else { [100 * MB, 1024 * MB, 10 * 1024 * MB, 100 * 1024 * MB] };
    steps.iter().filter(|&&s| size >= s.max(1)).count()
}

/// Enough of it changed lately to say so: a hundredth of it, or 100 MB.
pub fn grew(it: &Item) -> bool {
    it.state == State::Known && it.recent > 0 && (it.recent.saturating_mul(100) >= it.size || it.recent >= 100 << 20)
}

/// One item's bar, in its groove: the groove is the whole folder, the fill
/// the part of it this item takes up, its colour how heavy the item is.
#[allow(clippy::too_many_arguments)]
fn bar(sf: &mut Surface, it: &Item, x0: f32, x1: f32, cy: f32, whole: u64, by_share: bool, look: Look) {
    let s = sf.s();
    let bh = (6.0 * s).round().max(4.0);
    let (y0, y1) = (cy - bh / 2.0, cy + bh / 2.0);
    let span = x1 - x0;
    if span < 14.0 * s {
        return;
    }
    let mut c = sf.canvas();
    c.groove(x0, y0, x1, y1, look.track, look.dark);
    if it.state == State::Waiting || it.size == 0 || whole == 0 {
        return;
    }
    let fill = (span * (it.size as f32 / whole as f32).min(1.0)).max(bh);
    let color = look.scale[weight(it.size, whole, by_share)];
    match it.state {
        State::Counting => c.dashed(x0, y0, x0 + fill, y1, color, s),
        _ if it.cloud => c.hollow(x0, y0, x0 + fill, y1, color, look.track),
        _ => c.glossy(x0, y0, x0 + fill, y1, color, look.dark),
    }
}

fn paint_rows(sf: &mut Surface, f: &Frame, look: Look) {
    let s = sf.s();
    let w = sf.w;
    let x0 = f.lead as f32;
    let narrow = f.mode == Mode::Narrow;
    let pad = if narrow { 6.0 * s } else { 10.0 * s };
    let text_w = 78.0 * s;

    // Header: a faint divider where the column starts, its mark and its label.
    let hb = (f.header_bottom - f.top) as f32;
    let sq = 7.0 * s;
    {
        let mut c = sf.canvas();
        c.round_rect(x0, 6.0 * s, x0 + 1.0, hb - 6.0 * s, 0.0, look.line, 1.0);
        if f.open {
            c.round_rect(x0 + 4.0 * s, 4.0 * s, w as f32 - 3.0 * s, hb - 4.0 * s, 4.0 * s, look.hover, 1.0);
        }
        c.round_rect(x0 + pad, hb / 2.0 - sq / 2.0, x0 + pad + sq, hb / 2.0 + sq / 2.0, 2.0 * s, look.accent, 0.9);
    }
    if !narrow {
        let arrow = if f.open { (20.0 * s) as i32 } else { 4 };
        let label = RECT { left: (x0 + pad + sq + 6.0 * s) as i32, top: 0, right: w - arrow, bottom: hb as i32 };
        sf.text("Total size", label, DT_LEFT, look.text, Font::Text);
        if f.open {
            let r = RECT { left: w - (22.0 * s) as i32, top: 0, right: w - (6.0 * s) as i32, bottom: hb as i32 };
            sf.text(surface::CHEVRON_DOWN, r, DT_CENTER, look.text, Font::Icon);
        }
    }
    // Rows are cut off at the header, like Explorer's own (partly hidden rows stay partly hidden).
    sf.clip_top = hb as i32;

    for cell in &f.cells {
        let (y0, y1) = ((cell.top - f.top) as f32, (cell.bottom - f.top) as f32);
        let cy = (y0 + y1) / 2.0;
        if let Some(fill) = cell.fill {
            // Square on the left (it continues Explorer's), rounded on the right,
            // with the focus outline carried on too when the row has one.
            let r = 4.0 * s;
            let (fy0, fy1) = ((fill.top - f.top) as f32, (fill.bottom - f.top) as f32);
            let x1 = w as f32 - 2.0 * s;
            let mut c = sf.canvas();
            match fill.outline {
                Some(o) => {
                    c.round_rect(-2.0 * r, fy0, x1, fy1, r, o, 1.0);
                    c.round_rect(-2.0 * r, fy0 + 1.0, x1 - 1.0, fy1 - 1.0, r - 1.0, fill.color, 1.0);
                }
                None => c.round_rect(-2.0 * r, fy0, x1, fy1, r, fill.color, 1.0),
            }
        }
        let row = RECT { left: (w as f32 - pad - text_w) as i32, top: y0 as i32, right: (w as f32 - pad) as i32, bottom: y1 as i32 };
        let Some(it) = &cell.item else {
            if !narrow {
                sf.text("–", row, DT_RIGHT, look.faint, Font::Text);
            }
            continue;
        };
        let bar_x0 = x0 + pad;
        let bar_x1 = if narrow { w as f32 - pad } else { w as f32 - pad - text_w - 8.0 * s };
        bar(sf, it, bar_x0, bar_x1, cy, f.total, f.by_share, look);
        if narrow {
            continue;
        }
        let t = size_text(it, f.share, f.total);
        let color = if it.state == State::Known { look.text } else { look.faint };
        // Grew lately: a rising line in a slot of its own after the bar, so they
        // line up down the column whatever the sizes beside them say.
        let (slot, mark) = ((bar_x1 + 5.0 * s).round(), 11.0 * s);
        if grew(it) {
            sf.canvas().trend(slot, cy.round(), mark, look.warm);
        }
        if it.cloud {
            let right = row.right - sf.measure(&t, Font::Text) - (4.0 * s) as i32;
            let r = RECT { left: right - (14.0 * s) as i32, top: row.top, right, bottom: row.bottom };
            if r.left as f32 >= slot + mark {
                sf.text(surface::CLOUD, r, DT_CENTER, look.faint, Font::Icon);
            }
        }
        sf.text(&t, row, DT_RIGHT, color, Font::Text);
    }
    sf.clip_top = 0;
}

fn paint_tiles(sf: &mut Surface, f: &Frame, look: Look) {
    let s = sf.s();
    for cell in &f.cells {
        let (Some((cx0, cx1)), Some(it)) = (cell.x, &cell.item) else { continue };
        let (x0, x1) = ((cx0 - f.left) as f32, (cx1 - f.left) as f32);
        let (y0, y1) = (cell.top - f.top, cell.bottom - f.top);
        let cy = (y0 + y1) as f32 / 2.0;
        let bar_w = (64.0 * s).min((x1 - x0) * 0.45);
        bar(sf, it, x0, x0 + bar_w, cy, f.total, f.by_share, look);
        let t = size_text(it, f.share, f.total);
        let tw = sf.measure(&t, Font::Text);
        let mut x = (x0 + bar_w + 8.0 * s) as i32;
        let color = if it.state == State::Known { look.text } else { look.faint };
        sf.text(&t, RECT { left: x, top: y0, right: (x + tw + 2).min(x1 as i32), bottom: y1 }, DT_LEFT, color, Font::Text);
        x += tw + (6.0 * s) as i32;
        if it.cloud && x + (14.0 * s) as i32 <= x1 as i32 {
            sf.text(surface::CLOUD, RECT { left: x, top: y0, right: x + (14.0 * s) as i32, bottom: y1 }, DT_CENTER, look.faint, Font::Icon);
            x += (18.0 * s) as i32;
        }
        if grew(it) && x + (14.0 * s) as i32 <= x1 as i32 {
            sf.canvas().trend(x as f32 + 1.0, cy.round(), 11.0 * s, look.warm);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_are_coloured_by_size_or_by_share() {
        const GB: u64 = 1 << 30;
        // By size: the same step whatever the folder holds.
        assert_eq!(weight(50 << 20, 100 * GB, false), 0);
        assert_eq!(weight(500 << 20, 100 * GB, false), 1);
        assert_eq!(weight(5 * GB, 5 * GB, false), 2);
        assert_eq!(weight(12 * GB, 720 * GB, false), 3);
        assert_eq!(weight(250 * GB, 356 * GB, false), 4);
        // By share: the same step whatever the sizes.
        assert_eq!(weight(12 * GB, 720 * GB, true), 0);
        assert_eq!(weight(29 * GB, 356 * GB, true), 1);
        assert_eq!(weight(59 * GB, 356 * GB, true), 2);
        assert_eq!(weight(1400, 2000, true), 4);
        assert_eq!(weight(0, 0, true), 0);
    }

    #[test]
    fn small_changes_in_big_things_go_unmarked() {
        let it = |size, recent| Item { size, recent, ..Default::default() };
        assert!(grew(&it(1000, 10)));
        assert!(!grew(&it(1 << 40, 1 << 20)));
        assert!(grew(&it(1 << 40, 200 << 20)));
        assert!(!grew(&it(1000, 0)));
    }
}
