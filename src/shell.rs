//! Real Windows icons and thumbnails, the same ones Explorer shows, loaded from
//! the shell on a background thread and kept as textures.

use eframe::egui::{self, ColorImage, TextureHandle, TextureId, TextureOptions, Vec2};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::mpsc::{self, Receiver, Sender};

#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);

const IID_IMAGE_FACTORY: Guid = Guid(0xbcc1_8b79, 0xba16, 0x442f, [0x80, 0xc4, 0x8a, 0x59, 0xc3, 0x0c, 0x46, 0x3b]);

#[repr(C)]
#[derive(Clone, Copy)]
struct Size {
    cx: i32,
    cy: i32,
}

/// IShellItemImageFactory
#[repr(C)]
struct FactoryVtbl {
    query_interface: usize,
    add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_image: unsafe extern "system" fn(*mut c_void, Size, u32, *mut *mut c_void) -> i32,
}

#[repr(C)]
struct Bitmap {
    kind: i32,
    width: i32,
    height: i32,
    width_bytes: i32,
    planes: u16,
    bits_pixel: u16,
    bits: *mut c_void,
}

#[repr(C)]
#[derive(Default)]
struct BitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_ppm: i32,
    y_ppm: i32,
    clr_used: u32,
    clr_important: u32,
}

#[repr(C)]
struct BitmapInfo {
    header: BitmapInfoHeader,
    colors: [u32; 1],
}

#[link(name = "ole32")]
extern "system" {
    fn CoInitializeEx(reserved: *mut c_void, coinit: u32) -> i32;
}

#[link(name = "shell32")]
extern "system" {
    fn SHCreateItemFromParsingName(path: *const u16, ctx: *mut c_void, riid: *const Guid, out: *mut *mut c_void) -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn GetObjectW(h: *mut c_void, size: i32, out: *mut c_void) -> i32;
    fn GetDIBits(dc: *mut c_void, bm: *mut c_void, start: u32, lines: u32, bits: *mut c_void, info: *mut BitmapInfo, usage: u32) -> i32;
    fn DeleteObject(h: *mut c_void) -> i32;
}

#[link(name = "user32")]
extern "system" {
    fn GetDC(hwnd: *mut c_void) -> *mut c_void;
    fn ReleaseDC(hwnd: *mut c_void, dc: *mut c_void) -> i32;
}

const BIGGER_OK: u32 = 0x1;
const ICON_ONLY: u32 = 0x4;

unsafe fn shell_image(path: &str, size: i32, flags: u32) -> Option<ColorImage> {
    let w: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let mut factory: *mut c_void = null_mut();
    if SHCreateItemFromParsingName(w.as_ptr(), null_mut(), &IID_IMAGE_FACTORY, &mut factory) < 0 || factory.is_null() {
        return None;
    }
    let vtbl = *(factory as *const *const FactoryVtbl);
    let mut bitmap: *mut c_void = null_mut();
    let hr = ((*vtbl).get_image)(factory, Size { cx: size, cy: size }, flags, &mut bitmap);
    ((*vtbl).release)(factory);
    if hr < 0 || bitmap.is_null() {
        return None;
    }
    let img = to_image(bitmap);
    DeleteObject(bitmap);
    img
}

unsafe fn to_image(bitmap: *mut c_void) -> Option<ColorImage> {
    let mut bm: Bitmap = std::mem::zeroed();
    if GetObjectW(bitmap, std::mem::size_of::<Bitmap>() as i32, &mut bm as *mut Bitmap as _) == 0 {
        return None;
    }
    let (w, h) = (bm.width, bm.height.abs());
    if w <= 0 || h <= 0 || w > 2048 || h > 2048 {
        return None;
    }
    // Negative height asks for rows top-down, whatever the bitmap's own order.
    let mut info = BitmapInfo {
        header: BitmapInfoHeader { size: 40, width: w, height: -h, planes: 1, bit_count: 32, ..Default::default() },
        colors: [0],
    };
    let mut px = vec![0u8; (w * h * 4) as usize];
    let dc = GetDC(null_mut());
    let lines = GetDIBits(dc, bitmap, 0, h as u32, px.as_mut_ptr() as _, &mut info, 0);
    ReleaseDC(null_mut(), dc);
    if lines == 0 {
        return None;
    }
    // Images without an alpha channel come back fully transparent.
    let opaque = px.chunks_exact(4).all(|p| p[3] == 0);
    for p in px.chunks_exact_mut(4) {
        p.swap(0, 2);
        if opaque {
            p[3] = 255;
        }
    }
    Some(ColorImage::from_rgba_premultiplied([w as usize, h as usize], &px))
}

struct Req {
    key: String,
    path: String,
    size: i32,
    flags: u32,
}

enum Slot {
    Pending,
    Ready(TextureHandle),
    Missing,
}

pub struct Icons {
    tx: Sender<Req>,
    rx: Receiver<(String, Option<ColorImage>)>,
    slots: HashMap<String, Slot>,
    /// Thumbnails are big, so only the most recent few are kept.
    thumbs: VecDeque<String>,
}

const MAX_THUMBS: usize = 48;

impl Icons {
    pub fn start(ctx: egui::Context) -> Icons {
        let (tx, requests) = mpsc::channel::<Req>();
        let (images, rx) = mpsc::channel();
        std::thread::spawn(move || {
            unsafe { CoInitializeEx(null_mut(), 0x2 | 0x4) }; // apartment-threaded, no OLE1 DDE
            let mut stack: Vec<Req> = Vec::new();
            loop {
                if stack.is_empty() {
                    match requests.recv() {
                        Ok(r) => stack.push(r),
                        Err(_) => return,
                    }
                }
                while let Ok(r) = requests.try_recv() {
                    stack.push(r);
                }
                // Newest first: what's on screen now matters more than what scrolled by.
                let Some(r) = stack.pop() else { continue };
                let img = unsafe { shell_image(&r.path, r.size, r.flags) };
                if images.send((r.key, img)).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
        });
        Icons { tx, rx, slots: HashMap::new(), thumbs: VecDeque::new() }
    }

    /// Turns finished images into textures. Call once per frame.
    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok((key, img)) = self.rx.try_recv() {
            let slot = match img {
                Some(img) => Slot::Ready(ctx.load_texture(&key, img, TextureOptions::LINEAR)),
                None => Slot::Missing,
            };
            self.slots.insert(key, slot);
        }
    }

    fn get(&mut self, key: &str, path: impl FnOnce() -> String, size: i32, flags: u32) -> Option<(TextureId, Vec2)> {
        match self.slots.get(key) {
            Some(Slot::Ready(t)) => return Some((t.id(), t.size_vec2())),
            Some(_) => return None,
            None => {}
        }
        self.slots.insert(key.to_string(), Slot::Pending);
        let _ = self.tx.send(Req { key: key.to_string(), path: path(), size, flags });
        None
    }

    /// A small icon, shared by everything with the same `key`.
    pub fn icon(&mut self, key: &str, path: impl FnOnce() -> String) -> Option<TextureId> {
        self.get(key, path, 48, ICON_ONLY | BIGGER_OK).map(|t| t.0)
    }

    /// A large thumbnail (or the icon, for files without one).
    pub fn thumb(&mut self, path: &str) -> Option<(TextureId, Vec2)> {
        let key = format!("thumb:{path}");
        if !self.slots.contains_key(&key) {
            self.thumbs.push_back(key.clone());
            while self.thumbs.len() > MAX_THUMBS {
                if let Some(old) = self.thumbs.pop_front() {
                    self.slots.remove(&old);
                }
            }
        }
        self.get(&key, || path.to_string(), 256, 0)
    }
}
