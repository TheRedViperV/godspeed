//! Drawing shared by the column's windows (the column itself, the line in the
//! status bar, the hover card and the pop-up lists): colours taken from Windows,
//! a pixel canvas with soft-edged shapes, text in Explorer's font, and see-through
//! windows to show the result in.

use std::cell::RefCell;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Explorer's colours, light or dark, plus the Windows accent colour.
#[derive(Clone, Copy, Hash, PartialEq)]
pub struct Look {
    pub dark: bool,
    /// Explorer's own detail text (dates, sizes).
    pub text: [u8; 3],
    /// Names and headlines.
    pub strong: [u8; 3],
    /// Hints, and sizes not known yet.
    pub faint: [u8; 3],
    pub line: [u8; 3],
    pub track: [u8; 3],
    pub accent: [u8; 3],
    pub warm: [u8; 3],
    /// Bar colours, from the faint one for small things to the strong one for big.
    pub scale: [[u8; 3]; 5],
    /// Cards and pop-up lists: their background, outline, and the row under the pointer.
    pub panel: [u8; 3],
    pub panel_line: [u8; 3],
    pub hover: [u8; 3],
}

fn reg_dword(key: PCWSTR, value: PCWSTR) -> Option<u32> {
    let mut v = 0u32;
    let mut n = 4u32;
    let r = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key, value, RRF_RT_REG_DWORD, None, Some(&mut v as *mut u32 as *mut _), Some(&mut n))
    };
    r.is_ok().then_some(v)
}

pub fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    [l(a[0], b[0]), l(a[1], b[1]), l(a[2], b[2])]
}

impl Look {
    /// Read from the current Windows settings (cheap: two registry values).
    pub fn current() -> Look {
        let light = reg_dword(w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"), w!("AppsUseLightTheme"))
            .is_none_or(|v| v != 0);
        // For testing the other look without changing Windows' setting.
        let dark = match std::env::var("SPEED_LOOK").as_deref() {
            Ok("light") => false,
            Ok("dark") => true,
            _ => !light,
        };
        // Stored as 0xAABBGGRR.
        let accent = reg_dword(w!("Software\\Microsoft\\Windows\\DWM"), w!("AccentColor"))
            .map(|v| [v as u8, (v >> 8) as u8, (v >> 16) as u8])
            .unwrap_or([0x00, 0x67, 0xc0]);
        if dark {
            Look {
                dark,
                text: [0xcf, 0xcf, 0xcf],
                strong: [0xf2, 0xf2, 0xf2],
                faint: [0xa3, 0xa3, 0xa3],
                line: [0x3b, 0x3b, 0x3b],
                track: [0x3a, 0x3a, 0x3a],
                // Explorer uses a lighter shade of the accent on dark backgrounds.
                accent: mix(accent, [0xff, 0xff, 0xff], 0.45),
                warm: [0xf0, 0x8a, 0x52],
                scale: [[0x8a, 0x9d, 0xb3], [0x7f, 0xb6, 0xee], [0x3f, 0x93, 0xf0], [0xf2, 0xa6, 0x2e], [0xf0, 0x62, 0x3c]],
                panel: [0x2b, 0x2b, 0x2b],
                panel_line: [0x3d, 0x3d, 0x3d],
                hover: [0x3a, 0x3a, 0x3a],
            }
        } else {
            Look {
                dark,
                text: [0x5f, 0x5f, 0x5f],
                strong: [0x1b, 0x1b, 0x1b],
                faint: [0x6b, 0x6b, 0x6b],
                line: [0xe5, 0xe5, 0xe5],
                track: [0xe5, 0xe5, 0xe5],
                accent,
                warm: [0xc4, 0x56, 0x1b],
                scale: [[0x9f, 0xb3, 0xc8], [0x6f, 0xa8, 0xe6], [0x1f, 0x74, 0xd0], [0xd9, 0x88, 0x12], [0xd0, 0x4a, 0x22]],
                panel: [0xfb, 0xfb, 0xfb],
                panel_line: [0xdc, 0xdc, 0xdc],
                hover: [0xec, 0xec, 0xec],
            }
        }
    }
}

/// Premultiplied BGRA pixels.
pub struct Canvas<'a> {
    pub w: i32,
    pub h: i32,
    pub px: &'a mut [u32],
    /// Nothing is drawn above this row (rows scrolled under the header).
    pub clip_top: i32,
}

impl Canvas<'_> {
    pub fn blend(&mut self, x: i32, y: i32, c: [u8; 3], a: f32) {
        if a <= 0.0 || x < 0 || y < self.clip_top || x >= self.w || y >= self.h {
            return;
        }
        let i = (y * self.w + x) as usize;
        let d = self.px[i];
        let a = a.min(1.0);
        let k = 1.0 - a;
        let ch = |shift: u32, src: u8| ((src as f32 * a) + ((d >> shift) & 0xff) as f32 * k).round() as u32;
        self.px[i] = (ch(24, 255) << 24) | (ch(16, c[0]) << 16) | (ch(8, c[1]) << 8) | ch(0, c[2]);
    }

    /// A rounded rectangle shaded from one colour at the top to another at the bottom.
    #[allow(clippy::too_many_arguments)]
    pub fn round_rect_v(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, r: f32, top: [u8; 3], bottom: [u8; 3], alpha: f32) {
        let h = (y1 - y0).max(1.0);
        for y in y0.floor() as i32..y1.ceil() as i32 {
            let t = ((y as f32 + 0.5 - y0) / h).clamp(0.0, 1.0);
            let c = mix(top, bottom, t);
            // One row at a time, clipped to the shape.
            self.round_rect_rows(x0, y0, x1, y1, r, c, alpha, y);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn round_rect_rows(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, r: f32, c: [u8; 3], alpha: f32, y: i32) {
        let r = r.min((x1 - x0) / 2.0).min((y1 - y0) / 2.0);
        for x in x0.floor() as i32..x1.ceil() as i32 {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let dx = (x0 + r - px).max(px - (x1 - r)).max(0.0);
            let dy = (y0 + r - py).max(py - (y1 - r)).max(0.0);
            let outside = (dx * dx + dy * dy).sqrt() - r;
            let inside_x = (px - x0 + 0.5).min(x1 - px + 0.5).clamp(0.0, 1.0);
            let inside_y = (py - y0 + 0.5).min(y1 - py + 0.5).clamp(0.0, 1.0);
            let cov = (0.5 - outside).clamp(0.0, 1.0) * inside_x * inside_y;
            self.blend(x, y, c, cov * alpha);
        }
    }

    /// A raised, glossy bar (the look of macOS's storage bars): lit from above,
    /// a sheen across its top half and a soft shadow underneath.
    pub fn glossy(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, c: [u8; 3], dark: bool) {
        let r = (y1 - y0) / 2.0;
        self.round_rect(x0, y0 + 0.6, x1, y1 + 1.4, r, [0, 0, 0], if dark { 0.45 } else { 0.18 });
        self.round_rect_v(x0, y0, x1, y1, r, mix(c, [255, 255, 255], 0.30), mix(c, [0, 0, 0], 0.10), 1.0);
        let sheen = y0 + (y1 - y0) * 0.48;
        self.round_rect_v(x0 + r * 0.5, y0 + 0.5, x1 - r * 0.5, sheen, r * 0.6, [255, 255, 255], [255, 255, 255], 0.22);
    }

    /// The groove a bar sits in: shaded darker at the top, as if pressed in.
    pub fn groove(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, c: [u8; 3], dark: bool) {
        let r = (y1 - y0) / 2.0;
        let (top, bottom) = if dark {
            (mix(c, [0, 0, 0], 0.45), mix(c, [255, 255, 255], 0.05))
        } else {
            (mix(c, [0, 0, 0], 0.16), mix(c, [255, 255, 255], 0.35))
        };
        self.round_rect_v(x0, y0, x1, y1, r, top, bottom, 1.0);
    }

    /// An outlined, empty bar: for what isn't stored on this PC.
    pub fn hollow(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, c: [u8; 3], inside: [u8; 3]) {
        let r = (y1 - y0) / 2.0;
        self.round_rect(x0, y0, x1, y1, r, c, 1.0);
        self.round_rect(x0 + 1.0, y0 + 1.0, x1 - 1.0, y1 - 1.0, r - 1.0, inside, 1.0);
    }

    /// A bar in short dashes: still being counted.
    pub fn dashed(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, c: [u8; 3], s: f32) {
        let (on, off) = (4.0 * s, 3.0 * s);
        let mut x = x0;
        while x < x1 {
            self.round_rect(x, y0, (x + on).min(x1), y1, 1.5 * s, c, 0.9);
            x += on + off;
        }
    }

    /// Straight strokes with soft edges, drawn as one shape (joints aren't doubled).
    pub fn stroke(&mut self, segs: &[((f32, f32), (f32, f32))], width: f32, c: [u8; 3]) {
        let h = width / 2.0;
        let ends = || segs.iter().flat_map(|s| [s.0, s.1]);
        let (x0, x1) = ends().fold((f32::MAX, f32::MIN), |m, p| (m.0.min(p.0), m.1.max(p.0)));
        let (y0, y1) = ends().fold((f32::MAX, f32::MIN), |m, p| (m.0.min(p.1), m.1.max(p.1)));
        for y in (y0 - h - 1.0).floor() as i32..=(y1 + h + 1.0).ceil() as i32 {
            for x in (x0 - h - 1.0).floor() as i32..=(x1 + h + 1.0).ceil() as i32 {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let d = segs
                    .iter()
                    .map(|&((ax, ay), (bx, by))| {
                        let (vx, vy) = (bx - ax, by - ay);
                        let t = (((px - ax) * vx + (py - ay) * vy) / (vx * vx + vy * vy).max(1e-6)).clamp(0.0, 1.0);
                        ((px - ax - vx * t).powi(2) + (py - ay - vy * t).powi(2)).sqrt()
                    })
                    .fold(f32::MAX, f32::min);
                self.blend(x, y, c, (h + 0.5 - d).clamp(0.0, 1.0));
            }
        }
    }

    /// A line rising to the right with an arrow's corner at its end: grew lately.
    /// `x` is its left edge, `cy` its middle, `w` its width.
    pub fn trend(&mut self, x: f32, cy: f32, w: f32, c: [u8; 3]) {
        // Drawn on an 18 x 10 grid: up, a dip, up again, then the corner.
        let k = w / 18.0;
        let p = |gx: f32, gy: f32| (x + gx * k, cy + (gy - 5.0) * k);
        let segs = [(p(0.0, 10.0), p(6.0, 4.0)), (p(6.0, 4.0), p(10.0, 8.0)), (p(10.0, 8.0), p(18.0, 0.0)), (p(11.5, 0.0), p(18.0, 0.0)), (p(18.0, 0.0), p(18.0, 6.5))];
        self.stroke(&segs, (w / 9.0).max(1.2), c);
    }

    /// A rectangle with rounded ends and soft (anti-aliased) edges.
    #[allow(clippy::too_many_arguments)]
    pub fn round_rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, r: f32, c: [u8; 3], alpha: f32) {
        if x1 - x0 < 0.5 || y1 - y0 < 0.5 {
            return;
        }
        let r = r.max(0.0).min((x1 - x0) / 2.0).min((y1 - y0) / 2.0);
        for y in y0.floor() as i32..y1.ceil() as i32 {
            for x in x0.floor() as i32..x1.ceil() as i32 {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let dx = (x0 + r - px).max(px - (x1 - r)).max(0.0);
                let dy = (y0 + r - py).max(py - (y1 - r)).max(0.0);
                let outside = (dx * dx + dy * dy).sqrt() - r;
                // Coverage of the edge pixels, from the distance to the edge.
                let inside_x = (px - x0 + 0.5).min(x1 - px + 0.5).clamp(0.0, 1.0);
                let inside_y = (py - y0 + 0.5).min(y1 - py + 0.5).clamp(0.0, 1.0);
                let cov = (0.5 - outside).clamp(0.0, 1.0) * inside_x * inside_y;
                self.blend(x, y, c, cov * alpha);
            }
        }
    }
}

/// A size written the way Explorer writes it: three figures, "0.98 MB", "373 bytes".
pub fn explorer_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return if bytes == 1 { "1 byte".into() } else { format!("{bytes} bytes") };
    }
    let mut v = bytes as f64 / 1024.0;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    let digits = if v >= 99.95 { 0 } else if v >= 9.995 { 1 } else { 2 };
    format!("{v:.digits$} {}", UNITS[i])
}

/// A part of a whole, as a percentage: "56%", "<1%".
pub fn share(part: u64, whole: u64) -> String {
    if whole == 0 || part == 0 {
        return "0%".into();
    }
    let p = part as f64 * 100.0 / whole as f64;
    if p < 1.0 {
        "<1%".into()
    } else {
        format!("{:.0}%", p.min(100.0))
    }
}

/// The kinds of text drawn.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Font {
    Text,
    Bold,
    SmallBold,
    Big,
    /// Windows' own symbols (cloud, tick, arrows).
    Icon,
}

pub const CLOUD: &str = "\u{E753}";
pub const CHEVRON_DOWN: &str = "\u{E70D}";
pub const CHECK: &str = "\u{E73E}";
/// A picture being drawn, to be shown in a see-through window: shapes go on
/// through `canvas`, text through `text`.
pub struct Surface {
    pub w: i32,
    pub h: i32,
    pub dpi: u32,
    pub clip_top: i32,
    screen: HDC,
    dc: HDC,
    mask_dc: HDC,
    bmp: HBITMAP,
    mask: HBITMAP,
    old: HGDIOBJ,
    old_mask: HGDIOBJ,
    px: *mut u32,
    mask_px: *mut u32,
    fonts: Vec<(Font, HFONT)>,
    old_font: Option<HGDIOBJ>,
}

impl Surface {
    pub fn new(w: i32, h: i32, dpi: u32) -> Option<Surface> {
        if w <= 0 || h <= 0 || w > 8000 || h > 8000 {
            return None;
        }
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let mask_dc = CreateCompatibleDC(Some(screen));
            let mut info = BITMAPINFO::default();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            };
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let mut mask_bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0);
            let mask = CreateDIBSection(Some(mask_dc), &info, DIB_RGB_COLORS, &mut mask_bits, None, 0);
            let (Ok(bmp), Ok(mask)) = (bmp, mask) else {
                let _ = DeleteDC(dc);
                let _ = DeleteDC(mask_dc);
                ReleaseDC(None, screen);
                return None;
            };
            let old = SelectObject(dc, bmp.into());
            let old_mask = SelectObject(mask_dc, mask.into());
            let n = (w * h) as usize;
            std::slice::from_raw_parts_mut(bits as *mut u32, n).fill(0);
            std::slice::from_raw_parts_mut(mask_bits as *mut u32, n).fill(0);
            // Text goes into the mask (white on black, greyscale smoothing), then is
            // laid onto the picture in its colour, using the mask as coverage.
            SetTextColor(mask_dc, COLORREF(0x00ff_ffff));
            SetBkMode(mask_dc, TRANSPARENT);
            Some(Surface {
                w,
                h,
                dpi,
                clip_top: 0,
                screen,
                dc,
                mask_dc,
                bmp,
                mask,
                old,
                old_mask,
                px: bits as *mut u32,
                mask_px: mask_bits as *mut u32,
                fonts: Vec::new(),
                old_font: None,
            })
        }
    }

    /// Pixels per design unit (1.0 at 100% scaling).
    pub fn s(&self) -> f32 {
        self.dpi as f32 / 96.0
    }

    pub fn canvas(&mut self) -> Canvas<'_> {
        let px = unsafe { std::slice::from_raw_parts_mut(self.px, (self.w * self.h) as usize) };
        Canvas { w: self.w, h: self.h, px, clip_top: self.clip_top }
    }

    fn font(&mut self, font: Font) -> HFONT {
        if let Some((_, f)) = self.fonts.iter().find(|(k, _)| *k == font) {
            return *f;
        }
        let (pt, weight, face) = match font {
            Font::Text => (9.0, FW_NORMAL, w!("Segoe UI")),
            Font::Bold => (9.0, FW_SEMIBOLD, w!("Segoe UI")),
            Font::SmallBold => (8.0, FW_SEMIBOLD, w!("Segoe UI")),
            Font::Big => (16.0, FW_SEMIBOLD, w!("Segoe UI")),
            Font::Icon => (8.5, FW_NORMAL, w!("Segoe Fluent Icons")),
        };
        let height = -((pt * self.dpi as f32 / 72.0).round() as i32);
        let f = unsafe {
            CreateFontW(
                height,
                0,
                0,
                0,
                weight.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                0,
                face,
            )
        };
        self.fonts.push((font, f));
        f
    }

    fn select(&mut self, font: Font) {
        let f = self.font(font);
        let old = unsafe { SelectObject(self.mask_dc, f.into()) };
        self.old_font.get_or_insert(old);
    }

    /// How wide the text comes out, in pixels.
    pub fn measure(&mut self, t: &str, font: Font) -> i32 {
        self.select(font);
        let buf: Vec<u16> = t.encode_utf16().collect();
        let mut size = SIZE::default();
        unsafe {
            let _ = GetTextExtentPoint32W(self.mask_dc, &buf, &mut size);
        }
        size.cx
    }

    /// One line of text inside `r`, centred top to bottom, cut short with "…" if it doesn't fit.
    pub fn text(&mut self, t: &str, r: RECT, align: DRAW_TEXT_FORMAT, color: [u8; 3], font: Font) {
        if t.is_empty() || r.right <= r.left || r.bottom <= r.top {
            return;
        }
        self.select(font);
        let mut buf: Vec<u16> = t.encode_utf16().collect();
        let mut rr = r;
        unsafe {
            // Paths are cut in the middle (their end says the most), everything else at the end.
            let cut = if align.contains(DT_PATH_ELLIPSIS) { DRAW_TEXT_FORMAT(0) } else { DT_END_ELLIPSIS };
            DrawTextW(self.mask_dc, &mut buf, &mut rr, align | cut | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
            let _ = GdiFlush();
        }
        let (x0, x1) = (r.left.max(0), r.right.min(self.w));
        let (y0, y1) = (r.top.max(0), r.bottom.min(self.h));
        let n = (self.w * self.h) as usize;
        let mask = unsafe { std::slice::from_raw_parts_mut(self.mask_px, n) };
        let px = unsafe { std::slice::from_raw_parts_mut(self.px, n) };
        let mut c = Canvas { w: self.w, h: self.h, px, clip_top: self.clip_top };
        for y in y0..y1 {
            for x in x0..x1 {
                let i = (y * self.w + x) as usize;
                let m = mask[i];
                if m & 0x00ff_ffff != 0 {
                    let cov = (((m >> 16) & 0xff).max((m >> 8) & 0xff).max(m & 0xff)) as f32 / 255.0;
                    c.blend(x, y, color, cov);
                    mask[i] = 0;
                }
            }
        }
    }

    /// Puts the picture on screen in a layered window, at (x, y).
    pub fn present(&self, hwnd: HWND, x: i32, y: i32) -> bool {
        unsafe {
            let _ = GdiFlush();
            let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
            let pos = POINT { x, y };
            let size = SIZE { cx: self.w, cy: self.h };
            let src = POINT { x: 0, y: 0 };
            UpdateLayeredWindow(hwnd, Some(self.screen), Some(&pos), Some(&size), Some(self.dc), Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA)
                .is_ok()
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            if let Some(f) = self.old_font {
                SelectObject(self.mask_dc, f);
            }
            for (_, f) in self.fonts.drain(..) {
                let _ = DeleteObject(f.into());
            }
            SelectObject(self.dc, self.old);
            SelectObject(self.mask_dc, self.old_mask);
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteObject(self.mask.into());
            let _ = DeleteDC(self.dc);
            let _ = DeleteDC(self.mask_dc);
            ReleaseDC(None, self.screen);
        }
    }
}

thread_local! {
    /// The windows this thread made, so they can be told from other programs'.
    static OURS: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
}

pub type Proc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

/// A see-through window that never takes the keyboard. `ex` adds to its style:
/// click-through, or always on top.
pub fn window(class: PCWSTR, proc: Proc, ex: WINDOW_EX_STYLE) -> windows::core::Result<HWND> {
    unsafe {
        let inst = GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: inst.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let style = WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | ex;
        let h = CreateWindowExW(style, class, w!("Size column"), WS_POPUP, 0, 0, 1, 1, None, None, Some(inst.into()), None)?;
        OURS.with(|o| o.borrow_mut().push(h.0 as isize));
        Ok(h)
    }
}

pub fn is_ours(h: HWND) -> bool {
    OURS.with(|o| o.borrow().contains(&(h.0 as isize)))
}

/// Keeps a window just above its Explorer window in the stacking order, under
/// anything covering that. (Never made an owned window of Explorer's: across two
/// programs that ties both to one input queue, and Explorer then hangs whenever
/// this thread is busy.) Called every frame; does nothing when already in place.
pub fn stack(hwnd: HWND, owner: HWND, shown: bool) {
    unsafe {
        let above = GetWindow(owner, GW_HWNDPREV).ok();
        if shown {
            // In place: among this thread's windows sitting directly on Explorer.
            let mut w = above;
            for _ in 0..8 {
                match w {
                    Some(x) if x == hwnd => return,
                    Some(x) if is_ours(x) => w = GetWindow(x, GW_HWNDPREV).ok(),
                    _ => break,
                }
            }
        }
        // Slotted in under whatever is above Explorer; if that's an always-on-top
        // window (or nothing), Explorer is the highest ordinary window: go on top.
        let topmost = |h: HWND| GetWindowLongPtrW(h, GWL_EXSTYLE) & WS_EX_TOPMOST.0 as isize != 0;
        let after = above.filter(|&h| h != hwnd && !topmost(h)).unwrap_or(HWND_TOP);
        let flags = SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW | SWP_NOOWNERZORDER;
        let _ = SetWindowPos(hwnd, Some(after), 0, 0, 0, 0, flags);
    }
}

/// The colours on screen down one column of pixels, from `top` to `bottom`
/// (layered windows, like the column itself, left out).
pub fn sample(x: i32, top: i32, bottom: i32) -> Vec<[u8; 3]> {
    let h = bottom - top;
    if h <= 0 {
        return Vec::new();
    }
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, 1, h);
        let old = SelectObject(dc, bmp.into());
        let ok = BitBlt(dc, 0, 0, 1, h, Some(screen), x, top, SRCCOPY).is_ok();
        let mut info = BITMAPINFO::default();
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: 1,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut px = vec![0u32; h as usize];
        let got = GetDIBits(dc, bmp, 0, h as u32, Some(px.as_mut_ptr() as *mut _), &mut info, DIB_RGB_COLORS);
        SelectObject(dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        if !ok || got == 0 {
            return Vec::new();
        }
        px.iter().map(|&p| [(p >> 16) as u8, (p >> 8) as u8, p as u8]).collect()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes_read_like_explorer() {
        use super::explorer_size as f;
        assert_eq!(f(373), "373 bytes");
        assert_eq!(f(1), "1 byte");
        assert_eq!(f(2458), "2.40 KB");
        assert_eq!(f(1_027_604), "0.98 MB");
        assert_eq!(f(12_800_000), "12.2 MB");
        assert_eq!(f(150_000_000_000), "140 GB");
    }

    #[test]
    fn shares_read_plainly() {
        use super::share as f;
        assert_eq!(f(56, 100), "56%");
        assert_eq!(f(1, 1000), "<1%");
        assert_eq!(f(0, 1000), "0%");
        assert_eq!(f(5, 0), "0%");
    }
}
