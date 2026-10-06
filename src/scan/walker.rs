//! Parallel directory walker: getdents64 + statx relative to the directory fd,
//! rayon work-stealing across subdirectories. Never follows symlinks and
//! never crosses into another mount.

use crate::model::tree::{Kind, flags};
use rayon::prelude::*;
use rustix::fs::{AtFlags, FileType, Mode, OFlags, StatxFlags};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

#[derive(Debug)]
pub struct TmpEntry {
    pub name: Box<[u8]>,
    pub kind: Kind,
    pub flags: u8,
    pub apparent: u64,
    pub alloc: u64,
    pub mtime: i64,
}

#[derive(Debug, Default)]
pub struct TmpDir {
    pub name: Box<[u8]>,
    pub flags: u8,
    /// Aggregated over the subtree.
    pub apparent: u64,
    pub alloc: u64,
    pub items: u32,
    pub mtime: i64,
    pub files: Vec<TmpEntry>,
    pub dirs: Vec<TmpDir>,
}

#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub denied: AtomicU64,
    pub errors: AtomicU64,
    pub current: Mutex<String>,
}

const SHARDS: usize = 64;

pub struct WalkCtx<'a> {
    /// Every mount point on the system; the walk stops at these.
    pub mountpoints: &'a HashSet<PathBuf>,
    /// Device of the filesystem being walked; crossing to another device stops
    /// the walk too (except on btrfs, where subvolumes get their own st_dev).
    pub dev: (u32, u32),
    pub allow_dev_change: bool,
    pub excludes: Option<&'a globset::GlobSet>,
    pub hardlinks: &'a [Mutex<HashSet<(u64, u64)>>],
    pub progress: &'a Progress,
}

pub fn new_hardlink_set() -> Vec<Mutex<HashSet<(u64, u64)>>> {
    (0..SHARDS).map(|_| Mutex::new(HashSet::new())).collect()
}

const MASK: StatxFlags = StatxFlags::TYPE
    .union(StatxFlags::MODE)
    .union(StatxFlags::NLINK)
    .union(StatxFlags::INO)
    .union(StatxFlags::SIZE)
    .union(StatxFlags::BLOCKS)
    .union(StatxFlags::MTIME);

fn kind_of(ft: FileType) -> Kind {
    match ft {
        FileType::Directory => Kind::Dir,
        FileType::RegularFile => Kind::File,
        FileType::Symlink => Kind::Symlink,
        _ => Kind::Special,
    }
}

fn is_sparse(apparent: u64, alloc: u64) -> bool {
    apparent > (1 << 20) && alloc < apparent / 10 * 9
}

/// Stat the scan root itself and walk it.
pub fn walk_root(ctx: &WalkCtx, root: &Path, name: &[u8]) -> TmpDir {
    match rustix::fs::statx(rustix::fs::CWD, root, AtFlags::SYMLINK_NOFOLLOW, MASK) {
        Ok(st) => {
            let alloc = st.stx_blocks * 512;
            walk_dir(ctx, root.to_path_buf(), name.into(), st.stx_size, alloc, st.stx_mtime.tv_sec)
        }
        Err(e) => {
            ctx.progress.errors.fetch_add(1, Relaxed);
            let mut d =
                TmpDir { name: name.into(), flags: flags::ERROR | flags::INCOMPLETE, items: 1, ..Default::default() };
            if e == rustix::io::Errno::ACCESS || e == rustix::io::Errno::PERM {
                d.flags |= flags::DENIED;
            }
            d
        }
    }
}

fn walk_dir(ctx: &WalkCtx, path: PathBuf, name: Box<[u8]>, own_apparent: u64, own_alloc: u64, mtime: i64) -> TmpDir {
    let mut dir = TmpDir { name, apparent: own_apparent, alloc: own_alloc, items: 1, mtime, ..Default::default() };
    ctx.progress.dirs.fetch_add(1, Relaxed);
    ctx.progress.bytes.fetch_add(own_alloc, Relaxed);

    let fd = match rustix::fs::open(
        &path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(e) => {
            if e == rustix::io::Errno::ACCESS || e == rustix::io::Errno::PERM {
                ctx.progress.denied.fetch_add(1, Relaxed);
                dir.flags |= flags::DENIED | flags::INCOMPLETE;
            } else {
                ctx.progress.errors.fetch_add(1, Relaxed);
                dir.flags |= flags::ERROR | flags::INCOMPLETE;
            }
            return dir;
        }
    };

    let mut subdirs: Vec<(Box<[u8]>, u64, u64, i64)> = Vec::new();
    let mut buf: Vec<MaybeUninit<u8>> = vec![MaybeUninit::uninit(); 32 * 1024];
    let mut iter = rustix::fs::RawDir::new(&fd, &mut buf);
    while let Some(ent) = iter.next() {
        let ent = match ent {
            Ok(e) => e,
            Err(_) => {
                ctx.progress.errors.fetch_add(1, Relaxed);
                dir.flags |= flags::ERROR | flags::INCOMPLETE;
                break;
            }
        };
        let cname = ent.file_name();
        let nb = cname.to_bytes();
        if nb == b"." || nb == b".." {
            continue;
        }
        if let Some(ex) = ctx.excludes
            && ex.is_match(path.join(OsStr::from_bytes(nb)))
        {
            continue;
        }
        let st = match rustix::fs::statx(&fd, cname, AtFlags::SYMLINK_NOFOLLOW | AtFlags::STATX_DONT_SYNC, MASK) {
            Ok(st) => st,
            Err(rustix::io::Errno::NOENT) => continue,
            Err(_) => {
                ctx.progress.errors.fetch_add(1, Relaxed);
                dir.flags |= flags::INCOMPLETE;
                dir.files.push(TmpEntry {
                    name: nb.into(),
                    kind: kind_of(ent.file_type()),
                    flags: flags::ERROR,
                    apparent: 0,
                    alloc: 0,
                    mtime: 0,
                });
                continue;
            }
        };
        let ft = FileType::from_raw_mode(st.stx_mode as u32);
        let kind = kind_of(ft);
        let apparent = st.stx_size;
        let alloc = st.stx_blocks * 512;
        let mtime = st.stx_mtime.tv_sec;
        if kind == Kind::Dir {
            let child = path.join(OsStr::from_bytes(nb));
            let other_dev = (st.stx_dev_major, st.stx_dev_minor) != ctx.dev && !ctx.allow_dev_change;
            if ctx.mountpoints.contains(&child) || other_dev {
                dir.files.push(TmpEntry {
                    name: nb.into(),
                    kind: Kind::Dir,
                    flags: flags::MOUNTPOINT,
                    apparent: 0,
                    alloc: 0,
                    mtime,
                });
            } else {
                subdirs.push((nb.into(), apparent, alloc, mtime));
            }
            continue;
        }
        let mut fl = 0;
        if st.stx_nlink > 1 {
            fl |= flags::HARDLINK;
            let key = (((st.stx_dev_major as u64) << 32) | st.stx_dev_minor as u64, st.stx_ino);
            let shard = &ctx.hardlinks[(st.stx_ino as usize) % ctx.hardlinks.len()];
            if !shard.lock().unwrap().insert(key) {
                fl = flags::HARDLINK_DUP;
            }
        }
        if is_sparse(apparent, alloc) {
            fl |= flags::SPARSE;
        }
        ctx.progress.files.fetch_add(1, Relaxed);
        if fl & flags::HARDLINK_DUP == 0 {
            dir.apparent += apparent;
            dir.alloc += alloc;
            ctx.progress.bytes.fetch_add(alloc, Relaxed);
        }
        dir.mtime = dir.mtime.max(mtime);
        dir.files.push(TmpEntry { name: nb.into(), kind, flags: fl, apparent, alloc, mtime });
    }
    drop(buf);
    drop(fd);
    dir.items += dir.files.len() as u32;

    if ctx.progress.dirs.load(Relaxed).is_multiple_of(2048)
        && let Ok(mut c) = ctx.progress.current.try_lock()
    {
        *c = path.to_string_lossy().into_owned();
    }

    let children: Vec<TmpDir> = if subdirs.len() == 1 {
        let (n, a, b, m) = subdirs.pop().unwrap();
        vec![walk_dir(ctx, path.join(OsStr::from_bytes(&n)), n, a, b, m)]
    } else {
        subdirs
            .into_par_iter()
            .map(|(n, a, b, m)| walk_dir(ctx, path.join(OsStr::from_bytes(&n)), n, a, b, m))
            .collect()
    };
    for c in &children {
        dir.apparent += c.apparent;
        dir.alloc += c.alloc;
        dir.items += c.items;
        dir.mtime = dir.mtime.max(c.mtime);
        if c.flags & flags::INCOMPLETE != 0 {
            dir.flags |= flags::INCOMPLETE;
        }
    }
    dir.dirs = children;
    dir
}

/// Sum a subtree without building it (used for hidden-under-mount scans).
pub fn measure(path: &Path) -> (u64, u64, u64) {
    let mps = HashSet::new();
    let hl = new_hardlink_set();
    let progress = Progress::default();
    let dev = rustix::fs::statx(rustix::fs::CWD, path, AtFlags::SYMLINK_NOFOLLOW, MASK)
        .map(|s| (s.stx_dev_major, s.stx_dev_minor))
        .unwrap_or((0, 0));
    let ctx = WalkCtx {
        mountpoints: &mps,
        dev,
        allow_dev_change: false,
        excludes: None,
        hardlinks: &hl,
        progress: &progress,
    };
    let d = walk_root(&ctx, path, path.as_os_str().as_bytes());
    (d.alloc, d.apparent, d.items as u64)
}
