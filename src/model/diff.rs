//! Compare two snapshots: per-filesystem usage, growth hotspots, entity deltas.

use super::{Snapshot, tree::Kind};
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize)]
pub struct FsDelta {
    pub mount_point: String,
    pub old_used: u64,
    pub new_used: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PathDelta {
    pub path: String,
    pub old: u64,
    pub new: u64,
    /// Owning entity in the new snapshot, for context ("docker image foo").
    pub owner: Option<String>,
}

impl PathDelta {
    pub fn delta(&self) -> i64 {
        self.new as i64 - self.old as i64
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityDelta {
    pub group: String,
    pub kind: String,
    pub name: String,
    pub old: u64,
    pub new: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffReport {
    pub old_time: i64,
    pub new_time: i64,
    pub filesystems: Vec<FsDelta>,
    /// Most specific paths that explain growth/shrinkage, largest |delta| first.
    pub hotspots: Vec<PathDelta>,
    pub entities: Vec<EntityDelta>,
}

/// Directory (and large file) sizes keyed by absolute path.
fn size_map(snap: &Snapshot, min_file: u64) -> HashMap<String, u64> {
    let mut m = HashMap::new();
    for &root in &snap.tree.roots {
        let mut stack = vec![(root, snap.tree.path(root))];
        while let Some((id, path)) = stack.pop() {
            let n = snap.tree.node(id);
            if n.kind == Kind::Dir || n.alloc >= min_file {
                m.insert(path.clone(), n.alloc);
            }
            if n.kind == Kind::Dir {
                for c in snap.tree.children(id) {
                    let cn = snap.tree.node(c);
                    if cn.kind == Kind::Dir || cn.alloc >= min_file {
                        let name = snap.tree.name(c);
                        let p = if path.ends_with('/') { format!("{path}{name}") } else { format!("{path}/{name}") };
                        stack.push((c, p));
                    }
                }
            }
        }
    }
    m
}

fn parent_of(p: &str) -> Option<&str> {
    let i = p.rfind('/')?;
    if i == 0 { (p.len() > 1).then_some("/") } else { Some(&p[..i]) }
}

pub fn diff(old: &Snapshot, new: &Snapshot, threshold: u64) -> DiffReport {
    let mut filesystems: Vec<FsDelta> = new
        .filesystems
        .iter()
        .filter_map(|f| {
            let nu = f.statvfs?.used();
            let ou = old
                .filesystems
                .iter()
                .find(|o| o.mount_point == f.mount_point)
                .and_then(|o| o.statvfs)
                .map(|s| s.used())
                .unwrap_or(0);
            Some(FsDelta { mount_point: f.mount_point.clone(), old_used: ou, new_used: nu })
        })
        .collect();
    filesystems.sort_by_key(|d| std::cmp::Reverse((d.new_used as i64 - d.old_used as i64).unsigned_abs()));

    let min_file = threshold.max(1 << 20);
    let om = size_map(old, min_file);
    let nm = size_map(new, min_file);
    let mut deltas: HashMap<&str, (u64, u64)> = HashMap::new();
    for (p, &s) in &nm {
        deltas.insert(p.as_str(), (om.get(p).copied().unwrap_or(0), s));
    }
    for (p, &s) in &om {
        deltas.entry(p.as_str()).or_insert((s, 0));
    }
    // Largest same-direction child delta for each parent.
    let mut max_child: HashMap<&str, (i64, i64)> = HashMap::new();
    for (&p, &(o, n)) in &deltas {
        let d = n as i64 - o as i64;
        if let Some(par) = parent_of(p) {
            let e = max_child.entry(par).or_insert((0, 0));
            e.0 = e.0.max(d);
            e.1 = e.1.min(d);
        }
    }
    let by_node = super::attribution::claims_by_node(new);
    let mut hotspots: Vec<PathDelta> = deltas
        .iter()
        .filter_map(|(&p, &(o, n))| {
            let d = n as i64 - o as i64;
            if d.unsigned_abs() < threshold {
                return None;
            }
            let (up, down) = max_child.get(p).copied().unwrap_or((0, 0));
            let explained = if d > 0 { up as f64 >= 0.8 * d as f64 } else { (down as f64) <= 0.8 * d as f64 };
            if explained {
                return None;
            }
            let owner = new.lookup(p).and_then(|node| {
                let owners = super::attribution::owners(new, &by_node, node);
                owners.first().map(|&e| {
                    let e = &new.entities[e as usize];
                    format!("{} · {}", e.group, e.name)
                })
            });
            Some(PathDelta { path: p.to_string(), old: o, new: n, owner })
        })
        .collect();
    // Keep the outermost of nested hotspots moving in the same direction; the
    // frontends can drill into the details.
    let listed: HashMap<String, bool> = hotspots.iter().map(|h| (h.path.clone(), h.delta() > 0)).collect();
    hotspots.retain(|h| {
        let up = h.delta() > 0;
        let mut p = h.path.as_str();
        while let Some(par) = parent_of(p) {
            if listed.get(par) == Some(&up) {
                return false;
            }
            p = par;
        }
        true
    });
    hotspots.sort_by_key(|h| std::cmp::Reverse(h.delta().unsigned_abs()));
    hotspots.truncate(200);

    let key = |e: &super::Entity| (e.group.clone(), e.kind.clone(), e.name.clone());
    let old_e: HashMap<_, u64> = old.entities.iter().map(|e| (key(e), e.total_bytes())).collect();
    let mut seen = std::collections::HashSet::new();
    let mut entities: Vec<EntityDelta> = new
        .entities
        .iter()
        .filter(|e| e.parent.is_some() || new.entities.iter().all(|c| c.parent != Some(e.id)))
        .map(|e| {
            let k = key(e);
            seen.insert(k.clone());
            EntityDelta {
                group: k.0.clone(),
                kind: k.1.clone(),
                name: k.2.clone(),
                old: old_e.get(&k).copied().unwrap_or(0),
                new: e.total_bytes(),
            }
        })
        .collect();
    for e in &old.entities {
        let k = key(e);
        if !seen.contains(&k) && e.parent.is_some() {
            entities.push(EntityDelta { group: k.0, kind: k.1, name: k.2, old: e.total_bytes(), new: 0 });
        }
    }
    entities.retain(|d| (d.new as i64 - d.old as i64).unsigned_abs() >= threshold);
    entities.sort_by_key(|d| std::cmp::Reverse((d.new as i64 - d.old as i64).unsigned_abs()));

    DiffReport { old_time: old.meta.started, new_time: new.meta.started, filesystems, hotspots, entities }
}
