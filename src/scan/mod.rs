//! Scan orchestration: choose filesystems, walk them in parallel, build the
//! arena tree, then collect space the tree can't see (deleted-but-open files,
//! files hidden under mountpoints).

pub mod deleted_open;
pub mod hidden;
pub mod mounts;
pub mod walker;

use crate::model::tree::{FNode, Kind, flags};
use crate::model::{FileTree, FsInfo, NONE, StatVfs};
use mounts::{Mount, ScanUnit, Selection};
use rayon::prelude::*;
use std::collections::{HashSet, VecDeque};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};
use walker::{Progress, TmpDir};

#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    /// Explicit roots; empty means every real filesystem.
    pub roots: Vec<PathBuf>,
    /// With explicit roots: don't descend into other filesystems mounted below them.
    pub one_file_system: bool,
    pub selection: Selection,
    pub excludes: Vec<String>,
    pub threads: Option<usize>,
    pub quiet: bool,
}

pub struct ScanResult {
    pub tree: FileTree,
    pub filesystems: Vec<FsInfo>,
    pub mounts: Vec<Mount>,
}

fn statvfs(path: &str) -> Option<StatVfs> {
    let s = rustix::fs::statvfs(path).ok()?;
    let frsize = if s.f_frsize > 0 { s.f_frsize } else { s.f_bsize };
    Some(StatVfs {
        total: s.f_blocks * frsize,
        free: s.f_bfree * frsize,
        avail: s.f_bavail * frsize,
        files: s.f_files,
        files_free: s.f_ffree,
        bsize: frsize,
    })
}

fn fs_info(u: &ScanUnit) -> FsInfo {
    FsInfo {
        mount_point: u.mount.mount_point.clone(),
        scan_root: u.scan_root.clone(),
        aliases: u.aliases.clone(),
        source: u.mount.source.clone(),
        fstype: u.mount.fstype.clone(),
        options: u.mount.options.clone(),
        fs_root: u.mount.root.clone(),
        dev_major: u.mount.major,
        dev_minor: u.mount.minor,
        statvfs: statvfs(&u.mount.mount_point),
        ..Default::default()
    }
}

/// Decide what to walk for the given options.
pub fn plan(opts: &ScanOptions, all_mounts: &[Mount]) -> (Vec<ScanUnit>, Vec<(Mount, String)>) {
    let (units, skipped) = mounts::select_units(all_mounts, &opts.selection);
    if opts.roots.is_empty() {
        return (units, skipped);
    }
    let mut out: Vec<ScanUnit> = Vec::new();
    for root in &opts.roots {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        let rs = root.to_string_lossy().into_owned();
        let Some(m) = mounts::mount_for(all_mounts, &rs) else { continue };
        // The unit owning this mount (or an ad-hoc one for pseudo/skipped fs).
        let mut unit = units.iter().find(|u| u.mount.id == m.id).cloned().unwrap_or_else(|| ScanUnit {
            mount: m.clone(),
            scan_root: m.mount_point.clone(),
            aliases: vec![],
        });
        unit.scan_root = rs.clone();
        out.push(unit);
        if !opts.one_file_system {
            for u in &units {
                if u.mount.mount_point != m.mount_point && mounts::is_path_under(&u.mount.mount_point, &rs) {
                    out.push(u.clone());
                }
            }
        }
    }
    let mut seen = HashSet::new();
    out.retain(|u| seen.insert(u.scan_root.clone()));
    (out, skipped)
}

fn spawn_progress(
    progress: Arc<Progress>,
    quiet: bool,
) -> Option<(std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>)> {
    if quiet || !std::io::stderr().is_terminal() {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let h = std::thread::spawn(move || {
        let start = Instant::now();
        loop {
            if rx.recv_timeout(Duration::from_millis(150)).is_ok() {
                break;
            }
            let cur = progress.current.lock().map(|c| c.clone()).unwrap_or_default();
            let cur: String = if cur.chars().count() > 60 {
                let tail: String = cur.chars().rev().take(57).collect::<Vec<_>>().into_iter().rev().collect();
                format!("...{tail}")
            } else {
                cur
            };
            eprint!(
                "\r\x1b[2K{:>6.1}s  {} files  {} dirs  {}  denied {}  {}",
                start.elapsed().as_secs_f64(),
                progress.files.load(Relaxed),
                progress.dirs.load(Relaxed),
                crate::model::fmt_size(progress.bytes.load(Relaxed)),
                progress.denied.load(Relaxed),
                cur
            );
        }
        eprint!("\r\x1b[2K");
    });
    Some((tx, h))
}

pub fn scan(opts: &ScanOptions) -> anyhow::Result<ScanResult> {
    let all_mounts = mounts::read_mountinfo();
    let (units, skipped) = plan(opts, &all_mounts);
    let mountpoints: HashSet<PathBuf> = all_mounts.iter().map(|m| PathBuf::from(&m.mount_point)).collect();
    let excludes = if opts.excludes.is_empty() {
        None
    } else {
        let mut b = globset::GlobSetBuilder::new();
        for e in &opts.excludes {
            b.add(globset::Glob::new(e)?);
        }
        Some(b.build()?)
    };
    let hardlinks = walker::new_hardlink_set();
    let progress = Arc::new(Progress::default());
    let threads =
        opts.threads.unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(8, 32));
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).stack_size(16 << 20).build()?;
    raise_nofile_limit();

    let reporter = spawn_progress(progress.clone(), opts.quiet);
    let walked: Vec<TmpDir> = pool.install(|| {
        units
            .par_iter()
            .map(|u| {
                let ctx = walker::WalkCtx {
                    mountpoints: &mountpoints,
                    dev: (u.mount.major, u.mount.minor),
                    allow_dev_change: u.mount.fstype == "btrfs" || u.mount.fstype == "zfs",
                    excludes: excludes.as_ref(),
                    hardlinks: &hardlinks,
                    progress: &progress,
                };
                // A different root must not be treated as a mountpoint of itself.
                walker::walk_root(&ctx, Path::new(&u.scan_root), u.scan_root.as_bytes())
            })
            .collect()
    });
    if let Some((tx, h)) = reporter {
        let _ = tx.send(());
        let _ = h.join();
    }

    let mut tree = FileTree::default();
    let mut filesystems = Vec::new();
    for (u, dir) in units.iter().zip(walked) {
        let mut info = fs_info(u);
        info.scanned_alloc = dir.alloc;
        info.scanned_apparent = dir.apparent;
        info.scanned_items = dir.items as u64;
        let root = append_tree(&mut tree, dir, &mut info);
        info.root_node = Some(root);
        tree.roots.push(root);
        filesystems.push(info);
    }
    for (m, reason) in skipped {
        if filesystems.iter().any(|f| f.mount_point == m.mount_point) {
            continue;
        }
        filesystems.push(FsInfo {
            mount_point: m.mount_point.clone(),
            scan_root: m.mount_point.clone(),
            source: m.source.clone(),
            fstype: m.fstype.clone(),
            options: m.options.clone(),
            fs_root: m.root.clone(),
            dev_major: m.major,
            dev_minor: m.minor,
            statvfs: statvfs(&m.mount_point),
            skipped_reason: Some(reason),
            ..Default::default()
        });
    }
    Ok(ScanResult { tree, filesystems, mounts: all_mounts })
}

/// Breadth-first flatten so every directory's children are contiguous.
pub fn append_tree(tree: &mut FileTree, root: TmpDir, info: &mut FsInfo) -> u32 {
    let root_id = tree.nodes.len() as u32;
    let (off, len) = tree.push_name(&root.name);
    tree.nodes.push(FNode {
        name_off: off,
        name_len: len,
        kind: Kind::Dir,
        flags: root.flags,
        parent: NONE,
        child_start: 0,
        child_count: 0,
        apparent: root.apparent,
        alloc: root.alloc,
        items: root.items,
        mtime: root.mtime,
    });
    let mut queue: VecDeque<(u32, TmpDir)> = VecDeque::new();
    queue.push_back((root_id, root));
    while let Some((id, mut dir)) = queue.pop_front() {
        if dir.flags & flags::DENIED != 0 {
            info.denied_dirs += 1;
        }
        if dir.flags & flags::ERROR != 0 {
            info.errors += 1;
        }
        let start = tree.nodes.len() as u32;
        let count = (dir.files.len() + dir.dirs.len()) as u32;
        for f in std::mem::take(&mut dir.files) {
            if f.flags & flags::ERROR != 0 {
                info.errors += 1;
            }
            let (off, len) = tree.push_name(&f.name);
            tree.nodes.push(FNode {
                name_off: off,
                name_len: len,
                kind: f.kind,
                flags: f.flags,
                parent: id,
                child_start: 0,
                child_count: 0,
                apparent: f.apparent,
                alloc: f.alloc,
                items: 1,
                mtime: f.mtime,
            });
        }
        for d in std::mem::take(&mut dir.dirs) {
            let cid = tree.nodes.len() as u32;
            let (off, len) = tree.push_name(&d.name);
            tree.nodes.push(FNode {
                name_off: off,
                name_len: len,
                kind: Kind::Dir,
                flags: d.flags,
                parent: id,
                child_start: 0,
                child_count: 0,
                apparent: d.apparent,
                alloc: d.alloc,
                items: d.items,
                mtime: d.mtime,
            });
            queue.push_back((cid, d));
        }
        let n = &mut tree.nodes[id as usize];
        n.child_start = start;
        n.child_count = count;
    }
    root_id
}

fn raise_nofile_limit() {
    let lim = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    if let Some(max) = lim.maximum {
        let want = max.min(1 << 20);
        if lim.current.is_some_and(|c| c < want) {
            let _ = rustix::process::setrlimit(
                rustix::process::Resource::Nofile,
                rustix::process::Rlimit { current: Some(want), maximum: lim.maximum },
            );
        }
    }
}

pub fn read_swaps() -> Vec<crate::model::Swap> {
    let Ok(s) = std::fs::read_to_string("/proc/swaps") else { return vec![] };
    s.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 4).then(|| crate::model::Swap {
                path: crate::util::unescape_octal(f[0]),
                kind: f[1].to_string(),
                size: f[2].parse::<u64>().unwrap_or(0) * 1024,
                used: f[3].parse::<u64>().unwrap_or(0) * 1024,
            })
        })
        .collect()
}

#[cfg(test)]
pub fn path_bytes(p: &Path) -> &[u8] {
    std::os::unix::ffi::OsStrExt::as_bytes(p.as_os_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scan_dir(p: &Path) -> (FileTree, FsInfo) {
        let mps = HashSet::new();
        let hl = walker::new_hardlink_set();
        let progress = Progress::default();
        let st = std::fs::metadata(p).unwrap();
        use std::os::unix::fs::MetadataExt;
        let dev = (libc::major(st.dev()), libc::minor(st.dev()));
        let ctx = walker::WalkCtx {
            mountpoints: &mps,
            dev,
            allow_dev_change: false,
            excludes: None,
            hardlinks: &hl,
            progress: &progress,
        };
        let d = walker::walk_root(&ctx, p, path_bytes(p));
        let mut tree = FileTree::default();
        let mut info = FsInfo {
            scan_root: p.to_string_lossy().into(),
            mount_point: p.to_string_lossy().into(),
            ..Default::default()
        };
        let r = append_tree(&mut tree, d, &mut info);
        info.root_node = Some(r);
        tree.roots.push(r);
        (tree, info)
    }

    #[test]
    fn walks_hardlinks_sparse_symlinks() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path();
        std::fs::create_dir_all(p.join("a/b/c")).unwrap();
        let mut f = std::fs::File::create(p.join("a/data")).unwrap();
        f.write_all(&vec![7u8; 1 << 20]).unwrap();
        f.sync_all().unwrap();
        std::fs::hard_link(p.join("a/data"), p.join("a/b/link")).unwrap();
        let sparse = std::fs::File::create(p.join("a/b/c/sparse")).unwrap();
        sparse.set_len(64 << 20).unwrap();
        std::os::unix::fs::symlink("../..", p.join("a/b/c/loop")).unwrap();

        let (tree, info) = scan_dir(p);
        let root = info.root_node.unwrap();
        let rn = tree.node(root);
        // 1 MiB counted once despite the hardlink; sparse file adds ~nothing allocated.
        assert!(rn.alloc >= 1 << 20 && rn.alloc < (1 << 20) + 256 * 1024, "alloc {}", rn.alloc);
        assert!(rn.apparent >= (65 << 20), "apparent {}", rn.apparent);
        assert_eq!(rn.items, 8); // root, a, b, c, data, link, sparse, loop

        let mut snap = crate::model::Snapshot { tree, filesystems: vec![info], ..Default::default() };
        let link = snap.lookup(&format!("{}/a/b/link", p.display()));
        let data = snap.lookup(&format!("{}/a/data", p.display()));
        let (l, d) = (snap.tree.node(link.unwrap()), snap.tree.node(data.unwrap()));
        assert!(l.has(flags::HARDLINK_DUP) ^ d.has(flags::HARDLINK_DUP));
        let sp = snap.lookup(&format!("{}/a/b/c/sparse", p.display())).unwrap();
        assert!(snap.tree.node(sp).has(flags::SPARSE));
        let lp = snap.lookup(&format!("{}/a/b/c/loop", p.display())).unwrap();
        assert_eq!(snap.tree.node(lp).kind, Kind::Symlink);
        assert_eq!(snap.tree.path(lp), format!("{}/a/b/c/loop", p.display()));

        // Attribution: two entities sharing one dir.
        let base = p.display().to_string();
        for (i, name) in ["x", "y"].iter().enumerate() {
            snap.entities.push(crate::model::Entity {
                name: name.to_string(),
                paths: vec![
                    format!("{base}/a/b"),
                    if i == 0 { format!("{base}/a/data") } else { format!("{base}/nope") },
                ],
                share_key: Some("k".into()),
                ..Default::default()
            });
        }
        crate::model::attribution::attribute(&mut snap);
        let (x, y) = (&snap.entities[0], &snap.entities[1]);
        assert_eq!(y.unresolved_paths.len(), 1);
        assert_eq!(x.measured_unique, snap.tree.node(data.unwrap()).alloc);
        assert_eq!(y.measured_unique, 0);
    }

    #[test]
    fn diff_finds_most_specific_growth() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path();
        std::fs::create_dir_all(p.join("a/b")).unwrap();
        std::fs::create_dir_all(p.join("c")).unwrap();
        std::fs::write(p.join("c/old.bin"), vec![1u8; 2 << 20]).unwrap();
        let snap = |p: &Path| {
            let (tree, info) = scan_dir(p);
            crate::model::Snapshot { tree, filesystems: vec![info], ..Default::default() }
        };
        let before = snap(p);
        std::fs::write(p.join("a/b/new.bin"), vec![2u8; 3 << 20]).unwrap();
        std::fs::remove_file(p.join("c/old.bin")).unwrap();
        let after = snap(p);
        let d = crate::model::diff::diff(&before, &after, 1 << 20);
        let paths: Vec<(&str, i64)> = d.hotspots.iter().map(|h| (h.path.as_str(), h.delta())).collect();
        assert!(paths.iter().any(|(h, dl)| h.ends_with("a/b/new.bin") && *dl >= 3 << 20), "{paths:?}");
        assert!(paths.iter().any(|(h, dl)| h.ends_with("c/old.bin") && *dl <= -(2 << 20)), "{paths:?}");
        // Parents fully explained by one child are not listed.
        assert!(!paths.iter().any(|(h, _)| h.ends_with("/a") || h.ends_with("/a/b")), "{paths:?}");
    }

    #[test]
    fn ncdu_export_roundtrips_sizes() {
        let td = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(td.path().join("d")).unwrap();
        std::fs::write(td.path().join("d/f"), vec![0u8; 10_000]).unwrap();
        let (tree, info) = scan_dir(td.path());
        let root = info.root_node.unwrap();
        let snap = crate::model::Snapshot { tree, filesystems: vec![info], ..Default::default() };
        let mut out = Vec::new();
        crate::model::ncdu::export(&snap, root, true, &mut out).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v[0], 1);
        fn sum(v: &serde_json::Value) -> u64 {
            match v {
                serde_json::Value::Array(a) => a.iter().map(sum).sum(),
                serde_json::Value::Object(o) => o.get("dsize").and_then(|x| x.as_u64()).unwrap_or(0),
                _ => 0,
            }
        }
        assert_eq!(sum(&v[3]), snap.tree.node(root).alloc);
    }

    #[cfg(unix)]
    #[test]
    fn denied_dirs_are_flagged() {
        if crate::util::is_root() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::tempdir().unwrap();
        let locked = td.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("secret"), b"x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let (tree, info) = scan_dir(td.path());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(info.denied_dirs, 1);
        assert!(tree.node(info.root_node.unwrap()).has(flags::INCOMPLETE));
    }
}
