//! Physical block layer from lsblk (udev db, no root needed) plus sysfs.

use super::{Ctx, Outcome, Provider};
use crate::model::{BlockDev, Snapshot};
use serde_json::Value;

pub struct Block;

const COLUMNS: &str =
    "NAME,KNAME,PATH,TYPE,SIZE,FSTYPE,LABEL,UUID,PARTTYPENAME,MOUNTPOINTS,MODEL,TRAN,RO,RM,FSAVAIL,FSUSED";

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(|x| x.trim().to_string()).filter(|x| !x.is_empty())
}

fn n(v: &Value, k: &str) -> Option<u64> {
    let x = v.get(k)?;
    x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))
}

fn b(v: &Value, k: &str) -> bool {
    match v.get(k) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "1",
        Some(Value::Number(n)) => n.as_u64() == Some(1),
        _ => false,
    }
}

pub fn parse_lsblk(json: &str) -> Vec<BlockDev> {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return vec![] };
    let mut out: Vec<BlockDev> = Vec::new();
    fn visit(v: &Value, parent: Option<&str>, out: &mut Vec<BlockDev>) {
        let Some(kname) = s(v, "kname") else { return };
        if let Some(existing) = out.iter_mut().find(|d| d.kname == kname) {
            if let Some(p) = parent
                && !existing.parents.iter().any(|x| x == p)
            {
                existing.parents.push(p.to_string());
            }
        } else {
            let mountpoints = v
                .get("mountpoints")
                .and_then(|m| m.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();
            out.push(BlockDev {
                kname: kname.clone(),
                name: s(v, "name").unwrap_or_else(|| kname.clone()),
                path: s(v, "path").unwrap_or_else(|| format!("/dev/{kname}")),
                dtype: s(v, "type").unwrap_or_default(),
                size: n(v, "size").unwrap_or(0),
                fstype: s(v, "fstype"),
                label: s(v, "label"),
                uuid: s(v, "uuid"),
                part_type: s(v, "parttypename"),
                mountpoints,
                model: s(v, "model"),
                tran: s(v, "tran"),
                ro: b(v, "ro"),
                removable: b(v, "rm"),
                parents: parent.map(|p| vec![p.to_string()]).unwrap_or_default(),
                fs_avail: n(v, "fsavail"),
                fs_used: n(v, "fsused"),
                ..Default::default()
            });
        }
        if let Some(children) = v.get("children").and_then(|c| c.as_array()) {
            for c in children {
                visit(c, Some(&kname), out);
            }
        }
    }
    for d in v.get("blockdevices").and_then(|x| x.as_array()).into_iter().flatten() {
        visit(d, None, &mut out);
    }
    out
}

/// Minimal fallback when lsblk is missing: walk /sys/class/block.
fn from_sysfs(ctx: &Ctx) -> Vec<BlockDev> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/sys/class/block") else { return out };
    for e in rd.flatten() {
        let kname = e.file_name().to_string_lossy().into_owned();
        let base = format!("/sys/class/block/{kname}");
        let size =
            ctx.runner.read(&format!("{base}/size")).and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0) * 512;
        let is_part = std::path::Path::new(&format!("{base}/partition")).exists();
        let dm_name = ctx.runner.read(&format!("{base}/dm/name")).map(|s| s.trim().to_string());
        let dtype = if is_part {
            "part"
        } else if kname.starts_with("loop") {
            "loop"
        } else if kname.starts_with("dm-") {
            "dm"
        } else if kname.starts_with("md") {
            "raid"
        } else {
            "disk"
        };
        let mut parents: Vec<String> = std::fs::read_dir(format!("{base}/slaves"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|s| s.file_name().to_string_lossy().into_owned())
            .collect();
        if is_part && let Ok(p) = std::fs::canonicalize(format!("{base}/..")) {
            parents.push(p.file_name().unwrap_or_default().to_string_lossy().into_owned());
        }
        out.push(BlockDev {
            name: dm_name.unwrap_or_else(|| kname.clone()),
            path: format!("/dev/{kname}"),
            kname,
            dtype: dtype.into(),
            size,
            parents,
            ..Default::default()
        });
    }
    out
}

impl Provider for Block {
    fn name(&self) -> &'static str {
        "block"
    }

    fn collect(&self, ctx: &Ctx, snap: &mut Snapshot) -> Outcome {
        let mut outcome = Outcome::complete();
        let mut devs = match ctx.runner.run(&["lsblk", "-J", "-b", "-o", COLUMNS]) {
            Some(o) if o.ok() => parse_lsblk(&o.stdout),
            _ => {
                outcome.degrade("lsblk unavailable; using sysfs (no filesystem types)");
                from_sysfs(ctx)
            }
        };
        if devs.is_empty() {
            return Outcome::denied("no block devices found");
        }
        for d in devs.iter_mut() {
            if d.dtype == "loop" {
                d.loop_backing_file = ctx
                    .runner
                    .read(&format!("/sys/block/{}/loop/backing_file", d.kname))
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
            }
            // Zero-size placeholders (empty card readers) are noise.
        }
        devs.retain(|d| d.size > 0 || !d.mountpoints.is_empty());

        // Annotate who uses each device.
        let swaps = snap.swaps.clone();
        for d in devs.iter_mut() {
            let real = std::fs::canonicalize(&d.path).ok();
            for sw in &swaps {
                let swp = std::fs::canonicalize(&sw.path).ok();
                if swp.is_some() && swp == real {
                    d.used_by.push(format!("swap ({} used)", crate::model::fmt_size(sw.used)));
                }
            }
            match d.fstype.as_deref() {
                Some("LVM2_member") => d.used_by.push("LVM physical volume".into()),
                Some("crypto_LUKS") => d.used_by.push("LUKS encrypted container".into()),
                Some("BitLocker") => d.used_by.push("BitLocker volume (Windows)".into()),
                Some(t) if t.ends_with("_raid_member") => d.used_by.push(format!("RAID member ({t})")),
                Some("swap") if d.used_by.is_empty() => d.used_by.push("swap (inactive)".into()),
                _ => {}
            }
            if d.mountpoints.is_empty()
                && d.used_by.is_empty()
                && let Some(role) = d.part_type.as_deref().and_then(partition_role)
            {
                d.used_by.push(role.into());
            }
            if let Some(bf) = &d.loop_backing_file {
                d.used_by.push(format!("loop device backed by {bf}"));
            }
        }
        snap.block = devs;
        outcome
    }
}

/// What an unmounted partition is for, judging by its GPT/MBR type.
fn partition_role(part_type: &str) -> Option<&'static str> {
    Some(match part_type {
        "EFI System" => "EFI system partition (not mounted here; another OS's bootloader?)",
        "Microsoft reserved" => "Windows reserved partition (MSR)",
        "Windows recovery environment" => "Windows recovery partition",
        "BIOS boot" => "BIOS boot partition (GRUB)",
        "Linux swap" => "swap partition (inactive)",
        "Linux extended boot" => "Linux /boot partition (not mounted)",
        _ => return None,
    })
}

/// Space on a disk not covered by any partition.
pub fn unpartitioned(snap: &Snapshot, disk: &BlockDev) -> u64 {
    let parts: u64 = snap
        .block
        .iter()
        .filter(|d| d.dtype == "part" && d.parents.iter().any(|p| p == &disk.kname))
        .map(|d| d.size)
        .sum();
    let has_children = snap.block.iter().any(|d| d.parents.iter().any(|p| p == &disk.kname));
    // Disks used whole (PV on the raw disk, fs on the disk) have no gap.
    if parts == 0 && (has_children || disk.fstype.is_some()) {
        return 0;
    }
    if parts == 0 {
        return disk.size;
    }
    // Partition tables + alignment take ~1-2 MiB; ignore tiny remainders.
    let gap = disk.size.saturating_sub(parts);
    if gap > 16 << 20 { gap } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lsblk_tree() {
        let json = include_str!("../../tests/fixtures/lsblk.json");
        let devs = parse_lsblk(json);
        let root = devs.iter().find(|d| d.name == "vg_system-lv_root").unwrap();
        assert_eq!(root.dtype, "lvm");
        assert_eq!(root.parents, vec!["nvme1n1p2".to_string()]);
        assert_eq!(root.mountpoints, vec!["/".to_string()]);
        let vm = devs.iter().find(|d| d.name == "vg_system-lv_vm_test").unwrap();
        assert!(vm.fstype.is_none() && vm.mountpoints.is_empty());
    }
}
