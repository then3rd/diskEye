//! The Workloads tab: groups → entities → child entities, plus a details pane.

use super::app::App;
use super::lineview::{LineView, Row, Target};
use super::theme;
use crate::model::{Entity, fmt_size};
use crate::views;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WRow {
    Group(usize),
    Entity { id: u32, depth: usize },
}

#[derive(Default)]
pub struct WorkState {
    pub collapsed: HashSet<usize>,
    pub expanded: HashSet<u32>,
    pub sel: usize,
    pub offset: usize,
    pub height: usize,
    /// Keyboard focus is in the details pane's path list.
    pub focus_paths: bool,
    pub detail: LineView,
}

pub fn rows(app: &App) -> Vec<WRow> {
    let mut out = Vec::new();
    for (gi, g) in app.groups.iter().enumerate() {
        out.push(WRow::Group(gi));
        if app.work.collapsed.contains(&gi) {
            continue;
        }
        for &id in &g.entities {
            push_entity(app, id, 1, &mut out);
        }
    }
    out
}

fn push_entity(app: &App, id: u32, depth: usize, out: &mut Vec<WRow>) {
    out.push(WRow::Entity { id, depth });
    if depth < 16 && app.work.expanded.contains(&id) {
        for &c in app.kids.get(&id).map(|v| v.as_slice()).unwrap_or(&[]) {
            push_entity(app, c, depth + 1, out);
        }
    }
}

pub fn has_children(app: &App, id: u32) -> bool {
    app.kids.get(&id).is_some_and(|v| !v.is_empty())
}

/// Make `id` visible (expand its group and ancestors) and select it.
pub fn reveal(app: &mut App, id: u32) {
    let mut chain = vec![];
    let mut cur = Some(id);
    while let Some(c) = cur {
        chain.push(c);
        cur = app.snap.entities.get(c as usize).and_then(|e| e.parent);
        if chain.len() > 64 {
            break;
        }
    }
    let top = *chain.last().unwrap();
    if let Some(gi) = app.groups.iter().position(|g| g.entities.contains(&top)) {
        app.work.collapsed.remove(&gi);
    }
    for &a in chain.iter().skip(1) {
        app.work.expanded.insert(a);
    }
    let rs = rows(app);
    if let Some(i) = rs.iter().position(|r| matches!(r, WRow::Entity { id: x, .. } if *x == id)) {
        app.work.sel = i;
        let h = app.work.height.max(1);
        if i < app.work.offset || i >= app.work.offset + h {
            app.work.offset = i.saturating_sub(h / 3);
        }
    }
    app.work.focus_paths = false;
    app.work.detail = LineView::default();
}

pub fn selected(app: &App) -> Option<WRow> {
    rows(app).get(app.work.sel).copied()
}

fn entity_line(app: &App, e: &Entity, depth: usize, w: usize) -> Line<'static> {
    let marker =
        if has_children(app, e.id) { if app.work.expanded.contains(&e.id) { "▾ " } else { "▸ " } } else { "  " };
    let nums_w = 4 * 11;
    let name_w = w.saturating_sub(nums_w + 2).max(10);
    let label = format!("{}{marker}{}: {}", "  ".repeat(depth), views::kind_label(&e.kind), e.name);
    let opt = |v: Option<u64>| v.map(fmt_size).unwrap_or_else(|| "-".into());
    let mut spans = vec![Span::raw(theme::pad(&label, name_w))];
    spans.push(Span::raw(format!(" {:>10}", fmt_size(e.total_bytes()))));
    let uniq_style = if e.unique_bytes() < e.total_bytes() { theme::warn() } else { Style::new() };
    spans.push(Span::styled(format!(" {:>10}", fmt_size(e.unique_bytes())), uniq_style));
    spans.push(Span::styled(format!(" {:>10}", opt(e.reported)), theme::dim()));
    spans.push(Span::styled(format!(" {:>10}", opt(e.virtual_size)), theme::dim()));
    if let Some(r) = &e.reclaim {
        spans.push(Span::styled(" ♻", theme::risk(r.risk)));
    }
    Line::from(spans)
}

pub fn render(app: &mut App, area: Rect, buf: &mut Buffer) {
    if app.groups.is_empty() {
        let mut lines = vec![
            Line::styled(" No workloads detected in this snapshot.", theme::bold()),
            Line::default(),
            Line::styled(
                " Containers, VMs, package caches and other owners show up here when their providers find them.",
                theme::dim(),
            ),
            Line::default(),
            Line::styled(" Providers:", theme::heading()),
        ];
        for p in &app.snap.providers {
            lines.push(Line::from(vec![
                Span::raw(format!("   {:<22} ", p.name)),
                Span::styled(
                    format!("{:<9}", format!("{:?}", p.coverage)),
                    super::reconcile::coverage_style(p.coverage),
                ),
                Span::styled(format!(" {}", p.notes.join("; ")), theme::dim()),
            ]));
        }
        for (i, l) in lines.iter().enumerate().take(area.height as usize) {
            buf.set_line(area.x, area.y + i as u16, l, area.width);
        }
        return;
    }
    let wide = area.width >= 110;
    let (tree, detail) = if wide {
        let tw = area.width * 3 / 5;
        (
            Rect::new(area.x, area.y, tw, area.height),
            Rect::new(area.x + tw + 1, area.y, area.width - tw - 1, area.height),
        )
    } else {
        let th = area.height / 2;
        (Rect::new(area.x, area.y, area.width, th), Rect::new(area.x, area.y + th, area.width, area.height - th))
    };
    render_tree(app, tree, buf);
    if wide {
        for y in detail.y..detail.y + detail.height {
            if let Some(c) = buf.cell_mut((detail.x - 1, y)) {
                c.set_symbol("│").set_style(theme::dim());
            }
        }
    } else {
        let sep = "─".repeat(detail.width as usize);
        buf.set_string(detail.x, detail.y, sep, theme::dim());
    }
    let inner =
        if wide { detail } else { Rect::new(detail.x, detail.y + 1, detail.width, detail.height.saturating_sub(1)) };
    let mut drows = detail_rows(app);
    let focused = app.work.focus_paths;
    if !focused {
        // Show the details from the top; path rows become selectable on Enter.
        drows.iter_mut().for_each(|r| r.selectable = false);
        app.work.detail = LineView::default();
    }
    app.work.detail.render(&drows, inner, buf, focused);
}

fn render_tree(app: &mut App, area: Rect, buf: &mut Buffer) {
    if area.height < 2 {
        return;
    }
    let w = area.width as usize;
    let nums_w = 4 * 11;
    let name_w = w.saturating_sub(nums_w + 2).max(10);
    let head = format!(
        "{}{:>11}{:>11}{:>11}{:>11}",
        theme::pad(" workload", name_w),
        "total",
        "unique",
        "reported",
        "virtual"
    );
    buf.set_string(area.x, area.y, theme::trunc(&head, w), theme::dim().patch(theme::bold()));
    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
    let rs = rows(app);
    let h = body.height as usize;
    app.work.height = h;
    let st = &mut app.work;
    st.sel = st.sel.min(rs.len().saturating_sub(1));
    if st.sel < st.offset {
        st.offset = st.sel;
    } else if st.sel >= st.offset + h {
        st.offset = st.sel + 1 - h;
    }
    let (sel, offset) = (st.sel, st.offset);
    for (i, r) in rs.iter().enumerate().skip(offset).take(h) {
        let y = body.y + (i - offset) as u16;
        let line = match *r {
            WRow::Group(gi) => {
                let g = &app.groups[gi];
                let m = if app.work.collapsed.contains(&gi) { "▸ " } else { "▾ " };
                let title = theme::trunc(&format!("{m}{} ({})", g.name, g.entities.len()), name_w);
                let mut used = title.chars().count();
                let mut spans = vec![Span::styled(title, theme::heading())];
                if g.reclaimable > 0 {
                    let r = format!("  ♻ {}", fmt_size(g.reclaimable));
                    if used + r.chars().count() <= name_w {
                        used += r.chars().count();
                        spans.push(Span::styled(r, theme::good()));
                    }
                }
                spans.push(Span::raw(" ".repeat(name_w - used)));
                spans.push(Span::styled(format!(" {:>10}", fmt_size(g.total)), theme::bold()));
                spans.push(Span::raw(format!(" {:>10}", fmt_size(g.unique))));
                Line::from(spans)
            }
            WRow::Entity { id, depth } => entity_line(app, &app.snap.entities[id as usize], depth, w),
        };
        buf.set_line(body.x, y, &line, body.width.saturating_sub(1));
        if i == sel {
            let st = if app.work.focus_paths { theme::bold() } else { theme::selected() };
            buf.set_style(Rect::new(body.x, y, body.width.saturating_sub(1), 1), st);
        }
    }
    if rs.len() > h {
        super::lineview::draw_scroll_hint(buf, body, offset, rs.len());
    }
}

pub fn detail_rows(app: &App) -> Vec<Row> {
    let mut out = Vec::new();
    let Some(sel) = selected(app) else { return out };
    match sel {
        WRow::Group(gi) => {
            let g = &app.groups[gi];
            out.push(Row::text(Line::styled(g.name.clone(), theme::heading())));
            out.push(Row::text(format!("total       {}", fmt_size(g.total))));
            out.push(Row::text(format!("unique      {}", fmt_size(g.unique))));
            out.push(Row::text(Line::styled(format!("reclaimable {}", fmt_size(g.reclaimable)), theme::good())));
            out.push(Row::text(format!("{} top-level entities", g.entities.len())));
            out.push(Row::blank());
            out.push(Row::text(Line::styled("Enter/Space: expand or collapse", theme::dim())));
        }
        WRow::Entity { id, .. } => {
            let e = &app.snap.entities[id as usize];
            out.push(Row::text(Line::styled(format!("{}: {}", views::kind_label(&e.kind), e.name), theme::heading())));
            out.push(Row::text(Line::styled(
                format!("{} · provider {} · id {}", e.group, e.provider, e.id),
                theme::dim(),
            )));
            out.push(Row::blank());
            out.push(Row::text(format!("on disk     {}", fmt_size(e.total_bytes()))));
            let shared = e.total_bytes().saturating_sub(e.unique_bytes());
            out.push(Row::text(format!(
                "unique      {}{}",
                fmt_size(e.unique_bytes()),
                if shared > 0 { format!("  ({} shared with others)", fmt_size(shared)) } else { String::new() }
            )));
            out.push(Row::text(format!("apparent    {}", fmt_size(e.measured_apparent))));
            if let Some(r) = e.reported {
                let d = r as i64 - e.total_bytes() as i64;
                out.push(Row::text(Line::from(vec![
                    Span::raw(format!("reported    {}", fmt_size(r))),
                    Span::styled(
                        if d.unsigned_abs() > (e.total_bytes() / 10).max(16 << 20) {
                            format!("  (tool says {} vs measured)", crate::model::fmt_signed(d))
                        } else {
                            String::new()
                        },
                        theme::dim(),
                    ),
                ])));
            }
            if let Some(v) = e.virtual_size {
                out.push(Row::text(format!("virtual     {}", fmt_size(v))));
            }
            if e.external_bytes > 0 {
                out.push(Row::text(format!("outside fs  {}  (raw devices / unscanned)", fmt_size(e.external_bytes))));
            }
            if let Some(r) = &e.reclaim {
                out.push(Row::blank());
                out.push(Row::text(Line::from(vec![
                    Span::styled("reclaim     ", theme::bold()),
                    Span::styled(r.risk.label().to_string(), theme::risk(r.risk)),
                    Span::raw(format!("  ~{}", fmt_size(r.estimate.unwrap_or_else(|| e.unique_bytes())))),
                ])));
                out.push(Row::text(Line::styled(format!("  {}", r.reason), theme::dim())));
                match &r.action {
                    Some(a) => {
                        for l in crate::actions::describe(a) {
                            out.push(Row::text(format!("  • {l}")));
                        }
                        out.push(Row::text(Line::styled("  d: preview / run this cleanup", theme::good())));
                    }
                    None => out.push(Row::text(Line::styled("  no automatic action", theme::dim()))),
                }
            }
            if !e.attrs.is_empty() {
                out.push(Row::blank());
                out.push(Row::text(Line::styled("attributes", theme::bold())));
                for (k, v) in &e.attrs {
                    out.push(Row::text(Line::from(vec![
                        Span::styled(format!("  {k:<14} "), theme::dim()),
                        Span::raw(v.clone()),
                    ])));
                }
            }
            let kids = app.kids.get(&id).map(|v| v.len()).unwrap_or(0);
            if kids > 0 {
                out.push(Row::text(Line::styled(format!("{kids} child entities"), theme::dim())));
            }
            if !e.paths.is_empty() {
                out.push(Row::blank());
                out.push(Row::text(Line::styled(
                    if app.work.focus_paths {
                        "paths (Enter: open in Files, Esc: back)"
                    } else {
                        "paths (Enter to pick)"
                    },
                    theme::bold(),
                )));
                for p in &e.paths {
                    let unresolved = e.unresolved_paths.contains(p);
                    let target = if unresolved { None } else { app.snap.lookup_static(p).map(Target::Node) };
                    let line = if unresolved {
                        Line::from(vec![Span::raw(format!("  {p}")), Span::styled("  (not in scan)", theme::warn())])
                    } else {
                        let size = target
                            .map(|t| match t {
                                Target::Node(n) => fmt_size(app.snap.tree.node(n).alloc),
                                Target::Entity(_) => String::new(),
                            })
                            .unwrap_or_default();
                        Line::from(vec![Span::raw(format!("  {p}")), Span::styled(format!("  {size}"), theme::dim())])
                    };
                    out.push(Row::item(line, target));
                }
            }
            if !e.block_devs.is_empty() {
                out.push(Row::blank());
                out.push(Row::text(Line::styled("block devices", theme::bold())));
                for b in &e.block_devs {
                    let d = app.snap.block.iter().find(|d| &d.kname == b);
                    let s = d
                        .map(|d| format!("  {} ({}, {})", d.name, d.dtype, fmt_size(d.size)))
                        .unwrap_or(format!("  {b}"));
                    out.push(Row::text(s));
                }
            }
            if !e.unresolved_paths.is_empty() {
                out.push(Row::blank());
                out.push(Row::text(Line::styled("unresolved paths (outside the scanned filesystems)", theme::warn())));
                for p in &e.unresolved_paths {
                    out.push(Row::text(Line::styled(format!("  {p}"), theme::dim())));
                }
            }
        }
    }
    out
}
