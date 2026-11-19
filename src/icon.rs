//! The app icons, drawn in code so they're sharp at every size: white shapes on
//! a gradient tile. Godspeed is a lens with a pulse line (violet to cyan),
//! LordSpeed a lens with a crown (gold to red). Used for the .exe (through each
//! build.rs) and the window.

#![allow(dead_code)]

/// A shape in tile units (0..1, y down). Strokes give a half width.
#[derive(Clone, Copy)]
pub enum Shape {
    Ring { cx: f32, cy: f32, r: f32, hw: f32 },
    Line { x0: f32, y0: f32, x1: f32, y1: f32, hw: f32 },
    Fill(&'static [(f32, f32)]),
}

#[derive(Clone, Copy)]
pub struct Palette {
    /// Tile gradient, top-left to bottom-right.
    pub top: [f32; 3],
    pub bottom: [f32; 3],
    pub glyph: &'static [Shape],
}

const fn line(x0: f32, y0: f32, x1: f32, y1: f32, hw: f32) -> Shape {
    Shape::Line { x0, y0, x1, y1, hw }
}

/// Lens with a heartbeat-style pulse running through it.
const PULSE_LENS: &[Shape] = &[
    Shape::Ring { cx: 0.47, cy: 0.46, r: 0.25, hw: 0.045 },
    line(0.65, 0.64, 0.78, 0.77, 0.052),
    line(0.31, 0.47, 0.38, 0.47, 0.027),
    line(0.38, 0.47, 0.42, 0.37, 0.027),
    line(0.42, 0.37, 0.49, 0.57, 0.027),
    line(0.49, 0.57, 0.53, 0.47, 0.027),
    line(0.53, 0.47, 0.63, 0.47, 0.027),
];

/// Lens with a crown inside.
const CROWN_LENS: &[Shape] = &[
    Shape::Ring { cx: 0.46, cy: 0.46, r: 0.26, hw: 0.04 },
    line(0.65, 0.65, 0.78, 0.78, 0.052),
    Shape::Fill(&[(0.33, 0.55), (0.33, 0.38), (0.39, 0.45), (0.46, 0.33), (0.53, 0.45), (0.59, 0.38), (0.59, 0.55)]),
];

pub const GOD: Palette = Palette { top: [116.0, 92.0, 255.0], bottom: [24.0, 196.0, 240.0], glyph: PULSE_LENS };
pub const LORD: Palette = Palette { top: [255.0, 200.0, 72.0], bottom: [226.0, 84.0, 54.0], glyph: CROWN_LENS };

fn inside(poly: &[(f32, f32)], x: f32, y: f32) -> bool {
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let ((xi, yi), (xj, yj)) = (poly[i], poly[j]);
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn hits(s: &Shape, x: f32, y: f32) -> bool {
    match *s {
        Shape::Ring { cx, cy, r, hw } => (((x - cx).powi(2) + (y - cy).powi(2)).sqrt() - r).abs() <= hw,
        Shape::Line { x0, y0, x1, y1, hw } => {
            let (dx, dy) = (x1 - x0, y1 - y0);
            let t = (((x - x0) * dx + (y - y0) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
            let (px, py) = (x0 + dx * t - x, y0 + dy * t - y);
            px * px + py * py <= hw * hw
        }
        Shape::Fill(poly) => inside(poly, x, y),
    }
}

fn in_tile(x: f32, y: f32) -> bool {
    let (m, r) = (0.01, 0.23);
    if x < m || y < m || x > 1.0 - m || y > 1.0 - m {
        return false;
    }
    let cx = x.clamp(m + r, 1.0 - m - r);
    let cy = y.clamp(m + r, 1.0 - m - r);
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

/// RGBA pixels (straight alpha), top row first.
pub fn rgba(n: u32, pal: Palette) -> Vec<u8> {
    const SS: u32 = 4;
    let mut out = Vec::with_capacity((n * n * 4) as usize);
    for row in 0..n {
        for col in 0..n {
            let (mut tile, mut glyph) = (0u32, 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (col as f32 + (sx as f32 + 0.5) / SS as f32) / n as f32;
                    let y = (row as f32 + (sy as f32 + 0.5) / SS as f32) / n as f32;
                    if in_tile(x, y) {
                        tile += 1;
                        glyph += pal.glyph.iter().any(|s| hits(s, x, y)) as u32;
                    }
                }
            }
            let t = (col + row) as f32 / (2 * n) as f32;
            let k = if tile > 0 { glyph as f32 / tile as f32 } else { 0.0 };
            for c in 0..3 {
                let base = pal.top[c] + (pal.bottom[c] - pal.top[c]) * t;
                out.push((base + (255.0 - base) * k).round() as u8);
            }
            out.push((tile * 255 / (SS * SS)) as u8);
        }
    }
    out
}

/// A PNG of the icon (uncompressed inside), for previewing a design.
pub fn png(n: u32, pal: Palette) -> Vec<u8> {
    encode_png(n, n, &rgba(n, pal))
}

/// Any RGBA image as a PNG (uncompressed inside).
pub fn encode_png(w: u32, h: u32, px: &[u8]) -> Vec<u8> {
    fn crc(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc(&body).to_be_bytes());
    }
    let mut raw = Vec::new();
    for row in px.chunks_exact((w * 4) as usize) {
        raw.push(0); // no filter
        raw.extend_from_slice(row);
    }
    // zlib stream of stored (uncompressed) blocks.
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push((i + 1 == blocks.len()) as u8);
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    let (mut a, mut s) = (1u32, 0u32);
    for &v in &raw {
        a = (a + v as u32) % 65_521;
        s = (s + a) % 65_521;
    }
    z.extend_from_slice(&((s << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// One ICO image entry in the classic 32-bit BMP format.
fn bmp_entry(n: u32, pal: Palette) -> Vec<u8> {
    let mut out = Vec::new();
    for v in [40u32, n, n * 2, 1 | (32 << 16), 0, 0, 0, 0, 0, 0] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    // Pixels are stored bottom-up, BGRA.
    let px = rgba(n, pal);
    for row in (0..n).rev() {
        for col in 0..n {
            let i = ((row * n + col) * 4) as usize;
            out.extend_from_slice(&[px[i + 2], px[i + 1], px[i], px[i + 3]]);
        }
    }
    // AND mask: all zero (alpha does the work), rows padded to 32 bits.
    let mask_row = n.div_ceil(32) * 4;
    out.extend(std::iter::repeat(0u8).take((mask_row * n) as usize));
    out
}

/// A complete .ico file with the usual sizes. The 256 one matters: Explorer
/// doesn't scale smaller ones up, so without it big icon views show a small
/// tile floating in the middle.
pub fn ico(pal: Palette) -> Vec<u8> {
    let sizes = [16u32, 20, 24, 32, 40, 48, 64, 96, 128, 256];
    let images: Vec<Vec<u8>> = sizes.iter().map(|&n| bmp_entry(n, pal)).collect();
    let mut out = vec![0, 0, 1, 0];
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (n, img) in sizes.iter().zip(&images) {
        let dim = if *n >= 256 { 0 } else { *n as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(img.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in images {
        out.extend_from_slice(&img);
    }
    out
}
