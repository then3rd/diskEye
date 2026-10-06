//! The Physical tab: block devices → LVM → filesystems, plus warnings,
//! a filesystem summary and the heaviest directories (hotspots).

use super::app::App;
use super::lineview::{Row, Target};
use super::theme;
use crate::model::{fmt_size, tree::Kind};
use crate::views;
use ratatui::text::{Line, Span};

pub fn rows(app: &App, width: u16) -> Vec<Row> {
    let snap = &app.snap;
    let w = width as usize;
    let label_w = (w * 2 / 5).clamp(24, 52);
    let mut out = Vec::new();
    out.push(Row::text(Line::styled(" PHYSICAL LAYOUT", theme::heading())));
    if app.phys_rows.is_empty() {
        out.push(Row::text(Line::styled("   no block devices recorded", theme::dim())));
    }
    for r in &app.phys_rows {
        let indent = "  ".repeat(r.depth);
        let glyph = if r.depth > 0 { "└ " } else { "" };
        let fstype = r.fstype.as_deref().map(|t| format!(" [{t}]")).unwrap_or_default();
        let label = format!(" {indent}{glyph}{}{fstype}", r.label);
        let base = if r.kind == "free" {
            theme::good()
        } else if r.flag {
            theme::warn()
        } else {
            ratatui::style::Style::new()
        };
        let mut spans = vec![
            Span::styled(format!(" {:<5}", theme::trunc(&r.kind, 5)), theme::dim()),
            Span::styled(theme::pad(&label, label_w), base),
            Span::styled(format!("{:>10} ", fmt_size(r.size)), theme::bold()),
        ];
        match (r.used, r.avail) {
            (Some(u), Some(a)) if u + a > 0 => {
                let f = u as f64 / (u + a) as f64;
                spans.extend(theme::bar(f, 12, theme::usage(f)));
                spans.push(Span::styled(format!(" {:>3.0}%", f * 100.0), theme::usage(f)));
                spans.push(Span::styled(format!(" {:>9} free", fmt_size(a)), theme::dim()));
            }
            _ => spans.push(Span::raw(" ".repeat(27))),
        }
        if let Some(m) = &r.mount {
            spans.push(Span::styled(format!("  → {m}"), theme::accent()));
        }
        let target = r.mount.as_deref().and_then(|m| mount_node(app, m)).map(Target::Node);
        out.push(Row::item(Line::from(spans), target));
        if !r.note.is_empty() {
            let style = if r.flag { theme::warn() } else { theme::dim() };
            out.push(Row::text(Line::styled(format!("       {indent}  {}", r.note), style)));
        }
    }

    // Warnings the tree doesn't show directly.
    let unused = views::unused_lvs(snap);
    let skipped: Vec<_> =
        snap.filesystems.iter().filter(|f| f.root_node.is_none() && f.statvfs.is_some_and(|s| s.used() > 0)).collect();
    if !unused.is_empty() || !skipped.is_empty() {
        out.push(Row::blank());
        out.push(Row::text(Line::styled(" ATTENTION", theme::heading())));
        for l in unused {
            out.push(Row::text(Line::styled(
                format!(
                    "  ! LV {}/{} ({}) is not mounted and not used by any known VM",
                    l.vg,
                    l.name,
                    fmt_size(l.size)
                ),
                theme::warn(),
            )));
        }
        for f in skipped {
            out.push(Row::text(Line::styled(
                format!(
                    "  · {} ({}, {} used) was not walked{}",
                    f.mount_point,
                    f.fstype,
                    fmt_size(f.statvfs.map(|s| s.used()).unwrap_or(0)),
                    f.skipped_reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default()
                ),
                theme::dim(),
            )));
        }
    }

    // Filesystems.
    let scanned: Vec<_> = snap.filesystems.iter().filter(|f| f.root_node.is_some()).collect();
    if !scanned.is_empty() {
        out.push(Row::blank());
        out.push(Row::text(Line::styled(" SCANNED FILESYSTEMS", theme::heading())));
        let mw = (w / 4).clamp(16, 40);
        for f in scanned {
            let mut spans = vec![
                Span::raw(format!("   {} ", theme::pad(&f.scan_root, mw))),
                Span::styled(format!("{:<8}", theme::trunc(&f.fstype, 8)), theme::dim()),
            ];
            if let Some(sv) = f.statvfs {
                let (u, a) = (sv.used(), sv.avail);
                let frac = u as f64 / (u + a).max(1) as f64;
                spans.push(Span::styled(format!("{:>10} ", fmt_size(sv.total)), theme::bold()));
                spans.extend(theme::bar(frac, 12, theme::usage(frac)));
                spans.push(Span::styled(format!(" {:>3.0}%", frac * 100.0), theme::usage(frac)));
                spans.push(Span::styled(format!(" {:>9} free", fmt_size(a)), theme::dim()));
            }
            spans.push(Span::styled(format!("  scanned {}", fmt_size(f.scanned_alloc)), theme::dim()));
            if f.denied_dirs > 0 {
                spans.push(Span::styled(format!("  {} unreadable dirs", f.denied_dirs), theme::warn()));
            }
            out.push(Row::item(Line::from(spans), f.root_node.map(Target::Node)));
        }
    }

    // Hotspots.
    out.push(Row::blank());
    out.push(Row::text(Line::styled(
        " HEAVIEST DIRECTORIES  (most specific large dirs; Enter opens in Files)",
        theme::heading(),
    )));
    if app.hotspots.is_empty() {
        out.push(Row::text(Line::styled("   none above the threshold (1% of a filesystem, ≥256 MiB)", theme::dim())));
    }
    for &id in &app.hotspots {
        let n = snap.tree.node(id);
        let owners = crate::model::attribution::owners(snap, &app.by_node, id);
        let mut spans = vec![
            Span::styled(format!("   {:>10}  ", fmt_size(n.alloc)), theme::bold()),
            Span::raw(snap.tree.path(id)),
            Span::styled(format!("  {} items", n.items), theme::dim()),
        ];
        if let Some(&e) = owners.first() {
            spans.push(Span::styled(format!("  [{}]", snap.entities[e as usize].name), theme::owner()));
        }
        out.push(Row::item(Line::from(spans), Some(Target::Node(id))));
    }
    out
}

/// The scanned node for a mountpoint path, preferring the filesystem root.
pub fn mount_node(app: &App, m: &str) -> Option<crate::model::NodeId> {
    let snap = &app.snap;
    snap.filesystems
        .iter()
        .find(|f| f.mount_point == m && f.root_node.is_some())
        .and_then(|f| f.root_node)
        .or_else(|| snap.lookup(m).filter(|&n| snap.tree.node(n).kind == Kind::Dir))
}
