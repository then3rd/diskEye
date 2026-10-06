//! The Reconcile tab: "where did the rest go" per filesystem, deleted-but-open
//! files, files hidden under mountpoints, unreadable dirs, provider coverage.

use super::app::App;
use super::lineview::{Row, Target};
use super::theme;
use crate::model::{Coverage, Reconcile, fmt_signed, fmt_size};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

pub fn coverage_style(c: Coverage) -> Style {
    match c {
        Coverage::Complete => theme::good(),
        Coverage::Partial => theme::warn(),
        Coverage::Denied => theme::bad(),
        Coverage::Absent => theme::dim(),
    }
}

/// (label, bytes, colour, glyph) for each segment of the stacked bar.
pub fn segments(rc: &Reconcile) -> [(&'static str, u64, Color, char); 6] {
    let used_known = rc.deleted_open + rc.hidden;
    // When the scan saw more than statvfs reports (negative unaccounted), clip it.
    let scanned = rc.scanned.min(rc.used.saturating_sub(used_known));
    [
        ("scanned", scanned, Color::Blue, '█'),
        ("deleted-open", rc.deleted_open, Color::Magenta, '█'),
        ("hidden under mounts", rc.hidden, Color::Cyan, '█'),
        ("unaccounted", rc.unaccounted.max(0) as u64, Color::Yellow, '█'),
        ("reserved", rc.reserved, Color::Red, '▒'),
        ("free", rc.avail, Color::Green, '░'),
    ]
}

/// Integer cell widths proportional to `vals` summing to `width` (largest remainder),
/// with every non-zero value getting at least one cell when there is room.
pub fn split_widths(vals: &[u64], width: usize) -> Vec<usize> {
    let total: u128 = vals.iter().map(|&v| v as u128).sum();
    if total == 0 || width == 0 {
        return vec![0; vals.len()];
    }
    let mut w: Vec<usize> = vals.iter().map(|&v| (v as u128 * width as u128 / total) as usize).collect();
    let mut rem: Vec<(u128, usize)> =
        vals.iter().enumerate().map(|(i, &v)| ((v as u128 * width as u128) % total, i)).collect();
    rem.sort_by_key(|r| std::cmp::Reverse(r.0));
    let mut left = width - w.iter().sum::<usize>();
    for (_, i) in rem {
        if left == 0 {
            break;
        }
        w[i] += 1;
        left -= 1;
    }
    // Minimum one cell for non-zero segments, taken from the widest.
    if vals.iter().filter(|&&v| v > 0).count() <= width {
        for i in 0..vals.len() {
            if vals[i] > 0
                && w[i] == 0
                && let Some(j) = (0..w.len()).max_by_key(|&j| w[j]).filter(|&j| w[j] > 1)
            {
                w[j] -= 1;
                w[i] = 1;
            }
        }
    }
    w
}

pub fn rows(app: &App, width: u16) -> Vec<Row> {
    let snap = &app.snap;
    let mut out = Vec::new();
    let bar_w = (width as usize).saturating_sub(6).clamp(10, 120);
    out.push(Row::text(Line::styled(
        " WHERE THE SPACE GOES  (used = scanned + deleted-open + hidden + unaccounted; plus reserved and free = size)",
        theme::heading(),
    )));
    if app.reconcile.is_empty() {
        out.push(Row::text(Line::styled("   no fully scanned filesystems with statvfs data", theme::dim())));
    }
    for rc in &app.reconcile {
        let f = &snap.filesystems[rc.fs];
        let frac = rc.used as f64 / (rc.used + rc.avail).max(1) as f64;
        out.push(Row::blank());
        out.push(Row::item(
            Line::from(vec![
                Span::styled(format!("   {}", f.mount_point), theme::bold()),
                Span::styled(format!("  {}  {}", f.fstype, f.source), theme::dim()),
                Span::raw(format!("   size {}  used ", fmt_size(rc.total))),
                Span::styled(format!("{} ({:.0}%)", fmt_size(rc.used), frac * 100.0), theme::usage(frac)),
                Span::raw(format!("  free {}", fmt_size(rc.avail))),
            ]),
            f.root_node.map(Target::Node),
        ));
        let segs = segments(rc);
        let widths = split_widths(&segs.iter().map(|s| s.1).collect::<Vec<_>>(), bar_w);
        let mut bar = vec![Span::raw("   ")];
        for (s, w) in segs.iter().zip(&widths) {
            if *w > 0 {
                bar.push(Span::styled(s.3.to_string().repeat(*w), Style::new().fg(s.2)));
            }
        }
        out.push(Row::text(Line::from(bar)));
        let mut legend = vec![Span::raw("   ")];
        for (i, s) in segs.iter().enumerate() {
            let val = if s.0 == "unaccounted" { fmt_signed(rc.unaccounted) } else { fmt_size(s.1) };
            let zero = s.1 == 0 && !(s.0 == "unaccounted" && rc.unaccounted != 0);
            legend.push(Span::styled(s.3.to_string(), Style::new().fg(s.2)));
            legend.push(Span::styled(format!(" {} {val}", s.0), if zero { theme::dim() } else { Style::new() }));
            if i + 1 < segs.len() {
                legend.push(Span::raw("   "));
            }
        }
        out.push(Row::text(Line::from(legend)));
        let mut notes = Vec::new();
        if rc.unaccounted < 0 {
            notes.push(
                "scan saw more than the filesystem reports used (files changed during the scan or compression)".into(),
            );
        } else if rc.unaccounted as u64 > rc.used / 20 && rc.unaccounted > (1 << 30) {
            notes.push("large unaccounted share: filesystem metadata/journal, snapshots, unreadable dirs".to_string());
        }
        if rc.denied_dirs > 0 {
            notes.push(format!(
                "{} unreadable directories (their contents are in “unaccounted”){}",
                rc.denied_dirs,
                if snap.meta.is_root() { "" } else { " — re-run with sudo" }
            ));
        }
        for n in notes {
            out.push(Row::text(Line::styled(format!("   {n}"), theme::warn())));
        }
    }

    let skipped: Vec<_> = snap
        .filesystems
        .iter()
        .filter(|f| f.root_node.is_none() && f.statvfs.is_some_and(|s| s.used() > 0))
        .map(|f| format!("{} ({}, {})", f.mount_point, f.fstype, fmt_size(f.statvfs.map(|s| s.used()).unwrap_or(0))))
        .collect();
    if !skipped.is_empty() {
        out.push(Row::blank());
        out.push(Row::text(Line::styled(format!("   not walked: {}", skipped.join(", ")), theme::dim())));
    }

    // Deleted but open.
    out.push(Row::blank());
    let total: u64 = snap.deleted_open.iter().map(|d| d.alloc).sum();
    out.push(Row::text(Line::styled(
        format!(" DELETED BUT STILL OPEN  {} (freed when these processes close them)", fmt_size(total)),
        theme::heading(),
    )));
    if snap.deleted_open.is_empty() {
        out.push(Row::text(Line::styled("   none found", theme::dim())));
    } else {
        out.push(Row::text(Line::styled(
            format!("   {:>10}  {:>7}  {:<16} path", "size", "pid", "command"),
            theme::dim(),
        )));
        let mut v: Vec<_> = snap.deleted_open.iter().collect();
        v.sort_by_key(|d| std::cmp::Reverse(d.alloc));
        for d in v {
            out.push(Row::item(
                Line::from(vec![
                    Span::styled(format!("   {:>10}", fmt_size(d.alloc)), theme::bold()),
                    Span::raw(format!("  {:>7}  {:<16} ", d.pid, theme::trunc(&d.comm, 16))),
                    Span::raw(d.path.clone()),
                    Span::styled(
                        d.fs.map(|i| format!("  on {}", snap.filesystems[i].mount_point)).unwrap_or_default(),
                        theme::dim(),
                    ),
                ]),
                None,
            ));
        }
    }

    // Hidden under mountpoints.
    out.push(Row::blank());
    out.push(Row::text(Line::styled(" HIDDEN UNDER MOUNTPOINTS", theme::heading())));
    let hidden_report = snap.providers.iter().find(|p| p.name == "hidden-under-mounts");
    if snap.hidden.is_empty() {
        let why = match hidden_report {
            Some(p) if p.coverage != Coverage::Complete => {
                format!(
                    "   not checked: {}",
                    if p.notes.is_empty() { format!("{:?}", p.coverage) } else { p.notes.join("; ") }
                )
            }
            _ => "   none found".into(),
        };
        out.push(Row::text(Line::styled(why, theme::dim())));
    }
    for h in &snap.hidden {
        let target = snap.lookup(&h.path).map(Target::Node);
        out.push(Row::item(
            Line::from(vec![
                Span::styled(format!("   {:>10}", fmt_size(h.alloc)), theme::bold()),
                Span::raw(format!("  {}", h.path)),
                Span::styled(
                    format!("  ({} items under the mount, on {})", h.items, snap.filesystems[h.fs].mount_point),
                    theme::dim(),
                ),
            ]),
            target,
        ));
    }

    // Unreadable directories.
    let denied: Vec<_> = snap.filesystems.iter().filter(|f| f.denied_dirs > 0).collect();
    out.push(Row::blank());
    out.push(Row::text(Line::styled(" UNREADABLE DIRECTORIES", theme::heading())));
    if denied.is_empty() {
        out.push(Row::text(Line::styled("   none", theme::dim())));
    }
    for f in &denied {
        out.push(Row::item(
            Line::from(vec![
                Span::styled(format!("   {:>10}", f.denied_dirs), theme::warn()),
                Span::raw(format!("  on {}", f.mount_point)),
            ]),
            f.root_node.map(Target::Node),
        ));
    }
    if !denied.is_empty() && !snap.meta.is_root() {
        out.push(Row::text(Line::styled(
            "   This snapshot was taken as a regular user — re-run with sudo (`sudo diskeye scan`) to see inside them.",
            theme::warn(),
        )));
    }

    // Providers.
    out.push(Row::blank());
    out.push(Row::text(Line::styled(" PROVIDER COVERAGE", theme::heading())));
    for p in &snap.providers {
        out.push(Row::item(
            Line::from(vec![
                Span::raw(format!("   {:<22} ", p.name)),
                Span::styled(format!("{:<9}", format!("{:?}", p.coverage)), coverage_style(p.coverage)),
                Span::styled(format!(" {:>6}ms  ", p.duration_ms), theme::dim()),
                Span::raw(p.notes.join("; ")),
            ]),
            None,
        ));
    }
    out
}
