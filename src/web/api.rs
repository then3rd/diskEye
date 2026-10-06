//! JSON API over a read-only snapshot. Every handler is O(children) of what it
//! shows (plus cached whole-snapshot views computed once), never O(tree), except
//! `/api/diff`, which has to compare two whole trees.

use crate::model::tree::{Kind, flags};
use crate::model::{ActionSpec, Coverage, NodeId, Risk, Snapshot, attribution};
use crate::views::{self, Metric};
use axum::Json;
use axum::extract::{FromRef, Path as AxPath, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

/// Runs a cleanup action. Injectable so tests don't append to the user's audit log.
pub type Executor = fn(&ActionSpec, Risk, u64) -> anyhow::Result<()>;

pub struct AppState {
    pub snap: Snapshot,
    pub snap_path: Option<PathBuf>,
    pub token: String,
    /// Whether the *server* runs as root (actions then need a typed name).
    pub is_root: bool,
    /// Accepted `Host` headers (DNS-rebinding guard); empty disables the check.
    pub allowed_hosts: Vec<String>,
    pub executor: Executor,
    by_node: HashMap<NodeId, Vec<u32>>,
    targets: HashMap<String, NodeId>,
    /// Filesystem root node -> the mountpoint node it is mounted on in the enclosing filesystem.
    mount_parent: HashMap<NodeId, NodeId>,
    /// (alloc, apparent, items) of filesystems mounted somewhere below a node, so a
    /// directory's size in the unified view includes the filesystems mounted inside it.
    mounted_below: HashMap<NodeId, [u64; 3]>,
    /// Bytes claimed by each owner group somewhere below a node (for coloring unowned dirs).
    claimed_below: HashMap<NodeId, Vec<(u16, u64)>>,
    /// Group names in workload order; a group's index is its stable color slot.
    groups: Vec<String>,
    workload_groups: Vec<views::WorkloadGroup>,
    group_idx: HashMap<String, u16>,
    children_of: HashMap<u32, Vec<u32>>,
    workloads: OnceLock<Value>,
    physical: OnceLock<Value>,
    filesystems: OnceLock<Value>,
    hotspots: OnceLock<Value>,
    summary: OnceLock<Value>,
    diffs: Mutex<HashMap<(String, u64), Arc<Value>>>,
    /// Serializes diff computations (each loads a whole baseline snapshot).
    diff_lock: Mutex<()>,
    /// Entity -> outcome of actions executed during this session.
    executed: Mutex<HashMap<u32, (bool, String)>>,
    exec_lock: Mutex<()>,
}

impl AppState {
    pub fn new(snap: Snapshot, snap_path: Option<PathBuf>, token: String, is_root: bool) -> Self {
        let by_node = attribution::claims_by_node(&snap);
        let targets = views::mount_targets(&snap);
        let mount_parent = mount_parents(&snap);
        let mounted_below = mounted_below(&snap, &targets, &mount_parent);
        let workload_groups = views::workloads(&snap);
        let groups: Vec<String> = workload_groups.iter().map(|g| g.name.clone()).collect();
        let group_idx: HashMap<String, u16> = groups.iter().enumerate().map(|(i, g)| (g.clone(), i as u16)).collect();
        // Claimed nodes per owner group (children inherit their top-level ancestor's
        // group), without nodes nested in another claim of the same group.
        let mut per_group: HashMap<u16, std::collections::HashSet<NodeId>> = HashMap::new();
        for c in &snap.claims {
            if c.entity as usize >= snap.entities.len() || c.node as usize >= snap.tree.len() {
                continue;
            }
            if let Some(&g) = group_idx.get(&top_entity(&snap, c.entity).group) {
                per_group.entry(g).or_default().insert(c.node);
            }
        }
        let mut claimed_below: HashMap<NodeId, Vec<(u16, u64)>> = HashMap::new();
        for (&g, set) in &per_group {
            for &node in set {
                if snap.tree.ancestors(node).skip(1).any(|a| set.contains(&a)) {
                    continue;
                }
                let bytes = snap.tree.node(node).alloc;
                for a in snap.tree.ancestors(node).skip(1) {
                    let v = claimed_below.entry(a).or_default();
                    match v.iter_mut().find(|(k, _)| *k == g) {
                        Some(x) => x.1 += bytes,
                        None => v.push((g, bytes)),
                    }
                }
            }
        }
        let mut children_of: HashMap<u32, Vec<u32>> = HashMap::new();
        for e in &snap.entities {
            if let Some(p) = e.parent {
                children_of.entry(p).or_default().push(e.id);
            }
        }
        for v in children_of.values_mut() {
            v.sort_by_key(|&i| std::cmp::Reverse(snap.entities[i as usize].total_bytes()));
        }
        AppState {
            snap,
            snap_path,
            token,
            is_root,
            allowed_hosts: Vec::new(),
            executor: crate::actions::execute,
            by_node,
            targets,
            mount_parent,
            mounted_below,
            claimed_below,
            groups,
            workload_groups,
            group_idx,
            children_of,
            workloads: OnceLock::new(),
            physical: OnceLock::new(),
            filesystems: OnceLock::new(),
            hotspots: OnceLock::new(),
            summary: OnceLock::new(),
            diffs: Mutex::new(HashMap::new()),
            diff_lock: Mutex::new(()),
            executed: Mutex::new(HashMap::new()),
            exec_lock: Mutex::new(()),
        }
    }

    /// Sizes of a node as listed under its parent, in the unified view: follows a
    /// mountpoint to the mounted root, adds filesystems mounted below, and counts a
    /// duplicate hardlink as zero bytes. Returns (resolved id, [alloc, apparent, items]).
    fn sizes(&self, listed: NodeId) -> (NodeId, [u64; 3]) {
        let id = self.resolve(listed);
        let n = self.snap.tree.node(id);
        let extra = self.mounted_below.get(&id).copied().unwrap_or_default();
        let dup = id == listed && n.has(flags::HARDLINK_DUP);
        let (a, p) = if dup { (0, 0) } else { (n.alloc + extra[0], n.apparent + extra[1]) };
        (id, [a, p, n.items as u64 + extra[2]])
    }

    fn value(&self, listed: NodeId, m: Metric) -> u64 {
        let (_, s) = self.sizes(listed);
        s[match m {
            Metric::Alloc => 0,
            Metric::Apparent => 1,
            Metric::Items => 2,
        }]
    }

    fn valid_node(&self, id: NodeId) -> bool {
        (id as usize) < self.snap.tree.len()
    }

    /// Follow a mountpoint to the root of the filesystem mounted there.
    fn resolve(&self, id: NodeId) -> NodeId {
        views::mount_target(&self.snap, &self.targets, id).unwrap_or(id)
    }

    /// Path from the top of the unified view down to `id`, stitching across mountpoints.
    fn crumbs(&self, id: NodeId) -> Vec<Value> {
        let tree = &self.snap.tree;
        let mut out = Vec::new();
        let mut cur = Some(id);
        let mut guard = 0;
        while let Some(c) = cur {
            guard += 1;
            if guard > 4096 {
                break;
            }
            match tree.parent(c) {
                Some(p) => {
                    out.push(json!({"id": c, "name": tree.name(c)}));
                    cur = Some(p);
                }
                None => match self.mount_parent.get(&c) {
                    Some(&mp) if tree.parent(mp).is_some() => {
                        out.push(json!({"id": c, "name": tree.name(mp)}));
                        cur = tree.parent(mp);
                    }
                    _ => {
                        out.push(json!({"id": c, "name": tree.name(c)}));
                        cur = None;
                    }
                },
            }
        }
        out.reverse();
        out
    }

    fn entity_ref(&self, id: u32) -> Value {
        let e = &self.snap.entities[id as usize];
        json!({"id": e.id, "name": e.name, "kind": e.kind, "kind_label": views::kind_label(&e.kind),
            "group": e.group, "group_idx": self.group_of(id)})
    }

    fn group_of(&self, id: u32) -> Option<u16> {
        self.group_idx.get(&top_entity(&self.snap, id).group).copied()
    }

    pub fn entity_summary(&self, id: u32) -> Value {
        let e = &self.snap.entities[id as usize];
        let reclaim = e.reclaim.as_ref().map(|r| {
            json!({"risk": r.risk.label(), "reason": r.reason, "bytes": r.estimate.unwrap_or_else(|| e.unique_bytes()),
                "has_action": r.action.is_some()})
        });
        json!({
            "id": e.id, "kind": e.kind, "kind_label": views::kind_label(&e.kind), "name": e.name,
            "group": e.group, "group_idx": self.group_of(id), "provider": e.provider, "parent": e.parent,
            "total": e.total_bytes(), "unique": e.unique_bytes(), "alloc": e.measured_alloc,
            "apparent": e.measured_apparent, "external": e.external_bytes, "reported": e.reported,
            "virtual_size": e.virtual_size, "children": self.children_of.get(&id).map_or(0, |v| v.len()),
            "paths": e.paths.len(), "unresolved": e.unresolved_paths.len(), "block_devs": e.block_devs,
            "attrs": e.attrs, "reclaim": reclaim,
        })
    }

    /// Owner info for a child: direct/inherited claim, else the dominant group claimed below it.
    fn owner_of_child(&self, child: NodeId, inherited: Option<u32>, alloc: u64) -> Value {
        if let Some(es) = self.by_node.get(&child) {
            return json!({"entity": self.entity_ref(es[0]), "direct": true, "count": es.len()});
        }
        if let Some(e) = inherited {
            return json!({"entity": self.entity_ref(e), "direct": false, "count": 1});
        }
        if let Some(&(g, b)) = self.claimed_below.get(&child).and_then(|v| v.iter().max_by_key(|(_, b)| *b)) {
            let share = if alloc == 0 { 0.0 } else { (b as f64 / alloc as f64).min(1.0) };
            return json!({"mixed": {"group": self.groups[g as usize], "group_idx": g, "share": share}});
        }
        Value::Null
    }

    fn node_flags(&self, id: NodeId) -> Vec<&'static str> {
        let n = self.snap.tree.node(id);
        let all = [
            (flags::DENIED, "denied"),
            (flags::HARDLINK_DUP, "hardlink_dup"),
            (flags::MOUNTPOINT, "mountpoint"),
            (flags::SPARSE, "sparse"),
            (flags::ERROR, "error"),
            (flags::INCOMPLETE, "incomplete"),
            (flags::HARDLINK, "hardlink"),
        ];
        all.iter().filter(|(f, _)| n.has(*f)).map(|(_, s)| *s).collect()
    }

    /// One child row. `id` is the node as listed under its parent; a stitched
    /// mountpoint reports the mounted filesystem's root as its id and sizes.
    fn child_json(&self, listed: NodeId, m: Metric, inherited: Option<u32>, depth: u32, sub_limit: usize) -> Value {
        let tree = &self.snap.tree;
        let (id, [alloc, apparent, items]) = self.sizes(listed);
        let n = tree.node(id);
        let value = self.value(listed, m);
        let own = self.by_node.get(&id).or_else(|| self.by_node.get(&listed)).map(|v| v[0]).or(inherited);
        let mut v = json!({
            "id": id, "name": tree.name(listed), "kind": kind_str(n.kind), "value": value,
            "alloc": alloc, "apparent": apparent, "items": items,
            "mtime": n.mtime, "flags": self.node_flags(listed), "badges": views::badges(&self.snap, &self.by_node, listed),
            "has_children": n.child_count > 0, "count": n.child_count,
            "owner": self.owner_of_child(listed, inherited, n.alloc),
        });
        if alloc > n.alloc {
            v["mounted_alloc"] = json!(alloc - n.alloc);
        }
        if id != listed {
            v["mount_of"] = json!(listed);
            v["fs"] = json!(tree.name(id));
        }
        if depth > 1 && n.child_count > 0 {
            v["children"] = self.children_json(id, m, own, depth - 1, sub_limit, sub_limit);
        }
        v
    }

    /// Children of `id` sorted by metric; the tail beyond `limit` is folded into one aggregate row.
    fn children_json(
        &self,
        id: NodeId,
        m: Metric,
        inherited: Option<u32>,
        depth: u32,
        limit: usize,
        sub_limit: usize,
    ) -> Value {
        let tree = &self.snap.tree;
        let mut kids: Vec<(u64, NodeId)> = tree.children(id).map(|c| (self.value(c, m), c)).collect();
        let cmp = |a: &(u64, NodeId), b: &(u64, NodeId)| b.0.cmp(&a.0).then(a.1.cmp(&b.1));
        let rest = if kids.len() > limit {
            if limit > 0 {
                kids.select_nth_unstable_by(limit - 1, cmp);
            }
            kids.split_off(limit)
        } else {
            Vec::new()
        };
        kids.sort_unstable_by(cmp);
        let mut out: Vec<Value> =
            kids.iter().map(|&(_, c)| self.child_json(c, m, inherited, depth, sub_limit)).collect();
        if !rest.is_empty() {
            let (mut alloc, mut apparent, mut items, mut value) = (0u64, 0u64, 0u64, 0u64);
            for &(v, c) in &rest {
                let (_, s) = self.sizes(c);
                value += v;
                alloc += s[0];
                apparent += s[1];
                items += s[2];
            }
            out.push(json!({"other": true, "id": Value::Null, "name": format!("{} smaller items", rest.len()),
                "count": rest.len(), "value": value, "alloc": alloc, "apparent": apparent, "items": items}));
        }
        Value::Array(out)
    }

    fn node_json(&self, id: NodeId, m: Metric, depth: u32, limit: usize, sub_limit: usize) -> Value {
        let tree = &self.snap.tree;
        let n = tree.node(id);
        let (_, [alloc, apparent, items]) = self.sizes(id);
        let owners = attribution::owners(&self.snap, &self.by_node, id);
        let inherited = owners.first().copied();
        let fs = self.snap.fs_of_node(id).map(|i| &self.snap.filesystems[i]);
        json!({
            "id": id, "name": tree.name(id), "path": tree.path(id), "kind": kind_str(n.kind),
            "metric": metric_str(m), "value": self.value(id, m), "alloc": alloc, "apparent": apparent,
            "items": items, "mounted_alloc": alloc - n.alloc, "own_alloc": n.alloc, "mtime": n.mtime, "count": n.child_count, "flags": self.node_flags(id),
            "badges": views::badges(&self.snap, &self.by_node, id),
            "owners": owners.iter().map(|&e| self.entity_ref(e)).collect::<Vec<_>>(),
            "owner": self.owner_of_child(id, None, n.alloc),
            "fs": fs.map(|f| json!({"mount_point": f.mount_point, "fstype": f.fstype, "source": f.source})),
            "crumbs": self.crumbs(id),
            "children": self.children_json(id, m, inherited, depth, limit, sub_limit),
        })
    }
}

/// For each scanned filesystem root, the node it is mounted on inside the
/// closest enclosing scanned filesystem.
fn mount_parents(snap: &Snapshot) -> HashMap<NodeId, NodeId> {
    let mut out = HashMap::new();
    let roots: Vec<(&str, NodeId)> =
        snap.filesystems.iter().filter_map(|f| Some((f.scan_root.as_str(), f.root_node?))).collect();
    for &(path, root) in &roots {
        let enclosing = roots
            .iter()
            .filter(|(p, r)| *r != root && *p != path && crate::scan::mounts::is_path_under(path, p))
            .max_by_key(|(p, _)| p.len());
        let Some(&(ppath, proot)) = enclosing else { continue };
        let rest = path[ppath.len()..].trim_start_matches('/');
        let mut node = Some(proot);
        for comp in rest.split('/').filter(|c| !c.is_empty()) {
            node = node.and_then(|n| snap.tree.child_by_name(n, comp.as_bytes()));
        }
        if let Some(mp) = node.filter(|&n| n != proot) {
            out.insert(root, mp);
        }
    }
    out
}

/// For every scanned filesystem mounted on a mountpoint of another scanned filesystem,
/// add its size to each directory above that mountpoint, across nested mounts.
fn mounted_below(
    snap: &Snapshot,
    targets: &HashMap<String, NodeId>,
    mount_parent: &HashMap<NodeId, NodeId>,
) -> HashMap<NodeId, [u64; 3]> {
    let tree = &snap.tree;
    let mut out: HashMap<NodeId, [u64; 3]> = HashMap::new();
    for (&root, &mp) in mount_parent {
        // Only filesystems the parent walk stopped at; otherwise the bytes are already counted.
        if views::mount_target(snap, targets, mp) != Some(root) {
            continue;
        }
        let n = tree.node(root);
        let add = [n.alloc, n.apparent, n.items as u64];
        let mut cur = tree.parent(mp);
        let mut guard = 0;
        while let Some(a) = cur {
            guard += 1;
            if guard > 4096 {
                break;
            }
            let e = out.entry(a).or_default();
            for i in 0..3 {
                e[i] += add[i];
            }
            cur = match tree.parent(a) {
                Some(p) => Some(p),
                // `a` is a filesystem root: continue above the place it is mounted on.
                None => mount_parent.get(&a).and_then(|&m| tree.parent(m)),
            };
        }
    }
    out
}

fn top_entity(snap: &Snapshot, mut id: u32) -> &crate::model::Entity {
    for _ in 0..64 {
        match snap.entities[id as usize].parent {
            Some(p) if (p as usize) < snap.entities.len() => id = p,
            _ => break,
        }
    }
    &snap.entities[id as usize]
}

fn kind_str(k: Kind) -> &'static str {
    match k {
        Kind::Dir => "dir",
        Kind::File => "file",
        Kind::Symlink => "symlink",
        Kind::Special => "special",
    }
}

fn metric_str(m: Metric) -> &'static str {
    match m {
        Metric::Alloc => "alloc",
        Metric::Apparent => "apparent",
        Metric::Items => "items",
    }
}

fn parse_metric(s: Option<&str>) -> Metric {
    match s {
        Some("apparent") => Metric::Apparent,
        Some("items") => Metric::Items,
        _ => Metric::Alloc,
    }
}

fn coverage_str(c: Coverage) -> &'static str {
    match c {
        Coverage::Complete => "complete",
        Coverage::Partial => "partial",
        Coverage::Denied => "denied",
        Coverage::Absent => "absent",
    }
}

pub fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({"ok": false, "error": msg.into()}))).into_response()
}

fn ok(v: &Value) -> Response {
    Json(v).into_response()
}

type St = State<Arc<AppState>>;

// ---------------------------------------------------------------- rescan

/// Produces a fresh snapshot (and where it was saved) for `/api/rescan`.
pub type Rescanner = Arc<dyn Fn() -> anyhow::Result<(Snapshot, Option<PathBuf>)> + Send + Sync>;

/// Router state: the current snapshot's `AppState`, replaced wholesale after a
/// rescan. Handlers extract `Arc<AppState>` (see `FromRef`), so a request keeps
/// the snapshot it started with even if a rescan lands meanwhile.
#[derive(Clone)]
pub struct Server(Arc<ServerInner>);

struct ServerInner {
    current: RwLock<Arc<AppState>>,
    rescanner: Option<Rescanner>,
    rescan: Mutex<RescanStatus>,
}

#[derive(Default, Clone, serde::Serialize)]
struct RescanStatus {
    available: bool,
    running: bool,
    started: Option<i64>,
    finished: Option<i64>,
    error: Option<String>,
    /// Bumped every time a new snapshot is swapped in.
    generation: u64,
}

impl Server {
    pub fn new(state: Arc<AppState>, rescanner: Option<Rescanner>) -> Self {
        let rescan = RescanStatus { available: rescanner.is_some(), ..Default::default() };
        Server(Arc::new(ServerInner { current: RwLock::new(state), rescanner, rescan: Mutex::new(rescan) }))
    }

    pub fn current(&self) -> Arc<AppState> {
        self.0.current.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn status(&self) -> std::sync::MutexGuard<'_, RescanStatus> {
        self.0.rescan.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Scan on a background thread and swap the result in; false if one is already running.
    fn start_rescan(&self, rescanner: Rescanner) -> bool {
        {
            let mut s = self.status();
            if s.running {
                return false;
            }
            s.running = true;
            s.started = Some(crate::util::now_secs());
            s.error = None;
        }
        let sv = self.clone();
        std::thread::spawn(move || {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let (snap, path) = rescanner()?;
                let old = sv.current();
                let mut st = AppState::new(snap, path, old.token.clone(), old.is_root);
                st.allowed_hosts = old.allowed_hosts.clone();
                st.executor = old.executor;
                Ok::<_, anyhow::Error>(st)
            }));
            let mut s = sv.status();
            match res {
                Ok(Ok(st)) => {
                    *sv.0.current.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(st);
                    s.generation += 1;
                }
                Ok(Err(e)) => s.error = Some(format!("{e:#}")),
                Err(_) => s.error = Some("the scan crashed (see the server's terminal)".into()),
            }
            s.running = false;
            s.finished = Some(crate::util::now_secs());
        });
        true
    }
}

impl FromRef<Server> for Arc<AppState> {
    fn from_ref(sv: &Server) -> Self {
        sv.current()
    }
}

pub async fn rescan_status(State(sv): State<Server>) -> Response {
    Json(sv.status().clone()).into_response()
}

pub async fn rescan_start(State(sv): State<Server>) -> Response {
    let Some(rescanner) = sv.0.rescanner.clone() else {
        return err(StatusCode::NOT_IMPLEMENTED, "re-scanning is not available on this server");
    };
    if !sv.start_rescan(rescanner) {
        return err(StatusCode::CONFLICT, "a scan is already running");
    }
    (StatusCode::ACCEPTED, Json(sv.status().clone())).into_response()
}

/// Compute a whole-snapshot view once on a blocking thread, then serve it from memory.
async fn cached(st: Arc<AppState>, slot: fn(&AppState) -> &OnceLock<Value>, f: fn(&AppState) -> Value) -> Response {
    if let Some(v) = slot(&st).get() {
        return ok(v);
    }
    let st2 = st.clone();
    match tokio::task::spawn_blocking(move || {
        let v = f(&st2);
        let _ = slot(&st2).set(v);
    })
    .await
    {
        Ok(()) => ok(slot(&st).get().unwrap_or(&Value::Null)),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---------------------------------------------------------------- auth

fn token_from_query(q: Option<&str>) -> Option<String> {
    q?.split('&').find_map(|kv| kv.strip_prefix("token=").map(str::to_string))
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Host check for every request; token check for `/api/*`.
pub async fn guard(State(st): St, req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok());
    if let Some(h) = host.filter(|_| !st.allowed_hosts.is_empty())
        && !st.allowed_hosts.iter().any(|a| a == h)
    {
        return err(StatusCode::FORBIDDEN, "unexpected Host header");
    }
    if req.uri().path().starts_with("/api/") {
        let given = req
            .headers()
            .get("x-diskeye-token")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .or_else(|| token_from_query(req.uri().query()));
        if !given.is_some_and(|t| ct_eq(t.as_bytes(), st.token.as_bytes())) {
            return err(StatusCode::UNAUTHORIZED, "missing or wrong token (open the URL printed by `diskeye serve`)");
        }
    }
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    resp
}

// ---------------------------------------------------------------- handlers

pub async fn summary(State(st): St) -> Response {
    cached(st, |s| &s.summary, build_summary).await
}

fn build_summary(st: &AppState) -> Value {
    let snap = &st.snap;
    let rec = snap.reconcile();
    let sum = |f: fn(&crate::model::Reconcile) -> u64| rec.iter().map(f).sum::<u64>();
    let items = views::reclaim(snap);
    let by_risk = |r: Risk| items.iter().filter(|i| i.risk == r).map(|i| i.bytes).sum::<u64>();
    let gaps: Vec<Value> = snap
        .providers
        .iter()
        .filter(|p| matches!(p.coverage, Coverage::Partial | Coverage::Denied))
        .map(|p| json!({"name": p.name, "coverage": coverage_str(p.coverage), "notes": p.notes}))
        .collect();
    json!({
        "meta": snap.meta,
        "started_human": crate::util::timestamp_human(snap.meta.started),
        "scan_as_root": snap.meta.is_root(),
        "server_root": st.is_root,
        "snapshot_path": st.snap_path.as_ref().map(|p| p.display().to_string()),
        "totals": {
            "fs_total": sum(|r| r.total), "fs_used": sum(|r| r.used), "fs_avail": sum(|r| r.avail),
            "scanned": sum(|r| r.scanned), "deleted_open": snap.deleted_open.iter().map(|d| d.alloc).sum::<u64>(),
            "hidden": snap.hidden.iter().map(|h| h.alloc).sum::<u64>(),
            "unaccounted": rec.iter().map(|r| r.unaccounted).sum::<i64>(),
            "reclaim_safe": by_risk(Risk::Safe), "reclaim_review": by_risk(Risk::Review),
            "reclaim_danger": by_risk(Risk::Danger),
            "workloads": st.workload_groups.iter().map(|g| g.total).sum::<u64>(),
        },
        "counts": {
            "nodes": snap.tree.len(), "filesystems": snap.filesystems.len(),
            "scanned_filesystems": snap.filesystems.iter().filter(|f| f.root_node.is_some()).count(),
            "entities": snap.entities.len(), "claims": snap.claims.len(), "reclaim_items": items.len(),
            "deleted_open": snap.deleted_open.len(), "hidden": snap.hidden.len(),
            "denied_dirs": snap.filesystems.iter().map(|f| f.denied_dirs).sum::<u64>(),
        },
        "groups": st.groups,
        "providers": snap.providers.iter().map(|p| json!({"name": p.name, "coverage": coverage_str(p.coverage),
            "notes": p.notes, "duration_ms": p.duration_ms})).collect::<Vec<_>>(),
        "coverage_gaps": gaps,
    })
}

pub async fn physical(State(st): St) -> Response {
    cached(st, |s| &s.physical, build_physical).await
}

fn build_physical(st: &AppState) -> Value {
    let snap = &st.snap;
    let unused_lvs: Vec<Value> =
        views::unused_lvs(snap).into_iter().map(|l| json!({"vg": l.vg, "name": l.name, "size": l.size})).collect();
    json!({"rows": views::physical(snap), "lvm": snap.lvm, "swaps": snap.swaps, "unused_lvs": unused_lvs})
}

pub async fn filesystems(State(st): St) -> Response {
    cached(st, |s| &s.filesystems, build_filesystems).await
}

fn build_filesystems(st: &AppState) -> Value {
    let snap = &st.snap;
    let rec = snap.reconcile();
    let list: Vec<Value> = snap
        .filesystems
        .iter()
        .enumerate()
        .map(|(i, f)| {
            json!({
                "index": i, "mount_point": f.mount_point, "scan_root": f.scan_root, "fstype": f.fstype,
                "source": f.source, "options": f.options, "statvfs": f.statvfs,
                "used": f.statvfs.map(|s| s.used()), "root_node": f.root_node,
                "scanned_alloc": f.scanned_alloc, "scanned_apparent": f.scanned_apparent,
                "scanned_items": f.scanned_items, "denied_dirs": f.denied_dirs, "errors": f.errors,
                "skipped_reason": f.skipped_reason, "aliases": f.aliases,
                "reconcile": rec.iter().find(|r| r.fs == i),
                "hidden": snap.hidden.iter().filter(|h| h.fs == i).collect::<Vec<_>>(),
            })
        })
        .collect();
    let skipped: Vec<Value> = snap
        .filesystems
        .iter()
        .filter(|f| f.root_node.is_none())
        .map(|f| {
            json!({"mount_point": f.mount_point, "fstype": f.fstype, "source": f.source,
                "used": f.statvfs.map(|s| s.used()), "total": f.statvfs.map(|s| s.total), "reason": f.skipped_reason})
        })
        .collect();
    json!({"filesystems": list, "reconcile": rec, "skipped": skipped})
}

pub async fn roots(State(st): St) -> Response {
    let snap = &st.snap;
    let row = |id: NodeId| {
        let (_, [alloc, apparent, items]) = st.sizes(id);
        let fs = snap.fs_of_node(id).map(|i| &snap.filesystems[i]);
        json!({"id": id, "path": snap.tree.path(id), "alloc": alloc, "apparent": apparent, "items": items,
            "own_alloc": snap.tree.node(id).alloc,
            "fstype": fs.map(|f| f.fstype.clone()), "mount_point": fs.map(|f| f.mount_point.clone())})
    };
    let top: Vec<Value> = views::top_roots(snap).into_iter().map(row).collect();
    let all: Vec<Value> = snap.filesystems.iter().filter_map(|f| f.root_node).map(row).collect();
    ok(&json!({"top": top, "all": all}))
}

#[derive(Deserialize)]
pub struct NodeQuery {
    metric: Option<String>,
    depth: Option<u32>,
    limit: Option<usize>,
    sublimit: Option<usize>,
}

pub async fn node(State(st): St, AxPath(id): AxPath<u32>, Query(q): Query<NodeQuery>) -> Response {
    if !st.valid_node(id) {
        return err(StatusCode::NOT_FOUND, format!("no node {id}"));
    }
    let m = parse_metric(q.metric.as_deref());
    let depth = q.depth.unwrap_or(1).clamp(1, 3);
    let limit = q.limit.unwrap_or(200).clamp(1, 5000);
    let sub = q.sublimit.unwrap_or(if depth > 2 { 12 } else { 40 }).clamp(1, 500);
    let st2 = st.clone();
    match tokio::task::spawn_blocking(move || st2.node_json(st2.resolve(id), m, depth, limit, sub)).await {
        Ok(v) => ok(&v),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct PathQuery {
    p: String,
}

pub async fn path(State(st): St, Query(q): Query<PathQuery>) -> Response {
    if !q.p.starts_with('/') {
        return err(StatusCode::BAD_REQUEST, "path must be absolute");
    }
    match st.snap.lookup_static(&q.p) {
        Some(n) => {
            let id = st.resolve(n);
            ok(&json!({"id": id, "path": st.snap.tree.path(id), "crumbs": st.crumbs(id)}))
        }
        None => err(StatusCode::NOT_FOUND, format!("{} is not in the snapshot", q.p)),
    }
}

pub async fn workloads(State(st): St) -> Response {
    cached(st, |s| &s.workloads, build_workloads).await
}

fn build_workloads(st: &AppState) -> Value {
    let groups: Vec<Value> = st
        .workload_groups
        .iter()
        .map(|g| {
            json!({"name": g.name, "idx": st.group_idx.get(&g.name), "total": g.total, "unique": g.unique,
                "reclaimable": g.reclaimable,
                "entities": g.entities.iter().map(|&i| st.entity_summary(i)).collect::<Vec<_>>()})
        })
        .collect();
    json!({"groups": groups})
}

pub async fn entity(State(st): St, AxPath(id): AxPath<u32>) -> Response {
    let snap = &st.snap;
    let Some(e) = snap.entities.get(id as usize) else {
        return err(StatusCode::NOT_FOUND, format!("no entity {id}"));
    };
    let mut v = st.entity_summary(id);
    let paths: Vec<Value> = e
        .paths
        .iter()
        .map(|p| {
            let node = snap.lookup_static(p).map(|n| st.resolve(n));
            let n = node.map(|n| snap.tree.node(n));
            json!({"path": p, "node": node, "alloc": n.map(|n| n.alloc), "apparent": n.map(|n| n.apparent),
                "items": n.map(|n| n.items), "kind": n.map(|n| kind_str(n.kind))})
        })
        .collect();
    let mut chain = Vec::new();
    let mut cur = e.parent;
    while let Some(p) = cur.filter(|&p| (p as usize) < snap.entities.len() && chain.len() < 64) {
        chain.push(st.entity_ref(p));
        cur = snap.entities[p as usize].parent;
    }
    chain.reverse();
    let children: Vec<Value> =
        st.children_of.get(&id).map(|v| v.iter().map(|&c| st.entity_summary(c)).collect()).unwrap_or_default();
    v["path_list"] = json!(paths);
    v["unresolved_paths"] = json!(e.unresolved_paths);
    v["share_key"] = json!(e.share_key);
    v["ancestors"] = json!(chain);
    v["child_list"] = json!(children);
    if let Some(a) = e.reclaim.as_ref().and_then(|r| r.action.as_ref()) {
        v["action"] = json!({"label": a.label, "steps": crate::actions::describe(a)});
    }
    ok(&v)
}

fn confirm_phrase(st: &AppState, id: u32) -> (String, &'static str) {
    let e = &st.snap.entities[id as usize];
    let danger = e.reclaim.as_ref().is_some_and(|r| r.risk == Risk::Danger);
    if danger || st.is_root {
        (e.name.clone(), if danger { "danger" } else { "root" })
    } else {
        ("yes".into(), "normal")
    }
}

pub async fn reclaim(State(st): St) -> Response {
    let executed = st.executed.lock().map(|m| m.clone()).unwrap_or_default();
    let items: Vec<Value> = views::reclaim(&st.snap)
        .into_iter()
        .map(|r| {
            let e = &st.snap.entities[r.entity as usize];
            let paths: Vec<Value> = e
                .paths
                .iter()
                .take(3)
                .map(|p| json!({"path": p, "node": st.snap.lookup_static(p).map(|n| st.resolve(n))}))
                .collect();
            json!({
                "entity": r.entity, "name": e.name, "kind": e.kind, "kind_label": views::kind_label(&e.kind),
                "group": e.group, "group_idx": st.group_of(r.entity), "risk": r.risk.label(), "bytes": r.bytes,
                "reason": r.reason, "has_action": r.action.is_some(),
                "action": r.action.as_ref().map(|a| json!({"label": a.label, "steps": crate::actions::describe(a)})),
                "paths": paths, "confirm": confirm_phrase(&st, r.entity).1,
                "done": executed.get(&r.entity).map(|(ok, msg)| json!({"ok": ok, "message": msg})),
            })
        })
        .collect();
    ok(&json!({"items": items, "server_root": st.is_root}))
}

pub async fn hotspots(State(st): St) -> Response {
    cached(st, |s| &s.hotspots, build_hotspots).await
}

fn build_hotspots(st: &AppState) -> Value {
    let snap = &st.snap;
    let rows: Vec<Value> = views::hotspots(snap, 40)
        .into_iter()
        .map(|id| {
            let n = snap.tree.node(id);
            let owners = attribution::owners(snap, &st.by_node, id);
            let fs = snap.fs_of_node(id).map(|i| &snap.filesystems[i]);
            let fs_alloc = fs.and_then(|f| f.root_node).map(|r| snap.tree.node(r).alloc).unwrap_or(0);
            json!({"id": id, "path": snap.tree.path(id), "alloc": n.alloc, "apparent": n.apparent, "items": n.items,
                "mtime": n.mtime, "fs": fs.map(|f| f.mount_point.clone()),
                "fs_share": if fs_alloc > 0 { n.alloc as f64 / fs_alloc as f64 } else { 0.0 },
                "owner": owners.first().map(|&e| st.entity_ref(e)),
                "owner_hint": st.owner_of_child(id, None, n.alloc)})
        })
        .collect();
    json!({"hotspots": rows})
}

pub async fn deleted_open(State(st): St) -> Response {
    let snap = &st.snap;
    let mut rows: Vec<&crate::model::DeletedOpen> = snap.deleted_open.iter().collect();
    rows.sort_by_key(|d| std::cmp::Reverse(d.alloc));
    let rows: Vec<Value> = rows
        .into_iter()
        .map(|d| {
            json!({"pid": d.pid, "comm": d.comm, "fd": d.fd, "path": d.path, "alloc": d.alloc, "apparent": d.apparent,
                "fs": d.fs.and_then(|i| snap.filesystems.get(i)).map(|f| f.mount_point.clone())})
        })
        .collect();
    let hidden: Vec<Value> = snap
        .hidden
        .iter()
        .map(|h| {
            json!({"path": h.path, "alloc": h.alloc, "apparent": h.apparent, "items": h.items,
                "fs": snap.filesystems.get(h.fs).map(|f| f.mount_point.clone())})
        })
        .collect();
    ok(
        &json!({"deleted_open": rows, "total": snap.deleted_open.iter().map(|d| d.alloc).sum::<u64>(), "hidden": hidden}),
    )
}

fn current_name(st: &AppState) -> Option<String> {
    st.snap_path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned())
}

pub async fn snapshots(State(st): St) -> Response {
    let cur_canon = st.snap_path.as_ref().and_then(|p| std::fs::canonicalize(p).ok());
    let list: Vec<Value> = crate::model::snapshot::list()
        .into_iter()
        .rev()
        .map(|p| {
            let md = std::fs::metadata(&p).ok();
            let mtime = md
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as i64);
            let current = cur_canon.is_some() && std::fs::canonicalize(&p).ok() == cur_canon;
            json!({"name": p.file_name().map(|n| n.to_string_lossy().into_owned()), "path": p.display().to_string(),
                "size": md.map(|m| m.len()), "mtime": mtime, "mtime_human": crate::util::timestamp_human(mtime),
                "current": current})
        })
        .collect();
    ok(&json!({"snapshots": list, "dir": crate::model::snapshot::default_dir().display().to_string(),
        "current": current_name(&st), "current_time": st.snap.meta.started}))
}

#[derive(Deserialize)]
pub struct DiffQuery {
    against: String,
    threshold: Option<String>,
}

pub async fn diff(State(st): St, Query(q): Query<DiffQuery>) -> Response {
    let threshold_s = q.threshold.unwrap_or_else(|| "100M".into());
    let Some(threshold) = crate::providers::parse_size(&threshold_s) else {
        return err(StatusCode::BAD_REQUEST, format!("bad threshold {threshold_s}"));
    };
    if q.against.contains('/') || q.against.starts_with('.') {
        return err(StatusCode::BAD_REQUEST, "`against` must be a snapshot file name");
    }
    let Some(file) =
        crate::model::snapshot::list().into_iter().find(|p| p.file_name().is_some_and(|n| *n == *q.against))
    else {
        return err(StatusCode::NOT_FOUND, format!("no snapshot named {}", q.against));
    };
    let key = (q.against.clone(), threshold);
    if let Some(v) = st.diffs.lock().ok().and_then(|m| m.get(&key).cloned()) {
        return ok(&v);
    }
    let st2 = st.clone();
    let key2 = key.clone();
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let _g = st2.diff_lock.lock().unwrap_or_else(|p| p.into_inner());
        // Another request may have computed it while we waited.
        if let Some(v) = st2.diffs.lock().ok().and_then(|m| m.get(&key2).cloned()) {
            return Ok((*v).clone());
        }
        let old = crate::model::snapshot::load(&file)?;
        let d = crate::model::diff::diff(&old, &st2.snap, threshold);
        let mut v = serde_json::to_value(&d)?;
        if let Some(hs) = v["hotspots"].as_array_mut() {
            for h in hs {
                let node = h["path"].as_str().and_then(|p| st2.snap.lookup(p));
                h["node"] = json!(node);
            }
        }
        v["baseline"] = json!(q.against);
        v["baseline_newer"] = json!(old.meta.started > st2.snap.meta.started);
        v["threshold"] = json!(threshold);
        Ok(v)
    })
    .await;
    match res {
        Ok(Ok(v)) => {
            let v = Arc::new(v);
            if let Ok(mut m) = st.diffs.lock() {
                m.insert(key, v.clone());
            }
            ok(&v)
        }
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn action_of(st: &AppState, id: u32) -> Result<(&ActionSpec, Risk, u64), Box<Response>> {
    let Some(e) = st.snap.entities.get(id as usize) else {
        return Err(Box::new(err(StatusCode::NOT_FOUND, format!("no entity {id}"))));
    };
    let Some(r) = e.reclaim.as_ref() else {
        return Err(Box::new(err(StatusCode::BAD_REQUEST, format!("{} has no cleanup suggestion", e.name))));
    };
    let Some(a) = r.action.as_ref() else {
        return Err(Box::new(err(
            StatusCode::BAD_REQUEST,
            format!("{} has no automatic action; clean it up manually", e.name),
        )));
    };
    Ok((a, r.risk, r.estimate.unwrap_or_else(|| e.unique_bytes())))
}

pub async fn action_preview(State(st): St, AxPath(id): AxPath<u32>) -> Response {
    let (spec, risk, bytes) = match action_of(&st, id) {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let spec = spec.clone();
    let pre = tokio::task::spawn_blocking(move || crate::actions::preflight(&spec).map_err(|e| format!("{e:#}")))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    let (phrase, why) = confirm_phrase(&st, id);
    let (spec, ..) = match action_of(&st, id) {
        Ok(x) => x,
        Err(r) => return *r,
    };
    ok(&json!({
        "entity": st.entity_ref(id), "label": spec.label, "risk": risk.label(), "bytes": bytes,
        "steps": crate::actions::describe(spec), "raw_steps": spec.steps,
        "preflight": {"ok": pre.is_ok(), "error": pre.err()},
        "confirm": {"phrase": phrase, "reason": why}, "server_root": st.is_root,
        "done": st.executed.lock().ok().and_then(|m| m.get(&id).cloned()).map(|(ok, msg)| json!({"ok": ok, "message": msg})),
    }))
}

#[derive(Deserialize)]
pub struct ExecBody {
    confirm: String,
}

pub async fn action_execute(State(st): St, AxPath(id): AxPath<u32>, Json(body): Json<ExecBody>) -> Response {
    let (spec, risk, bytes) = match action_of(&st, id) {
        Ok(x) => (x.0.clone(), x.1, x.2),
        Err(r) => return *r,
    };
    let (phrase, _) = confirm_phrase(&st, id);
    if body.confirm.trim() != phrase {
        return err(StatusCode::BAD_REQUEST, format!("confirmation does not match: type `{phrase}` to proceed"));
    }
    let st2 = st.clone();
    let res = tokio::task::spawn_blocking(move || {
        let _g = st2.exec_lock.lock().unwrap_or_else(|p| p.into_inner());
        (st2.executor)(&spec, risk, bytes).map_err(|e| format!("{e:#}"))
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));
    let name = &st.snap.entities[id as usize].name;
    let (okb, msg) = match &res {
        Ok(()) => (
            true,
            format!("done: {name} (about {} freed; rescan to refresh the numbers)", crate::model::fmt_size(bytes)),
        ),
        Err(e) => (false, e.clone()),
    };
    if let Ok(mut m) = st.executed.lock() {
        m.insert(id, (okb, msg.clone()));
    }
    if okb {
        ok(&json!({"ok": true, "message": msg}))
    } else {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "error": msg}))).into_response()
    }
}
