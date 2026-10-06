//! Turns entity path claims into measured sizes using the scanned tree as
//! ground truth, splitting bytes shared between entities (e.g. image layers).

use super::{Claim, NodeId, Snapshot};
use std::collections::{HashMap, HashSet};

/// Resolve every entity's `paths` to tree nodes, record claims, and compute
/// measured_alloc / measured_apparent / measured_unique.
pub fn attribute(snap: &mut Snapshot) {
    snap.claims.clear();
    let mut per_entity: Vec<Vec<NodeId>> = Vec::with_capacity(snap.entities.len());
    let mut unresolved: Vec<Vec<String>> = Vec::with_capacity(snap.entities.len());
    for e in &snap.entities {
        let mut nodes = Vec::new();
        let mut missing = Vec::new();
        for p in &e.paths {
            match snap.lookup_static(p) {
                Some(n) => nodes.push(n),
                None => missing.push(p.clone()),
            }
        }
        per_entity.push(nodes);
        unresolved.push(missing);
    }
    for (e, u) in snap.entities.iter_mut().zip(unresolved) {
        e.unresolved_paths = u;
    }

    // Drop claims nested inside another claim of the same entity.
    let tree = &snap.tree;
    for nodes in per_entity.iter_mut() {
        nodes.sort_unstable();
        nodes.dedup();
        let set: HashSet<NodeId> = nodes.iter().copied().collect();
        nodes.retain(|&n| !tree.ancestors(n).skip(1).any(|a| set.contains(&a)));
    }

    // How many entities within one share domain claim each node.
    let mut share_count: HashMap<(&str, NodeId), u32> = HashMap::new();
    for (e, nodes) in snap.entities.iter().zip(&per_entity) {
        if let Some(k) = e.share_key.as_deref() {
            for &n in nodes {
                *share_count.entry((k, n)).or_default() += 1;
            }
        }
    }

    let mut measured = Vec::with_capacity(per_entity.len());
    for (e, nodes) in snap.entities.iter().zip(&per_entity) {
        let (mut alloc, mut apparent, mut unique) = (0u64, 0u64, 0u64);
        for &n in nodes {
            let f = tree.node(n);
            alloc += f.alloc;
            apparent += f.apparent;
            let shared = e.share_key.as_deref().and_then(|k| share_count.get(&(k, n))).is_some_and(|&c| c > 1);
            if !shared {
                unique += f.alloc;
            }
        }
        measured.push((alloc, apparent, unique));
    }

    for (i, (e, (alloc, apparent, unique))) in snap.entities.iter_mut().zip(measured).enumerate() {
        e.id = i as u32;
        e.measured_alloc = alloc;
        e.measured_apparent = apparent;
        e.measured_unique = unique;
    }
    // Entities that own no paths themselves (groups such as "Docker images")
    // get the de-duplicated union of their descendants' claims.
    let n = snap.entities.len();
    let parents: Vec<Option<usize>> =
        snap.entities.iter().map(|e| e.parent.map(|p| p as usize).filter(|&p| p < n)).collect();
    let mut union: Vec<Vec<NodeId>> = vec![Vec::new(); n];
    let mut ext = vec![0u64; n];
    for i in 0..n {
        let mut cur = parents[i];
        let mut guard = 0;
        while let Some(p) = cur {
            union[p].extend(&per_entity[i]);
            ext[p] += snap.entities[i].external_bytes;
            cur = parents[p];
            guard += 1;
            if guard > 64 {
                break;
            }
        }
    }
    for (i, e) in snap.entities.iter_mut().enumerate() {
        let own = !e.paths.is_empty() || !e.block_devs.is_empty() || e.external_bytes != 0;
        if own || union[i].is_empty() && ext[i] == 0 {
            continue;
        }
        let nodes = &mut union[i];
        nodes.sort_unstable();
        nodes.dedup();
        let set: HashSet<NodeId> = nodes.iter().copied().collect();
        nodes.retain(|&n| !tree.ancestors(n).skip(1).any(|a| set.contains(&a)));
        let alloc: u64 = nodes.iter().map(|&n| tree.node(n).alloc).sum();
        e.measured_alloc = alloc;
        e.measured_apparent = nodes.iter().map(|&n| tree.node(n).apparent).sum();
        e.measured_unique = alloc;
        e.external_bytes = ext[i];
    }

    for (i, nodes) in per_entity.into_iter().enumerate() {
        snap.claims.extend(nodes.into_iter().map(|node| Claim { node, entity: i as u32 }));
    }
    snap.claims.sort_unstable();
}

/// Index from node to the entities that claim it (directly).
pub fn claims_by_node(snap: &Snapshot) -> HashMap<NodeId, Vec<u32>> {
    let mut m: HashMap<NodeId, Vec<u32>> = HashMap::new();
    for c in &snap.claims {
        m.entry(c.node).or_default().push(c.entity);
    }
    m
}

/// Entities owning `node` or its nearest claimed ancestor (most specific first).
pub fn owners(snap: &Snapshot, by_node: &HashMap<NodeId, Vec<u32>>, node: NodeId) -> Vec<u32> {
    for a in snap.tree.ancestors(node) {
        if let Some(v) = by_node.get(&a) {
            return v.clone();
        }
    }
    Vec::new()
}

impl Snapshot {
    /// `lookup` that also follows filesystem aliases (bind mounts) to the
    /// scanned copy of the same directory.
    pub fn lookup_static(&self, path: &str) -> Option<NodeId> {
        if let Some(n) = self.lookup(path) {
            return Some(n);
        }
        for fs in &self.filesystems {
            if fs.root_node.is_none() {
                continue;
            }
            for (alias, target) in &fs.aliases {
                if let Some(rest) = path.strip_prefix(alias.as_str())
                    && (rest.is_empty() || rest.starts_with('/'))
                {
                    let mapped = format!("{}{}", target.trim_end_matches('/'), rest);
                    if let Some(n) = self.lookup(&mapped) {
                        return Some(n);
                    }
                }
            }
        }
        None
    }
}
