//! API tests against the router, on a snapshot built from a real tempdir scan.

use super::api::AppState;
use crate::model::{ActionSpec, ActionStep, Entity, FileTree, FsInfo, Reclaim, Risk, Snapshot};
use crate::scan::walker;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;

const TOKEN: &str = "00112233445566778899aabbccddeeff";

fn walk(tree: &mut FileTree, p: &Path, mountpoints: &HashSet<PathBuf>) -> FsInfo {
    let hl = walker::new_hardlink_set();
    let progress = walker::Progress::default();
    let st = std::fs::metadata(p).unwrap();
    let ctx = walker::WalkCtx {
        mountpoints,
        dev: (libc::major(st.dev()), libc::minor(st.dev())),
        allow_dev_change: false,
        excludes: None,
        hardlinks: &hl,
        progress: &progress,
    };
    let d = walker::walk_root(&ctx, p, crate::scan::path_bytes(p));
    let mut info = FsInfo {
        scan_root: p.to_string_lossy().into(),
        mount_point: p.to_string_lossy().into(),
        fstype: "ext4".into(),
        ..Default::default()
    };
    info.scanned_alloc = d.alloc;
    let r = crate::scan::append_tree(tree, d, &mut info);
    info.root_node = Some(r);
    tree.roots.push(r);
    info
}

struct Fixture {
    _td: tempfile::TempDir,
    root: PathBuf,
    state: Arc<AppState>,
}

fn reclaim(risk: Risk, path: &Path) -> Option<Reclaim> {
    Some(Reclaim {
        risk,
        reason: "test".into(),
        estimate: None,
        action: Some(ActionSpec {
            label: "empty test dir".into(),
            steps: vec![ActionStep::EmptyDir { path: path.display().to_string() }],
        }),
    })
}

/// Stands in for `actions::execute` so tests don't write to the user's audit log;
/// performs the same preflight and the one step kind the fixture uses.
fn test_executor(spec: &ActionSpec, _: Risk, _: u64) -> anyhow::Result<()> {
    crate::actions::preflight(spec)?;
    for s in &spec.steps {
        let ActionStep::EmptyDir { path } = s else { anyhow::bail!("unexpected step") };
        for e in std::fs::read_dir(path)? {
            let e = e?;
            if e.file_type()?.is_dir() {
                std::fs::remove_dir_all(e.path())?;
            } else {
                std::fs::remove_file(e.path())?;
            }
        }
    }
    Ok(())
}

fn fixture(is_root: bool) -> Fixture {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("root");
    for d in ["a/many", "cache/sub", "mnt/inner", "vm"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(root.join("a/big"), vec![1u8; 256 * 1024]).unwrap();
    for i in 0..30 {
        std::fs::write(root.join(format!("a/many/f{i:02}")), vec![2u8; 4096 * (i + 1)]).unwrap();
    }
    std::fs::write(root.join("cache/x"), vec![3u8; 64 * 1024]).unwrap();
    std::fs::write(root.join("cache/sub/y"), vec![3u8; 8192]).unwrap();
    std::fs::write(root.join("mnt/inner/data"), vec![4u8; 128 * 1024]).unwrap();
    std::fs::write(root.join("vm/disk.img"), vec![5u8; 32 * 1024]).unwrap();

    let mut tree = FileTree::default();
    let mps: HashSet<PathBuf> = [root.join("mnt")].into_iter().collect();
    let fs_root = walk(&mut tree, &root, &mps);
    let fs_mnt = walk(&mut tree, &root.join("mnt"), &HashSet::new());
    let r = |p: &str| root.join(p).display().to_string();
    let entities = vec![
        Entity {
            kind: "cache".into(),
            name: "test-cache".into(),
            provider: "test".into(),
            group: "Caches".into(),
            paths: vec![r("cache")],
            reclaim: reclaim(Risk::Safe, &root.join("cache")),
            ..Default::default()
        },
        Entity {
            kind: "vm".into(),
            name: "testvm".into(),
            provider: "test".into(),
            group: "VMs".into(),
            ..Default::default()
        },
        Entity {
            kind: "vm.disk".into(),
            name: "testvm-disk".into(),
            provider: "test".into(),
            group: "VMs".into(),
            parent: Some(1),
            paths: vec![r("vm/disk.img"), r("missing")],
            reclaim: reclaim(Risk::Danger, &root.join("vm")),
            ..Default::default()
        },
        Entity {
            kind: "data".into(),
            name: "inner".into(),
            provider: "test".into(),
            group: "Data".into(),
            paths: vec![r("mnt/inner")],
            ..Default::default()
        },
    ];
    let mut snap = Snapshot { tree, filesystems: vec![fs_root, fs_mnt], entities, ..Default::default() };
    crate::model::attribution::attribute(&mut snap);
    let mut st = AppState::new(snap, None, TOKEN.into(), is_root);
    st.executor = test_executor;
    Fixture { _td: td, root, state: Arc::new(st) }
}

async fn call(f: &Fixture, method: &str, uri: &str, body: Option<Value>, token: bool) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    if token {
        b = b.header("x-diskeye-token", TOKEN);
    }
    let req = match body {
        Some(v) => b.header("content-type", "application/json").body(Body::from(v.to_string())).unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = super::router(f.state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn get(f: &Fixture, uri: &str) -> Value {
    let (s, v) = call(f, "GET", uri, None, true).await;
    assert_eq!(s, StatusCode::OK, "{uri}: {v}");
    v
}

#[tokio::test]
async fn auth_is_required() {
    let f = fixture(false);
    let (s, _) = call(&f, "GET", "/api/summary", None, false).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call(&f, "GET", "/api/summary?token=wrong", None, false).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call(&f, "GET", &format!("/api/summary?token={TOKEN}"), None, false).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(&f, "POST", "/api/action/0/execute", Some(serde_json::json!({"confirm": "yes"})), false).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(std::fs::read_dir(f.root.join("cache")).unwrap().count(), 2, "nothing deleted without token");
    // Static assets need no token.
    let resp = super::router(f.state.clone()).oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    assert!(resp.headers()["content-type"].to_str().unwrap().starts_with("text/html"));
}

#[tokio::test]
async fn host_header_is_checked() {
    let td = fixture(false);
    let mut st = AppState::new(td.state.snap.clone(), None, TOKEN.into(), false);
    st.allowed_hosts = vec!["127.0.0.1:7878".into()];
    let app = super::router(Arc::new(st));
    let req = Request::get("/api/summary").header("host", "evil.example:7878").header("x-diskeye-token", TOKEN);
    let resp = app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let req = Request::get("/api/summary").header("host", "127.0.0.1:7878").header("x-diskeye-token", TOKEN);
    let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn summary_and_static_views() {
    let f = fixture(false);
    let s = get(&f, "/api/summary").await;
    assert_eq!(s["counts"]["entities"], 4);
    assert_eq!(s["counts"]["scanned_filesystems"], 2);
    assert!(s["groups"].as_array().unwrap().iter().any(|g| g == "VMs"));
    for ep in ["/api/physical", "/api/filesystems", "/api/hotspots", "/api/deleted-open", "/api/snapshots"] {
        get(&f, ep).await;
    }
    let roots = get(&f, "/api/roots").await;
    assert_eq!(roots["top"].as_array().unwrap().len(), 1, "mnt is stitched under root: {roots}");
    let (s, _) = call(&f, "GET", "/api/nope", None, true).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn node_children_folding_and_stitching() {
    let f = fixture(false);
    let root_id = f.state.snap.filesystems[0].root_node.unwrap();
    let v = get(&f, &format!("/api/node/{root_id}?depth=2&limit=10")).await;
    let kids = v["children"].as_array().unwrap();
    let names: Vec<&str> = kids.iter().map(|k| k["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"a") && names.contains(&"mnt"), "{names:?}");
    // Sorted by allocated size, largest first.
    let vals: Vec<u64> = kids.iter().map(|k| k["value"].as_u64().unwrap()).collect();
    assert!(vals.windows(2).all(|w| w[0] >= w[1]), "{vals:?}");
    // The mountpoint is stitched to the other filesystem's root.
    let mnt = kids.iter().find(|k| k["name"] == "mnt").unwrap();
    let mnt_root = f.state.snap.filesystems[1].root_node.unwrap();
    assert_eq!(mnt["id"], mnt_root);
    assert!(mnt["value"].as_u64().unwrap() >= 128 * 1024);
    assert!(mnt["children"].as_array().is_some_and(|c| !c.is_empty()), "depth=2 includes grandchildren");
    // The inner directory is owned by an entity.
    assert_eq!(mnt["children"][0]["owner"]["entity"]["name"], "inner");

    // The root's size includes the filesystem mounted below it.
    let own = f.state.snap.tree.node(root_id).alloc;
    assert_eq!(v["own_alloc"], own);
    assert_eq!(v["alloc"].as_u64().unwrap(), own + f.state.snap.tree.node(mnt_root).alloc);
    assert_eq!(v["value"], v["alloc"]);

    // Folding: a/many has 30 files; limit 10 → 10 rows + one aggregate.
    let many = f.state.snap.lookup(&f.root.join("a/many").display().to_string()).unwrap();
    let v = get(&f, &format!("/api/node/{many}?limit=10&metric=apparent")).await;
    let kids = v["children"].as_array().unwrap();
    assert_eq!(kids.len(), 11);
    assert_eq!(kids[0]["name"], "f29");
    let other = &kids[10];
    assert_eq!(other["other"], true);
    assert_eq!(other["count"], 20);
    let total: u64 = kids.iter().map(|k| k["value"].as_u64().unwrap()).sum();
    assert_eq!(total, (1..=30).map(|i| 4096 * i).sum::<u64>());

    // Breadcrumb across the mount: [root, mnt, inner].
    let inner = f.state.snap.lookup(&f.root.join("mnt/inner").display().to_string()).unwrap();
    let v = get(&f, &format!("/api/node/{inner}")).await;
    let crumbs: Vec<&str> = v["crumbs"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(crumbs.len(), 3, "{crumbs:?}");
    assert_eq!(&crumbs[1..], ["mnt", "inner"]);
    assert_eq!(v["owners"][0]["name"], "inner");

    let (s, _) = call(&f, "GET", "/api/node/999999", None, true).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn path_lookup() {
    let f = fixture(false);
    let p = f.root.join("a/many/f03").display().to_string();
    let v = get(&f, &format!("/api/path?p={p}")).await;
    assert_eq!(v["path"], p);
    let v = get(&f, &format!("/api/path?p={}", f.root.join("mnt").display())).await;
    assert_eq!(v["id"], f.state.snap.filesystems[1].root_node.unwrap(), "mountpoint resolves to the mounted root");
    let (s, _) = call(&f, "GET", &format!("/api/path?p={}/nope", f.root.display()), None, true).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn workloads_and_entities() {
    let f = fixture(false);
    let w = get(&f, "/api/workloads").await;
    let groups = w["groups"].as_array().unwrap();
    let vms = groups.iter().find(|g| g["name"] == "VMs").unwrap();
    assert_eq!(vms["entities"][0]["name"], "testvm");
    assert_eq!(vms["entities"][0]["children"], 1);
    assert!(vms["total"].as_u64().unwrap() > 0);

    let e = get(&f, "/api/entity/1").await;
    assert_eq!(e["child_list"][0]["name"], "testvm-disk");
    let d = get(&f, "/api/entity/2").await;
    assert_eq!(d["ancestors"][0]["name"], "testvm");
    let paths = d["path_list"].as_array().unwrap();
    assert!(paths[0]["node"].is_u64());
    assert!(paths[1]["node"].is_null());
    assert_eq!(d["unresolved_paths"].as_array().unwrap().len(), 1);
    assert!(d["action"]["steps"][0].as_str().unwrap().starts_with("delete contents of"));
}

#[tokio::test]
async fn reclaim_preview_and_execute() {
    let f = fixture(false);
    let r = get(&f, "/api/reclaim").await;
    let items = r["items"].as_array().unwrap();
    assert_eq!(items[0]["name"], "test-cache");
    assert_eq!(items[0]["risk"], "safe");

    // GET never executes.
    let (s, _) = call(&f, "GET", "/api/action/0/execute", None, true).await;
    assert_eq!(s, StatusCode::METHOD_NOT_ALLOWED);

    let (s, p) = call(&f, "POST", "/api/action/0/preview", None, true).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(p["preflight"]["ok"], true, "{p}");
    assert_eq!(p["confirm"]["phrase"], "yes");
    assert!(p["steps"][0].as_str().unwrap().contains("/cache/*"));

    let cache = f.root.join("cache");
    let (s, v) = call(&f, "POST", "/api/action/0/execute", Some(serde_json::json!({"confirm": "no"})), true).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, _) = call(&f, "POST", "/api/action/0/execute", Some(serde_json::json!({})), true).await;
    assert!(s.is_client_error());
    assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 2, "nothing deleted after rejected confirms");

    let (s, v) = call(&f, "POST", "/api/action/0/execute", Some(serde_json::json!({"confirm": "yes"})), true).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["ok"], true);
    assert!(cache.exists());
    assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);
    let r = get(&f, "/api/reclaim").await;
    assert_eq!(r["items"][0]["done"]["ok"], true);

    // Danger items need the entity's name, not "yes".
    let (_, p) = call(&f, "POST", "/api/action/2/preview", None, true).await;
    assert_eq!(p["confirm"]["phrase"], "testvm-disk");
    let (s, _) = call(&f, "POST", "/api/action/2/execute", Some(serde_json::json!({"confirm": "yes"})), true).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(f.root.join("vm/disk.img").exists());
    let (s, v) =
        call(&f, "POST", "/api/action/2/execute", Some(serde_json::json!({"confirm": "testvm-disk"})), true).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(!f.root.join("vm/disk.img").exists());

    // Entities without an action can't be executed.
    let (s, _) = call(&f, "POST", "/api/action/3/execute", Some(serde_json::json!({"confirm": "yes"})), true).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn root_server_requires_names() {
    let f = fixture(true);
    let (_, p) = call(&f, "POST", "/api/action/0/preview", None, true).await;
    assert_eq!(p["confirm"]["phrase"], "test-cache");
    let (s, _) = call(&f, "POST", "/api/action/0/execute", Some(serde_json::json!({"confirm": "yes"})), true).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(std::fs::read_dir(f.root.join("cache")).unwrap().count(), 2);
}

#[tokio::test]
async fn diff_rejects_bad_names() {
    let f = fixture(false);
    let (s, _) = call(&f, "GET", "/api/diff?against=../../etc/passwd", None, true).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(&f, "GET", "/api/diff?against=x.dkeye&threshold=lots", None, true).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rescan_swaps_the_snapshot() {
    let f = fixture(false);
    let fresh = f.state.snap.clone();
    let rescanner: super::api::Rescanner =
        Arc::new(move || Ok((Snapshot { entities: fresh.entities[..1].to_vec(), ..fresh.clone() }, None)));
    let app = super::router_with(super::api::Server::new(f.state.clone(), Some(rescanner)));
    let send = |method: &str| {
        let req = Request::builder().method(method).uri("/api/rescan").header("x-diskeye-token", TOKEN);
        app.clone().oneshot(req.body(Body::empty()).unwrap())
    };
    let json = |resp: axum::response::Response| async move {
        serde_json::from_slice::<Value>(&axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap()
    };
    let resp = send("POST").await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let mut status = Value::Null;
    for _ in 0..200 {
        status = json(send("GET").await.unwrap()).await;
        if status["running"] == false {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(status["generation"], 1, "{status}");
    assert!(status["error"].is_null());
    let req = Request::get("/api/summary").header("x-diskeye-token", TOKEN);
    let s = json(app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap()).await;
    assert_eq!(s["counts"]["entities"], 1);
}

#[tokio::test]
async fn rescan_unavailable_without_rescanner() {
    let f = fixture(false);
    let (s, v) = call(&f, "POST", "/api/rescan", None, true).await;
    assert_eq!(s, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(v["ok"], false);
    assert_eq!(get(&f, "/api/rescan").await["available"], false);
}
