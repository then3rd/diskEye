//! Squarified treemap (Bruls, Huizing & van Wijk) laid out in terminal cells.
//! Layout runs in "visual" units where a cell is twice as tall as it is wide,
//! so blocks look square on screen.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RectF {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Lay out `sizes` (any order; zeros get empty rects) inside `area`. Returns
/// one rect per input, in input order. Areas are proportional to sizes.
pub fn squarify(sizes: &[u64], area: RectF) -> Vec<RectF> {
    let mut out = vec![RectF { x: area.x, y: area.y, w: 0.0, h: 0.0 }; sizes.len()];
    let total: f64 = sizes.iter().map(|&s| s as f64).sum();
    if total <= 0.0 || area.w <= 0.0 || area.h <= 0.0 {
        return out;
    }
    let mut order: Vec<usize> = (0..sizes.len()).filter(|&i| sizes[i] > 0).collect();
    order.sort_by(|&a, &b| sizes[b].cmp(&sizes[a]).then(a.cmp(&b)));
    let scale = area.w * area.h / total;
    let areas: Vec<f64> = order.iter().map(|&i| sizes[i] as f64 * scale).collect();

    let mut rect = area;
    let mut start = 0;
    while start < areas.len() {
        let side = rect.w.min(rect.h);
        // Grow the row while the worst aspect ratio improves.
        let mut end = start + 1;
        let mut best = worst(&areas[start..end], side);
        while end < areas.len() {
            let w = worst(&areas[start..end + 1], side);
            if w > best {
                break;
            }
            best = w;
            end += 1;
        }
        let row = &areas[start..end];
        let sum: f64 = row.iter().sum();
        if rect.w >= rect.h {
            // Column on the left.
            let cw = if rect.h > 0.0 { sum / rect.h } else { 0.0 };
            let mut y = rect.y;
            for (k, a) in row.iter().enumerate() {
                let h = if cw > 0.0 { a / cw } else { 0.0 };
                out[order[start + k]] = RectF { x: rect.x, y, w: cw, h };
                y += h;
            }
            rect = RectF { x: rect.x + cw, y: rect.y, w: (rect.w - cw).max(0.0), h: rect.h };
        } else {
            // Row along the top.
            let rh = if rect.w > 0.0 { sum / rect.w } else { 0.0 };
            let mut x = rect.x;
            for (k, a) in row.iter().enumerate() {
                let w = if rh > 0.0 { a / rh } else { 0.0 };
                out[order[start + k]] = RectF { x, y: rect.y, w, h: rh };
                x += w;
            }
            rect = RectF { x: rect.x, y: rect.y + rh, w: rect.w, h: (rect.h - rh).max(0.0) };
        }
        start = end;
    }
    out
}

fn worst(row: &[f64], side: f64) -> f64 {
    let sum: f64 = row.iter().sum();
    if sum <= 0.0 || side <= 0.0 {
        return f64::INFINITY;
    }
    let max = row.iter().cloned().fold(f64::MIN, f64::max);
    let min = row.iter().cloned().fold(f64::MAX, f64::min);
    let s2 = side * side;
    let ss = sum * sum;
    (s2 * max / ss).max(ss / (s2 * min))
}

/// Snap a visual-unit layout (cells wide, half-cells tall) to whole cells inside `area`.
/// Edges are rounded consistently, so neighbouring blocks never overlap or leave gaps.
pub fn to_cells(r: RectF, area: Rect) -> Rect {
    let x0 = (r.x.round() as i64).clamp(0, area.width as i64) as u16;
    let x1 = ((r.x + r.w).round() as i64).clamp(0, area.width as i64) as u16;
    let y0 = ((r.y / 2.0).round() as i64).clamp(0, area.height as i64) as u16;
    let y1 = (((r.y + r.h) / 2.0).round() as i64).clamp(0, area.height as i64) as u16;
    Rect { x: area.x + x0, y: area.y + y0, width: x1.saturating_sub(x0), height: y1.saturating_sub(y0) }
}

/// Cell rects for `sizes` inside `area`.
pub fn layout(sizes: &[u64], area: Rect) -> Vec<Rect> {
    let vis = RectF { x: 0.0, y: 0.0, w: area.width as f64, h: area.height as f64 * 2.0 };
    squarify(sizes, vis).into_iter().map(|r| to_cells(r, area)).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// Index of the block to move to from `from` in direction `d`, if any.
pub fn neighbor(rects: &[Rect], from: usize, d: Dir) -> Option<usize> {
    let cur = rects.get(from)?;
    let c = center(cur);
    let mut best: Option<(f64, usize)> = None;
    for (i, r) in rects.iter().enumerate() {
        if i == from || r.width == 0 || r.height == 0 {
            continue;
        }
        let o = center(r);
        let (dx, dy) = (o.0 - c.0, (o.1 - c.1) * 2.0);
        // Must lie beyond the current block's edge in that direction.
        let beyond = match d {
            Dir::Right => r.x >= cur.x + cur.width,
            Dir::Left => r.x + r.width <= cur.x,
            Dir::Down => r.y >= cur.y + cur.height,
            Dir::Up => r.y + r.height <= cur.y,
        };
        if !beyond {
            continue;
        }
        // Overlap along the perpendicular axis is strongly preferred.
        let overlap = match d {
            Dir::Left | Dir::Right => r.y < cur.y + cur.height && cur.y < r.y + r.height,
            Dir::Up | Dir::Down => r.x < cur.x + cur.width && cur.x < r.x + r.width,
        };
        let (primary, perp) = match d {
            Dir::Left | Dir::Right => (dx.abs(), dy.abs()),
            Dir::Up | Dir::Down => (dy.abs(), dx.abs()),
        };
        let score = primary + perp * 2.0 + if overlap { 0.0 } else { 1000.0 };
        if best.is_none_or(|(s, _)| score < s) {
            best = Some((score, i));
        }
    }
    best.map(|(_, i)| i)
}

fn center(r: &Rect) -> (f64, f64) {
    (r.x as f64 + r.width as f64 / 2.0, r.y as f64 + r.height as f64 / 2.0)
}

const PALETTE: [(u8, u8, u8); 10] = [
    (66, 133, 180),
    (214, 120, 60),
    (90, 160, 90),
    (190, 80, 90),
    (140, 110, 180),
    (150, 110, 80),
    (200, 110, 170),
    (120, 120, 120),
    (170, 170, 60),
    (60, 170, 170),
];

pub struct Block<'a> {
    pub rect: Rect,
    pub label: &'a str,
    pub sub: &'a str,
    /// Grey "everything else" block.
    pub other: bool,
}

/// Paint blocks into the buffer. `sel` gets a bright highlight.
pub fn paint(buf: &mut Buffer, blocks: &[Block], sel: Option<usize>) {
    for (i, b) in blocks.iter().enumerate() {
        let r = b.rect;
        if r.width == 0 || r.height == 0 {
            continue;
        }
        let (cr, cg, cb) = if b.other { (90, 90, 90) } else { PALETTE[i % PALETTE.len()] };
        let is_sel = sel == Some(i);
        let (bg, edge) = if is_sel {
            (Color::Rgb(250, 230, 120), Color::Rgb(200, 170, 40))
        } else {
            let shade = |c: u8| (u16::from(c) * 3 / 5) as u8;
            (Color::Rgb(cr, cg, cb), Color::Rgb(shade(cr), shade(cg), shade(cb)))
        };
        let lum = 0.299 * f64::from(if is_sel { 250 } else { cr })
            + 0.587 * f64::from(if is_sel { 230 } else { cg })
            + 0.114 * f64::from(if is_sel { 120 } else { cb });
        let fg = if lum > 140.0 { Color::Black } else { Color::White };
        for y in r.y..r.y + r.height {
            for x in r.x..r.x + r.width {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    let on_edge = (x == r.x + r.width - 1 && r.width > 2) || (y == r.y + r.height - 1 && r.height > 2);
                    cell.set_symbol(" ").set_style(Style::new().bg(if on_edge { edge } else { bg }).fg(fg));
                }
            }
        }
        let inner_w = if r.width > 2 { r.width - 1 } else { r.width };
        let mut style = Style::new().fg(fg).bg(bg);
        if is_sel {
            style = style.add_modifier(Modifier::BOLD);
        }
        if inner_w >= 1 {
            let label = super::theme::trunc(b.label, inner_w as usize);
            buf.set_stringn(r.x, r.y, &label, inner_w as usize, style);
            if r.height >= 3 && !b.sub.is_empty() {
                let sub = super::theme::trunc(b.sub, inner_w as usize);
                buf.set_stringn(r.x, r.y + 1, &sub, inner_w as usize, style);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn areas_are_proportional_and_cover() {
        let sizes = [600, 300, 100, 0, 50, 50];
        let a = RectF { x: 0.0, y: 0.0, w: 60.0, h: 40.0 };
        let r = squarify(&sizes, a);
        let total: f64 = r.iter().map(|r| r.w * r.h).sum();
        assert!((total - 2400.0).abs() < 1e-6, "{total}");
        let sum: u64 = sizes.iter().sum();
        for (s, rr) in sizes.iter().zip(&r) {
            let want = *s as f64 / sum as f64 * 2400.0;
            assert!((rr.w * rr.h - want).abs() < 1e-6);
            assert!(rr.x >= -1e-9 && rr.y >= -1e-9 && rr.x + rr.w <= 60.0 + 1e-6 && rr.y + rr.h <= 40.0 + 1e-6);
        }
        // No two non-empty rects overlap.
        for i in 0..r.len() {
            for j in i + 1..r.len() {
                let (p, q) = (r[i], r[j]);
                let ox = (p.x + p.w).min(q.x + q.w) - p.x.max(q.x);
                let oy = (p.y + p.h).min(q.y + q.h) - p.y.max(q.y);
                assert!(ox <= 1e-6 || oy <= 1e-6, "{i} overlaps {j}");
            }
        }
    }

    #[test]
    fn squarified_aspect_is_reasonable() {
        let sizes = vec![10u64; 16];
        let r = squarify(&sizes, RectF { x: 0.0, y: 0.0, w: 40.0, h: 40.0 });
        for rr in r {
            let ar = (rr.w / rr.h).max(rr.h / rr.w);
            assert!(ar < 2.5, "aspect {ar}");
        }
    }

    #[test]
    fn cells_tile_without_overlap() {
        let area = Rect::new(2, 3, 50, 20);
        let rects = layout(&[500, 200, 120, 80, 40, 30, 20, 10], area);
        let mut seen = vec![0u8; 50 * 20];
        for r in &rects {
            for y in r.y..r.y + r.height {
                for x in r.x..r.x + r.width {
                    let i = (y - 3) as usize * 50 + (x - 2) as usize;
                    seen[i] += 1;
                }
            }
        }
        assert!(seen.iter().all(|&c| c == 1), "every cell covered exactly once");
    }

    #[test]
    fn neighbor_moves() {
        // [0][1]
        // [0][2]
        let rects = vec![Rect::new(0, 0, 10, 10), Rect::new(10, 0, 10, 5), Rect::new(10, 5, 10, 5)];
        assert_eq!(neighbor(&rects, 0, Dir::Right), Some(1));
        assert_eq!(neighbor(&rects, 1, Dir::Down), Some(2));
        assert_eq!(neighbor(&rects, 2, Dir::Up), Some(1));
        assert_eq!(neighbor(&rects, 2, Dir::Left), Some(0));
        assert_eq!(neighbor(&rects, 0, Dir::Left), None);
    }
}
