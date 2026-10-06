//! System logs: the systemd journal (cross-checked with `journalctl
//! --disk-usage`), coredumps, and the rest of /var/log with rotated logs
//! split out as safe to delete.

use super::classifier::{Claimed, alloc_of};
use super::{Ctx, Outcome, Provider};
use crate::model::tree::{Kind, flags};
use crate::model::{ActionSpec, ActionStep, Entity, NodeId, Reclaim, Risk, Snapshot};

pub struct Journald;

const JOURNAL_DIRS: [&str; 2] = ["/var/log/journal", "/run/log/journal"];
const COREDUMP_DIR: &str = "/var/lib/systemd/coredump";
const KEEP: u64 = 500 << 20;

/// "Archived and active journals take up 3.2G in the file system." -> bytes.
pub fn parse_disk_usage(out: &str) -> Option<u64> {
    let rest = out.split(" take up ").nth(1)?;
    super::parse_size(rest.split_whitespace().next()?)
}

/// logrotate output: `x.gz`, `x.1`, `x.old`, `x-20260101`, ...
pub fn is_rotated(name: &str) -> bool {
    const EXT: [&str; 6] = [".gz", ".xz", ".bz2", ".zst", ".lz4", ".old"];
    if EXT.iter().any(|e| name.ends_with(e)) {
        return true;
    }
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let num_ext = name.rsplit_once('.').is_some_and(|(stem, n)| !stem.is_empty() && digits(n));
    let date = name.rsplit_once('-').is_some_and(|(stem, d)| !stem.is_empty() && d.len() == 8 && digits(d));
    num_ext || date
}

fn entity(kind: &str, name: &str, paths: Vec<String>) -> Entity {
    Entity {
        kind: kind.into(),
        name: name.into(),
        provider: "journald".into(),
        group: "Logs".into(),
        paths,
        ..Default::default()
    }
}

/// Entity bytes the scan could not see (unreadable directories) but a tool reported.
fn fill_unreadable(snap: &Snapshot, nodes: &[NodeId], e: &mut Entity, reported: u64) -> bool {
    let unreadable =
        nodes.is_empty() || nodes.iter().any(|&n| snap.tree.node(n).has(flags::DENIED | flags::INCOMPLETE));
    let measured = alloc_of(&snap.tree, nodes);
    if unreadable && reported > measured {
        e.external_bytes = reported - measured;
        e.attrs.push(("size source".into(), "journalctl (journal not readable by this user)".into()));
        return true;
    }
    false
}

impl Provider for Journald {
    fn name(&self) -> &'static str {
        "journald"
    }

    fn collect(&self, ctx: &Ctx, snap: &mut Snapshot) -> Outcome {
        let mut outcome = Outcome::complete();
        let mut claimed = Claimed::from_entities(snap);
        let tree = &snap.tree;
        let mut new: Vec<Entity> = Vec::new();

        // Journal.
        let reported = ctx
            .runner
            .run(&["journalctl", "--disk-usage"])
            .filter(|o| o.ok())
            .and_then(|o| parse_disk_usage(&o.stdout));
        let jnodes: Vec<NodeId> =
            JOURNAL_DIRS.iter().filter_map(|p| snap.lookup_static(p)).filter(|&n| !claimed.covers(tree, n)).collect();
        if !jnodes.is_empty() || reported.is_some() {
            let paths = jnodes.iter().map(|&n| tree.path(n)).collect();
            let mut e = entity("logs.journal", "systemd journal", paths);
            e.reported = reported;
            if let Some(r) = reported
                && fill_unreadable(snap, &jnodes, &mut e, r)
            {
                outcome.degrade("journal directory not readable; size taken from journalctl --disk-usage");
            }
            let total = alloc_of(tree, &jnodes) + e.external_bytes;
            if total > KEEP {
                e.reclaim = Some(Reclaim {
                    risk: Risk::Review,
                    reason: "old journal entries; vacuuming keeps the newest 500 MiB".into(),
                    estimate: Some(total - KEEP),
                    action: Some(ActionSpec {
                        label: "journalctl --vacuum-size=500M".into(),
                        steps: vec![ActionStep::Command {
                            argv: vec!["journalctl".into(), "--vacuum-size=500M".into()],
                            root: true,
                        }],
                    }),
                });
            }
            for &n in &jnodes {
                claimed.add(tree, n);
            }
            new.push(e);
        }

        // Coredumps.
        if let Some(n) = snap.lookup_static(COREDUMP_DIR).filter(|&n| !claimed.covers(tree, n)) {
            if tree.node(n).alloc > 0 {
                let mut e = entity("coredump", "systemd coredumps", vec![COREDUMP_DIR.into()]);
                e.reclaim = Some(Reclaim {
                    risk: Risk::Safe,
                    reason: "memory dumps of crashed programs, only useful for debugging".into(),
                    estimate: None,
                    action: Some(ActionSpec {
                        label: "delete coredumps".into(),
                        steps: vec![ActionStep::EmptyDir { path: COREDUMP_DIR.into() }],
                    }),
                });
                new.push(e);
            }
            claimed.add(tree, n);
        }

        // The rest of /var/log: rotated files are safe, everything else is listed.
        if let Some(log) = snap.lookup_static("/var/log").filter(|&n| !claimed.covers(tree, n)) {
            let mut rotated: Vec<NodeId> = Vec::new();
            let mut stack = vec![log];
            while let Some(n) = stack.pop() {
                if claimed.covers(tree, n) {
                    continue;
                }
                let node = tree.node(n);
                if node.kind == Kind::Dir {
                    stack.extend(tree.children(n));
                } else if node.kind == Kind::File && is_rotated(&tree.name(n)) {
                    rotated.push(n);
                }
            }
            rotated.sort_unstable();
            if alloc_of(tree, &rotated) > 0 {
                let paths: Vec<String> = rotated.iter().map(|&n| tree.path(n)).collect();
                let mut e = entity("logs.rotated", "rotated logs in /var/log", paths.clone());
                e.reclaim = Some(Reclaim {
                    risk: Risk::Safe,
                    reason: "old rotated log files".into(),
                    estimate: None,
                    action: Some(ActionSpec {
                        label: "delete rotated logs".into(),
                        steps: paths.into_iter().map(|path| ActionStep::DeletePath { path, trash: false }).collect(),
                    }),
                });
                new.push(e);
                for &n in &rotated {
                    claimed.add(tree, n);
                }
            }
            let rest = claimed.subtract(tree, log);
            if alloc_of(tree, &rest) > 0 {
                new.push(entity("logs", "other logs in /var/log", rest.iter().map(|&n| tree.path(n)).collect()));
            }
        }

        if new.is_empty() {
            return Outcome::absent();
        }
        for e in new {
            snap.add_entity(e);
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

    #[test]
    fn parses_disk_usage() {
        let real = include_str!("../../tests/fixtures/journald/disk-usage.txt");
        assert_eq!(parse_disk_usage(real), Some((3.2 * (1u64 << 30) as f64) as u64));
        assert_eq!(parse_disk_usage("garbage"), None);
    }

    #[test]
    fn rotated_names() {
        for n in ["pacman.log.1", "Xorg.0.log.old", "messages-20260101", "syslog.2.gz", "dmesg.0"] {
            assert!(is_rotated(n), "{n}");
        }
        for n in ["pacman.log", "Xorg.0.log", "wtmp", "btmp", "boot.log"] {
            assert!(!is_rotated(n), "{n}");
        }
    }

    #[test]
    fn journal_coredumps_and_logs() {
        let mut snap = snap_from(&[
            ("/var/log/journal/abc/system.journal", 2000 * MB),
            ("/var/log/pacman.log", 2 * MB),
            ("/var/log/pacman.log.1", 3 * MB),
            ("/var/log/nginx/access.log", MB),
            ("/var/log/nginx/access.log.2.gz", 4 * MB),
            ("/var/lib/systemd/coredump/core.x.zst", 10 * MB),
        ]);
        let runner = FakeRunner::default()
            .with("journalctl --disk-usage", "Archived and active journals take up 1.9G in the file system.\n");
        let out = Journald.collect(&ctx(&runner, &[("u", "/home/u")]), &mut snap);
        assert_eq!(out.coverage, crate::model::Coverage::Complete);
        attribute(&mut snap);

        let j = find(&snap, "systemd journal");
        assert_eq!(j.measured_alloc, 2000 * MB);
        assert_eq!(j.external_bytes, 0);
        let r = j.reclaim.as_ref().unwrap();
        assert_eq!(r.estimate, Some(1500 * MB));
        assert_eq!(
            r.action.as_ref().unwrap().steps,
            vec![ActionStep::Command { argv: vec!["journalctl".into(), "--vacuum-size=500M".into()], root: true }]
        );
        assert_eq!(find(&snap, "systemd coredumps").reclaim.as_ref().unwrap().risk, Risk::Safe);
        let rot = find(&snap, "rotated logs in /var/log");
        assert_eq!(rot.measured_alloc, 7 * MB);
        let other = find(&snap, "other logs in /var/log");
        assert_eq!(other.measured_alloc, 3 * MB);
        assert!(other.reclaim.is_none());
    }

    #[test]
    fn unreadable_journal_uses_reported_size() {
        let mut snap = snap_from(&[("/var/log/journal/", 0)]);
        let n = snap.lookup("/var/log/journal").unwrap();
        snap.tree.nodes[n as usize].flags |= flags::DENIED;
        let runner = FakeRunner::default()
            .with("journalctl --disk-usage", "Archived and active journals take up 800M in the file system.\n");
        let out = Journald.collect(&ctx(&runner, &[("u", "/home/u")]), &mut snap);
        assert_eq!(out.coverage, crate::model::Coverage::Partial);
        attribute(&mut snap);
        let j = find(&snap, "systemd journal");
        assert_eq!(j.total_bytes(), 800 * MB);
        assert_eq!(j.reclaim.as_ref().unwrap().estimate, Some(300 * MB));
    }
}
