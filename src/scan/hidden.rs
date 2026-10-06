//! Files hidden underneath mountpoints (e.g. data written to /home before
//! /home was mounted). Needs root: we re-exec ourselves, unshare a private
//! mount namespace, bind-mount each filesystem non-recursively to a temp dir,
//! and measure what sits under its mountpoints there.

use crate::model::{FsInfo, HiddenUnderMount};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::process::{Command, Stdio};

pub const SUBCOMMAND: &str = "__hidden-under-mounts";

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub fs: usize,
    pub mount_point: String,
    /// Mountpoints located on this filesystem, as absolute paths.
    pub covered: Vec<String>,
}

/// Build requests for every scanned filesystem and run the helper process.
pub fn collect(filesystems: &[FsInfo], mounts: &[super::mounts::Mount]) -> anyhow::Result<Vec<HiddenUnderMount>> {
    let mut reqs = Vec::new();
    for (i, f) in filesystems.iter().enumerate() {
        if f.root_node.is_none() || f.scan_root != f.mount_point {
            continue;
        }
        // Mountpoints whose parent mount is this filesystem's mount.
        let Some(me) =
            mounts.iter().find(|m| m.mount_point == f.mount_point && m.major == f.dev_major && m.minor == f.dev_minor)
        else {
            continue;
        };
        let mut covered: Vec<String> = mounts
            .iter()
            .filter(|m| m.parent == me.id && m.mount_point != f.mount_point)
            .map(|m| m.mount_point.clone())
            .collect();
        covered.sort();
        covered.dedup();
        if !covered.is_empty() {
            reqs.push(Request { fs: i, mount_point: f.mount_point.clone(), covered });
        }
    }
    if reqs.is_empty() {
        return Ok(vec![]);
    }
    let exe = std::env::current_exe()?;
    let mut child = Command::new(exe)
        .arg(SUBCOMMAND)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(&serde_json::to_vec(&reqs)?)?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        anyhow::bail!("hidden-under-mount helper failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

/// Entry point of the helper process. Must run before any threads exist.
pub fn helper_main() -> anyhow::Result<()> {
    use rustix::mount::{MountPropagationFlags, mount_bind, mount_change};
    let reqs: Vec<Request> = serde_json::from_reader(std::io::stdin())?;
    // SAFETY: we are single-threaded at this point (called first thing in main).
    unsafe { rustix::thread::unshare_unsafe(rustix::thread::UnshareFlags::NEWNS)? };
    mount_change("/", MountPropagationFlags::REC | MountPropagationFlags::PRIVATE)?;
    let base = std::env::temp_dir().join(format!("diskeye-hidden-{}", std::process::id()));
    let mut results = Vec::new();
    for r in reqs {
        let target = base.join(format!("fs{}", r.fs));
        std::fs::create_dir_all(&target)?;
        if mount_bind(r.mount_point.as_str(), &target).is_err() {
            continue;
        }
        for c in &r.covered {
            let rel = c.strip_prefix(r.mount_point.trim_end_matches('/')).unwrap_or(c).trim_start_matches('/');
            let p = target.join(rel);
            if !p.is_dir() {
                continue;
            }
            let (alloc, apparent, items) = super::walker::measure(&p);
            // Discount the (empty) mountpoint directory itself.
            let own =
                std::fs::symlink_metadata(&p).map(|m| std::os::unix::fs::MetadataExt::blocks(&m) * 512).unwrap_or(0);
            let alloc = alloc.saturating_sub(own);
            if items > 1 {
                results.push(HiddenUnderMount { fs: r.fs, path: c.clone(), alloc, apparent, items: items - 1 });
            }
        }
        let _ = rustix::mount::unmount(&target, rustix::mount::UnmountFlags::DETACH);
        // Never remove recursively: if the unmount failed this is a live view of a filesystem.
        let _ = std::fs::remove_dir(&target);
    }
    let _ = std::fs::remove_dir(&base);
    serde_json::to_writer(std::io::stdout(), &results)?;
    Ok(())
}
