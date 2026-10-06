//! Docker Engine (rootful and rootless) via its unix-socket API.
//!
//! Two storage layouts are mapped to images/containers:
//! - classic graphdrivers (overlay2...): `GraphDriver.Data` Upper/LowerDir
//!   name the `overlay2/<id>` directories of every layer;
//! - the containerd image store (Docker 29 default, `Driver: overlayfs`):
//!   layers live in `containerd/daemon/io.containerd.snapshotter.v1.<driver>/
//!   snapshots/<n>`. The snapshotter's `metadata.db` maps `<n>` to the layer's
//!   chain ID, which we compute from each image's `RootFS.Layers`; compressed
//!   blobs are found by walking the image's index/manifest in the content store.
//!
//! Layers shared between images use a common `share_key`, so an image's unique
//! size is what removing it would free.

use super::containerd::{self, KIND_COMMITTED, SnapIndex};
use super::{Ctx, Outcome, Provider};
use crate::model::tree::flags;
use crate::model::{ActionSpec, ActionStep, Coverage, Entity, Reclaim, Risk, Snapshot, fmt_size};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub struct Docker;

/// Container logs above this are flagged for review.
const BIG_LOG: u64 = 100 << 20;
/// Build cache records above this get their own entity.
const BIG_CACHE_RECORD: u64 = 64 << 20;

/// Minimal HTTP/1.0 client over a unix socket. Returns (status, body).
pub fn api_request(socket: &str, method: &str, path: &str) -> Result<(u16, String)> {
    let mut s = UnixStream::connect(socket).with_context(|| format!("connecting to {socket}"))?;
    s.set_read_timeout(Some(Duration::from_secs(300)))?;
    write!(s, "{method} {path} HTTP/1.0\r\nHost: docker\r\nUser-Agent: diskeye\r\nContent-Length: 0\r\n\r\n")?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").context("malformed HTTP response")?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut body = raw[split + 4..].to_vec();
    let status: u16 = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).context("no HTTP status")?;
    if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        body = dechunk(&body)?;
    }
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

fn dechunk(mut b: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let nl = b.windows(2).position(|w| w == b"\r\n").context("bad chunk")?;
        let size_str = String::from_utf8_lossy(&b[..nl]);
        let size = usize::from_str_radix(size_str.split(';').next().unwrap_or("").trim(), 16)?;
        b = &b[nl + 2..];
        if size == 0 {
            return Ok(out);
        }
        if b.len() < size {
            bail!("truncated chunk");
        }
        out.extend_from_slice(&b[..size]);
        b = &b[(size + 2).min(b.len())..];
    }
}

/// GET returning parsed JSON.
pub fn api_get(socket: &str, path: &str) -> Result<Value> {
    let (status, body) = api_request(socket, "GET", path)?;
    if status != 200 {
        bail!("GET {path}: HTTP {status}: {}", body.trim());
    }
    Ok(serde_json::from_str(&body)?)
}

/// Read-only access to one engine's API (a socket, or recorded JSON in tests).
pub trait Api {
    fn get(&self, path: &str) -> Result<Value>;
}

struct SocketApi<'a>(&'a str);

impl Api for SocketApi<'_> {
    fn get(&self, path: &str) -> Result<Value> {
        api_get(self.0, path)
    }
}

// ---------------------------------------------------------------- JSON helpers

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

fn i(v: &Value, k: &str) -> Option<i64> {
    v.get(k).and_then(|x| x.as_i64())
}

fn arr<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k).and_then(|x| x.as_array()).map(|a| a.as_slice()).unwrap_or(&[])
}

fn strs(v: &Value, k: &str) -> Vec<String> {
    arr(v, k).iter().filter_map(|x| x.as_str().map(String::from)).collect()
}

/// A `/system/df` list, from the classic key or the API 1.52+ `*Usage.Items`.
fn df_list<'a>(df: &'a Value, key: &str, usage: &str) -> &'a [Value] {
    match df.get(key).and_then(|x| x.as_array()) {
        Some(a) => a,
        None => df.get(usage).map(|u| arr(u, "Items")).unwrap_or(&[]),
    }
}

fn short_id(id: &str) -> &str {
    let id = id.strip_prefix("sha256:").unwrap_or(id);
    &id[..id.len().min(12)]
}

fn when(secs: i64) -> String {
    crate::util::timestamp_human(secs)
}

// ---------------------------------------------------------------- fetching

/// Raw API responses for one engine; `build` is a pure function of these.
#[derive(Debug, Default)]
pub struct EngineData {
    pub socket: String,
    pub info: Value,
    pub df: Value,
    /// `/images/{id}/json` by image id.
    pub images: HashMap<String, Value>,
    /// `/containers/{id}/json` by container id.
    pub containers: HashMap<String, Value>,
    /// Things we couldn't fetch.
    pub gaps: Vec<String>,
}

pub fn fetch(api: &dyn Api, socket: &str) -> Result<EngineData> {
    let info = api.get("/info")?;
    let mut d = EngineData { socket: socket.into(), info, ..Default::default() };
    d.df = match api.get("/system/df") {
        Ok(v) => v,
        Err(e) => {
            d.gaps.push(format!("/system/df failed ({e:#}); sizes from list endpoints"));
            let mut df = serde_json::Map::new();
            for (key, path) in
                [("Images", "/images/json"), ("Containers", "/containers/json?all=1&size=1"), ("Volumes", "/volumes")]
            {
                match api.get(path) {
                    Ok(v) => {
                        let list = if key == "Volumes" { v.get("Volumes").cloned().unwrap_or(Value::Null) } else { v };
                        df.insert(key.into(), list);
                    }
                    Err(e) => d.gaps.push(format!("GET {path}: {e:#}")),
                }
            }
            Value::Object(df)
        }
    };
    let image_ids: Vec<String> =
        df_list(&d.df, "Images", "ImageUsage").iter().map(|x| s(x, "Id").to_string()).collect();
    for id in image_ids.into_iter().filter(|id| !id.is_empty()) {
        match api.get(&format!("/images/{id}/json")) {
            Ok(v) => {
                d.images.insert(id, v);
            }
            Err(e) => d.gaps.push(format!("inspect image {}: {e:#}", short_id(&id))),
        }
    }
    let ctr_ids: Vec<String> =
        df_list(&d.df, "Containers", "ContainerUsage").iter().map(|x| s(x, "Id").to_string()).collect();
    for id in ctr_ids.into_iter().filter(|id| !id.is_empty()) {
        match api.get(&format!("/containers/{id}/json")) {
            Ok(v) => {
                d.containers.insert(id, v);
            }
            Err(e) => d.gaps.push(format!("inspect container {}: {e:#}", short_id(&id))),
        }
    }
    Ok(d)
}

// ---------------------------------------------------------------- layout

/// Where an engine keeps its layers.
#[derive(Debug)]
pub enum Store {
    /// containerd image store.
    Containerd {
        ns: String,
        snap_root: String,
        content_root: String,
        /// `None` when the snapshotter's metadata.db couldn't be read.
        snaps: Option<SnapIndex>,
    },
    /// Classic graphdriver directory, e.g. `<root>/overlay2`.
    Graph { driver_dir: String, driver: String },
}

#[derive(Debug)]
pub struct Layout {
    pub root: String,
    pub store: Store,
}

pub fn uses_containerd_store(info: &Value) -> bool {
    arr(info, "DriverStatus").iter().any(|p| {
        p.as_array()
            .is_some_and(|kv| kv.iter().any(|x| x.as_str().is_some_and(|s| s.contains("io.containerd.snapshotter"))))
    })
}

/// The containerd root holding Docker's snapshots: the daemon-managed
/// containerd's config next to its socket, `<root>/containerd/daemon`, or the
/// system containerd (rootful Docker with the containerd store).
pub fn containerd_root(info: &Value, read: &dyn Fn(&str) -> Option<String>, exists: &dyn Fn(&str) -> bool) -> String {
    let root = s(info, "DockerRootDir");
    let addr = info.pointer("/Containerd/Address").and_then(|a| a.as_str()).unwrap_or("");
    if let Some(dir) = std::path::Path::new(addr).parent()
        && let Some(r) = read(&format!("{}/containerd.toml", dir.display())).and_then(|t| containerd::config_root(&t))
    {
        return r;
    }
    let managed = format!("{root}/containerd/daemon");
    if exists(&managed) || addr.is_empty() || addr.contains("/docker/containerd/") {
        return managed;
    }
    "/var/lib/containerd".into()
}

fn layout(ctx: &Ctx, info: &Value) -> Layout {
    let root = s(info, "DockerRootDir").trim_end_matches('/').to_string();
    let driver = s(info, "Driver").to_string();
    let store = if uses_containerd_store(info) {
        let read = |p: &str| ctx.runner.read(p);
        let exists = |p: &str| ctx.exists(p);
        let croot = containerd_root(info, &read, &exists);
        let snap_root = format!("{croot}/io.containerd.snapshotter.v1.{driver}");
        Store::Containerd {
            ns: info.pointer("/Containerd/Namespaces/Containers").and_then(|n| n.as_str()).unwrap_or("moby").into(),
            snaps: SnapIndex::load(&format!("{snap_root}/metadata.db")),
            snap_root,
            content_root: format!("{croot}/io.containerd.content.v1.content"),
        }
    } else {
        Store::Graph { driver_dir: format!("{root}/{driver}"), driver }
    };
    Layout { root, store }
}

// ---------------------------------------------------------------- entities

struct Builder<'a> {
    snap: &'a mut Snapshot,
    group: String,
}

impl Builder<'_> {
    fn add(&mut self, kind: &str, name: impl Into<String>, parent: Option<u32>, f: impl FnOnce(&mut Entity)) -> u32 {
        let mut e = Entity {
            kind: kind.into(),
            name: name.into(),
            provider: "docker".into(),
            group: self.group.clone(),
            parent,
            ..Default::default()
        };
        f(&mut e);
        self.snap.add_entity(e)
    }
}

fn api_action(socket: &str, label: String, steps: &[(&str, String)]) -> Option<ActionSpec> {
    Some(ActionSpec {
        label,
        steps: steps
            .iter()
            .map(|(m, p)| ActionStep::DockerApi { socket: socket.into(), method: (*m).into(), path: p.clone() })
            .collect(),
    })
}

/// `overlay2/<id>/diff` → `overlay2/<id>` for every dir in a GraphDriver.Data field.
fn graph_dirs(gd: &Value, field: &str) -> Vec<String> {
    s(gd, field)
        .split(':')
        .filter(|p| !p.is_empty())
        .map(|p| p.strip_suffix("/diff").unwrap_or(p).to_string())
        .collect()
}

fn node_alloc(snap: &Snapshot, path: &str) -> Option<u64> {
    snap.lookup_static(path).map(|n| snap.tree.node(n).alloc)
}

/// Turn one engine's API data into entities. Returns notes about gaps.
pub fn build(
    snap: &mut Snapshot,
    data: &EngineData,
    lay: &Layout,
    group: &str,
    read: &dyn Fn(&str) -> Option<String>,
) -> Vec<String> {
    let mut notes = data.gaps.clone();
    let root = lay.root.as_str();
    let sock = data.socket.as_str();
    let share = Some(format!("docker:{root}"));
    let df = &data.df;
    let images = df_list(df, "Images", "ImageUsage");
    let containers = df_list(df, "Containers", "ContainerUsage");
    let volumes = df_list(df, "Volumes", "VolumeUsage");
    let cache = df_list(df, "BuildCache", "BuildCacheUsage");
    // Without the container list, "unused" can't be decided.
    let ctrs_known = df.get("Containers").is_some_and(|c| c.is_array()) || df.get("ContainerUsage").is_some();
    let first = snap.entities.len();
    let mut b = Builder { snap, group: group.into() };

    let cname = |c: &Value| -> String {
        strs(c, "Names")
            .first()
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_else(|| short_id(s(c, "Id")).into())
    };
    let mut image_users: HashMap<&str, Vec<String>> = HashMap::new();
    let mut volume_users: HashMap<&str, Vec<String>> = HashMap::new();
    for c in containers {
        image_users.entry(s(c, "ImageID")).or_default().push(cname(c));
        for m in arr(c, "Mounts").iter().filter(|m| s(m, "Type") == "volume") {
            volume_users.entry(s(m, "Name")).or_default().push(cname(c));
        }
    }

    // ------------------------------------------------ images
    let (snaps, ns) = match &lay.store {
        Store::Containerd { snaps, ns, .. } => (snaps.as_ref(), ns.as_str()),
        Store::Graph { .. } => (None, ""),
    };
    let snap_dir = |id: u64| match &lay.store {
        Store::Containerd { snap_root, .. } => format!("{snap_root}/snapshots/{id}"),
        Store::Graph { .. } => String::new(),
    };
    let images_id = b.add("docker.images", "Images", None, |e| {
        e.reported = i(df, "LayersSize").map(|x| x as u64);
        e.attrs.push(("count".into(), images.len().to_string()));
    });
    let mut image_snaps: HashSet<u64> = HashSet::new();
    let mut mapped_any = false;
    for img in images {
        let id = s(img, "Id");
        let inspect = data.images.get(id);
        let tags: Vec<String> = strs(img, "RepoTags").into_iter().filter(|t| t != "<none>:<none>").collect();
        let dangling = tags.is_empty();
        let users = image_users.get(id).cloned().unwrap_or_default();
        let n_users = i(img, "Containers").unwrap_or(-1).max(users.len() as i64);
        let mut paths = Vec::new();
        match &lay.store {
            Store::Containerd { content_root, .. } => {
                let desc = img.get("Descriptor").or_else(|| inspect.and_then(|v| v.get("Descriptor")));
                let digest = desc.map(|d| s(d, "digest")).filter(|d| !d.is_empty()).unwrap_or(id);
                let mt = desc.map(|d| s(d, "mediaType")).filter(|m| !m.is_empty()).unwrap_or("index");
                let refs = containerd::walk_content(content_root, digest, mt, read);
                // Manifests also list blobs never pulled (other platforms, attestations).
                let scanned = b.snap.lookup_static(content_root).is_some();
                paths.extend(
                    refs.blobs
                        .iter()
                        .filter_map(|d| containerd::blob_path(content_root, d))
                        .filter(|p| !scanned || b.snap.lookup_static(p).is_some()),
                );
                let diffs = inspect.and_then(|v| v.pointer("/RootFS/Layers")).map(|l| {
                    l.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>()
                });
                if let (Some(ix), Some(diffs)) = (snaps, diffs) {
                    for c in containerd::chain_ids(&diffs) {
                        if let Some(r) = ix.get(ns, &c) {
                            image_snaps.insert(r.id);
                            paths.push(snap_dir(r.id));
                            mapped_any = true;
                        }
                    }
                }
            }
            Store::Graph { .. } => {
                if let Some(gd) = inspect.and_then(|v| v.get("GraphDriver")).and_then(|g| g.get("Data")) {
                    paths.extend(graph_dirs(gd, "UpperDir"));
                    paths.extend(graph_dirs(gd, "LowerDir"));
                    mapped_any |= !paths.is_empty();
                }
            }
        }
        let name = tags.first().cloned().unwrap_or_else(|| format!("<none> {}", short_id(id)));
        b.add("docker.image", name.clone(), Some(images_id), |e| {
            e.paths = paths;
            e.share_key = share.clone();
            e.reported = i(img, "Size").filter(|&x| x >= 0).map(|x| x as u64);
            e.attrs.push(("id".into(), id.into()));
            if tags.len() > 1 {
                e.attrs.push(("tags".into(), tags.join(", ")));
            }
            if let Some(c) = i(img, "Created") {
                e.attrs.push(("created".into(), when(c)));
            }
            if let Some(sh) = i(img, "SharedSize").filter(|&x| x > 0) {
                e.attrs.push(("shared size".into(), fmt_size(sh as u64)));
            }
            e.attrs.push(("containers".into(), if users.is_empty() { "none".into() } else { users.join(", ") }));
            if n_users > 0 || !ctrs_known && i(img, "Containers").unwrap_or(-1) < 0 {
                return;
            }
            // Images with several tags must be untagged one by one.
            let steps: Vec<(&str, String)> = if tags.len() > 1 {
                tags.iter().map(|t| ("DELETE", format!("/images/{t}"))).collect()
            } else {
                vec![("DELETE", format!("/images/{id}"))]
            };
            e.reclaim = Some(if dangling {
                Reclaim {
                    risk: Risk::Safe,
                    reason: "dangling image (untagged, no container)".into(),
                    estimate: None,
                    action: api_action(sock, format!("remove dangling image {}", short_id(id)), &steps),
                }
            } else {
                Reclaim {
                    risk: Risk::Review,
                    reason: "no container uses this image; it can be pulled or rebuilt again".into(),
                    estimate: None,
                    action: api_action(sock, format!("remove image {name}"), &steps),
                }
            });
        });
    }
    if !mapped_any && !images.is_empty() {
        // Layers we couldn't map per image: claim them as one block.
        let (p, why) = match &lay.store {
            Store::Containerd { snap_root, .. } => {
                (format!("{snap_root}/snapshots"), format!("could not read {snap_root}/metadata.db"))
            }
            Store::Graph { driver_dir, driver } => (driver_dir.clone(), format!("{driver} has no per-layer paths")),
        };
        notes.push(format!("{why}: image layers shown as one block"));
        b.add("docker.layers", "Image layers (all images)", Some(images_id), |e| e.paths = vec![p]);
    }

    // ------------------------------------------------ containers
    let ctrs_id = b.add("docker.containers", "Containers", None, |e| {
        e.attrs.push(("count".into(), containers.len().to_string()));
    });
    for c in containers {
        let id = s(c, "Id");
        let inspect = data.containers.get(id);
        let name = cname(c);
        let state = s(c, "State");
        let stopped = matches!(state, "exited" | "created" | "dead");
        let mut paths = vec![format!("{root}/containers/{id}")];
        match &lay.store {
            Store::Containerd { .. } => {
                if let Some(ix) = snaps {
                    for key in [id.to_string(), format!("{id}-init")] {
                        let Some(r) = ix.get(ns, &key) else { continue };
                        paths.push(snap_dir(r.id));
                        // Layers of an image deleted while this container kept it alive.
                        let mut cur = r.parent.clone();
                        while let Some(p) = cur.and_then(|p| ix.get(ns, &p)) {
                            if image_snaps.contains(&p.id) || p.kind != KIND_COMMITTED {
                                break;
                            }
                            paths.push(snap_dir(p.id));
                            cur = p.parent.clone();
                        }
                    }
                }
            }
            Store::Graph { .. } => {
                if let Some(gd) = inspect.and_then(|v| v.get("GraphDriver")).and_then(|g| g.get("Data")) {
                    paths.extend(graph_dirs(gd, "UpperDir"));
                    paths.extend(graph_dirs(gd, "LowerDir").into_iter().filter(|p| p.ends_with("-init")));
                }
            }
        }
        paths.sort();
        paths.dedup();
        let log_path = inspect
            .map(|v| s(v, "LogPath").to_string())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| format!("{root}/containers/{id}/{id}-json.log"));
        let log_size = node_alloc(b.snap, &log_path).or_else(|| std::fs::metadata(&log_path).ok().map(|m| m.len()));
        let mounts: Vec<String> = arr(c, "Mounts")
            .iter()
            .map(|m| match s(m, "Type") {
                "volume" => format!("volume {} → {}", s(m, "Name"), s(m, "Destination")),
                t => format!("{t} {} → {}", s(m, "Source"), s(m, "Destination")),
            })
            .collect();
        let cid = b.add("docker.container", name.clone(), Some(ctrs_id), |e| {
            e.paths = paths;
            e.reported = i(c, "SizeRw").filter(|&x| x >= 0).map(|x| x as u64);
            e.attrs.push(("id".into(), short_id(id).into()));
            e.attrs.push(("image".into(), s(c, "Image").into()));
            e.attrs.push(("status".into(), s(c, "Status").into()));
            if let Some(t) = i(c, "Created") {
                e.attrs.push(("created".into(), when(t)));
            }
            if let Some(rw) = i(c, "SizeRw").filter(|&x| x >= 0) {
                e.attrs.push(("writable layer".into(), fmt_size(rw as u64)));
            }
            if let Some(l) = log_size {
                e.attrs.push(("log".into(), fmt_size(l)));
            }
            if !mounts.is_empty() {
                e.attrs.push(("mounts".into(), mounts.join("; ")));
            }
            if stopped {
                let caveat = if inspect.is_none() { " (docker could not inspect it; removal may fail)" } else { "" };
                e.reclaim = Some(Reclaim {
                    risk: Risk::Review,
                    reason: format!(
                        "stopped container ({}); its writable layer and logs go with it{caveat}",
                        s(c, "Status")
                    ),
                    estimate: None,
                    action: api_action(
                        sock,
                        format!("remove container {name}"),
                        &[("DELETE", format!("/containers/{id}"))],
                    ),
                });
            }
        });
        if let Some(size) = log_size.filter(|&l| l >= BIG_LOG) {
            let driver = inspect.and_then(|v| v.pointer("/HostConfig/LogConfig")).map(|l| s(l, "Type")).unwrap_or("");
            b.add("docker.log", format!("{name} log"), Some(cid), |e| {
                e.paths = vec![log_path.clone()];
                e.attrs.push(("log driver".into(), if driver.is_empty() { "json-file".into() } else { driver.into() }));
                if stopped {
                    return;
                }
                e.reclaim = Some(Reclaim {
                    risk: Risk::Review,
                    reason: format!(
                        "{} container log with no size limit; truncate it (`truncate -s 0 {log_path}`) or recreate the \
                         container with `--log-opt max-size=50m --log-opt max-file=3` (or set log-opts in daemon.json)",
                        fmt_size(size)
                    ),
                    estimate: None,
                    action: None,
                });
            });
        }
    }

    // ------------------------------------------------ volumes
    let vols_id = b.add("docker.volumes", "Volumes", None, |e| {
        e.attrs.push(("count".into(), volumes.len().to_string()));
    });
    for v in volumes {
        let name = s(v, "Name");
        let mp = s(v, "Mountpoint");
        let opts = v.get("Options").cloned().unwrap_or(Value::Null);
        let usage = v.get("UsageData");
        let refs = usage.and_then(|u| i(u, "RefCount")).unwrap_or(-1);
        let users = volume_users.get(name).cloned().unwrap_or_default();
        let in_use = refs > 0 || !users.is_empty();
        let std_dir = format!("{root}/volumes/{name}");
        let mut paths = Vec::new();
        if mp.starts_with(&std_dir) {
            paths.push(std_dir.clone());
        } else if !mp.is_empty() {
            paths.push(mp.to_string());
        }
        // `local` volumes bound to a host directory keep their data there.
        let device = s(&opts, "device");
        if s(&opts, "o").split(',').any(|o| o == "bind") && device.starts_with('/') {
            paths.push(device.to_string());
        }
        // Data dirs owned by container uids are often unreadable: trust Docker's size then.
        let unreadable = paths.iter().any(|p| {
            b.snap.lookup_static(p).is_some_and(|n| b.snap.tree.node(n).has(flags::INCOMPLETE | flags::DENIED))
        });
        let reported = usage.and_then(|u| i(u, "Size")).filter(|&x| x >= 0).map(|x| x as u64);
        b.add("docker.volume", name, Some(vols_id), |e| {
            e.paths = paths;
            e.reported = reported;
            e.attrs.push(("driver".into(), s(v, "Driver").into()));
            e.attrs.push(("used by".into(), if users.is_empty() { "none".into() } else { users.join(", ") }));
            if let Some(p) = v.pointer("/Labels/com.docker.compose.project").and_then(|p| p.as_str()) {
                e.attrs.push(("compose project".into(), p.into()));
            }
            if v.pointer("/Labels/com.docker.volume.anonymous").is_some() {
                e.attrs.push(("anonymous".into(), "yes".into()));
            }
            if !s(v, "CreatedAt").is_empty() {
                e.attrs.push(("created".into(), s(v, "CreatedAt").into()));
            }
            if in_use || refs < 0 && (!ctrs_known || s(v, "Driver") != "local") {
                return;
            }
            e.reclaim = Some(Reclaim {
                risk: Risk::Review,
                reason: "volume not used by any container — it holds DATA (databases, uploads...) that is gone for \
                         good once removed"
                    .into(),
                estimate: reported.filter(|_| unreadable),
                action: api_action(sock, format!("remove volume {name}"), &[("DELETE", format!("/volumes/{name}"))]),
            });
        });
    }

    // ------------------------------------------------ build cache
    let total: i64 =
        cache.iter().filter(|r| r.get("Shared") != Some(&Value::Bool(true))).filter_map(|r| i(r, "Size")).sum();
    let reclaimable: i64 = cache
        .iter()
        .filter(|r| r.get("InUse") != Some(&Value::Bool(true)) && r.get("Shared") != Some(&Value::Bool(true)))
        .filter_map(|r| i(r, "Size"))
        .sum();
    let cache_id = b.add("docker.buildcache", "Build cache", None, |e| {
        e.reported = Some(total.max(0) as u64);
        e.attrs.push(("records".into(), cache.len().to_string()));
        e.attrs.push(("reclaimable (docker)".into(), fmt_size(reclaimable.max(0) as u64)));
        if reclaimable > 0 {
            e.reclaim = Some(Reclaim {
                risk: Risk::Safe,
                reason: "build cache not used by a running build; rebuilt on demand".into(),
                estimate: Some(reclaimable as u64),
                action: api_action(sock, "prune build cache".into(), &[("POST", "/build/prune?all=1".into())]),
            });
        }
    });
    b.add("docker.buildkit", "BuildKit state", Some(cache_id), |e| e.paths = vec![format!("{root}/buildkit")]);
    if let Some(ix) = snaps {
        let mut small: Vec<String> = Vec::new();
        let mut small_n = 0;
        for r in cache {
            let rid = s(r, "ID");
            let dirs: Vec<String> = [rid.to_string(), format!("{rid}-view")]
                .iter()
                .filter_map(|k| ix.get(ns, k))
                .map(|x| snap_dir(x.id))
                .collect();
            if dirs.is_empty() {
                continue;
            }
            let size = i(r, "Size").unwrap_or(0).max(0) as u64;
            if size >= BIG_CACHE_RECORD {
                let desc = s(r, "Description");
                let label = if desc.is_empty() { format!("{} {rid}", s(r, "Type")) } else { desc.to_string() };
                b.add("docker.buildcache.record", label, Some(cache_id), |e| {
                    e.paths = dirs;
                    e.reported = Some(size);
                    e.attrs.push(("id".into(), rid.into()));
                    e.attrs.push(("type".into(), s(r, "Type").into()));
                    e.attrs.push(("in use".into(), (r.get("InUse") == Some(&Value::Bool(true))).to_string()));
                    e.attrs.push(("shared".into(), (r.get("Shared") == Some(&Value::Bool(true))).to_string()));
                    e.attrs.push(("last used".into(), s(r, "LastUsedAt").into()));
                });
            } else {
                small.extend(dirs);
                small_n += 1;
            }
        }
        if !small.is_empty() {
            b.add("docker.buildcache.record", format!("{small_n} smaller records"), Some(cache_id), |e| {
                e.paths = small
            });
        }
    }
    if let Store::Graph { driver_dir, .. } = &lay.store {
        // Graphdriver layers that no image or container references are BuildKit's.
        let rest: Vec<String> =
            containerd::unclaimed_under(b.snap, driver_dir).into_iter().filter(|p| !p.ends_with("/l")).collect();
        if !rest.is_empty() && !cache.is_empty() {
            b.add("docker.buildcache.record", "Build cache layers", Some(cache_id), |e| {
                e.attrs.push(("mapping".into(), "layer dirs no image or container references".into()));
                e.paths = rest;
            });
        }
    }

    // ------------------------------------------------ everything else under the data root
    let other_id = b.add("docker.other", "Other", None, |_| {});
    let rest = containerd::unclaimed_under(b.snap, root);
    if rest.is_empty() && b.snap.lookup_static(root).is_none() {
        notes.push(format!("{root} is not in the scanned tree (or unreadable); sizes come from Docker only"));
    }
    let mut buckets: Vec<(&str, &str, Vec<String>)> = vec![
        ("docker.other.snapshots", "Unreferenced snapshots", vec![]),
        ("docker.other.content", "Unreferenced content blobs", vec![]),
        ("docker.other.layers", "Unreferenced layers", vec![]),
        ("docker.other.misc", "Engine metadata & misc", vec![]),
    ];
    for p in rest {
        let idx = match &lay.store {
            Store::Containerd { snap_root, content_root, .. } => {
                if p.starts_with(&format!("{snap_root}/snapshots/")) {
                    0
                } else if p.starts_with(&format!("{content_root}/blobs/")) {
                    1
                } else {
                    3
                }
            }
            Store::Graph { driver_dir, .. } => {
                if p.starts_with(&format!("{driver_dir}/")) && !p.ends_with("/l") {
                    2
                } else {
                    3
                }
            }
        };
        buckets[idx].2.push(p);
    }
    for (kind, name, paths) in buckets.into_iter().filter(|b| !b.2.is_empty()) {
        b.add(kind, name, Some(other_id), |e| {
            e.attrs.push(("entries".into(), paths.len().to_string()));
            e.paths = paths;
        });
    }
    let partial = containerd::flag_incomplete(b.snap, first);
    if partial > 0 {
        notes.push(format!(
            "{partial} items contain directories unreadable as this user (files owned by container uids); their \
             sizes are lower bounds"
        ));
    }
    notes
}

// ---------------------------------------------------------------- discovery

/// Candidate sockets with the uid of a rootless engine's owner.
fn sockets(ctx: &Ctx) -> Vec<(String, Option<u32>)> {
    let mut v: Vec<(String, Option<u32>)> = Vec::new();
    if let Ok(h) = std::env::var("DOCKER_HOST")
        && let Some(p) = h.strip_prefix("unix://")
    {
        v.push((p.to_string(), socket_owner(p)));
    }
    v.push(("/run/docker.sock".into(), None));
    v.push(("/var/run/docker.sock".into(), None));
    for (uid, _, _) in &ctx.users {
        v.push((format!("/run/user/{uid}/docker.sock"), Some(*uid)));
    }
    let mut seen = HashSet::new();
    v.retain(|(p, _)| {
        ctx.exists(p) && seen.insert(std::fs::canonicalize(p).map(|c| c.display().to_string()).unwrap_or(p.clone()))
    });
    v
}

/// `/run/user/<uid>/...` → uid.
pub fn socket_owner(path: &str) -> Option<u32> {
    path.strip_prefix("/run/user/")?.split('/').next()?.parse().ok()
}

fn group_name(info: &Value, owner: Option<&str>) -> String {
    let rootless = strs(info, "SecurityOptions").iter().any(|o| o.contains("name=rootless"));
    match (rootless, owner) {
        (true, Some(u)) => format!("Docker (rootless, {u})"),
        (true, None) => "Docker (rootless)".into(),
        (false, _) => "Docker (system)".into(),
    }
}

fn is_denied(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|c| c.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied))
}

impl Provider for Docker {
    fn name(&self) -> &'static str {
        "docker"
    }

    fn collect(&self, ctx: &Ctx, snap: &mut Snapshot) -> Outcome {
        let user_name = |uid: u32| ctx.users.iter().find(|u| u.0 == uid).map(|u| u.1.clone());
        let mut outcome = Outcome::complete();
        let mut roots_done: HashSet<String> = HashSet::new();
        let (mut ok, mut denied, mut down) = (0, 0, 0);
        for (sock, uid) in sockets(ctx) {
            let data = match fetch(&SocketApi(&sock), &sock) {
                Ok(d) => d,
                Err(e) => {
                    if is_denied(&e) {
                        denied += 1;
                        outcome
                            .notes
                            .push(format!("{sock}: permission denied (join the docker group or run with sudo)"));
                    } else {
                        down += 1;
                        outcome.notes.push(format!("{sock}: daemon not reachable ({e:#})"));
                    }
                    continue;
                }
            };
            let lay = layout(ctx, &data.info);
            if lay.root.is_empty() || !roots_done.insert(lay.root.clone()) {
                continue;
            }
            ok += 1;
            let owner = uid.and_then(user_name).or_else(|| {
                ctx.users.iter().find(|u| lay.root.starts_with(&format!("{}/", u.2.display()))).map(|u| u.1.clone())
            });
            let group = group_name(&data.info, owner.as_deref());
            let read = |p: &str| ctx.runner.read(p);
            let store = match &lay.store {
                Store::Containerd { snaps, .. } => {
                    format!(
                        "containerd image store ({})",
                        if snaps.is_some() { "layers mapped" } else { "no layer map" }
                    )
                }
                Store::Graph { driver, .. } => format!("{driver} graphdriver"),
            };
            outcome.notes.push(format!("{group}: {} at {}, {store}", s(&data.info, "ServerVersion"), lay.root));
            for n in build(snap, &data, &lay, &group, &read) {
                outcome.degrade(format!("{group}: {n}"));
            }
        }
        // Data roots whose daemon we couldn't query: claim them as one block.
        let mut roots: Vec<(String, String)> = vec![("/var/lib/docker".into(), "Docker (system)".into())];
        for (_, name, home) in &ctx.users {
            roots.push((format!("{}/.local/share/docker", home.display()), format!("Docker (rootless, {name})")));
        }
        let mut orphan = 0;
        for (root, group) in roots {
            if roots_done.contains(&root) || !ctx.exists(&root) {
                continue;
            }
            orphan += 1;
            snap.add_entity(Entity {
                kind: "docker.data".into(),
                name: "Docker data (daemon not queried)".into(),
                provider: "docker".into(),
                group,
                paths: vec![root.clone()],
                ..Default::default()
            });
            outcome.degrade(format!("{root}: no reachable daemon; shown as one block"));
        }
        match (ok, denied, down, orphan) {
            (0, 0, 0, 0) => Outcome::absent(),
            (0, d, _, _) if d > 0 => Outcome { coverage: Coverage::Denied, notes: outcome.notes },
            (_, d, n, _) if d + n > 0 => {
                outcome.degrade("some engines could not be queried");
                outcome
            }
            _ => outcome,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::containerd::testtree;

    fn fx(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/docker/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn json(name: &str) -> Value {
        serde_json::from_str(&fx(name)).unwrap()
    }

    #[test]
    fn dechunks() {
        let b = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(super::dechunk(b).unwrap(), b"Wikipedia");
    }

    /// Replays recorded API responses.
    struct FakeApi(HashMap<String, Value>);

    impl Api for FakeApi {
        fn get(&self, path: &str) -> Result<Value> {
            self.0.get(path).cloned().with_context(|| format!("no fixture for {path}"))
        }
    }

    fn rootless_api() -> FakeApi {
        let mut m = HashMap::new();
        m.insert("/info".to_string(), json("info.json"));
        m.insert("/system/df".to_string(), json("system_df.json"));
        for (id, v) in json("images_inspect.json").as_object().unwrap() {
            m.insert(format!("/images/{id}/json"), v.clone());
        }
        for (id, v) in json("containers_inspect.json").as_object().unwrap() {
            m.insert(format!("/containers/{id}/json"), v.clone());
        }
        FakeApi(m)
    }

    const ROOT: &str = "/home/u/.local/share/docker";

    /// The rootless engine recorded on the dev machine: containerd image store.
    #[test]
    fn containerd_store_end_to_end() {
        let data = fetch(&rootless_api(), "/run/user/1000/docker.sock").unwrap();
        // One container is listed by /system/df but can't be inspected (stale state on the dev machine).
        assert_eq!(data.gaps.len(), 1, "{:?}", data.gaps);
        assert!(uses_containerd_store(&data.info));
        let croot = format!("{ROOT}/containerd/daemon");
        let snap_root = format!("{croot}/io.containerd.snapshotter.v1.overlayfs");
        let content_root = format!("{croot}/io.containerd.content.v1.content");
        let ix = SnapIndex::from_bolt(
            std::fs::read(format!("{}/tests/fixtures/docker/snapshots.db", env!("CARGO_MANIFEST_DIR"))).unwrap(),
        )
        .unwrap();
        let content: serde_json::Map<String, Value> = serde_json::from_str(&fx("content.json")).unwrap();

        // A tree shaped like the real data root.
        let mut files: Vec<(String, u64)> = ix
            .recs
            .values()
            .map(|r| (format!("{snap_root}/snapshots/{}/fs/data", r.id), r.size.unwrap_or(4096).max(4096) as u64))
            .collect();
        files.push((format!("{snap_root}/metadata.db"), 2 << 20));
        for (d, v) in &content {
            files.push((containerd::blob_path(&content_root, d).unwrap(), v.as_str().map_or(0, |s| s.len() as u64)));
        }
        let df = &data.df;
        for c in df_list(df, "Containers", "ContainerUsage") {
            let id = s(c, "Id");
            let log = if strs(c, "Names").first().map(String::as_str) == Some("/tracker-poller-1") {
                4 << 30
            } else {
                1 << 20
            };
            files.push((format!("{ROOT}/containers/{id}/{id}-json.log"), log));
            files.push((format!("{ROOT}/containers/{id}/config.v2.json"), 4096));
        }
        for v in df_list(df, "Volumes", "VolumeUsage") {
            let size = v.pointer("/UsageData/Size").and_then(|x| x.as_u64()).unwrap_or(0);
            files.push((format!("{}/file", s(v, "Mountpoint")), size));
        }
        files.push((format!("{ROOT}/buildkit/cache.db"), 32 << 20));
        files.push((format!("{ROOT}/engine-id"), 4096));
        files.push((format!("{ROOT}/network/files/local-kv.db"), 128 << 10));
        let refs: Vec<(&str, u64)> = files.iter().map(|(p, s)| (p.as_str(), *s)).collect();
        let mut snap = testtree::snapshot(&refs);

        let lay = Layout {
            root: ROOT.into(),
            store: Store::Containerd {
                ns: "moby".into(),
                snap_root: snap_root.clone(),
                content_root: content_root.clone(),
                snaps: Some(ix),
            },
        };
        let read = |p: &str| {
            let d = p.rsplit('/').next()?;
            content.get(&format!("sha256:{d}")).and_then(|v| v.as_str()).map(String::from)
        };
        let group = group_name(&data.info, Some("u"));
        assert_eq!(group, "Docker (rootless, u)");
        let notes = build(&mut snap, &data, &lay, &group, &read);
        assert_eq!(notes, data.gaps);
        crate::model::attribution::attribute(&mut snap);

        let top: Vec<&str> = snap.entities.iter().filter(|e| e.parent.is_none()).map(|e| e.name.as_str()).collect();
        assert_eq!(top, ["Images", "Containers", "Volumes", "Build cache", "Other"]);
        let find = |n: &str| snap.entities.iter().find(|e| e.name == n).unwrap_or_else(|| panic!("no entity {n}"));

        // Every image's layers resolved to snapshot dirs.
        let imgs: Vec<&Entity> = snap.entities.iter().filter(|e| e.kind == "docker.image").collect();
        assert_eq!(imgs.len(), 21);
        for e in &imgs {
            let id = e.attr("id").unwrap();
            let layers = data.images[id].pointer("/RootFS/Layers").unwrap().as_array().unwrap().len();
            let n = e.paths.iter().filter(|p| p.contains("/snapshots/")).count();
            assert_eq!(n, layers, "{}", e.name);
            assert!(e.paths.iter().any(|p| p.contains("/blobs/sha256/")), "{} has no blobs", e.name);
            assert!(e.unresolved_paths.is_empty(), "{}: {:?}", e.name, e.unresolved_paths);
        }
        // The three weather indexer images share most of their layers.
        let poll = find("weather-indexer-poll:latest");
        assert!(poll.measured_unique * 4 < poll.measured_alloc);
        assert!(poll.reclaim.is_none());
        let unused = find("shop/backend:latest");
        let r = unused.reclaim.as_ref().unwrap();
        assert_eq!(r.risk, Risk::Review);
        assert_eq!(
            r.action.as_ref().unwrap().steps[0],
            ActionStep::DockerApi {
                socket: "/run/user/1000/docker.sock".into(),
                method: "DELETE".into(),
                path: format!("/images/{}", unused.attr("id").unwrap()),
            }
        );

        // Stopped (and stale) container: removable, with a caveat.
        let exited = snap.entities.iter().find(|e| e.kind == "docker.container" && e.reclaim.is_some()).unwrap();
        assert!(exited.attr("status").unwrap().starts_with("Exited"));
        assert!(exited.reclaim.as_ref().unwrap().reason.contains("could not inspect"));
        let running = find("shop-dev-control-plane");
        assert!(running.reclaim.is_none());
        assert_eq!(running.paths.iter().filter(|p| p.contains("/snapshots/")).count(), 2);

        // Big log of a running container: review, no automatic action.
        let log = find("tracker-poller-1 log");
        let r = log.reclaim.as_ref().unwrap();
        assert!(r.action.is_none() && r.reason.contains("max-size"));
        assert_eq!(log.measured_alloc, 4 << 30);

        // Volumes: unused → review with a data warning; used ones untouched.
        let v = find("robot_sim_ros_ws_install");
        assert!(v.reclaim.as_ref().unwrap().reason.contains("DATA"));
        assert_eq!(v.measured_alloc, 150353991);
        let used = find("83e57b6ae5503e631f927dd38ff58b066832adda12e5d1dda628c46964a22d3e");
        assert!(used.reclaim.is_none());
        assert_eq!(used.attr("used by"), Some("shop-dev-control-plane"));

        // Build cache: safe prune, records mapped to snapshots.
        let bc = find("Build cache");
        assert_eq!(bc.reclaim.as_ref().unwrap().risk, Risk::Safe);
        assert!(snap.entities.iter().any(|e| e.kind == "docker.buildcache.record" && e.parent == Some(bc.id)));

        // Leftovers land in Other; nothing under the root is double counted or missed.
        let misc = find("Engine metadata & misc");
        assert!(misc.paths.iter().any(|p| p.ends_with("/engine-id")));
        assert!(misc.paths.iter().any(|p| p.ends_with("/metadata.db")));
        let groups = crate::views::workloads(&snap);
        let total = snap.tree.node(snap.lookup(ROOT).unwrap()).alloc;
        assert_eq!(groups[0].total, total);
        let imgs_parent = find("Images");
        let image_union =
            snap.entities.iter().filter(|e| e.parent == Some(imgs_parent.id)).map(|e| e.measured_unique).sum::<u64>();
        assert!(imgs_parent.measured_alloc > image_union, "shared layers are only counted once");
    }

    /// Classic overlay2 graphdriver with a handwritten (but API-shaped) engine.
    #[test]
    fn overlay2_graphdriver() {
        let root = "/var/lib/docker";
        let mut m = HashMap::new();
        m.insert("/info".to_string(), json("overlay2/info.json"));
        // /system/df fails: falls back to list endpoints.
        m.insert("/images/json".to_string(), json("overlay2/images.json"));
        m.insert("/containers/json?all=1&size=1".to_string(), json("overlay2/containers.json"));
        m.insert("/volumes".to_string(), json("overlay2/volumes.json"));
        for (id, v) in json("overlay2/inspect.json").as_object().unwrap() {
            let kind = if id.starts_with("sha256:") { "images" } else { "containers" };
            m.insert(format!("/{kind}/{id}/json"), v.clone());
        }
        let data = fetch(&FakeApi(m), "/run/docker.sock").unwrap();
        assert_eq!(data.gaps.len(), 1, "{:?}", data.gaps);
        assert!(!uses_containerd_store(&data.info));

        let o = format!("{root}/overlay2");
        let mb = 1u64 << 20;
        let files = [
            (format!("{o}/base/diff/bin"), 80 * mb),
            (format!("{o}/app1/diff/app"), 20 * mb),
            (format!("{o}/app2/diff/app"), 30 * mb),
            (format!("{o}/old/diff/x"), 5 * mb),
            (format!("{o}/web-rw/diff/tmp"), 2 * mb),
            (format!("{o}/web-rw-init/diff/etc"), 4096),
            (format!("{o}/job-rw/diff/out"), 7 * mb),
            (format!("{o}/job-rw-init/diff/etc"), 4096),
            (format!("{o}/buildkit-layer/diff/cache"), 40 * mb),
            (format!("{o}/l/ABCDEF"), 0),
            (format!("{root}/containers/c1web/c1web-json.log"), 300 * mb),
            (format!("{root}/containers/c2job/c2job-json.log"), mb),
            (format!("{root}/volumes/pgdata/_data/base"), 60 * mb),
            (format!("{root}/volumes/scratch/_data/f"), 9 * mb),
            (format!("{root}/image/overlay2/repositories.json"), 4096),
            (format!("{root}/buildkit/metadata_v2.db"), mb),
        ];
        let refs: Vec<(&str, u64)> = files.iter().map(|(p, s)| (p.as_str(), *s)).collect();
        let mut snap = testtree::snapshot(&refs);
        let lay =
            Layout { root: root.into(), store: Store::Graph { driver_dir: o.clone(), driver: "overlay2".into() } };
        build(&mut snap, &data, &lay, "Docker (system)", &|_| None);
        crate::model::attribution::attribute(&mut snap);
        let find = |n: &str| snap.entities.iter().find(|e| e.name == n).unwrap_or_else(|| panic!("no entity {n}"));

        let app1 = find("app:1");
        let app2 = find("app:2");
        assert_eq!(app1.measured_alloc, 100 * mb);
        assert_eq!(app1.measured_unique, 20 * mb);
        assert_eq!(app2.measured_unique, 30 * mb);
        assert_eq!(app2.reclaim.as_ref().unwrap().risk, Risk::Review);
        assert!(app1.reclaim.is_none(), "used by a container");
        let dangling = snap.entities.iter().find(|e| e.name.starts_with("<none>")).unwrap();
        assert_eq!(dangling.reclaim.as_ref().unwrap().risk, Risk::Safe);
        assert_eq!(dangling.measured_unique, 5 * mb);

        let web = find("web");
        assert_eq!(web.measured_alloc, 2 * mb + 4096 + 300 * mb);
        assert!(web.reclaim.is_none());
        let log = find("web log");
        assert!(log.reclaim.as_ref().unwrap().action.is_none());
        let job = find("job");
        let step = &job.reclaim.as_ref().unwrap().action.as_ref().unwrap().steps[0];
        assert_eq!(
            step,
            &ActionStep::DockerApi {
                socket: "/run/docker.sock".into(),
                method: "DELETE".into(),
                path: "/containers/c2job".into()
            }
        );

        assert!(find("pgdata").reclaim.is_none());
        let scratch = find("scratch");
        assert_eq!(scratch.reclaim.as_ref().unwrap().action.as_ref().unwrap().steps.len(), 1);
        assert_eq!(scratch.measured_alloc, 9 * mb);

        // Without /system/df there is no build cache info: orphan layer dirs stay unattributed.
        let layers = find("Unreferenced layers");
        assert_eq!(layers.measured_alloc, 40 * mb);
        let misc = find("Engine metadata & misc");
        assert!(misc.paths.iter().any(|p| p.ends_with("/image")));
        assert!(misc.paths.iter().any(|p| p.ends_with("/overlay2/l")));
        let groups = crate::views::workloads(&snap);
        assert_eq!(groups[0].total, snap.tree.node(snap.lookup(root).unwrap()).alloc);
    }

    #[test]
    fn discovery_helpers() {
        assert_eq!(socket_owner("/run/user/1000/docker.sock"), Some(1000));
        assert_eq!(socket_owner("/run/docker.sock"), None);
        let info = json("info.json");
        assert_eq!(group_name(&info, None), "Docker (rootless)");
        assert_eq!(group_name(&json("overlay2/info.json"), Some("x")), "Docker (system)");
        let toml = "root = \"/srv/docker-ctd\"\n";
        let read = |p: &str| (p == "/run/user/1000/docker/containerd/containerd.toml").then(|| toml.to_string());
        assert_eq!(containerd_root(&info, &read, &|_| false), "/srv/docker-ctd");
        assert_eq!(containerd_root(&info, &|_| None, &|_| false), format!("{ROOT}/containerd/daemon"));
        let rootful: Value = serde_json::json!({
            "DockerRootDir": "/var/lib/docker",
            "Containerd": {"Address": "/run/containerd/containerd.sock"}
        });
        assert_eq!(containerd_root(&rootful, &|_| None, &|_| false), "/var/lib/containerd");
    }

    /// Live cross-check against the local engine: .
    #[test]
    #[ignore]
    fn live_docker() {
        let home = crate::util::invoking_home();
        let opts = crate::pipeline::PipelineOptions {
            scan: crate::scan::ScanOptions {
                roots: vec![home.join(".local/share/docker")],
                one_file_system: true,
                quiet: true,
                ..Default::default()
            },
            only_providers: Some(vec!["docker".into()]),
            ..Default::default()
        };
        let snap = crate::pipeline::build(&opts).unwrap();
        println!("{:<60} {:>10} {:>10} {:>10}", "entity", "total", "unique", "reported");
        for e in snap.entities.iter().filter(|e| e.provider == "docker") {
            let depth = std::iter::successors(e.parent, |&p| snap.entities[p as usize].parent).count();
            let name = format!("{}{}", "  ".repeat(depth), e.name);
            println!(
                "{:<60} {:>10} {:>10} {:>10}",
                &name[..name.len().min(60)],
                fmt_size(e.total_bytes()),
                fmt_size(e.unique_bytes()),
                e.reported.map(fmt_size).unwrap_or_default()
            );
        }
        for p in &snap.providers {
            println!("{}: {:?} {:?}", p.name, p.coverage, p.notes);
        }
    }
}
