//! Podman / containers-storage: rootless stores per user and the rootful
//! `/var/lib/containers/storage`.
//!
//! Layer directories are mapped from containers-storage's own JSON metadata
//! (`<driver>-layers/layers.json`, `<driver>-images/images.json`,
//! `<driver>-containers/containers.json`), which needs no podman binary. When
//! podman can query the store (rootless as its owner, rootful as root),
//! `podman system df -v`, `podman images` and `podman volume ls` add reported
//! sizes, container status and volume usage for reclaim decisions.

use super::containerd::{flag_incomplete, unclaimed_under};
use super::{Ctx, Outcome, Provider};
use crate::model::{ActionSpec, ActionStep, Entity, Reclaim, Risk, Snapshot};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

pub struct Podman;

/// One containers-storage store.
#[derive(Debug, Clone)]
pub struct Store {
    pub group: String,
    pub graphroot: String,
    pub driver: String,
    /// Podman can be run against this store as the current user.
    pub queryable: bool,
    /// Commands against this store need root.
    pub rootful: bool,
}

/// Output of the podman CLI for a store (all optional).
#[derive(Debug, Default)]
pub struct Cli {
    pub df: Option<Value>,
    pub images: Option<Value>,
    pub volumes: Option<Value>,
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

fn arr<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k).and_then(|x| x.as_array()).map(|a| a.as_slice()).unwrap_or(&[])
}

fn list(v: Option<&Value>) -> &[Value] {
    v.and_then(|x| x.as_array()).map(|a| a.as_slice()).unwrap_or(&[])
}

/// The entry of a CLI listing whose `key` is `id` or a prefix of it (short IDs).
fn find_by_id<'a>(list: &'a [Value], key: &str, id: &str) -> Option<&'a Value> {
    list.iter().find(|x| {
        let v = s(x, key);
        !v.is_empty() && (id.starts_with(v) || v.starts_with(id))
    })
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

/// `graphroot` and `driver` from a storage.conf, with `~`/`$HOME` expanded.
pub fn parse_storage_conf(text: &str, home: &str) -> (Option<String>, Option<String>) {
    let Ok(v) = toml::from_str::<toml::Value>(text) else { return (None, None) };
    let st = v.get("storage");
    let get = |k: &str| st.and_then(|s| s.get(k)).and_then(|x| x.as_str()).filter(|s| !s.is_empty());
    let expand = |p: &str| p.replacen("$HOME", home, 1).replacen('~', home, 1);
    let root = get("rootless_storage_path").filter(|_| !home.is_empty()).or(get("graphroot")).map(expand);
    (root, get("driver").map(String::from))
}

fn stores(ctx: &Ctx) -> Vec<Store> {
    let mut out = Vec::new();
    let has_podman = ctx.runner.has("podman");
    let mut add = |group: String, graphroot: String, driver: Option<String>, queryable: bool, rootful: bool| {
        if !ctx.exists(&graphroot) {
            return;
        }
        let driver = driver.unwrap_or_else(|| {
            ["overlay", "vfs", "btrfs", "zfs"]
                .into_iter()
                .find(|d| ctx.exists(&format!("{graphroot}/{d}-layers")))
                .unwrap_or("overlay")
                .to_string()
        });
        out.push(Store { group, graphroot, driver, queryable, rootful });
    };
    for (uid, name, home) in &ctx.users {
        let home_s = home.display().to_string();
        let (root, driver) = ctx
            .runner
            .read(&format!("{home_s}/.config/containers/storage.conf"))
            .map(|t| parse_storage_conf(&t, &home_s))
            .unwrap_or_default();
        let root = root.unwrap_or_else(|| format!("{home_s}/.local/share/containers/storage"));
        add(format!("Podman (rootless, {name})"), root, driver, has_podman && !ctx.is_root && *uid == ctx.uid, false);
    }
    let (root, driver) =
        ctx.runner.read("/etc/containers/storage.conf").map(|t| parse_storage_conf(&t, "")).unwrap_or_default();
    let root = root.unwrap_or_else(|| "/var/lib/containers/storage".into());
    add("Podman (system)".into(), root, driver, has_podman && ctx.is_root, true);
    out
}

fn podman_json(ctx: &Ctx, args: &[&str]) -> Option<Value> {
    let mut argv = vec!["podman"];
    argv.extend_from_slice(args);
    let out = ctx.runner.run(&argv).filter(|o| o.ok())?;
    serde_json::from_str(&out.stdout).ok()
}

fn cmd(store: &Store, label: String, argv: &[&str]) -> Option<ActionSpec> {
    Some(ActionSpec {
        label,
        steps: vec![ActionStep::Command { argv: argv.iter().map(|a| a.to_string()).collect(), root: store.rootful }],
    })
}

/// Build entities for one store. `read` returns a metadata file's text.
pub fn build(snap: &mut Snapshot, st: &Store, cli: &Cli, read: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let mut notes = Vec::new();
    let g = &st.graphroot;
    let d = &st.driver;
    let first = snap.entities.len();
    let load = |name: &str| -> Vec<Value> {
        read(&format!("{g}/{name}")).and_then(|t| serde_json::from_str::<Vec<Value>>(&t).ok()).unwrap_or_default()
    };
    let mut layers = load(&format!("{d}-layers/layers.json"));
    layers.extend(load(&format!("{d}-layers/volatile-layers.json")));
    let images = load(&format!("{d}-images/images.json"));
    let containers = load(&format!("{d}-containers/containers.json"));
    if layers.is_empty() && images.is_empty() {
        notes.push(format!("could not read {g}/{d}-layers/layers.json; layers shown as one block"));
    }
    let parent_of: HashMap<&str, &str> = layers.iter().map(|l| (s(l, "id"), s(l, "parent"))).collect();
    let layer_paths = |top: &str| -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = top;
        while !cur.is_empty() && out.len() < 512 {
            out.push(format!("{g}/{d}/{cur}"));
            out.push(format!("{g}/{d}-layers/{cur}.tar-split.gz"));
            cur = parent_of.get(cur).copied().unwrap_or("");
        }
        out
    };
    let share = Some(format!("podman:{g}"));

    // CLI facts keyed by (prefix of) id.
    let df = cli.df.as_ref();
    let df_images = df.map(|v| arr(v, "Images")).unwrap_or(&[]);
    let df_ctrs = df.map(|v| arr(v, "Containers")).unwrap_or(&[]);
    let df_vols = df.map(|v| arr(v, "Volumes")).unwrap_or(&[]);
    let cli_images = list(cli.images.as_ref());
    let used_images: HashSet<&str> = containers.iter().map(|c| s(c, "image")).collect();

    let add = |snap: &mut Snapshot, kind: &str, name: String, parent: Option<u32>, f: &mut dyn FnMut(&mut Entity)| {
        let mut e = Entity {
            kind: kind.into(),
            name,
            provider: "podman".into(),
            group: st.group.clone(),
            parent,
            ..Default::default()
        };
        f(&mut e);
        // Not every layer has a tar-split file: keep only paths that exist in a scanned store.
        if snap.lookup_static(g).is_some() {
            e.paths.retain(|p| snap.lookup_static(p).is_some());
        }
        snap.add_entity(e)
    };

    // ------------------------------------------------ images
    let images_id = add(snap, "podman.images", "Images".into(), None, &mut |e| {
        e.reported = df.and_then(|v| v.get("ImagesSize")).and_then(|x| x.as_u64());
    });
    if layers.is_empty() && images.is_empty() {
        add(snap, "podman.layers", "Image layers (all images)".into(), Some(images_id), &mut |e| {
            e.paths = vec![format!("{g}/{d}")];
        });
    }
    for img in &images {
        let id = s(img, "id");
        let names: Vec<String> = arr(img, "names").iter().filter_map(|n| n.as_str().map(String::from)).collect();
        let dfi = find_by_id(df_images, "ImageID", id);
        let clii = find_by_id(cli_images, "Id", id);
        let n_ctrs = dfi
            .and_then(|x| x.get("Containers"))
            .or_else(|| clii.and_then(|x| x.get("Containers")))
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
            .max(used_images.contains(id) as u64);
        let dangling = clii.and_then(|x| x.get("Dangling")).and_then(|x| x.as_bool()).unwrap_or(names.is_empty());
        let mut paths = layer_paths(s(img, "layer"));
        paths.push(format!("{g}/{d}-images/{id}"));
        let name = names.first().cloned().unwrap_or_else(|| format!("<none> {}", short(id)));
        add(snap, "podman.image", name.clone(), Some(images_id), &mut |e| {
            e.paths = paths.clone();
            e.share_key = share.clone();
            e.reported = dfi.or(clii).and_then(|x| x.get("Size")).and_then(|x| x.as_u64());
            e.attrs.push(("id".into(), short(id).into()));
            if names.len() > 1 {
                e.attrs.push(("names".into(), names.join(", ")));
            }
            if !s(img, "created").is_empty() {
                e.attrs.push(("created".into(), s(img, "created").into()));
            }
            e.attrs.push(("containers".into(), n_ctrs.to_string()));
            if !st.queryable || n_ctrs > 0 {
                return;
            }
            e.reclaim = Some(Reclaim {
                risk: if dangling { Risk::Safe } else { Risk::Review },
                reason: if dangling {
                    "dangling image (untagged, no container)".into()
                } else {
                    "no container uses this image; it can be pulled or rebuilt again".into()
                },
                estimate: None,
                action: cmd(st, format!("remove image {name}"), &["podman", "rmi", id]),
            });
        });
    }

    // ------------------------------------------------ containers
    let ctrs_id = add(snap, "podman.containers", "Containers".into(), None, &mut |_| {});
    for c in &containers {
        let id = s(c, "id");
        let meta: Value = serde_json::from_str(s(c, "metadata")).unwrap_or(Value::Null);
        let name = arr(c, "names").first().and_then(|n| n.as_str()).unwrap_or(short(id)).to_string();
        let dfc = find_by_id(df_ctrs, "ContainerID", id);
        let status = dfc.map(|x| s(x, "Status")).unwrap_or("");
        let stopped = ["exited", "created", "stopped", "configured", "dead"].iter().any(|k| status.starts_with(k));
        let mut paths = vec![format!("{g}/{d}-containers/{id}")];
        if !s(c, "layer").is_empty() {
            paths.push(format!("{g}/{d}/{}", s(c, "layer")));
            paths.push(format!("{g}/{d}-layers/{}.tar-split.gz", s(c, "layer")));
        }
        add(snap, "podman.container", name.clone(), Some(ctrs_id), &mut |e| {
            e.paths = paths.clone();
            e.reported = dfc.and_then(|x| x.get("RWSize")).and_then(|x| x.as_u64());
            e.attrs.push(("id".into(), short(id).into()));
            let image = meta.get("image-name").and_then(|x| x.as_str()).unwrap_or(short(s(c, "image")));
            e.attrs.push(("image".into(), image.into()));
            if !status.is_empty() {
                e.attrs.push(("status".into(), status.into()));
            }
            if st.queryable && stopped {
                e.reclaim = Some(Reclaim {
                    risk: Risk::Review,
                    reason: format!("stopped container ({status}); its writable layer goes with it"),
                    estimate: None,
                    action: cmd(st, format!("remove container {name}"), &["podman", "rm", id]),
                });
            }
        });
    }

    // ------------------------------------------------ volumes
    let vols_id = add(snap, "podman.volumes", "Volumes".into(), None, &mut |_| {});
    let mut vols: Vec<(String, String)> =
        list(cli.volumes.as_ref()).iter().map(|v| (s(v, "Name").to_string(), s(v, "Mountpoint").to_string())).collect();
    if vols.is_empty() {
        // Without podman: every directory under volumes/ is a volume.
        if let Some(n) = snap.lookup_static(&format!("{g}/volumes")) {
            vols = snap.tree.children(n).map(|c| (snap.tree.name(c).into_owned(), String::new())).collect();
        }
    }
    for (name, mp) in vols {
        let dir = format!("{g}/volumes/{name}");
        let path = if mp.is_empty() || mp.starts_with(&dir) { dir } else { mp };
        let dfv = df_vols.iter().find(|x| s(x, "VolumeName") == name);
        let links = dfv.and_then(|x| x.get("Links")).and_then(|x| x.as_u64());
        add(snap, "podman.volume", name.clone(), Some(vols_id), &mut |e| {
            e.paths = vec![path.clone()];
            e.reported = dfv.and_then(|x| x.get("Size")).and_then(|x| x.as_u64());
            if let Some(l) = links {
                e.attrs.push(("containers".into(), l.to_string()));
            }
            if st.queryable && links == Some(0) {
                e.reclaim = Some(Reclaim {
                    risk: Risk::Review,
                    reason: "volume not used by any container — it holds DATA that is gone for good once removed"
                        .into(),
                    estimate: None,
                    action: cmd(st, format!("remove volume {name}"), &["podman", "volume", "rm", &name]),
                });
            }
        });
    }

    // ------------------------------------------------ the rest of the store
    let rest = unclaimed_under(snap, g);
    if !rest.is_empty() {
        add(snap, "podman.other", "Other (metadata, unreferenced layers)".into(), None, &mut |e| {
            e.attrs.push(("entries".into(), rest.len().to_string()));
            e.paths = rest.clone();
        });
    } else if snap.lookup_static(g).is_none() {
        notes.push(format!("{g} is not in the scanned tree; sizes come from podman only"));
    }
    let partial = flag_incomplete(snap, first);
    if partial > 0 {
        notes.push(format!("{partial} items contain directories unreadable as this user; sizes are lower bounds"));
    }
    notes
}

impl Provider for Podman {
    fn name(&self) -> &'static str {
        "podman"
    }

    fn collect(&self, ctx: &Ctx, snap: &mut Snapshot) -> Outcome {
        let stores = stores(ctx);
        if stores.is_empty() {
            return Outcome::absent();
        }
        let mut outcome = Outcome::complete();
        for st in stores {
            let cli = if st.queryable {
                Cli {
                    df: podman_json(ctx, &["system", "df", "-v", "--format", "json"]),
                    images: podman_json(ctx, &["images", "--format", "json"]),
                    volumes: podman_json(ctx, &["volume", "ls", "--format", "json"]),
                }
            } else {
                Cli::default()
            };
            if !st.queryable {
                outcome.degrade(format!(
                    "{}: {}; mapped from storage metadata only (no reclaim suggestions)",
                    st.group,
                    if ctx.runner.has("podman") {
                        "podman can't query this store as this user"
                    } else {
                        "podman not installed"
                    }
                ));
            } else if cli.df.is_none() {
                outcome.degrade(format!("{}: `podman system df` failed", st.group));
            }
            outcome.notes.push(format!("{}: {} ({})", st.group, st.graphroot, st.driver));
            let read = |p: &str| ctx.runner.read(p);
            for n in build(snap, &st, &cli, &read) {
                outcome.degrade(format!("{}: {n}", st.group));
            }
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Ctx;
    use crate::providers::containerd::testtree;
    use crate::providers::runner::FakeRunner;

    const G: &str = "/home/alice/.local/share/containers/storage";

    fn fx(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/podman/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn runner() -> FakeRunner {
        FakeRunner::default()
            .with("podman system df -v --format json", &fx("system-df.json"))
            .with("podman images --format json", &fx("images.json"))
            .with("podman volume ls --format json", &fx("volume-ls.json"))
            .file(&format!("{G}/overlay-layers/layers.json"), &fx("storage/layers.json"))
            .file(&format!("{G}/overlay-images/images.json"), &fx("storage/images.json"))
            .file(&format!("{G}/overlay-containers/containers.json"), &fx("storage/containers.json"))
    }

    fn tree() -> Snapshot {
        let mb = 1u64 << 20;
        let files: Vec<(String, u64)> = vec![
            (format!("{G}/overlay/l1base/diff/bin/sh"), 7 * mb),
            (format!("{G}/overlay/l2nginx/diff/usr/sbin/nginx"), 40 * mb),
            (format!("{G}/overlay/l3py/diff/usr/bin/python3"), 50 * mb),
            (format!("{G}/overlay/l4old/diff/x"), 3 * mb),
            (format!("{G}/overlay/rwweb/diff/tmp"), 2 * mb),
            (format!("{G}/overlay/rwjob/diff/out"), 6 * mb),
            (format!("{G}/overlay/orphan/diff/junk"), mb),
            (format!("{G}/overlay/l/SHORTLINK"), 0),
            (format!("{G}/overlay-layers/layers.json"), 8192),
            (format!("{G}/overlay-layers/l1base.tar-split.gz"), 65536),
            (format!("{G}/overlay-images/img-alpine/manifest"), 4096),
            (format!("{G}/overlay-images/img-nginx/manifest"), 4096),
            (format!("{G}/overlay-images/img-python/manifest"), 4096),
            (format!("{G}/overlay-images/img-dangling/manifest"), 4096),
            (format!("{G}/overlay-containers/ctr-web/userdata/config.json"), 16384),
            (format!("{G}/overlay-containers/ctr-job/userdata/config.json"), 16384),
            (format!("{G}/volumes/pgdata/_data/base"), 30 * mb),
            (format!("{G}/volumes/cache/_data/f"), 4 * mb),
            (format!("{G}/db.sql"), 256 << 10),
        ];
        let refs: Vec<(&str, u64)> = files.iter().map(|(p, s)| (p.as_str(), *s)).collect();
        testtree::snapshot(&refs)
    }

    #[test]
    fn maps_store_with_podman() {
        let r = runner();
        let ctx = Ctx {
            runner: &r,
            is_root: false,
            uid: 1000,
            home: "/home/alice".into(),
            mounts: &[],
            users: vec![(1000, "alice".into(), "/home/alice".into())],
        };
        let mut snap = tree();
        let st = Store {
            group: "Podman (rootless, alice)".into(),
            graphroot: G.into(),
            driver: "overlay".into(),
            queryable: true,
            rootful: false,
        };
        let cli = Cli {
            df: podman_json(&ctx, &["system", "df", "-v", "--format", "json"]),
            images: podman_json(&ctx, &["images", "--format", "json"]),
            volumes: podman_json(&ctx, &["volume", "ls", "--format", "json"]),
        };
        assert!(cli.df.is_some() && cli.images.is_some() && cli.volumes.is_some());
        let read = |p: &str| crate::providers::runner::CommandRunner::read(&r, p);
        let notes = build(&mut snap, &st, &cli, &read);
        assert!(notes.is_empty(), "{notes:?}");
        crate::model::attribution::attribute(&mut snap);
        let find = |n: &str| snap.entities.iter().find(|e| e.name == n).unwrap_or_else(|| panic!("no {n}"));
        let mb = 1u64 << 20;

        let nginx = find("docker.io/library/nginx:1.27");
        assert_eq!(nginx.measured_alloc, 47 * mb + 65536 + 4096);
        assert_eq!(nginx.measured_unique, 40 * mb + 4096, "base layer is shared with python");
        assert!(nginx.reclaim.is_none(), "used by web");
        let py = find("docker.io/library/python:3.12-slim");
        let r = py.reclaim.as_ref().unwrap();
        assert_eq!(r.risk, Risk::Review);
        assert_eq!(
            r.action.as_ref().unwrap().steps[0],
            ActionStep::Command { argv: vec!["podman".into(), "rmi".into(), "img-python".into()], root: false }
        );
        let dangling = snap.entities.iter().find(|e| e.name.starts_with("<none>")).unwrap();
        assert_eq!(dangling.reclaim.as_ref().unwrap().risk, Risk::Safe);

        let web = find("web");
        assert!(web.reclaim.is_none());
        assert_eq!(web.measured_alloc, 2 * mb + 16384);
        let job = find("job");
        assert_eq!(job.reclaim.as_ref().unwrap().risk, Risk::Review);

        assert!(find("pgdata").reclaim.is_none());
        let cache = find("cache");
        assert_eq!(cache.reclaim.as_ref().unwrap().risk, Risk::Review);
        assert_eq!(cache.measured_alloc, 4 * mb);

        let other = find("Other (metadata, unreferenced layers)");
        assert!(other.paths.iter().any(|p| p.ends_with("/overlay/orphan")));
        assert!(other.paths.iter().any(|p| p.ends_with("/db.sql")));
        let groups = crate::views::workloads(&snap);
        assert_eq!(groups[0].total, snap.tree.node(snap.lookup(G).unwrap()).alloc);
    }

    #[test]
    fn metadata_only_without_podman() {
        let r = FakeRunner::default()
            .file(&format!("{G}/overlay-layers/layers.json"), &fx("storage/layers.json"))
            .file(&format!("{G}/overlay-images/images.json"), &fx("storage/images.json"))
            .file(&format!("{G}/overlay-containers/containers.json"), &fx("storage/containers.json"));
        let mut snap = tree();
        let st = Store {
            group: "Podman (system)".into(),
            graphroot: G.into(),
            driver: "overlay".into(),
            queryable: false,
            rootful: true,
        };
        let read = |p: &str| crate::providers::runner::CommandRunner::read(&r, p);
        build(&mut snap, &st, &Cli::default(), &read);
        crate::model::attribution::attribute(&mut snap);
        assert!(snap.entities.iter().all(|e| e.reclaim.is_none()));
        // Volumes are found by listing the volumes directory.
        assert!(snap.entities.iter().any(|e| e.kind == "podman.volume" && e.name == "pgdata"));
        let py = snap.entities.iter().find(|e| e.name == "docker.io/library/python:3.12-slim").unwrap();
        assert_eq!(py.measured_alloc, 57 * (1 << 20) + 65536 + 4096);
    }

    #[test]
    fn storage_conf() {
        let t = "[storage]\ndriver = \"overlay\"\ngraphroot = \"/srv/containers\"\n";
        assert_eq!(parse_storage_conf(t, ""), (Some("/srv/containers".into()), Some("overlay".into())));
        let t = "[storage]\nrootless_storage_path = \"$HOME/podman-store\"\n";
        assert_eq!(parse_storage_conf(t, "/home/a").0.as_deref(), Some("/home/a/podman-store"));
        assert_eq!(parse_storage_conf("not toml [", ""), (None, None));
    }
}
