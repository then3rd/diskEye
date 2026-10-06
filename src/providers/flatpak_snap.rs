//! Flatpak installations (system and per-user) and Snap packages.
//!
//! Flatpak deployments (`<inst>/app/<id>`, `<inst>/runtime/<id>`) are
//! hardlink checkouts of objects in the OSTree repo (`<inst>/repo`). The
//! scanner counts a hardlinked inode once, wherever the parallel walk meets
//! it first, so bytes are split arbitrarily between an app and the repo. The
//! installation total is exact; per-app numbers are not, which is why each
//! app/runtime also carries flatpak's own size as `reported`, and unused
//! runtimes are estimated from that.

use super::classifier::{Claimed, alloc_of, homes, user_suffix};
use super::{Ctx, Outcome, Provider};
use crate::model::tree::Kind;
use crate::model::{ActionSpec, ActionStep, Entity, FileTree, NodeId, Reclaim, Risk, Snapshot};
use std::collections::{HashMap, HashSet};

pub struct FlatpakSnap;

const SNAPS_DIR: &str = "/var/lib/snapd/snaps";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FlatpakRow {
    pub id: String,
    pub branch: String,
    pub installation: String,
    pub size: Option<u64>,
    /// Runtime ref of an app: `org.gnome.Platform/x86_64/46`.
    pub runtime: Option<String>,
}

/// Tab-separated `flatpak list --columns=application,branch,installation,size[,runtime]`.
pub fn parse_flatpak_list(out: &str) -> Vec<FlatpakRow> {
    out.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').map(str::trim).collect();
            if f.len() < 4 || f[0].is_empty() {
                return None;
            }
            Some(FlatpakRow {
                id: f[0].into(),
                branch: f[1].into(),
                installation: f[2].into(),
                size: parse_si(f[3]),
                runtime: f.get(4).filter(|r| !r.is_empty()).map(|r| r.to_string()),
            })
        })
        .collect()
}

/// GLib-formatted sizes: SI units ("1.2 GB", "18.3 kB"), often with a no-break space.
pub fn parse_si(s: &str) -> Option<u64> {
    let s = s.replace('\u{a0}', " ");
    let s = s.trim();
    let idx = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let n: f64 = s[..idx].parse().ok()?;
    let mult = match s[idx..].trim() {
        "" | "B" | "bytes" | "byte" => 1e0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        _ => return super::parse_size(s),
    };
    Some((n * mult) as u64)
}

/// `org.gnome.Platform/x86_64/46` -> (`org.gnome.Platform`, `46`).
fn split_ref(r: &str) -> Option<(String, String)> {
    let p: Vec<&str> = r.split('/').collect();
    (p.len() == 3).then(|| (p[0].to_string(), p[2].to_string()))
}

/// `runtime=` from an app's `metadata` keyfile.
fn metadata_runtime(s: &str) -> Option<String> {
    s.lines().find_map(|l| l.trim().strip_prefix("runtime=")).map(|v| v.trim().to_string())
}

/// Whether some installed app needs this runtime, directly or as an extension.
/// Conservative: anything that looks like a driver/codec or a locale/debug
/// extension of something in use counts as used.
pub fn runtime_used(id: &str, branch: &str, used: &HashSet<(String, String)>, apps: &HashSet<String>) -> bool {
    const SHARED_EXT: [&str; 9] =
        [".GL.", ".GL32.", ".VAAPI.", ".ffmpeg", ".openh264", ".codecs", ".Codecs", ".VulkanLayer.", ".Compat."];
    used.contains(&(id.to_string(), branch.to_string()))
        || SHARED_EXT.iter().any(|x| id.contains(x))
        || used.iter().any(|(u, _)| id.strip_prefix(u.as_str()).is_some_and(|r| r.starts_with('.')))
        || apps.iter().any(|a| id.strip_prefix(a.as_str()).is_some_and(|r| r.starts_with('.')))
}

/// Directory children of `n` (follows nothing; symlinks such as `current` are skipped).
fn dirs(tree: &FileTree, n: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    tree.children(n).filter(move |&c| tree.node(c).kind == Kind::Dir)
}

/// File bytes in a subtree including hardlinks counted elsewhere.
fn full_size(tree: &FileTree, n: NodeId) -> u64 {
    let mut sum = 0;
    let mut stack = vec![n];
    while let Some(c) = stack.pop() {
        let node = tree.node(c);
        match node.kind {
            Kind::Dir => stack.extend(tree.children(c)),
            _ => sum += node.alloc,
        }
    }
    sum
}

struct Inst {
    label: String,
    path: String,
    /// Name in flatpak list's installation column, when rows can be trusted for it.
    list_name: Option<String>,
    system: bool,
    /// We may run flatpak against it (not another user's installation under sudo).
    actionable: bool,
}

fn flatpak_entity(kind: &str, name: String, paths: Vec<String>) -> Entity {
    Entity { kind: kind.into(), name, provider: "flatpak".into(), group: "Flatpak".into(), paths, ..Default::default() }
}

fn collect_flatpak(ctx: &Ctx, snap: &mut Snapshot, claimed: &mut Claimed, outcome: &mut Outcome) -> usize {
    let has = ctx.runner.has("flatpak");
    let run = |argv: &[&str]| ctx.runner.run(argv).filter(|o| o.ok()).map(|o| o.stdout);
    let mut system_paths: Vec<String> = if has {
        run(&["flatpak", "--installations"])
            .map(|s| s.lines().map(|l| l.trim().to_string()).filter(|l| l.starts_with('/')).collect())
            .unwrap_or_default()
    } else {
        vec![]
    };
    if system_paths.is_empty() {
        system_paths.push("/var/lib/flatpak".into());
    }
    let mut insts: Vec<Inst> = system_paths
        .iter()
        .enumerate()
        .map(|(i, p)| Inst {
            label: if i == 0 { "system".into() } else { p.clone() },
            path: p.trim_end_matches('/').to_string(),
            list_name: (i == 0).then(|| "system".to_string()),
            system: true,
            actionable: has,
        })
        .collect();
    for (user, home) in homes(ctx) {
        let own = !ctx.is_root;
        insts.push(Inst {
            label: format!("user{}", user_suffix(ctx, &user)),
            path: format!("{home}/.local/share/flatpak"),
            list_name: own.then(|| "user".to_string()),
            system: false,
            actionable: has && own,
        });
    }
    let tree_insts: Vec<(Inst, NodeId)> = insts
        .into_iter()
        .filter_map(|i| {
            let n = snap.lookup_static(&i.path)?;
            let t = &snap.tree;
            let populated = ["app", "runtime"].iter().any(|d| t.child_by_name(n, d.as_bytes()).is_some());
            (populated && !claimed.covers(t, n)).then_some((i, n))
        })
        .collect();
    if tree_insts.is_empty() {
        return 0;
    }

    let (apps_rows, rt_rows) = if has {
        let cols = "--columns=application,branch,installation,size";
        (
            run(&["flatpak", "list", "--app", &format!("{cols},runtime")]).map(|s| parse_flatpak_list(&s)),
            run(&["flatpak", "list", "--runtime", cols]).map(|s| parse_flatpak_list(&s)),
        )
    } else {
        (None, None)
    };
    if !has {
        outcome.degrade("flatpak not installed; using the installation directory layout");
    } else if apps_rows.is_none() || rt_rows.is_none() {
        outcome.degrade("`flatpak list` failed; sizes are from the scan only");
    }
    let apps_rows = apps_rows.unwrap_or_default();
    let rt_rows = rt_rows.unwrap_or_default();
    let row = |rows: &[FlatpakRow], inst: &Inst, id: &str, branch: Option<&str>| -> Option<FlatpakRow> {
        let ln = inst.list_name.as_deref()?;
        rows.iter().find(|r| r.installation == ln && r.id == id && branch.is_none_or(|b| r.branch == b)).cloned()
    };

    // Pass 1: what's installed, and which runtimes apps need (across installations).
    struct App {
        inst: usize,
        id: String,
        node: NodeId,
        branches: Vec<String>,
        runtime: Option<String>,
        row: Option<FlatpakRow>,
    }
    struct Rt {
        inst: usize,
        id: String,
        arch: String,
        branch: String,
        node: NodeId,
        row: Option<FlatpakRow>,
    }
    let tree = &snap.tree;
    let mut apps: Vec<App> = Vec::new();
    let mut rts: Vec<Rt> = Vec::new();
    for (ii, (inst, n)) in tree_insts.iter().enumerate() {
        for a in tree.child_by_name(*n, b"app").into_iter().flat_map(|d| dirs(tree, d)) {
            let id = tree.name(a).into_owned();
            let branches: Vec<String> =
                dirs(tree, a).flat_map(|arch| dirs(tree, arch)).map(|b| tree.name(b).into_owned()).collect();
            let row = row(&apps_rows, inst, &id, None);
            let runtime = row.as_ref().and_then(|r| r.runtime.clone()).or_else(|| {
                let meta = format!("{}/app/{id}/current/active/metadata", inst.path);
                ctx.runner.read(&meta).and_then(|s| metadata_runtime(&s))
            });
            apps.push(App { inst: ii, id, node: a, branches, runtime, row });
        }
        for r in tree.child_by_name(*n, b"runtime").into_iter().flat_map(|d| dirs(tree, d)) {
            let id = tree.name(r).into_owned();
            for arch in dirs(tree, r) {
                for b in dirs(tree, arch) {
                    let branch = tree.name(b).into_owned();
                    let row = row(&rt_rows, inst, &id, Some(&branch));
                    rts.push(Rt { inst: ii, id: id.clone(), arch: tree.name(arch).into_owned(), branch, node: b, row });
                }
            }
        }
    }
    let used: HashSet<(String, String)> =
        apps.iter().filter_map(|a| a.runtime.as_deref().and_then(split_ref)).collect();
    let app_ids: HashSet<String> = apps.iter().map(|a| a.id.clone()).collect();
    let unknown_runtimes = apps.iter().any(|a| a.runtime.is_none());
    if unknown_runtimes {
        outcome.degrade("could not read every app's runtime; unused runtimes not flagged");
    }

    // Pass 2: entities.
    let mut out: Vec<(usize, Entity)> = Vec::new();
    let mut local = claimed.clone();
    for a in &apps {
        let mut e = flatpak_entity("flatpak.app", a.id.clone(), vec![tree.path(a.node)]);
        e.reported = a.row.as_ref().and_then(|r| r.size).or_else(|| Some(full_size(tree, a.node)));
        e.attrs.push(("branch".into(), a.branches.join(", ")));
        if let Some(r) = &a.runtime {
            e.attrs.push(("runtime".into(), r.clone()));
        }
        local.add(tree, a.node);
        out.push((a.inst, e));
    }
    for r in &rts {
        let (inst, _) = &tree_insts[r.inst];
        let mut e = flatpak_entity("flatpak.runtime", format!("{}//{}", r.id, r.branch), vec![tree.path(r.node)]);
        let reported = r.row.as_ref().and_then(|x| x.size);
        e.reported = reported.or_else(|| Some(full_size(tree, r.node)));
        e.attrs.push(("arch".into(), r.arch.clone()));
        if !unknown_runtimes && !runtime_used(&r.id, &r.branch, &used, &app_ids) {
            let flag = if inst.system { "--system" } else { "--user" };
            e.attrs.push(("status".into(), "not used by any installed app".into()));
            e.reclaim = Some(Reclaim {
                risk: Risk::Review,
                reason: "runtime not needed by any installed app (flatpak keeps it if it was installed explicitly)"
                    .into(),
                estimate: e.reported,
                action: inst.actionable.then(|| ActionSpec {
                    label: "flatpak uninstall --unused".into(),
                    steps: vec![ActionStep::Command {
                        argv: ["flatpak", "uninstall", "--unused", "-y", flag].map(String::from).to_vec(),
                        root: inst.system,
                    }],
                }),
            });
        }
        local.add(tree, r.node);
        out.push((r.inst, e));
    }
    for (ii, (inst, n)) in tree_insts.iter().enumerate() {
        let rest = local.subtract(tree, *n);
        if alloc_of(tree, &rest) > 0 {
            let mut e = flatpak_entity(
                "flatpak.repo",
                "OSTree repository & metadata".into(),
                rest.iter().map(|&x| tree.path(x)).collect(),
            );
            e.attrs.push((
                "note".into(),
                format!("deployed files are hardlinks into {}/repo; each file is counted once", inst.path),
            ));
            out.push((ii, e));
        }
    }

    let mut added = 0;
    let parents: Vec<u32> = tree_insts
        .iter()
        .map(|(inst, n)| {
            let mut p = flatpak_entity("flatpak.installation", format!("Flatpak {} installation", inst.label), vec![]);
            p.attrs.push(("path".into(), inst.path.clone()));
            claimed.add(&snap.tree, *n);
            p
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|p| snap.add_entity(p))
        .collect();
    for (ii, mut e) in out {
        e.parent = Some(parents[ii]);
        snap.add_entity(e);
        added += 1;
    }
    added + parents.len()
}

#[derive(Debug, Clone, PartialEq)]
pub struct SnapRow {
    pub name: String,
    pub version: String,
    pub rev: String,
    pub disabled: bool,
}

/// `snap list --all`: Name Version Rev Tracking Publisher Notes.
pub fn parse_snap_list(out: &str) -> Vec<SnapRow> {
    out.lines()
        .skip_while(|l| !l.starts_with("Name"))
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 3).then(|| SnapRow {
                name: f[0].into(),
                version: f[1].into(),
                rev: f[2].into(),
                disabled: f.last().is_some_and(|n| n.split(',').any(|x| x == "disabled")),
            })
        })
        .collect()
}

fn collect_snap(ctx: &Ctx, snap: &mut Snapshot, claimed: &mut Claimed, outcome: &mut Outcome) -> usize {
    let tree = &snap.tree;
    let Some(dir) = snap.lookup_static(SNAPS_DIR).filter(|&n| !claimed.covers(tree, n)) else { return 0 };
    // (name, rev, node)
    let mut files: Vec<(String, String, NodeId)> = tree
        .children(dir)
        .filter(|&c| tree.node(c).kind == Kind::File)
        .filter_map(|c| {
            let name = tree.name(c);
            let (snap_name, rev) = name.strip_suffix(".snap")?.rsplit_once('_')?;
            Some((snap_name.to_string(), rev.to_string(), c))
        })
        .collect();
    if files.is_empty() {
        return 0;
    }
    files.sort();
    let rows = match ctx.runner.run(&["snap", "list", "--all"]) {
        Some(o) if o.ok() => parse_snap_list(&o.stdout),
        _ => {
            outcome.degrade("`snap list --all` unavailable; disabled revisions not identified");
            vec![]
        }
    };
    let mut by_name: HashMap<String, Vec<Entity>> = HashMap::new();
    for (name, rev, node) in &files {
        let row = rows.iter().find(|r| &r.name == name && &r.rev == rev);
        let mut e = Entity {
            kind: "snap.revision".into(),
            name: format!("{name} rev {rev}"),
            provider: "snap".into(),
            group: "Snap packages".into(),
            paths: vec![tree.path(*node)],
            ..Default::default()
        };
        if let Some(r) = row {
            e.attrs.push(("version".into(), r.version.clone()));
            e.attrs.push(("status".into(), if r.disabled { "disabled" } else { "active" }.into()));
            if r.disabled {
                e.reclaim = Some(Reclaim {
                    risk: Risk::Safe,
                    reason: "disabled snap revision kept for rollback".into(),
                    estimate: None,
                    action: Some(ActionSpec {
                        label: format!("snap remove {name} --revision={rev}"),
                        steps: vec![ActionStep::Command {
                            argv: vec!["snap".into(), "remove".into(), name.clone(), format!("--revision={rev}")],
                            root: true,
                        }],
                    }),
                });
            }
        }
        by_name.entry(name.clone()).or_default().push(e);
    }
    claimed.add(tree, dir);
    let mut names: Vec<String> = by_name.keys().cloned().collect();
    names.sort();
    let mut added = 0;
    for name in names {
        let data = format!("/var/snap/{name}");
        let data_node = snap.lookup_static(&data).filter(|&n| !claimed.covers(&snap.tree, n));
        let parent = snap.add_entity(Entity {
            kind: "snap".into(),
            name: name.clone(),
            provider: "snap".into(),
            group: "Snap packages".into(),
            ..Default::default()
        });
        added += 1;
        for mut e in by_name.remove(&name).unwrap_or_default() {
            e.parent = Some(parent);
            snap.add_entity(e);
            added += 1;
        }
        if let Some(n) = data_node {
            claimed.add(&snap.tree, n);
            snap.add_entity(Entity {
                kind: "snap.data".into(),
                name: format!("{name} system data"),
                provider: "snap".into(),
                group: "Snap packages".into(),
                parent: Some(parent),
                paths: vec![data],
                ..Default::default()
            });
            added += 1;
        }
    }
    added
}

impl Provider for FlatpakSnap {
    fn name(&self) -> &'static str {
        "flatpak-snap"
    }

    fn collect(&self, ctx: &Ctx, snap: &mut Snapshot) -> Outcome {
        let mut outcome = Outcome::complete();
        let mut claimed = Claimed::from_entities(snap);
        let n = collect_flatpak(ctx, snap, &mut claimed, &mut outcome)
            + collect_snap(ctx, snap, &mut claimed, &mut outcome);
        if n == 0 && outcome.notes.is_empty() {
            return Outcome::absent();
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::attribution::attribute;
    use crate::providers::classifier::testing::{ctx, find, snap_from};
    use crate::providers::runner::FakeRunner;

    const MB: u64 = 1 << 20;
    const APP_COLS: &str = "flatpak list --app --columns=application,branch,installation,size,runtime";
    const RT_COLS: &str = "flatpak list --runtime --columns=application,branch,installation,size";

    fn tree() -> Snapshot {
        snap_from(&[
            ("/var/lib/flatpak/app/org.mozilla.firefox/x86_64/stable/abc/files/lib", 100 * MB),
            ("/var/lib/flatpak/app/com.spotify.Client/x86_64/stable/def/files/bin", 90 * MB),
            ("/var/lib/flatpak/runtime/org.freedesktop.Platform/x86_64/24.08/a/files/x", 300 * MB),
            ("/var/lib/flatpak/runtime/org.freedesktop.Platform/x86_64/23.08/b/files/x", 280 * MB),
            ("/var/lib/flatpak/runtime/org.freedesktop.Platform.GL.default/x86_64/24.08/c/files/x", 200 * MB),
            ("/var/lib/flatpak/runtime/org.freedesktop.Platform.Locale/x86_64/24.08/d/files/x", MB),
            ("/var/lib/flatpak/runtime/org.mozilla.firefox.Locale/x86_64/stable/e/files/x", 7 * MB),
            ("/var/lib/flatpak/runtime/org.gnome.Platform/x86_64/46/f/files/x", 500 * MB),
            ("/var/lib/flatpak/runtime/org.kde.Platform/x86_64/6.7/g/files/x", 600 * MB),
            ("/var/lib/flatpak/repo/objects/aa/bb.file", 400 * MB),
            ("/var/lib/flatpak/repo/config", 1000),
            ("/home/u/.local/share/flatpak/app/org.gnome.Calculator/x86_64/stable/h/files/x", 3 * MB),
            ("/home/u/.local/share/flatpak/runtime/org.gnome.Platform/x86_64/47/i/files/x", 520 * MB),
            ("/home/u/.local/share/flatpak/repo/objects/x", 10 * MB),
            ("/var/lib/snapd/snaps/core22_1122.snap", 70 * MB),
            ("/var/lib/snapd/snaps/core22_1380.snap", 74 * MB),
            ("/var/lib/snapd/snaps/firefox_4136.snap", 250 * MB),
            ("/var/lib/snapd/snaps/firefox_4173.snap", 255 * MB),
            ("/var/lib/snapd/snaps/snapd_21465.snap", 40 * MB),
            ("/var/snap/firefox/common/x", 2 * MB),
        ])
    }

    fn runner() -> FakeRunner {
        FakeRunner::default()
            .with("flatpak --installations", include_str!("../../tests/fixtures/flatpak/installations.txt"))
            .with(APP_COLS, include_str!("../../tests/fixtures/flatpak/list-app.txt"))
            .with(RT_COLS, include_str!("../../tests/fixtures/flatpak/list-runtime.txt"))
            .with("snap list --all", include_str!("../../tests/fixtures/snap/list-all.txt"))
    }

    #[test]
    fn parses_lists() {
        let rows = parse_flatpak_list(include_str!("../../tests/fixtures/flatpak/list-app.txt"));
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].size, Some(312_400_000));
        assert_eq!(rows[0].runtime.as_deref(), Some("org.freedesktop.Platform/x86_64/24.08"));
        assert_eq!(parse_si("18.3\u{a0}kB"), Some(18_300));
        assert_eq!(parse_si("1.1 GB"), Some(1_100_000_000));
        let snaps = parse_snap_list(include_str!("../../tests/fixtures/snap/list-all.txt"));
        assert_eq!(snaps.len(), 5);
        assert!(snaps[0].disabled && !snaps[1].disabled && snaps[2].disabled && !snaps[3].disabled);
        assert_eq!(snaps[4].rev, "21465");
    }

    #[test]
    fn flatpak_and_snap_entities() {
        let mut snap = tree();
        let r = runner();
        let out = FlatpakSnap.collect(&ctx(&r, &[("u", "/home/u")]), &mut snap);
        assert_eq!(out.coverage, crate::model::Coverage::Complete, "{:?}", out.notes);
        attribute(&mut snap);

        let sys = find(&snap, "Flatpak system installation");
        assert_eq!(sys.measured_alloc, snap.tree.node(snap.lookup("/var/lib/flatpak").unwrap()).alloc);
        let user = find(&snap, "Flatpak user installation");
        assert_eq!(user.measured_alloc, 533 * MB);

        let ff = find(&snap, "org.mozilla.firefox");
        assert_eq!(ff.reported, Some(312_400_000));
        assert_eq!(ff.parent, Some(sys.id));

        let unused: Vec<&str> = snap
            .entities
            .iter()
            .filter(|e| e.kind == "flatpak.runtime" && e.reclaim.is_some())
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(unused, vec!["org.freedesktop.Platform//23.08", "org.gnome.Platform//46", "org.kde.Platform//6.7"]);
        let kde = find(&snap, "org.kde.Platform//6.7").reclaim.clone().unwrap();
        assert_eq!(kde.estimate, Some(1_200_000_000));
        assert_eq!(
            kde.action.unwrap().steps,
            vec![ActionStep::Command {
                argv: ["flatpak", "uninstall", "--unused", "-y", "--system"].map(String::from).to_vec(),
                root: true
            }]
        );
        let repo: Vec<&Entity> = snap.entities.iter().filter(|e| e.kind == "flatpak.repo").collect();
        assert_eq!(repo.len(), 2);
        assert_eq!(repo[0].measured_alloc, 400 * MB + 1000);

        // Snap: disabled revisions are safe to remove.
        let old = find(&snap, "firefox rev 4136").reclaim.clone().unwrap();
        assert_eq!(old.risk, Risk::Safe);
        assert_eq!(
            old.action.unwrap().steps,
            vec![ActionStep::Command {
                argv: ["snap", "remove", "firefox", "--revision=4136"].map(String::from).to_vec(),
                root: true
            }]
        );
        assert!(find(&snap, "firefox rev 4173").reclaim.is_none());
        let fx = find(&snap, "firefox");
        assert_eq!(fx.measured_alloc, 507 * MB);
        assert!(find(&snap, "core22 rev 1122").reclaim.is_some());
    }

    #[test]
    fn path_based_without_tools() {
        let mut snap = tree();
        let r = FakeRunner::default().file(
            "/var/lib/flatpak/app/org.mozilla.firefox/current/active/metadata",
            "[Application]\nname=org.mozilla.firefox\nruntime=org.freedesktop.Platform/x86_64/24.08\n",
        );
        let out = FlatpakSnap.collect(&ctx(&r, &[("u", "/home/u")]), &mut snap);
        assert_eq!(out.coverage, crate::model::Coverage::Partial);
        attribute(&mut snap);
        // Spotify's runtime is unknown, so nothing is flagged as unused.
        assert!(snap.entities.iter().all(|e| e.kind != "flatpak.runtime" || e.reclaim.is_none()));
        assert_eq!(find(&snap, "org.mozilla.firefox").reported, Some(100 * MB));
        assert!(find(&snap, "firefox rev 4136").reclaim.is_none());
    }

    #[test]
    fn runtime_usage_rules() {
        let used: HashSet<(String, String)> = [("org.gnome.Platform".to_string(), "47".to_string())].into();
        let apps: HashSet<String> = ["org.gnome.Calculator".to_string()].into();
        assert!(runtime_used("org.gnome.Platform", "47", &used, &apps));
        assert!(!runtime_used("org.gnome.Platform", "46", &used, &apps));
        assert!(runtime_used("org.gnome.Platform.Locale", "47", &used, &apps));
        assert!(runtime_used("org.gnome.Calculator.Locale", "stable", &used, &apps));
        assert!(runtime_used("org.freedesktop.Platform.GL.nvidia-550-78", "1.4", &used, &apps));
        assert!(!runtime_used("org.gnome.PlatformX", "47", &used, &apps));
    }
}
