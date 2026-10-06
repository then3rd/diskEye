//! Plain-text and JSON summary of a snapshot.

use crate::model::{Coverage, Snapshot, fmt_signed, fmt_size};
use crate::views;
use serde::Serialize;
use std::fmt::Write;

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const YEL: &str = "\x1b[33m";
const GRN: &str = "\x1b[32m";
const CYA: &str = "\x1b[36m";
const RST: &str = "\x1b[0m";

struct Style {
    on: bool,
}

impl Style {
    fn c(&self, code: &'static str) -> &'static str {
        if self.on { code } else { "" }
    }
}

fn bar(frac: f64, width: usize) -> String {
    let filled = (frac.clamp(0.0, 1.0) * width as f64).round() as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

pub fn text(snap: &Snapshot, color: bool, top: usize) -> String {
    let s = Style { on: color };
    let (b, d, r, y, g, c, x) = (s.c(BOLD), s.c(DIM), s.c(RED), s.c(YEL), s.c(GRN), s.c(CYA), s.c(RST));
    let mut o = String::new();
    let m = &snap.meta;
    let _ = writeln!(
        o,
        "{b}diskeye{x} {} · {} · scanned in {:.1}s as {}{}",
        m.host,
        crate::util::timestamp_human(m.started),
        m.duration_ms as f64 / 1000.0,
        if m.is_root() { "root" } else { "user" },
        if m.is_root() { "" } else { " (run with sudo for the full picture)" }
    );

    // Physical
    let _ = writeln!(o, "\n{b}PHYSICAL{x}");
    for row in views::physical(snap) {
        let indent = "  ".repeat(row.depth);
        let usage = match (row.used, row.avail) {
            (Some(u), Some(a)) if u + a > 0 => {
                format!("{} {:>9} used {:>9} free", bar(u as f64 / (u + a) as f64, 12), fmt_size(u), fmt_size(a))
            }
            _ => String::new(),
        };
        let mount = row.mount.as_deref().map(|m| format!(" → {m}")).unwrap_or_default();
        let fstype = row.fstype.as_deref().map(|t| format!(" [{t}]")).unwrap_or_default();
        let color = if row.kind == "free" {
            g
        } else if row.flag {
            y
        } else {
            ""
        };
        let label = format!("{indent}{}{fstype}{mount}", row.label);
        let _ = writeln!(o, "  {color}{label:<52}{x} {:>10}  {usage}", fmt_size(row.size));
        if !row.note.is_empty() {
            let _ = writeln!(o, "  {indent}  {d}{}{x}", row.note);
        }
    }
    for l in views::unused_lvs(snap) {
        let _ = writeln!(
            o,
            "  {y}! LV {}/{} ({}) is not mounted and not used by any known VM{x}",
            l.vg,
            l.name,
            fmt_size(l.size)
        );
    }

    // Filesystems & reconciliation
    let _ = writeln!(o, "\n{b}FILESYSTEMS{x}  {d}(used = scanned + deleted-open + hidden + unaccounted){x}");
    let _ = writeln!(
        o,
        "  {d}{:<24} {:>8} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}{x}",
        "mount", "type", "size", "used", "free", "reserved", "scanned", "unaccntd"
    );
    for rc in snap.reconcile() {
        let f = &snap.filesystems[rc.fs];
        let pct = rc.used as f64 / (rc.used + rc.avail).max(1) as f64;
        let col = if pct > 0.9 {
            r
        } else if pct > 0.8 {
            y
        } else {
            ""
        };
        let _ = writeln!(
            o,
            "  {col}{:<24}{x} {:>8} {:>10} {col}{:>10}{x} {:>10} {:>10} {:>10} {:>10}",
            f.mount_point,
            f.fstype,
            fmt_size(rc.total),
            fmt_size(rc.used),
            fmt_size(rc.avail),
            fmt_size(rc.reserved),
            fmt_size(rc.scanned),
            fmt_signed(rc.unaccounted),
        );
        let mut extra = Vec::new();
        if rc.deleted_open > 0 {
            extra.push(format!("{} in deleted-but-open files", fmt_size(rc.deleted_open)));
        }
        if rc.hidden > 0 {
            extra.push(format!("{} hidden under mountpoints", fmt_size(rc.hidden)));
        }
        if rc.denied_dirs > 0 {
            extra.push(format!("{} unreadable dirs (counted in unaccounted)", rc.denied_dirs));
        }
        if !extra.is_empty() {
            let _ = writeln!(o, "  {d}{:<24} {}{x}", "", extra.join(" · "));
        }
    }
    let skipped: Vec<String> = snap
        .filesystems
        .iter()
        .filter(|f| f.root_node.is_none() && f.statvfs.is_some_and(|s| s.used() > 0))
        .map(|f| format!("{} ({}, {})", f.mount_point, f.fstype, fmt_size(f.statvfs.map(|s| s.used()).unwrap_or(0))))
        .collect();
    if !skipped.is_empty() {
        let _ = writeln!(o, "  {d}not walked: {}{x}", skipped.join(", "));
    }

    // Workloads
    let groups = views::workloads(snap);
    if !groups.is_empty() {
        let _ = writeln!(o, "\n{b}WORKLOADS{x}");
        let max = groups.first().map(|g| g.total).unwrap_or(1).max(1);
        for g in &groups {
            // Detected but essentially empty (e.g. a root-only data dir seen as a user).
            if g.total < 1 << 20 {
                continue;
            }
            let recl = if g.reclaimable > 0 {
                format!("  {}{} reclaimable{x}", s.c(GRN), fmt_size(g.reclaimable))
            } else {
                String::new()
            };
            let _ = writeln!(
                o,
                "  {c}{:<40}{x} {:>10} {}{recl}",
                g.name,
                fmt_size(g.total),
                bar(g.total as f64 / max as f64, 20)
            );
            for &id in g.entities.iter().take(5) {
                let e = &snap.entities[id as usize];
                if e.total_bytes() == 0 {
                    continue;
                }
                let shared = e.total_bytes().saturating_sub(e.unique_bytes());
                let sh = if shared > 0 { format!(" {d}({} shared){x}", fmt_size(shared)) } else { String::new() };
                let _ = writeln!(o, "    {:<38} {:>10}{sh}", truncate(&e.name, 38), fmt_size(e.total_bytes()));
            }
            if g.entities.len() > 5 {
                let _ = writeln!(o, "    {d}… {} more{x}", g.entities.len() - 5);
            }
        }
    }

    // Reclaim
    let items = views::reclaim(snap);
    if !items.is_empty() {
        let total_safe: u64 = items.iter().filter(|i| i.risk == crate::model::Risk::Safe).map(|i| i.bytes).sum();
        let _ = writeln!(o, "\n{b}RECLAIMABLE{x}  {g}{} safe to free{x}", fmt_size(total_safe));
        for it in items.iter().take(top) {
            let e = &snap.entities[it.entity as usize];
            let rc = match it.risk {
                crate::model::Risk::Safe => g,
                crate::model::Risk::Review => y,
                crate::model::Risk::Danger => r,
            };
            let _ = writeln!(
                o,
                "  {:>3} {rc}{:<7}{x} {:>10}  {:<44} {d}{}{x}",
                e.id,
                it.risk.label(),
                fmt_size(it.bytes),
                truncate(&format!("{}: {}", views::kind_label(&e.kind), e.name), 44),
                it.reason
            );
        }
        if items.len() > top {
            let _ = writeln!(o, "  {d}… {} more (diskeye clean --list){x}", items.len() - top);
        }
    }

    // Hotspots
    let hs = views::hotspots(snap, top);
    if !hs.is_empty() {
        let by_node = crate::model::attribution::claims_by_node(snap);
        let _ = writeln!(o, "\n{b}HEAVIEST DIRECTORIES{x}");
        for id in hs {
            let owners = crate::model::attribution::owners(snap, &by_node, id);
            let owner =
                owners.first().map(|&e| format!("  {d}[{}]{x}", snap.entities[e as usize].name)).unwrap_or_default();
            let _ = writeln!(o, "  {:>10}  {}{owner}", fmt_size(snap.tree.node(id).alloc), snap.tree.path(id));
        }
    }

    // Deleted-open
    if !snap.deleted_open.is_empty() {
        let total: u64 = snap.deleted_open.iter().map(|d| d.alloc).sum();
        if total > 1 << 20 {
            let _ = writeln!(
                o,
                "\n{b}DELETED BUT STILL OPEN{x}  {} (freed when these processes close them)",
                fmt_size(total)
            );
            for dl in snap.deleted_open.iter().take(5) {
                let _ = writeln!(o, "  {:>10}  pid {} ({})  {}", fmt_size(dl.alloc), dl.pid, dl.comm, dl.path);
            }
        }
    }
    if !snap.hidden.is_empty() {
        let _ = writeln!(o, "\n{b}HIDDEN UNDER MOUNTPOINTS{x}");
        for h in &snap.hidden {
            let _ = writeln!(
                o,
                "  {:>10}  {} ({} items under the mount on {})",
                fmt_size(h.alloc),
                h.path,
                h.items,
                snap.filesystems[h.fs].mount_point
            );
        }
    }

    // Coverage
    let gaps: Vec<_> =
        snap.providers.iter().filter(|p| matches!(p.coverage, Coverage::Partial | Coverage::Denied)).collect();
    if !gaps.is_empty() {
        let _ = writeln!(o, "\n{b}COVERAGE GAPS{x}");
        for p in gaps {
            let _ = writeln!(o, "  {y}{:<20}{x} {:?}: {}", p.name, p.coverage, p.notes.join("; "));
        }
    }
    o
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
    t.push('…');
    t
}

#[derive(Serialize)]
struct JsonReport<'a> {
    meta: &'a crate::model::ScanMeta,
    physical: Vec<views::PhysRow>,
    lvm: &'a crate::model::LvmInfo,
    filesystems: Vec<serde_json::Value>,
    workloads: Vec<serde_json::Value>,
    reclaim: Vec<serde_json::Value>,
    hotspots: Vec<serde_json::Value>,
    deleted_open: &'a [crate::model::DeletedOpen],
    hidden: &'a [crate::model::HiddenUnderMount],
    providers: &'a [crate::model::ProviderReport],
}

pub fn json(snap: &Snapshot, top: usize) -> serde_json::Value {
    let rec = snap.reconcile();
    let filesystems = snap
        .filesystems
        .iter()
        .enumerate()
        .map(|(i, f)| {
            serde_json::json!({
                "mount_point": f.mount_point, "fstype": f.fstype, "source": f.source,
                "statvfs": f.statvfs, "scanned_alloc": f.scanned_alloc, "scanned_items": f.scanned_items,
                "skipped_reason": f.skipped_reason, "aliases": f.aliases,
                "reconcile": rec.iter().find(|r| r.fs == i),
            })
        })
        .collect();
    let entity_json = |id: u32| {
        let e = &snap.entities[id as usize];
        serde_json::json!({
            "id": e.id, "kind": e.kind, "name": e.name, "total": e.total_bytes(), "unique": e.unique_bytes(),
            "reported": e.reported, "virtual_size": e.virtual_size, "paths": e.paths, "attrs": e.attrs,
            "children": views::child_entities(snap, id).len(),
        })
    };
    let workloads = views::workloads(snap)
        .into_iter()
        .map(|g| {
            serde_json::json!({
                "group": g.name, "total": g.total, "unique": g.unique, "reclaimable": g.reclaimable,
                "entities": g.entities.iter().map(|&i| entity_json(i)).collect::<Vec<_>>(),
            })
        })
        .collect();
    let reclaim = views::reclaim(snap)
        .into_iter()
        .map(|r| {
            let e = &snap.entities[r.entity as usize];
            serde_json::json!({"entity": r.entity, "kind": e.kind, "name": e.name, "group": e.group,
                "risk": r.risk.label(), "bytes": r.bytes, "reason": r.reason, "action": r.action})
        })
        .collect();
    let hotspots = views::hotspots(snap, top)
        .into_iter()
        .map(|id| serde_json::json!({"path": snap.tree.path(id), "alloc": snap.tree.node(id).alloc, "items": snap.tree.node(id).items}))
        .collect();
    serde_json::to_value(JsonReport {
        meta: &snap.meta,
        physical: views::physical(snap),
        lvm: &snap.lvm,
        filesystems,
        workloads,
        reclaim,
        hotspots,
        deleted_open: &snap.deleted_open,
        hidden: &snap.hidden,
        providers: &snap.providers,
    })
    .unwrap_or_default()
}

pub fn diff_text(d: &crate::model::diff::DiffReport, color: bool) -> String {
    let s = Style { on: color };
    let (b, dm, r, g, x) = (s.c(BOLD), s.c(DIM), s.c(RED), s.c(GRN), s.c(RST));
    let mut o = String::new();
    let _ = writeln!(
        o,
        "{b}diff{x} {} → {}",
        crate::util::timestamp_human(d.old_time),
        crate::util::timestamp_human(d.new_time)
    );
    let col = |v: i64| if v > 0 { r } else { g };
    let _ = writeln!(o, "\n{b}FILESYSTEMS{x}");
    let mut unchanged = 0;
    for f in &d.filesystems {
        let dl = f.new_used as i64 - f.old_used as i64;
        if dl.unsigned_abs() < 1 << 20 {
            unchanged += 1;
            continue;
        }
        let _ = writeln!(
            o,
            "  {:<28} {:>10} → {:>10}  {}{:>10}{x}",
            f.mount_point,
            fmt_size(f.old_used),
            fmt_size(f.new_used),
            col(dl),
            fmt_signed(dl)
        );
    }
    if unchanged > 0 {
        let _ = writeln!(o, "  {dm}{unchanged} filesystems changed by less than 1 MiB{x}");
    }
    if !d.hotspots.is_empty() {
        let _ = writeln!(o, "\n{b}WHERE IT CHANGED{x}");
        for h in d.hotspots.iter().take(30) {
            let owner = h.owner.as_deref().map(|w| format!("  {dm}[{w}]{x}")).unwrap_or_default();
            let _ = writeln!(o, "  {}{:>10}{x}  {}{owner}", col(h.delta()), fmt_signed(h.delta()), h.path);
        }
    }
    if !d.entities.is_empty() {
        let _ = writeln!(o, "\n{b}WORKLOADS{x}");
        for e in d.entities.iter().take(30) {
            let dl = e.new as i64 - e.old as i64;
            let _ = writeln!(o, "  {}{:>10}{x}  {} · {}", col(dl), fmt_signed(dl), e.group, e.name);
        }
    }
    o
}
