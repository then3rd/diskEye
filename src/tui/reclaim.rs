//! The Reclaim tab and the action preview / confirmation popups.

use super::app::App;
use super::theme;
use crate::model::{Risk, fmt_size};
use crate::views;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::collections::BTreeSet;

#[derive(Default)]
pub struct ReclaimState {
    pub sel: usize,
    pub offset: usize,
    pub height: usize,
    /// Indices into `App::reclaim` marked with Space.
    pub marked: BTreeSet<usize>,
}

pub fn render(app: &mut App, area: Rect, buf: &mut Buffer) {
    let items = &app.reclaim;
    if items.is_empty() {
        let lines = [
            Line::styled(" Nothing reclaimable was identified in this snapshot.", theme::bold()),
            Line::default(),
            Line::styled(
                " Reclaim candidates come from workload providers (caches, dangling images, unused volumes, …).",
                theme::dim(),
            ),
            Line::styled(" Use Files (2) to look for large directories manually.", theme::dim()),
        ];
        for (i, l) in lines.iter().enumerate().take(area.height as usize) {
            buf.set_line(area.x, area.y + i as u16, l, area.width);
        }
        return;
    }
    let live = |i: &usize| !app.done.contains(&items[*i].entity);
    let sum = |risk: Risk| -> u64 {
        (0..items.len()).filter(live).filter(|&i| items[i].risk == risk).map(|i| items[i].bytes).sum()
    };
    let marked: Vec<usize> = app.recl.marked.iter().copied().filter(live).collect();
    let marked_total: u64 = marked.iter().map(|&i| items[i].bytes).sum();
    let head = Line::from(vec![
        Span::styled(" safe ", theme::risk(Risk::Safe)),
        Span::styled(fmt_size(sum(Risk::Safe)), theme::bold()),
        Span::styled("  review ", theme::risk(Risk::Review)),
        Span::styled(fmt_size(sum(Risk::Review)), theme::bold()),
        Span::styled("  danger ", theme::risk(Risk::Danger)),
        Span::styled(fmt_size(sum(Risk::Danger)), theme::bold()),
        Span::styled("   │ selected: ", theme::dim()),
        Span::styled(
            format!("{} items, {}", marked.len(), fmt_size(marked_total)),
            theme::accent().patch(theme::bold()),
        ),
    ]);
    buf.set_line(area.x, area.y, &head, area.width);
    if !app.done.is_empty() {
        buf.set_line(
            area.x,
            area.y + 1,
            &Line::styled(
                format!(" {} action(s) run this session — sizes are stale until you re-scan", app.done.len()),
                theme::warn(),
            ),
            area.width,
        );
    }
    let body = Rect::new(area.x, area.y + 2, area.width, area.height.saturating_sub(2));
    let h = body.height as usize;
    let st = &mut app.recl;
    st.height = h;
    st.sel = st.sel.min(items.len() - 1);
    if st.sel < st.offset {
        st.offset = st.sel;
    } else if h > 0 && st.sel >= st.offset + h {
        st.offset = st.sel + 1 - h;
    }
    let w = area.width as usize;
    let name_w = (w / 3).clamp(20, 50);
    for (i, it) in items.iter().enumerate().skip(st.offset).take(h) {
        let y = body.y + (i - st.offset) as u16;
        let e = &app.snap.entities[it.entity as usize];
        let done = app.done.contains(&it.entity);
        let mark = if done {
            "✓"
        } else if st.marked.contains(&i) {
            "■"
        } else {
            "□"
        };
        let auto = if it.action.is_some() { " " } else { "·" };
        let line = Line::from(vec![
            Span::styled(format!(" {mark} "), if done { theme::good() } else { theme::accent() }),
            Span::styled(format!("{:<7}", it.risk.label()), theme::risk(it.risk)),
            Span::styled(format!("{:>10} ", fmt_size(it.bytes)), theme::bold()),
            Span::styled(auto.to_string(), theme::dim()),
            Span::raw(theme::pad(&format!("{}: {}", views::kind_label(&e.kind), e.name), name_w)),
            Span::styled(format!("  {} — {}", e.group, it.reason), theme::dim()),
            Span::styled(if done { "  (done — rescan to update)".to_string() } else { String::new() }, theme::good()),
        ]);
        let line = if done { line.patch_style(theme::dim()) } else { line };
        buf.set_line(body.x, y, &line, body.width.saturating_sub(1));
        if i == st.sel {
            buf.set_style(Rect::new(body.x, y, body.width.saturating_sub(1), 1), theme::selected());
        }
    }
    if items.len() > h {
        super::lineview::draw_scroll_hint(buf, body, st.offset, items.len());
    }
}

/// Lines describing what running `items` would do, with preflight results.
pub fn preview_lines(app: &App, items: &[usize], checks: &[Result<(), String>]) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (k, &i) in items.iter().enumerate() {
        let it = &app.reclaim[i];
        let e = &app.snap.entities[it.entity as usize];
        if k > 0 {
            out.push(Line::default());
        }
        out.push(Line::from(vec![
            Span::styled(format!("{}: {}", views::kind_label(&e.kind), e.name), theme::bold()),
            Span::raw("  "),
            Span::styled(it.risk.label().to_string(), theme::risk(it.risk)),
            Span::styled(format!("  ~{}", fmt_size(it.bytes)), theme::bold()),
        ]));
        out.push(Line::styled(format!("{} · {}", e.group, it.reason), theme::dim()));
        match &it.action {
            Some(a) => {
                out.push(Line::styled(format!("Action: {}", a.label), Style::new()));
                for l in crate::actions::describe(a) {
                    out.push(Line::from(format!("  • {l}")));
                }
                match checks.get(k) {
                    Some(Ok(())) => out.push(Line::styled("Preflight: ok", theme::good())),
                    Some(Err(e)) => out.push(Line::styled(format!("Cannot run: {e}"), theme::bad())),
                    None => {}
                }
            }
            None => out.push(Line::styled("No automatic action — clean this up manually.", theme::warn())),
        }
        if app.done.contains(&it.entity) {
            out.push(Line::styled("Already run in this session.", theme::good()));
        }
    }
    out
}

pub fn preflight_all(app: &App, items: &[usize]) -> Vec<Result<(), String>> {
    items
        .iter()
        .map(|&i| match &app.reclaim[i].action {
            Some(a) => crate::actions::preflight(a).map_err(|e| format!("{e:#}")),
            None => Err("no automatic action".into()),
        })
        .collect()
}
