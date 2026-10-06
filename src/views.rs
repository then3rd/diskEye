//! View models shared by the text report, the TUI and the web UI. Each
//! function projects the snapshot into rows a frontend can render directly.

use crate::model::tree::{Kind, flags};
use crate::model::{ActionSpec, NodeId, Reconcile, Risk, Snapshot};
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub enum Metric {
    /// Bytes actually allocated on disk (st_blocks).
    #[default]
    Alloc,
    /// File sizes as reported by ls.
    Apparent,
    /// Number of entries.
    Items,
}

impl Metric {
    pub fn of(self, snap: &Snapshot, id: NodeId) -> u64 {
        let n = snap.tree.node(id);
        match self {
            Metric::Alloc => n.alloc,
            Metric::Apparent => n.apparent,
            Metric::Items => n.items as u64,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Metric::Alloc => "disk usage",
            Metric::Apparent => "apparent size",
            Metric::Items => "item count",
        }
    }
    pub fn fmt(self, v: u64) -> String {
        match self {
            Metric::Items => format!("{v}"),
            _ => crate::model::fmt_size(v),
        }
    }
}

// ---------------------------------------------------------------- Files

/// Scan roots keyed by path, for stitching mountpoints to the filesystem mounted there.
pub fn mount_targets(snap: &Snapshot) -> HashMap<String, NodeId> {
    snap.filesystems.iter().filter_map(|f| Some((f.scan_root.clone(), f.root_node?))).collect()
}

/// If `id` is a mountpoint whose filesystem was scanned, the root of that filesystem.
pub fn mount_target(snap: &Snapshot, targets: &HashMap<String, NodeId>, id: NodeId) -> Option<NodeId> {
    let n = snap.tree.node(id);
    if !n.has(flags::MOUNTPOINT) {
        return None;
    }
    targets.get(&snap.tree.path(id)).copied()
}

/// Effective size of a node, following a mountpoint to its filesystem.
pub fn effective(snap: &Snapshot, targets: &HashMap<String, NodeId>, id: NodeId, m: Metric) -> u64 {
    match mount_target(snap, targets, id) {
        Some(t) => m.of(snap, t),
        None => {
            if snap.tree.node(id).has(flags::HARDLINK_DUP) && m != Metric::Items {
                0
            } else {
                m.of(snap, id)
            }
        }
    }
}

/// The top level of the unified file view: the scanned filesystem roots that
/// aren't reachable by stitching from another root.
pub fn top_roots(snap: &Snapshot) -> Vec<NodeId> {
    let mut roots: Vec<(String, NodeId)> =
        snap.filesystems.iter().filter_map(|f| Some((f.scan_root.clone(), f.root_node?))).collect();
    roots.sort();
    let all: Vec<String> = roots.iter().map(|(p, _)| p.clone()).collect();
    roots
        .into_iter()
        .filter(|(p, _)| {
            // Reachable if some other root is a proper prefix of it.
            !all.iter().any(|o| o != p && crate::scan::mounts::is_path_under(p, o))
        })
        .map(|(_, n)| n)
        .collect()
}

/// Short badges describing a node: owner entity, flags.
pub fn badges(snap: &Snapshot, by_node: &HashMap<NodeId, Vec<u32>>, id: NodeId) -> Vec<String> {
    let n = snap.tree.node(id);
    let mut b = Vec::new();
    if let Some(es) = by_node.get(&id) {
        let first = &snap.entities[es[0] as usize];
        let mut label = format!("{}: {}", kind_label(&first.kind), first.name);
        if es.len() > 1 {
            label.push_str(&format!(" +{} more", es.len() - 1));
        }
        b.push(label);
    }
    if n.has(flags::MOUNTPOINT) {
        b.push("mountpoint".into());
    }
    if n.has(flags::DENIED) {
        b.push("permission denied".into());
    } else if n.has(flags::INCOMPLETE) && n.kind == Kind::Dir {
        b.push("incomplete".into());
    }
    if n.has(flags::HARDLINK_DUP) {
        b.push("hardlink (counted elsewhere)".into());
    }
    if n.has(flags::SPARSE) {
        b.push("sparse".into());
    }
    b
}

pub fn kind_label(kind: &str) -> &str {
    match kind {
        "docker.image" => "docker image",
        "docker.layer" => "docker layer",
        "docker.container" => "container",
        "docker.volume" => "docker volume",
        "docker.buildcache" => "build cache",
        "vm" => "VM",
        "vm.disk" => "VM disk",
        "vm.backing" => "VM base image",
        "vm.iso" => "ISO",
        "vm.image" => "disk image",
        "vm.folder" => "VM folder",
        "lv" => "logical volume",
        "cache" => "cache",
        "pkgcache" => "package cache",
        "devcache" => "dev cache",
        "build-artifacts" => "build artifacts",
        "models" => "AI models",
        "crash" => "crash dumps",
        "coredump" => "core dumps",
        "kernel" => "old kernel",
        "snapshots" => "fs snapshots",
        "tmp" => "temp files",
        "logs" | "logs.rotated" => "logs",
        "logs.journal" => "systemd journal",
        "flatpak.app" => "flatpak app",
        "flatpak.runtime" => "flatpak runtime",
        "flatpak.repo" => "flatpak repo",
        "flatpak.installation" => "flatpak",
        "snap" => "snap",
        "snap.revision" => "snap revision",
        "snap.data" => "snap data",
        "vagrant.box" | "vagrant.box.version" => "vagrant box",
        k if k.starts_with("incus.") => "incus",
        "docker.log" => "container log",
        "docker.buildcache.record" | "docker.buildkit" => "build cache",
        "kube.pod" | "kube.pods" => "k8s pod",
        "kube.pv" | "kube.pvs" => "k8s volume",
        // Generic container-engine kinds: "<engine>.<thing>".
        k => match k.rsplit('.').next().unwrap_or(k) {
            "image" | "images" => "image",
            "container" | "containers" => "container",
            "volume" | "volumes" => "volume",
            "layers" => "image layers",
            "other" | "misc" | "data" | "content" | "snapshots" => "engine data",
            _ => k,
        },
    }
}

// ---------------------------------------------------------------- Physical

#[derive(Debug, Clone, Serialize)]
pub struct PhysRow {
    pub depth: usize,
    /// disk, part, lvm, crypt, loop, vg, free, swap, fs...
    pub kind: String,
    pub label: String,
    pub size: u64,
    pub used: Option<u64>,
    pub avail: Option<u64>,
    pub mount: Option<String>,
    pub fstype: Option<String>,
    pub note: String,
    /// Draw attention: unused space, unmounted volumes, nearly-full filesystems.
    pub flag: bool,
}

pub fn physical(snap: &Snapshot) -> Vec<PhysRow> {
    let mut rows = Vec::new();
    let mut tops: Vec<&crate::model::BlockDev> = snap.block.iter().filter(|d| d.parents.is_empty()).collect();
    tops.sort_by(|a, b| (a.dtype != "disk", &a.kname).cmp(&(b.dtype != "disk", &b.kname)));
    let mut seen = std::collections::HashSet::new();
    for d in tops {
        phys_dev(snap, d, 0, &mut rows, &mut seen);
    }
    for sw in &snap.swaps {
        if sw.kind == "file" {
            rows.push(PhysRow {
                depth: 0,
                kind: "swap".into(),
                label: format!("swapfile {}", sw.path),
                size: sw.size,
                used: Some(sw.used),
                avail: None,
                mount: None,
                fstype: None,
                note: "swap file".into(),
                flag: false,
            });
        }
    }
    rows
}

fn phys_dev(
    snap: &Snapshot,
    d: &crate::model::BlockDev,
    depth: usize,
    rows: &mut Vec<PhysRow>,
    seen: &mut std::collections::HashSet<String>,
) {
    if !seen.insert(d.kname.clone()) {
        return;
    }
    let fs = d.mountpoints.iter().find_map(|m| snap.filesystems.iter().find(|f| &f.mount_point == m));
    let (used, avail) = match fs.and_then(|f| f.statvfs) {
        Some(sv) => (Some(sv.used()), Some(sv.avail)),
        None => (d.fs_used, d.fs_avail),
    };
    let children: Vec<&crate::model::BlockDev> =
        snap.block.iter().filter(|c| c.parents.iter().any(|p| p == &d.kname)).collect();
    let mut note = d.used_by.join(", ");
    let mut flag = false;
    let unused = children.is_empty() && d.mountpoints.is_empty() && d.used_by.is_empty() && d.dtype != "rom";
    if unused {
        note = if d.fstype.is_some() {
            format!("{} filesystem, not mounted", d.fstype.as_deref().unwrap_or(""))
        } else {
            "no filesystem, not mounted — unused or raw VM/database disk?".into()
        };
        flag = true;
    }
    if let (Some(u), Some(a)) = (used, avail)
        && u + a > 0
        && a * 100 / (u + a) < 10
    {
        flag = true;
        if !note.is_empty() {
            note.push_str(", ");
        }
        note.push_str("less than 10% free");
    }
    let label = match (&d.model, d.dtype.as_str()) {
        (Some(m), "disk") => format!("{} ({m})", d.name),
        _ => d.name.clone(),
    };
    rows.push(PhysRow {
        depth,
        kind: d.dtype.clone(),
        label,
        size: d.size,
        used,
        avail,
        mount: d.mountpoints.first().cloned(),
        fstype: d.fstype.clone(),
        note,
        flag,
    });
    if d.dtype == "disk" {
        let gap = crate::providers::block::unpartitioned(snap, d);
        if gap > 0 {
            rows.push(free_row(depth + 1, "unpartitioned space", gap));
        }
    }
    if d.fstype.as_deref() == Some("LVM2_member") {
        let pv = snap.lvm.pvs.iter().find(|p| p.name == d.path || p.name.ends_with(&format!("/{}", d.kname)));
        if let Some(vg) = pv.and_then(|p| snap.lvm.vgs.iter().find(|v| v.name == p.vg)) {
            rows.push(PhysRow {
                depth: depth + 1,
                kind: "vg".into(),
                label: format!("VG {}", vg.name),
                size: vg.size,
                used: Some(vg.size.saturating_sub(vg.free)),
                avail: Some(vg.free),
                mount: None,
                fstype: None,
                note: if vg.free_is_estimate {
                    "free space estimated (run as root for exact)".into()
                } else {
                    String::new()
                },
                flag: false,
            });
            if vg.free > (64 << 20) {
                rows.push(free_row(depth + 2, &format!("unallocated in VG {}", vg.name), vg.free));
            }
            for c in children {
                phys_dev(snap, c, depth + 2, rows, seen);
            }
            return;
        }
    }
    for c in children {
        phys_dev(snap, c, depth + 1, rows, seen);
    }
}

/// Logical volumes that are not mounted, not swap, and not used by any known
/// VM or other entity (candidates for "forgotten" volumes).
pub fn unused_lvs(snap: &Snapshot) -> Vec<&crate::model::Lv> {
    snap.lvm
        .lvs
        .iter()
        .filter(|l| l.mountpoints.is_empty() && l.fstype.as_deref() != Some("swap"))
        .filter(|l| {
            let k = l.kname.as_deref().unwrap_or("");
            !snap.block.iter().any(|d| d.kname == k && !d.used_by.is_empty())
                && !snap.entities.iter().any(|e| e.block_devs.iter().any(|b| b == k))
        })
        .collect()
}

fn free_row(depth: usize, label: &str, size: u64) -> PhysRow {
    PhysRow {
        depth,
        kind: "free".into(),
        label: label.into(),
        size,
        used: None,
        avail: None,
        mount: None,
        fstype: None,
        note: "available to allocate".into(),
        flag: true,
    }
}

// ---------------------------------------------------------------- Workloads

#[derive(Debug, Clone, Serialize)]
pub struct WorkloadGroup {
    pub name: String,
    pub total: u64,
    pub unique: u64,
    pub reclaimable: u64,
    /// Top-level entities in this group, largest first.
    pub entities: Vec<u32>,
}

/// Entities with no parent, grouped by `group`.
pub fn workloads(snap: &Snapshot) -> Vec<WorkloadGroup> {
    let mut groups: Vec<WorkloadGroup> = Vec::new();
    for e in snap.entities.iter().filter(|e| e.parent.is_none()) {
        let g = match groups.iter_mut().position(|g| g.name == e.group) {
            Some(i) => &mut groups[i],
            None => {
                groups.push(WorkloadGroup {
                    name: e.group.clone(),
                    total: 0,
                    unique: 0,
                    reclaimable: 0,
                    entities: vec![],
                });
                groups.last_mut().unwrap()
            }
        };
        g.entities.push(e.id);
    }
    for g in groups.iter_mut() {
        g.entities.sort_by_key(|&id| std::cmp::Reverse(snap.entities[id as usize].total_bytes()));
        g.total = group_total(snap, &g.entities);
        g.unique = g.entities.iter().map(|&i| snap.entities[i as usize].unique_bytes()).sum();
        g.reclaimable =
            reclaim(snap).iter().filter(|r| snap.entities[r.entity as usize].group == g.name).map(|r| r.bytes).sum();
    }
    groups.sort_by_key(|g| std::cmp::Reverse(g.total));
    groups
}

/// Total bytes of top-level entities without double counting overlapping paths.
fn group_total(snap: &Snapshot, ids: &[u32]) -> u64 {
    let mut nodes: Vec<NodeId> = Vec::new();
    let mut ext = 0;
    for &id in ids {
        let e = &snap.entities[id as usize];
        ext += e.external_bytes;
        let mut stack = vec![id];
        while let Some(cur) = stack.pop() {
            nodes.extend(snap.claims.iter().filter(|c| c.entity == cur).map(|c| c.node));
            stack.extend(snap.entities.iter().filter(|c| c.parent == Some(cur)).map(|c| c.id));
        }
    }
    nodes.sort_unstable();
    nodes.dedup();
    let set: std::collections::HashSet<NodeId> = nodes.iter().copied().collect();
    let bytes: u64 = nodes
        .iter()
        .filter(|&&n| !snap.tree.ancestors(n).skip(1).any(|a| set.contains(&a)))
        .map(|&n| snap.tree.node(n).alloc)
        .sum();
    bytes + ext
}

pub fn child_entities(snap: &Snapshot, id: u32) -> Vec<u32> {
    let mut v: Vec<u32> = snap.entities.iter().filter(|e| e.parent == Some(id)).map(|e| e.id).collect();
    v.sort_by_key(|&i| std::cmp::Reverse(snap.entities[i as usize].total_bytes()));
    v
}

// ---------------------------------------------------------------- Reclaim

#[derive(Debug, Clone, Serialize)]
pub struct ReclaimItem {
    pub entity: u32,
    pub risk: Risk,
    pub bytes: u64,
    pub reason: String,
    pub action: Option<ActionSpec>,
}

pub fn reclaim(snap: &Snapshot) -> Vec<ReclaimItem> {
    let mut v: Vec<ReclaimItem> = snap
        .entities
        .iter()
        .filter_map(|e| {
            let r = e.reclaim.as_ref()?;
            let bytes = r.estimate.unwrap_or_else(|| e.unique_bytes());
            (bytes > 0).then(|| ReclaimItem {
                entity: e.id,
                risk: r.risk,
                bytes,
                reason: r.reason.clone(),
                action: r.action.clone(),
            })
        })
        .collect();
    v.sort_by_key(|r| (r.risk, std::cmp::Reverse(r.bytes)));
    v
}

// ---------------------------------------------------------------- Hotspots

/// The most specific large directories: at least `min_share` of their
/// filesystem, and not mostly explained by a single child.
pub fn hotspots(snap: &Snapshot, limit: usize) -> Vec<NodeId> {
    let mut out = Vec::new();
    for f in &snap.filesystems {
        let Some(root) = f.root_node else { continue };
        let total = snap.tree.node(root).alloc.max(1);
        let min = (total / 100).max(256 << 20);
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let n = snap.tree.node(id);
            if n.alloc < min || n.kind != Kind::Dir {
                continue;
            }
            let big_kids: Vec<NodeId> = snap.tree.children(id).filter(|&c| snap.tree.node(c).alloc >= min).collect();
            let dominant = big_kids.iter().any(|&c| snap.tree.node(c).alloc * 10 >= n.alloc * 7);
            if !dominant && id != root {
                out.push(id);
            }
            stack.extend(big_kids);
        }
    }
    out.sort_by_key(|&id| std::cmp::Reverse(snap.tree.node(id).alloc));
    out.truncate(limit);
    out
}

pub fn reconcile(snap: &Snapshot) -> Vec<Reconcile> {
    snap.reconcile()
}
