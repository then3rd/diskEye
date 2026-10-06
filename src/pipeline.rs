//! Scan → physical layer → space outside the tree → providers → attribution.

use crate::model::{Coverage, ProviderReport, SNAPSHOT_VERSION, ScanMeta, Snapshot};
use crate::providers::{self, Ctx, runner::SystemRunner};
use crate::scan::{self, ScanOptions};
use std::time::Instant;

#[derive(Debug, Clone, Default)]
pub struct PipelineOptions {
    pub scan: ScanOptions,
    /// Skip the filesystem walk entirely (providers/physical only).
    pub no_walk: bool,
    pub no_providers: bool,
    pub only_providers: Option<Vec<String>>,
    pub no_hidden: bool,
}

pub fn build(opts: &PipelineOptions) -> anyhow::Result<Snapshot> {
    let start = Instant::now();
    let started = crate::util::now_secs();
    let mut snap = Snapshot { version: SNAPSHOT_VERSION, ..Default::default() };
    snap.meta = ScanMeta {
        host: crate::util::hostname(),
        diskeye_version: env!("CARGO_PKG_VERSION").into(),
        started,
        duration_ms: 0,
        euid: crate::util::euid(),
        sudo_user: crate::util::sudo_user(),
        roots: opts.scan.roots.iter().map(|p| p.display().to_string()).collect(),
        kernel: crate::util::kernel(),
    };

    let mounts = if opts.no_walk {
        let all = scan::mounts::read_mountinfo();
        let (units, skipped) = scan::plan(&opts.scan, &all);
        for u in units {
            snap.filesystems.push(crate::model::FsInfo {
                mount_point: u.mount.mount_point.clone(),
                scan_root: u.scan_root.clone(),
                aliases: u.aliases.clone(),
                source: u.mount.source.clone(),
                fstype: u.mount.fstype.clone(),
                dev_major: u.mount.major,
                dev_minor: u.mount.minor,
                statvfs: rustix::fs::statvfs(u.mount.mount_point.as_str()).ok().map(|s| crate::model::StatVfs {
                    total: s.f_blocks * s.f_frsize,
                    free: s.f_bfree * s.f_frsize,
                    avail: s.f_bavail * s.f_frsize,
                    files: s.f_files,
                    files_free: s.f_ffree,
                    bsize: s.f_frsize,
                }),
                skipped_reason: Some("not walked (--no-walk)".into()),
                ..Default::default()
            });
        }
        drop(skipped);
        all
    } else {
        let r = scan::scan(&opts.scan)?;
        snap.tree = r.tree;
        snap.filesystems = r.filesystems;
        r.mounts
    };
    snap.swaps = scan::read_swaps();

    let t = Instant::now();
    let (deleted, unreadable) = scan::deleted_open::collect(&snap.filesystems);
    snap.deleted_open = deleted;
    snap.providers.push(ProviderReport {
        name: "deleted-open".into(),
        coverage: if unreadable > 0 { Coverage::Partial } else { Coverage::Complete },
        notes: if unreadable > 0 {
            vec![format!("{unreadable} processes not inspectable without root")]
        } else {
            vec![]
        },
        duration_ms: t.elapsed().as_millis() as u64,
    });

    let t = Instant::now();
    let hidden_report = if opts.no_hidden || opts.no_walk {
        ProviderReport {
            name: "hidden-under-mounts".into(),
            coverage: Coverage::Absent,
            notes: vec!["disabled".into()],
            duration_ms: 0,
        }
    } else if !crate::util::is_root() {
        ProviderReport {
            name: "hidden-under-mounts".into(),
            coverage: Coverage::Denied,
            notes: vec!["needs root to look underneath mountpoints".into()],
            duration_ms: 0,
        }
    } else {
        match scan::hidden::collect(&snap.filesystems, &mounts) {
            Ok(h) => {
                snap.hidden = h;
                ProviderReport {
                    name: "hidden-under-mounts".into(),
                    coverage: Coverage::Complete,
                    notes: vec![],
                    duration_ms: 0,
                }
            }
            Err(e) => ProviderReport {
                name: "hidden-under-mounts".into(),
                coverage: Coverage::Denied,
                notes: vec![e.to_string()],
                duration_ms: 0,
            },
        }
    };
    snap.providers.push(ProviderReport { duration_ms: t.elapsed().as_millis() as u64, ..hidden_report });

    let runner = SystemRunner::default();
    let (uid, _) = crate::util::invoking_ids();
    let is_root = crate::util::is_root();
    let users = if is_root {
        crate::util::human_users()
    } else {
        vec![(uid, crate::util::user_name(uid).unwrap_or_default(), crate::util::invoking_home())]
    };
    let ctx = Ctx { runner: &runner, is_root, uid, home: crate::util::invoking_home(), mounts: &mounts, users };
    let all = providers::all();
    if opts.no_providers {
        // The physical layer is cheap and always useful.
        let core: Vec<String> = vec!["block".into(), "lvm".into()];
        providers::run(&all, &ctx, &mut snap, Some(&core), opts.scan.quiet);
    } else {
        providers::run(&all, &ctx, &mut snap, opts.only_providers.as_deref(), opts.scan.quiet);
    }
    crate::model::attribution::attribute(&mut snap);
    snap.meta.duration_ms = start.elapsed().as_millis() as u64;
    Ok(snap)
}
