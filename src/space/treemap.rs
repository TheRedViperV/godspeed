//! Squarified treemap layout and painting.

use crate::space::fmt;
use crate::space::theme::{self, Pal};
use crate::space::tree::*;
use eframe::egui::{epaint::Mesh, pos2, vec2, Align2, Color32, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, Vec2};

/// Marks the cell that stands for many items too small to draw.
pub const REST: u32 = u32::MAX;
const MAX_CELLS: usize = 40_000;
/// Items smaller than this many square pixels get folded into a REST cell.
const MIN_AREA: f64 = 6.0;

pub struct Cell {
    pub rect: Rect,
    pub node: u32,
    pub cat: Cat,
}

pub fn header_height(r: Rect) -> f32 {
    if r.height() > 44.0 && r.width() > 60.0 {
        17.0
    } else {
        0.0
    }
}

pub fn build(tree: &Tree, root: u32, area: Rect) -> Vec<Cell> {
    let mut cells = Vec::new();
    lay(tree, root, area, 0, &mut cells);
    cells
}

fn lay(tree: &Tree, dir: u32, area: Rect, depth: u32, cells: &mut Vec<Cell>) {
    let total = tree.node(dir).alloc as f64;
    if cells.len() >= MAX_CELLS || total <= 0.0 || area.width() < 2.0 || area.height() < 2.0 {
        return;
    }
    let px = area.area() as f64 / total;
    let mut items: Vec<(u32, f64)> = Vec::new();
    let mut rest = 0.0;
    for k in tree.sorted_kids(dir) {
        let a = tree.node(k).alloc as f64 * px;
        if a <= 0.0 {
            continue;
        }
        if a < MIN_AREA {
            rest += a;
        } else {
            items.push((k, a));
        }
    }
    if rest > 0.0 {
        items.push((REST, rest));
    }
    let values: Vec<f64> = items.iter().map(|i| i.1).collect();
    let rects = squarify(&values, area);
    for (&(node, _), r) in items.iter().zip(rects) {
        let is_dir = node != REST && tree.node(node).kind == DIR;
        let cat = if node == REST || is_dir { Cat::Other } else { cat_of(&tree.node(node).name) };
        cells.push(Cell { rect: r, node, cat });
        if is_dir && depth < 14 && r.width() > 14.0 && r.height() > 14.0 {
            let h = header_height(r);
            let inner = Rect::from_min_max(pos2(r.min.x + 2.0, r.min.y + 2.0 + h), pos2(r.max.x - 2.0, r.max.y - 2.0));
            lay(tree, node, inner, depth + 1, cells);
        }
    }
}

/// Bruls, Huizing & van Wijk's squarified layout: fill rows along the shorter side,
/// adding items while that keeps the worst aspect ratio from getting worse.
pub fn squarify(values: &[f64], area: Rect) -> Vec<Rect> {
    let mut out = Vec::with_capacity(values.len());
    let total: f64 = values.iter().sum();
    if total <= 0.0 {
        return vec![Rect::NOTHING; values.len()];
    }
    let scale = area.area() as f64 / total;
    let mut r = area;
    let mut i = 0;
    while i < values.len() {
        let (w, h) = (r.width() as f64, r.height() as f64);
        if w <= 0.0 || h <= 0.0 {
            out.extend(std::iter::repeat(Rect::NOTHING).take(values.len() - i));
            break;
        }
        let short = w.min(h);
        let (mut j, mut sum, mut hi, mut lo, mut best) = (i, 0.0, 0.0f64, f64::MAX, f64::MAX);
        while j < values.len() {
            let v = values[j] * scale;
            let (s2, hi2, lo2) = (sum + v, hi.max(v), lo.min(v));
            let worst = {
                let (ss, ww) = (s2 * s2, short * short);
                (ww * hi2 / ss).max(ss / (ww * lo2))
            };
            if j > i && worst > best {
                break;
            }
            (best, sum, hi, lo) = (worst, s2, hi2, lo2);
            j += 1;
        }
        let thick = sum / short;
        if w >= h {
            let mut y = r.min.y as f64;
            for v in &values[i..j] {
                let len = v * scale / thick;
                out.push(Rect::from_min_size(pos2(r.min.x, y as f32), vec2(thick as f32, len as f32)));
                y += len;
            }
            r.min.x += thick as f32;
        } else {
            let mut x = r.min.x as f64;
            for v in &values[i..j] {
                let len = v * scale / thick;
                out.push(Rect::from_min_size(pos2(x as f32, r.min.y), vec2(len as f32, thick as f32)));
                x += len;
            }
            r.min.y += thick as f32;
        }
        i = j;
    }
    out
}

/// Deepest cell under `pos` (cells are stored parents first).
pub fn hit(cells: &[Cell], pos: Pos2) -> Option<usize> {
    cells.iter().rposition(|c| c.rect.contains(pos))
}

fn cushion(mesh: &mut Mesh, r: Rect, c: Color32) {
    if r.width() < 0.5 || r.height() < 0.5 {
        return;
    }
    let i = mesh.vertices.len() as u32;
    mesh.colored_vertex(r.left_top(), theme::mix(c, Color32::WHITE, 0.16));
    mesh.colored_vertex(r.right_top(), c);
    mesh.colored_vertex(r.right_bottom(), theme::mix(c, Color32::BLACK, 0.22));
    mesh.colored_vertex(r.left_bottom(), c);
    mesh.add_triangle(i, i + 1, i + 2);
    mesh.add_triangle(i, i + 2, i + 3);
}

pub fn paint(
    painter: &Painter,
    offset: Vec2,
    cells: &[Cell],
    tree: &Tree,
    p: &Pal,
    selected: Option<u32>,
    hovered: Option<usize>,
) {
    let full = cells.iter().fold(Rect::NOTHING, |acc, c| acc.union(c.rect)).translate(offset);
    painter.rect_filled(full.expand(1.0), 3.0, p.tm_bg);

    let mut mesh = Mesh::default();
    let mut labels: Vec<(Rect, u32)> = Vec::new();
    let small = theme::font(11.0);
    for c in cells {
        let r = c.rect.translate(offset);
        if c.node == REST {
            painter.rect_filled(r.shrink(0.5), 0.0, p.tm_rest);
            continue;
        }
        let nd = tree.node(c.node);
        if nd.kind == DIR {
            painter.rect_filled(r.shrink(0.5), 0.0, p.tm_dir);
            painter.rect_stroke(r.shrink(0.5), 0.0, Stroke::new(1.0_f32, p.tm_line), StrokeKind::Inside);
            let h = header_height(c.rect);
            if h > 0.0 {
                let cy = r.min.y + 2.0 + h * 0.5;
                let size = fmt::size(nd.alloc);
                let size_w = painter.layout_no_wrap(size.clone(), small.clone(), p.tm_text).size().x;
                let name_w = r.width() - 16.0 - size_w - 8.0;
                if name_w > 12.0 {
                    theme::text_at(painter, r.min.x + 6.0, cy, &nd.name, theme::semibold(11.0), p.tm_text, name_w);
                }
                painter.text(pos2(r.max.x - 6.0, cy), Align2::RIGHT_CENTER, size, small.clone(), p.tm_weak);
            }
        } else {
            cushion(&mut mesh, r.shrink(0.5), theme::cat_color(c.cat, p.dark));
            if r.width() > 56.0 && r.height() > 18.0 {
                labels.push((r, c.node));
            }
        }
    }
    painter.add(Shape::mesh(mesh));
    let ink = Color32::from_rgba_unmultiplied(20, 20, 20, 210);
    for (r, n) in labels {
        let nd = tree.node(n);
        theme::text_at(painter, r.min.x + 5.0, r.min.y + 10.0, &nd.name, small.clone(), ink, r.width() - 10.0);
        if r.height() > 34.0 {
            let faint = Color32::from_rgba_unmultiplied(20, 20, 20, 150);
            painter.text(pos2(r.min.x + 5.0, r.min.y + 24.0), Align2::LEFT_CENTER, fmt::size(nd.alloc), small.clone(), faint);
        }
    }

    if let Some(h) = hovered {
        let r = cells[h].rect.translate(offset);
        painter.rect_stroke(r, 0.0, Stroke::new(1.5_f32, p.tm_hover), StrokeKind::Inside);
    }
    if let Some(sel) = selected {
        if let Some(c) = cells.iter().find(|c| c.node == sel) {
            let r = c.rect.translate(offset);
            painter.rect_stroke(r, 0.0, Stroke::new(2.0_f32, p.accent), StrokeKind::Inside);
        }
    }
}

/// On-disk totals per file category under `root`.
pub fn legend(tree: &Tree, root: u32) -> Vec<(Cat, u64)> {
    let mut acc = [0u64; Cat::ALL.len()];
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        let nd = tree.node(n);
        if nd.kind == FILE {
            acc[cat_of(&nd.name) as usize] += nd.alloc;
        } else {
            stack.extend(tree.kids(n));
        }
    }
    let mut out: Vec<(Cat, u64)> = Cat::ALL.iter().map(|&c| (c, acc[c as usize])).filter(|e| e.1 > 0).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out
}
