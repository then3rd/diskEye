//! Files that were unlinked but are still held open by a process. They use
//! space `df` reports, yet no directory walk can find them.

use crate::model::{DeletedOpen, FsInfo};
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;

pub fn collect(filesystems: &[FsInfo]) -> (Vec<DeletedOpen>, usize) {
    let mut out = Vec::new();
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut unreadable_procs = 0;
    let Ok(procs) = std::fs::read_dir("/proc") else { return (out, 0) };
    for p in procs.flatten() {
        let Some(pid) = p.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
        let fd_dir = p.path().join("fd");
        let Ok(fds) = std::fs::read_dir(&fd_dir) else {
            unreadable_procs += 1;
            continue;
        };
        let comm = crate::util::read_trim(p.path().join("comm")).unwrap_or_default();
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else { continue };
            let t = target.to_string_lossy();
            let Some(path) = t.strip_suffix(" (deleted)") else { continue };
            if path.starts_with("/memfd:") || path.starts_with("/dev/") || !path.starts_with('/') {
                continue;
            }
            // metadata() follows the magic link to the open inode.
            let Ok(md) = std::fs::metadata(fd.path()) else { continue };
            if !md.is_file() || !seen.insert((md.dev(), md.ino())) {
                continue;
            }
            let dev = md.dev();
            let fs = filesystems
                .iter()
                .position(|f| f.dev() == dev && f.root_node.is_some())
                .or_else(|| filesystems.iter().position(|f| f.dev() == dev));
            out.push(DeletedOpen {
                pid,
                comm: comm.clone(),
                fd: fd.file_name().to_str().and_then(|s| s.parse().ok()).unwrap_or(-1),
                path: path.to_string(),
                dev,
                ino: md.ino(),
                apparent: md.size(),
                alloc: md.blocks() * 512,
                fs,
            });
        }
    }
    out.sort_by_key(|d| std::cmp::Reverse(d.alloc));
    (out, unreadable_procs)
}
