//! libvirt/QEMU virtual machines: domains and their disks (files with
//! qcow2 backing chains, LVs and other block devices, pool volumes, ISOs),
//! plus disk images and logical volumes that no VM uses.

use super::classifier::{Claimed, homes};
use super::{Ctx, Outcome, Provider};
use crate::model::tree::{Kind, flags};
use crate::model::{ActionSpec, ActionStep, BlockDev, Entity, NodeId, Reclaim, Risk, Snapshot, fmt_size};
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

pub struct Libvirt;

const URIS: [&str; 2] = ["qemu:///system", "qemu:///session"];
const GROUP: &str = "Virtual machines";
pub const ORPHAN_GROUP: &str = "Disk images (not used by any VM)";
const LV_GROUP: &str = "Unused logical volumes";
/// Smallest unreferenced image worth reporting.
const MIN_ORPHAN: u64 = 100 << 20;
/// Share domain for VM files, so a base image or ISO used by two VMs counts as shared.
const SHARE: &str = "libvirt";

// ---------------------------------------------------------------- XML

/// Minimal element tree; enough for libvirt's domain and pool XML.
#[derive(Debug, Default, Clone)]
pub struct XNode {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<XNode>,
    pub text: String,
}

impl XNode {
    pub fn attr(&self, k: &str) -> Option<&str> {
        self.attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }
    pub fn child(&self, name: &str) -> Option<&XNode> {
        self.children.iter().find(|c| c.name == name)
    }
    pub fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a XNode> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }
    fn text_of(&self, name: &str) -> Option<String> {
        self.child(name).map(|c| c.text.trim().to_string()).filter(|s| !s.is_empty())
    }
}

fn element(e: &BytesStart) -> XNode {
    XNode {
        name: e.local_name().as_ref().to_string(),
        attrs: e
            .attributes()
            .flatten()
            .map(|a| {
                let k = a.key.local_name().as_ref().to_string();
                let v = a.normalized_value(XmlVersion::Implicit1_0).map(|v| v.into_owned()).unwrap_or_default();
                (k, v)
            })
            .collect(),
        ..Default::default()
    }
}

pub fn parse_xml(s: &str) -> Option<XNode> {
    let mut r = quick_xml::Reader::from_str(s);
    let mut stack = vec![XNode::default()];
    loop {
        match r.read_event().ok()? {
            Event::Start(e) => stack.push(element(&e)),
            Event::Empty(e) => stack.last_mut()?.children.push(element(&e)),
            Event::End(_) => {
                if stack.len() < 2 {
                    return None;
                }
                let n = stack.pop()?;
                stack.last_mut()?.children.push(n);
            }
            Event::Text(t) => stack.last_mut()?.text.push_str(&t.xml10_content()),
            Event::CData(t) => stack.last_mut()?.text.push_str(&t.xml10_content()),
            Event::GeneralRef(g) => {
                let ch = match &*g {
                    "amp" => "&",
                    "lt" => "<",
                    "gt" => ">",
                    "quot" => "\"",
                    "apos" => "'",
                    _ => "",
                };
                stack.last_mut()?.text.push_str(ch);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let mut doc = stack.into_iter().next()?;
    doc.children.pop()
}

// ---------------------------------------------------------------- domains

#[derive(Debug, Default, Clone)]
pub struct Disk {
    /// file | block | volume | network | dir
    pub dtype: String,
    /// disk | cdrom | floppy | lun
    pub device: String,
    pub format: Option<String>,
    /// `source file=` or `source dev=`.
    pub source: Option<String>,
    pub pool: Option<String>,
    pub volume: Option<String>,
    pub network: Option<String>,
    pub target: String,
    /// Backing chain from `<backingStore>` (live XML), top first.
    pub backing: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct Domain {
    pub name: String,
    pub memory: u64,
    pub vcpus: u32,
    pub os: Option<String>,
    pub disks: Vec<Disk>,
}

fn unit_bytes(unit: &str) -> u64 {
    match unit {
        "b" | "bytes" => 1,
        "KB" => 1000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        "M" | "MiB" => 1 << 20,
        "G" | "GiB" => 1 << 30,
        "T" | "TiB" => 1 << 40,
        _ => 1024,
    }
}

pub fn parse_domain(xml: &str) -> Option<Domain> {
    let root = parse_xml(xml)?;
    if root.name != "domain" {
        return None;
    }
    let memory = root.child("memory").and_then(|m| {
        let v: u64 = m.text.trim().parse().ok()?;
        Some(v * unit_bytes(m.attr("unit").unwrap_or("KiB")))
    });
    let os = root
        .child("metadata")
        .and_then(|m| m.child("libosinfo"))
        .and_then(|l| l.child("os"))
        .and_then(|o| o.attr("id"))
        .map(|id| id.trim_start_matches("http://").trim_start_matches("https://").to_string());
    let disks = root
        .child("devices")
        .into_iter()
        .flat_map(|d| d.all("disk"))
        .map(|d| {
            let src = d.child("source");
            let sattr = |k: &str| src.and_then(|s| s.attr(k)).map(String::from);
            let mut backing = Vec::new();
            let mut bs = d.child("backingStore");
            while let Some(b) = bs {
                match b.child("source").and_then(|s| s.attr("file").or(s.attr("dev"))) {
                    Some(p) => backing.push(p.to_string()),
                    None => break,
                }
                bs = b.child("backingStore");
            }
            Disk {
                dtype: d.attr("type").unwrap_or("file").into(),
                device: d.attr("device").unwrap_or("disk").into(),
                format: d.child("driver").and_then(|x| x.attr("type")).map(String::from),
                source: sattr("file").or_else(|| sattr("dev")),
                pool: sattr("pool"),
                volume: sattr("volume"),
                network: sattr("protocol").map(|p| format!("{p}:{}", sattr("name").unwrap_or_default())),
                target: d.child("target").and_then(|t| t.attr("dev")).unwrap_or("?").into(),
                backing,
            }
        })
        .collect();
    Some(Domain {
        name: root.text_of("name")?,
        memory: memory.unwrap_or(0),
        vcpus: root.text_of("vcpu").and_then(|v| v.parse().ok()).unwrap_or(0),
        os,
        disks,
    })
}

/// (type, name, target path) from `virsh pool-dumpxml`.
pub fn parse_pool(xml: &str) -> Option<(String, String, Option<String>)> {
    let root = parse_xml(xml)?;
    (root.name == "pool").then(|| {
        (
            root.attr("type").unwrap_or_default().to_string(),
            root.text_of("name").unwrap_or_default(),
            root.child("target").and_then(|t| t.text_of("path")),
        )
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Vol {
    pub name: String,
    pub path: String,
    pub vtype: String,
    pub capacity: Option<u64>,
    pub allocation: Option<u64>,
}

/// `virsh vol-list <pool> --details`: columns are cut at the header's offsets
/// because names and paths may contain spaces.
pub fn parse_vol_list(out: &str) -> Vec<Vol> {
    let mut lines = out.lines();
    let Some(header) = lines.by_ref().find(|l| l.contains("Name") && l.contains("Path")) else { return vec![] };
    let cols: Vec<usize> =
        ["Name", "Path", "Type", "Capacity", "Allocation"].iter().filter_map(|c| header.find(c)).collect();
    if cols.len() != 5 {
        return vec![];
    }
    lines
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with("---"))
        .filter_map(|l| {
            let ch: Vec<char> = l.chars().collect();
            let field = |i: usize| -> String {
                let end = cols.get(i + 1).copied().unwrap_or(ch.len()).min(ch.len());
                ch.get(cols[i].min(ch.len())..end)
                    .map(|s| s.iter().collect::<String>())
                    .unwrap_or_default()
                    .trim()
                    .into()
            };
            let path = field(1);
            path.starts_with('/').then(|| Vol {
                name: field(0),
                path,
                vtype: field(2),
                capacity: super::parse_size(&field(3)),
                allocation: super::parse_size(&field(4)),
            })
        })
        .collect()
}

// ---------------------------------------------------------------- qemu-img

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Image {
    pub filename: String,
    pub format: String,
    pub virtual_size: u64,
    pub actual_size: u64,
    /// Internal snapshots and the VM state they hold.
    pub snapshots: usize,
    pub vm_state: u64,
}

/// `qemu-img info --backing-chain --output=json`: top image first.
pub fn parse_qemu_img(json: &str) -> Vec<Image> {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return vec![] };
    let items: Vec<Value> = match v {
        Value::Array(a) => a,
        o @ Value::Object(_) => vec![o],
        _ => return vec![],
    };
    let mut out: Vec<Image> = Vec::new();
    for it in items {
        let num = |k: &str| it.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        let mut filename = it.get("filename").and_then(|x| x.as_str()).unwrap_or_default().to_string();
        // Relative backing names are relative to the image that refers to them.
        if !filename.starts_with('/')
            && let Some(dir) = out.last().and_then(|p| p.filename.rsplit_once('/')).map(|(d, _)| d.to_string())
        {
            filename = format!("{dir}/{filename}");
        }
        let snaps = it.get("snapshots").and_then(|s| s.as_array());
        out.push(Image {
            filename,
            format: it.get("format").and_then(|x| x.as_str()).unwrap_or_default().into(),
            virtual_size: num("virtual-size"),
            actual_size: num("actual-size"),
            snapshots: snaps.map(|s| s.len()).unwrap_or(0),
            vm_state: snaps.into_iter().flatten().filter_map(|s| s.get("vm-state-size").and_then(|x| x.as_u64())).sum(),
        });
    }
    out
}

// ---------------------------------------------------------------- helpers

/// Block device for a disk source path: `/dev/VG/LV`, `/dev/mapper/NAME`, `/dev/KNAME`,
/// or (real system only) anything that canonicalizes to one.
pub fn find_block<'a>(block: &'a [BlockDev], dev: &str) -> Option<&'a BlockDev> {
    let dm = |vg: &str, lv: &str| format!("{}-{}", vg.replace('-', "--"), lv.replace('-', "--"));
    let rest = dev.strip_prefix("/dev/").unwrap_or(dev);
    let want = match rest.strip_prefix("mapper/") {
        Some(n) => Some(n.to_string()),
        None => rest.split_once('/').filter(|(_, lv)| !lv.contains('/')).map(|(vg, lv)| dm(vg, lv)),
    };
    block.iter().find(|d| d.path == dev || want.as_deref() == Some(d.name.as_str()) || d.kname == rest).or_else(|| {
        let real = std::fs::canonicalize(dev).ok()?;
        let k = real.file_name()?.to_string_lossy().into_owned();
        block.iter().find(|d| d.kname == k)
    })
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn is_under(path: &str, dirs: &[String]) -> bool {
    dirs.iter().any(|d| path.strip_prefix(d.as_str()).is_some_and(|r| r.starts_with('/')))
}

enum Source {
    File(String),
    Block(String),
    Other,
}

fn vm_entity(kind: &str, name: String, parent: u32) -> Entity {
    Entity {
        kind: kind.into(),
        name,
        provider: "libvirt".into(),
        group: GROUP.into(),
        parent: Some(parent),
        share_key: Some(SHARE.into()),
        ..Default::default()
    }
}

/// Use a tool-reported size when the scan could not see the file.
fn external_if_unscanned(snap: &Snapshot, e: &mut Entity, path: &str, actual: Option<u64>) {
    let seen = snap.lookup_static(path).is_some_and(|n| !snap.tree.node(n).has(flags::DENIED));
    if let (false, Some(a)) = (seen, actual.filter(|&a| a > 0)) {
        e.external_bytes = a;
        e.attrs.push(("size source".into(), "qemu-img (file not visible to the scan)".into()));
    }
}

fn file_entities(
    ctx: &Ctx,
    snap: &Snapshot,
    vm: u32,
    disk: &Disk,
    path: &str,
    qemu_cache: &mut HashMap<String, Vec<Image>>,
    outcome: &mut Outcome,
) -> (Vec<Entity>, Vec<String>) {
    let mut refs = vec![path.to_string()];
    if disk.device == "cdrom" || disk.device == "floppy" {
        let mut e = vm_entity("vm.iso", format!("{}: {} ({})", disk.target, basename(path), disk.device), vm);
        e.paths = vec![path.into()];
        e.attrs.push(("path".into(), path.into()));
        return (vec![e], refs);
    }
    let chain = qemu_cache
        .entry(path.to_string())
        .or_insert_with(|| {
            let argv = ["qemu-img", "info", "--backing-chain", "--output=json", "-U", path];
            match ctx.runner.run(&argv) {
                Some(o) if o.ok() => parse_qemu_img(&o.stdout),
                Some(o) => {
                    let err = o.stderr.lines().last().unwrap_or("").trim().to_string();
                    outcome.degrade(format!("qemu-img could not read {path}: {err}"));
                    vec![]
                }
                None => vec![],
            }
        })
        .clone();
    let top = chain.first();
    let mut e = vm_entity("vm.disk", format!("{}: {}", disk.target, basename(path)), vm);
    e.paths = vec![path.into()];
    e.attrs.push(("path".into(), path.into()));
    let format = top.map(|t| t.format.clone()).or_else(|| disk.format.clone());
    if let Some(f) = format {
        e.attrs.push(("format".into(), f));
    }
    if let Some(t) = top {
        e.virtual_size = Some(t.virtual_size);
        e.reported = Some(t.actual_size);
        e.attrs.push(("virtual size".into(), fmt_size(t.virtual_size)));
        if t.snapshots > 0 {
            e.attrs.push(("internal snapshots".into(), format!("{} ({} VM state)", t.snapshots, fmt_size(t.vm_state))));
        }
    }
    let backing: Vec<(String, Option<&Image>)> = if chain.len() > 1 {
        chain[1..].iter().map(|i| (i.filename.clone(), Some(i))).collect()
    } else {
        disk.backing.iter().map(|p| (p.clone(), None)).collect()
    };
    if !backing.is_empty() {
        let names: Vec<&str> = backing.iter().map(|(p, _)| basename(p)).collect();
        e.attrs.push(("backing chain".into(), names.join(" → ")));
    }
    external_if_unscanned(snap, &mut e, path, top.map(|t| t.actual_size));
    let mut out = vec![e];
    for (p, img) in backing {
        let mut b = vm_entity("vm.backing", format!("{} backing: {}", disk.target, basename(&p)), vm);
        b.paths = vec![p.clone()];
        b.attrs.push(("path".into(), p.clone()));
        if let Some(i) = img {
            b.virtual_size = Some(i.virtual_size);
            b.reported = Some(i.actual_size);
            b.attrs.push(("format".into(), i.format.clone()));
        }
        external_if_unscanned(snap, &mut b, &p, img.map(|i| i.actual_size));
        refs.push(p);
        out.push(b);
    }
    (out, refs)
}

impl Provider for Libvirt {
    fn name(&self) -> &'static str {
        "libvirt"
    }

    fn collect(&self, ctx: &Ctx, snap: &mut Snapshot) -> Outcome {
        let mut outcome = Outcome::complete();
        let has_virsh = ctx.runner.has("virsh");
        let (mut system_ok, mut session_ok) = (false, false);
        let mut doms: Vec<(&str, Domain, String)> = Vec::new();
        // (uri, name, type, target path)
        let mut pools: Vec<(&str, String, String, Option<String>)> = Vec::new();
        let ok = |o: &super::runner::Output| o.ok();
        for uri in URIS.iter().copied().filter(|_| has_virsh) {
            let virsh = |args: &[&str]| {
                let mut argv = vec!["virsh", "-c", uri];
                argv.extend(args);
                ctx.runner.run(&argv)
            };
            let names: Vec<String> = match virsh(&["list", "--all", "--name"]) {
                Some(o) if o.ok() => {
                    o.stdout.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
                }
                Some(o) => {
                    let err = o.stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("failed").trim().to_string();
                    outcome.degrade(format!("{uri}: {err}"));
                    continue;
                }
                None => continue,
            };
            if uri == URIS[0] {
                system_ok = true;
            } else {
                session_ok = true;
            }
            for name in names {
                match virsh(&["dumpxml", &name]).filter(ok).and_then(|o| parse_domain(&o.stdout)) {
                    Some(d) => {
                        let state = virsh(&["domstate", &name]).filter(ok).map(|o| o.stdout.trim().to_string());
                        doms.push((uri, d, state.unwrap_or_default()));
                    }
                    None => outcome.degrade(format!("{uri}: could not read domain {name}")),
                }
            }
            let pool_names = virsh(&["pool-list", "--all", "--name"]).filter(ok).map(|o| o.stdout).unwrap_or_default();
            for p in pool_names.lines().map(str::trim).filter(|l| !l.is_empty()) {
                if let Some((t, _, path)) = virsh(&["pool-dumpxml", p]).filter(ok).and_then(|o| parse_pool(&o.stdout)) {
                    pools.push((uri, p.to_string(), t, path));
                }
            }
        }

        // Domains and their disks.
        let mut vols: HashMap<(&str, String), Vec<Vol>> = HashMap::new();
        let mut qemu_cache: HashMap<String, Vec<Image>> = HashMap::new();
        let mut ref_paths: Vec<String> = Vec::new();
        let mut ref_devs: HashSet<String> = HashSet::new();
        let mut used_by: Vec<(String, String)> = Vec::new();
        // Disk sources already attached to a VM (e.g. one VM defined in both
        // qemu:///system and qemu:///session): count their bytes only once.
        let mut first_user: HashMap<String, String> = HashMap::new();
        let mut added = 0usize;
        for (uri, d, state) in &doms {
            let session = *uri == URIS[1];
            let mut vm = Entity {
                kind: "vm".into(),
                name: if session { format!("{} (session)", d.name) } else { d.name.clone() },
                provider: "libvirt".into(),
                group: GROUP.into(),
                ..Default::default()
            };
            vm.attrs.push(("state".into(), state.clone()));
            vm.attrs.push(("vCPUs".into(), d.vcpus.to_string()));
            vm.attrs.push(("memory".into(), fmt_size(d.memory)));
            if let Some(os) = &d.os {
                vm.attrs.push(("os".into(), os.clone()));
            }
            vm.attrs.push(("connection".into(), uri.to_string()));
            let vm_id = snap.add_entity(vm);
            added += 1;
            for disk in &d.disks {
                let src = match (disk.dtype.as_str(), &disk.source) {
                    ("file", Some(p)) => Source::File(p.clone()),
                    ("block", Some(p)) => Source::Block(p.clone()),
                    ("volume", _) => {
                        let (Some(pool), Some(vol)) = (&disk.pool, &disk.volume) else { continue };
                        let ptype = pools.iter().find(|p| p.0 == *uri && &p.1 == pool).map(|p| p.2.as_str());
                        let list = vols.entry((uri, pool.clone())).or_insert_with(|| {
                            let argv = ["virsh", "-c", uri, "vol-list", pool, "--details"];
                            ctx.runner.run(&argv).filter(ok).map(|o| parse_vol_list(&o.stdout)).unwrap_or_default()
                        });
                        match list.iter().find(|v| &v.name == vol) {
                            Some(v) if v.vtype == "block" || ptype == Some("logical") => Source::Block(v.path.clone()),
                            Some(v) => Source::File(v.path.clone()),
                            None => {
                                outcome.degrade(format!("{}: volume {pool}/{vol} not found", d.name));
                                Source::Other
                            }
                        }
                    }
                    _ => Source::Other,
                };
                match src {
                    Source::File(path) => {
                        let (mut es, refs) =
                            file_entities(ctx, snap, vm_id, disk, &path, &mut qemu_cache, &mut outcome);
                        ref_paths.extend(refs);
                        if let Some(other) = first_user.get(&path) {
                            for e in es.iter_mut() {
                                e.attrs.push(("shared with".into(), format!("VM {other}")));
                            }
                        } else {
                            first_user.insert(path.clone(), d.name.clone());
                        }
                        for e in es {
                            snap.add_entity(e);
                            added += 1;
                        }
                    }
                    Source::Block(dev) => {
                        let mut e = vm_entity("vm.disk", format!("{}: {dev}", disk.target), vm_id);
                        e.share_key = None;
                        e.attrs.push(("device".into(), dev.clone()));
                        match find_block(&snap.block, &dev) {
                            Some(b) => {
                                e.block_devs = vec![b.kname.clone()];
                                match first_user.get(&b.kname) {
                                    Some(other) => {
                                        e.attrs.push(("shared with".into(), format!("VM {other} (counted there)")));
                                    }
                                    None => {
                                        first_user.insert(b.kname.clone(), d.name.clone());
                                        e.external_bytes = b.size;
                                    }
                                }
                                e.virtual_size = Some(b.size);
                                ref_devs.insert(b.kname.clone());
                                used_by.push((b.kname.clone(), format!("disk of VM {}", d.name)));
                            }
                            None => e.attrs.push(("note".into(), "block device not found".into())),
                        }
                        snap.add_entity(e);
                        added += 1;
                    }
                    Source::Other => {
                        if let Some(net) = &disk.network {
                            let mut e = vm_entity("vm.disk", format!("{}: {net}", disk.target), vm_id);
                            e.share_key = None;
                            e.attrs.push(("note".into(), "network disk, stored on another host".into()));
                            snap.add_entity(e);
                            added += 1;
                        }
                    }
                }
            }
        }
        for (k, what) in used_by {
            if let Some(b) = snap.block.iter_mut().find(|b| b.kname == k)
                && !b.used_by.contains(&what)
            {
                b.used_by.push(what);
            }
        }

        // Disk images no VM references.
        let check_files = !has_virsh || system_ok;
        if !check_files {
            outcome.degrade("system libvirt not reachable; unused disk images not flagged");
        }
        let claimed = Claimed::from_entities(snap);
        let tree = &snap.tree;
        let ref_nodes: HashSet<NodeId> = ref_paths.iter().filter_map(|p| snap.lookup_static(p)).collect();
        let homes = homes(ctx);
        let mut vm_dirs: Vec<String> = pools
            .iter()
            .filter(|p| matches!(p.2.as_str(), "dir" | "fs" | "netfs"))
            .filter_map(|p| p.3.clone())
            .collect();
        vm_dirs.push("/var/lib/libvirt/images".into());
        let mut skip: Vec<String> =
            ["/var/snap/multipass", "/var/lib/incus", "/var/lib/lxd", "/var/snap/lxd"].map(String::from).to_vec();
        for (_, h) in &homes {
            vm_dirs.push(format!("{h}/.local/share/libvirt/images"));
            vm_dirs.push(format!("{h}/.local/share/gnome-boxes/images"));
            skip.push(format!("{h}/VirtualBox VMs"));
            skip.push(format!("{h}/.vagrant.d"));
            skip.push(format!("{h}/.local/share/Trash"));
            if ctx.is_root || !session_ok {
                // Their session VMs were not (or could not be) checked.
                skip.push(format!("{h}/.local/share/libvirt"));
                skip.push(format!("{h}/.local/share/gnome-boxes"));
            }
        }
        let mut orphans: Vec<(String, bool)> = Vec::new();
        for (i, n) in tree.nodes.iter().enumerate().filter(|_| check_files) {
            if n.kind != Kind::File || n.alloc < MIN_ORPHAN || n.has(flags::HARDLINK_DUP) {
                continue;
            }
            let id = i as NodeId;
            let name = tree.name_bytes(id);
            let Some(dot) = name.iter().rposition(|&b| b == b'.') else { continue };
            let ext = name[dot + 1..].to_ascii_lowercase();
            let pool_only = match ext.as_slice() {
                b"qcow2" | b"qcow" | b"vmdk" | b"vdi" | b"vhd" | b"vhdx" | b"iso" => false,
                b"img" | b"raw" => true,
                _ => continue,
            };
            if ref_nodes.contains(&id) || claimed.covers(tree, id) {
                continue;
            }
            let path = tree.path(id);
            if is_under(&path, &skip) || (pool_only && !is_under(&path, &vm_dirs)) || path.contains("/.Trash-") {
                continue;
            }
            orphans.push((path, ext == b"iso"));
        }
        for (path, iso) in orphans {
            let reason = match (iso, has_virsh) {
                (true, _) => "ISO image not attached to any VM",
                (false, true) => "disk image not used by any libvirt VM",
                (false, false) => "disk image not used by any VM (libvirt not installed)",
            };
            snap.add_entity(Entity {
                kind: if iso { "iso" } else { "vm.image" }.into(),
                name: basename(&path).to_string(),
                provider: "libvirt".into(),
                group: ORPHAN_GROUP.into(),
                paths: vec![path.clone()],
                attrs: vec![("path".into(), path.clone())],
                reclaim: Some(Reclaim {
                    risk: Risk::Review,
                    reason: reason.into(),
                    estimate: None,
                    action: Some(ActionSpec {
                        label: format!("move {} to trash", basename(&path)),
                        steps: vec![ActionStep::DeletePath { path, trash: true }],
                    }),
                }),
                ..Default::default()
            });
            added += 1;
        }

        // Logical volumes nothing uses (report only: LVM changes are never automated).
        let mut lv_orphans: Vec<Entity> = Vec::new();
        for lv in &snap.lvm.lvs {
            let Some(k) = lv.kname.as_deref() else { continue };
            let pool_like = lv.segtype.as_deref().is_some_and(|s| s.contains("pool"))
                || lv.attr.as_deref().is_some_and(|a| a.starts_with('t'));
            if ref_devs.contains(k) || pool_like {
                continue;
            }
            let Some(d) = snap.block.iter().find(|d| d.kname == k) else { continue };
            let stacked = snap.block.iter().any(|c| c.parents.iter().any(|p| p == k));
            if d.fstype.is_some() || !d.mountpoints.is_empty() || !d.used_by.is_empty() || stacked {
                continue;
            }
            let size = if lv.size > 0 { lv.size } else { d.size };
            let caveat = if has_virsh && !system_ok { " (libvirt VMs could not be checked)" } else { "" };
            lv_orphans.push(Entity {
                kind: "lv".into(),
                name: format!("LV {}/{}", lv.vg, lv.name),
                provider: "libvirt".into(),
                group: LV_GROUP.into(),
                block_devs: vec![k.to_string()],
                external_bytes: size,
                attrs: vec![("device".into(), d.path.clone())],
                reclaim: Some(Reclaim {
                    risk: Risk::Danger,
                    reason: format!(
                        "LV {}/{} appears unused: no filesystem, not mounted, not swap, not used by any VM{caveat}",
                        lv.vg, lv.name
                    ),
                    estimate: None,
                    action: None,
                }),
                ..Default::default()
            });
        }
        added += lv_orphans.len();
        for e in lv_orphans {
            snap.add_entity(e);
        }

        if !has_virsh {
            if added == 0 {
                return Outcome::absent();
            }
            outcome = outcome.note("virsh not installed; unused images/LVs found by file pattern and block layout");
        }
        outcome.note(format!("{} VMs", doms.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::attribution::attribute;
    use crate::providers::classifier::testing::{ctx, find, snap_from};
    use crate::providers::runner::FakeRunner;

    const MB: u64 = 1 << 20;
    const GB: u64 = 1 << 30;

    fn fx(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/libvirt/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn parses_real_domain() {
        let d = parse_domain(&fx("archlinux.xml")).unwrap();
        assert_eq!(d.name, "archlinux");
        assert_eq!(d.vcpus, 6);
        assert_eq!(d.memory, GB);
        assert_eq!(d.os.as_deref(), Some("archlinux.org/archlinux/rolling"));
        assert_eq!(d.disks.len(), 2);
        assert_eq!(d.disks[0].dtype, "block");
        assert_eq!(d.disks[0].source.as_deref(), Some("/dev/vg_system/lv_vm_test"));
        assert_eq!(d.disks[0].target, "vda");
        assert_eq!(d.disks[1].device, "cdrom");
        assert_eq!(d.disks[1].source.as_deref(), Some("/home/u/Downloads/archlinux-x86_64.iso"));
    }

    #[test]
    fn parses_handwritten_domains() {
        let d = parse_domain(&fx("web1.xml")).unwrap();
        assert_eq!(d.disks[0].backing, vec!["/var/lib/libvirt/images/base.qcow2".to_string()]);
        assert_eq!(d.disks[1].source, None);
        assert_eq!(d.disks[2].network.as_deref(), Some("rbd:pool/web1-data"));
        let d = parse_domain(&fx("web2.xml")).unwrap();
        assert_eq!(d.name, "web2 & friends");
        assert_eq!(d.memory, 2 * GB);
        assert_eq!((d.disks[0].pool.as_deref(), d.disks[0].volume.as_deref()), (Some("default"), Some("web2.qcow2")));
        assert!(parse_domain("<pool/>").is_none());
        assert!(parse_domain("<domain><name>x</broken>").is_none());
    }

    #[test]
    fn parses_pools_and_volumes() {
        assert_eq!(
            parse_pool(&fx("pool-vg_system.xml")),
            Some(("logical".into(), "vg_system".into(), Some("/dev/vg_system".into())))
        );
        assert_eq!(parse_pool(&fx("pool-default.xml")).unwrap().2.as_deref(), Some("/var/lib/libvirt/images"));
        let v = parse_vol_list(&fx("vol-list-vg_system.txt"));
        assert_eq!(v.len(), 6);
        let t = v.iter().find(|x| x.name == "lv_vm_test").unwrap();
        assert_eq!((t.vtype.as_str(), t.capacity), ("block", Some(30 * GB)));
        assert!(parse_vol_list(&fx("vol-list-default.txt")).is_empty());
        let v = parse_vol_list(&fx("vol-list-default-vms.txt"));
        assert_eq!(v[1].name, "my disk.raw");
        assert_eq!(v[1].path, "/var/lib/libvirt/images/my disk.raw");
        assert_eq!(v[1].allocation, Some(GB));
    }

    #[test]
    fn parses_qemu_img_chain() {
        let c = parse_qemu_img(&fx("qemu-img-web1.json"));
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].format, "qcow2");
        assert_eq!(c[0].virtual_size, 20 * GB);
        assert_eq!(c[0].snapshots, 1);
        assert_eq!(c[1].filename, "/var/lib/libvirt/images/base.qcow2");
        let iso = parse_qemu_img(&fx("qemu-img-iso.json"));
        assert_eq!((iso[0].format.as_str(), iso[0].virtual_size), ("raw", 1550024704));
    }

    #[test]
    fn finds_block_devices() {
        let block = crate::providers::block::parse_lsblk(include_str!("../../tests/fixtures/lsblk.json"));
        assert_eq!(find_block(&block, "/dev/vg_system/lv_vm_test").unwrap().kname, "dm-4");
        assert_eq!(find_block(&block, "/dev/mapper/vg_system-lv_vm_test").unwrap().kname, "dm-4");
        assert_eq!(find_block(&block, "/dev/nvme0n1p4").unwrap().kname, "nvme0n1p4");
        assert!(find_block(&block, "/dev/vg_system/nope").is_none());
    }

    fn system() -> (Snapshot, FakeRunner) {
        let mut snap = snap_from(&[
            ("/var/lib/libvirt/images/base.qcow2", 2 * GB),
            ("/var/lib/libvirt/images/web1.qcow2", 200 * MB),
            ("/var/lib/libvirt/images/web2.qcow2", 150 * MB),
            ("/var/lib/libvirt/images/stale.qcow2", 300 * MB),
            ("/var/lib/libvirt/images/my disk.raw", GB),
            ("/var/lib/libvirt/images/tiny.qcow2", MB),
            ("/home/u/Downloads/archlinux-x86_64.iso", 1500 * MB),
            ("/home/u/Downloads/debian-12.iso", 600 * MB),
            ("/home/u/Downloads/ubuntu.iso", 5 * GB),
            ("/home/u/Pictures/sdcard.img", 2 * GB),
            ("/home/u/VirtualBox VMs/x/x.vdi", 3 * GB),
            ("/home/u/.local/share/Trash/files/old.qcow2", GB),
            ("/home/u/docker/overlay2/l/disk.qcow2", GB),
        ]);
        snap.add_entity(Entity { name: "docker".into(), paths: vec!["/home/u/docker".into()], ..Default::default() });
        snap.meta.kernel = "x".into();
        let mut block = crate::providers::block::parse_lsblk(include_str!("../../tests/fixtures/lsblk.json"));
        for (k, name, size) in [("dm-9", "vg_system-lv_web2--data", 8 * GB), ("dm-10", "vg_system-lv_scratch", 4 * GB)]
        {
            block.push(BlockDev {
                kname: k.into(),
                name: name.into(),
                path: format!("/dev/mapper/{name}"),
                dtype: "lvm".into(),
                size,
                parents: vec!["nvme1n1p2".into()],
                ..Default::default()
            });
        }
        // As the block provider would: swap is marked in use.
        block.iter_mut().find(|d| d.kname == "dm-0").unwrap().used_by.push("swap".into());
        snap.lvm = crate::providers::lvm::infer_from_block(&block, |_| None);
        snap.block = block;
        let sys = |args: &str| format!("virsh -c qemu:///system {args}");
        let runner = FakeRunner::default()
            .with(&sys("list --all --name"), "archlinux\nweb1\nweb2 & friends\n\n")
            .with(&sys("dumpxml archlinux"), &fx("archlinux.xml"))
            .with(&sys("domstate archlinux"), &fx("domstate.txt"))
            .with(&sys("dumpxml web1"), &fx("web1.xml"))
            .with(&sys("domstate web1"), "running\n")
            .with(&sys("dumpxml web2 & friends"), &fx("web2.xml"))
            .with(&sys("domstate web2 & friends"), "shut off\n")
            .with(&sys("pool-list --all --name"), "default\nvg_system\n")
            .with(&sys("pool-dumpxml default"), &fx("pool-default.xml"))
            .with(&sys("pool-dumpxml vg_system"), &fx("pool-vg_system.xml"))
            .with(&sys("vol-list default --details"), &fx("vol-list-default-vms.txt"))
            .with("virsh -c qemu:///session list --all --name", "\n")
            .with("virsh -c qemu:///session pool-list --all --name", "\n")
            .with(
                "qemu-img info --backing-chain --output=json -U /var/lib/libvirt/images/web1.qcow2",
                &fx("qemu-img-web1.json"),
            )
            .with(
                "qemu-img info --backing-chain --output=json -U /var/lib/libvirt/images/web2.qcow2",
                &fx("qemu-img-web2.json"),
            );
        (snap, runner)
    }

    #[test]
    fn same_vm_in_system_and_session_counts_disks_once() {
        let (mut snap, mut runner) = system();
        let xml = fx("archlinux.xml");
        let ses = |args: &str| format!("virsh -c qemu:///session {args}");
        runner = runner
            .with(&ses("list --all --name"), "archlinux\n")
            .with(&ses("dumpxml archlinux"), &xml)
            .with(&ses("domstate archlinux"), &fx("domstate.txt"));
        Libvirt.collect(&ctx(&runner, &[("u", "/home/u")]), &mut snap);
        attribute(&mut snap);
        let disks: Vec<&Entity> = snap.entities.iter().filter(|e| e.name == "vda: /dev/vg_system/lv_vm_test").collect();
        assert_eq!(disks.len(), 2);
        assert_eq!(disks.iter().map(|e| e.external_bytes).sum::<u64>(), 30 * GB);
        assert!(disks[1].attr("shared with").is_some_and(|v| v.starts_with("VM archlinux")));
        let isos: Vec<&Entity> = snap.entities.iter().filter(|e| e.kind == "vm.iso").collect();
        assert!(isos.iter().any(|e| e.attr("shared with").is_some()));
        let group = crate::views::workloads(&snap).into_iter().find(|g| g.name == "Virtual machines").unwrap();
        // Both VMs share the 30 GiB LV and the 1.5 GiB ISO; web1/web2 add their own disks.
        assert!(group.total < 2 * 30 * GB, "{}", group.total);
    }

    #[test]
    fn vms_disks_and_orphans() {
        let (mut snap, runner) = system();
        let out = Libvirt.collect(&ctx(&runner, &[("u", "/home/u")]), &mut snap);
        assert_eq!(out.coverage, crate::model::Coverage::Complete, "{:?}", out.notes);
        attribute(&mut snap);

        // Block disk: the LV is now accounted to the VM and marked as used.
        let arch = find(&snap, "archlinux");
        assert_eq!(arch.attr("state"), Some("shut off"));
        assert_eq!(arch.attr("vCPUs"), Some("6"));
        let lvdisk = find(&snap, "vda: /dev/vg_system/lv_vm_test");
        assert_eq!(lvdisk.block_devs, vec!["dm-4".to_string()]);
        assert_eq!(lvdisk.external_bytes, 30 * GB);
        assert_eq!(arch.total_bytes(), 30 * GB + 1500 * MB);
        let dm4 = snap.block.iter().find(|b| b.kname == "dm-4").unwrap();
        assert_eq!(dm4.used_by, vec!["disk of VM archlinux".to_string()]);

        // File disk with a backing chain shared by two VMs.
        let w1 = find(&snap, "vda: web1.qcow2");
        assert_eq!(w1.virtual_size, Some(20 * GB));
        assert_eq!(w1.attr("backing chain"), Some("base.qcow2"));
        assert!(w1.attr("internal snapshots").unwrap().starts_with("1 "));
        let bases: Vec<&Entity> = snap.entities.iter().filter(|e| e.name == "vda backing: base.qcow2").collect();
        assert_eq!(bases.len(), 2);
        assert!(bases.iter().all(|b| b.measured_alloc == 2 * GB && b.measured_unique == 0));
        let w2vm = find(&snap, "web2 & friends");
        assert_eq!(w2vm.total_bytes(), 150 * MB + 2 * GB + 8 * GB + 600 * MB);
        assert_eq!(find(&snap, "vdb: /dev/vg_system/lv_web2-data").block_devs, vec!["dm-9".to_string()]);

        // Orphans: unreferenced images, but not tiny ones, not pool-less .img, not
        // other hypervisors' dirs, not the trash, not inside other providers' paths.
        let mut orphans: Vec<&str> =
            snap.entities.iter().filter(|e| e.group == ORPHAN_GROUP).map(|e| e.name.as_str()).collect();
        orphans.sort();
        assert_eq!(orphans, vec!["my disk.raw", "stale.qcow2", "ubuntu.iso"]);
        let st = find(&snap, "stale.qcow2").reclaim.clone().unwrap();
        assert_eq!(st.risk, Risk::Review);
        assert_eq!(
            st.action.unwrap().steps,
            vec![ActionStep::DeletePath { path: "/var/lib/libvirt/images/stale.qcow2".into(), trash: true }]
        );

        // Unused LV: only lv_scratch (lv_vm_test and lv_web2-data are VM disks, swap is swap).
        let lvs: Vec<&Entity> = snap.entities.iter().filter(|e| e.group == LV_GROUP).collect();
        assert_eq!(lvs.len(), 1);
        assert_eq!(lvs[0].name, "LV vg_system/lv_scratch");
        assert_eq!(lvs[0].external_bytes, 4 * GB);
        let r = lvs[0].reclaim.as_ref().unwrap();
        assert!(r.risk == Risk::Danger && r.action.is_none());
        assert!(r.reason.contains("appears unused"));
    }

    #[test]
    fn unreachable_system_skips_orphan_files() {
        let (mut snap, _) = system();
        let mut runner = FakeRunner::default().with("virsh -c qemu:///session list --all --name", "");
        runner.outputs.insert(
            "virsh -c qemu:///system list --all --name".into(),
            crate::providers::runner::Output {
                status: 1,
                stdout: String::new(),
                stderr: "error: failed to connect to the hypervisor\n".into(),
            },
        );
        let out = Libvirt.collect(&ctx(&runner, &[("u", "/home/u")]), &mut snap);
        assert_eq!(out.coverage, crate::model::Coverage::Partial);
        assert!(snap.entities.iter().all(|e| e.group != ORPHAN_GROUP));
        // LVs are still reported, with a caveat; lv_vm_test now looks unused.
        let lv = find(&snap, "LV vg_system/lv_vm_test");
        assert!(lv.reclaim.as_ref().unwrap().reason.contains("could not be checked"));
    }

    #[test]
    fn without_libvirt() {
        let (mut snap, _) = system();
        let runner = FakeRunner::default();
        let out = Libvirt.collect(&ctx(&runner, &[("u", "/home/u")]), &mut snap);
        assert_eq!(out.coverage, crate::model::Coverage::Complete);
        let orphans = snap.entities.iter().filter(|e| e.group == ORPHAN_GROUP).count();
        // Nothing references anything: every large image is flagged.
        assert_eq!(orphans, 8);
    }
}
