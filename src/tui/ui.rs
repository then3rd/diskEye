//! Frame layout: header, tab bar, the active tab, footer, popups.

use super::app::{App, Level, Popup, Tab};
use super::{files, reclaim, theme, workloads};
use crate::model::fmt_size;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget, Wrap};

pub const MIN_W: u16 = 60;
pub const MIN_H: u16 = 12;

pub fn draw(app: &mut App, f: &mut Frame) {
    let area = f.area();
    draw_into(app, area, f.buffer_mut());
}

pub fn draw_into(app: &mut App, area: Rect, buf: &mut Buffer) {
    if area.width < MIN_W || area.height < MIN_H {
        let msg = format!("terminal too small ({}×{}); need at least {MIN_W}×{MIN_H}", area.width, area.height);
        let p = Paragraph::new(msg).wrap(Wrap { trim: true }).style(theme::warn());
        p.render(area, buf);
        return;
    }
    let header = Rect::new(area.x, area.y, area.width, 1);
    let tabs = Rect::new(area.x, area.y + 1, area.width, 1);
    let body = Rect::new(area.x, area.y + 2, area.width, area.height - 3);
    let footer = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
    app.width = body.width;
    draw_header(app, header, buf);
    draw_tabs(app, tabs, buf);
    match app.tab {
        Tab::Files => {
            let c = files::Ctx { snap: &app.snap, targets: &app.targets, by_node: &app.by_node };
            files::render(&mut app.files, &c, body, buf);
        }
        Tab::Workloads => workloads::render(app, body, buf),
        Tab::Reclaim => reclaim::render(app, body, buf),
        Tab::Physical | Tab::Reconcile | Tab::Diff => {
            let rows = app.rows_for(app.tab);
            let view = match app.tab {
                Tab::Physical => &mut app.phys,
                Tab::Reconcile => &mut app.reconc,
                _ => &mut app.diff.view,
            };
            view.render(&rows, body, buf, true);
        }
    }
    draw_footer(app, footer, buf);
    if app.popup.is_some() {
        draw_popup(app, body, buf);
    }
}

fn draw_header(app: &App, area: Rect, buf: &mut Buffer) {
    let m = &app.snap.meta;
    let (used, free, total) = app.totals;
    let mut spans = vec![
        Span::styled(" diskeye ", theme::bold().patch(theme::selected())),
        Span::styled(format!(" {} ", m.host), theme::bold()),
        Span::styled(
            format!("· {} ({} ago) ", crate::util::timestamp_human(m.started), crate::util::age_human(m.started)),
            theme::dim(),
        ),
        Span::raw("· as "),
        if m.is_root() { Span::styled("root ", theme::good()) } else { Span::styled("user ", theme::warn()) },
    ];
    if total > 0 {
        let frac = used as f64 / (used + free).max(1) as f64;
        spans.push(Span::raw("· used "));
        spans.push(Span::styled(fmt_size(used), theme::usage(frac).patch(theme::bold())));
        spans.push(Span::raw(" · free "));
        spans.push(Span::styled(fmt_size(free), theme::bold()));
        spans.push(Span::raw(" "));
    }
    let (p, d) = app.coverage_gaps();
    if p > 0 || d > 0 {
        let mut parts = vec![];
        if p > 0 {
            parts.push(format!("{p} provider{}", if p == 1 { "" } else { "s" }));
        }
        if d > 0 {
            parts.push(format!("{d} unreadable dirs"));
        }
        spans.push(Span::styled(format!("· ⚠ gaps: {}", parts.join(", ")), theme::warn()));
    } else {
        spans.push(Span::styled("· ✓ full coverage", theme::good()));
    }
    buf.set_line(area.x, area.y, &Line::from(spans), area.width);
}

fn draw_tabs(app: &App, area: Rect, buf: &mut Buffer) {
    let mut spans = vec![Span::raw(" ")];
    for (i, t) in Tab::ALL.iter().enumerate() {
        let label = format!(" {} {} ", i + 1, t.title());
        let st = if *t == app.tab { theme::selected().patch(theme::bold()) } else { theme::dim() };
        spans.push(Span::styled(label, st));
        spans.push(Span::raw(" "));
    }
    if app.diff.busy() {
        spans.push(Span::styled(" diff: loading… ", theme::warn()));
    }
    buf.set_line(area.x, area.y, &Line::from(spans), area.width);
}

pub fn hints(app: &App) -> &'static str {
    match app.tab {
        Tab::Files if app.files.filter_editing => "type to filter · Enter keep · Esc clear · ↑↓ move",
        Tab::Files if app.files.treemap => {
            "←↑↓→ move · Enter open · Bksp up · t list · s metric · g owner · d cleanup · ? help · q quit"
        }
        Tab::Files => {
            "↑↓ move · → open · ← up · s metric · n name · / filter · t treemap · g owner · d cleanup · ? help"
        }
        Tab::Workloads if app.work.focus_paths => "↑↓ pick path · Enter open in Files · Esc back",
        Tab::Workloads => "↑↓ move · Space/←→ expand · Enter paths · g open in Files · d cleanup · ? help · q quit",
        Tab::Reclaim => "↑↓ move · Space mark · a mark safe · Enter preview · x run · g workload · ? help · q quit",
        Tab::Diff => "↑↓ move · Enter open path · [ older baseline · ] newer baseline · ? help · q quit",
        Tab::Physical | Tab::Reconcile => "↑↓ move · Enter open in Files · 1-6/Tab switch view · ? help · q quit",
    }
}

fn draw_footer(app: &App, area: Rect, buf: &mut Buffer) {
    let line = match &app.status {
        Some((msg, lvl)) => {
            let st = match lvl {
                Level::Info => theme::accent(),
                Level::Warn => theme::warn(),
                Level::Error => theme::bad().patch(theme::bold()),
            };
            Line::from(Span::styled(format!(" {msg}"), st))
        }
        None => Line::from(Span::styled(format!(" {}", hints(app)), theme::dim())),
    };
    buf.set_line(area.x, area.y, &line, area.width);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

pub const HELP: &[(&str, &str)] = &[
    ("1-6, Tab/Shift-Tab", "switch view"),
    ("q, Ctrl-C", "quit   (Esc: back / close, quits at the top)"),
    ("↑↓ j k, PgUp/PgDn, Home/End", "move"),
    ("?", "this help"),
    ("", ""),
    ("Files", ""),
    ("Enter → l / ← h Backspace", "open directory / go up"),
    ("s", "cycle metric: disk usage → apparent size → item count"),
    ("n", "sort by name (again: by size)"),
    ("/", "filter entries by substring (Esc clears)"),
    ("t", "toggle treemap (arrows move, Enter opens, Backspace up)"),
    ("g", "jump to the owning workload"),
    ("d", "cleanup preview for the owning workload"),
    ("", ""),
    ("Workloads", ""),
    ("Space, ← →", "collapse / expand"),
    ("Enter", "pick one of the entity's paths, then Enter opens it in Files"),
    ("g / d", "open first path in Files / cleanup preview"),
    ("", ""),
    ("Reclaim", ""),
    ("Space / a", "mark item / mark all safe items (running total at the top)"),
    ("Enter", "preview: reason, exact steps, preflight check"),
    ("x", "run marked items (or the current one) after typing a confirmation"),
    ("", ""),
    ("Physical / Reconcile / Diff", ""),
    ("Enter", "open the selected filesystem / directory in Files"),
    ("[ ]", "Diff: older / newer baseline snapshot"),
];

fn draw_popup(app: &App, area: Rect, buf: &mut Buffer) {
    let Some(p) = &app.popup else { return };
    match p {
        Popup::Help => {
            let lines: Vec<Line> = HELP
                .iter()
                .map(|(k, v)| {
                    if v.is_empty() && !k.is_empty() {
                        Line::styled(k.to_string(), theme::heading())
                    } else {
                        Line::from(vec![Span::styled(format!("{k:<30}"), theme::bold()), Span::raw(v.to_string())])
                    }
                })
                .collect();
            let r = centered(area, 96, lines.len() as u16 + 2);
            Clear.render(r, buf);
            Paragraph::new(lines).block(Block::bordered().title(" keys — any key closes ")).render(r, buf);
        }
        Popup::Preview { items, checks, scroll } => {
            let mut lines = reclaim::preview_lines(app, items, checks);
            lines.push(Line::default());
            let can_run = checks.iter().all(|c| c.is_ok());
            lines.push(Line::styled(
                if can_run { "x: run (asks for confirmation) · any other key: close" } else { "any key: close" },
                theme::dim(),
            ));
            let h = (lines.len() as u16 + 2).min(area.height);
            let r = centered(area, area.width.saturating_sub(8).min(110), h);
            Clear.render(r, buf);
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((*scroll, 0))
                .block(Block::bordered().title(" cleanup preview "))
                .render(r, buf);
        }
        Popup::Confirm { items, required, input } => {
            let mut lines = reclaim::preview_lines(app, items, &[]);
            let total: u64 = items.iter().map(|&i| app.reclaim[i].bytes).sum();
            lines.push(Line::default());
            if app.run_as_root {
                lines.push(Line::styled("You are running as root.", theme::bad().patch(theme::bold())));
            }
            lines.push(Line::from(vec![Span::raw(format!(
                "This will free about {} and cannot be undone (unless moved to trash). ",
                fmt_size(total)
            ))]));
            lines.push(Line::from(vec![
                Span::raw("Type "),
                Span::styled(required.clone(), theme::bold().patch(theme::warn())),
                Span::raw(" and press Enter to run, Esc to cancel:"),
            ]));
            lines.push(Line::from(vec![Span::styled("> ", theme::bold()), Span::raw(input.clone()), Span::raw("▏")]));
            let h = (lines.len() as u16 + 2).min(area.height);
            let r = centered(area, area.width.saturating_sub(8).min(110), h);
            Clear.render(r, buf);
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().border_style(theme::bad()).title(" confirm cleanup "))
                .render(r, buf);
        }
    }
}
