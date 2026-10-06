//! A scrollable page of pre-styled lines where some lines are selectable rows
//! (with an optional jump target). Used by Physical, Reconcile and Diff, whose
//! content is small and rebuilt every frame for the current width.

use super::theme;
use crate::model::NodeId;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Open this directory (or select this file) in Files.
    Node(NodeId),
    /// Select this entity in Workloads.
    Entity(u32),
}

pub struct Row {
    pub line: Line<'static>,
    pub selectable: bool,
    pub target: Option<Target>,
}

impl Row {
    pub fn text(line: impl Into<Line<'static>>) -> Self {
        Row { line: line.into(), selectable: false, target: None }
    }
    pub fn item(line: impl Into<Line<'static>>, target: Option<Target>) -> Self {
        Row { line: line.into(), selectable: true, target }
    }
    pub fn blank() -> Self {
        Row::text(Line::default())
    }
}

#[derive(Debug, Default, Clone)]
pub struct LineView {
    /// Index of the current line (always a selectable one when any exist).
    pub cursor: usize,
    pub offset: usize,
    /// Rows visible at the last render; used for paging.
    pub height: usize,
}

impl LineView {
    pub fn fix(&mut self, rows: &[Row]) {
        if rows.is_empty() {
            self.cursor = 0;
            self.offset = 0;
            return;
        }
        self.cursor = self.cursor.min(rows.len() - 1);
        if !rows[self.cursor].selectable {
            if let Some(i) = (self.cursor..rows.len()).find(|&i| rows[i].selectable) {
                self.cursor = i;
            } else if let Some(i) = (0..self.cursor).rev().find(|&i| rows[i].selectable) {
                self.cursor = i;
            }
        }
    }

    pub fn has_selectable(rows: &[Row]) -> bool {
        rows.iter().any(|r| r.selectable)
    }

    /// Move by `delta` selectable rows (or scroll when nothing is selectable).
    pub fn step(&mut self, rows: &[Row], delta: isize) {
        if !Self::has_selectable(rows) {
            let max = rows.len().saturating_sub(self.height.max(1));
            self.offset = (self.offset as isize + delta).clamp(0, max as isize) as usize;
            return;
        }
        self.fix(rows);
        let mut cur = self.cursor;
        let mut left = delta.unsigned_abs();
        while left > 0 {
            let next = if delta > 0 {
                (cur + 1..rows.len()).find(|&i| rows[i].selectable)
            } else {
                (0..cur).rev().find(|&i| rows[i].selectable)
            };
            match next {
                Some(n) => cur = n,
                None => break,
            }
            left -= 1;
        }
        // At the edges, still reveal the non-selectable lines around the cursor.
        if delta < 0 && cur == self.cursor {
            self.offset = 0;
        }
        if delta > 0 && cur == self.cursor {
            self.offset = rows.len().saturating_sub(self.height.max(1));
        }
        self.cursor = cur;
    }

    pub fn page(&mut self, rows: &[Row], down: bool) {
        let h = self.height.max(2) as isize - 1;
        if !Self::has_selectable(rows) {
            self.step(rows, if down { h } else { -h });
            return;
        }
        let target = if down { self.cursor as isize + h } else { self.cursor as isize - h };
        let target = target.clamp(0, rows.len() as isize - 1) as usize;
        // Nearest selectable row towards the target.
        let pick = if down {
            (target..rows.len())
                .find(|&i| rows[i].selectable)
                .or_else(|| (0..target).rev().find(|&i| rows[i].selectable))
        } else {
            (0..=target)
                .rev()
                .find(|&i| rows[i].selectable)
                .or_else(|| (target..rows.len()).find(|&i| rows[i].selectable))
        };
        if let Some(p) = pick {
            self.cursor = p;
        }
    }

    pub fn home(&mut self, rows: &[Row]) {
        self.cursor = 0;
        self.offset = 0;
        self.fix(rows);
    }

    pub fn end(&mut self, rows: &[Row]) {
        self.cursor = rows.len().saturating_sub(1);
        self.fix(rows);
        self.offset = rows.len().saturating_sub(self.height.max(1));
    }

    pub fn current<'a>(&self, rows: &'a [Row]) -> Option<&'a Row> {
        rows.get(self.cursor).filter(|r| r.selectable)
    }

    pub fn render(&mut self, rows: &[Row], area: Rect, buf: &mut Buffer, focused: bool) {
        self.height = area.height as usize;
        let h = self.height;
        if h == 0 {
            return;
        }
        self.fix(rows);
        let sel = Self::has_selectable(rows);
        if sel {
            if self.cursor < self.offset {
                self.offset = self.cursor;
            } else if self.cursor >= self.offset + h {
                self.offset = self.cursor + 1 - h;
            }
        }
        self.offset = self.offset.min(rows.len().saturating_sub(1));
        for (i, row) in rows.iter().enumerate().skip(self.offset).take(h) {
            let y = area.y + (i - self.offset) as u16;
            buf.set_line(area.x, y, &row.line, area.width);
            if sel && focused && i == self.cursor {
                buf.set_style(Rect::new(area.x, y, area.width, 1), theme::selected());
            }
        }
        if rows.len() > h {
            draw_scroll_hint(buf, area, self.offset, rows.len());
        }
    }
}

/// A one-column position indicator on the right edge.
pub fn draw_scroll_hint(buf: &mut Buffer, area: Rect, offset: usize, total: usize) {
    if area.height == 0 || area.width == 0 || total == 0 {
        return;
    }
    let h = area.height as usize;
    let x = area.x + area.width - 1;
    let thumb = ((h * h) / total).clamp(1, h);
    let pos = if total > h { offset * (h - thumb) / (total - h) } else { 0 };
    for i in 0..h {
        let (sym, st) = if i >= pos && i < pos + thumb { ("┃", theme::accent()) } else { ("│", theme::dim()) };
        if let Some(c) = buf.cell_mut((x, area.y + i as u16)) {
            c.set_symbol(sym).set_style(st);
        }
    }
}
