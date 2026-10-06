//! Other VM managers: VirtualBox (`VBoxManage` when installed, else the
//! `~/VirtualBox VMs` layout), Vagrant boxes, GNOME Boxes images not already
//! claimed by libvirt, Multipass instances, and Incus/LXD storage pools.
//! Everything is path-based except VirtualBox's disk registry.

use super::classifier::{Claimed, alloc_of, homes, user_suffix};
use super::libvirt::ORPHAN_GROUP;
use super::{Ctx, Outcome, Provider};
use crate::model::tree::Kind;
use crate::model::{ActionSpec, ActionStep, Entity, FileTree, NodeId, Reclaim, Risk, Snapshot};
use std::cmp::Ordering;
use std::collections::HashMap;

pub struct VboxVagrant;

const INCUS_ROOTS: [(&str, &str); 3] =
    [("/var/lib/incus", "Incus"), ("/var/lib/lxd", "LXD"), ("/var/snap/lxd/common/lxd", "LXD")];
/// Storage-pool subdirectories holding one item per entry.
const INCUS_KINDS: [(&str, &str); 6] = [
    ("containers", "incus.container"),
    ("virtual-machines", "incus.vm"),
    ("custom", "incus.volume"),
    ("images", "incus.image"),
    ("containers-snapshots", "incus.snapshot"),
    ("virtual-machines-snapshots", "incus.snapshot"),
];
const MULTIPASS_INSTANCES: &str = "/var/snap/multipass/common/data/multipassd/vault/instances";
const MULTIPASS_IMAGES: &str = "/var/snap/multipass/common/cache/multipassd/vault/images";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hdd {
    pub location: String,
    pub format: String,
    pub capacity: Option<u64>,
    /// Names of the VMs the disk is attached to.
    pub in_use: Vec<String>,
}

/// `VBoxManage list hdds`: blank-line separated `Key: value` blocks.
pub fn parse_hdds(out: &str) -> Vec<Hdd> {
    let mut v: Vec<Hdd> = Vec::new();
    let mut cur: Option<Hdd> = None;
    for l in out.lines() {
        let Some((k, val)) = l.split_once(':') else {
            if l.trim().is_empty() {
                v.extend(cur.take());
            }
            continue;
        };
        let val = val.trim();
        let h = cur.get_or_insert_with(Hdd::default);
        match k.trim() {
            "Location" => h.location = val.into(),
            "Storage format" => h.format = val.into(),
            "Capacity" => {
                let mb: Option<u64> = val.split_whitespace().next().and_then(|n| n.parse().ok());
                h.capacity = mb.map(|m| m << 20);
            }
            "In use by VMs" => {
                h.in_use = val
                    .split(") ")
                    .filter_map(|s| s.split(" (UUID").next())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            }
            _ => {}
        }
    }
    v.extend(cur);
    v.retain(|h| !h.location.is_empty());
    v
}

/// `VBoxManage list vms`: `"name" {uuid}`.
pub fn parse_vms(out: &str) -> Vec<String> {
    out.lines().filter_map(|l| l.trim().strip_prefix('"')?.rsplit_once("\" {").map(|(n, _)| n.to_string())).collect()
}

/// Compare version strings numerically by component ("1.10" > "1.9").
pub fn version_cmp(a: &str, b: &str) -> Ordering {
    let parts =
        |s: &str| -> Vec<u64> { s.split(|c: char| !c.is_ascii_digit()).filter_map(|p| p.parse().ok()).collect() };
    parts(a).cmp(&parts(b)).then_with(|| a.cmp(b))
}

fn dirs(tree: &FileTree, n: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    tree.children(n).filter(move |&c| tree.node(c).kind == Kind::Dir)
}

fn entity(provider: &str, group: &str, kind: &str, name: String, paths: Vec<String>) -> Entity {
    Entity { kind: kind.into(), name, provider: provider.into(), group: group.into(), paths, ..Default::default() }
}

fn trash_action(path: &str, label: String) -> ActionSpec {
    ActionSpec { label, steps: vec![ActionStep::DeletePath { path: path.into(), trash: true }] }
}

fn virtualbox(ctx: &Ctx, snap: &mut Snapshot, claimed: &mut Claimed) -> usize {
    let homes = homes(ctx);
    let mut added = 0;
    // VM name -> parent entity id.
    let mut vms: HashMap<String, u32> = HashMap::new();
    for (user, home) in &homes {
        let base = format!("{home}/VirtualBox VMs");
        let Some(n) = snap.lookup_static(&base).filter(|&n| !claimed.covers(&snap.tree, n)) else { continue };
        let folders: Vec<(String, NodeId)> = dirs(&snap.tree, n).map(|d| (snap.tree.name(d).into_owned(), d)).collect();
        for (name, d) in folders {
            let parent = snap.add_entity(entity(
                "virtualbox",
                "VirtualBox",
                "vm",
                format!("{name}{}", user_suffix(ctx, user)),
                vec![],
            ));
            let path = snap.tree.path(d);
            let mut f = entity("virtualbox", "VirtualBox", "vm.folder", format!("{name} folder"), vec![path]);
            f.parent = Some(parent);
            snap.add_entity(f);
            claimed.add(&snap.tree, d);
            vms.insert(name, parent);
            added += 2;
        }
    }
    // The disk registry knows about disks outside the VM folders and unattached ones.
    if ctx.is_root || !ctx.runner.has("VBoxManage") {
        return added;
    }
    let run = |args: &[&str]| ctx.runner.run(args).filter(|o| o.ok()).map(|o| o.stdout).unwrap_or_default();
    let registered = parse_vms(&run(&["VBoxManage", "list", "vms"]));
    let hdds = parse_hdds(&run(&["VBoxManage", "list", "hdds"]));
    for name in registered {
        if let std::collections::hash_map::Entry::Vacant(v) = vms.entry(name) {
            let name = v.key().clone();
            v.insert(snap.add_entity(entity("virtualbox", "VirtualBox", "vm", name, vec![])));
            added += 1;
        }
    }
    for h in hdds {
        let node = snap.lookup_static(&h.location);
        // Already reported as an unused image by the libvirt provider?
        let orphan = snap.entities.iter().position(|e| e.group == ORPHAN_GROUP && e.paths == [h.location.clone()]);
        let base = h.location.rsplit('/').next().unwrap_or(&h.location).to_string();
        match h.in_use.first() {
            Some(vm) => {
                if let Some(i) = orphan {
                    let e = &mut snap.entities[i];
                    e.group = "VirtualBox".into();
                    e.kind = "vm.disk".into();
                    e.provider = "virtualbox".into();
                    e.name = format!("{vm}: {base}");
                    e.reclaim = None;
                    e.parent = vms.get(vm).copied();
                    e.virtual_size = h.capacity;
                    continue;
                }
                if node.is_some_and(|n| claimed.covers(&snap.tree, n)) {
                    continue;
                }
                let parent = match vms.get(vm) {
                    Some(&p) => p,
                    None => {
                        let p = snap.add_entity(entity("virtualbox", "VirtualBox", "vm", vm.clone(), vec![]));
                        vms.insert(vm.clone(), p);
                        added += 1;
                        p
                    }
                };
                let mut e =
                    entity("virtualbox", "VirtualBox", "vm.disk", format!("{vm}: {base}"), vec![h.location.clone()]);
                e.parent = Some(parent);
                e.virtual_size = h.capacity;
                e.attrs.push(("format".into(), h.format.clone()));
                snap.add_entity(e);
                added += 1;
            }
            None => {
                let reason = "VirtualBox disk not attached to any VM";
                if let Some(i) = orphan {
                    if let Some(r) = snap.entities[i].reclaim.as_mut() {
                        r.reason = reason.into();
                    }
                    continue;
                }
                // Inside a VM folder: still worth flagging, but don't double count.
                let mut e = entity("virtualbox", ORPHAN_GROUP, "vm.image", base.clone(), vec![h.location.clone()]);
                e.virtual_size = h.capacity;
                e.reclaim = Some(Reclaim {
                    risk: Risk::Review,
                    reason: reason.into(),
                    estimate: node.map(|n| snap.tree.node(n).alloc),
                    action: Some(ActionSpec {
                        label: format!("VBoxManage closemedium disk {base} --delete"),
                        steps: vec![ActionStep::Command {
                            argv: ["VBoxManage", "closemedium", "disk", &h.location, "--delete"]
                                .map(String::from)
                                .to_vec(),
                            root: false,
                        }],
                    }),
                });
                if node.is_some_and(|n| claimed.covers(&snap.tree, n)) {
                    e.paths.clear();
                    e.attrs.push(("path".into(), h.location.clone()));
                    e.attrs.push(("note".into(), "inside a VM folder (counted there)".into()));
                }
                snap.add_entity(e);
                added += 1;
            }
        }
    }
    added
}

/// (version, directory, providers).
type BoxVersion = (String, NodeId, String);

fn vagrant(ctx: &Ctx, snap: &mut Snapshot, claimed: &mut Claimed) -> usize {
    let mut added = 0;
    let can_cli = !ctx.is_root && ctx.runner.has("vagrant");
    for (user, home) in homes(ctx) {
        let base = format!("{home}/.vagrant.d/boxes");
        let Some(n) = snap.lookup_static(&base).filter(|&n| !claimed.covers(&snap.tree, n)) else { continue };
        let boxes: Vec<(String, Vec<BoxVersion>)> = dirs(&snap.tree, n)
            .map(|b| {
                let t = &snap.tree;
                let name = t.name(b).replace("-VAGRANTSLASH-", "/");
                let mut versions: Vec<BoxVersion> = dirs(t, b)
                    .map(|v| {
                        let providers: Vec<String> = dirs(t, v).map(|p| t.name(p).into_owned()).collect();
                        (t.name(v).into_owned(), v, providers.join(", "))
                    })
                    .collect();
                versions.sort_by(|a, b| version_cmp(&a.0, &b.0));
                (name, versions)
            })
            .collect();
        for (name, versions) in boxes {
            if versions.is_empty() {
                continue;
            }
            let parent = snap.add_entity(entity(
                "vagrant",
                "Vagrant boxes",
                "vagrant.box",
                format!("{name}{}", user_suffix(ctx, &user)),
                vec![],
            ));
            added += 1;
            let newest = versions.len() - 1;
            for (i, (ver, node, providers)) in versions.into_iter().enumerate() {
                let path = snap.tree.path(node);
                let mut e = entity(
                    "vagrant",
                    "Vagrant boxes",
                    "vagrant.box.version",
                    format!("{name} {ver}"),
                    vec![path.clone()],
                );
                e.parent = Some(parent);
                e.attrs.push(("provider".into(), providers));
                if i < newest {
                    let action = if can_cli {
                        ActionSpec {
                            label: format!("vagrant box remove {name} --box-version {ver}"),
                            steps: vec![ActionStep::Command {
                                argv: vec![
                                    "vagrant".into(),
                                    "box".into(),
                                    "remove".into(),
                                    name.clone(),
                                    "--box-version".into(),
                                    ver.clone(),
                                ],
                                root: false,
                            }],
                        }
                    } else {
                        trash_action(&path, format!("move {name} {ver} to trash"))
                    };
                    e.reclaim = Some(Reclaim {
                        risk: Risk::Review,
                        reason: "older box version; new machines use the newest unless a Vagrantfile pins this one"
                            .into(),
                        estimate: None,
                        action: Some(action),
                    });
                }
                claimed.add(&snap.tree, node);
                snap.add_entity(e);
                added += 1;
            }
        }
    }
    added
}

/// One entity per child (`dirs_only`) of `dir`, unless already claimed.
fn per_child(
    snap: &mut Snapshot,
    claimed: &mut Claimed,
    dir: &str,
    dirs_only: bool,
    make: &dyn Fn(&str, String) -> Entity,
) -> usize {
    let Some(n) = snap.lookup_static(dir) else { return 0 };
    let kids: Vec<NodeId> = snap
        .tree
        .children(n)
        .filter(|&c| !dirs_only || snap.tree.node(c).kind == Kind::Dir)
        .filter(|&c| !claimed.covers(&snap.tree, c))
        .collect();
    for &c in &kids {
        let e = make(&snap.tree.name(c), snap.tree.path(c));
        claimed.add(&snap.tree, c);
        snap.add_entity(e);
    }
    kids.len()
}

fn others(ctx: &Ctx, snap: &mut Snapshot, claimed: &mut Claimed) -> usize {
    let mut added = 0;
    for (user, home) in homes(ctx) {
        let suffix = user_suffix(ctx, &user);
        added += per_child(snap, claimed, &format!("{home}/.local/share/gnome-boxes/images"), false, &|name, p| {
            entity("gnome-boxes", "GNOME Boxes", "vm.disk", format!("{name}{suffix}"), vec![p])
        });
    }
    added += per_child(snap, claimed, MULTIPASS_INSTANCES, true, &|name, p| {
        entity("multipass", "Multipass", "vm", name.to_string(), vec![p])
    });
    if let Some(n) = snap.lookup_static(MULTIPASS_IMAGES).filter(|&n| !claimed.covers(&snap.tree, n)) {
        let mut e =
            entity("multipass", "Multipass", "cache", "Multipass image cache".into(), vec![MULTIPASS_IMAGES.into()]);
        e.reclaim = Some(Reclaim {
            risk: Risk::Review,
            reason: "downloaded base images; re-downloaded when launching new instances".into(),
            estimate: None,
            action: None,
        });
        claimed.add(&snap.tree, n);
        snap.add_entity(e);
        added += 1;
    }
    for (root, label) in INCUS_ROOTS {
        let Some(rn) = snap.lookup_static(root).filter(|&n| !claimed.covers(&snap.tree, n)) else { continue };
        let pools: Vec<(String, String)> = snap
            .lookup_static(&format!("{root}/storage-pools"))
            .map(|p| dirs(&snap.tree, p).map(|d| (snap.tree.name(d).into_owned(), snap.tree.path(d))).collect())
            .unwrap_or_default();
        for (pool, ppath) in pools {
            for (kind, ek) in INCUS_KINDS {
                added += per_child(snap, claimed, &format!("{ppath}/{kind}"), true, &|name, p| {
                    let shown: String = if kind == "images" { name.chars().take(12).collect() } else { name.into() };
                    entity(&label.to_ascii_lowercase(), label, ek, format!("{pool}/{kind}/{shown}"), vec![p])
                });
            }
        }
        added += per_child(snap, claimed, &format!("{root}/disks"), false, &|name, p| {
            let mut e =
                entity(&label.to_ascii_lowercase(), label, "incus.pool", format!("storage pool file {name}"), vec![p]);
            e.attrs.push(("note".into(), "loop-file backed storage pool".into()));
            e
        });
        let rest = claimed.subtract(&snap.tree, rn);
        if alloc_of(&snap.tree, &rest) > 0 {
            let paths = rest.iter().map(|&n| snap.tree.path(n)).collect();
            snap.add_entity(entity(
                &label.to_ascii_lowercase(),
                label,
                "incus.other",
                format!("{label} other data"),
                paths,
            ));
            added += 1;
        }
        claimed.add(&snap.tree, rn);
    }
    added
}

impl Provider for VboxVagrant {
    fn name(&self) -> &'static str {
        "vbox-vagrant"
    }

    fn collect(&self, ctx: &Ctx, snap: &mut Snapshot) -> Outcome {
        let mut claimed = Claimed::from_entities(snap);
        let n =
            virtualbox(ctx, snap, &mut claimed) + vagrant(ctx, snap, &mut claimed) + others(ctx, snap, &mut claimed);
        if n == 0 {
            return Outcome::absent();
        }
        let mut out = Outcome::complete();
        if ctx.is_root && ctx.runner.has("VBoxManage") {
            out = out.note("VirtualBox disk registry is per user; only VM folders inspected under sudo");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::attribution::attribute;
    use crate::providers::classifier::testing::{ctx, find, snap_from};
    use crate::providers::runner::FakeRunner;

    const MB: u64 = 1 << 20;

    const HDDS: &str = "UUID:           6a3d6c2f-1111-4c3e-9c47-2d1b2f0c9a01
Parent UUID:    base
State:          created
Type:           normal (base)
Location:       /home/u/VirtualBox VMs/win10/win10.vdi
Storage format: VDI
Capacity:       51200 MBytes
Encryption:     disabled
In use by VMs:  win10 (UUID: 0f6c1c55-7a33-4c3e-9c47-2d1b2f0c9a01)

UUID:           6a3d6c2f-2222-4c3e-9c47-2d1b2f0c9a02
Parent UUID:    base
State:          inaccessible
Type:           normal (base)
Location:       /home/u/vms/data.vdi
Storage format: VDI
Capacity:       20480 MBytes
Encryption:     disabled
In use by VMs:  win10 (UUID: 0f6c1c55-7a33-4c3e-9c47-2d1b2f0c9a01)

UUID:           6a3d6c2f-3333-4c3e-9c47-2d1b2f0c9a03
Parent UUID:    base
State:          created
Type:           normal (base)
Location:       /home/u/vms/old.vdi
Storage format: VDI
Capacity:       10240 MBytes
Encryption:     disabled
";

    #[test]
    fn parses_vbox_output() {
        let h = parse_hdds(HDDS);
        assert_eq!(h.len(), 3);
        assert_eq!(h[0].in_use, vec!["win10".to_string()]);
        assert_eq!(h[0].capacity, Some(51200 << 20));
        assert!(h[2].in_use.is_empty());
        assert_eq!(parse_vms("\"win10\" {0f6c1c55-7a33}\n\"my vm\" {abc}\n"), vec!["win10", "my vm"]);
        assert_eq!(version_cmp("1.10.0", "1.9.2"), Ordering::Greater);
        assert_eq!(version_cmp("20240101.0.0", "20231201.0.0"), Ordering::Greater);
    }

    fn tree() -> Snapshot {
        snap_from(&[
            ("/home/u/VirtualBox VMs/win10/win10.vdi", 30_000 * MB),
            ("/home/u/VirtualBox VMs/win10/Logs/VBox.log", MB),
            ("/home/u/vms/data.vdi", 5000 * MB),
            ("/home/u/vms/old.vdi", 2000 * MB),
            ("/home/u/.vagrant.d/boxes/generic-VAGRANTSLASH-debian12/4.3.10/libvirt/box.img", 900 * MB),
            ("/home/u/.vagrant.d/boxes/generic-VAGRANTSLASH-debian12/4.3.2/libvirt/box.img", 800 * MB),
            ("/home/u/.vagrant.d/boxes/generic-VAGRANTSLASH-debian12/metadata_url", 100),
            ("/home/u/.local/share/gnome-boxes/images/fedora", 7000 * MB),
            ("/var/snap/multipass/common/data/multipassd/vault/instances/primary/disk.img", 3000 * MB),
            ("/var/snap/multipass/common/cache/multipassd/vault/images/noble/img", 600 * MB),
            ("/var/lib/incus/storage-pools/default/containers/web/rootfs/x", 400 * MB),
            ("/var/lib/incus/storage-pools/default/images/0123456789abcdef0123/rootfs/x", 200 * MB),
            ("/var/lib/incus/disks/default.img", 1000 * MB),
            ("/var/lib/incus/database/global/db.bin", 10 * MB),
        ])
    }

    #[test]
    fn path_based_and_registry() {
        let mut snap = tree();
        // As libvirt would have flagged them before us.
        for p in ["/home/u/vms/data.vdi", "/home/u/vms/old.vdi"] {
            snap.add_entity(Entity {
                name: p.rsplit('/').next().unwrap().into(),
                group: ORPHAN_GROUP.into(),
                kind: "vm.image".into(),
                paths: vec![p.into()],
                reclaim: Some(Reclaim { risk: Risk::Review, reason: "x".into(), estimate: None, action: None }),
                ..Default::default()
            });
        }
        let runner = FakeRunner::default()
            .with("VBoxManage list vms", "\"win10\" {0f6c1c55-7a33-4c3e-9c47-2d1b2f0c9a01}\n")
            .with("VBoxManage list hdds", HDDS)
            .with("vagrant --version", "Vagrant 2.4.1");
        let out = VboxVagrant.collect(&ctx(&runner, &[("u", "/home/u")]), &mut snap);
        assert_eq!(out.coverage, crate::model::Coverage::Complete);
        attribute(&mut snap);

        let win = snap.entities.iter().find(|e| e.name == "win10" && e.kind == "vm").unwrap();
        // Folder + the attached disk that lives outside it.
        assert_eq!(win.measured_alloc, 30_001 * MB + 5000 * MB);
        let data = find(&snap, "win10: data.vdi");
        assert!(data.reclaim.is_none() && data.group == "VirtualBox");
        assert_eq!(find(&snap, "old.vdi").reclaim.as_ref().unwrap().reason, "VirtualBox disk not attached to any VM");

        let old = find(&snap, "generic/debian12 4.3.2");
        let r = old.reclaim.as_ref().unwrap();
        assert_eq!(r.risk, Risk::Review);
        assert!(
            matches!(&r.action.as_ref().unwrap().steps[0], ActionStep::Command { argv, .. } if argv[0] == "vagrant")
        );
        assert!(find(&snap, "generic/debian12 4.3.10").reclaim.is_none());
        assert_eq!(find(&snap, "generic/debian12").measured_alloc, 1700 * MB);

        assert_eq!(find(&snap, "fedora").group, "GNOME Boxes");
        assert_eq!(find(&snap, "primary").measured_alloc, 3000 * MB);
        assert_eq!(find(&snap, "Multipass image cache").measured_alloc, 600 * MB);
        assert_eq!(find(&snap, "default/containers/web").measured_alloc, 400 * MB);
        assert_eq!(find(&snap, "default/images/0123456789ab").measured_alloc, 200 * MB);
        assert_eq!(find(&snap, "storage pool file default.img").measured_alloc, 1000 * MB);
        assert_eq!(find(&snap, "Incus other data").measured_alloc, 10 * MB);
    }

    #[test]
    fn nothing_installed() {
        let mut snap = snap_from(&[("/home/u/x", 1)]);
        let runner = FakeRunner::default();
        let out = VboxVagrant.collect(&ctx(&runner, &[("u", "/home/u")]), &mut snap);
        assert_eq!(out.coverage, crate::model::Coverage::Absent);
    }
}
